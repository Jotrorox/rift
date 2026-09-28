#!/usr/bin/env python3
"""Package the tested native executable and README with a SHA-256 sidecar."""

import argparse
import hashlib
from pathlib import Path
import platform
import subprocess
import tarfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
PLATFORMS = {
    "linux-x86_64": ("Linux", "x86_64"),
    "macos-aarch64": ("Darwin", "arm64"),
    "windows-x86_64": ("Windows", "AMD64"),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != PLATFORMS[args.platform]:
        parser.error("runner architecture does not match the release label")
    windows = args.platform.startswith("windows")
    binary = ROOT / "target" / "release" / ("rift.exe" if windows else "rift")
    subprocess.run([str(binary), "--help"], check=True, timeout=10)
    output = ROOT / "dist"
    output.mkdir(exist_ok=True)
    files = [binary, ROOT / "README.md"]
    archive = output / f"rift-{args.platform}{'.zip' if windows else '.tar.gz'}"
    if windows:
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as package:
            for path in files:
                package.write(path, path.name)
    else:
        with tarfile.open(archive, "w:gz") as package:
            for path in files:
                package.add(path, arcname=path.name)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{digest}  {archive.name}\n", encoding="utf-8", newline="\n"
    )
    print(archive)


if __name__ == "__main__":
    main()
