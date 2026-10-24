#!/usr/bin/env python3
"""Decode Minecraft packets with PrismarineJS minecraft-data protocol schemas.

This is an independent oracle for protocol translation tests: it shares no
code with Rift's hand-written layouts. Schemas are pinned by commit and
SHA-256 in tests/minecraft_data.json. Where a pinned schema disagrees with the
official server's stream codecs, CORRECTIONS replaces that type and records
why; every correction was checked against the decompiled 1.21.6 server.
"""

import copy
import hashlib
import json
from pathlib import Path
import struct
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PINS = json.loads((ROOT / "tests" / "minecraft_data.json").read_text())
CACHE = ROOT / "target" / "minecraft" / "minecraft-data"


class DecodeError(ValueError):
    pass


def schema(version):
    """The pinned protocol.json for `version`, downloaded once and verified."""
    pin = PINS["protocols"][version]
    path = CACHE / f"{PINS['commit']}-{version}-protocol.json"
    if not path.exists():
        CACHE.mkdir(parents=True, exist_ok=True)
        url = (f"https://raw.githubusercontent.com/PrismarineJS/minecraft-data/"
               f"{PINS['commit']}/data/pc/{pin['path']}/protocol.json")
        with urllib.request.urlopen(url, timeout=60) as response:
            data = response.read()
        if hashlib.sha256(data).hexdigest() != pin["sha256"]:
            raise ValueError(f"minecraft-data checksum mismatch for {version}")
        with tempfile.NamedTemporaryFile(dir=CACHE, delete=False) as pending:
            pending.write(data)
        Path(pending.name).replace(path)
    data = path.read_bytes()
    if hashlib.sha256(data).hexdigest() != pin["sha256"]:
        raise ValueError(f"cached minecraft-data checksum mismatch for {version}")
    return corrected(json.loads(data))


def _container(*fields):
    return ["container", [{"name": name, "type": kind} for name, kind in fields]]


# Each replaces a type the pinned schema gets wrong for 1.21.6-1.21.8.
CORRECTIONS = {
    # GlobalPos.STREAM_CODEC is a dimension key followed by a block position.
    ("entityMetadataEntry", "optional_global_pos"):
        ["option", _container(("dimension", "string"), ("position", "position"))],
    # PaintingVariant.DIRECT_STREAM_CODEC uses VarInt width and height.
    "EntityMetadataPaintingVariant": _container(
        ("width", "varint"), ("height", "varint"), ("assetId", "string"),
        ("title", ["option", "anonymousNbt"]), ("author", ["option", "anonymousNbt"])),
    # BlockPredicate.STREAM_CODEC: an optional NbtPredicate (boolean-prefixed
    # compound), and partial component predicates are (type, NBT) pairs.
    "ItemBlockPredicate": _container(
        ("blockSet", ["option", ["registryEntryHolderSet", {
            "base": {"name": "name", "type": "string"},
            "otherwise": {"name": "ids", "type": "varint"}}]]),
        ("properties", ["option", ["array", {"countType": "varint", "type": "ItemBlockProperty"}]]),
        ("nbt", ["option", "anonymousNbt"]),
        ("components", "DataComponentMatchers")),
    "DataComponentMatchers": _container(
        ("exactMatchers", "ExactComponentMatcher"),
        ("partialMatchers", ["array", {"countType": "varint", "type": _container(
            ("type", "varint"), ("predicate", "anonymousNbt"))}])),
    # StatePropertiesPredicate.ValueMatcher is ByteBufCodecs.either(exact,
    # range), and a range's bounds are each optional.
    "ItemBlockProperty": _container(
        ("name", "string"), ("isExactMatch", "bool"),
        ("value", ["switch", {"compareTo": "isExactMatch", "fields": {
            "true": _container(("exactValue", "string")),
            "false": _container(("minValue", ["option", "string"]),
                                ("maxValue", ["option", "string"]))}}])),
}


def corrected(protocol):
    protocol = copy.deepcopy(protocol)
    types = protocol["types"]
    for key, replacement in CORRECTIONS.items():
        if isinstance(key, tuple):
            name, case = key
            fields = types[name][1][2]["type"][1]["fields"]
            if case not in fields:
                raise KeyError(f"correction target {name}.{case} is missing")
            fields[case] = replacement
        else:
            if key not in types:
                raise KeyError(f"correction target {key} is missing")
            types[key] = replacement
    return protocol


class Reader:
    def __init__(self, data):
        self.data = memoryview(data)
        self.offset = 0

    def take(self, size):
        if size < 0 or self.offset + size > len(self.data):
            raise DecodeError("short packet")
        value = bytes(self.data[self.offset:self.offset + size])
        self.offset += size
        return value

    def peek(self):
        if self.offset >= len(self.data):
            raise DecodeError("short packet")
        return self.data[self.offset]

    def varint(self, width=5):
        result = 0
        for shift in range(0, 7 * width, 7):
            byte = self.take(1)[0]
            result |= (byte & 127) << shift
            if not byte & 128:
                bits = 7 * width if width > 5 else 32
                result &= (1 << bits) - 1
                if result >= 1 << (bits - 1):
                    result -= 1 << bits
                return result
        raise DecodeError("invalid VarInt")

    def remaining(self):
        return len(self.data) - self.offset


NUMBERS = {"u8": ">B", "u16": ">H", "u32": ">I", "u64": ">Q", "i8": ">b", "i16": ">h",
           "i32": ">i", "i64": ">q", "f32": ">f", "f64": ">d"}


def nbt_payload(reader, tag, depth=0):
    if depth > 512:
        raise DecodeError("NBT too deep")
    if tag == 0:
        return None
    if tag in (1, 2, 3, 4, 5, 6):
        return struct.unpack(">" + "bhiqfd"[tag - 1], reader.take([1, 2, 4, 8, 4, 8][tag - 1]))[0]
    if tag in (7, 11, 12):
        count = struct.unpack(">i", reader.take(4))[0]
        width, code = {7: (1, "b"), 11: (4, "i"), 12: (8, "q")}[tag]
        return list(struct.unpack(f">{count}{code}", reader.take(count * width)))
    if tag == 8:
        return reader.take(struct.unpack(">H", reader.take(2))[0]).decode("utf-8", "surrogatepass")
    if tag == 9:
        element = reader.take(1)[0]
        count = struct.unpack(">i", reader.take(4))[0]
        return [nbt_payload(reader, element, depth + 1) for _ in range(count)]
    if tag == 10:
        value = {}
        while (element := reader.take(1)[0]) != 0:
            name = reader.take(struct.unpack(">H", reader.take(2))[0]).decode("utf-8", "surrogatepass")
            value[name] = nbt_payload(reader, element, depth + 1)
        return value
    raise DecodeError(f"invalid NBT tag {tag}")


class Decoder:
    """Decode packets of one protocol state and direction."""

    def __init__(self, protocol, state, direction):
        section = protocol[state]["toClient" if direction == "clientbound" else "toServer"]
        self.globals = protocol["types"]
        self.locals = section["types"]
        packet = self.locals["packet"][1]
        self.names = {int(key, 0): value for key, value in packet[0]["type"][1]["mappings"].items()}
        self.packets = packet[1]["type"][1]["fields"]
        # Hooks see each value of a named type after it decodes: name -> fn(value).
        self.hooks = {}

    def packet(self, packet_id, body):
        name = self.names.get(packet_id)
        if name is None:
            raise DecodeError(f"unknown packet ID {packet_id:#x}")
        reader = Reader(body)
        value = self.read(reader, self.packets[name], [])
        if reader.remaining():
            raise DecodeError(f"{name}: {reader.remaining()} trailing bytes")
        return name, value

    def resolve(self, name):
        if name in self.locals:
            return self.locals[name]
        if name in self.globals:
            return self.globals[name]
        raise DecodeError(f"unknown type {name}")

    def read(self, reader, kind, scopes):
        if isinstance(kind, str):
            value = self.named(reader, kind, scopes)
            if kind in self.hooks:
                self.hooks[kind](value)
            return value
        name, args = kind
        method = getattr(self, "read_" + name, None)
        if method is None:
            # A parameterised native or a custom type taking arguments.
            return self.named_with(reader, name, args, scopes)
        return method(reader, args, scopes)

    def named(self, reader, name, scopes):
        if name in NUMBERS:
            size = struct.calcsize(NUMBERS[name])
            return struct.unpack(NUMBERS[name], reader.take(size))[0]
        if name in ("varint", "optvarint", "ContainerID"):
            return reader.varint()
        if name == "varlong":
            return reader.varint(10)
        if name == "bool":
            byte = reader.take(1)[0]
            if byte > 1:
                raise DecodeError("invalid boolean")
            return bool(byte)
        if name == "UUID":
            return reader.take(16).hex()
        if name == "void":
            return None
        if name == "restBuffer":
            return reader.take(reader.remaining())
        if name == "anonymousNbt":
            return nbt_payload(reader, reader.take(1)[0])
        if name == "anonOptionalNbt":
            return nbt_payload(reader, reader.take(1)[0])
        if name == "string":
            return self.read_pstring(reader, {"countType": "varint"}, scopes)
        definition = self.resolve(name)
        if definition == "native":
            raise DecodeError(f"unsupported native {name}")
        return self.read(reader, definition, scopes)

    def named_with(self, reader, name, args, scopes):
        raise DecodeError(f"unsupported parameterised type {name}")

    def count(self, reader, args, scopes):
        if "countType" in args:
            count = self.read(reader, args["countType"], scopes)
        else:
            count = args["count"]
            if isinstance(count, str):
                count = self.lookup(count, scopes)
        if not 0 <= count <= reader.remaining() + 1:
            raise DecodeError(f"invalid count {count}")
        return count

    def lookup(self, path, scopes):
        depth = len(scopes) - 1
        parts = path.split("/")
        while parts and parts[0] == "..":
            depth -= 1
            parts.pop(0)
        value = scopes[depth]
        for part in parts:
            value = value[part]
        return value

    def read_pstring(self, reader, args, scopes):
        size = self.count(reader, args, scopes)
        return reader.take(size).decode("utf-8")

    def read_buffer(self, reader, args, scopes):
        return reader.take(self.count(reader, args, scopes))

    def read_array(self, reader, args, scopes):
        return [self.read(reader, args["type"], scopes) for _ in range(self.count(reader, args, scopes))]

    def read_option(self, reader, args, scopes):
        return self.read(reader, args, scopes) if self.named(reader, "bool", scopes) else None

    def read_container(self, reader, args, scopes):
        value = {}
        scopes = scopes + [value]
        for field in args:
            result = self.read(reader, field["type"], scopes)
            if field.get("anon"):
                if isinstance(result, dict):
                    value.update(result)
            else:
                value[field["name"]] = result
        return value

    def read_switch(self, reader, args, scopes):
        key = self.lookup(args["compareTo"], scopes)
        if isinstance(key, bool):
            key = "true" if key else "false"
        key = str(key)
        if key in args["fields"]:
            kind = args["fields"][key]
        elif "default" in args:
            kind = args["default"]
        else:
            return None
        return self.read(reader, kind, scopes)

    def read_mapper(self, reader, args, scopes):
        raw = self.read(reader, args["type"], scopes)
        mappings = {int(key, 0): value for key, value in args["mappings"].items()}
        if raw not in mappings:
            raise DecodeError(f"unmapped value {raw}")
        return mappings[raw]

    def read_bitfield(self, reader, args, scopes):
        bits = sum(field["size"] for field in args)
        raw = int.from_bytes(reader.take(bits // 8), "big")
        value, shift = {}, bits
        for field in args:
            shift -= field["size"]
            part = (raw >> shift) & ((1 << field["size"]) - 1)
            if field["signed"] and part >= 1 << (field["size"] - 1):
                part -= 1 << field["size"]
            value[field["name"]] = part
        return value

    def read_bitflags(self, reader, args, scopes):
        raw = self.read(reader, args["type"], scopes)
        flags = args["flags"]
        if isinstance(flags, list):
            flags = {name: 1 << index for index, name in enumerate(flags)}
        value = {name: bool(raw & mask) for name, mask in flags.items()}
        value["_value"] = raw
        return value

    def read_entityMetadataLoop(self, reader, args, scopes):
        entries = []
        while reader.peek() != args["endVal"]:
            entries.append(self.read(reader, args["type"], scopes))
        reader.take(1)
        return entries

    def read_topBitSetTerminatedArray(self, reader, args, scopes):
        entries = []
        while True:
            more = reader.peek() & 0x80
            entries.append(self.read(reader, args["type"], scopes))
            if not more:
                return entries

    def read_registryEntryHolder(self, reader, args, scopes):
        value = reader.varint()
        if value == 0:
            return {args["otherwise"]["name"]: self.read(reader, args["otherwise"]["type"], scopes)}
        return {args["baseName"]: value - 1}

    def read_registryEntryHolderSet(self, reader, args, scopes):
        size = reader.varint()
        if size == 0:
            return {args["base"]["name"]: self.read(reader, args["base"]["type"], scopes)}
        if not 0 < size <= reader.remaining() + 1:
            raise DecodeError("invalid holder set size")
        return {args["otherwise"]["name"]: [self.read(reader, args["otherwise"]["type"], scopes)
                                             for _ in range(size - 1)]}
