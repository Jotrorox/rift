#!/usr/bin/env python3
"""Generate crates/rift-translation/data/entities from Mojang's server jars.

For every protocol the translation crate supports this writes
data/entities/<protocol>.json with what the entity codec needs to convert
entity data between versions:

- the entity-data serializers in network-ID order, by kind;
- every entity class that defines entity data, with its parent and its
  fields (entity-data accessors) in definition order, each with a key that
  stays the same across versions for the same field;
- the class of every entity type, by network name.

All of it is read from the bytecode of the official server jars pinned in
tests/servers.json (cached under ~/.cache/rift-via/servers): entity data
indexes are handed out per class in the order the class's static
initialiser calls `SynchedEntityData.defineId`, continuing from the parent
class, so the class hierarchy and those calls determine every index and
serializer. Serializer IDs are the order of `EntityDataSerializers`'
registrations. For 1.14.4 to 1.21.11 Mojang's official obfuscation mappings
(downloaded to ~/.cache/rift-via/mappings) name the classes, fields and
serializers; 26.x jars are not obfuscated. 1.12 to 1.14.3 have no mappings:
classes are recognised by the entity types they serve and by their position
in the hierarchy, serializers by their value type (the generic signature of
their field) against SERIALIZER_KINDS, and fields inherit their keys from the
same field one version later (ALIGN lists where that needs help).

Usage (Python 3.11+, stdlib only):

    python3 scripts/translation_entities.py            # regenerate
    python3 scripts/translation_entities.py --check    # fail if stale
    python3 scripts/translation_entities.py --report   # how versions align
    python3 scripts/translation_entities.py --validate DUMP  # schema check
"""

import argparse
from concurrent.futures import ProcessPoolExecutor
import hashlib
import io
import json
import os
from pathlib import Path
import re
import struct
import sys
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[1]
CRATE = ROOT / "crates" / "rift-translation"
OUTPUT = CRATE / "data" / "entities"
CACHE = Path(os.environ.get("RIFT_VIA_CACHE", Path.home() / ".cache" / "rift-via"))
SERVERS = json.loads((ROOT / "tests" / "servers.json").read_text())
PROTOCOLS = [(int(protocol), name) for protocol, name in re.findall(
    r'\((\d+), "([^"]+)"\)', (CRATE / "src" / "version.rs").read_text().split("#[cfg(test)]")[0])]
MANIFEST = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"


def log(message):
    print(message, file=sys.stderr, flush=True)


# Class files.

class ClassFile:
    """The parts of a class file this script reads."""

    def __init__(self, data):
        self.data = data
        if data[:4] != b"\xca\xfe\xba\xbe":
            raise ValueError("not a class file")
        position = 8
        count = struct.unpack_from(">H", data, position)[0]
        position += 2
        pool = [None] * count
        index = 1
        while index < count:
            tag = data[position]
            position += 1
            if tag == 1:
                size = struct.unpack_from(">H", data, position)[0]
                raw = data[position + 2:position + 2 + size]
                pool[index] = (1, raw.decode("utf-8", "surrogatepass")
                               if b"\xc0\x80" not in raw and b"\xed" not in raw
                               else raw.decode("utf-8", "replace"))
                position += 2 + size
            elif tag in (3, 4):
                pool[index] = (tag, data[position:position + 4])
                position += 4
            elif tag in (5, 6):
                pool[index] = (tag, data[position:position + 8])
                position += 8
                index += 1
            elif tag in (7, 8, 16, 19, 20):
                pool[index] = (tag, struct.unpack_from(">H", data, position)[0])
                position += 2
            elif tag in (9, 10, 11, 12, 17, 18):
                pool[index] = (tag,) + struct.unpack_from(">HH", data, position)
                position += 4
            elif tag == 15:
                pool[index] = (tag, data[position], struct.unpack_from(">H", data, position + 1)[0])
                position += 3
            else:
                raise ValueError(f"unknown constant pool tag {tag}")
            index += 1
        self.pool = pool
        self.access, this, parent, interfaces = struct.unpack_from(">HHHH", data, position)
        position += 8
        self.name = self.class_name(this)
        self.parent = self.class_name(parent) if parent else None
        self.interfaces = [self.class_name(struct.unpack_from(">H", data, position + 2 * i)[0])
                           for i in range(interfaces)]
        position += 2 * interfaces
        self.fields = []
        position = self._members(position, self.fields)
        self.methods = []
        position = self._members(position, self.methods)
        self.bootstrap = []
        for name, value in self._attributes(position):
            if name == "BootstrapMethods":
                count = struct.unpack_from(">H", value, 0)[0]
                offset = 2
                for _ in range(count):
                    method, arguments = struct.unpack_from(">HH", value, offset)
                    offset += 4
                    self.bootstrap.append((method, [
                        struct.unpack_from(">H", value, offset + 2 * i)[0]
                        for i in range(arguments)]))
                    offset += 2 * arguments

    def _attributes(self, position):
        count = struct.unpack_from(">H", self.data, position)[0]
        position += 2
        result = []
        for _ in range(count):
            name, size = struct.unpack_from(">HI", self.data, position)
            result.append((self.utf8(name), self.data[position + 6:position + 6 + size]))
            position += 6 + size
        self._end = position
        return result

    def _members(self, position, into):
        count = struct.unpack_from(">H", self.data, position)[0]
        position += 2
        for _ in range(count):
            access, name, descriptor = struct.unpack_from(">HHH", self.data, position)
            attributes = dict(self._attributes(position + 6))
            position = self._end
            into.append(Member(self, access, self.utf8(name), self.utf8(descriptor), attributes))
        return position

    def utf8(self, index):
        return self.pool[index][1]

    def class_name(self, index):
        return self.utf8(self.pool[index][1])

    def reference(self, index):
        """(owner, name, descriptor) of a field or method reference."""
        _, owner, name_and_type = self.pool[index]
        _, name, descriptor = self.pool[name_and_type]
        return self.class_name(owner), self.utf8(name), self.utf8(descriptor)

    def constant(self, index):
        entry = self.pool[index]
        if entry[0] == 7:
            return ("class", self.utf8(entry[1]))
        if entry[0] == 8:
            return ("string", self.utf8(entry[1]))
        if entry[0] == 3:
            return ("int", struct.unpack(">i", entry[1])[0])
        if entry[0] == 4:
            return ("float", struct.unpack(">f", entry[1])[0])
        return ("other", None)

    def method_handle(self, index):
        """(kind, owner, name, descriptor) of a method handle constant."""
        _, kind, reference = self.pool[index]
        return (kind,) + self.reference(reference)

    def method(self, name, descriptor=None):
        for method in self.methods:
            if method.name == name and (descriptor is None or method.descriptor == descriptor):
                return method
        return None


class Member:
    def __init__(self, owner, access, name, descriptor, attributes):
        self.owner = owner
        self.access = access
        self.name = name
        self.descriptor = descriptor
        self.attributes = attributes

    @property
    def is_static(self):
        return bool(self.access & 0x0008)

    @property
    def signature(self):
        value = self.attributes.get("Signature")
        return self.owner.utf8(struct.unpack(">H", value)[0]) if value else None

    def code(self):
        """The method's instructions as (opcode, operand) pairs. Operands are
        resolved: ("class"|"string"|"int"|..., value) for ldc, (owner, name,
        descriptor) for member references, the bootstrap entry for
        invokedynamic, the immediate value for push instructions."""
        raw = self.attributes.get("Code")
        if raw is None:
            return []
        size = struct.unpack_from(">I", raw, 4)[0]
        code = raw[8:8 + size]
        owner = self.owner
        result = []
        position = 0
        while position < len(code):
            opcode = code[position]
            start = position
            operand = None
            if opcode in (0x12,):  # ldc
                operand = owner.constant(code[position + 1])
                position += 2
            elif opcode in (0x13, 0x14):  # ldc_w, ldc2_w
                operand = owner.constant(struct.unpack_from(">H", code, position + 1)[0])
                position += 3
            elif opcode in (0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8):
                operand = owner.reference(struct.unpack_from(">H", code, position + 1)[0])
                position += 3
            elif opcode == 0xb9:  # invokeinterface
                operand = owner.reference(struct.unpack_from(">H", code, position + 1)[0])
                position += 5
            elif opcode == 0xba:  # invokedynamic
                index = struct.unpack_from(">H", code, position + 1)[0]
                _, bootstrap, name_and_type = owner.pool[index]
                _, name, descriptor = owner.pool[name_and_type]
                method, arguments = owner.bootstrap[bootstrap]
                handles = [owner.method_handle(argument) for argument in arguments
                           if owner.pool[argument][0] == 15]
                operand = (owner.utf8(name), owner.utf8(descriptor), handles)
                position += 5
            elif opcode in (0xbb, 0xbd, 0xc0, 0xc1):  # new, anewarray, checkcast, instanceof
                operand = owner.class_name(struct.unpack_from(">H", code, position + 1)[0])
                position += 3
            elif opcode == 0x10:  # bipush
                operand = struct.unpack_from(">b", code, position + 1)[0]
                position += 2
            elif opcode == 0x11:  # sipush
                operand = struct.unpack_from(">h", code, position + 1)[0]
                position += 3
            elif 0x02 <= opcode <= 0x08:  # iconst_m1 .. iconst_5
                operand = opcode - 0x03
                position += 1
            elif opcode == 0xaa:  # tableswitch
                position = (position + 4) & ~3
                low, high = struct.unpack_from(">ii", code, position + 4)
                position += 12 + 4 * (high - low + 1)
            elif opcode == 0xab:  # lookupswitch
                position = (position + 4) & ~3
                pairs = struct.unpack_from(">i", code, position + 4)[0]
                position += 8 + 8 * pairs
            elif opcode == 0xc4:  # wide
                position += 6 if code[position + 1] == 0x84 else 4
            else:
                position += 1 + OPERAND_SIZES.get(opcode, 0)
            result.append((opcode, operand, start))
        return result


OPERAND_SIZES = {
    0x15: 1, 0x16: 1, 0x17: 1, 0x18: 1, 0x19: 1,  # loads
    0x36: 1, 0x37: 1, 0x38: 1, 0x39: 1, 0x3a: 1,  # stores
    0xa9: 1, 0xbc: 1,  # ret, newarray
    0x84: 2,  # iinc
    **{opcode: 2 for opcode in range(0x99, 0xa9)},  # if*, goto, jsr
    0xc6: 2, 0xc7: 2,  # ifnull, ifnonnull
    0xc5: 3,  # multianewarray
    0xc8: 4, 0xc9: 4,  # goto_w, jsr_w
}


class Jar:
    """Every class of a server jar, parsed on demand."""

    def __init__(self, path):
        archive = zipfile.ZipFile(path)
        nested = [name for name in archive.namelist()
                  if name.startswith("META-INF/versions/") and name.endswith(".jar")]
        if nested:
            archive = zipfile.ZipFile(io.BytesIO(archive.read(nested[0])))
        self.raw = {name[:-6]: archive.read(name) for name in archive.namelist()
                    if name.endswith(".class")
                    and ("/" not in name or name.startswith("net/minecraft/"))}
        self.cache = {}

    def get(self, name):
        if name not in self.raw:
            return None
        if name not in self.cache:
            self.cache[name] = ClassFile(self.raw[name])
        return self.cache[name]

    def grep(self, *needles):
        """The classes whose bytes contain any of `needles`."""
        for name in sorted(self.raw):
            if any(needle in self.raw[name] for needle in needles):
                yield self.get(name)


# Inputs.

def server(version):
    """The pinned vanilla server jar of a release, checksum verified."""
    matches = [fixture for key, fixture in SERVERS.items()
               if key.startswith("vanilla") and fixture["version"] == version]
    if len(matches) != 1:
        raise SystemExit(f"tests/servers.json has no single vanilla {version} server")
    fixture = matches[0]
    path = CACHE / "servers" / fixture["filename"]
    if not path.exists():
        raise SystemExit(f"{path} is missing; run scripts/translation_crate.py --reports first")
    with path.open("rb") as source:
        actual = hashlib.file_digest(source, fixture["algorithm"]).hexdigest()
    if actual != fixture["checksum"]:
        raise SystemExit(f"checksum mismatch: {path}")
    return path


def mappings_text(version):
    """Mojang's official server mappings of a release, or None before 1.14.4
    and for unobfuscated releases. Downloaded once, checked by SHA-1."""
    path = CACHE / "mappings" / f"{version}.txt"
    meta = CACHE / "mappings" / f"{version}.json"
    if not meta.exists():
        manifest = json.load(urllib.request.urlopen(MANIFEST, timeout=60))
        entry = next(entry for entry in manifest["versions"] if entry["id"] == version)
        details = json.load(urllib.request.urlopen(entry["url"], timeout=60))
        meta.parent.mkdir(parents=True, exist_ok=True)
        meta.write_text(json.dumps(details["downloads"].get("server_mappings")))
    download = json.loads(meta.read_text())
    if download is None:
        return None
    if not path.exists() or hashlib.sha1(path.read_bytes()).hexdigest() != download["sha1"]:
        log(f"Downloading {version} mappings...")
        path.write_bytes(urllib.request.urlopen(download["url"], timeout=300).read())
    if hashlib.sha1(path.read_bytes()).hexdigest() != download["sha1"]:
        raise SystemExit(f"checksum mismatch: {path}")
    return path.read_text()


class Names:
    """Obfuscated to Mojang names; the identity for unobfuscated jars."""

    def __init__(self, text):
        self.classes = {}
        self.fields = {}
        self.methods = {}
        if text is None:
            return
        current = None
        for line in text.splitlines():
            if line.startswith("#") or not line.strip():
                continue
            if not line.startswith(" "):
                named, obfuscated = line.rstrip(":").split(" -> ")
                current = obfuscated.replace(".", "/")
                self.classes[current] = named.replace(".", "/")
            elif "(" not in line:
                _, rest = line.strip().split(" ", 1)
                named, obfuscated = rest.split(" -> ")
                self.fields[(current, obfuscated)] = named
            else:
                signature, obfuscated = line.strip().split(" -> ")
                named = signature.split("(")[0].rsplit(" ", 1)[-1]
                self.methods.setdefault((current, obfuscated), set()).add(named)

    def cls(self, name):
        return self.classes.get(name, name)

    def field(self, owner, name):
        return self.fields.get((owner, name), name)

    def method(self, owner, name):
        return self.methods.get((owner, name), {name})


def simple(name):
    return name.rsplit("/", 1)[-1].rsplit("$", 1)[-1]


# Extraction.

GETSTATIC, PUTSTATIC, INVOKESTATIC, AASTORE, LDC = 0xb2, 0xb3, 0xb8, 0x53, {0x12, 0x13}


def find_define(jar):
    """`SynchedEntityData.defineId`: the static (Class, serializer) method
    the most static initialisers call."""
    counts = {}
    for cls in jar.grep(b"(Ljava/lang/Class;L"):
        initialiser = cls.method("<clinit>")
        if initialiser is None:
            continue
        code = initialiser.code()
        for index, (opcode, operand, _) in enumerate(code):
            if (opcode == INVOKESTATIC and operand[2].startswith("(Ljava/lang/Class;L")
                    and index >= 2 and code[index - 1][0] == GETSTATIC
                    and code[index - 2][0] in LDC and code[index - 2][1][0] == "class"):
                counts[operand] = counts.get(operand, 0) + 1
    return max(counts, key=counts.get)


def serializers(jar, registry, names):
    """The serializer fields of the registry class in network-ID order, as
    (field, value type, names of the static methods that build it)."""
    cls = jar.get(registry)
    descriptor = None
    order = []
    built = {}
    calls = set()
    code = cls.method("<clinit>").code()
    for index, (opcode, operand, _) in enumerate(code):
        if opcode == INVOKESTATIC and operand[0] != registry:
            calls |= {f"{simple(names.cls(operand[0]))}.{method}"
                      for method in names.method(operand[0], operand[1])}
        elif opcode == GETSTATIC and operand[0] != registry:
            # A stream codec kept elsewhere (`WolfVariant.STREAM_CODEC`).
            calls |= field_calls(jar, operand[0], operand[1], names)
        elif opcode == PUTSTATIC and operand[0] == registry:
            built[operand[1]] = calls
            calls = set()
        if (opcode == INVOKESTATIC and operand[0] == registry and index >= 1
                and code[index - 1][0] == GETSTATIC and code[index - 1][1][0] == registry
                and operand[2].startswith("(L") and operand[2].endswith(";)V")):
            if descriptor not in (None, operand[1:]):
                raise SystemExit(f"{registry} registers serializers two ways")
            descriptor = operand[1:]
            order.append(code[index - 1][1][1])
    fields = {field.name: field for field in cls.fields}
    result = []
    for name in order:
        match = re.fullmatch(r"L[^<]+<(.*)>;", fields[name].signature or "")
        if match is None:
            raise SystemExit(f"serializer {name} has no value type")
        result.append((name, deobfuscate(match.group(1), names), built.get(name, set())))
    return result


def field_calls(jar, owner, field, names):
    """The static methods a class's static initialiser calls to build one
    of its fields."""
    cls = jar.get(owner)
    initialiser = cls and cls.method("<clinit>")
    calls = set()
    for opcode, operand, _ in initialiser.code() if initialiser else []:
        if opcode == INVOKESTATIC:
            calls |= {f"{simple(names.cls(operand[0]))}.{method}"
                      for method in names.method(operand[0], operand[1])}
        elif opcode == PUTSTATIC and operand[0] == owner:
            if operand[1] == field:
                return calls
            calls = set()
    return set()


def deobfuscate(signature, names):
    return re.sub(r"L([^<;]+)", lambda match: "L" + names.cls(match.group(1)), signature)


# Serializer kinds by value type, for jars with names. A Holder that can be
# sent inline (`ByteBufCodecs.holder`) is a different encoding from a plain
# registry ID and gets its own kind.
VALUE_KINDS = {
    "Byte": "byte", "Integer": "int", "Long": "long", "Float": "float", "String": "string",
    "Component": "component", "Optional<Component>": "optional_component",
    "ItemStack": "item_stack", "Boolean": "boolean", "Rotations": "rotations",
    "BlockPos": "block_pos", "Optional<BlockPos>": "optional_block_pos",
    "Direction": "direction", "Optional<UUID>": "optional_uuid",
    "Optional<EntityReference<LivingEntity>>": "optional_uuid",
    "BlockState": "block_state", "Optional<BlockState>": "optional_block_state",
    "CompoundTag": "compound_tag", "ParticleOptions": "particle",
    "List<ParticleOptions>": "particles", "VillagerData": "villager_data",
    "OptionalInt": "optional_unsigned_int", "Pose": "pose",
    "Optional<GlobalPos>": "optional_global_pos", "State": "sniffer_state",
    "ArmadilloState": "armadillo_state", "CopperGolemState": "copper_golem_state",
    "WeatherState": "weathering_copper_state", "Vector3f": "vector3", "Vector3fc": "vector3",
    "Quaternionf": "quaternion", "Quaternionfc": "quaternion",
    "ResolvableProfile": "resolvable_profile", "HumanoidArm": "humanoid_arm",
    "DyeColor": "dye_color",
}

# 1.12 to 1.14.3 have no mappings: their serializers by ID, checked against
# the Java value type of each where it is not obfuscated.
SERIALIZER_KINDS = {
    335: ["byte", "int", "float", "string", "component", "item_stack", "boolean", "rotations",
          "block_pos", "optional_block_pos", "direction", "optional_uuid",
          "optional_block_state", "compound_tag"],
    393: ["byte", "int", "float", "string", "component", "optional_component", "item_stack",
          "boolean", "rotations", "block_pos", "optional_block_pos", "direction",
          "optional_uuid", "optional_block_state", "compound_tag", "particle"],
    477: ["byte", "int", "float", "string", "component", "optional_component", "item_stack",
          "boolean", "rotations", "block_pos", "optional_block_pos", "direction",
          "optional_uuid", "optional_block_state", "compound_tag", "particle",
          "villager_data", "optional_unsigned_int", "pose"],
}
JAVA_VALUES = {"byte": "Byte", "int": "Integer", "float": "Float", "string": "String",
               "boolean": "Boolean", "optional_uuid": "Optional<UUID>",
               "optional_unsigned_int": "OptionalInt"}


def plain(value_type):
    """`Ljava/util/Optional<Lnet/minecraft/core/BlockPos;>;` -> `Optional<BlockPos>`."""
    return re.sub(r"L([^<;]+);?", lambda match: simple(match.group(1)), value_type).replace(";", "")


def snake(name):
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def classify(protocol, entries):
    """Serializer kinds in ID order."""
    if protocol < 498:
        kinds = SERIALIZER_KINDS[max(key for key in SERIALIZER_KINDS if key <= protocol)]
        if len(kinds) != len(entries):
            raise SystemExit(f"{protocol}: {len(entries)} serializers, expected {len(kinds)}")
        for kind, (_, value, _) in zip(kinds, entries):
            value = plain(value).replace("com/google/common/base/", "")
            expected = JAVA_VALUES.get(kind)
            if expected and value != expected or kind.startswith("optional_") and not (
                    value.startswith("Optional") or kind == "optional_unsigned_int"):
                raise SystemExit(f"{protocol}: serializer {kind} has value type {value}")
        return kinds
    kinds = []
    for _, value, calls in entries:
        value = plain(value)
        holder = re.fullmatch(r"Holder<(\w+)>", value)
        if holder:
            kind = snake(holder.group(1))
            if "ByteBufCodecs.holder" in calls:
                kind += "_holder"
        elif value.endswith("Variant"):
            kind = snake(value)
        elif value in VALUE_KINDS:
            kind = VALUE_KINDS[value]
        else:
            raise SystemExit(f"{protocol}: unknown serializer value type {value}")
        kinds.append(kind)
    return kinds


def enum_constants(jar, name):
    """An enum's constant names in ordinal order, from its static
    initialiser: each constant is constructed with its name."""
    cls = jar.get(name)
    if cls is None or not cls.access & 0x4000:
        return None
    constants = {field.name for field in cls.fields if field.access & 0x4000}
    result = []
    label = None
    for opcode, operand, _ in cls.method("<clinit>").code():
        if opcode in LDC and operand[0] == "string" and label is None:
            label = operand[1]
        elif opcode == PUTSTATIC and operand[0] == name and operand[1] in constants:
            result.append(label)
            label = None
        elif opcode == 0xbb and operand == name:
            label = None
    if len(result) != len(constants) or None in result:
        raise SystemExit(f"cannot read the constants of enum {name}")
    return result


def defined_fields(jar, define):
    """Every class's entity data accessors in definition order: class ->
    [(accessor field, serializer field)]."""
    result = {}
    for cls in jar.grep(define[2].encode()):
        initialiser = cls.method("<clinit>")
        if initialiser is None:
            continue
        code = initialiser.code()
        # Accessors stored into an array (boat paddles) are named after the
        # array field and their position in it.
        pending = []
        for index, (opcode, operand, _) in enumerate(code):
            if opcode == PUTSTATIC and operand[2].startswith("[") and pending:
                for position, serializer in enumerate(pending):
                    result.setdefault(cls.name, []).append((f"{operand[1]}[{position}]", serializer))
                pending = []
            if opcode != INVOKESTATIC or operand != define:
                continue
            if not (index >= 2 and code[index - 1][0] == GETSTATIC
                    and code[index - 2][0] in LDC and code[index - 2][1][0] == "class"
                    and index + 1 < len(code) and code[index + 1][0] in (PUTSTATIC, AASTORE)):
                raise SystemExit(f"unexpected defineId call in {cls.name}")
            owner = code[index - 2][1][1]
            if owner != cls.name:
                raise SystemExit(f"{cls.name} defines entity data for {owner}")
            if code[index + 1][0] == AASTORE:
                pending.append(code[index - 1][1][:2])
            else:
                result.setdefault(owner, []).append((code[index + 1][1][1], code[index - 1][1][:2]))
        if pending:
            raise SystemExit(f"unnamed entity data accessors in {cls.name}")
    return result


def entity_types(jar, protocol):
    """Entity type network name -> class."""
    types = {}
    if protocol < 393:
        # EntityList: register(int id, String name, Class class, String oldName).
        for cls in jar.grep(b"(ILjava/lang/String;Ljava/lang/Class;Ljava/lang/String;)V"):
            for method in cls.methods:
                code = method.code()
                for index, (opcode, operand, _) in enumerate(code):
                    if (opcode == INVOKESTATIC and operand[2]
                            == "(ILjava/lang/String;Ljava/lang/Class;Ljava/lang/String;)V"):
                        # The old name may be computed (minecarts), so look
                        # back for the class constant and the name before it.
                        for back in range(index - 1, max(index - 6, 0), -1):
                            if code[back][0] in LDC and code[back][1][0] == "class":
                                before = code[back - 1]
                                if before[0] in LDC and before[1][0] == "string":
                                    types[before[1][1]] = code[back][1][1]
                                break
        return types
    # EntityType's static fields: `EntityType<Cow> COW = register("cow", ...)`,
    # since 26.1 in EntityTypes with the name from a ResourceKey constant.
    for cls in jar.grep(b"area_effect_cloud", b"EntityType$Builder"):
        typed = {}
        for field in cls.fields:
            match = re.fullmatch(r"L([^<;]+)<L([^<;]+);>;", field.signature or "")
            if field.is_static and match and field.descriptor == f"L{match.group(1)};":
                typed.setdefault(match.group(1), {})[field.name] = match.group(2)
        holder = max(typed, key=lambda name: len(typed[name]), default=None)
        if holder is None or len(typed[holder]) < 50:
            continue
        keys = {}
        name = None
        for opcode, operand, _ in cls.method("<clinit>").code():
            if opcode in LDC and operand[0] == "string" and name is None:
                name = operand[1]
            elif opcode == GETSTATIC and operand[0] != cls.name and name is None:
                if operand[0] not in keys:
                    keys[operand[0]] = resource_keys(jar, operand[0])
                name = keys[operand[0]].get(operand[1])
            elif opcode == PUTSTATIC and operand[0] == cls.name:
                if operand[1] in typed[holder]:
                    if name is None:
                        raise SystemExit(f"cannot name entity type field {operand[1]}")
                    types[name] = typed[holder][operand[1]]
                name = None
        if "creeper" in types and "area_effect_cloud" in types:
            return types
        types = {}
    raise SystemExit("no entity type registrations found")


def resource_keys(jar, owner):
    """The names a class's static ResourceKey constants are created from."""
    cls = jar.get(owner)
    initialiser = cls and cls.method("<clinit>")
    result = {}
    name = None
    for opcode, operand, _ in initialiser.code() if initialiser else []:
        if opcode in LDC and operand[0] == "string":
            name = operand[1]
        elif opcode == PUTSTATIC and operand[0] == owner and name is not None:
            result[operand[1]] = name
            name = None
    return result


def extract(version, protocol):
    """One release's entity data as read from its jar, with Mojang's names
    where it has them:

        {"serializers": [kind, ...],
         "enums": {kind: [constant, ...]},
         "classes": {class: {"parent": class | None, "fields": [[name, kind], ...]}},
         "types": {type: class}}

    Only classes that define entity data are listed; a class's parent is its
    nearest ancestor that does. Cached per jar checksum.
    """
    path = server(version)
    cache = CACHE / "entities" / f"{version}.json"
    digest = hashlib.sha1(path.read_bytes()).hexdigest()
    if cache.exists():
        cached = json.loads(cache.read_text())
        if cached.get("jar") == digest and cached.get("format") == CACHE_FORMAT:
            return cached["model"]
    jar = Jar(path)
    names = Names(mappings_text(version) if protocol >= 498 else None)
    define = find_define(jar)
    fields = defined_fields(jar, define)
    owners = {serializer[0] for accessors in fields.values() for _, serializer in accessors}
    if len(owners) != 1:
        raise SystemExit(f"{version}: serializers come from {owners}")
    owner = owners.pop()
    entries = serializers(jar, owner, names)
    kinds = classify(protocol, entries)
    by_field = {name: kind for (name, _, _), kind in zip(entries, kinds)}
    enums = {}
    raw_entries = {field.name: field.signature for field in jar.get(owner).fields}
    for (name, _, _), kind in zip(entries, kinds):
        value = re.fullmatch(r"L[^<]+<L([^<;]+);>;", raw_entries[name] or "")
        constants = value and enum_constants(jar, value.group(1))
        if constants:
            enums[kind] = constants

    def named(cls):
        return simple(names.cls(cls))

    def nearest(cls):
        """The nearest ancestor of `cls` (itself included) defining entity data."""
        while cls is not None and cls not in fields:
            parent = jar.get(cls)
            cls = parent.parent if parent else None
        return cls

    registered = entity_types(jar, protocol)
    if "player" not in registered:
        # Before 1.14 the player is not a registered entity type; it is the
        # one class whose entity data holds compound tags (shoulder parrots).
        players = [cls for cls, accessors in fields.items()
                   if sum(by_field[serializer[1]] == "compound_tag"
                          for _, serializer in accessors) >= 2]
        if len(players) != 1:
            raise SystemExit(f"{version}: cannot find the player class")
        registered["player"] = players[0]
    types = {}
    classes = {}
    for type_name, cls in registered.items():
        leaf = nearest(cls)
        if leaf is None:
            raise SystemExit(f"{version}: entity type {type_name} has no entity data")
        types[type_name] = named(leaf)
        current = leaf
        while current is not None and named(current) not in classes:
            parent = nearest(jar.get(current).parent)
            classes[named(current)] = {
                "parent": named(parent) if parent else None,
                "fields": [[names.field(current, accessor.split("[")[0])
                            + (accessor[accessor.index("["):] if "[" in accessor else ""),
                            by_field[serializer[1]]]
                           for accessor, serializer in fields[current]],
            }
            current = parent
    model = {"serializers": kinds, "enums": enums, "classes": classes, "types": types,
             "particles": particles(jar, protocol, names)}
    cache.parent.mkdir(parents=True, exist_ok=True)
    cache.write_text(json.dumps({"jar": digest, "format": CACHE_FORMAT, "model": model}))
    return model


CACHE_FORMAT = 4


# Particles with options before 1.14.4, whose jars have no names; the same
# option types as 1.14.4's.
LEGACY_PARTICLES = {"block": "BlockParticleOption", "falling_dust": "BlockParticleOption",
                    "dust": "DustParticleOptions", "item": "ItemParticleOption"}


def particles(jar, protocol, names):
    """Particle name -> options type, for the particles that have options."""
    if protocol < 393:
        return {}
    if protocol < 498:
        return dict(LEGACY_PARTICLES)
    for cls in jar.grep(b"angry_villager"):
        initialiser = cls.method("<clinit>")
        if initialiser is None:
            continue
        code = initialiser.code()
        if not any(opcode in LDC and operand == ("string", "angry_villager")
                   for opcode, operand, _ in code):
            continue
        descriptors = {}
        for field in cls.fields:
            descriptors[field.descriptor] = descriptors.get(field.descriptor, 0) + 1
        # Particle types are the static fields of the two most common types
        # (simple and with options); others are codecs and the like.
        fields = {field.name: field for field in cls.fields
                  if field.is_static and descriptors[field.descriptor] >= 3}
        result = {}
        name = None
        count = 0
        for opcode, operand, _ in code:
            if opcode in LDC and operand[0] == "string" and name is None:
                name = operand[1]
            elif opcode == PUTSTATIC and operand[0] == cls.name and operand[1] in fields:
                field = fields[operand[1]]
                options = re.fullmatch(r"L[^<;]+<L([^<;]+);>;", field.signature or "")
                if name is None:
                    raise SystemExit(f"cannot name particle field {operand[1]}")
                if options:
                    result[name] = simple(names.cls(options.group(1)))
                count += 1
                name = None
        if count > 40:
            return result
    raise SystemExit("no particle registrations found")


# Keys.

def chain(model, cls):
    """`cls` and its ancestors, root first."""
    result = []
    while cls is not None:
        result.append(cls)
        cls = model["classes"][cls]["parent"]
    return result[::-1]


def lcs(a, b, same):
    """Index pairs of a longest common subsequence of `a` and `b`."""
    rows = [[0] * (len(b) + 1) for _ in range(len(a) + 1)]
    for i in range(len(a) - 1, -1, -1):
        for j in range(len(b) - 1, -1, -1):
            rows[i][j] = (rows[i + 1][j + 1] + 1 if same(a[i], b[j])
                          else max(rows[i + 1][j], rows[i][j + 1]))
    pairs = []
    i = j = 0
    while i < len(a) and j < len(b):
        if same(a[i], b[j]) and rows[i][j] == rows[i + 1][j + 1] + 1:
            pairs.append((i, j))
            i += 1
            j += 1
        elif rows[i + 1][j] >= rows[i][j + 1]:
            i += 1
        else:
            j += 1
    return pairs


def similarity(older, newer):
    """How alike two field lists are, by serializer kind."""
    kinds = lambda fields: [kind for _, kind in fields]
    return len(lcs(kinds(older), kinds(newer), lambda x, y: x == y)) / max(
        len(older), len(newer), 1)


def align_classes(older, newer, named):
    """Older class -> newer class. With names on both sides classes match by
    name; the rest (all of them without names) by their position in the
    hierarchies of the entity types the two versions share."""
    result = {}
    if named:
        result = {cls: cls for cls in older["classes"] if cls in newer["classes"]}
    votes = {}
    renames = {old: new for new, old in TYPE_RENAMES.get(newer["protocol"], {}).items()}
    for type_name, leaf in older["types"].items():
        newer_name = renames.get(type_name, type_name)
        if newer_name not in newer["types"]:
            continue
        a, b = chain(older, leaf), chain(newer, newer["types"][newer_name])

        def same(x, y):
            if x in result:
                return result[x] == y
            if y in result.values():
                return False
            return similarity(older["classes"][x]["fields"], newer["classes"][y]["fields"]) >= 0.5

        for i, j in lcs(a, b, same):
            if a[i] not in result:
                votes.setdefault(a[i], {}).setdefault(b[j], 0)
                votes[a[i]][b[j]] += 1
    for cls, candidates in votes.items():
        best = max(candidates, key=lambda candidate: (candidates[candidate], candidate))
        if best not in result.values():
            result[cls] = best
    return result


def align_fields(older, newer, named):
    """Older field index -> newer field index within two matching classes."""
    if named:
        names = {name: index for index, (name, _) in enumerate(newer)}
        return {index: names[name] for index, (name, _) in enumerate(older) if name in names}
    return dict(lcs(older, newer, lambda x, y: x[1] == y[1]))


def assign_keys(models):
    """Give every class and field a key that stays the same across versions:
    the newest version's name, carried back to older versions through the
    alignments above and ALIGN. Returns {protocol: {class: (class key,
    [field key, ...])}}."""
    protocols = sorted(models)
    keys = {}
    newest = models[protocols[-1]]
    keys[protocols[-1]] = {cls: (cls, [f"{cls}.{name}" for name, _ in data["fields"]])
                           for cls, data in newest["classes"].items()}
    for older_protocol, newer_protocol in reversed(list(zip(protocols, protocols[1:]))):
        older, newer = models[older_protocol], models[newer_protocol]
        named = older_protocol >= 498
        overrides = ALIGN.get(older_protocol, {})
        classes = align_classes(older, newer, named)
        named_classes = {}

        def resolve(reference):
            """An override's older class: a name, `@type` for the class of an
            entity type, `~Class` for the class aligned to a newer class."""
            if reference.startswith("@"):
                return older["types"][reference[1:]]
            if reference.startswith("~"):
                matches = [cls for cls, target in classes.items()
                           if keys[newer_protocol][target][0] == reference[1:]]
                if len(matches) != 1:
                    raise SystemExit(f"{older_protocol}: no class aligns to {reference[1:]}")
                return matches[0]
            if reference not in older["classes"]:
                raise SystemExit(f"{older_protocol}: no class {reference}")
            return reference

        for reference, (name, fields) in overrides.get("names", {}).items():
            # A class the newer version dropped, named for the data files.
            cls = resolve(reference)
            named_classes[cls] = (name, [f"{name}.{field}" for field in fields])
        for reference, target in overrides.get("classes", {}).items():
            match = [cls for cls, (key, _) in keys[newer_protocol].items() if key == target]
            classes[resolve(reference)] = match[0]
        moved = {}
        newer_keys = {key for _, field_keys in keys[newer_protocol].values() for key in field_keys}
        for reference, fields in overrides.get("fields", {}).items():
            cls = resolve(reference)
            names = [name for name, _ in older["classes"][cls]["fields"]]
            for field, target in fields.items():
                index = names.index(field) if isinstance(field, str) else field
                if target is not None and target not in newer_keys:
                    raise SystemExit(f"{older_protocol}: override target {target} does not exist")
                moved[(cls, index)] = target
        result = {}
        for cls, data in older["classes"].items():
            target = classes.get(cls)
            if cls in named_classes:
                result[cls] = named_classes[cls]
                continue
            if target is None:
                class_key = cls if named else f"{simple_version(older_protocol)}:{cls}"
                aligned, target_keys = {}, []
            else:
                class_key, target_keys = keys[newer_protocol][target]
                aligned = align_fields(data["fields"], newer["classes"][target]["fields"], named)
            field_keys = []
            for index, (name, _) in enumerate(data["fields"]):
                if (cls, index) in moved:
                    key = moved[(cls, index)]
                elif aligned.get(index) is not None:
                    key = target_keys[aligned[index]]
                else:
                    key = None
                if key is None:
                    prefix = "" if named or target is None else f"{simple_version(older_protocol)}:"
                    key = f"{class_key}.{prefix}{name}"
                field_keys.append(key)
            result[cls] = (class_key, field_keys)
        keys[older_protocol] = result
    return keys


def simple_version(protocol):
    return dict(PROTOCOLS)[protocol]


# Entity types Mojang renamed, by the protocol that renamed them: new name ->
# old name. Also written to the data files for steps whose entity type
# mapping lacks them.
TYPE_RENAMES = {
    393: {"end_crystal": "ender_crystal", "evoker_fangs": "evocation_fangs",
          "evoker": "evocation_illager", "eye_of_ender": "eye_of_ender_signal",
          "firework_rocket": "fireworks_rocket", "illusioner": "illusion_illager",
          "snow_golem": "snowman", "iron_golem": "villager_golem",
          "vindicator": "vindication_illager", "experience_bottle": "xp_bottle",
          "experience_orb": "xp_orb", "command_block_minecart": "commandblock_minecart"},
    735: {"zombified_piglin": "zombie_pigman"},
    768: {"oak_boat": "boat", "oak_chest_boat": "chest_boat"},
    770: {"splash_potion": "potion"},
}


# Corrections to the automatic alignment, per older protocol of a pair. An
# older class is named as in that version, as `@type` (the class of an entity
# type) or as `~Key` (the class aligned to the newer class with that key).
# "classes" maps older classes to newer class keys; "fields" maps an older
# class's fields (by name, or by index without names) to the newer field key
# that is the same field, or None when none is. Facts from the protocol
# history of each version, checked against the jars by the report.
ALIGN = {
    # 1.13: custom names became optional text components.
    340: {"fields": {"~Entity": {2: "Entity.DATA_CUSTOM_NAME"}}},
    # 1.14: zombies lost "hands held up"; fireworks track their shooter as an
    # optional ID; villager professions became villager data.
    404: {
        "classes": {"@firework_rocket": "FireworkRocketEntity", "@villager": "Villager",
                    "@ocelot": "Ocelot"},
        "names": {"@vindicator": ("AbstractIllager", ["DATA_FLAGS_ID"]),
                  "@skeleton": ("AbstractSkeleton", ["DATA_SWINGING_ARMS"])},
        "fields": {
            "~Zombie": {2: None, 3: "Zombie.DATA_DROWNED_CONVERSION_ID"},
            "@firework_rocket": {0: "FireworkRocketEntity.DATA_ID_FIREWORKS_ITEM",
                                 1: "FireworkRocketEntity.DATA_ATTACHED_TO_TARGET"},
            "@villager": {0: "Villager.DATA_VILLAGER_DATA"},
            "@ocelot": {0: None},
            "@zombie_villager": {1: "ZombieVillager.DATA_VILLAGER_DATA"},
        },
    },
    # 1.16: renames, and potions became item projectiles.
    578: {"fields": {
        "Bee": {"ANGER_TIME": "Bee.DATA_REMAINING_ANGER_TIME"},
        "Dolphin": {"MOISNTESS_LEVEL": "Dolphin.MOISTNESS_LEVEL"},
        "ThrownPotion": {"DATA_ITEM_STACK": "ThrowableItemProjectile.DATA_ITEM_STACK"},
    }},
    # 1.16.2: AbstractPiglin took the zombification flag from Piglin.
    736: {"fields": {"Piglin": {
        "DATA_IMMUNE_TO_ZOMBIFICATION": "AbstractPiglin.DATA_IMMUNE_TO_ZOMBIFICATION"}}},
    # 1.19: cat types became the cat_variant registry, in the same order.
    758: {"fields": {"Cat": {"DATA_TYPE_ID": "Cat.DATA_VARIANT_ID"}}},
    # 1.20.2: display interpolation split into transformation and movement.
    763: {"fields": {"Display": {
        "DATA_INTERPOLATION_START_DELTA_TICKS_ID":
            "Display.DATA_TRANSFORMATION_INTERPOLATION_START_DELTA_TICKS_ID",
        "DATA_INTERPOLATION_DURATION_ID": "Display.DATA_TRANSFORMATION_INTERPOLATION_DURATION_ID",
    }}},
    # 1.20.3: boats and minecarts share their damage fields in VehicleEntity.
    764: {"fields": {
        "Boat": {"DATA_ID_HURT": "VehicleEntity.DATA_ID_HURT",
                 "DATA_ID_HURTDIR": "VehicleEntity.DATA_ID_HURTDIR",
                 "DATA_ID_DAMAGE": "VehicleEntity.DATA_ID_DAMAGE"},
        "AbstractMinecart": {"DATA_ID_HURT": "VehicleEntity.DATA_ID_HURT",
                             "DATA_ID_HURTDIR": "VehicleEntity.DATA_ID_HURTDIR",
                             "DATA_ID_DAMAGE": "VehicleEntity.DATA_ID_DAMAGE"},
    }},
    # 1.20.5: the effect colour became a list of effect particles.
    765: {"fields": {"LivingEntity": {"DATA_EFFECT_COLOR_ID": "LivingEntity.DATA_EFFECT_PARTICLES"}}},
    # 1.21.5: a minecart's display block became an optional block state.
    769: {"fields": {"AbstractMinecart": {
        "DATA_ID_DISPLAY_BLOCK": "AbstractMinecart.DATA_ID_CUSTOM_DISPLAY_BLOCK"}}},
    # 1.21.9: mannequins share the player's skin fields in Avatar.
    772: {"fields": {"Player": {
        "DATA_PLAYER_MODE_CUSTOMISATION": "Avatar.DATA_PLAYER_MODE_CUSTOMISATION",
        "DATA_PLAYER_MAIN_HAND": "Avatar.DATA_PLAYER_MAIN_HAND"}}},
}


# minecraft-data, which the crate's 1.13 registries come from, names two
# entity types differently from the 1.13 jars; the data files follow the
# registry so that names resolve.
REGISTRY_ALIASES = {"command_block_minecart": "commandblock_minecart",
                    "firework_rocket": "fireworks_rocket"}


# Readable keys for fields that have no newer counterpart and no Mojang name.
LEGACY_KEYS = {
    "AreaEffectCloud.1.12.2:d": "AreaEffectCloud.LEGACY_PARTICLE_ID",
    "AreaEffectCloud.1.12.2:e": "AreaEffectCloud.LEGACY_PARTICLE_PARAM_1",
    "AreaEffectCloud.1.12.2:f": "AreaEffectCloud.LEGACY_PARTICLE_PARAM_2",
    "Horse.1.13.2:bO": "Horse.LEGACY_ARMOR_TYPE",
    "Ocelot.1.13.2:bH": "Ocelot.LEGACY_TYPE_ID",
    "Zombie.1.13.2:bE": "Zombie.LEGACY_HANDS_UP",
}


def outputs(models, keys):
    """data/entities/<protocol>.json for every protocol."""
    files = {}
    for protocol, model in models.items():
        registry = json.loads((CRATE / "data" / f"{protocol}.json").read_text())["registries"]
        names = {name for name in registry["entity_type"] if name}
        types = {}
        for type_name, cls in model["types"].items():
            if type_name not in names and REGISTRY_ALIASES.get(type_name) in names:
                type_name = REGISTRY_ALIASES[type_name]
            types[type_name] = keys[protocol][cls][0]
        missing = names - set(types)
        unknown = set(types) - names - {"player"}
        if missing or unknown:
            raise SystemExit(f"{protocol}: entity types differ from the registry: "
                             f"missing {sorted(missing)}, unknown {sorted(unknown)}")
        classes = {}
        for cls, data in model["classes"].items():
            class_key, field_keys = keys[protocol][cls]
            if class_key in classes:
                raise SystemExit(f"{protocol}: two classes have the key {class_key}")
            parent = data["parent"]
            classes[class_key] = {
                "parent": keys[protocol][parent][0] if parent else None,
                "fields": [[LEGACY_KEYS.get(key, key), kind]
                           for key, (_, kind) in zip(field_keys, data["fields"])],
            }
            if ":" in "".join(field_keys).replace("1.12.2:", "").replace("1.13.2:", "") or any(
                    ":" in key and key not in LEGACY_KEYS for key in field_keys):
                raise SystemExit(f"{protocol}: {class_key} has a field without a readable key")
            if len(set(field_keys)) != len(field_keys):
                raise SystemExit(f"{protocol}: {class_key} repeats a field key")
        document = {
            "protocol": protocol,
            "serializers": model["serializers"],
            "enums": model["enums"],
            "classes": dict(sorted(classes.items())),
            "types": dict(sorted(types.items())),
            "particles": dict(sorted(model["particles"].items())),
            "renamed": TYPE_RENAMES.get(protocol, {}),
        }
        files[f"{protocol}.json"] = json.dumps(document, indent=1) + "\n"
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--check", action="store_true", help="fail if data/entities is stale")
    parser.add_argument("--report", action="store_true", help="print how versions align")
    parser.add_argument("--validate", metavar="DUMP",
                        help="decode converted packets dumped by the entity codec's tests "
                             "(RIFT_ENTITY_DUMP) with the minecraft-data schemas")
    args = parser.parse_args()
    if args.validate:
        validate(Path(args.validate))
        return
    with ProcessPoolExecutor(max_workers=os.cpu_count()) as pool:
        models = dict(zip([protocol for protocol, _ in PROTOCOLS], pool.map(
            extract, [version for _, version in PROTOCOLS], [protocol for protocol, _ in PROTOCOLS])))
    for protocol, model in models.items():
        model["protocol"] = protocol
    keys = assign_keys(models)
    if args.report:
        report(models, keys)
        return
    files = outputs(models, keys)
    existing = {path.name for path in OUTPUT.glob("*.json")} if OUTPUT.exists() else set()
    stale = sorted(name for name, text in files.items()
                   if not (OUTPUT / name).exists() or (OUTPUT / name).read_text() != text)
    unknown = sorted(existing - set(files))
    if args.check:
        if stale or unknown:
            raise SystemExit(f"stale entity data: {', '.join(stale + unknown)}; "
                             "run scripts/translation_entities.py")
        return
    OUTPUT.mkdir(parents=True, exist_ok=True)
    for name in stale:
        (OUTPUT / name).write_text(files[name])
        log(f"wrote {OUTPUT / name}")
    for name in unknown:
        (OUTPUT / name).unlink()
        log(f"removed {OUTPUT / name}")


def validate(dump):
    """Decode every dumped packet with the minecraft-data (MIT) schema of
    its protocol, through tests/protodef.py, an implementation independent
    of the crate's. minecraft-data has no schema for some versions and is
    wrong in places (see protodef.CORRECTIONS), so this reports rather than
    judges: failures per version and packet, with one example each."""
    sys.path.insert(0, str(ROOT / "tests"))
    import protodef
    checkout = CACHE / "ref" / "minecraft-data" / "data"
    paths = json.loads((checkout / "dataPaths.json").read_text())["pc"]
    releases = dict(PROTOCOLS)
    decoders = {}
    results = {}
    for line in dump.read_text().splitlines():
        packet = json.loads(line)
        protocol = packet["protocol"]
        if protocol not in decoders:
            path = paths.get(releases[protocol], {}).get("protocol")
            schema = json.loads((checkout / path / "protocol.json").read_text()) if path else None
            decoders[protocol] = schema and protodef.Decoder(schema, "play", "clientbound")
        decoder = decoders[protocol]
        key = (releases[protocol], packet["name"])
        entry = results.setdefault(key, [0, 0, None])
        if decoder is None:
            continue
        try:
            decoder.packet(packet["id"], bytes.fromhex(packet["hex"]))
            entry[0] += 1
        except Exception as error:  # A schema gap is as likely as a bug.
            entry[1] += 1
            entry[2] = entry[2] or f"{error} (from {packet['from']}: {packet['hex'][:120]})"
    unchecked = sorted({version for (version, _), (ok, bad, _) in results.items() if ok == bad == 0})
    print(f"no minecraft-data schema: {', '.join(unchecked) or 'none'}")
    for (version, name), (ok, bad, example) in sorted(results.items()):
        if ok or bad:
            print(f"{version:8} {name:20} decoded {ok:5}  failed {bad:5}"
                  + (f"  e.g. {example}" if example else ""))


def report(models, keys):
    """Per pair: what changed per entity type, by key."""
    protocols = sorted(models)
    for older, newer in zip(protocols, protocols[1:]):
        print(f"== {simple_version(older)} -> {simple_version(newer)}")
        def flat(protocol, type_name):
            model = models[protocol]
            out = []
            for cls in chain(model, model["types"][type_name]):
                class_key, field_keys = keys[protocol][cls]
                out += [(key, kind) for key, (_, kind) in zip(field_keys, model["classes"][cls]["fields"])]
            return out
        changes = {}
        for type_name in sorted(set(models[older]["types"]) & set(models[newer]["types"])):
            a, b = flat(older, type_name), flat(newer, type_name)
            if a == b:
                continue
            akeys = dict(a)
            bkeys = dict(b)
            change = tuple([f"-{key}:{kind}" for key, kind in a if key not in bkeys]
                           + [f"+{key}:{kind}" for key, kind in b if key not in akeys]
                           + [f"~{key}:{akeys[key]}->{kind}" for key, kind in b
                              if key in akeys and akeys[key] != kind]
                           + (["(reordered)"] if [k for k, _ in a if k in bkeys]
                              != [k for k, _ in b if k in akeys] else []))
            changes.setdefault(change, []).append(type_name)
        for change, types in changes.items():
            print(f"  {', '.join(change) or '(index shift only)'}\n      {' '.join(types[:12])}{' ...' if len(types) > 12 else ''}")
        gone = set(models[older]["types"]) - set(models[newer]["types"])
        new = set(models[newer]["types"]) - set(models[older]["types"])
        if gone or new:
            print(f"  types -{sorted(gone)} +{sorted(new)}")


if __name__ == "__main__":
    main()
