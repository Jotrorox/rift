#!/usr/bin/env python3
"""Stage and smoke-test a standalone native release executable."""

import argparse
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = {
    "linux-x86_64": ("Linux", "x86_64"),
    "macos-aarch64": ("Darwin", "arm64"),
    "windows-x86_64": ("Windows", "AMD64"),
}


def smoke_test(executable, version):
    # Run with only the executable present, as downloaded from a release.
    with tempfile.TemporaryDirectory(prefix="rift-package-") as temporary:
        directory = Path(temporary)
        binary = directory / executable.name
        shutil.copy2(executable, binary)
        # Informational flags must not evaluate an implicit configuration.
        (directory / "rift.lua").write_text("this is invalid Lua", encoding="utf-8")
        result = subprocess.run([str(binary), "--version"], cwd=directory,
                                check=True, capture_output=True, text=True, timeout=10)
        if result.stdout.strip() != f"rift {version}" or result.stderr:
            raise RuntimeError(f"unexpected packaged version output: {result}")
        subprocess.run([str(binary), "--help"], cwd=directory, check=True, timeout=10)
        license_files = [ROOT / "LICENSE", ROOT / "THIRD_PARTY_NOTICES"]
        expected_license = "\n".join(path.read_text(encoding="utf-8") for path in license_files)
        # The standalone binary must carry its notices without external files.
        result = subprocess.run([str(binary), "--license"], cwd=directory,
                                check=True, capture_output=True, text=True,
                                encoding="utf-8", timeout=10)
        if result.stdout != expected_license or result.stderr:
            raise RuntimeError("unexpected packaged license output")
        configs = [*sorted((ROOT / "examples").glob("*.lua")),
                   ROOT / "examples/modular/rift.lua"]
        for config in configs:
            subprocess.run([str(binary), "--check", str(config)],
                           cwd=directory, check=True, timeout=10)
        generated = directory / "generated.lua"
        subprocess.run([str(binary), "init", str(generated)],
                       cwd=directory, check=True, timeout=10)
        subprocess.run([str(binary), "check", str(generated)],
                       cwd=directory, check=True, timeout=10)
        before = generated.read_bytes()
        result = subprocess.run([str(binary), "init", str(generated)],
                                cwd=directory, capture_output=True, timeout=10)
        if result.returncode == 0 or generated.read_bytes() != before:
            raise RuntimeError("rift init must refuse to overwrite an existing configuration")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != PLATFORMS[args.platform]:
        parser.error("runner architecture does not match the release label")
    windows = args.platform.startswith("windows")
    binary = ROOT / "target" / "release" / ("rift.exe" if windows else "rift")
    output = ROOT / "dist"
    output.mkdir(exist_ok=True)
    executable = output / f"rift-{args.platform}{'.exe' if windows else ''}"
    shutil.copy2(binary, executable)
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    smoke_test(executable, version)
    print(executable)


if __name__ == "__main__":
    main()
