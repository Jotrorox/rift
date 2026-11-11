#!/usr/bin/env python3
"""Generate crates/rift-translation/data/tags from Mojang's own server jars.

Writes two files for the translation crate's misc codec:

- data/tags/vanilla.json: the resolved vanilla tag set the official server
  sends in its update_tags packet, for every protocol from 1.13 on, and the
  vanilla entry order of every data-driven (synchronised, not built-in)
  registry those tags refer to, since tag entries are network IDs in that
  order. Each protocol is stored as a change to the one before it: a tag or
  registry mapped to null is removed, anything listed replaces the previous
  value.
- data/tags/misc.json: small registries the crate's per-version data lacks
  because the server reports did not exist yet: 1.13's stat types, custom
  statistics and fluids, and 1.12's statistic names and legacy entity names.

Inputs are the vanilla server jars pinned in tests/servers.json (cached under
~/.cache/rift-via/servers, downloaded and verified when missing): the tag
files of the jar's built-in data pack, its data-driven registry entries, and
string constants of a few classes, read in bytecode order. The crate's
data/<protocol>.json files say which registries are built in.

Which registries the server sends tags for follows the game: blocks, items
and fluids from 1.13, entity types from 1.14, game events from 1.17, and from
1.18.2 every built-in registry plus the data-driven registries the server
synchronises to clients (SYNCED_DYNAMIC). With the real-traffic corpus
present (~/.cache/rift-via/corpus or $RIFT_TRANSLATION_CORPUS), every
generated set is compared with what the official server really sent.

Usage (Python 3.11+, stdlib only):

    python3 scripts/translation_tags.py            # regenerate data/tags
    python3 scripts/translation_tags.py --check    # fail if data/tags is stale
"""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import struct
import sys
import tempfile
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[1]
CRATE = ROOT / "crates" / "rift-translation"
OUTPUT = CRATE / "data" / "tags"
CACHE = Path(os.environ.get("RIFT_VIA_CACHE", Path.home() / ".cache" / "rift-via"))
CORPUS = Path(os.environ.get("RIFT_TRANSLATION_CORPUS", CACHE / "corpus"))
SERVERS = json.loads((ROOT / "tests" / "servers.json").read_text())
PROTOCOLS = [(int(protocol), name) for protocol, name in re.findall(
    r'\((\d+), "([^"]+)"\)', (CRATE / "src" / "version.rs").read_text().split("#[cfg(test)]")[0])]

# Data-driven registries the server synchronises to clients (from 1.16.2 in
# the login packet, from 1.20.2 in registry_data). Their tags are sent like
# those of built-in registries; tags of other data-driven registries
# (structures, presets, configured features, villager trades) stay on the
# server.
SYNCED_DYNAMIC = {
    "worldgen/biome", "dimension_type", "chat_type", "damage_type", "trim_pattern",
    "trim_material", "banner_pattern", "painting_variant", "enchantment", "jukebox_song",
    "instrument", "wolf_variant", "wolf_sound_variant", "pig_variant", "pig_sound_variant",
    "cat_variant", "cat_sound_variant", "cow_variant", "cow_sound_variant",
    "chicken_variant", "chicken_sound_variant", "frog_variant", "zombie_nautilus_variant",
    "test_environment", "test_instance", "dialog", "world_clock", "timeline",
    "decorated_pot_pattern", "block_transformer", "sulfur_cube_archetype",
    "worldgen/block_state_provider",
}

# Tag directory names before 1.21 used plural registry names.
PLURAL = {"blocks": "block", "items": "item", "fluids": "fluid",
          "entity_types": "entity_type", "game_events": "game_event"}


def log(message):
    print(message, file=sys.stderr, flush=True)


def server(version):
    matches = [fixture for key, fixture in SERVERS.items()
               if key.startswith("vanilla") and fixture["version"] == version]
    if len(matches) != 1:
        raise SystemExit(f"tests/servers.json has no single vanilla {version} server")
    return matches[0]


def verify(path, fixture):
    with path.open("rb") as source:
        actual = hashlib.file_digest(source, fixture["algorithm"]).hexdigest()
    if actual != fixture["checksum"]:
        raise SystemExit(f"checksum mismatch: {path}: {actual}")


def jar(version):
    """The game jar of a release; since 1.18 the server jar is a bundler."""
    fixture = server(version)
    directory = CACHE / "servers"
    directory.mkdir(parents=True, exist_ok=True)
    artifact = directory / fixture["filename"]
    if not artifact.exists():
        log(f"Downloading {version}...")
        request = urllib.request.Request(fixture["url"], headers={
            "User-Agent": "rift-translation-data/0.1.0 (https://github.com/Jotrorox/rift)"})
        with tempfile.TemporaryDirectory(dir=directory) as temporary:
            pending = Path(temporary) / "download"
            with urllib.request.urlopen(request, timeout=300) as response:
                pending.write_bytes(response.read())
            verify(pending, fixture)
            pending.replace(artifact)
    verify(artifact, fixture)
    archive = zipfile.ZipFile(artifact)
    nested = [name for name in archive.namelist()
              if name.startswith("META-INF/versions/") and name.endswith(".jar")]
    if nested:
        return zipfile.ZipFile(io.BytesIO(archive.read(nested[0])))
    return archive


def crate_registries(protocol):
    return json.loads((CRATE / "data" / f"{protocol}.json").read_text())["registries"]


# --- Class files -----------------------------------------------------------

def constants(data):
    """The constant pool of a class file and the offset after it."""
    count = struct.unpack_from(">H", data, 8)[0]
    pool = [None] * count
    offset = 10
    index = 1
    while index < count:
        tag = data[offset]
        if tag == 1:
            size = struct.unpack_from(">H", data, offset + 1)[0]
            pool[index] = ("utf8", data[offset + 3:offset + 3 + size].decode("utf-8", "replace"))
            offset += 3 + size
        elif tag in (3, 4):
            offset += 5
        elif tag in (5, 6):
            offset += 9
            index += 1
        elif tag == 7:
            pool[index] = ("class", struct.unpack_from(">H", data, offset + 1)[0])
            offset += 3
        elif tag == 8:
            pool[index] = ("string", struct.unpack_from(">H", data, offset + 1)[0])
            offset += 3
        elif tag in (9, 10, 11, 12, 17, 18):
            offset += 5
        elif tag == 15:
            offset += 4
        elif tag in (16, 19, 20):
            offset += 3
        else:
            raise ValueError(f"unknown constant tag {tag}")
        index += 1
    return pool, offset


ONE = {0x10, 0x12, 0x15, 0x16, 0x17, 0x18, 0x19, 0x36, 0x37, 0x38, 0x39, 0x3a, 0xa9, 0xbc}
TWO = {0x11, 0x13, 0x14, 0x84, 0xbb, 0xbd, 0xc0, 0xc1, 0xc6, 0xc7,
       *range(0x99, 0xa9), *range(0xb2, 0xb9)}
FOUR = {0xb9, 0xba, 0xc8, 0xc9}


def loaded_constants(data):
    """Per method, the String and class constants its code loads, in order:
    ("string", text) and ("class", name) pairs."""
    pool, offset = constants(data)

    def utf8(index):
        return pool[index][1]

    def attributes(offset):
        count = struct.unpack_from(">H", data, offset)[0]
        offset += 2
        found = []
        for _ in range(count):
            name, size = struct.unpack_from(">HI", data, offset)
            found.append((utf8(name), offset + 6, size))
            offset += 6 + size
        return found, offset

    offset += 6
    interfaces = struct.unpack_from(">H", data, offset)[0]
    offset += 2 + 2 * interfaces
    for _ in range(2):  # Fields, then methods.
        count = struct.unpack_from(">H", data, offset)[0]
        offset += 2
        methods = []
        for _ in range(count):
            found, offset = attributes(offset + 6)
            methods.append(found)
    result = []
    for found in methods:
        loads = []
        for name, start, _ in found:
            if name != "Code":
                continue
            length = struct.unpack_from(">I", data, start + 4)[0]
            code = data[start + 8:start + 8 + length]
            position = 0
            while position < len(code):
                opcode = code[position]
                if opcode in (0x12, 0x13):
                    index = code[position + 1] if opcode == 0x12 else \
                        struct.unpack_from(">H", code, position + 1)[0]
                    kind, value = pool[index] or (None, None)
                    if kind == "string":
                        loads.append(("string", utf8(value)))
                    elif kind == "class":
                        loads.append(("class", utf8(value)))
                if opcode in ONE:
                    position += 2
                elif opcode in TWO:
                    position += 3
                elif opcode == 0xc5:
                    position += 4
                elif opcode in FOUR:
                    position += 5
                elif opcode == 0xc4:
                    position += 6 if code[position + 1] == 0x84 else 4
                elif opcode in (0xaa, 0xab):
                    position = (position + 4) & ~3
                    if opcode == 0xaa:
                        low, high = struct.unpack_from(">ii", code, position + 4)
                        position += 12 + 4 * (high - low + 1)
                    else:
                        pairs = struct.unpack_from(">i", code, position + 4)[0]
                        position += 8 + 8 * pairs
                else:
                    position += 1
        result.append(loads)
    return result


def classes_with(archive, *needles):
    """Every class whose constants include all of `needles`."""
    encoded = [needle.encode() for needle in needles]
    for name in archive.namelist():
        if name.endswith(".class"):
            data = archive.read(name)
            if all(needle in data for needle in encoded):
                yield name, data


def strings_in_order(archive, *needles):
    """The String constants loaded by the one method that loads all of
    `needles`, in bytecode order."""
    found = []
    for name, data in classes_with(archive, *needles):
        for loads in loaded_constants(data):
            strings = [value for kind, value in loads if kind == "string"]
            if all(needle in strings for needle in needles):
                found.append((name, strings))
    if len(found) != 1:
        raise SystemExit(f"expected one method loading {needles}, found {[n for n, _ in found]}")
    return found[0][1]


# --- Tags ------------------------------------------------------------------

def tag_files(archive):
    """{registry: {tag: definition}} from the jar's built-in data pack."""
    prefix = "data/minecraft/tags/"
    found = {}
    for name in archive.namelist():
        if not name.startswith(prefix) or not name.endswith(".json"):
            continue
        # Registry names have one path segment, or two under worldgen/; tag
        # names may contain slashes themselves (mineable/axe).
        parts = name[len(prefix):-len(".json")].split("/")
        split = 2 if parts[0] == "worldgen" else 1
        registry = "/".join(parts[:split])
        registry = PLURAL.get(registry, registry)
        tag = "/".join(parts[split:])
        found.setdefault(registry, {})[tag] = json.loads(archive.read(name))
    return found


def resolve(registry, tags, known):
    """Every tag of a registry resolved to entry names, in the game's order
    (first occurrence wins). `known` is the set of entry names."""
    resolved = {}

    def visit(tag, stack):
        if tag in resolved:
            return resolved[tag]
        if tag in stack:
            raise SystemExit(f"tag cycle in {registry}: {tag}")
        entries = []
        for value in tags[tag].get("values", []):
            required = True
            if isinstance(value, dict):
                required = value.get("required", True)
                value = value["id"]
            if value.startswith("#"):
                name = strip(value[1:])
                if name not in tags:
                    if required:
                        raise SystemExit(f"{registry} tag {tag} refers to missing tag {name}")
                    continue
                entries.extend(visit(name, stack | {tag}))
            else:
                name = strip(value)
                if name not in known:
                    if required:
                        raise SystemExit(f"{registry} tag {tag} names unknown entry {name}")
                    continue
                entries.append(name)
        unique = list(dict.fromkeys(entries))
        resolved[tag] = unique
        return unique

    for tag in sorted(tags):
        visit(tag, frozenset())
    return resolved


def strip(name):
    return name[len("minecraft:"):] if name.startswith("minecraft:") else name


def dynamic_entries(archive, registry, protocol):
    """The vanilla entries of a data-driven registry in network order: sorted
    by name, as the server loads them from its data pack, except biomes before
    1.19.3, which were registered in code in the order Biomes declares them."""
    prefix = f"data/minecraft/{registry}/"
    files = sorted(strip_json(name[len(prefix):]) for name in archive.namelist()
                   if name.startswith(prefix) and name.endswith(".json"))
    if files:
        return files
    if registry == "worldgen/biome":
        names = strings_in_order(archive, "the_void", "sunflower_plains")
        return names if protocol < 761 else sorted(names)
    raise SystemExit(f"no vanilla {registry} entries in protocol {protocol}")


def strip_json(name):
    return name[:-len(".json")]


def synced(protocol, registry, builtin):
    if protocol < 477:
        return registry in ("block", "item", "fluid")
    if protocol < 755:
        return registry in ("block", "item", "fluid", "entity_type")
    if protocol < 758:
        return registry in ("block", "item", "fluid", "entity_type", "game_event")
    return registry in builtin or registry in SYNCED_DYNAMIC


def fluids(protocol, registries, archive):
    """1.13 reports no fluid registry; read its order from the jar."""
    if "fluid" in registries:
        return [name for name in registries["fluid"] if name]
    return jar_fluids(archive)


def jar_fluids(archive):
    """Fluid names in registration order, from the class that registers them
    (it also loads an error message, which is not a name)."""
    names = strings_in_order(archive, "empty", "flowing_water", "flowing_lava")
    return [name for name in names if re.fullmatch(r"[a-z_]+", name)]


def version_tags(protocol, release):
    archive = jar(release)
    registries = crate_registries(protocol)
    builtin = set(registries)
    tags = {}
    orders = {}
    for registry, definitions in sorted(tag_files(archive).items()):
        if not synced(protocol, registry, builtin):
            continue
        if registry == "fluid":
            known = fluids(protocol, registries, archive)
        elif registry in builtin:
            known = [name for name in registries[registry] if name]
        else:
            known = dynamic_entries(archive, registry, protocol)
            orders[registry] = known
        tags[registry] = resolve(registry, definitions, set(known))
    return tags, orders


def delta(previous, current):
    change = {}
    for registry in sorted(set(previous) | set(current)):
        if registry not in current:
            change[registry] = None
            continue
        before = previous.get(registry, {})
        after = current[registry]
        if isinstance(after, list):
            if before != after:
                change[registry] = after
            continue
        inner = {}
        for tag in sorted(set(before) | set(after)):
            if tag not in after:
                inner[tag] = None
            elif before.get(tag) != after[tag]:
                inner[tag] = after[tag]
        if inner:
            change[registry] = inner
    return change


# --- Corpus check ----------------------------------------------------------

class Reader:
    def __init__(self, data):
        self.data = data
        self.offset = 0

    def varint(self):
        value = 0
        for shift in range(0, 35, 7):
            byte = self.data[self.offset]
            self.offset += 1
            value |= (byte & 127) << shift
            if not byte & 128:
                return value
        raise ValueError("VarInt too long")

    def string(self):
        size = self.varint()
        value = self.data[self.offset:self.offset + size].decode()
        self.offset += size
        return value


def corpus_tags(protocol):
    """{registry: {tag: [ids]}} of every update_tags packet the official
    server sent, or None without a recording."""
    path = CORPUS / f"{protocol}.jsonl"
    if not path.exists():
        return None
    packets = json.loads((CRATE / "data" / f"{protocol}.json").read_text())["packets"]
    found = []
    for line in path.read_text().splitlines():
        record = json.loads(line)
        if record["d"] != "c" or record["s"] not in ("configuration", "play"):
            continue
        names = packets.get(f"{record['s']}/clientbound", [])
        if record["id"] >= len(names) or names[record["id"]] != "update_tags":
            continue
        reader = Reader(bytes.fromhex(record["hex"]))
        if protocol < 755:
            fixed = ["block", "item", "fluid"] + (["entity_type"] if protocol >= 477 else [])
            keys = iter(fixed)
            count = len(fixed)
        else:
            count = reader.varint()
            keys = None
        result = {}
        for _ in range(count):
            registry = next(keys) if keys else strip(reader.string())
            tags = {}
            for _ in range(reader.varint()):
                tag = strip(reader.string())
                tags[tag] = [reader.varint() for _ in range(reader.varint())]
            result[registry] = tags
        if reader.offset != len(reader.data):
            raise SystemExit(f"corpus {protocol}: trailing update_tags data")
        found.append(result)
    return found


def check_corpus(protocol, tags, orders):
    recorded = corpus_tags(protocol)
    if recorded is None:
        return "no corpus"
    if not recorded:
        raise SystemExit(f"corpus {protocol} has no update_tags packet")
    registries = crate_registries(protocol)
    for packet in recorded:
        if set(packet) != set(tags):
            raise SystemExit(f"{protocol}: server sent tags of {sorted(packet)}, "
                             f"generated {sorted(tags)}")
        for registry, sent in packet.items():
            if registry == "fluid" and "fluid" not in registries:
                names = None
            else:
                names = orders.get(registry) or registries.get(registry)
            if set(sent) != set(tags[registry]):
                raise SystemExit(f"{protocol} {registry}: tag names differ: "
                                 f"{sorted(set(sent) ^ set(tags[registry]))[:10]}")
            for tag, ids in sent.items():
                if names is None:
                    if len(ids) != len(tags[registry][tag]):
                        raise SystemExit(f"{protocol} {registry} {tag}: entry count differs")
                    continue
                actual = [names[id] if 0 <= id < len(names) else f"#{id}" for id in ids]
                if set(actual) != set(tags[registry][tag]):
                    raise SystemExit(f"{protocol} {registry} {tag}: sent {sorted(actual)[:8]}, "
                                     f"generated {sorted(tags[registry][tag])[:8]}")
    return f"{len(recorded)} packet(s) match"


# --- Small registries ------------------------------------------------------

def misc():
    result = {"stat_type": {}, "custom_stat": {}, "fluid": {}}
    for protocol, release in PROTOCOLS:
        if not 393 <= protocol < 477:
            continue
        archive = jar(release)
        names = strings_in_order(archive, "leave_game", "killed_by")
        split = names.index("custom") + 1
        result["stat_type"][str(protocol)] = names[:split]
        result["custom_stat"][str(protocol)] = names[split:]
        result["fluid"][str(protocol)] = jar_fluids(archive)
    # The method is the same in 1.14, which has a registries report to check
    # the extraction against.
    names = strings_in_order(jar("1.14"), "leave_game", "killed_by")
    expected = crate_registries(477)
    split = names.index("custom") + 1
    if names[:split] != expected["stat_type"] or names[split:] != expected["custom_stat"]:
        raise SystemExit("1.14 statistics extracted from the jar differ from its report")
    if result["fluid"]["404"] != expected["fluid"]:
        raise SystemExit("1.13.2 fluids differ from 1.14's")

    legacy = jar("1.12.2")
    # StatList registers every custom statistic as (ID, translation key).
    strings = strings_in_order(legacy, "stat.leaveGame", "stat.shulkerBoxOpened")
    first = strings.index("stat.leaveGame")
    result["legacy_custom_stat"] = strings[first::2]
    # EntityList registers (ID, name, class, legacy name) for every entity.
    for name, data in classes_with(legacy, "PigZombie", "zombie_pigman"):
        for loads in loaded_constants(data):
            triples = [(loads[index][1], loads[index + 2][1])
                       for index in range(len(loads) - 2)
                       if loads[index][0] == "string" and loads[index + 1][0] == "class"
                       and loads[index + 2][0] == "string"
                       # Minecarts compute their legacy name: skip them.
                       and loads[index + 2][1][:1].isupper()]
            if len(triples) > 50:
                result["legacy_entity"] = dict(triples)
    if "legacy_entity" not in result:
        raise SystemExit("1.12.2 EntityList registrations not found")
    known = {name for name in crate_registries(340)["entity_type"] if name}
    unknown = set(result["legacy_entity"]) - known
    if unknown:
        raise SystemExit(f"1.12.2 entity names unknown to its registry: {sorted(unknown)}")
    return result


def layout(versions):
    """vanilla.json with one tag or entry list per line, so that changes to
    the data read well in a diff."""
    def value(item):
        return json.dumps(item, separators=(",", ":"))

    lines = ['{"protocols": {']
    for index, (protocol, change) in enumerate(versions.items()):
        lines.append(f' "{protocol}": {{')
        for section_index, section in enumerate(("entries", "tags")):
            registries = change[section]
            lines.append(f'  "{section}": {{')
            for registry_index, (registry, tags) in enumerate(registries.items()):
                comma = "," if registry_index + 1 < len(registries) else ""
                if not isinstance(tags, dict):
                    lines.append(f'   "{registry}": {value(tags)}{comma}')
                    continue
                lines.append(f'   "{registry}": {{')
                for tag_index, (tag, entries) in enumerate(tags.items()):
                    inner = "," if tag_index + 1 < len(tags) else ""
                    lines.append(f'    "{tag}": {value(entries)}{inner}')
                lines.append(f'   }}{comma}')
            lines.append("  }" + ("," if section_index == 0 else ""))
        lines.append(" }" + ("," if index + 1 < len(versions) else ""))
    lines.append("}}")
    text = "\n".join(lines) + "\n"
    if json.loads(text) != {"protocols": versions}:
        raise SystemExit("vanilla.json layout does not round-trip")
    return text


def outputs():
    versions = {}
    previous_tags, previous_orders = {}, {}
    notes = []
    for protocol, release in PROTOCOLS:
        if protocol < 393:
            continue
        log(f"{release}...")
        tags, orders = version_tags(protocol, release)
        notes.append(f"{release}: {check_corpus(protocol, tags, orders)}")
        versions[str(protocol)] = {
            "tags": delta(previous_tags, tags),
            "entries": delta(previous_orders, orders),
        }
        previous_tags, previous_orders = tags, orders
    for note in notes:
        log(note)
    return {
        "vanilla.json": layout(versions),
        "misc.json": json.dumps(misc(), indent=1, sort_keys=True) + "\n",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="fail if generated files differ")
    args = parser.parse_args()
    files = outputs()
    stale = sorted(name for name, text in files.items()
                   if not (OUTPUT / name).exists() or (OUTPUT / name).read_text() != text)
    if args.check:
        if stale:
            raise SystemExit(f"stale tag data: {', '.join(stale)}; run scripts/translation_tags.py")
        return
    OUTPUT.mkdir(parents=True, exist_ok=True)
    for name in stale:
        (OUTPUT / name).write_text(files[name])
        log(f"wrote {OUTPUT / name}")


if __name__ == "__main__":
    main()
