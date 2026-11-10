#!/usr/bin/env python3
"""Check what the 1.14.4 to 1.16.5 steps produce against minecraft-data.

For each orientation of each pair (and the chains across all of them) this
replays the recorded sessions through the translator with
`examples/replay.rs`, adding to the client's recorded session serverbound
packets that exercise the steps' serverbound handlers (encoded from the
CLIENT's minecraft-data schema with tests/protodef.py, independently of
Rift). Every packet the translator produces is then decoded with the
RECEIVING version's schema. Failures are grouped by packet name and split
into packets these steps own and packets the shared codecs own (which pass
through unconverted until those codecs land).

    /opt/homebrew/bin/python3 crates/rift-translation/src/steps/v1_14_4_to_1_15/schema_check.py

Needs the corpus (~/.cache/rift-via/corpus or $RIFT_TRANSLATION_CORPUS) and
a release build of the replay example (built on first use).
"""
import collections
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT / "tests"))
import protodef  # noqa: E402

CORPUS = pathlib.Path(os.environ.get("RIFT_TRANSLATION_CORPUS")
                      or pathlib.Path.home() / ".cache/rift-via/corpus")
PROTOCOLS = [498, 573, 575, 578, 735, 736, 751, 753, 754]
ORIENTATIONS = [(a, b) for a, b in zip(PROTOCOLS, PROTOCOLS[1:])]
ORIENTATIONS += [(498, 578), (578, 754), (498, 754)]

# What these steps emit, by the receiver's packet name (ours, not
# minecraft-data's).
OWNED = {
    "login_finished", "login", "respawn", "game_event", "set_health", "explode",
    "sound", "client_command", "add_entity", "add_global_entity", "set_equipment",
    "set_equipped_item", "container_set_data", "map_item_data", "interact",
    "player_abilities", "set_jigsaw_block", "chunk_blocks_update",
    "section_blocks_update", "recipe_book_update", "recipe_book_change_settings",
    "recipe_book_seen_recipe", "edit_book", "player_command", "set_carried_item",
}

POSITION = {"x": 12, "y": 64, "z": -7}


def serverbound(protocol):
    """Serverbound play packets for `protocol` that the steps convert, as
    minecraft-data names and values."""
    packets = [
        ("entity_action", {"entityId": 1, "actionId": 0, "jumpBoost": 0}),
        ("use_entity", {"target": 5, "mouse": 0, "hand": 1, "sneaking": True}),
        ("use_entity", {"target": 5, "mouse": 1, "sneaking": True}),
        ("use_entity", {"target": 5, "mouse": 2, "x": 0.5, "y": 1.0, "z": 0.25,
                        "hand": 0, "sneaking": True}),
        ("entity_action", {"entityId": 1, "actionId": 1, "jumpBoost": 0}),
        ("abilities", {"flags": 2, "flyingSpeed": 0.05, "walkingSpeed": 0.1}),
        ("update_jigsaw_block", {
            "location": POSITION, "attachmentType": "minecraft:bottom",
            "name": "minecraft:start", "target": "minecraft:bottom",
            "targetPool": "minecraft:village/houses", "pool": "minecraft:village/houses",
            "finalState": "minecraft:air", "jointType": "aligned"}),
        ("held_item_slot", {"slotId": 4}),
    ]
    if protocol >= 735:
        packets.append(("generate_structure", {"location": POSITION, "levels": 3,
                                               "keepJigsaws": False}))
    if protocol >= 751:
        packets += [
            ("recipe_book", {"bookId": 2, "bookOpen": True, "filterActive": False}),
            ("displayed_recipe", {"recipeId": "minecraft:stick"}),
        ]
    else:
        packets += [
            ("crafting_book_data", {"type": 1, "craftingBookOpen": True,
                                    "smeltingFilter": True, "smokingBookOpen": True}),
            ("crafting_book_data", {"type": 0, "displayedRecipe": "minecraft:stick"}),
        ]
    for hand in ((0, 40) if protocol >= 754 else (0, 1)):
        packets.append(("edit_book", {"signing": False, "hand": hand}))
    encoder = protodef.Encoder(protodef.schema_for(protocol), "play", "serverbound")
    lines = []
    for name, value in packets:
        packet_id, body = encoder.packet(name, value)
        lines.append({"d": "s", "s": "play", "id": packet_id, "hex": body.hex()})
    return lines


def replay(binary, corpus, backend, client):
    result = subprocess.run(
        [str(binary), "--backend", str(backend), "--client", str(client)],
        capture_output=True, text=True, env={**os.environ, "RIFT_TRANSLATION_CORPUS": str(corpus)})
    return [json.loads(line) for line in result.stdout.splitlines() if line]


def main():
    subprocess.run(["cargo", "build", "-q", "--release", "-p", "rift-translation",
                    "--example", "replay"], cwd=ROOT, check=True)
    binary = ROOT / "target/release/examples/replay"
    total_failures = 0
    with tempfile.TemporaryDirectory() as directory:
        corpus = pathlib.Path(directory)
        for protocol in PROTOCOLS:
            source = CORPUS / f"{protocol}.jsonl"
            if not source.exists():
                sys.exit(f"no corpus for {protocol}")
            shutil.copy(source, corpus / source.name)
            with open(corpus / source.name, "a") as out:
                for line in serverbound(protocol):
                    out.write(json.dumps(line) + "\n")
        for older, newer in ORIENTATIONS:
            for backend, client in ((older, newer), (newer, older)):
                decoded = collections.Counter()
                failed = collections.defaultdict(list)
                translation_errors = collections.Counter()
                decoders = {}
                for line in replay(binary, corpus, backend, client):
                    if "error" in line:
                        translation_errors[f"{line['from']['name']}: {line['error']}"] += 1
                        continue
                    protocol = client if line["to"] == "client" else backend
                    direction = "clientbound" if line["d"] == "c" else "serverbound"
                    key = (protocol, line["s"], direction)
                    if key not in decoders:
                        decoders[key] = protodef.Decoder(protodef.schema_for(protocol), line["s"], direction)
                    try:
                        decoders[key].packet(line["id"], bytes.fromhex(line["hex"]))
                        decoded[line["name"]] += 1
                    except protodef.DecodeError as error:
                        failed[line["name"]].append(str(error))
                owned = {name: errors for name, errors in failed.items() if name in OWNED}
                checked_owned = sum(count for name, count in decoded.items() if name in OWNED)
                print(f"backend {backend} client {client}: {sum(decoded.values())} decoded "
                      f"({checked_owned} of the steps' own packets), "
                      f"{sum(map(len, failed.values()))} failed")
                for name, errors in sorted(failed.items()):
                    tag = "OWNED" if name in OWNED else "codec"
                    print(f"  {tag} {name} x{len(errors)}: {errors[0][:160]}")
                for error, count in sorted(translation_errors.items()):
                    print(f"  not translated x{count}: {error}")
                total_failures += sum(map(len, owned.values()))
    print(f"{total_failures} of the steps' own packets failed to decode")
    return 1 if total_failures else 0


if __name__ == "__main__":
    sys.exit(main())
