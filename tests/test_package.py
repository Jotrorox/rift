"""Release archive layout is the same on every supported platform."""

from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import package


class PackageTests(unittest.TestCase):
    def test_archives_preserve_operator_files_and_binary(self):
        root = Path(__file__).resolve().parents[1]
        required = {"README.md", "LICENSE", "THIRD_PARTY_NOTICES",
                    "examples/rift.lua", "examples/online.lua",
                    "examples/network.lua", "examples/rift.service", "examples/Dockerfile",
                    "examples/compose.yaml", "docs/operations.md",
                    "examples/admin.lua", "examples/messaging.lua", "docs/http.md",
                    "docs/messaging.md", "docs/messaging-lua.md",
                    "docs/messaging-protocol.md", "docs/network-protocol.md"}
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for windows in [False, True]:
                with self.subTest(windows=windows):
                    binary = directory / ("rift.exe" if windows else "rift")
                    binary.write_bytes(b"test binary")
                    binary.chmod(0o755)
                    archive = directory / ("release.zip" if windows else "release.tar.gz")
                    package.create_archive(archive, package.contents(root, binary), windows)
                    if windows:
                        with zipfile.ZipFile(archive) as bundle:
                            self.assertTrue(required | {binary.name} <= set(bundle.namelist()))
                            for name in required:
                                self.assertEqual(bundle.read(name), (root / name).read_bytes())
                            self.assertEqual(bundle.read(binary.name), b"test binary")
                    else:
                        with tarfile.open(archive) as bundle:
                            self.assertTrue(required | {binary.name} <= set(bundle.getnames()))
                            for name in required:
                                self.assertEqual(bundle.extractfile(name).read(), (root / name).read_bytes())
                            self.assertEqual(bundle.extractfile(binary.name).read(), b"test binary")
                            self.assertTrue(bundle.getmember(binary.name).mode & 0o111)


if __name__ == "__main__":
    unittest.main()
