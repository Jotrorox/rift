"""Each platform stages its standalone executable and an operator archive."""

from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import package

TRACKED = {
    "README.md": "See [operations](docs/operations.md) and [CI](.github/workflows/ci.yml).",
    "LICENSE": "License",
    "THIRD_PARTY_NOTICES": "Notices",
    "docs/operations.md": "Install [the unit](../examples/rift.service); see [SDKs](../sdk/c).",
    "docs/performance.md": "[Raw](performance/sample.json.gz), [index](performance/index.json).",
    "docs/performance/index.json": "{}",
    "docs/performance/sample.json.gz": "raw samples",
    "examples/rift.service": "[Service]",
    "examples/rift.lua": "-- config",
    "examples/modular/rift.lua": "-- modular",
    "examples/messaging_bench.rs": "fn main() {}",
    "examples/plugins/hello-rs/src/lib.rs": "// plugin",
    "src/main.rs": "fn main() {}",
}
PACKAGED = {
    "README.md", "LICENSE", "THIRD_PARTY_NOTICES", "docs/operations.md",
    "docs/performance.md", "docs/performance/index.json", "examples/rift.service",
    "examples/rift.lua", "examples/modular/rift.lua", "examples/plugins/hello-rs/src/lib.rs",
}


def repository(root):
    for name, text in TRACKED.items():
        (root / name).parent.mkdir(parents=True, exist_ok=True)
        (root / name).write_text(text)
    (root / "examples/untracked.lua").write_text("-- local scratch file")
    subprocess.run(["git", "init", "-q"], cwd=root, check=True)
    subprocess.run(["git", "add", *TRACKED], cwd=root, check=True)


class PackageTests(unittest.TestCase):
    def test_contents_are_tracked_operator_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repository(root)
            self.assertEqual(set(package.contents(root)), PACKAGED)

    def test_contents_require_every_packaged_path(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repository(root)
            subprocess.run(["git", "rm", "-q", "--cached", "THIRD_PARTY_NOTICES"],
                           cwd=root, check=True)
            with self.assertRaisesRegex(RuntimeError, "THIRD_PARTY_NOTICES"):
                package.contents(root)

    def test_stages_executable_and_archive_for_each_platform(self):
        for label, (system, machine) in package.PLATFORMS.items():
            with self.subTest(platform=label), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                repository(root)
                (root / "Cargo.toml").write_text('[package]\nversion = "0.1.0"\n')
                windows = system == "Windows"
                binary_name = "rift.exe" if windows else "rift"
                binary = root / "target" / "release" / binary_name
                binary.parent.mkdir(parents=True)
                binary.write_bytes(b"test binary")
                binary.chmod(0o755)
                with mock.patch.object(package, "ROOT", root), \
                        mock.patch.object(package.platform, "system", return_value=system), \
                        mock.patch.object(package.platform, "machine", return_value=machine), \
                        mock.patch.object(sys, "argv", ["package.py", "--platform", label]), \
                        mock.patch.object(package, "smoke_test") as smoke_test:
                    package.main()
                executable = root / "dist" / f"rift-{label}{'.exe' if windows else ''}"
                archive = root / "dist" / f"rift-{label}{'.zip' if windows else '.tar.gz'}"
                self.assertEqual(set((root / "dist").iterdir()), {executable, archive})
                self.assertEqual(executable.read_bytes(), binary.read_bytes())
                self.assertEqual(executable.stat().st_mode, binary.stat().st_mode)
                smoke_test.assert_called_once_with(executable, archive, "0.1.0")

                expected = PACKAGED | {binary_name}
                if windows:
                    with zipfile.ZipFile(archive) as bundle:
                        self.assertEqual(set(bundle.namelist()), expected)
                        files = {name: bundle.read(name) for name in expected}
                else:
                    with tarfile.open(archive) as bundle:
                        self.assertEqual(set(bundle.getnames()), expected)
                        self.assertEqual(bundle.getmember(binary_name).mode, 0o755)
                        self.assertEqual(bundle.getmember("README.md").mode, 0o644)
                        self.assertEqual({member.uid for member in bundle}, {0})
                        files = {name: bundle.extractfile(name).read() for name in expected}
                self.assertEqual(files[binary_name], b"test binary")
                for name in PACKAGED:
                    self.assertEqual(files[name], (root / name).read_bytes())

                extracted = root / "extracted"
                package.extract(archive, extracted)
                self.assertEqual(package.broken_links(extracted), [])

    def test_broken_links_reports_missing_packaged_targets(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary) / "archive"
            (directory / "docs").mkdir(parents=True)
            (directory / "examples").mkdir()
            (directory / "examples/rift.service").write_text("[Service]")
            (directory / "docs/operations.md").write_text(
                "[unit](../examples/rift.service#install) [missing](../examples/Dockerfile) "
                "[repository only](../tests/MANUAL_ONLINE.md) [site](https://example.com/x) "
                "[anchor](#first-start) [online only](performance/raw.json.gz) "
                "[outside](../../elsewhere.md)")
            self.assertEqual(package.broken_links(directory), [
                "docs/operations.md -> ../examples/Dockerfile",
                "docs/operations.md -> ../../elsewhere.md",
            ])


if __name__ == "__main__":
    unittest.main()
