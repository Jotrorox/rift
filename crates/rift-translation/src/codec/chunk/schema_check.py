"""Decode the chunk codec's converted corpus packets with minecraft-data's
protocol schemas (tests/protodef.py), independently of Rift's own readers.

    RIFT_CHUNK_DUMP=/tmp/dump cargo test --release -p rift-translation \
        dump_converted -- --ignored
    python3 crates/rift-translation/src/codec/chunk/schema_check.py /tmp/dump

Schemas come from a minecraft-data checkout ($MINECRAFT_DATA or
~/.cache/rift-via/ref/minecraft-data). Versions minecraft-data does not
describe are skipped. Exits non-zero when a packet fails to decode.
"""
import json
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT / "tests"))
import protodef  # noqa: E402

DATA = Path(os.environ.get("MINECRAFT_DATA", Path.home() / ".cache/rift-via/ref/minecraft-data")) / "data"
RELEASES = {
    335: "1.12", 338: "1.12.1", 340: "1.12.2", 393: "1.13", 401: "1.13.1", 404: "1.13.2",
    477: "1.14", 480: "1.14.1", 485: "1.14.2", 490: "1.14.3", 498: "1.14.4", 573: "1.15",
    575: "1.15.1", 578: "1.15.2", 735: "1.16", 736: "1.16.1", 751: "1.16.2", 753: "1.16.3",
    754: "1.16.5", 755: "1.17", 756: "1.17.1", 757: "1.18.1", 758: "1.18.2", 759: "1.19",
    760: "1.19.2", 761: "1.19.3", 762: "1.19.4", 763: "1.20.1", 764: "1.20.2", 765: "1.20.4",
    766: "1.20.6", 767: "1.21.1", 768: "1.21.3", 769: "1.21.4", 770: "1.21.5", 771: "1.21.6",
    772: "1.21.8", 773: "1.21.10", 774: "1.21.11", 775: "26.1", 776: "26.2", 777: "26.3",
}
PACKETS = {
    "level_chunk": "map_chunk", "level_chunk_with_light": "map_chunk", "light_update": "update_light",
    "forget_level_chunk": "unload_chunk", "block_update": "block_change",
    "section_blocks_update": "multi_block_change", "chunk_blocks_update": "multi_block_change",
    "block_entity_data": "tile_entity_data", "block_event": "block_action",
    "level_event": "world_event", "explode": "explosion", "chunks_biomes": "chunk_biomes",
}


class Decoder(protodef.Decoder):
    """Adds the named NBT roots of protocols before 1.20.2."""

    def named(self, reader, name, scopes):
        if name in ("nbt", "optionalNbt"):
            tag = reader.take(1)[0]
            if tag == 0:
                if name == "nbt":
                    raise protodef.DecodeError("missing NBT tag")
                return None
            size = int.from_bytes(reader.take(2), "big")
            reader.take(size)
            return protodef.nbt_payload(reader, tag)
        return super().named(reader, name, scopes)


def decoder(protocol, cache={}):
    if protocol not in cache:
        paths = json.loads((DATA / "dataPaths.json").read_text())["pc"]
        entry = paths.get(RELEASES[protocol])
        if entry is None or "protocol" not in entry:
            cache[protocol] = None
        else:
            schema = json.loads((DATA / entry["protocol"] / "protocol.json").read_text())
            cache[protocol] = Decoder(schema, "play", "clientbound")
    return cache[protocol]


def main(directory):
    failures = checked = skipped = 0
    for path in sorted(Path(directory).glob("*.jsonl")):
        source, target = (int(part) for part in path.stem.split("-"))
        decode = decoder(target)
        if decode is None:
            skipped += 1
            continue
        for line in path.read_text().splitlines():
            packet = json.loads(line)
            if packet["name"] not in PACKETS:
                continue
            try:
                name, _ = decode.packet(packet["id"], bytes.fromhex(packet["hex"]))
            except (protodef.DecodeError, KeyError, IndexError, ValueError) as error:
                failures += 1
                print(f"{path.stem} {packet['name']}: {error}")
                continue
            if name != PACKETS[packet["name"]]:
                failures += 1
                print(f"{path.stem} {packet['name']} decoded as {name}")
            checked += 1
    print(f"{checked} converted packets decode with minecraft-data; {failures} failures; "
          f"{skipped} pairs without a schema")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
