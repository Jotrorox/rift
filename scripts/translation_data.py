#!/usr/bin/env python3
"""Generate protocol translation data from Mojang's own server data reports.

Each pinned, checksum-verified vanilla server in tests/servers.json is run with
its bundled data generator (`--reports`). The reports list every built-in
registry with its network ID and every packet ID per protocol state. From
them this script writes, under src/protocol/translation/data/:

* `<version>.json`: the small registries that select wire layouts (data
  component types, particle types, slot displays, ...) and the configuration
  and play packet names, all in network-ID order;
* `<from>-<to>.json`: for an adjacent pair, the registries whose IDs changed,
  expressed as inserted and removed entries, plus any packet ID changes.

Rift remaps every other ID by this diff, so a reordered registry is rejected
here rather than translated incorrectly. Entries that exist only in the newer
version need an explicit stand-in for older peers, listed in FALLBACKS.

Requires Java 21+ (the fixtures' runtime). Usage:

    python3 scripts/translation_data.py              # regenerate everything
    python3 scripts/translation_data.py --check      # fail if files are stale
"""

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
CACHE = ROOT / "target" / "minecraft"
OUTPUT = ROOT / "src" / "protocol" / "translation" / "data"
SERVERS = json.loads((ROOT / "tests" / "servers.json").read_text())

# Adjacent steps Rift translates, oldest first: (older fixture, newer fixture).
# 1.21.7 and 1.21.8 share protocol 772; 1.21.8 is the release players run.
STEPS = [("vanilla-1.21.6", "vanilla-1.21.8")]

# Registries whose order selects a wire layout. Their names, in network-ID
# order, are embedded so a layout can dispatch on names instead of numbers.
LAYOUT_REGISTRIES = [
    "minecraft:data_component_type",
    "minecraft:particle_type",
    "minecraft:consume_effect_type",
    "minecraft:slot_display",
    "minecraft:recipe_display",
    "minecraft:stat_type",
    "minecraft:position_source_type",
    "minecraft:data_component_predicate_type",
]

# Stand-ins shown to an older peer for entries its version does not have.
FALLBACKS = {
    "minecraft:item": {
        "minecraft:music_disc_lava_chicken": "minecraft:music_disc_creator",
    },
    "minecraft:sound_event": {
        "minecraft:music_disc.lava_chicken": "minecraft:music_disc.creator",
    },
}


def download(name):
    fixture = SERVERS[name]
    directory = CACHE / "downloads"
    directory.mkdir(parents=True, exist_ok=True)
    artifact = directory / fixture["filename"]
    if not artifact.exists():
        print(f"Downloading {name}...", file=sys.stderr, flush=True)
        request = urllib.request.Request(fixture["url"], headers={
            "User-Agent": "rift-translation-data/0.1.0 (https://github.com/Jotrorox/rift)"
        })
        with tempfile.TemporaryDirectory(dir=directory) as temporary:
            pending = Path(temporary) / "download"
            with urllib.request.urlopen(request, timeout=120) as response:
                pending.write_bytes(response.read())
            verify(pending, fixture)
            pending.replace(artifact)
    verify(artifact, fixture)
    return artifact


def verify(path, fixture):
    with path.open("rb") as source:
        actual = hashlib.file_digest(source, fixture["algorithm"]).hexdigest()
    if actual != fixture["checksum"]:
        raise ValueError(f"checksum mismatch: {path}: {actual}")


def reports(name):
    """Run the server's data generator once and cache its reports."""
    fixture = SERVERS[name]
    directory = CACHE / "reports" / f"{fixture['version']}-{fixture['checksum'][:12]}"
    done = directory / "reports" / "registries.json"
    if not done.exists():
        jar = download(name)
        print(f"Generating reports for {fixture['version']}...", file=sys.stderr, flush=True)
        with tempfile.TemporaryDirectory(dir=CACHE) as temporary:
            subprocess.run(
                ["java", "-DbundlerMainClass=net.minecraft.data.Main", "-jar", str(jar),
                 "--reports", "--output", str(Path(temporary) / "out")],
                cwd=temporary, check=True, stdout=subprocess.DEVNULL, timeout=600)
            directory.parent.mkdir(parents=True, exist_ok=True)
            (Path(temporary) / "out").replace(directory)
    load = lambda report: json.loads((directory / "reports" / report).read_text())
    return load("registries.json"), load("packets.json")


def ordered(registry):
    entries = sorted(registry["entries"].items(), key=lambda entry: entry[1]["protocol_id"])
    names = [name for name, _ in entries]
    if [entry["protocol_id"] for _, entry in entries] != list(range(len(names))):
        raise ValueError("registry IDs are not dense")
    return names


def version_data(name):
    registries, packets = reports(name)
    fixture = SERVERS[name]
    return {
        "version": fixture["version"],
        "protocol": fixture["protocol"],
        "registries": {key: ordered(registries[key]) for key in LAYOUT_REGISTRIES},
        # Packet names in ID order; translation dispatches on names.
        "packets": {
            f"{state}/{direction}": ordered({"entries": packets[state][direction]})
            for state in ("configuration", "play")
            for direction in ("clientbound", "serverbound")
        },
    }


def registry_diff(key, old, new):
    """Express new IDs as old IDs plus insertions/removals, keeping order."""
    removed = [name for name in old if name not in set(new)]
    inserted = [name for name in new if name not in set(old)]
    kept_old = [name for name in old if name in set(new)]
    kept_new = [name for name in new if name in set(old)]
    if kept_old != kept_new:
        raise ValueError(f"{key}: entries were reordered; extend the diff format first")
    fallbacks = FALLBACKS.get(key, {})
    missing = [name for name in inserted + removed if name not in fallbacks]
    if missing:
        raise ValueError(f"{key}: add FALLBACKS for {missing}")
    for names, other, label in ((inserted, old, "older"), (removed, new, "newer")):
        for name in names:
            if fallbacks[name] not in other:
                raise ValueError(f"{key}: fallback {fallbacks[name]} does not exist in the {label} version")
    return {
        "old_size": len(old),
        "new_size": len(new),
        # [new ID, name, stand-in old ID, stand-in name], and the same for
        # removed entries with the stand-in from the newer version.
        "inserted": [[new.index(name), name, old.index(fallbacks[name]), fallbacks[name]]
                     for name in inserted],
        "removed": [[old.index(name), name, new.index(fallbacks[name]), fallbacks[name]]
                    for name in removed],
    }


def packet_diff(old, new):
    changes = {}
    for state in sorted(set(old) | set(new)):
        for direction in ("clientbound", "serverbound"):
            a = {k: v["protocol_id"] for k, v in old.get(state, {}).get(direction, {}).items()}
            b = {k: v["protocol_id"] for k, v in new.get(state, {}).get(direction, {}).items()}
            if a != b:
                changes[f"{state}/{direction}"] = {
                    name: [a.get(name), b.get(name)]
                    for name in sorted(set(a) | set(b)) if a.get(name) != b.get(name)
                }
    return changes


def step_data(older, newer):
    old_registries, old_packets = reports(older)
    new_registries, new_packets = reports(newer)
    changed = {}
    for key in sorted(set(old_registries) | set(new_registries)):
        if key not in old_registries or key not in new_registries:
            raise ValueError(f"{key}: registry added or removed")
        old, new = ordered(old_registries[key]), ordered(new_registries[key])
        if old != new:
            changed[key] = registry_diff(key, old, new)
    return {
        "from": SERVERS[older]["protocol"],
        "to": SERVERS[newer]["protocol"],
        "registries": changed,
        "packets": packet_diff(old_packets, new_packets),
    }


def render(value):
    return json.dumps(value, indent=1, sort_keys=True) + "\n"


def outputs():
    files = {}
    for older, newer in STEPS:
        for name in (older, newer):
            files[f"{SERVERS[name]['version']}.json"] = render(version_data(name))
        files[f"{SERVERS[older]['version']}-{SERVERS[newer]['version']}.json"] = render(
            step_data(older, newer))
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="fail if generated files differ")
    args = parser.parse_args()
    files = outputs()
    stale = [name for name, text in files.items()
             if not (OUTPUT / name).exists() or (OUTPUT / name).read_text() != text]
    if args.check:
        if stale:
            raise SystemExit(f"stale translation data: {', '.join(stale)}")
        return
    OUTPUT.mkdir(parents=True, exist_ok=True)
    for name in stale:
        (OUTPUT / name).write_text(files[name])
        print(f"wrote {OUTPUT / name}")


if __name__ == "__main__":
    main()
