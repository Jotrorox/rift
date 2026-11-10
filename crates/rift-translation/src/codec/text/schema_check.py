#!/usr/bin/env python3
"""Decode the text codec's converted corpus packets with minecraft-data.

    RIFT_TEXT_DUMP=/tmp/text.jsonl cargo test -p rift-translation dump_conversions
    python3 crates/rift-translation/src/codec/text/schema_check.py /tmp/text.jsonl

Each line holds a packet converted from a recorded session into an adjacent
protocol; it must decode completely with the schema of that protocol's
release (tests/protodef.py, which shares no code with Rift). Protocols
without a minecraft-data schema are skipped.
"""

import collections
import json
import os
from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT / "tests"))
import protodef  # noqa: E402

DATA = ROOT / "crates" / "rift-translation" / "data"
REF = Path(os.environ.get("RIFT_MINECRAFT_DATA", Path.home() / ".cache/rift-via/ref/minecraft-data")) / "data"


def schemas():
    paths = json.loads((REF / "dataPaths.json").read_text())["pc"]
    cache = {}

    def schema(protocol):
        if protocol not in cache:
            name = json.loads((DATA / f"{protocol}.json").read_text())["name"]
            path = paths.get(name, {}).get("protocol")
            if path is None:
                cache[protocol] = None
            else:
                raw = json.loads((REF / path / "protocol.json").read_text())
                # protodef's corrections are for 1.21.6-1.21.8 schemas.
                try:
                    cache[protocol] = protodef.corrected(raw)
                except (KeyError, IndexError, TypeError):
                    cache[protocol] = raw
        return cache[protocol]
    return schema


def main(dump):
    schema = schemas()
    names = {}
    counts = collections.Counter()
    failures = []
    for line in open(dump):
        packet = json.loads(line)
        protocol = packet["p"]
        found = schema(protocol)
        if found is None:
            counts["skipped (no schema)"] += 1
            continue
        if protocol not in names:
            names[protocol] = json.loads((DATA / f"{protocol}.json").read_text())["packets"]
        ids = names[protocol][f"{packet['s']}/clientbound"]
        decoder = protodef.Decoder(found, packet["s"], "clientbound")
        try:
            decoder.packet(ids.index(packet["name"]), bytes.fromhex(packet["hex"]))
            counts["decoded"] += 1
        except Exception as error:  # noqa: BLE001 - report every failure
            failures.append(f"{packet['from']}->{protocol} {packet['name']}: {error}")
    for failure in failures:
        print("FAIL", failure)
    print(dict(counts), "failed:", len(failures))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
