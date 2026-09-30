#!/usr/bin/env python3
"""Fetch the pinned Velocity benchmark jar, verifying downloads and cache hits."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import tempfile
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "tests/velocity.json"
CACHE = ROOT / "target/velocity"


def load_manifest(path=MANIFEST):
    artifact = json.loads(Path(path).read_text())
    filename = artifact.get("filename", "")
    url = urllib.parse.urlsplit(artifact.get("url", ""))
    if (not re.fullmatch(r"velocity-[A-Za-z0-9.-]+\.jar", filename)
            or not re.fullmatch(r"[0-9a-f]{64}", artifact.get("sha256", ""))
            or url.scheme != "https" or not url.hostname
            or type(artifact.get("size_bytes")) is not int
            or not 0 < artifact["size_bytes"] <= 128 * 1024 * 1024):
        raise ValueError("invalid Velocity artifact manifest")
    return artifact


def verify(path, artifact):
    path = Path(path)
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"expected a regular jar: {path}")
    if path.stat().st_size != artifact["size_bytes"]:
        raise ValueError(f"Velocity size mismatch: {path}")
    with path.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    if digest != artifact["sha256"]:
        raise ValueError(f"Velocity checksum mismatch: {path}: {digest}")


def fetch(cache=CACHE, manifest=MANIFEST):
    artifact = load_manifest(manifest)
    cache = Path(cache)
    cache.mkdir(parents=True, exist_ok=True)
    destination = cache / artifact["filename"]
    if destination.exists() or destination.is_symlink():
        verify(destination, artifact)
        return destination
    request = urllib.request.Request(artifact["url"], headers={
        "User-Agent": "rift-benchmark/1.0 (https://github.com/Jotrorox/rift)"})
    with tempfile.TemporaryDirectory(prefix=".download-", dir=cache) as temporary:
        pending = Path(temporary) / "velocity.jar"
        with urllib.request.urlopen(request, timeout=60) as response, pending.open("wb") as output:
            if urllib.parse.urlsplit(response.geturl()).scheme != "https":
                raise ValueError("Velocity download redirected away from HTTPS")
            total = 0
            while block := response.read(1024 * 1024):
                total += len(block)
                if total > artifact["size_bytes"]:
                    raise ValueError("Velocity download exceeds pinned size")
                output.write(block)
        verify(pending, artifact)
        pending.replace(destination)
    return destination


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, default=CACHE)
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    args = parser.parse_args()
    print(fetch(args.cache, args.manifest))


if __name__ == "__main__":
    main()
