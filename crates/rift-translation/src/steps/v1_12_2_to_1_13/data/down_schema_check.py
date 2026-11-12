"""Decode what the 1.12.2-on-1.13 step sends with minecraft-data's schemas.

    RIFT_SCHEMA_DUMP=/tmp/down.jsonl cargo test -p rift-translation v1_12_2_to_1_13::down
    python3 down_schema_check.py /tmp/down.jsonl

The step's tests append every packet the step emits to `$RIFT_SCHEMA_DUMP`;
this decodes each with `tests/protodef.py`, an independent decoder driven by
the receiving protocol's minecraft-data schema, and fails on any packet the
schema rejects or that leaves bytes over.
"""
import collections, json, sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "tests"))
import protodef  # noqa: E402

counts, failures = collections.Counter(), []
decoders = {}
for path in sys.argv[1:]:
    for line in open(path):
        packet = json.loads(line)
        key = (packet["protocol"], packet["state"], packet["direction"])
        if key not in decoders:
            decoders[key] = protodef.Decoder(protodef.schema_for(key[0]), key[1], key[2])
        try:
            decoders[key].packet(packet["id"], bytes.fromhex(packet["hex"]))
            counts[(packet["direction"], packet["name"])] += 1
        except protodef.DecodeError as error:
            failures.append(f"{packet['direction']} {packet['name']}: {error}")
for (direction, name), count in sorted(counts.items()):
    print(f"{count:6} {direction:12} {name}")
for failure in failures:
    print("FAILED", failure)
print(f"{sum(counts.values())} packets decoded, {len(failures)} failed")
sys.exit(1 if failures else 0)
