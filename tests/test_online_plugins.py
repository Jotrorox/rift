"""Offline failure-detection tests for authenticated fixture plugin installation."""

import copy
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import urllib.error

import minecraft as mc
import online_plugins as plugins


class Response(io.BytesIO):
    def geturl(self):
        return "https://example.test/pinned.jar"


class PluginManifestTests(unittest.TestCase):
    def setUp(self):
        self.manifest = copy.deepcopy(plugins.MANIFEST)

    def test_pins_match_the_paper_fixture_and_all_three_stacks(self):
        self.assertEqual(self.manifest["paper_version"], mc.SERVERS["paper"]["version"])
        self.assertEqual(plugins.STACKS, ("baseline", "essentials", "protocol"))
        self.assertEqual(plugins.selection("baseline")["expected_plugins"], {})
        self.assertEqual(plugins.selection("essentials")["expected_plugins"], {
            "LuckPerms": "5.5.85", "Essentials": "2.22.0", "EssentialsChat": "2.22.0"})
        self.assertEqual(plugins.selection("protocol")["expected_plugins"], {
            "ViaVersion": "5.12.0", "ViaBackwards": "5.12.0"})
        self.assertIn("translation is not covered", plugins.selection("protocol")["description"])

    def test_selection_is_a_copy_and_cannot_change_future_pins(self):
        selected = plugins.selection("essentials")
        selected["artifacts"][0]["sha256"] = "0" * 64
        selected["artifacts"][0]["requires"].append("bogus")
        self.assertEqual(plugins.MANIFEST, self.manifest)

    def test_unknown_stack_is_never_treated_as_baseline(self):
        for name in ("all", "", "Essentials", "../baseline"):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "unknown plugin stack"):
                plugins.selection(name)

    def test_invalid_headers_fail(self):
        mutations = [None, [], {}, {**self.manifest, "schema_version": True},
                     {**self.manifest, "schema_version": 2},
                     {**self.manifest, "paper_version": "latest"},
                     {**self.manifest, "unknown": 1},
                     {**self.manifest, "artifacts": []}, {**self.manifest, "stacks": {}}]
        for manifest in mutations:
            with self.subTest(manifest=manifest), self.assertRaises(ValueError):
                plugins.validate_manifest(manifest)

    def test_missing_fields_and_wrong_field_types_fail(self):
        for field in self.manifest["artifacts"]["luckperms"]:
            for value in (None, [], 12):
                manifest = copy.deepcopy(self.manifest)
                manifest["artifacts"]["luckperms"][field] = value
                if field == "requires" and value == []:
                    continue
                with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                    plugins.validate_manifest(manifest)
            manifest = copy.deepcopy(self.manifest)
            del manifest["artifacts"]["luckperms"][field]
            with self.subTest(missing=field), self.assertRaises(ValueError):
                plugins.validate_manifest(manifest)

    def test_unsafe_filenames_and_incomplete_checksums_fail(self):
        for filename in ("../escape.jar", "/absolute.jar", "nested/plugin.jar", "nested\\plugin.jar",
                         ".hidden.jar", "plugin..jar", "x.jar\n", "x.zip", "C:plugin.jar"):
            manifest = copy.deepcopy(self.manifest)
            manifest["artifacts"]["luckperms"]["filename"] = filename
            with self.subTest(filename=filename), self.assertRaises(ValueError):
                plugins.validate_manifest(manifest)
        for digest in ("", "0" * 63, "g" * 64, "A" * 64, "sha256:" + "a" * 64):
            manifest = copy.deepcopy(self.manifest)
            manifest["artifacts"]["luckperms"]["sha256"] = digest
            with self.subTest(digest=digest), self.assertRaises(ValueError):
                plugins.validate_manifest(manifest)

    def test_insecure_downloads_credentials_and_nonfixed_versions_fail(self):
        for url in ("http://example.test/x.jar", "file:///tmp/x.jar", "https:///x.jar",
                    "https://user:password@example.test/x.jar", "https://example.test/x.jar#fragment",
                    "https://example.test/x.jar\n"):
            manifest = copy.deepcopy(self.manifest)
            manifest["artifacts"]["luckperms"]["url"] = url
            with self.subTest(url=url), self.assertRaises(ValueError):
                plugins.validate_manifest(manifest)
        for version in ("latest", "5.5.0-SNAPSHOT", "", "5.5"):
            manifest = copy.deepcopy(self.manifest)
            manifest["artifacts"]["luckperms"]["version"] = version
            with self.subTest(version=version), self.assertRaises(ValueError):
                plugins.validate_manifest(manifest)

    def test_duplicate_filenames_or_bukkit_names_fail_case_insensitively(self):
        for field in ("filename", "name"):
            self.manifest["artifacts"]["essentials"][field] = (
                self.manifest["artifacts"]["luckperms"][field].swapcase()
                if field == "name" else self.manifest["artifacts"]["luckperms"][field])
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "duplicate"):
                plugins.validate_manifest(self.manifest)
            self.manifest = copy.deepcopy(plugins.MANIFEST)

    def test_invalid_stack_and_missing_dependency_fail(self):
        for selected in ([], ["nonexistent"], ["luckperms", "luckperms"],
                         ["essentials-chat"], ["viabackwards"], "essentials", [None]):
            manifest = copy.deepcopy(self.manifest)
            manifest["stacks"]["essentials"]["artifacts"] = selected
            with self.subTest(selected=selected), self.assertRaises(ValueError):
                plugins.validate_manifest(manifest)
        self.manifest["stacks"]["baseline"]["artifacts"] = ["luckperms"]
        with self.assertRaises(ValueError):
            plugins.validate_manifest(self.manifest)

    def test_dependencies_must_be_known_unique_and_not_self(self):
        for dependencies in (["unknown"], ["luckperms"], ["essentials", "essentials"], [None]):
            manifest = copy.deepcopy(self.manifest)
            manifest["artifacts"]["luckperms"]["requires"] = dependencies
            with self.subTest(dependencies=dependencies), self.assertRaises(ValueError):
                plugins.validate_manifest(manifest)

    def test_loading_invalid_json_fails(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "manifest.json"
            path.write_text("{broken")
            with self.assertRaises(json.JSONDecodeError):
                plugins.load_manifest(path)


class PluginInstallationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.cache = self.directory / "cache"
        self.destination = self.directory / "plugins"
        self.manifest = copy.deepcopy(plugins.MANIFEST)
        self.contents = {}
        for artifact in self.manifest["artifacts"].values():
            content = (artifact["name"] + " pinned fixture bytes").encode()
            artifact["sha256"] = hashlib.sha256(content).hexdigest()
            self.contents[artifact["url"]] = content
        self.network = patch.object(plugins.urllib.request, "urlopen", side_effect=self.response).start()
        self.addCleanup(patch.stopall)

    def response(self, request, timeout):
        self.assertEqual(timeout, 60)
        return Response(self.contents[request.full_url])

    def install(self, stack="essentials"):
        return plugins.install(stack, self.destination, cache=self.cache, manifest=self.manifest)

    def test_complete_stack_is_downloaded_and_copies_are_verified(self):
        evidence = self.install()
        self.assertEqual(self.network.call_count, 3)
        self.assertEqual(set(evidence["expected_plugins"]), {"LuckPerms", "Essentials", "EssentialsChat"})
        self.assertEqual(len(list(self.destination.iterdir())), 3)
        for artifact in evidence["artifacts"]:
            self.assertEqual((self.destination / artifact["filename"]).read_bytes(),
                             self.contents[artifact["url"]])
        self.assertEqual(json.loads(json.dumps(evidence)), evidence)

    def test_baseline_does_not_access_the_network(self):
        self.assertEqual(self.install("baseline")["artifacts"], [])
        self.network.assert_not_called()
        self.assertEqual(list(self.destination.iterdir()), [])

    def test_unknown_stack_fails_before_creating_anything(self):
        with self.assertRaises(ValueError):
            self.install("typo")
        self.network.assert_not_called()
        self.assertFalse(self.destination.exists())
        self.assertFalse(self.cache.exists())

    def test_warm_cache_is_reused_after_verification(self):
        first = self.install()
        self.network.reset_mock()
        second = plugins.install("essentials", self.directory / "second", cache=self.cache,
                                 manifest=self.manifest)
        self.assertEqual(first, second)
        self.network.assert_not_called()

    def test_corrupted_cached_jar_fails_even_when_installed_copy_is_valid(self):
        evidence = self.install()
        bad = self.cache / evidence["artifacts"][1]["filename"]
        bad.write_bytes(b"corrupted")
        self.network.reset_mock()
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            self.install()
        self.assertEqual(bad.read_bytes(), b"corrupted")
        self.network.assert_not_called()

    def test_wrong_download_checksum_leaves_no_installed_stack_or_bad_cache_entry(self):
        artifact = self.manifest["artifacts"]["essentials"]
        self.contents[artifact["url"]] = b"corrupt download"
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            self.install()
        self.assertEqual(list(self.destination.iterdir()), [])
        self.assertFalse((self.cache / artifact["filename"]).exists())
        self.assertEqual(len(list(self.cache.iterdir())), 1)

    def test_unavailable_plugin_cannot_silently_reduce_the_stack(self):
        failure = urllib.error.HTTPError("https://example.test/missing", 404, "not found", None, None)
        self.addCleanup(failure.close)
        def unavailable(request, timeout):
            if "EssentialsXChat" in request.full_url:
                raise failure
            return self.response(request, timeout)
        self.network.side_effect = unavailable
        with self.assertRaises(urllib.error.HTTPError):
            self.install()
        self.assertEqual(list(self.destination.iterdir()), [])

    def test_failed_download_cleans_up_temporary_files(self):
        class InterruptedResponse(Response):
            def read(self, size):
                raise OSError("connection interrupted")
        self.network.return_value = InterruptedResponse(b"")
        self.network.side_effect = None
        with self.assertRaisesRegex(OSError, "interrupted"):
            self.install()
        self.assertEqual(list(self.cache.iterdir()), [])
        self.assertEqual(list(self.destination.iterdir()), [])

    def test_download_cannot_redirect_to_an_insecure_url(self):
        class InsecureResponse(Response):
            def geturl(self):
                return "http://example.test/plugin.jar"
        self.network.side_effect = lambda request, timeout: InsecureResponse(b"")
        with self.assertRaisesRegex(ValueError, "HTTPS"):
            self.install()
        self.assertEqual(list(self.cache.iterdir()), [])

    def test_download_and_cached_files_have_a_size_limit(self):
        with patch.object(plugins, "MAX_ARTIFACT_BYTES", 1):
            with self.assertRaisesRegex(ValueError, "size limit"):
                self.install()
        self.assertEqual(list(self.cache.iterdir()), [])
        self.install()
        with patch.object(plugins, "MAX_ARTIFACT_BYTES", 1):
            with self.assertRaisesRegex(ValueError, "size limit"):
                self.install()

    def test_existing_modified_destination_is_not_overwritten(self):
        evidence = self.install()
        path = self.destination / evidence["artifacts"][0]["filename"]
        path.write_bytes(b"operator modification")
        self.network.reset_mock()
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            self.install()
        self.assertEqual(path.read_bytes(), b"operator modification")
        self.network.assert_not_called()

    def test_cache_symlinks_and_dangling_symlinks_fail(self):
        self.cache.mkdir()
        artifact = self.manifest["artifacts"]["luckperms"]
        path = self.cache / artifact["filename"]
        outside = self.directory / "outside.jar"
        for exists in (False, True):
            if exists:
                outside.write_bytes(self.contents[artifact["url"]])
            path.symlink_to(outside)
            with self.subTest(exists=exists), self.assertRaisesRegex(ValueError, "regular file"):
                self.install()
            path.unlink()
        self.network.assert_not_called()

    def test_destination_symlink_cannot_overwrite_outside_file(self):
        self.destination.mkdir()
        artifact = self.manifest["artifacts"]["luckperms"]
        outside = self.directory / "outside.jar"
        outside.write_bytes(self.contents[artifact["url"]])
        (self.destination / artifact["filename"]).symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "regular file"):
            self.install()
        self.assertEqual(outside.read_bytes(), self.contents[artifact["url"]])
        self.network.assert_not_called()

    def test_symlinked_plugin_or_cache_directory_is_rejected(self):
        outside = self.directory / "outside"
        outside.mkdir()
        for directory in (self.destination, self.cache):
            if directory.exists():
                directory.rmdir()
            directory.symlink_to(outside, target_is_directory=True)
            with self.subTest(directory=directory), self.assertRaisesRegex(ValueError, "symlink"):
                self.install()
            directory.unlink()
        self.network.assert_not_called()


if __name__ == "__main__":
    unittest.main()
