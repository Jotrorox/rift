#!/usr/bin/env python3
"""Generate crates/rift-translation/data from Mojang's own server jars.

Every input is pinned: the vanilla server jars by the checksums in
tests/servers.json, PrismarineJS minecraft-data (MIT) and ViaVersion by git
commit (files are read from that commit, whatever the checkout's HEAD is).
Inputs and the server's data generator output are cached under
~/.cache/rift-via (override with RIFT_VIA_CACHE) so other worktrees reuse them.

Packet tables (names in packet-ID order per state and direction) come from
the server's reports/packets.json from 1.21 on, and before that from
ViaVersion's packet-type enums, which use Mojang's names; both are compared
where both exist. Where Mojang renamed a packet that stayed the same thing,
every version uses the newest name (PACKET_RENAMES). The tables are also
checked against minecraft-data's protocol.json and against the packet IDs
Rift's proxy hard-codes in src/protocol.

Usage (Python 3.11+, stdlib only):

    python3 scripts/translation_crate.py            # regenerate data/
    python3 scripts/translation_crate.py --check    # fail if data/ is stale
    python3 scripts/translation_crate.py --reports  # only fill the cache

Each server's data generator runs with the Java release servers.json names,
found as ~/.cache/rift-via/jdks/jdk<N>/bin/java (or $RIFT_JDK_<N>).
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.request
import zipfile

ROOT = Path(__file__).resolve().parents[1]
CRATE = ROOT / "crates" / "rift-translation"
OUTPUT = CRATE / "data"
CACHE = Path(os.environ.get("RIFT_VIA_CACHE", Path.home() / ".cache" / "rift-via"))
SERVERS = json.loads((ROOT / "tests" / "servers.json").read_text())

# Every protocol the crate translates with its newest release, as listed in
# the crate's version module.
PROTOCOLS = [(int(protocol), name) for protocol, name in re.findall(
    r'\((\d+), "([^"]+)"\)', (CRATE / "src" / "version.rs").read_text().split("#[cfg(test)]")[0])]


# Files under data/ that people write: rules/ holds each step's rename and
# stand-in rules.
HAND_WRITTEN = {"rules"}


def log(message):
    print(message, file=sys.stderr, flush=True)


def server(version):
    """The pinned vanilla server fixture of a release."""
    matches = [fixture for key, fixture in SERVERS.items()
               if key.startswith("vanilla") and fixture["version"] == version]
    if len(matches) != 1:
        raise ValueError(f"tests/servers.json has no single vanilla {version} server")
    return matches[0]


def verify(path, fixture):
    with path.open("rb") as source:
        actual = hashlib.file_digest(source, fixture["algorithm"]).hexdigest()
    if actual != fixture["checksum"]:
        raise ValueError(f"checksum mismatch: {path}: {actual}")


def jar(version):
    fixture = server(version)
    directory = CACHE / "servers"
    directory.mkdir(parents=True, exist_ok=True)
    artifact = directory / fixture["filename"]
    if not artifact.exists():
        log(f"Downloading {version}...")
        request = urllib.request.Request(fixture["url"], headers={
            "User-Agent": "rift-translation-data/0.1.0 (https://github.com/Jotrorox/rift)"})
        with tempfile.TemporaryDirectory(dir=directory) as temporary:
            pending = Path(temporary) / "download"
            with urllib.request.urlopen(request, timeout=300) as response:
                pending.write_bytes(response.read())
            verify(pending, fixture)
            pending.replace(artifact)
    verify(artifact, fixture)
    return artifact


def java(release):
    override = os.environ.get(f"RIFT_JDK_{release}")
    path = Path(override) if override else CACHE / "jdks" / f"jdk{release}" / "bin" / "java"
    if not path.exists():
        raise SystemExit(f"Java {release} not found at {path}; set RIFT_JDK_{release}")
    return str(path)


def inner_jar(archive):
    """The game jar itself: since 1.18 the server jar is a bundler around it."""
    names = archive.namelist()
    nested = [name for name in names
              if name.startswith("META-INF/versions/") and name.endswith(".jar")]
    if nested:
        return zipfile.ZipFile(archive.open(nested[0]))
    return archive


def reports(version):
    """The cache directory of a release's data generator reports and its
    vanilla datapack, generating both on first use:

        reports/<version>/reports/{registries,blocks,packets,...}.json
        reports/<version>/datapack/   (the jar's data/minecraft/ folder)
    """
    directory = CACHE / "reports" / version
    done = directory / "reports" / "blocks.json"
    if done.exists():
        return directory
    path = jar(version)
    fixture = server(version)
    if not version_at_least(version, "1.13"):
        raise ValueError(f"{version} has no data generator")
    log(f"Generating reports for {version}...")
    (CACHE / "reports").mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=CACHE / "reports") as temporary:
        temporary = Path(temporary)
        output = temporary / "out"
        if version_at_least(version, "1.18"):
            command = [java(fixture["java"]), "-DbundlerMainClass=net.minecraft.data.Main",
                       "-jar", str(path)]
        else:
            command = [java(fixture["java"]), "-cp", str(path), "net.minecraft.data.Main"]
        subprocess.run(command + ["--reports", "--output", str(output)], cwd=temporary,
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=900)
        staged = temporary / "staged"
        staged.mkdir()
        (output / "reports").replace(staged / "reports")
        with zipfile.ZipFile(path) as archive:
            game = inner_jar(archive)
            for name in game.namelist():
                if name.startswith("data/minecraft/") and not name.endswith("/"):
                    target = staged / "datapack" / name.removeprefix("data/minecraft/")
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_bytes(game.read(name))
        if directory.exists():
            shutil.rmtree(directory)
        staged.replace(directory)
    return directory


def version_key(version):
    return tuple(int(part) for part in version.split("."))


def version_at_least(version, minimum):
    return version_key(version) >= version_key(minimum)


def report(version, name):
    path = reports(version) / "reports" / name
    return json.loads(path.read_text()) if path.exists() else None


# ViaVersion's packet-type enums (GPL-3) are read only for the packet order
# of versions whose server ships no packets report: names and order are
# Mojang's, the enum constant order is the packet ID. Pinned by commit.
VIA_REPOSITORY = "https://github.com/ViaVersion/ViaVersion.git"
VIA_COMMIT = "22ea5afea51b01ec3e1d3f6d94cdf173d3dd3947"
VIA_PROTOCOLS = "common/src/main/java/com/viaversion/viaversion/protocols"
MCDATA_REPOSITORY = "https://github.com/PrismarineJS/minecraft-data.git"
MCDATA_COMMIT = "9b995542ab4a17837dee785439954e43d031b33e"


def checkout(name, repository, commit):
    """A clone of `repository` holding `commit`, under the cache's ref/."""
    directory = CACHE / "ref" / name
    if not directory.exists():
        log(f"Cloning {repository}...")
        directory.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "clone", "--quiet", repository, str(directory)], check=True)
    present = subprocess.run(["git", "-C", str(directory), "cat-file", "-e", f"{commit}^{{commit}}"],
                             capture_output=True).returncode == 0
    if not present:
        subprocess.run(["git", "-C", str(directory), "fetch", "--quiet", "origin"], check=True)
    return directory


def git_files(name, repository, commit, prefix):
    directory = checkout(name, repository, commit)
    listing = subprocess.run(["git", "-C", str(directory), "ls-tree", "-r", "--name-only", commit,
                              "--", prefix], check=True, capture_output=True, text=True).stdout
    return listing.split()


def git_read(name, repository, commit, path):
    directory = checkout(name, repository, commit)
    return subprocess.run(["git", "-C", str(directory), "show", f"{commit}:{path}"],
                          check=True, capture_output=True, text=True).stdout


def via_read(path):
    return git_read("ViaVersion", VIA_REPOSITORY, VIA_COMMIT, path)


def mcdata(path):
    """A minecraft-data file under data/, parsed, or None when absent."""
    files = mcdata_files()
    if f"data/{path}" not in files:
        return None
    return json.loads(git_read("minecraft-data", MCDATA_REPOSITORY, MCDATA_COMMIT, f"data/{path}"))


_mcdata_files = None


def mcdata_files():
    global _mcdata_files
    if _mcdata_files is None:
        _mcdata_files = set(git_files("minecraft-data", MCDATA_REPOSITORY, MCDATA_COMMIT, "data/pc"))
        _mcdata_files.add("data/dataPaths.json")
    return _mcdata_files


# ViaVersion names its protocol classes after release names; the target
# release of each class is the protocol whose packet types its second and
# fourth type parameters are.
VIA_RELEASES = {
    "1_12": 335, "1_12_1": 338, "1_12_2": 340, "1_13": 393, "1_13_1": 401, "1_13_2": 404,
    "1_14": 477, "1_14_1": 480, "1_14_2": 485, "1_14_3": 490, "1_14_4": 498, "1_15": 573,
    "1_15_1": 575, "1_15_2": 578, "1_16": 735, "1_16_1": 736, "1_16_2": 751, "1_16_3": 753,
    "1_16_4": 754, "1_17": 755, "1_17_1": 756, "1_18": 757, "1_18_2": 758, "1_19": 759,
    "1_19_1": 760, "1_19_3": 761, "1_19_4": 762, "1_20": 763, "1_20_2": 764, "1_20_3": 765,
    "1_20_5": 766, "1_21": 767, "1_21_2": 768, "1_21_4": 769, "1_21_5": 770, "1_21_6": 771,
    "1_21_7": 772, "1_21_9": 773, "1_21_11": 774, "26_1": 775, "26_2": 776, "26_3": 777,
}


def via_sources():
    """Enum name to constants, and interface name to implementing enums."""
    enums, implementers, protocols = {}, {}, {}
    for path in git_files("ViaVersion", VIA_REPOSITORY, VIA_COMMIT, VIA_PROTOCOLS):
        if not path.endswith(".java"):
            continue
        name = Path(path).stem
        if re.fullmatch(r"(Client|Server)bound\w*Packets\w*", name) or name.startswith("Protocol"):
            text = via_read(path)
        else:
            continue
        declaration = re.search(r"public enum (\w+) implements ([^{]+)\{(.*?);", text, re.S)
        if declaration and declaration.group(1) == name:
            body = re.sub(r"//[^\n]*|/\*.*?\*/", "", declaration.group(3), flags=re.S)
            constants = [constant.strip() for constant in body.split(",")]
            if not all(re.fullmatch(r"[A-Z0-9_]+", constant) for constant in constants):
                raise ValueError(f"{path}: cannot read the enum constants")
            enums[name] = [constant.lower() for constant in constants]
            for interface in declaration.group(2).split(","):
                implementers.setdefault(interface.strip(), []).append(name)
        match = re.search(r"class Protocol(\w+?)To(\w+) extends AbstractProtocol<([^>]+)>", text)
        if match and match.group(2) in VIA_RELEASES:
            types = [part.strip() for part in match.group(3).split(",")]
            protocols[VIA_RELEASES[match.group(2)]] = (types[1], types[3])
    return enums, implementers, protocols


# Login, status and handshake packets are one enum each for every version;
# a packet is in a version's table from the protocol that introduced it.
BASE_PACKETS = {
    "login/clientbound": [("login_disconnect", 0), ("hello", 0), ("login_finished", 0),
                          ("login_compression", 0), ("custom_query", 393),
                          ("cookie_request", 766)],
    "login/serverbound": [("hello", 0), ("key", 0), ("custom_query_answer", 393),
                          ("login_acknowledged", 764), ("cookie_response", 766)],
    "status/clientbound": [("status_response", 0), ("pong_response", 0)],
    "status/serverbound": [("status_request", 0), ("ping_request", 0)],
    "handshake/serverbound": [("intention", 0)],
}

# Where ViaVersion's base enums do not use Mojang's name.
VIA_BASE_NAMES = {"client_intention": "intention", "encryption_key": "key"}
VIA_BASE_ENUMS = {
    "login/clientbound": "ClientboundLoginPackets",
    "login/serverbound": "ServerboundLoginPackets",
    "status/clientbound": "ClientboundStatusPackets",
    "status/serverbound": "ServerboundStatusPackets",
    "handshake/serverbound": "ServerboundHandshakePackets",
}


def via_tables():
    """Packet tables of every protocol from ViaVersion's enums, in its names."""
    enums, implementers, protocols = via_sources()
    for key, enum in VIA_BASE_ENUMS.items():
        names = [VIA_BASE_NAMES.get(name, name) for name in enums[enum]]
        if names != [name for name, _ in BASE_PACKETS[key]]:
            raise ValueError(f"ViaVersion's {enum} changed: {names}")
    tables = {}
    for protocol, _ in PROTOCOLS:
        clientbound, serverbound = protocols[protocol]
        table = {key: [name for name, since in packets if protocol >= since]
                 for key, packets in BASE_PACKETS.items()}
        for direction, type_name in (("clientbound", clientbound), ("serverbound", serverbound)):
            if type_name in enums:
                play, configuration = type_name, None
            else:
                found = implementers.get(type_name, [])
                plays = [enum for enum in found if "Configuration" not in enum]
                configurations = [enum for enum in found if "Configuration" in enum]
                if len(plays) != 1 or len(configurations) != 1:
                    raise ValueError(f"cannot resolve ViaVersion's {type_name}: {found}")
                play, configuration = plays[0], configurations[0]
            table[f"play/{direction}"] = enums[play]
            if protocol >= 764:
                if configuration is None:
                    # 1.20.2's protocol class names only the play enums.
                    configuration = f"{direction.capitalize()}ConfigurationPackets1_20_2"
                table[f"configuration/{direction}"] = enums[configuration]
        tables[protocol] = table
    return tables


# One name per logical packet: where Mojang renamed a packet that stayed the
# same thing (its layout may still change), every version uses the newest
# name. Packets that were split, merged or restructured keep their names.
PACKET_RENAMES = {
    "login/clientbound": {
        "game_profile": "login_finished",  # 1.21.2
    },
    "configuration/clientbound": {
        "resource_pack": "resource_pack_push",  # 1.20.3, gained a pack UUID
    },
    "play/clientbound": {
        "resource_pack": "resource_pack_push",  # 1.20.3, gained a pack UUID
        "set_carried_item": "set_held_slot",  # 1.21.2
        "horse_screen_open": "mount_screen_open",  # 1.21.11
    },
    "play/serverbound": {
        "spectate_entity": "spectator_action",  # 26.2, the entity became optional
    },
}

# Where ViaVersion's enum constant is not Mojang's name in that version:
# (table, ViaVersion name, Mojang name, first protocol, last protocol).
VIA_PACKET_NAMES = [
    ("play/serverbound", "debug_subscription_request", "debug_sample_subscription", 766, 772),
    ("play/clientbound", "tracked_waypoint", "waypoint", 771, 777),
    ("play/clientbound", "debug_block_value", "debug/block_value", 773, 777),
    ("play/clientbound", "debug_chunk_value", "debug/chunk_value", 773, 777),
    ("play/clientbound", "debug_entity_value", "debug/entity_value", 773, 777),
    ("play/clientbound", "debug_event", "debug/event", 773, 777),
]

# The first protocol whose server writes reports/packets.json.
PACKET_REPORTS = 767


def renamed(table):
    return {key: [PACKET_RENAMES.get(key, {}).get(name, name) for name in names]
            for key, names in table.items()}


def via_packets(protocol, tables):
    table = {key: list(names) for key, names in tables[protocol].items()}
    for key, via, mojang, first, last in VIA_PACKET_NAMES:
        if first <= protocol <= last:
            if via not in table.get(key, []):
                raise ValueError(f"{protocol} {key}: ViaVersion has no {via}")
            table[key] = [mojang if name == via else name for name in table[key]]
    return renamed(table)


def reported_packets(version):
    packets = report(version, "packets.json")
    if packets is None:
        return None
    return renamed({f"{state}/{direction}": [name.removeprefix("minecraft:")
                                             for name in ordered({"entries": entries})]
                    for state, directions in packets.items()
                    for direction, entries in directions.items()})


def ordered(registry):
    entries = sorted(registry["entries"].items(), key=lambda entry: entry[1]["protocol_id"])
    names = [name for name, _ in entries]
    if [entry["protocol_id"] for _, entry in entries] != list(range(len(names))):
        raise ValueError("registry IDs are not dense")
    return names


def packet_tables():
    """Every protocol's packet tables, from its packets report where the
    server writes one and from ViaVersion's enums before that."""
    via = via_tables()
    tables = {}
    for protocol, version in PROTOCOLS:
        derived = via_packets(protocol, via)
        reported = reported_packets(version) if protocol >= PACKET_REPORTS else None
        if protocol >= PACKET_REPORTS and reported is None:
            raise ValueError(f"{version} wrote no packets report")
        if reported is not None and reported != derived:
            differences = {key: (derived.get(key), reported.get(key))
                           for key in set(reported) | set(derived)
                           if reported.get(key) != derived.get(key)}
            raise ValueError(f"{version}: ViaVersion's packet order disagrees with Mojang's "
                             f"report: {differences}")
        tables[protocol] = reported or derived
    check_packet_names(tables)
    return tables


def check_packet_names(tables):
    for protocol, table in tables.items():
        for key, names in table.items():
            if len(set(names)) != len(names):
                raise ValueError(f"{protocol} {key} repeats a packet name")
    for key, renames in PACKET_RENAMES.items():
        for old, new in renames.items():
            holders = [protocol for protocol, table in tables.items() if new in table.get(key, [])]
            if not holders:
                raise ValueError(f"rename {key} {old} -> {new}: no version has {new}")


class RustTables:
    """Reads the per-protocol packet ID tables Rift's proxy hard-codes.

    Understands only the forms those tables use: `match` over the protocol
    number with `|`, `..=` and `_` patterns, integer, `Some`/`None`, tuple
    and struct literals (with `..BASE`), named constants, calls of known
    functions and `if <protocol> <op> <n> { a } else { b }`.
    """

    TOKEN = re.compile(r'\s*(r#*"|"(?:\\.|[^"\\])*"|0x[0-9a-fA-F_]+|\d[\d_]*'
                       r"|[A-Za-z_]\w*(?:::\w+)*|\.\.=|=>|<=|>=|==|\.\.|\S)")

    def __init__(self, text):
        self.tokens = []
        text = re.sub(r"//[^\n]*", "", text)
        position = 0
        while position < len(text):
            match = self.TOKEN.match(text, position)
            if not match:
                if text[position:].strip() == "":
                    break
                raise ValueError(f"cannot tokenize {text[position:position + 40]!r}")
            token = match.group(1)
            position = match.end()
            if token.startswith("r") and token.endswith('"'):
                # A raw string: skip to its closing quote and hashes.
                end = text.index('"' + token[1:-1], position)
                position = end + len(token) - 1
                token = '""'
            self.tokens.append(token)
        self.position = 0
        self.constants = {"true": lambda protocol: True, "false": lambda protocol: False}
        self.functions = {}

    def find(self, *sequence):
        """Move to just after the first occurrence of these tokens."""
        for start in range(len(self.tokens)):
            if tuple(self.tokens[start:start + len(sequence)]) == sequence:
                self.position = start + len(sequence)
                return self
        raise ValueError(f"cannot find {' '.join(sequence)}")

    def peek(self, offset=0):
        index = self.position + offset
        return self.tokens[index] if index < len(self.tokens) else None

    def take(self, expected=None):
        token = self.tokens[self.position]
        if expected is not None and token != expected:
            context = " ".join(self.tokens[max(0, self.position - 8):self.position + 8])
            raise ValueError(f"expected {expected!r}, found {token!r} near: {context}")
        self.position += 1
        return token

    def number(self):
        negative = self.peek() == "-"
        if negative:
            self.take()
        value = int(self.take().replace("_", ""), 0)
        return -value if negative else value

    def expression(self):
        token = self.peek()
        if token == "Some":
            self.take()
            self.take("(")
            value = self.expression()
            self.take(")")
            return value
        if token in ("None",):
            self.take()
            return lambda protocol: None
        if token == "return":
            self.take()
            return self.expression()
        if token == "{":
            self.take()
            value = self.expression()
            self.take("}")
            return value
        if token == "if":
            self.take()
            subject = self.take()
            while self.peek() in (".", "0", "number", "(", ")") and subject != "number":
                subject += self.take()
            operator = self.take()
            if operator in ("<", ">") and self.peek() == "=":
                operator += self.take()
            limit = self.number()
            self.take("{")
            then = self.expression()
            self.take("}")
            self.take("else")
            self.take("{")
            otherwise = self.expression()
            self.take("}")
            test = {"<": int.__lt__, "<=": int.__le__, ">": int.__gt__, ">=": int.__ge__,
                    "==": int.__eq__}[operator]
            return lambda protocol: (then if test(protocol, limit) else otherwise)(protocol)
        if token == "match":
            self.take()
            while self.peek() != "{":
                self.take()
            return self.arms()
        if token == "(":
            self.take()
            items = []
            while self.peek() != ")":
                items.append(self.expression())
                if self.peek() == ",":
                    self.take()
            self.take(")")
            return lambda protocol: tuple(item(protocol) for item in items)
        if token == "-" or token[0].isdigit():
            value = self.number()
            return lambda protocol: value
        name = self.take()
        if self.peek() == "{" and name[0].isupper():
            return self.structure()
        if self.peek() == "(":
            self.take()
            while self.take() != ")":
                pass
            return lambda protocol: self.functions[name](protocol)
        return lambda protocol: self.constants[name](protocol)

    def structure(self):
        self.take("{")
        fields, base = {}, None
        while self.peek() != "}":
            if self.peek() == "..":
                self.take()
                base = self.take()
            else:
                field = self.take()
                self.take(":")
                fields[field] = self.expression()
            if self.peek() == ",":
                self.take()
        self.take("}")

        def build(protocol):
            values = dict(self.constants[base](protocol)) if base else {}
            values.update({field: value(protocol) for field, value in fields.items()})
            return values
        return build

    def pattern(self):
        tests = []
        while True:
            if self.peek() == "_":
                self.take()
                tests.append(lambda protocol: True)
            elif self.peek() == "number":
                self.take()
                tests.append(lambda protocol: True)
            else:
                low = self.number()
                if self.peek() == "..=":
                    self.take()
                    high = self.number()
                else:
                    high = low
                tests.append(lambda protocol, low=low, high=high: low <= protocol <= high)
            if self.peek() != "|":
                return lambda protocol: any(test(protocol) for test in tests)
            self.take()

    def arms(self):
        self.take("{")
        arms = []
        while self.peek() != "}":
            test = self.pattern()
            self.take("=>")
            arms.append((test, self.expression()))
            if self.peek() == ",":
                self.take()
        self.take("}")

        def evaluate(protocol):
            for test, value in arms:
                if test(protocol):
                    return value(protocol)
            raise ValueError(f"no arm matches protocol {protocol}")
        return evaluate


def rift_packet_ids():
    """(source, protocol, table, packet name, ID) for every packet ID Rift's
    proxy hard-codes for a protocol this crate translates."""
    source = ROOT / "src" / "protocol"
    read = lambda name: (source / name).read_text()
    found = []

    def add(label, table, name, value, protocols=None):
        for protocol, _ in PROTOCOLS:
            if protocols and protocol not in protocols:
                continue
            packet = name(protocol) if callable(name) else name
            packet_id = value(protocol)
            if packet_id is not None and packet_id >= 0:
                found.append((label, protocol, table, packet, packet_id))

    packets = RustTables(read("packets.rs"))
    add("packets.rs custom_payload_id", "play/clientbound", "custom_payload",
        packets.find("(", "State::Play", ",", "Direction::Clientbound", ")", "=>").expression())
    for direction, table in (("Serverbound", "configuration/serverbound"),
                             ("Clientbound", "configuration/clientbound")):
        add("packets.rs custom_payload_id", table, "custom_payload",
            packets.find("State::Configuration", ",", f"Direction::{direction}", ")", "if", "self",
                         ".", "has_configuration", "(", ")", "=>").expression(),
            [protocol for protocol, _ in PROTOCOLS if protocol >= 764])
    for function, table, name in (
            ("config_finish", "configuration/clientbound", "finish_configuration"),
            ("config_finish", "configuration/serverbound", "finish_configuration"),
            ("config_disconnect", "configuration/clientbound", "disconnect"),
            ("play_disconnect", "play/clientbound", "disconnect"),
            ("start_configuration", "play/clientbound", "start_configuration"),
            ("configuration_acknowledged", "play/serverbound", "configuration_acknowledged")):
        packets.find("fn", function)
        while packets.peek() not in ("match", "if"):
            packets.take()
        value = packets.expression()
        add(f"packets.rs {function}", table, name, value,
            [protocol for protocol, _ in PROTOCOLS
             if protocol >= 764 or not function.startswith("config_")])
    login = [("login/serverbound", 0, "hello", 0), ("login/serverbound", 1, "key", 0),
             ("login/serverbound", 2, "custom_query_answer", 393),
             ("login/serverbound", 3, "login_acknowledged", 764),
             ("login/serverbound", 4, "cookie_response", 766),
             ("login/clientbound", 0, "login_disconnect", 0), ("login/clientbound", 1, "hello", 0),
             ("login/clientbound", 2, "login_finished", 0),
             ("login/clientbound", 3, "login_compression", 0),
             ("login/clientbound", 4, "custom_query", 393),
             ("login/clientbound", 5, "cookie_request", 766)]
    for table, packet_id, name, since in login:
        add("packets.rs kind", table, name, lambda protocol, packet_id=packet_id: packet_id,
            [protocol for protocol, _ in PROTOCOLS if protocol >= since])

    switching = RustTables(read("switching.rs") + read("legacy_capabilities.rs"))
    for name in re.findall(r"const (\w+): SwitchingCapabilities", read("switching.rs")):
        switching.find("const", name, ":", "SwitchingCapabilities", "=")
        switching.constants[name] = switching.expression()
    switching.find("fn", "legacy_capabilities")
    while switching.peek() != "match":
        switching.take()
    switching.functions["legacy_capabilities"] = switching.expression()
    switching.find("fn", "for_version")
    while switching.peek() != "match":
        switching.take()
    capabilities = switching.expression()
    fields = {
        "join_game": ("play/clientbound", "login"),
        "commands": ("play/clientbound", "commands"),
        "bundle_delimiter": ("play/clientbound", "bundle_delimiter"),
        "play_information": ("play/serverbound", "client_information"),
        "config_information": ("configuration/serverbound", "client_information"),
        "play_payload": ("play/serverbound", "custom_payload"),
        "config_payload": ("configuration/serverbound", "custom_payload"),
        "respawn": ("play/clientbound", "respawn"),
        "game_event": ("play/clientbound", "game_event"),
        "config_pack_pop": ("configuration/clientbound", "resource_pack_pop"),
        # Commands are chat messages before 1.19, signed from 1.19 to 1.20.4.
        "command": ("play/serverbound", lambda p: "chat" if p < 759 else "chat_command"),
        "signed_command": ("play/serverbound",
                           lambda p: "chat_command" if p < 766 else "chat_command_signed"),
        "chat_acknowledgement": ("play/serverbound", "chat_ack"),
        "system_chat": ("play/clientbound", lambda p: "chat" if p < 759 else "system_chat"),
        "suggestion_request": ("play/serverbound", "command_suggestion"),
        "suggestion_response": ("play/clientbound", "command_suggestions"),
    }
    for field, (table, name) in fields.items():
        add(f"switching.rs {field}", table, name,
            lambda protocol, field=field: (capabilities(protocol) or {}).get(field))

    legacy = RustTables(read("legacy_ids.rs")).find("fn", "ids")
    while legacy.peek() != "match":
        legacy.take()
    legacy = legacy.expression()
    columns = [("respawn", "play/clientbound", "respawn"),
               ("keepalive", "play/clientbound", "keep_alive"),
               ("keepalive_reply", "play/serverbound", "keep_alive"),
               ("boss", "play/clientbound", "boss_event"),
               ("players", "play/clientbound",
                lambda p: "player_info" if p < 761 else "player_info_update"),
               ("remove_players", "play/clientbound", "player_info_remove"),
               ("header", "play/clientbound", "tab_list"),
               ("title", "play/clientbound", lambda p: "set_titles" if p < 755 else "clear_titles")]
    legacy_protocols = [protocol for protocol, _ in PROTOCOLS if protocol <= 763]
    for index, (column, table, name) in enumerate(columns):
        add(f"legacy_ids.rs {column}", table, name,
            lambda protocol, index=index: legacy(protocol)[index], legacy_protocols)

    boss = RustTables(read("boss_bar.rs")).find("fn", "id")
    while boss.peek() != "match":
        boss.take()
    add("boss_bar.rs id", "play/clientbound", "boss_event", boss.expression(),
        [protocol for protocol, _ in PROTOCOLS if protocol >= 764])

    dialog = RustTables(read("dialog.rs")).find("fn", "ids")
    while dialog.peek() != "match":
        dialog.take()
    play = dialog.expression()
    while dialog.peek() != "if":
        dialog.take()
    configuration = dialog.expression()
    dialogs = [protocol for protocol, _ in PROTOCOLS if protocol >= 771]
    for index, name in enumerate(("show_dialog", "clear_dialog")):
        add("dialog.rs ids", "play/clientbound", name,
            lambda protocol, index=index: play(protocol)[index], dialogs)
        add("dialog.rs ids", "configuration/clientbound", name,
            lambda protocol, index=index: configuration(protocol)[index], dialogs)
    add("dialog.rs ids", "play/serverbound", "custom_click_action",
        lambda protocol: play(protocol)[2], dialogs)
    add("dialog.rs ids", "configuration/serverbound", "custom_click_action",
        lambda protocol: 0x08, dialogs)
    return found


def check_rift_ids(tables):
    """Rift's hard-coded packet IDs must name the same packets as the tables."""
    problems = []
    checked = 0
    for label, protocol, table, name, packet_id in rift_packet_ids():
        names = tables[protocol].get(table, [])
        actual = names[packet_id] if packet_id < len(names) else None
        checked += 1
        if actual != name:
            problems.append(f"{label}: {protocol} {table} 0x{packet_id:02x} is {actual}, "
                            f"expected {name}")
    if problems:
        raise ValueError("Rift's hard-coded packet IDs disagree with the tables:\n  "
                         + "\n  ".join(problems))
    return checked


def mcdata_path(version, kind):
    """minecraft-data's folder for one kind of data of a release, or None."""
    paths = mcdata("dataPaths.json")["pc"]
    entry = paths.get(version)
    if entry is None and version_key(version)[:2] == (26, 1):
        entry = paths.get("26.1")  # minecraft-data lists 26.1, which 26.1.2 shares
    return entry.get(kind) if entry else None


# minecraft-data's names of well-known packets, to compare their IDs.
MCDATA_PACKETS = {
    "play/clientbound": {
        "keep_alive": ["keep_alive"], "login": ["login"], "respawn": ["respawn"],
        "custom_payload": ["custom_payload"], "disconnect": ["kick_disconnect"],
        "commands": ["declare_commands"], "start_configuration": ["start_configuration"],
        "player_position": ["position"], "game_event": ["game_state_change"],
        "system_chat": ["system_chat"], "chat": ["chat"], "player_chat": ["player_chat"],
        "set_chunk_cache_center": ["update_view_position"], "level_chunk": ["map_chunk"],
        "level_chunk_with_light": ["map_chunk"], "bundle_delimiter": ["bundle_delimiter"],
        "add_entity": ["spawn_entity"], "set_entity_data": ["entity_metadata"],
        "block_update": ["block_change"], "container_set_slot": ["set_slot"],
        "set_time": ["update_time"], "level_particles": ["world_particles"],
        "sound": ["sound_effect"], "update_tags": ["tags"], "tab_list": ["playerlist_header"],
    },
    "play/serverbound": {
        "keep_alive": ["keep_alive"], "custom_payload": ["custom_payload"],
        "chat": ["chat", "chat_message"], "client_information": ["settings"],
        "configuration_acknowledged": ["configuration_acknowledged"],
        "accept_teleportation": ["teleport_confirm"], "chat_command": ["chat_command"],
        "use_item_on": ["block_place"], "set_creative_mode_slot": ["set_creative_slot"],
        "container_click": ["window_click"], "interact": ["use_entity"],
        "move_player_pos": ["position"], "swing": ["arm_animation"],
    },
    "configuration/clientbound": {
        "finish_configuration": ["finish_configuration"], "custom_payload": ["custom_payload"],
        "keep_alive": ["keep_alive"], "disconnect": ["disconnect"],
        "registry_data": ["registry_data"],
    },
    "configuration/serverbound": {
        "finish_configuration": ["finish_configuration"], "custom_payload": ["custom_payload"],
        "keep_alive": ["keep_alive"], "client_information": ["settings"],
    },
    "login/clientbound": {
        "login_disconnect": ["disconnect"], "hello": ["encryption_begin"],
        "login_finished": ["success"], "login_compression": ["compress"],
        "custom_query": ["login_plugin_request"],
    },
    "login/serverbound": {
        "hello": ["login_start"], "key": ["encryption_begin"],
        "custom_query_answer": ["login_plugin_response"],
        "login_acknowledged": ["login_acknowledged"],
    },
}

# Known minecraft-data errors, checked against Mojang's packets report:
# 1.21 to 1.21.5 list the clientbound-only custom_report_details and
# server_links as serverbound configuration packets 0x08 and 0x09.
MCDATA_PACKET_ERRATA = {
    (protocol, "configuration/serverbound"): 10 for protocol in (767, 768, 769, 770)
}


def check_mcdata_packets(tables):
    """Packet counts per table and the IDs of well-known packets must match
    minecraft-data's protocol.json wherever it has the release."""
    states = {"handshaking": "handshake", "status": "status", "login": "login",
              "configuration": "configuration", "play": "play"}
    problems, compared = [], 0
    for protocol, version in PROTOCOLS:
        folder = mcdata_path(version, "protocol")
        if folder is None:
            continue
        if mcdata(f"{mcdata_path(version, 'version')}/version.json")["version"] != protocol:
            problems.append(f"minecraft-data's {folder} is not protocol {protocol}")
            continue
        schema = mcdata(f"{folder}/protocol.json")
        for state, directions in schema.items():
            if state == "types":
                continue
            for direction, body in directions.items():
                key = f"{states[state]}/{'clientbound' if direction == 'toClient' else 'serverbound'}"
                mappings = body["types"]["packet"][1][0]["type"][1]["mappings"]
                ids = {name: int(packet_id, 16) for packet_id, name in mappings.items()}
                ids.pop("legacy_server_list_ping", None)  # the pre-Netty ping, not a packet
                ours = tables[protocol].get(key, [])
                expected = MCDATA_PACKET_ERRATA.get((protocol, key), len(ours))
                if sorted(ids.values()) != list(range(expected)):
                    problems.append(f"{version} {key}: minecraft-data has {len(ids)} packets, "
                                    f"the table {len(ours)}")
                for name, aliases in MCDATA_PACKETS.get(key, {}).items():
                    if name not in ours:
                        continue
                    theirs = [ids[alias] for alias in aliases if alias in ids]
                    compared += 1
                    if ours.index(name) not in theirs:
                        problems.append(f"{version} {key} {name}: ID {ours.index(name)}, "
                                        f"minecraft-data {theirs}")
    if problems:
        raise ValueError("packet tables disagree with minecraft-data:\n  "
                         + "\n  ".join(problems))
    return compared


def render(value):
    """Compact JSON with one top-level section per line, so diffs stay
    readable without bloating the files."""
    lines = [f"{json.dumps(key)}:{json.dumps(item, separators=(',', ':'))}"
             for key, item in value.items()]
    return "{" + ",\n".join(lines) + "}\n"


def version_file(protocol, version, packets):
    return {
        "protocol": protocol,
        "name": version,
        "packets": dict(sorted(packets.items())),
    }


def outputs():
    tables = packet_tables()
    check_mcdata_packets(tables)
    check_rift_ids(tables)
    files = {}
    for protocol, version in PROTOCOLS:
        files[f"{protocol}.json"] = render(version_file(protocol, version, tables[protocol]))
    return files


def fill_cache(jobs):
    versions = [name for _, name in PROTOCOLS if version_at_least(name, "1.13")]
    for version in versions:
        jar(version)
    with ThreadPoolExecutor(jobs) as pool:
        list(pool.map(reports, versions))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="fail if generated files differ")
    parser.add_argument("--reports", action="store_true", help="only fill the report cache")
    parser.add_argument("--jobs", type=int, default=4, help="data generators run at once")
    args = parser.parse_args()
    fill_cache(args.jobs)
    if args.reports:
        return
    files = outputs()
    stale = sorted(name for name, text in files.items()
                   if not (OUTPUT / name).exists() or (OUTPUT / name).read_text() != text)
    generated = {str(path.relative_to(OUTPUT)) for path in OUTPUT.rglob("*.json")
                 if not path.relative_to(OUTPUT).parts[0] in HAND_WRITTEN} if OUTPUT.exists() else set()
    unknown = sorted(generated - set(files))
    if args.check:
        if stale or unknown:
            raise SystemExit(f"stale translation data: {', '.join(stale + unknown)}; "
                             "run scripts/translation_crate.py")
        return
    for name in stale:
        (OUTPUT / name).parent.mkdir(parents=True, exist_ok=True)
        (OUTPUT / name).write_text(files[name])
        log(f"wrote {OUTPUT / name}")
    for name in unknown:
        (OUTPUT / name).unlink()
        log(f"removed {OUTPUT / name}")


if __name__ == "__main__":
    main()
