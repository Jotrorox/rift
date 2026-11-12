#!/usr/bin/env python3
"""Check what the chat-signing era's translation steps (1.18.2 <-> 1.19 <->
1.19.1 <-> 1.19.3) produce against the independent minecraft-data schemas.

Replays the recorded session of every protocol in the range through the
translator to every other one (`examples/replay.rs`), decodes each packet
sent to the client or backend with the receiving protocol's schema
(tests/protodef.py) and reports, per packet name, how many decoded and why
the others did not. Failures of packets the steps own fail the run; packets
owned by the shared codecs pass through unconverted until those land and are
listed separately.

    /opt/homebrew/bin/python3 tests/translation_signing_era.py [--pairs]

`--pairs` limits the replays to adjacent protocols. The step unit tests,
whose hand-made packets cover what the recorded sessions lack (serverbound
chat and commands, digging, respawns, ...), are run too, with every packet a
step produces in them dumped and decoded the same way.
"""

import collections
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tests"))
import protodef  # noqa: E402

PROTOCOLS = [758, 759, 760, 761]

# Packets (by the sender's name) a step of the era handles itself; every
# packet it produces from them must decode.
OWNED = {
    "hello", "key", "login_finished", "custom_query",
    "login", "respawn", "add_entity", "add_mob", "add_painting", "add_vibration_signal",
    "block_break_ack", "block_changed_ack", "chat", "player_chat", "system_chat",
    "disguised_chat", "server_data", "custom_sound", "update_tags", "container_set_data",
    "player_info", "player_info_update", "player_info_remove", "set_health",
    "chat_command", "player_action", "use_item_on", "use_item", "set_beacon",
    "container_button_click",
}

# Where the pinned schemas are wrong for packets the steps produce, checked
# against the official servers' recordings and their stream codecs.
SCHEMA_GAPS = {
    # 1.19's vibration particle is a position source (block position, or
    # entity ID and Y offset) and arrival ticks; the schema reads extra
    # VarInts.
    (759, "level_particles"): "vibration particle options",
    # The 1.19 schema lists entity data serializers up to 18 (pose); the cat,
    # frog and painting variants (19, 20, 22) and global positions (21) are
    # missing. The 1.19 server's own painting data uses 22 at index 8.
    (759, "set_entity_data"): "entity data serializers 19 to 22",
    # 1.19.3's respawn ends with a byte of data to keep (1 attributes, 2
    # entity data: ClientboundRespawnPacket.dataToKeep in Mojang's 1.19.3
    # mappings); the schema still reads 1.19.2's boolean.
    (761, "respawn"): "keep-data mask",
}


def replay(backend, client):
    result = subprocess.run(
        ["cargo", "run", "-q", "-p", "rift-translation", "--example", "replay", "--",
         "--backend", str(backend), "--client", str(client)],
        cwd=ROOT, capture_output=True, text=True, check=False)
    return [json.loads(line) for line in result.stdout.splitlines() if line]


def check(backend, client, totals, failures):
    decoders = {}
    for line in replay(backend, client):
        if "error" in line:
            failures.append((backend, client, line["from"]["name"], "translator: " + line["error"]))
            continue
        protocol = client if line["to"] == "client" else backend
        direction = "clientbound" if line["d"] == "c" else "serverbound"
        key = (protocol, line["s"], direction)
        if key not in decoders:
            decoders[key] = protodef.Decoder(protodef.schema_for(protocol), line["s"], direction)
        source = line["from"]["name"]
        owned = source in OWNED
        try:
            decoders[key].packet(line["id"], bytes.fromhex(line["hex"]))
            totals[(owned, line["name"])][0] += 1
        except protodef.DecodeError as error:
            totals[(owned, line["name"])][1] += 1
            gap = SCHEMA_GAPS.get((protocol, line["name"]))
            if owned and not gap:
                failures.append((backend, client, f"{source} -> {line['name']}", str(error)))


def unit_tests(totals, failures):
    import os
    import tempfile
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "dump.jsonl"
        env = dict(os.environ, RIFT_SIGNING_ERA_DUMP=str(path))
        for module in ["v1_18_2_to_1_19", "v1_19_to_1_19_1", "v1_19_1_to_1_19_3"]:
            subprocess.run(["cargo", "test", "-q", "-p", "rift-translation", "--lib", f"steps::{module}"],
                           cwd=ROOT, env=env, capture_output=True, check=True)
        lines = [json.loads(line) for line in path.read_text().splitlines()]
    decoders = {}
    for line in lines:
        direction = "clientbound" if line["d"] == "c" else "serverbound"
        key = (line["protocol"], line["s"], direction)
        if key not in decoders:
            decoders[key] = protodef.Decoder(protodef.schema_for(line["protocol"]), line["s"], direction)
        decoder = decoders[key]
        name = line["name"]
        owned = line["from"] in OWNED
        try:
            decoder.packet(line["id"], bytes.fromhex(line["hex"]))
            totals[("unit" if owned else False, name)][0] += 1
        except protodef.DecodeError as error:
            totals[("unit" if owned else False, name)][1] += 1
            if owned and not SCHEMA_GAPS.get((line["protocol"], name)):
                failures.append((line["protocol"], "unit test", f"{line['from']} -> {name}", str(error)))


def main():
    pairs = [(a, b) for a in PROTOCOLS for b in PROTOCOLS if a != b]
    if "--pairs" in sys.argv:
        pairs = [(a, b) for a, b in pairs if abs(PROTOCOLS.index(a) - PROTOCOLS.index(b)) == 1]
    totals = collections.defaultdict(lambda: [0, 0])
    failures = []
    for backend, client in pairs:
        check(backend, client, totals, failures)
    unit_tests(totals, failures)
    for owned in (True, False, "unit"):
        print({True: "Replays, owned by the steps:",
               False: "\nReplays, owned by shared codecs (unconverted until they land):",
               "unit": "\nProduced in the steps' unit tests:"}[owned])
        for (is_owned, name), (good, bad) in sorted(totals.items(), key=lambda item: (str(item[0][0]), item[0][1])):
            if is_owned == owned and (owned is not False or bad):
                print(f"  {name:32} {good:6} decoded {bad:4} failed")
    decoded = sum(good for (owned, _), (good, _) in totals.items() if owned is True)
    unit = sum(good for (owned, _), (good, _) in totals.items() if owned == "unit")
    print(f"\n{len(pairs)} replays: {decoded} packets from owned handlers decoded; "
          f"unit tests: {unit} produced packets decoded; {len(failures)} failures")
    for backend, client, name, error in failures[:40]:
        print(f"  {backend} / {client}: {name}: {error}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
