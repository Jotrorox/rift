"""Decode every packet the 1.12.2 -> 1.13 up step produces with the
minecraft-data schema of the version that receives it (tests/protodef.py,
independent of Rift's own readers).

    /opt/homebrew/bin/python3 crates/rift-translation/src/steps/v1_12_2_to_1_13/up/schema_check.py

Sources: the offline replay of the recorded 1.12.2 session for a 1.13 client
(`examples/replay.rs`), and every packet the step's unit tests see it
produce (`RIFT_S02_DUMP`). Only packets the step itself writes are checked;
codec-owned packets pass through unconverted until the codecs land.
Prints counts per packet and every failure; exits 1 on a failure.
"""
import collections
import json
import os
import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[6]
sys.path.insert(0, str(ROOT / "tests"))
import protodef  # noqa: E402

OLD, NEW = 340, 393
# The packets the step writes, by the side that receives them.
OWN = {
    "client": {
        "login", "update_recipes", "update_tags", "commands", "custom_payload",
        "stop_sound", "command_suggestions", "recipe", "place_ghost_recipe",
        "update_mob_effect", "block_update", "chunk_blocks_update", "level_chunk",
    },
    "backend": {"custom_payload", "command_suggestion", "place_recipe", "recipe_book_update"},
}


def lines():
    replay = subprocess.run(
        ["cargo", "run", "-q", "-p", "rift-translation", "--example", "replay", "--",
         "--backend", str(OLD), "--client", str(NEW)],
        cwd=ROOT, capture_output=True, text=True,
    )
    for line in replay.stdout.splitlines():
        yield "replay", json.loads(line)
    with tempfile.TemporaryDirectory() as tmp:
        path = pathlib.Path(tmp) / "dump.jsonl"
        subprocess.run(
            ["cargo", "test", "-q", "-p", "rift-translation", "v1_12_2_to_1_13::up"],
            cwd=ROOT, env={**os.environ, "RIFT_S02_DUMP": str(path)},
            check=True, capture_output=True,
        )
        for line in path.read_text().splitlines():
            yield "tests", json.loads(line)


def main():
    schemas = {"client": protodef.schema_for(NEW), "backend": protodef.schema_for(OLD)}
    decoders = {}
    counts = collections.Counter()
    failures = []
    for source, packet in lines():
        to, state, name = packet["to"], packet["s"], packet["name"]
        if "error" in packet or name not in OWN[to]:
            continue
        key = (to, state)
        if key not in decoders:
            direction = "clientbound" if to == "client" else "serverbound"
            decoders[key] = protodef.Decoder(schemas[to], state, direction)
        decoder = decoders[key]
        body = bytes.fromhex(packet["hex"])
        try:
            decoder.packet(decoder.ids[schema_name(decoder, to, name)], body)
            counts[(source, to, name)] += 1
        except (protodef.DecodeError, KeyError) as error:
            failures.append(f"{source} {to} {name}: {error} ({packet['hex'][:80]})")
    for (source, to, name), count in sorted(counts.items()):
        print(f"{source:7} {to:8} {name:24} {count:5} decoded")
    for failure in failures:
        print("FAILED", failure)
    print(f"{sum(counts.values())} packets decoded, {len(failures)} failed")
    sys.exit(1 if failures else 0)


# Rift's names (Mojang's) to minecraft-data's for the packets above.
SCHEMA_NAMES = {
    "login": "login", "update_recipes": "declare_recipes", "update_tags": "tags",
    "commands": "declare_commands", "custom_payload": "custom_payload",
    "stop_sound": "stop_sound", "command_suggestions": "tab_complete",
    "recipe": "unlock_recipes", "place_ghost_recipe": "craft_recipe_response",
    "update_mob_effect": "entity_effect", "block_update": "block_change",
    "chunk_blocks_update": "multi_block_change", "level_chunk": "map_chunk",
    "command_suggestion": "tab_complete", "place_recipe": "craft_recipe_request",
    "recipe_book_update": "crafting_book_data",
}


def schema_name(decoder, to, name):
    return SCHEMA_NAMES.get(name, name)


if __name__ == "__main__":
    main()
