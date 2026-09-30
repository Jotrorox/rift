"""Each supported platform stages only its standalone release executable."""

from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import package


class PackageTests(unittest.TestCase):
    def test_stages_only_executable_for_each_platform(self):
        for label, (system, machine) in package.PLATFORMS.items():
            with self.subTest(platform=label), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                (root / "Cargo.toml").write_text('[package]\nversion = "0.1.0"\n')
                # Operator files stay in the repository, even when present.
                (root / "README.md").write_text("Documentation")
                (root / "LICENSE").write_text("License")
                suffix = ".exe" if system == "Windows" else ""
                binary = root / "target" / "release" / f"rift{suffix}"
                binary.parent.mkdir(parents=True)
                binary.write_bytes(b"test binary")
                binary.chmod(0o755)
                with mock.patch.object(package, "ROOT", root), \
                        mock.patch.object(package.platform, "system", return_value=system), \
                        mock.patch.object(package.platform, "machine", return_value=machine), \
                        mock.patch.object(sys, "argv", ["package.py", "--platform", label]), \
                        mock.patch.object(package, "smoke_test") as smoke_test:
                    package.main()
                executable = root / "dist" / f"rift-{label}{suffix}"
                self.assertEqual(list((root / "dist").iterdir()), [executable])
                self.assertEqual(executable.read_bytes(), binary.read_bytes())
                self.assertEqual(executable.stat().st_mode, binary.stat().st_mode)
                smoke_test.assert_called_once_with(executable, "0.1.0")


if __name__ == "__main__":
    unittest.main()
