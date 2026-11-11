#!/usr/bin/env python3
"""Decode what a translation step produces with the receiving version's
minecraft-data schema (tests/protodef.py), as an oracle independent of the
step's own layouts.

    /opt/homebrew/bin/python3 tests/translation_step_schema.py --backend 767 --client 768 \
        --own login,respawn,player_position

replays the recorded session of --backend through the translator for a
--client player (crates/rift-translation/examples/replay.rs) and decodes
every packet that comes out, in the protocol of the side it goes to. Packets
named in --own are the ones the step under test converts: a decoding failure
among them fails the check. Everything else is reported for information
(packets the shared codecs own pass through unconverted until those land).
"""

import argparse
import collections
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
import protodef  # noqa: E402

# Our names for packets whose minecraft-data names differ are resolved by
# packet ID, so only corrections of the schema itself live here.
SERIALIZERS_1_21 = protodef._mapping([
    "crafting_shaped", "crafting_shapeless", "crafting_special_armordye", "crafting_special_bookcloning",
    "crafting_special_mapcloning", "crafting_special_mapextending", "crafting_special_firework_rocket",
    "crafting_special_firework_star", "crafting_special_firework_star_fade", "crafting_special_tippedarrow",
    "crafting_special_bannerduplicate", "crafting_special_shielddecoration",
    "crafting_special_shulkerboxcoloring", "crafting_special_suspiciousstew", "crafting_special_repairitem",
    "smelting", "blasting", "smoking", "campfire_cooking", "stonecutting", "smithing_transform",
    "smithing_trim", "crafting_decorated_pot"])

LOCAL_CORRECTIONS = {
    # minecraft-data 1.21.1 still lists the banner pattern recipe serializer
    # that 1.20.5 removed (the 1.21.1 data generator's recipe_serializer
    # registry has 23 entries); protodef corrects 766 only.
    767: [(protodef.RECIPE_TYPES, SERIALIZERS_1_21)],
}


def schema(protocol):
    document = protodef.schema_for(protocol)
    for path, value in LOCAL_CORRECTIONS.get(protocol, []):
        document = json.loads(json.dumps(document))
        protodef.patch(document, path, value)
    return document


def replay(backend, client):
    command = ["cargo", "run", "-q", "-p", "rift-translation", "--example", "replay", "--",
               "--backend", str(backend), "--client", str(client)]
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
    return [json.loads(line) for line in result.stdout.splitlines() if line.strip()]


def check(backend, client, own):
    schemas = {client: schema(client), backend: schema(backend)}
    decoders = {}
    results = collections.Counter()
    failures = []
    for line in replay(backend, client):
        if "error" in line:
            results[(line["from"]["name"], "translator error")] += 1
            continue
        protocol = client if line["to"] == "client" else backend
        direction = "clientbound" if line["d"] == "c" else "serverbound"
        key = (protocol, line["s"], direction)
        if key not in decoders:
            decoders[key] = protodef.Decoder(schemas[protocol], line["s"], direction)
        try:
            decoders[key].packet(line["id"], bytes.fromhex(line["hex"]))
            results[(line["name"], "ok")] += 1
        except protodef.DecodeError as error:
            results[(line["name"], "schema error")] += 1
            if line["name"] in own:
                failures.append(f"{line['name']}: {error}")
    print(f"backend {backend} -> client {client}")
    for (name, status), count in sorted(results.items()):
        mark = "*" if name in own else " "
        print(f"  {mark} {name:36} {status:16} {count}")
    for failure in failures[:20]:
        print("  FAIL", failure)
    return not failures


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--backend", type=int, required=True)
    parser.add_argument("--client", type=int, required=True)
    parser.add_argument("--own", default="", help="comma-separated packet names the step converts")
    args = parser.parse_args()
    own = {name for name in args.own.split(",") if name}
    sys.exit(0 if check(args.backend, args.client, own) else 1)


if __name__ == "__main__":
    main()
