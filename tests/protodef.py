#!/usr/bin/env python3
"""Decode and encode Minecraft packets with PrismarineJS minecraft-data
protocol schemas, for every protocol from 1.12 (335) to 26.1 (775).

This is an independent oracle for protocol translation tests: it shares no
code with Rift's hand-written layouts. Schemas are pinned by commit and
SHA-256 in tests/minecraft_data.json; a local minecraft-data checkout at
RIFT_VIA_CACHE/ref/minecraft-data that has the pinned commit is used before
GitHub. Where a pinned schema disagrees with the official server's stream
codecs, CORRECTIONS replaces the wrong type for the protocols it affects and
records why; nothing is skipped silently. 26.2 and 26.3, which minecraft-data
does not cover yet, are derived from 26.1 in schema_patches.py.

The encoder writes serverbound packets from the same schemas. Fields a caller
leaves out take zero values (0, false, "", absent options, empty arrays,
zeroed fixed buffers), which is what an offline client without chat signing
sends anyway.
"""

import copy
import hashlib
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
PINS = json.loads((ROOT / "tests" / "minecraft_data.json").read_text())
HOME = Path(os.environ.get("RIFT_VIA_CACHE") or Path.home() / ".cache" / "rift-via")
CACHE = HOME / "minecraft-data"
REFERENCE = HOME / "ref" / "minecraft-data"


# Hand-written decoders for types protodef cannot express, by type name:
# name -> fn(reader) (registered by schema_patches for derived schemas).
NATIVES = {}


class DecodeError(ValueError):
    """A decode failure; `path` names the field it happened in."""

    def __init__(self, message):
        super().__init__(message)
        self.message = message
        self.path = []
        self.offset = None

    def __str__(self):
        where = ".".join(str(part) for part in reversed(self.path))
        at = f" at byte {self.offset}" if self.offset is not None else ""
        return f"{self.message} (in {where}{at})" if where else f"{self.message}{at}"


def _within(error, part):
    error.path.append(part)
    return error


class Unverified(DecodeError):
    """A layout no checked schema covers (see schema_patches.UNVERIFIED)."""


def versions():
    """Pinned schema names by protocol number."""
    return {pin["protocol"]: name for name, pin in PINS["protocols"].items()}


def raw_schema(version):
    """The pinned, checksum-verified protocol.json for `version`, uncorrected."""
    pin = PINS["protocols"][version]
    commit = pin.get("commit", PINS["commit"])
    path = CACHE / f"{commit}-{pin['path']}-protocol.json"
    if not path.exists():
        data = None
        if (REFERENCE / ".git").exists():
            result = subprocess.run(
                ["git", "-C", str(REFERENCE), "show", f"{commit}:data/pc/{pin['path']}/protocol.json"],
                capture_output=True)
            data = result.stdout if result.returncode == 0 else None
        if data is None:
            url = (f"https://raw.githubusercontent.com/PrismarineJS/minecraft-data/"
                   f"{commit}/data/pc/{pin['path']}/protocol.json")
            with urllib.request.urlopen(url, timeout=60) as response:
                data = response.read()
        if hashlib.sha256(data).hexdigest() != pin["sha256"]:
            raise ValueError(f"minecraft-data checksum mismatch for {version}")
        CACHE.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(dir=CACHE, delete=False) as pending:
            pending.write(data)
        Path(pending.name).replace(path)
    data = path.read_bytes()
    if hashlib.sha256(data).hexdigest() != pin["sha256"]:
        raise ValueError(f"cached minecraft-data checksum mismatch for {version}")
    return json.loads(data)


_schemas = {}


def schema(version):
    """The corrected schema for a pinned version name, or a derived one
    (26.2, 26.3)."""
    if version not in _schemas:
        if version in PINS["protocols"]:
            protocol = PINS["protocols"][version]["protocol"]
            _schemas[version] = corrected(raw_schema(version), protocol)
        else:
            import schema_patches
            _schemas[version] = schema_patches.derive(version)
    return _schemas[version]


def schema_for(protocol):
    import schema_patches
    names = versions()
    if protocol in names:
        return schema(names[protocol])
    if protocol in schema_patches.DERIVED:
        return schema(schema_patches.DERIVED[protocol])
    raise KeyError(f"no schema for protocol {protocol}")


def _container(*fields):
    return ["container", [{"name": name, "type": kind} for name, kind in fields]]


# Each entry replaces the value at `path` (a key path into protocol.json) for
# protocols first..last. A correction whose target is missing fails loudly, so
# a schema update cannot silently disable it. Every entry was checked against
# the official server's stream codecs.
def _mapping(names):
    return {str(index): f"minecraft:{name}" for index, name in enumerate(names)}


SLOT_COMPONENT = ["types", "SlotComponent", 1, 1, "type", 1, "fields"]
RECIPE_TYPES = ["play", "toClient", "types", "packet_declare_recipes", 1, 0, "type", 1, "type", 1, 1,
                "type", 1, "mappings"]

GLOBAL_POS = ["option", _container(("dimension", "string"), ("position", "position"))]
METADATA_1_19 = ["types", "entityMetadataItem", 1, "fields"]

CORRECTIONS = [
    dict(protocols=(765, 765), path=["play", "toClient", "types", "packet_explosion", 1,
                                     {"field": "sound"}, "type"],
         value=_container(("soundName", "string"), ("range", ["option", "f32"])),
         why="1.20.3/1.20.4 ClientboundExplodePacket writes the explosion sound with "
             "SoundEvent.writeToNetwork (location and optional fixed range), not a registry holder; "
             "holders came in 1.20.5. A real TNT explosion left 33 trailing bytes."),
    dict(protocols=(759, 759), path=METADATA_1_19 + ["19"], value="varint", add=True,
         why="1.19 added the cat variant serializer (EntityDataSerializers.CAT_VARIANT, ID 19); "
             "the schema stops at 18 (a summoned painting's metadata failed to decode)."),
    dict(protocols=(759, 759), path=METADATA_1_19 + ["20"], value="varint", add=True,
         why="1.19 frog variant serializer (ID 20)."),
    dict(protocols=(759, 760), path=METADATA_1_19 + ["21"], value=GLOBAL_POS, add=True,
         why="1.19 optional global position serializer (ID 21): GlobalPos is a dimension key and "
             "a block position, not a string."),
    dict(protocols=(759, 759), path=METADATA_1_19 + ["22"], value="varint", add=True,
         why="1.19 painting variant serializer (ID 22): a registry ID."),
    dict(protocols=(761, 765), path=["play", "toClient", "types", "packet_respawn", 1,
                                     {"field": "copyMetadata"}, "type"], value="u8",
         why="ClientboundRespawnPacket writes the dataToKeep flags byte (KEEP_ATTRIBUTES 1, "
             "KEEP_ENTITY_DATA 2) since 1.19.3, not a boolean; a dimension change sends 3."),
    dict(protocols=(766, 767), path=RECIPE_TYPES,
         value=_mapping(["crafting_shaped", "crafting_shapeless", "crafting_special_armordye",
                         "crafting_special_bookcloning", "crafting_special_mapcloning",
                         "crafting_special_mapextending", "crafting_special_firework_rocket",
                         "crafting_special_firework_star", "crafting_special_firework_star_fade",
                         "crafting_special_tippedarrow", "crafting_special_bannerduplicate",
                         "crafting_special_shielddecoration", "crafting_special_shulkerboxcoloring",
                         "crafting_special_suspiciousstew", "crafting_special_repairitem", "smelting",
                         "blasting", "smoking", "campfire_cooking", "stonecutting", "smithing_transform",
                         "smithing_trim", "crafting_decorated_pot"]),
         why="1.20.5 removed the banner pattern recipe serializer; the schema still lists "
             "crafting_special_banneraddpattern, shifting every later ID (the 1.20.6 and 1.21.1 data generators' "
             "minecraft:recipe_serializer registries are identical to this list)."),
    dict(protocols=(766, 767), path=SLOT_COMPONENT + ["potion_contents"],
         value=_container(("potionId", ["option", "varint"]), ("customColor", ["option", "i32"]),
                          ("customEffects", ["array", {"countType": "varint", "type": "ItemPotionEffect"}])),
         why="PotionContents.STREAM_CODEC gained the optional custom name in 1.21.2; 1.20.5-1.21.1 "
             "end after the effects (ViaVersion PotionContents.TYPE1_20_5)."),
    dict(protocols=(771, 772),
         path=["types", "entityMetadataEntry", 1, 2, "type", 1, "fields", "optional_global_pos"],
         value=["option", _container(("dimension", "string"), ("position", "position"))],
         why="GlobalPos.STREAM_CODEC is a dimension key followed by a block position."),
    dict(protocols=(771, 772), path=["types", "EntityMetadataPaintingVariant"],
         value=_container(("width", "varint"), ("height", "varint"), ("assetId", "string"),
                          ("title", ["option", "anonymousNbt"]), ("author", ["option", "anonymousNbt"])),
         why="PaintingVariant.DIRECT_STREAM_CODEC uses VarInt width and height."),
    dict(protocols=(771, 772), path=["types", "ItemBlockPredicate"],
         value=_container(
             ("blockSet", ["option", ["registryEntryHolderSet", {
                 "base": {"name": "name", "type": "string"},
                 "otherwise": {"name": "ids", "type": "varint"}}]]),
             ("properties", ["option", ["array", {"countType": "varint", "type": "ItemBlockProperty"}]]),
             ("nbt", ["option", "anonymousNbt"]),
             ("components", "DataComponentMatchers")),
         why="BlockPredicate.STREAM_CODEC: an optional NbtPredicate (boolean-prefixed compound)."),
    dict(protocols=(771, 772), path=["types", "DataComponentMatchers"], add=True,
         value=_container(
             ("exactMatchers", "ExactComponentMatcher"),
             ("partialMatchers", ["array", {"countType": "varint", "type": _container(
                 ("type", "varint"), ("predicate", "anonymousNbt"))}])),
         why="Partial component predicates are (type, NBT) pairs."),
    dict(protocols=(771, 772), path=["types", "ItemBlockProperty"],
         value=_container(
             ("name", "string"), ("isExactMatch", "bool"),
             ("value", ["switch", {"compareTo": "isExactMatch", "fields": {
                 "true": _container(("exactValue", "string")),
                 "false": _container(("minValue", ["option", "string"]),
                                     ("maxValue", ["option", "string"]))}}])),
         why="StatePropertiesPredicate.ValueMatcher is ByteBufCodecs.either(exact, range), "
             "and a range's bounds are each optional."),
]


def corrections_for(protocol):
    return [entry for entry in CORRECTIONS
            if entry["protocols"][0] <= protocol <= entry["protocols"][1]]


def _key(target, key, path):
    """A path element; {"field": name} selects a container field by name."""
    if isinstance(key, dict):
        for index, field in enumerate(target):
            if isinstance(field, dict) and field.get("name") == key["field"]:
                return index
        raise KeyError(f"correction target {path} is missing")
    return key


def patch(document, path, value, add=False):
    """Replace `document` at `path`; the target must exist unless `add`."""
    target = document
    for key in path[:-1]:
        try:
            target = target[_key(target, key, path)]
        except (KeyError, IndexError, TypeError):
            raise KeyError(f"correction target {path} is missing") from None
    last = _key(target, path[-1], path)
    if isinstance(target, dict):
        if last not in target and not add:
            raise KeyError(f"correction target {path} is missing")
        target[last] = value
    else:
        target[last] = value


def corrected(protocol_json, protocol=772):
    protocol_json = copy.deepcopy(protocol_json)
    for entry in corrections_for(protocol):
        patch(protocol_json, entry["path"], entry["value"], entry.get("add", False))
    return protocol_json


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
        if count < 0:
            raise DecodeError("negative NBT array length")
        return list(struct.unpack(f">{count}{code}", reader.take(count * width)))
    if tag == 8:
        return reader.take(struct.unpack(">H", reader.take(2))[0]).decode("utf-8", "surrogatepass")
    if tag == 9:
        element = reader.take(1)[0]
        count = struct.unpack(">i", reader.take(4))[0]
        if count > 0 and element == 0:
            raise DecodeError("NBT list of TAG_End with elements")
        return [nbt_payload(reader, element, depth + 1) for _ in range(max(count, 0))]
    if tag == 10:
        value = {}
        while (element := reader.take(1)[0]) != 0:
            name = reader.take(struct.unpack(">H", reader.take(2))[0]).decode("utf-8", "surrogatepass")
            value[name] = nbt_payload(reader, element, depth + 1)
        return value
    raise DecodeError(f"invalid NBT tag {tag}")


def named_nbt(reader, optional):
    """A pre-1.20.2 network NBT root: tag, name, payload. TAG_End means none."""
    tag = reader.take(1)[0]
    if tag == 0:
        if optional:
            return None
        raise DecodeError("missing NBT root")
    reader.take(struct.unpack(">H", reader.take(2))[0])
    return nbt_payload(reader, tag)


def lp_vec3(reader):
    """net.minecraft.network.LpVec3 (1.21.9+): 15-bit components scaled by a
    shared factor, or a single zero byte for the zero vector."""
    lowest = reader.take(1)[0]
    if lowest == 0:
        return [0.0, 0.0, 0.0]
    middle = reader.take(1)[0]
    highest = struct.unpack(">I", reader.take(4))[0]
    packed = highest << 16 | middle << 8 | lowest
    scale = lowest & 3
    if lowest & 4:
        scale |= (reader.varint() & 0xFFFFFFFF) << 2
    unpack = lambda value: min(value & 32767, 32766) * 2.0 / 32766.0 - 1.0
    return [unpack(packed >> 3) * scale, unpack(packed >> 18) * scale, unpack(packed >> 33) * scale]


def substitute(kind, args):
    """A parameterised type's definition with its `$name` placeholders filled."""
    if isinstance(kind, str):
        return args[kind[1:]] if kind.startswith("$") else kind
    if isinstance(kind, list):
        return [substitute(entry, args) for entry in kind]
    if isinstance(kind, dict):
        return {key: substitute(value, args) for key, value in kind.items()}
    return kind


class Codec:
    """Shared schema lookups for one protocol state and direction."""

    def __init__(self, protocol, state, direction):
        section = protocol[state]["toClient" if direction == "clientbound" else "toServer"]
        self.globals = protocol["types"]
        self.locals = section["types"]
        packet = self.locals["packet"][1]
        self.names = {int(key, 0): value for key, value in packet[0]["type"][1]["mappings"].items()}
        self.ids = {name: packet_id for packet_id, name in self.names.items()}
        self.packets = packet[1]["type"][1]["fields"]
        self.parameterised = {}

    def resolve(self, name):
        if name in self.locals:
            return self.locals[name]
        if name in self.globals:
            return self.globals[name]
        raise DecodeError(f"unknown type {name}")

    def instantiate(self, name, args):
        key = (name, json.dumps(args, sort_keys=True))
        if key not in self.parameterised:
            definition = self.resolve(name)
            if definition == "native":
                raise DecodeError(f"unsupported parameterised native {name}")
            self.parameterised[key] = substitute(definition, args)
        return self.parameterised[key]

    @staticmethod
    def lookup(path, scopes):
        depth = len(scopes) - 1
        parts = path.split("/")
        while parts and parts[0] == "..":
            depth -= 1
            parts.pop(0)
        value = scopes[depth]
        for part in parts:
            value = value[part]
        return value


class Decoder(Codec):
    """Decode packets of one protocol state and direction."""

    def __init__(self, protocol, state, direction):
        super().__init__(protocol, state, direction)
        # Hooks see each value of a named type after it decodes: name -> fn(value).
        self.hooks = {}

    def packet(self, packet_id, body):
        name = self.names.get(packet_id)
        if name is None:
            raise DecodeError(f"unknown packet ID {packet_id:#x}")
        reader = Reader(body)
        try:
            value = self.read(reader, self.packets[name], [])
        except DecodeError as error:
            if error.offset is None:
                error.offset = reader.offset
            raise
        except RecursionError:
            raise DecodeError(f"{name}: nesting too deep") from None
        if reader.remaining():
            error = DecodeError(f"{name}: {reader.remaining()} trailing bytes")
            error.offset = reader.offset
            raise error
        return name, value

    def read(self, reader, kind, scopes):
        if isinstance(kind, str):
            value = self.named(reader, kind, scopes)
            if kind in self.hooks:
                self.hooks[kind](value)
            return value
        name, args = kind
        if name == "unverified":
            raise Unverified(f"unverified layout: {args}")
        method = getattr(self, "read_" + name, None)
        if method is None:
            # A custom type taking arguments, such as particleData.
            return self.read(reader, self.instantiate(name, args), scopes)
        return method(reader, args, scopes)

    def named(self, reader, name, scopes):
        if name in NUMBERS:
            size = struct.calcsize(NUMBERS[name])
            return struct.unpack(NUMBERS[name], reader.take(size))[0]
        if name == "varint":
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
        if name in ("anonymousNbt", "anonOptionalNbt"):
            return nbt_payload(reader, reader.take(1)[0])
        if name == "nbt":
            return named_nbt(reader, optional=False)
        if name == "optionalNbt":
            return named_nbt(reader, optional=True)
        if name == "lpVec3":
            return lp_vec3(reader)
        if name == "string":
            return self.read_pstring(reader, {"countType": "varint"}, scopes)
        if name in NATIVES:
            return NATIVES[name](reader)
        definition = self.resolve(name)
        if definition == "native":
            raise DecodeError(f"unsupported native {name}")
        if isinstance(definition, list) and definition and definition[0] == "unverified":
            raise Unverified(f"unverified layout {name}: {definition[1]}")
        try:
            return self.read(reader, definition, scopes)
        except DecodeError as error:
            raise _within(error, f"<{name}>")

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

    def read_pstring(self, reader, args, scopes):
        size = self.count(reader, args, scopes)
        try:
            return reader.take(size).decode("utf-8")
        except UnicodeDecodeError as error:
            raise DecodeError(f"invalid UTF-8: {error}") from None

    def read_buffer(self, reader, args, scopes):
        return reader.take(self.count(reader, args, scopes))

    def read_array(self, reader, args, scopes):
        result = []
        for index in range(self.count(reader, args, scopes)):
            try:
                result.append(self.read(reader, args["type"], scopes))
            except DecodeError as error:
                raise _within(error, f"[{index}]")
        return result

    def read_option(self, reader, args, scopes):
        return self.read(reader, args, scopes) if self.named(reader, "bool", scopes) else None

    def read_container(self, reader, args, scopes):
        value = {}
        scopes = scopes + [value]
        for field in args:
            try:
                result = self.read(reader, field["type"], scopes)
            except DecodeError as error:
                raise _within(error, field.get("name", "<anon>"))
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
        try:
            return self.read(reader, kind, scopes)
        except DecodeError as error:
            raise _within(error, f"<{key}>")

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


def write_varint(value, width=5):
    value &= (1 << (7 * width if width > 5 else 32)) - 1
    out = bytearray()
    while True:
        byte = value & 127
        value >>= 7
        if value:
            out.append(byte | 128)
        else:
            out.append(byte)
            return bytes(out)


class Raw(bytes):
    """Pre-encoded bytes the encoder writes verbatim (NBT, nested data)."""


class Encoder(Codec):
    """Encode packets of one protocol state and direction from dict values."""

    def packet(self, name, value=None):
        if name not in self.ids:
            raise KeyError(f"no {name} packet in this protocol state")
        out = bytearray()
        self.write(out, self.packets[name], value if value is not None else {}, [])
        return self.ids[name], bytes(out)

    def write(self, out, kind, value, scopes):
        if isinstance(kind, str):
            return self.named(out, kind, value, scopes)
        name, args = kind
        method = getattr(self, "write_" + name, None)
        if method is None:
            return self.write(out, self.instantiate(name, args), value, scopes)
        return method(out, args, value, scopes)

    def named(self, out, name, value, scopes):
        if isinstance(value, Raw):
            out += value
            return value
        if name in NUMBERS:
            value = value or 0
            if name in ("f32", "f64"):
                value = float(value)
            out += struct.pack(NUMBERS[name], value)
            return value
        if name == "varint":
            out += write_varint(value or 0)
            return value or 0
        if name == "varlong":
            out += write_varint(value or 0, 10)
            return value or 0
        if name == "bool":
            out.append(1 if value else 0)
            return bool(value)
        if name == "UUID":
            out += bytes.fromhex(value) if value else bytes(16)
            return value
        if name == "void":
            return None
        if name == "restBuffer":
            out += value or b""
            return value
        if name in ("anonOptionalNbt", "optionalNbt", "anonymousNbt", "nbt"):
            if value is not None:
                raise ValueError(f"pass {name} values as Raw bytes")
            out.append(0)  # TAG_End: no NBT.
            return None
        if name == "lpVec3":
            if value and any(value):
                raise ValueError("only the zero lpVec3 is supported")
            out.append(0)
            return value
        if name == "string":
            return self.write_pstring(out, {"countType": "varint"}, value, scopes)
        definition = self.resolve(name)
        if definition == "native":
            raise ValueError(f"unsupported native {name}")
        if isinstance(definition, list) and definition and definition[0] == "unverified":
            raise ValueError(f"cannot encode unverified layout {name}: {definition[1]}")
        return self.write(out, definition, value, scopes)

    def write_count(self, out, args, count, scopes):
        if "countType" in args:
            self.write(out, args["countType"], count, scopes)
        elif isinstance(args["count"], int) and args["count"] != count:
            raise ValueError(f"fixed count {args['count']}, got {count}")

    def write_pstring(self, out, args, value, scopes):
        data = (value or "").encode()
        self.write_count(out, args, len(data), scopes)
        out += data
        return value or ""

    def write_buffer(self, out, args, value, scopes):
        if value is None and isinstance(args.get("count"), int):
            value = bytes(args["count"])
        value = value or b""
        self.write_count(out, args, len(value), scopes)
        out += value
        return value

    def write_array(self, out, args, value, scopes):
        value = value or []
        self.write_count(out, args, len(value), scopes)
        return [self.write(out, args["type"], entry, scopes) for entry in value]

    def write_option(self, out, args, value, scopes):
        out.append(0 if value is None else 1)
        return None if value is None else self.write(out, args, value, scopes)

    def write_container(self, out, args, value, scopes):
        value = dict(value or {})
        written = {}
        scopes = scopes + [written]
        # Counts named by a later array default to that array's length.
        for field in args:
            kind = field["type"]
            if isinstance(kind, list) and kind[0] in ("array", "buffer") and \
                    isinstance(kind[1].get("count"), str) and "/" not in kind[1]["count"]:
                value.setdefault(kind[1]["count"], len(value.get(field["name"]) or []))
        for field in args:
            if field.get("anon"):
                result = self.write(out, field["type"], value, scopes)
                if isinstance(result, dict):
                    written.update(result)
            else:
                written[field["name"]] = self.write(out, field["type"], value.get(field["name"]), scopes)
        return written

    def write_switch(self, out, args, value, scopes):
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
        return self.write(out, kind, value, scopes)

    def write_mapper(self, out, args, value, scopes):
        mappings = {name: int(key, 0) for key, name in args["mappings"].items()}
        if value is None:
            raw = 0 if 0 in mappings.values() else min(mappings.values())
            value = next(name for name, key in mappings.items() if key == raw)
        elif isinstance(value, int) and not isinstance(value, bool):
            raw = value
        else:
            raw = mappings[value]
        self.write(out, args["type"], raw, scopes)
        return value

    def write_bitfield(self, out, args, value, scopes):
        value = value or {}
        bits = sum(field["size"] for field in args)
        raw = 0
        for field in args:
            part = value.get(field["name"], 0) & ((1 << field["size"]) - 1)
            raw = raw << field["size"] | part
        out += raw.to_bytes(bits // 8, "big")
        return value

    def write_bitflags(self, out, args, value, scopes):
        value = value or {}
        flags = args["flags"]
        if isinstance(flags, list):
            flags = {name: 1 << index for index, name in enumerate(flags)}
        raw = sum(mask for name, mask in flags.items() if value.get(name))
        self.write(out, args["type"], raw, scopes)
        return value

    def write_registryEntryHolder(self, out, args, value, scopes):
        value = value or {args["baseName"]: 0}
        if args["baseName"] in value:
            out += write_varint(value[args["baseName"]] + 1)
        else:
            out += write_varint(0)
            self.write(out, args["otherwise"]["type"], value[args["otherwise"]["name"]], scopes)
        return value

    def write_registryEntryHolderSet(self, out, args, value, scopes):
        value = value or {args["otherwise"]["name"]: []}
        if args["base"]["name"] in value:
            out += write_varint(0)
            self.write(out, args["base"]["type"], value[args["base"]["name"]], scopes)
        else:
            entries = value[args["otherwise"]["name"]]
            out += write_varint(len(entries) + 1)
            for entry in entries:
                self.write(out, args["otherwise"]["type"], entry, scopes)
        return value
