#!/usr/bin/env python3
"""Decode everything the 1.20.1 <-> 1.20.2 steps produce from the recorded
sessions with the receiving version's minecraft-data schema.

    /opt/homebrew/bin/python3 crates/rift-translation/src/steps/v1_20_to_1_20_2/check_schema.py

Runs `examples/replay.rs` in both orientations and decodes every packet sent
to the client (in the client's protocol) and to the backend (in the
backend's protocol) with tests/protodef.py, an implementation independent of
Rift's. Packets the steps produce themselves must all decode; packets the
shared codecs own are listed separately, since those codecs may not have
landed yet. Exits 1 when a step-owned packet fails.
"""

import collections
import json
from pathlib import Path
import struct
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT / "tests"))
import protodef  # noqa: E402

# Packets whose output these steps produce or re-phase (receiver's names).
OWNED = {
    "hello", "login_acknowledged", "login_finished", "registry_data", "update_enabled_features",
    "finish_configuration", "start_configuration", "configuration_acknowledged", "login",
    "respawn", "custom_payload", "keep_alive", "ping", "pong", "pong_response", "disconnect",
    "client_information", "resource_pack", "resource_pack_push", "update_tags",
    "chunk_batch_start", "chunk_batch_finished", "chunk_batch_received", "add_entity",
    "add_player", "rotate_head", "set_entity_motion", "game_event", "set_display_objective",
    "tag_query", "set_beacon",
}


def replay(backend, client):
    result = subprocess.run(
        ["cargo", "run", "-q", "-p", "rift-translation", "--example", "replay", "--",
         "--backend", str(backend), "--client", str(client)],
        cwd=ROOT, capture_output=True, text=True)
    lines = [json.loads(line) for line in result.stdout.splitlines() if line]
    errors = [line for line in lines if "error" in line]
    return [line for line in lines if "error" not in line], errors


def check(backend, client):
    lines, errors = replay(backend, client)
    decoders = {}
    counts = collections.Counter()
    failures = []
    for line in lines:
        protocol = client if line["to"] == "client" else backend
        direction = "clientbound" if line["d"] == "c" else "serverbound"
        key = (protocol, line["s"], direction)
        if key not in decoders:
            decoders[key] = protodef.Decoder(protodef.schema_for(protocol), line["s"], direction)
        owned = line["name"] in OWNED
        try:
            decoders[key].packet(line["id"], bytes.fromhex(line["hex"]))
            counts[(owned, "ok")] += 1
        except (protodef.DecodeError, ValueError, KeyError, IndexError, struct.error) as error:
            counts[(owned, "failed")] += 1
            failures.append((owned, line, str(error)))
    print(f"backend {backend} -> client {client}: {len(lines)} packets produced, "
          f"{len(errors)} rejected by the translator")
    for owned in (True, False):
        label = "step-owned" if owned else "codec-owned"
        print(f"  {label}: {counts[(owned, 'ok')]} decode, {counts[(owned, 'failed')]} fail")
    by_name = collections.Counter((owned, line["s"], line["name"], error.split(" (in")[0])
                                  for owned, line, error in failures)
    for (owned, state, name, error), count in sorted(by_name.items()):
        print(f"    {'OWNED ' if owned else ''}{state} {name} x{count}: {error}")
    return sum(1 for owned, _, _ in failures if owned) + len(errors)


def main():
    bad = check(763, 764) + check(764, 763)
    raise SystemExit(1 if bad else 0)


if __name__ == "__main__":
    main()
