"""Verify the comparison uses exactly its pinned official Velocity artifact."""

import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from scripts import fetch_velocity


class Response(io.BytesIO):
    def __init__(self, body, url="https://downloads.example.test/velocity.jar"):
        super().__init__(body)
        self.url = url

    def geturl(self):
        return self.url


class VelocityDownloadTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.cache = self.root / "cache"
        self.manifest = self.root / "velocity.json"
        self.body = b"pinned Velocity fixture\n"
        self.artifact = {
            "filename": "velocity-4.2.0-30.jar",
            "url": "https://downloads.example.test/velocity.jar",
            "sha256": hashlib.sha256(self.body).hexdigest(),
            "size_bytes": len(self.body),
        }
        self.write_manifest()

    def write_manifest(self):
        self.manifest.write_text(json.dumps(self.artifact), encoding="utf-8")

    def fetch(self):
        return fetch_velocity.fetch(self.cache, self.manifest)

    def test_official_manifest_loads(self):
        artifact = fetch_velocity.load_manifest()
        self.assertEqual(artifact["version"], "4.2.0")
        self.assertEqual(artifact["channel"], "STABLE")
        self.assertEqual(artifact["sha256"],
                         "35a5596a5468a035d8a32c8de5ebb0dc6b8d8f0cc3ff5169d514aca762af8aa8")

    def test_download_is_verified_and_cache_hits_are_reverified(self):
        with patch("scripts.fetch_velocity.urllib.request.urlopen",
                   return_value=Response(self.body)) as download:
            destination = self.fetch()
        self.assertEqual(destination.read_bytes(), self.body)
        self.assertEqual(list(self.cache.iterdir()), [destination])
        self.assertIn("rift-benchmark", download.call_args.args[0].get_header("User-agent"))
        with patch("scripts.fetch_velocity.urllib.request.urlopen") as download:
            self.assertEqual(self.fetch(), destination)
            download.assert_not_called()
            destination.write_bytes(b"x" * len(self.body))
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                self.fetch()
            download.assert_not_called()

    def test_invalid_download_never_becomes_cached_artifact(self):
        for body, url, error in [
            (b"x" * len(self.body), "https://downloads.example.test/jar", "checksum mismatch"),
            (self.body[:-1], "https://downloads.example.test/jar", "size mismatch"),
            (self.body + b"x", "https://downloads.example.test/jar", "exceeds pinned size"),
            (self.body, "http://downloads.example.test/jar", "away from HTTPS"),
        ]:
            with self.subTest(error=error), \
                    patch("scripts.fetch_velocity.urllib.request.urlopen", return_value=Response(body, url)):
                with self.assertRaisesRegex(ValueError, error):
                    self.fetch()
                self.assertEqual(list(self.cache.iterdir()), [])

    def test_existing_symlink_or_directory_is_rejected(self):
        self.cache.mkdir()
        destination = self.cache / self.artifact["filename"]
        destination.mkdir()
        with self.assertRaisesRegex(ValueError, "regular jar"):
            self.fetch()
        destination.rmdir()
        target = self.root / "elsewhere.jar"
        target.write_bytes(self.body)
        try:
            destination.symlink_to(target)
        except OSError:
            self.skipTest("symlink creation unavailable")
        with self.assertRaisesRegex(ValueError, "regular jar"):
            self.fetch()
        target.unlink()
        with self.assertRaisesRegex(ValueError, "regular jar"):
            self.fetch()

    def test_manifest_rejects_unsafe_or_ambiguous_identity(self):
        for field, value in [
            ("filename", "../velocity.jar"), ("filename", "velocity.jar/child"),
            ("sha256", "z" * 64), ("sha256", "0" * 63),
            ("url", "http://downloads.example.test/velocity.jar"),
            ("url", "https:///velocity.jar"),
            ("size_bytes", 0), ("size_bytes", -1), ("size_bytes", True),
            ("size_bytes", 129 * 1024 * 1024),
        ]:
            with self.subTest(field=field, value=value):
                artifact = dict(self.artifact, **{field: value})
                self.manifest.write_text(json.dumps(artifact), encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "invalid Velocity artifact manifest"):
                    fetch_velocity.load_manifest(self.manifest)


if __name__ == "__main__":
    unittest.main()
