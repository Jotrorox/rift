#!/usr/bin/env python3
"""Decode what the 1.20.2 <-> 1.20.5 steps produce with minecraft-data.

    /opt/homebrew/bin/python3 crates/rift-translation/src/steps/v1_20_3_to_1_20_5/schema_check.py

Replays the recorded sessions of every orientation of the pairs 764/765 and
765/766, and the chains across both, through the translator
(`examples/replay.rs`), plus the packets the steps' handlers produce from
hand-made inputs (the ignored test `dump_produced_packets`, written to a
temporary file), and decodes each produced packet with the receiving
version's schema (`tests/protodef.py`, an independent decoder). Prints the
failures per packet name and exits 1 if a packet the steps own fails.
Packets owned by shared codecs pass through unconverted until the codecs
exist; their failures are listed but do not count.
"""

import collections
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT / "tests"))
import protodef  # noqa: E402

ORIENTATIONS = [(764, 765), (765, 764), (765, 766), (766, 765), (764, 766), (766, 764)]

# What each pair's steps produce themselves and nothing else does, by
# receiving name. `update_attributes` and `disconnect` are also produced by
# the steps, but other packets of those names pass through from the backend:
# they are checked through the handler dump only.
OWNED = {
    (764, 765): {"resource_pack_push", "resource_pack_pop", "resource_pack", "set_score",
                 "reset_score", "game_event", "set_default_spawn_position",
                 "set_jigsaw_block"},
    (765, 766): {"hello", "login_finished", "cookie_response", "select_known_packs", "login",
                 "respawn", "server_data", "mount_screen_open", "chat", "chat_ack",
                 "chat_command", "chat_command_signed", "container_button_click"},
}


def replay(backend, client):
    result = subprocess.run(
        ["cargo", "run", "-q", "-p", "rift-translation", "--example", "replay", "--",
         "--backend", str(backend), "--client", str(client)],
        cwd=ROOT, capture_output=True, text=True)
    for line in result.stdout.splitlines():
        packet = json.loads(line)
        if "error" in packet:
            yield None, packet
            continue
        protocol = client if packet["to"] == "client" else backend
        yield protocol, packet


def produced():
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "produced.jsonl"
        subprocess.run(
            ["cargo", "test", "-q", "-p", "rift-translation", "dump_produced_packets", "--",
             "--ignored"],
            cwd=ROOT, env={**os.environ, "RIFT_S10_PRODUCED": str(path)}, check=True,
            capture_output=True)
        for line in path.read_text().splitlines():
            packet = json.loads(line)
            yield packet["protocol"], packet


def main():
    decoders = {}
    checked = collections.Counter()
    failures = collections.defaultdict(list)
    sources = [((backend, client), replay(backend, client)) for backend, client in ORIENTATIONS]
    sources.append(("handlers", produced()))
    for source, packets in sources:
        for protocol, packet in packets:
            if protocol is None:
                failures[("translator", packet["from"]["name"])].append((source, packet["error"]))
                continue
            direction = "clientbound" if packet["d"] == "c" else "serverbound"
            key = (protocol, packet["s"], direction)
            if key not in decoders:
                decoders[key] = protodef.Decoder(protodef.schema_for(protocol), packet["s"],
                                                 direction)
            name = packet.get("name", "?")
            checked[(protocol, name)] += 1
            try:
                decoders[key].packet(packet["id"], bytes.fromhex(packet["hex"]))
            except protodef.DecodeError as error:
                failures[(protocol, packet["s"], name)].append((source, str(error)))
    owned_failures = 0
    for key, errors in sorted(failures.items(), key=str):
        owned = [error for error in errors
                 if error[0] == "handlers"
                 or (error[0] != "handlers" and len(key) == 3
                     and key[2] in OWNED.get(tuple(sorted(error[0])), ()))]
        owned_failures += len(owned)
        label = "OWNED" if owned else "codec"
        source, error = (owned or errors)[0]
        print(f"{label} {key}: {len(errors)} failed, e.g. {source}: {error}")
    total = sum(checked.values())
    print(f"{total} packets decoded, {sum(map(len, failures.values()))} failed, "
          f"{owned_failures} of them produced by the steps")
    return 1 if owned_failures else 0


if __name__ == "__main__":
    sys.exit(main())
