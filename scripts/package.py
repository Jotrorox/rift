#!/usr/bin/env python3
"""Package and smoke-test an operator-ready native release with a SHA-256 sidecar."""

import argparse
import hashlib
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = {
    "linux-x86_64": ("Linux", "x86_64"),
    "macos-aarch64": ("Darwin", "arm64"),
    "windows-x86_64": ("Windows", "AMD64"),
}


def contents(root, binary):
    return [(binary, binary.name)] + [
        (path, path.relative_to(root).as_posix())
        for path in [root / "README.md", root / "LICENSE", root / "THIRD_PARTY_NOTICES",
                     *sorted((root / "examples").rglob("*.lua")),
                     root / "examples/rift.service",
                     root / "examples/Dockerfile",
                     root / "examples/compose.yaml",
                     *sorted((root / "docs").glob("*.md"))]
    ]


def create_archive(archive, files, windows):
    if windows:
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as package:
            for path, name in files:
                package.write(path, name)
    else:
        with tarfile.open(archive, "w:gz") as package:
            for path, name in files:
                package.add(path, arcname=name)


def smoke_test(archive, windows, version):
    # Run from the extracted archive, with no dependency on the source checkout.
    with tempfile.TemporaryDirectory(prefix="rift-package-") as temporary:
        directory = Path(temporary)
        if windows:
            with zipfile.ZipFile(archive) as package:
                package.extractall(directory)
        else:
            with tarfile.open(archive) as package:
                package.extractall(directory, filter="data")
        binary = directory / ("rift.exe" if windows else "rift")
        # Informational flags must not evaluate an implicit configuration.
        (directory / "rift.lua").write_text("this is invalid Lua", encoding="utf-8")
        result = subprocess.run([str(binary), "--version"], cwd=directory,
                                check=True, capture_output=True, text=True, timeout=10)
        if result.stdout.strip() != f"rift {version}" or result.stderr:
            raise RuntimeError(f"unexpected packaged version output: {result}")
        subprocess.run([str(binary), "--help"], cwd=directory, check=True, timeout=10)
        license_files = [directory / "LICENSE", directory / "THIRD_PARTY_NOTICES"]
        expected_license = "\n".join(path.read_text(encoding="utf-8") for path in license_files)
        # The standalone binary must carry its notices without external files.
        for path in license_files:
            path.unlink()
        result = subprocess.run([str(binary), "--license"], cwd=directory,
                                check=True, capture_output=True, text=True,
                                encoding="utf-8", timeout=10)
        if result.stdout != expected_license or result.stderr:
            raise RuntimeError("unexpected packaged license output")
        configs = [*sorted((directory / "examples").glob("*.lua")),
                   directory / "examples/modular/rift.lua"]
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
    archive = output / f"rift-{args.platform}{'.zip' if windows else '.tar.gz'}"
    create_archive(archive, contents(ROOT, binary), windows)
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    smoke_test(archive, windows, version)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{digest}  {archive.name}\n", encoding="utf-8", newline="\n"
    )
    print(archive)


if __name__ == "__main__":
    main()
