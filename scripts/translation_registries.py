#!/usr/bin/env python3
"""Generate the vanilla synchronised registries of every protocol from 1.16 to 26.3.

Since 1.16 a server sends the client registries that are not built into the
game: a dimension list (1.16, 1.16.1) or a registry codec (1.16.2 to 1.20.1)
in the play `login` packet, the same codec in one configuration-phase
`registry_data` packet (1.20.2 to 1.20.4), and one `registry_data` packet per
registry from 1.20.5 on. Translation needs each version's vanilla contents to
fill in registries a client expects but an older server never sends, and to
recognise unmodified vanilla entries.

The contents come from what the official server of each version really sends:
the sessions tests/translation_corpus.py records from the vanilla server jars
pinned in tests/servers.json (~/.cache/rift-via/corpus/<protocol>.jsonl, or
$RIFT_TRANSLATION_CORPUS). A vanilla server's registry data is deterministic,
so this is a pure function of the pinned jar; taking it from the wire rather
than from the jar's datapack JSON keeps every NBT type and field exactly as
the network codec writes it, which the JSON does not record (ints and floats,
fields the network codec leaves out). Entry names are checked against the
jar's built-in datapack (data/minecraft/<registry>/) wherever the jar has one.

Output, under crates/rift-translation/data/registries/:

- index.json: per protocol, the registries the server syncs in the order it
  sends them.
- <registry>.json (worldgen/biome.json and so on): a pool of distinct entry
  elements as SNBT, and per run of protocols with identical contents the
  entry names in network-ID order with their element's pool index (and the
  explicit codec IDs where they are not 0, 1, 2, ...).

SNBT here is the game's notation restricted to what this needs: typed number
suffixes (b, s, L, f, d; plain ints), double-quoted strings with \\ and \"
escapes, keys quoted unless they are plain [A-Za-z0-9_.+-], typed arrays
[B;...], [I;...], [L;...]. Floats are written with the fewest digits that
parse back to the same bits.

Usage (Python 3.11+, stdlib only):

    python3 scripts/translation_registries.py          # regenerate
    python3 scripts/translation_registries.py --check  # fail if stale
"""

import argparse
from fractions import Fraction
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import sys
import zipfile

ROOT = Path(__file__).resolve().parents[1]
CRATE = ROOT / "crates" / "rift-translation"
OUTPUT = CRATE / "data" / "registries"
CACHE = Path(os.environ.get("RIFT_VIA_CACHE", Path.home() / ".cache" / "rift-via"))
CORPUS = Path(os.environ.get("RIFT_TRANSLATION_CORPUS", CACHE / "corpus"))
SERVERS = json.loads((ROOT / "tests" / "servers.json").read_text())
PROTOCOLS = [(int(protocol), name) for protocol, name in re.findall(
    r'\((\d+), "([^"]+)"\)', (CRATE / "src" / "version.rs").read_text().split("#[cfg(test)]")[0])]
FIRST = 735


class Reader:
    def __init__(self, data):
        self.data, self.at = data, 0

    def take(self, size):
        if size < 0 or self.at + size > len(self.data):
            raise ValueError("truncated packet")
        value = self.data[self.at:self.at + size]
        self.at += size
        return value

    def unpack(self, form):
        return struct.unpack(form, self.take(struct.calcsize(form)))[0]

    def varint(self):
        value = 0
        for shift in range(0, 35, 7):
            byte = self.take(1)[0]
            value |= (byte & 0x7F) << shift
            if not byte & 0x80:
                return value - (1 << 32) if value >= 1 << 31 else value
        raise ValueError("varint too long")

    def string(self):
        return self.take(self.varint()).decode()

    def done(self):
        if self.at != len(self.data):
            raise ValueError(f"{len(self.data) - self.at} trailing bytes")


def java_string(reader):
    raw = reader.take(reader.unpack(">H"))
    # Modified UTF-8; registry data is ASCII in practice, checked below.
    return raw.decode("utf-8")


def nbt_payload(reader, kind):
    if kind == 1:
        return ("b", reader.unpack(">b"))
    if kind == 2:
        return ("s", reader.unpack(">h"))
    if kind == 3:
        return ("i", reader.unpack(">i"))
    if kind == 4:
        return ("l", reader.unpack(">q"))
    if kind == 5:
        return ("f", reader.take(4))
    if kind == 6:
        return ("d", reader.take(8))
    if kind == 7:
        return ("B", [byte - 256 if byte > 127 else byte
                      for byte in reader.take(reader.unpack(">i"))])
    if kind == 8:
        return ("S", java_string(reader))
    if kind == 9:
        element, count = reader.unpack(">B"), reader.unpack(">i")
        if count and not element:
            raise ValueError("list of TAG_End")
        return ("L", [nbt_payload(reader, element) for _ in range(count)])
    if kind == 10:
        entries = []
        while True:
            element = reader.unpack(">B")
            if not element:
                return ("C", entries)
            name = java_string(reader)
            entries.append((name, nbt_payload(reader, element)))
    if kind == 11:
        return ("I", [reader.unpack(">i") for _ in range(reader.unpack(">i"))])
    if kind == 12:
        return ("Ls", [reader.unpack(">q") for _ in range(reader.unpack(">i"))])
    raise ValueError(f"NBT tag {kind}")


def nbt(reader, named):
    kind = reader.unpack(">B")
    if not kind:
        return None
    if named:
        reader.take(reader.unpack(">H"))
    return nbt_payload(reader, kind)


def compound(value):
    if value is None or value[0] != "C":
        raise ValueError("expected a compound")
    return dict(value[1])


def shortest_float(raw):
    """The shortest decimal that a correctly rounding parser reads back as
    exactly these f32 bits (exact arithmetic, no double rounding)."""
    bits = struct.unpack(">I", raw)[0]
    value = struct.unpack(">f", raw)[0]
    if value != value or value in (float("inf"), float("-inf")):
        raise ValueError("non-finite float in registry data")
    if value == 0:
        return "-0.0" if bits >> 31 else "0.0"
    exact = Fraction(value)
    neighbours = [Fraction(struct.unpack(">f", struct.pack(">I", other))[0])
                  for other in (bits - 1, bits + 1) if (other & 0x7F800000) != 0x7F800000]
    for digits in range(1, 10):
        text = f"{value:.{digits}g}"
        decimal = Fraction(text)
        distance = abs(decimal - exact)
        if all(distance < abs(decimal - other) for other in neighbours):
            return text if any(c in text for c in ".en") else text + ".0"
    raise ValueError("no round-tripping float representation")


PLAIN_KEY = re.compile(r"^[A-Za-z0-9_.+-]+$")


def quote(text):
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'


def snbt(value):
    kind, payload = value
    if kind == "b":
        return f"{payload}b"
    if kind == "s":
        return f"{payload}s"
    if kind == "i":
        return str(payload)
    if kind == "l":
        return f"{payload}L"
    if kind == "f":
        return shortest_float(payload) + "f"
    if kind == "d":
        double = struct.unpack(">d", payload)[0]
        if double != double or double in (float("inf"), float("-inf")):
            raise ValueError("non-finite double in registry data")
        return repr(double) + "d"
    if kind == "S":
        if not payload.isascii():
            raise ValueError(f"non-ASCII string {payload!r}")
        return quote(payload)
    if kind == "L":
        return "[" + ",".join(snbt(element) for element in payload) + "]"
    if kind == "C":
        return "{" + ",".join(
            (name if PLAIN_KEY.match(name) else quote(name)) + ":" + snbt(element)
            for name, element in payload) + "}"
    if kind == "B":
        return "[B;" + ",".join(f"{element}b" for element in payload) + "]"
    if kind == "I":
        return "[I;" + ",".join(str(element) for element in payload) + "]"
    if kind == "Ls":
        return "[L;" + ",".join(f"{element}L" for element in payload) + "]"
    raise ValueError(kind)


def packet_names(protocol):
    tables = json.loads((CRATE / "data" / f"{protocol}.json").read_text())["packets"]
    return {state: tables.get(f"{state}/clientbound", []) for state in ("configuration", "play")}


def corpus(protocol):
    path = CORPUS / f"{protocol}.jsonl"
    if not path.exists():
        sys.exit(f"{path} is missing: capture it with tests/translation_corpus.py")
    names = packet_names(protocol)
    for line in path.read_text().splitlines():
        record = json.loads(line)
        if record["d"] != "c" or record["s"] not in names:
            continue
        table = names[record["s"]]
        name = table[record["id"]] if record["id"] < len(table) else "?"
        yield record["s"], name, bytes.fromhex(record["hex"])


def codec_registries(protocol, codec):
    """(registry, [(name, id, element)]) from a dimension list or codec."""
    root = compound(codec)
    if protocol < 751:
        if set(root) != {"dimension"}:
            raise ValueError(f"unexpected dimension codec keys {sorted(root)}")
        return [("dimension_type", [(compound(element)["name"][1], index, element)
                                    for index, element in enumerate(root["dimension"][1])])]
    registries = []
    for key, value in codec[1]:
        body = compound(value)
        if body["type"][1] != key:
            raise ValueError(f"codec entry {key} has type {body['type'][1]}")
        entries = []
        for entry in body["value"][1]:
            entry = compound(entry)
            entries.append((entry["name"][1], entry["id"][1], entry["element"]))
        registries.append((key.removeprefix("minecraft:"), entries))
    return registries


def login_codec(protocol, body):
    reader = Reader(body)
    reader.unpack(">i")
    if protocol >= 751:
        reader.take(1)
    reader.take(2)
    for _ in range(reader.varint()):
        reader.string()
    return nbt(reader, True)


def registries(protocol):
    found = []
    for state, name, body in corpus(protocol):
        if protocol < 764 and state == "play" and name == "login":
            return codec_registries(protocol, login_codec(protocol, body))
        if protocol in (764, 765) and name == "registry_data":
            reader = Reader(body)
            codec = nbt(reader, False)
            reader.done()
            return codec_registries(protocol, codec)
        if protocol >= 766 and name == "registry_data":
            reader = Reader(body)
            registry = reader.string().removeprefix("minecraft:")
            entries = []
            for index in range(reader.varint()):
                entry = reader.string()
                if not reader.take(1)[0]:
                    sys.exit(f"{protocol} {registry} {entry} has no data: the corpus client "
                             "must not claim known packs")
                entries.append((entry, index, nbt(reader, False)))
            reader.done()
            found.append((registry, entries))
    if not found:
        sys.exit(f"no registry data in the {protocol} corpus")
    return found


def server_jar(protocol):
    """The pinned vanilla jar of the newest release with `protocol`, if cached."""
    release = dict(PROTOCOLS)[protocol]
    for fixture in SERVERS.values():
        if fixture.get("version") == release and "piston-data.mojang.com" in fixture.get("url", ""):
            for directory in (CACHE / "servers", ROOT / "target" / "minecraft" / "downloads"):
                path = directory / fixture["filename"]
                if path.exists():
                    digest = hashlib.new(fixture["algorithm"], path.read_bytes()).hexdigest()
                    if digest != fixture["checksum"]:
                        sys.exit(f"{path} does not match its pinned checksum")
                    return path, fixture
            return None, fixture
    sys.exit(f"tests/servers.json pins no vanilla jar for {release}")


def datapack_names(jar):
    """registry -> entry names in the jar's built-in datapack."""
    names = {}
    with zipfile.ZipFile(jar) as outer:
        inner = [name for name in outer.namelist()
                 if name.startswith("META-INF/versions/") and name.endswith(".jar")]
        archive = zipfile.ZipFile(outer.open(inner[0])) if inner else outer
        for path in archive.namelist():
            match = re.match(r"data/minecraft/(.+)/([^/]+)\.json$", path)
            if match and not match.group(1).startswith("tags/"):
                folder, entry = match.groups()
                # Nested folders are part of an entry's name (worldgen/biome
                # is a registry; trim_pattern/foo would be an entry path).
                names.setdefault(folder, set()).add("minecraft:" + entry)
    return names


def check_against_jar(protocol, found):
    jar, _ = server_jar(protocol)
    if jar is None:
        print(f"{protocol}: jar not cached, names not cross-checked", file=sys.stderr)
        return 0
    pack = datapack_names(jar)
    checked = 0
    for registry, entries in found:
        if registry in pack:
            names = {name for name, _, _ in entries}
            if names != pack[registry]:
                sys.exit(f"{protocol} {registry}: corpus and jar datapack disagree: "
                         f"{sorted(names ^ pack[registry])}")
            checked += 1
    return checked


def generate():
    index, per_registry = {}, {}
    for protocol, _ in PROTOCOLS:
        if protocol < FIRST:
            continue
        found = registries(protocol)
        checked = check_against_jar(protocol, found)
        print(f"{protocol}: {len(found)} registries, {checked} checked against the jar",
              file=sys.stderr)
        index[str(protocol)] = [registry for registry, _ in found]
        for registry, entries in found:
            names = [name for name, _, _ in entries]
            if len(set(names)) != len(names):
                sys.exit(f"{protocol} {registry} repeats an entry")
            elements = [snbt(element) for _, _, element in entries]
            ids = [entry_id for _, entry_id, _ in entries]
            per_registry.setdefault(registry, []).append((protocol, names, ids, elements))
    files = {"index": {"protocols": index}}
    for registry, versions in per_registry.items():
        pool, positions, runs = [], {}, []
        for protocol, names, ids, elements in versions:
            references = []
            for element in elements:
                if element not in positions:
                    positions[element] = len(pool)
                    pool.append(element)
                references.append(positions[element])
            run = {"names": names, "elements": references}
            if ids != list(range(len(ids))):
                run["ids"] = ids
            if runs and {k: v for k, v in runs[-1].items() if k != "protocols"} == run:
                runs[-1]["protocols"].append(protocol)
            else:
                runs.append({"protocols": [protocol], **run})
        files[registry] = {"entries": pool, "versions": runs}
    return files


def render(value):
    """Compact JSON with one pool entry or version run per line."""
    if isinstance(value, dict) and "entries" in value:
        lines = ["{", '  "entries": [']
        lines += [f"    {json.dumps(entry)}," for entry in value["entries"]]
        lines[-1] = lines[-1].rstrip(",")
        lines += ["  ],", '  "versions": [']
        lines += [f"    {json.dumps(run, separators=(',', ':'))}," for run in value["versions"]]
        lines[-1] = lines[-1].rstrip(",")
        lines += ["  ]", "}"]
        return "\n".join(lines) + "\n"
    lines = ["{", '  "protocols": {']
    lines += [f"    {json.dumps(protocol)}: {json.dumps(names)}," for protocol, names in
              value["protocols"].items()]
    lines[-1] = lines[-1].rstrip(",")
    lines += ["  }", "}"]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="fail if generated files differ")
    args = parser.parse_args()
    files = {OUTPUT / f"{name}.json": render(value) for name, value in generate().items()}
    existing = set(OUTPUT.rglob("*.json")) if OUTPUT.exists() else set()
    stale = sorted(path for path in files if not path.exists() or path.read_text() != files[path])
    extra = sorted(existing - set(files))
    if args.check:
        if stale or extra:
            sys.exit("stale: " + ", ".join(str(path.relative_to(ROOT)) for path in stale + extra))
        print("registries are up to date")
        return
    for path in extra:
        path.unlink()
    for path in stale:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(files[path])
    print(f"wrote {len(stale)} files, removed {len(extra)}")


if __name__ == "__main__":
    main()
