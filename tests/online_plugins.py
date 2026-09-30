"""Pinned plugin combinations for the authenticated Paper acceptance fixture.

The manifest is the only source of versions: this module never resolves latest
releases. A successful installation is not a compatibility PASS; the runner must
also compare Paper's enabled plugin inventory with ``expected_plugins`` and
collect gameplay evidence. Plugins may fetch their own runtime dependencies.
"""

import copy
import hashlib
import json
from pathlib import Path
import re
import shutil
import tempfile
import urllib.parse
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
MANIFEST_PATH = ROOT / "tests/online_plugins.json"
CACHE = ROOT / "target/online-plugins"
MAX_ARTIFACT_BYTES = 64 * 1024 * 1024


def _https(value):
    if not isinstance(value, str):
        return False
    parsed = urllib.parse.urlsplit(value)
    return (parsed.scheme == "https" and bool(parsed.hostname)
            and parsed.username is None and parsed.password is None
            and not parsed.fragment and not any(ch.isspace() for ch in value))


def _identifiers(value):
    return (isinstance(value, list)
            and all(isinstance(item, str) for item in value)
            and len(set(value)) == len(value))


def validate_manifest(manifest):
    """Reject incomplete pins and ambiguous or unsafe selections before any I/O."""
    if (not isinstance(manifest, dict)
            or set(manifest) != {"schema_version", "paper_version", "artifacts", "stacks"}
            or type(manifest["schema_version"]) is not int or manifest["schema_version"] != 1
            or not isinstance(manifest["paper_version"], str)
            or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+-[0-9]+", manifest["paper_version"])):
        raise ValueError("invalid online plugin manifest header")
    artifacts, stacks = manifest["artifacts"], manifest["stacks"]
    if not isinstance(artifacts, dict) or not isinstance(stacks, dict) or not stacks:
        raise ValueError("plugin artifacts and stacks must be objects")
    names, filenames = set(), set()
    for key, artifact in artifacts.items():
        if not isinstance(key, str) or not re.fullmatch(r"[a-z][a-z0-9-]*", key):
            raise ValueError(f"invalid artifact id: {key!r}")
        if (not isinstance(artifact, dict) or set(artifact) != {
                "name", "version", "filename", "url", "sha256", "source", "requires"}
                or not all(isinstance(artifact[field], str) and artifact[field]
                           for field in ("name", "version", "filename", "url", "sha256", "source"))):
            raise ValueError(f"incomplete artifact pin: {key}")
        if (not re.fullmatch(r"[A-Za-z][A-Za-z0-9_]*", artifact["name"])
                or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", artifact["version"])
                or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*\.jar", artifact["filename"])
                or ".." in artifact["filename"]
                or not re.fullmatch(r"[0-9a-f]{64}", artifact["sha256"])
                or not _https(artifact["url"]) or not _https(artifact["source"])):
            raise ValueError(f"invalid artifact pin: {key}")
        if artifact["name"].casefold() in names or artifact["filename"].casefold() in filenames:
            raise ValueError(f"duplicate plugin name or filename: {key}")
        names.add(artifact["name"].casefold())
        filenames.add(artifact["filename"].casefold())
        if (not _identifiers(artifact["requires"])
                or any(dep not in artifacts or dep == key for dep in artifact["requires"])):
            raise ValueError(f"invalid plugin dependencies: {key}")
    for key, stack in stacks.items():
        if (not isinstance(key, str) or not re.fullmatch(r"[a-z][a-z0-9-]*", key)
                or not isinstance(stack, dict) or set(stack) != {"description", "artifacts"}
                or not isinstance(stack["description"], str) or not stack["description"].strip()
                or not _identifiers(stack["artifacts"])
                or any(item not in artifacts for item in stack["artifacts"])):
            raise ValueError(f"invalid plugin stack: {key}")
        selected = set(stack["artifacts"])
        if any(not set(artifacts[item]["requires"]) <= selected for item in selected):
            raise ValueError(f"missing plugin dependency in stack: {key}")
        if (key == "baseline") != (not selected):
            raise ValueError("only the baseline stack may be empty")
    if "baseline" not in stacks:
        raise ValueError("missing baseline stack")
    return manifest


def load_manifest(path=MANIFEST_PATH):
    return validate_manifest(json.loads(Path(path).read_text()))


MANIFEST = load_manifest()
STACKS = tuple(MANIFEST["stacks"])


def selection(stack_name, *, manifest=None):
    manifest = validate_manifest(MANIFEST if manifest is None else manifest)
    if stack_name not in manifest["stacks"]:
        raise ValueError(f"unknown plugin stack {stack_name!r}; choose {', '.join(manifest['stacks'])}")
    stack = manifest["stacks"][stack_name]
    artifacts = [copy.deepcopy(manifest["artifacts"][key]) for key in stack["artifacts"]]
    return {"stack": stack_name, "paper_version": manifest["paper_version"],
            "description": stack["description"], "artifacts": artifacts,
            "expected_plugins": {item["name"]: item["version"] for item in artifacts}}


def _verify(path, artifact):
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"plugin artifact is not a regular file: {path}")
    if path.stat().st_size > MAX_ARTIFACT_BYTES:
        raise ValueError(f"plugin artifact exceeds size limit: {path}")
    with path.open("rb") as source:
        actual = hashlib.file_digest(source, "sha256").hexdigest()
    if actual != artifact["sha256"]:
        raise ValueError(f"plugin checksum mismatch: {path}: {actual}")


def _download(artifact, cache):
    path = cache / artifact["filename"]
    if path.exists() or path.is_symlink():
        # Never trust a warm cache or silently replace evidence of corruption.
        _verify(path, artifact)
        return path
    request = urllib.request.Request(artifact["url"], headers={
        "User-Agent": "rift-online-acceptance/1.0 (https://github.com/Jotrorox/rift)"})
    with tempfile.TemporaryDirectory(prefix=".plugin-download-", dir=cache) as temporary:
        pending = Path(temporary) / "artifact.jar"
        with urllib.request.urlopen(request, timeout=60) as response, pending.open("wb") as output:
            if not _https(response.geturl()):
                raise ValueError("plugin download redirected away from HTTPS")
            total = 0
            while block := response.read(1024 * 1024):
                total += len(block)
                if total > MAX_ARTIFACT_BYTES:
                    raise ValueError("plugin download exceeds size limit")
                output.write(block)
        _verify(pending, artifact)
        pending.replace(path)
    return path


def install(stack_name, destination, *, cache=None, manifest=None):
    """Install every pinned artifact or raise; return JSON-serializable evidence.

    Revalidates cached and installed jars. Existing identical jars are allowed,
    but conflicting files and symlinks fail instead of being overwritten.
    """
    evidence = selection(stack_name, manifest=manifest)
    destination = Path(destination)
    cache = Path(CACHE if cache is None else cache)
    for directory in (destination, cache):
        if directory.is_symlink():
            raise ValueError(f"plugin directory must not be a symlink: {directory}")
        directory.mkdir(parents=True, exist_ok=True)
    for artifact in evidence["artifacts"]:
        target = destination / artifact["filename"]
        if target.exists() or target.is_symlink():
            _verify(target, artifact)
    # Complete downloads before installing so a missing artifact cannot leave
    # a partial plugin combination in the Paper fixture.
    downloads = [_download(artifact, cache) for artifact in evidence["artifacts"]]
    for artifact, source in zip(evidence["artifacts"], downloads):
        target = destination / artifact["filename"]
        if not target.exists():
            with tempfile.TemporaryDirectory(prefix=".plugin-install-", dir=destination) as temporary:
                pending = Path(temporary) / "artifact.jar"
                shutil.copyfile(source, pending)
                _verify(pending, artifact)
                pending.replace(target)
        _verify(target, artifact)
    return evidence
