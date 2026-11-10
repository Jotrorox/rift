#!/usr/bin/env python3
"""Stage, archive and smoke-test a native release executable.

Each platform produces two release assets: the standalone executable, and an
archive holding that executable with the README, licenses, docs/ and examples/.
"""

import argparse
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
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
# Tracked files under these paths go into every archive, beside the executable.
PACKAGED = ("README.md", "LICENSE", "THIRD_PARTY_NOTICES", "docs", "examples")
LINK = re.compile(r"\]\(([^)\s]+)\)")


def omitted(name):
    """Tracked files that stay online only."""
    path = PurePosixPath(name)
    # Cargo benchmark harnesses, not operator examples.
    if path.parent == PurePosixPath("examples") and path.suffix == ".rs":
        return True
    # Raw benchmark samples (about 24 MB); docs/performance.md summarises them.
    return path.parent == PurePosixPath("docs/performance") and name.endswith(".json.gz")


def contents(root):
    """Return the archive member names for the documentation and examples."""
    listed = subprocess.run(["git", "ls-files", "-z", "--", *PACKAGED], cwd=root,
                            check=True, capture_output=True).stdout.decode()
    names = sorted(name for name in listed.split("\0") if name and not omitted(name))
    missing = [name for name in PACKAGED if not any(
        entry == name or entry.startswith(name + "/") for entry in names)]
    if missing:
        raise RuntimeError(f"release archive inputs are not tracked: {missing}")
    return names


def archive_name(label):
    return f"rift-{label}{'.zip' if label.startswith('windows') else '.tar.gz'}"


def create_archive(archive, root, executable, binary_name, names):
    members = [(executable, binary_name)] + [(root / name, name) for name in names]
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as package:
            for path, name in members:
                package.write(path, name)
        return

    def normalise(info):
        info.uid = info.gid = 0
        info.uname = info.gname = ""
        info.mode = 0o755 if info.name == binary_name else 0o644
        return info

    with tarfile.open(archive, "w:gz") as package:
        for path, name in members:
            package.add(path, arcname=name, recursive=False, filter=normalise)


def extract(archive, directory):
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as package:
            package.extractall(directory)
    else:
        with tarfile.open(archive) as package:
            package.extractall(directory, filter="data")


def broken_links(directory):
    """Relative Markdown links into packaged paths that the archive lacks.

    Links to other repository paths (SDKs, tests, workflows) are left to the
    online documentation; anything under PACKAGED must work offline.
    """
    broken = []
    for document in sorted(directory.rglob("*.md")):
        text = document.read_text(encoding="utf-8")
        for target in LINK.findall(text):
            target = target.split("#", 1)[0]
            if not target or re.match(r"^[a-z][a-z0-9+.-]*:", target, re.IGNORECASE):
                continue
            resolved = (document.parent / target).resolve()
            try:
                name = resolved.relative_to(directory.resolve()).as_posix()
            except ValueError:
                broken.append(f"{document.relative_to(directory)} -> {target}")
                continue
            if name.split("/", 1)[0] not in PACKAGED or omitted(name):
                continue
            if not resolved.exists():
                broken.append(f"{document.relative_to(directory)} -> {target}")
    return broken


def smoke_test(executable, archive, version):
    with tempfile.TemporaryDirectory(prefix="rift-package-") as temporary:
        # Run with only the executable present, as downloaded from a release.
        directory = Path(temporary) / "standalone"
        directory.mkdir()
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

        # The archive must hold the same executable and working offline docs.
        extracted = Path(temporary) / "archive"
        extract(archive, extracted)
        packaged = extracted / ("rift.exe" if executable.suffix == ".exe" else "rift")
        if packaged.read_bytes() != executable.read_bytes():
            raise RuntimeError("archived executable differs from the standalone executable")
        broken = broken_links(extracted)
        if broken:
            raise RuntimeError("archived documentation links to missing files:\n"
                               + "\n".join(broken))
        configs = [*sorted((extracted / "examples").glob("*.lua")),
                   extracted / "examples/modular/rift.lua"]
        for config in configs:
            subprocess.run([str(packaged), "--check", str(config)],
                           cwd=extracted, check=True, timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    args = parser.parse_args()
    if (platform.system(), platform.machine()) != PLATFORMS[args.platform]:
        parser.error("runner architecture does not match the release label")
    windows = args.platform.startswith("windows")
    binary_name = "rift.exe" if windows else "rift"
    binary = ROOT / "target" / "release" / binary_name
    output = ROOT / "dist"
    output.mkdir(exist_ok=True)
    executable = output / f"rift-{args.platform}{'.exe' if windows else ''}"
    shutil.copy2(binary, executable)
    archive = output / archive_name(args.platform)
    create_archive(archive, ROOT, executable, binary_name, contents(ROOT))
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        version = tomllib.load(manifest)["package"]["version"]
    smoke_test(executable, archive, version)
    print(executable)
    print(archive)


if __name__ == "__main__":
    main()
