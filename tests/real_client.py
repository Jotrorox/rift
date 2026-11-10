#!/usr/bin/env python3
"""Run real, unmodified vanilla Minecraft clients headlessly and check that they can play.

The vanilla client is the authority on whether a clientbound stream is valid:
it disconnects or logs an exception when it receives something it cannot
handle. This tool prepares official Mojang client files (checksum-verified,
cached under ~/.cache/rift-via/clients), starts the game with the HeadlessMC
LWJGL agent so that nothing opens a window, grabs input or plays sound, makes
it join a server and classifies what it logs.

    python3 tests/real_client.py --accept-eula setup 1.12.2 1.20.1 26.3
    python3 tests/real_client.py --accept-eula join --version 1.20.1 --address 127.0.0.1:25565
    python3 tests/real_client.py --accept-eula selftest --versions 1.20.1 26.3

Only rendering, input and audio are stubbed (the agent rewrites org.lwjgl
classes); networking and packet handling are the vanilla code. Clients run
in offline mode, so targets must be online-mode=false servers or proxies.
Stdlib only; requires Python 3.11+ and the matching Java runtime
(~/.cache/rift-via/jdks/jdk{8,17,21,25} or RIFT_JAVA_<major>).
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager, ExitStack
from dataclasses import asdict, dataclass, field
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import uuid
import zipfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
VIA = Path(os.environ.get("RIFT_VIA_CACHE", Path.home() / ".cache" / "rift-via")).expanduser()
CACHE = VIA / "clients"
LOCKS = VIA / "locks"
JDKS = VIA / "jdks"
TOOLS = json.loads((HERE / "real_client_tools.json").read_text())
MANIFEST_URL = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"
USER_AGENT = "rift-integration-tests/0.1.0 (https://github.com/Jotrorox/rift)"
CLIENT_SLOTS = int(os.environ.get("RIFT_REAL_CLIENT_SLOTS", "2"))
# Authentication, session and services hosts are pointed at a closed local
# port: an offline client has nothing to say to Mojang and must not leak the
# made-up account. Refused connections fail fast and are logged as harmless.
DEAD_HOST = "http://127.0.0.1:9"


# --------------------------------------------------------------------------
# Downloads, checksums and locks


def log(message):
    print(f"[real-client] {message}", file=sys.stderr, flush=True)


@contextmanager
def file_lock(name, blocking=True):
    """Machine-wide exclusive lock (flock) on ~/.cache/rift-via/locks/<name>."""
    LOCKS.mkdir(parents=True, exist_ok=True)
    handle = open(LOCKS / name, "a+")
    try:
        flags = fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB)
        try:
            fcntl.flock(handle, flags)
        except BlockingIOError:
            handle.close()
            yield None
            return
        handle.seek(0)
        handle.truncate()
        handle.write(f"{os.getpid()}\n")
        handle.flush()
        try:
            yield handle
        finally:
            fcntl.flock(handle, fcntl.LOCK_UN)
    finally:
        if not handle.closed:
            handle.close()


def digest(path, algorithm):
    with open(path, "rb") as source:
        return hashlib.file_digest(source, algorithm).hexdigest()


def fetch(url, destination, algorithm=None, checksum=None, size=None, attempts=3):
    """Atomically download url to destination, verifying the checksum if given.

    An existing file is kept when it verifies. Concurrent writers are safe:
    every writer renames a complete, verified temporary file into place.
    """
    destination = Path(destination)
    if destination.exists() and (checksum is None or digest(destination, algorithm) == checksum):
        return destination
    destination.parent.mkdir(parents=True, exist_ok=True)
    error = None
    for attempt in range(attempts):
        handle, pending = tempfile.mkstemp(dir=destination.parent, prefix=".part-")
        try:
            request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
            with os.fdopen(handle, "wb") as output, urllib.request.urlopen(request, timeout=120) as response:
                while block := response.read(1 << 20):
                    output.write(block)
            if size is not None and os.path.getsize(pending) != size:
                raise ValueError(f"size mismatch for {url}: {os.path.getsize(pending)} != {size}")
            if checksum is not None and (actual := digest(pending, algorithm)) != checksum:
                raise ValueError(f"{algorithm} mismatch for {url}: {actual} != {checksum}")
            os.replace(pending, destination)
            return destination
        except (OSError, ValueError) as exc:
            error = exc
            time.sleep(1 + attempt)
        finally:
            if os.path.exists(pending):
                os.unlink(pending)
    raise RuntimeError(f"download failed: {url}: {error}")


def fetch_json(url, destination, sha1=None):
    return json.loads(fetch(url, destination, "sha1" if sha1 else None, sha1).read_text())


def tool(name):
    entry = TOOLS[name]
    return fetch(entry["url"], CACHE / "tools" / entry["filename"], entry["algorithm"], entry["checksum"])


# --------------------------------------------------------------------------
# Launcher metadata (rules, libraries, arguments)


def host_os():
    return {"darwin": "osx", "linux": "linux", "win32": "windows"}.get(sys.platform, sys.platform)


def host_arch():
    machine = platform.machine().lower()
    return {"amd64": "x86_64", "aarch64": "arm64"}.get(machine, machine)


def rules_allow(rules, os_name=None, arch=None, os_version="", features=None):
    """Evaluate a launcher rule list (the last matching rule wins; no rules allows)."""
    if not rules:
        return True
    os_name = os_name or host_os()
    arch = arch or host_arch()
    features = features or {}
    allowed = False
    for rule in rules:
        matches = True
        constraint = rule.get("os", {})
        if "name" in constraint and constraint["name"] != os_name:
            matches = False
        # "x86" in rules means a 32-bit JVM; 64-bit hosts never match it.
        if "arch" in constraint and constraint["arch"] != {"x86_64": "x64"}.get(arch, arch):
            matches = False
        if "version" in constraint and not re.search(constraint["version"], os_version):
            matches = False
        if "versionRange" in constraint:
            matches = False  # Windows build ranges only.
        for feature, wanted in rule.get("features", {}).items():
            if bool(features.get(feature)) != wanted:
                matches = False
        if matches:
            allowed = rule["action"] == "allow"
    return allowed


def maven_path(name):
    parts = name.split(":")
    group, artifact, version = parts[:3]
    classifier = f"-{parts[3]}" if len(parts) > 3 else ""
    return f"{group.replace('.', '/')}/{artifact}/{version}/{artifact}-{version}{classifier}.jar"


def is_native_library(library):
    """True for LWJGL/platform native jars: the agent stubs everything that would load them."""
    parts = library["name"].split(":")
    return "natives" in library or (len(parts) > 3 and parts[3].startswith("natives-"))


def libraries(version_json, os_name=None, arch=None):
    """(relative path, url, sha1, size) of every Java library the client needs here."""
    seen, result = set(), []
    for library in version_json["libraries"]:
        if not rules_allow(library.get("rules"), os_name, arch) or is_native_library(library):
            continue
        artifact = library.get("downloads", {}).get("artifact")
        if artifact is None:
            continue
        path = artifact.get("path") or maven_path(library["name"])
        if path in seen:
            continue
        seen.add(path)
        result.append((path, artifact["url"], artifact.get("sha1"), artifact.get("size")))
    return result


def substitute(value, variables):
    return re.sub(r"\$\{(\w+)\}", lambda match: str(variables.get(match.group(1), match.group(0))), value)


def arguments(version_json, kind, variables, features=None):
    """Evaluate the 'arguments' (1.13+) or 'minecraftArguments' (legacy) templates."""
    if "arguments" not in version_json:
        if kind == "game":
            return [substitute(part, variables) for part in version_json["minecraftArguments"].split()]
        return [substitute(part, variables) for part in ("-Djava.library.path=${natives_directory}",
                                                         "-cp", "${classpath}")]
    result = []
    for entry in version_json["arguments"].get(kind, []):
        if isinstance(entry, str):
            result.append(substitute(entry, variables))
            continue
        if not rules_allow(entry.get("rules"), features=features):
            continue
        values = entry["value"] if isinstance(entry["value"], list) else [entry["value"]]
        result.extend(substitute(value, variables) for value in values)
    return result


def supports_quick_play(version_json):
    game = version_json.get("arguments", {}).get("game", [])
    return any(isinstance(entry, dict) and "is_quick_play_multiplayer" in json.dumps(entry.get("rules"))
               for entry in game)


def wanted_asset(name):
    """Sounds and music (~370 MB) and translations are never needed by a muted English client."""
    if name.endswith(".ogg"):
        return False
    if name.startswith(("minecraft/lang/", "lang/")) and not re.search(r"/en_us\.(json|lang)$", name.lower()):
        return False
    return True


# --------------------------------------------------------------------------
# Setup


@dataclass
class Prepared:
    version: str
    java_major: int
    main_class: str
    client_jar: Path
    classpath: list
    asset_index: str
    version_json: dict


def version_manifest(refresh=False):
    path = CACHE / "meta" / "version_manifest_v2.json"
    if refresh or not path.exists() or time.time() - path.stat().st_mtime > 6 * 3600:
        with file_lock("clients-manifest"):
            fetch(MANIFEST_URL, path.with_suffix(".tmp"))
            os.replace(path.with_suffix(".tmp"), path)
    return json.loads(path.read_text())


def version_json(version):
    path = CACHE / "meta" / "versions" / f"{version}.json"
    if path.exists():
        return json.loads(path.read_text())
    for attempt in (False, True):
        for entry in version_manifest(refresh=attempt)["versions"]:
            if entry["id"] == version:
                return fetch_json(entry["url"], path, entry["sha1"])
    raise ValueError(f"unknown Minecraft version {version!r}")


def java_major(data):
    return data.get("javaVersion", {}).get("majorVersion", 8)


def setup(version, quiet=False):
    """Download and verify everything needed to run `version`. Idempotent and lock-guarded."""
    data = version_json(version)
    with file_lock(f"clients-setup-{version}"):
        stamp = CACHE / "meta" / "ready" / version
        client = data["downloads"]["client"]
        client_jar = CACHE / "versions" / version / f"{version}.jar"
        libs = libraries(data)
        if not stamp.exists():
            if not quiet:
                log(f"preparing {version} (client, {len(libs)} libraries, assets {data['assetIndex']['id']})")
            jobs = [(client["url"], client_jar, "sha1", client["sha1"], client["size"])]
            jobs += [(url, CACHE / "libraries" / path, "sha1" if sha1 else None, sha1, size)
                     for path, url, sha1, size in libs]
            index = data["assetIndex"]
            index_json = fetch_json(index["url"], CACHE / "assets" / "indexes" / f"{index['id']}.json",
                                    index["sha1"])
            for name, entry in index_json["objects"].items():
                if wanted_asset(name):
                    sha1 = entry["hash"]
                    jobs.append((f"https://resources.download.minecraft.net/{sha1[:2]}/{sha1}",
                                 CACHE / "assets" / "objects" / sha1[:2] / sha1, "sha1", sha1, entry["size"]))
            with ThreadPoolExecutor(12) as pool:
                for future in [pool.submit(fetch, *job) for job in jobs]:
                    future.result()
            stamp.parent.mkdir(parents=True, exist_ok=True)
            stamp.write_text(json.dumps({"files": len(jobs), "time": time.time()}))
        tool("headlessmc-lwjgl")
        find_java(java_major(data))
    return Prepared(version, java_major(data), data["mainClass"], client_jar,
                    [CACHE / "libraries" / path for path, *_ in libs] + [client_jar],
                    data["assetIndex"]["id"], data)


def find_java(major):
    """Java for a client's majorVersion: RIFT_JAVA_<n> or the cached JDK n (or the next newer LTS)."""
    for candidate in [major] + [lts for lts in (17, 21, 25) if lts > major]:
        override = os.environ.get(f"RIFT_JAVA_{candidate}")
        if override:
            return override
        path = JDKS / f"jdk{candidate}" / "bin" / "java"
        if path.exists():
            return str(path)
        if major == 8:
            break  # Old clients (LWJGL 2, launchwrapper era) need Java 8 itself.
    raise FileNotFoundError(f"no Java {major} runtime: set RIFT_JAVA_{major} or install {JDKS}/jdk{major}")


# --------------------------------------------------------------------------
# Headless helpers generated into the cache (no third-party code involved)

# Shadows the macOS narrator of every text2speech version (NarratorOSX up to
# 1.16, NarratorMac since 1.17) with a silent, inactive one. Otherwise the
# first-launch accessibility screen makes the real client speak aloud through
# NSSpeechSynthesizer: rendering and OpenAL stubs do not cover it.
SILENT_NARRATOR = """package com.mojang.text2speech;
public class {name} implements Narrator {{
    public {name}() {{}}
    public void say(String message) {{}}
    public void say(String message, boolean interrupt) {{}}
    public void clear() {{}}
    public boolean active() {{ return false; }}
    public void destroy() {{}}
}}
"""
# Only the stubs are packaged; this placeholder exists for javac and is
# replaced at runtime by the real interface (extra methods are harmless).
NARRATOR_PLACEHOLDER = "package com.mojang.text2speech;\npublic interface Narrator {}\n"
HELPERS_VERSION = 1


def helpers():
    """Agent jars and the silent narrator. Returns (lwjgl agent, macos agent, narrator jar)."""
    agent = tool("headlessmc-lwjgl")
    directory = CACHE / "tools" / f"helpers-{HELPERS_VERSION}"
    macos = directory / "macos-menu-agent.jar"
    narrator = directory / "silent-narrator.jar"
    if macos.exists() and narrator.exists():
        return agent, macos, narrator
    with file_lock("clients-helpers"):
        if macos.exists() and narrator.exists():
            return agent, macos, narrator
        directory.mkdir(parents=True, exist_ok=True)
        # HeadlessMC's MacosMenuAgent (removes the native menu bar access that
        # SDL-era clients crash in) is not its jar's Premain-Class, so a
        # manifest-only jar names it and puts the real jar on its class path.
        with tempfile.TemporaryDirectory(dir=directory) as temporary:
            pending = Path(temporary) / "agent.jar"
            with zipfile.ZipFile(pending, "w") as jar:
                jar.writestr("META-INF/MANIFEST.MF", (
                    "Manifest-Version: 1.0\r\n"
                    "Premain-Class: io.github.headlesshq.headlessmc.lwjgl.agent.MacosMenuAgent\r\n"
                    f"Class-Path: {os.path.relpath(agent, directory)}\r\n\r\n"))
            os.replace(pending, macos)
            sources = Path(temporary) / "src"
            package = sources / "com" / "mojang" / "text2speech"
            package.mkdir(parents=True)
            (package / "Narrator.java").write_text(NARRATOR_PLACEHOLDER)
            for name in ("NarratorOSX", "NarratorMac"):
                (package / f"{name}.java").write_text(SILENT_NARRATOR.format(name=name))
            classes = Path(temporary) / "classes"
            javac = find_javac()
            release = ["-source", "8", "-target", "8"] if "jdk8" in javac else ["--release", "8"]
            subprocess.run([javac, *release, "-nowarn", "-d", str(classes),
                            *map(str, package.glob("*.java"))], check=True, capture_output=True)
            pending = Path(temporary) / "narrator.jar"
            with zipfile.ZipFile(pending, "w") as jar:
                for name in ("NarratorOSX", "NarratorMac"):
                    jar.write(classes / "com/mojang/text2speech" / f"{name}.class",
                              f"com/mojang/text2speech/{name}.class")
            os.replace(pending, narrator)
    return agent, macos, narrator


def find_javac():
    for major in (17, 21, 25, 8):
        try:
            java = Path(find_java(major))
        except FileNotFoundError:
            continue
        if (javac := java.with_name("javac")).exists():
            return str(javac)
    raise FileNotFoundError("no javac found next to any cached Java runtime")


# --------------------------------------------------------------------------
# Launching


# Settings that make the headless client cheap and keep first-launch screens,
# narration, sound and Mojang services out of the way. Unknown keys are kept
# but ignored by versions that do not have them.
OPTIONS = {
    "version": None,  # filled with the client's data version when known
    "renderDistance": 2, "simulationDistance": 5, "maxFps": 10, "enableVsync": "false",
    "graphicsMode": 0, "fancyGraphics": "false", "renderClouds": '"false"', "clouds": "false",
    "graphicsPreset": '"custom"', "particles": 2, "entityShadows": "false", "mipmapLevels": 0,
    "biomeBlendRadius": 0, "cloudRange": 2, "weatherRadius": 3,
    "fboEnable": "false", "useVbo": "false", "entityDistanceScaling": 0.5,
    "narrator": 0, "tutorialStep": "none", "skipMultiplayerWarning": "true",
    "skipRealms32bitWarning": "true", "onboardAccessibility": "false", "joinedFirstServer": "true",
    "hasShownMultiplayerWarning": "true", "realmsNotifications": "false", "snooperEnabled": "false",
    "telemetryOptInExtra": "false", "allowServerListing": "false", "useNativeTransport": "false",
    "pauseOnLostFocus": "false", "autoJump": "false", "fullscreen": "false", "showSubtitles": "false",
    "hideSplashTexts": "true", "darkMojangStudiosBackground": "true", "lang": "en_us",
    "chatVisibility": 0, "chatLinksPrompt": "false", "resourcePacks": "[]", "incompatibleResourcePacks": "[]",
    "soundDevice": '""', "syncChunkWrites": "false", "prioritizeChunkUpdates": 0,
}
SOUND_CATEGORIES = ("master", "music", "record", "weather", "block", "hostile", "neutral", "player",
                    "ambient", "voice", "ui")

LOG4J_CONFIG = """<?xml version="1.0" encoding="UTF-8"?>
<Configuration status="WARN">
  <Appenders>
    <Console name="SysOut" target="SYSTEM_OUT"><PatternLayout pattern="{pattern}"/></Console>
    <File name="File" fileName="{file}" append="false"><PatternLayout pattern="{pattern}"/></File>
  </Appenders>
  <Loggers>
    <Root level="info">
      <filters><MarkerFilter marker="NETWORK_PACKETS" onMatch="DENY" onMismatch="NEUTRAL"/></filters>
      <AppenderRef ref="SysOut"/><AppenderRef ref="File"/>
    </Root>
  </Loggers>
</Configuration>
"""
# {nolookups} keeps old clients' log4j 2.8 from evaluating JNDI lookups.
LOG_PATTERN = "[%d{HH:mm:ss.SSS}] [%t/%level] [%logger{1}]: %msg{nolookups}%n"


def write_options(game_dir, data_version=None):
    lines = []
    for key, value in OPTIONS.items():
        if key == "version":
            if data_version:
                lines.append(f"version:{data_version}")
            continue
        lines.append(f"{key}:{value}")
    lines += [f"soundCategory_{category}:0.0" for category in SOUND_CATEGORIES]
    (game_dir / "options.txt").write_text("\n".join(lines) + "\n")


def data_version(prepared):
    """World/options data version from the client jar's version.json (1.14+), if any."""
    try:
        with zipfile.ZipFile(prepared.client_jar) as jar:
            info = json.loads(jar.read("version.json"))
        return info.get("world_version")
    except (KeyError, OSError, ValueError, zipfile.BadZipFile):
        return None


def main_class_options(prepared):
    """Launch options the client's main class knows (detected from its string constants)."""
    with zipfile.ZipFile(prepared.client_jar) as jar:
        main = jar.read(prepared.main_class.replace(".", "/") + ".class")
    return {option for option in ("offlineDeveloperMode", "userType", "quickPlayMultiplayer", "server", "proxyHost")
            if b"\x00" + bytes([len(option)]) + option.encode() in main}


def offline_uuid(name):
    """The UUID an offline-mode server assigns to name (UUID v3 of 'OfflinePlayer:<name>')."""
    raw = bytearray(hashlib.md5(f"OfflinePlayer:{name}".encode()).digest())
    raw[6] = (raw[6] & 0x0F) | 0x30
    raw[8] = (raw[8] & 0x3F) | 0x80
    return uuid.UUID(bytes=bytes(raw))


def default_name():
    return f"Rift{os.getpid() % 10000:04d}{random.randrange(1000):03d}"


def command(prepared, game_dir, name, host, port, heap="1G"):
    """The java command line for one headless client run."""
    data = prepared.version_json
    agent, macos, narrator = helpers()
    natives = game_dir / "natives"  # deliberately empty: every native library is stubbed
    natives.mkdir(exist_ok=True)
    quick_play = supports_quick_play(data)
    variables = {
        "auth_player_name": name, "version_name": prepared.version, "game_directory": game_dir,
        "assets_root": CACHE / "assets", "game_assets": CACHE / "assets",
        "assets_index_name": prepared.asset_index, "auth_uuid": offline_uuid(name).hex,
        "auth_access_token": "0", "auth_session": "0", "clientid": "0", "auth_xuid": "0",
        "user_type": "legacy", "version_type": "release", "user_properties": "{}",
        "natives_directory": natives, "launcher_name": "rift-real-client", "launcher_version": "1",
        "classpath": os.pathsep.join(map(str, [narrator, *prepared.classpath])),
        "classpath_separator": os.pathsep, "library_directory": CACHE / "libraries",
        "quickPlayMultiplayer": f"{host}:{port}", "quickPlayPath": game_dir / "quickPlay.json",
    }
    features = {"is_quick_play_multiplayer": quick_play}
    jvm = [arg for arg in arguments(data, "jvm", variables) if arg != "-XstartOnFirstThread"]
    game = arguments(data, "game", variables, features)
    if "--quickPlayPath" in game:  # only meaningful together with a quick play target
        index = game.index("--quickPlayPath")
        del game[index:index + 2]
    if not quick_play:
        game += ["--server", host, "--port", str(port)]
    known = main_class_options(prepared)
    if "offlineDeveloperMode" in known:
        # 26.x has no user type: this selects the offline user API service,
        # without which chat is disabled "by profile" and never logged.
        game.append("--offlineDeveloperMode")
    # A dead SOCKS proxy for everything the client does over HTTP (auth,
    # profile keys, skins, realms, telemetry); the game connection is direct.
    game += ["--proxyHost", "127.0.0.1", "--proxyPort", "9"]
    java = find_java(prepared.java_major)
    options = [
        f"-Xmx{heap}", "-Xms256M", "-Xss2M", "-XX:+UseSerialGC",
        f"-javaagent:{agent}", f"-javaagent:{macos}",
        "-Djava.awt.headless=true", "-Dapple.awt.UIElement=true",
        f"-Dlog4j.configurationFile={game_dir / 'log4j2.xml'}", "-Dlog4j2.formatMsgNoLookups=true",
        "-Dio.netty.transport.noNative=true", "-Dfml.earlyprogresswindow=false",
        f"-Djava.io.tmpdir={game_dir / 'tmp'}",
    ] + [f"-Dminecraft.api.{service}.host={DEAD_HOST}" for service in ("auth", "account", "session", "services")]
    if prepared.java_major >= 9:
        options.append("-XX:+IgnoreUnrecognizedVMOptions")
    (game_dir / "tmp").mkdir(exist_ok=True)
    (game_dir / "log4j2.xml").write_text(LOG4J_CONFIG.format(
        pattern=LOG_PATTERN, file=game_dir / "logs" / "latest.log"))
    return [java, *dict.fromkeys(options), *jvm, prepared.main_class, *game]


# --------------------------------------------------------------------------
# Log classification

LINE = re.compile(r"^\[(?P<time>[\d:.]+)\] \[(?P<thread>[^\]]*)/(?P<level>[A-Z]+)\] \[(?P<logger>[^\]]*)\]: (?P<message>.*)$")


@dataclass
class Entry:
    level: str
    thread: str
    logger: str
    message: str
    trace: list = field(default_factory=list)
    time: str = ""

    def text(self, frames=6):
        lines = [f"[{self.thread}/{self.level}] [{self.logger}]: {self.message}"]
        lines += [line.rstrip() for line in self.trace[:frames]]
        return "\n".join(lines)

    def blob(self):
        return "\n".join([self.message, *self.trace])


def parse_log(lines):
    """Group log lines into entries; stack traces and stray stderr lines attach to the entry before."""
    entries = []
    for raw in lines:
        line = raw.rstrip("\n")
        if not line.strip():
            continue
        match = LINE.match(line)
        if match:
            entries.append(Entry(match["level"], match["thread"], match["logger"], match["message"],
                                 time=match["time"]))
        elif line.startswith(("\tat ", "\t... ", "Caused by:", "\t", "    at ")) and entries:
            entries[-1].trace.append(line.strip())
        elif line.startswith(("WARNING: ", "SLF4J")):
            continue  # JVM deprecation notices (sun.misc.Unsafe, native access) and logger bootstrap
        else:
            # Not from log4j: stderr output, crash report text, JVM messages.
            level = "ERROR" if re.search(r"Exception|Error|FATAL|Crash Report", line) else "INFO"
            entries.append(Entry(level, "stdout", "-", line.strip()))
    return entries


# Known-harmless headless/offline noise: (regex over the message, regex over
# the logger or None, note). Matching entries are counted, never fatal.
NOISE = [
    # Offline account: Mojang services are unreachable through the dead proxy.
    (r"Failed to fetch user properties|Couldn't look up profile properties|Services are unavailable"
     r"|unable to fetch uri for|Failed to verify authentication|Failed to retrieve profile key pair"
     r"|Failed to fetch Realms|Could not connect to Realms|Couldn't connect to Realms|Realms"
     r"|Failed to (fetch|get) (player )?(certificates|attributes|ban status)|Failed to fetch banned"
     r"|Failed to check (realms|player)|UserApiService|Failed to get user properties"
     r"|Could not authorize you against Realms|Failed to load Realms|Failed to fetch news"
     r"|Couldn't download profile|Failed to fetch profile|User .* not found|Failed to request yggdrasil"
     r"|Failed to retrieve (the )?(player )?(profile|skin)|Couldn't get player certificates"
     r"|Failed to (download|load) (player )?(skin|cape)|Failed to download (player )?texture"
     r"|Failed to fetch skin|Error while loading the player's profile|Failed to fetch MOTD"
     r"|telemetry|Telemetry|Failed to send event|Failed to verify signature on chat message"
     r"|Received chat packet without valid signature|Failed to validate profile key"
     r"|Couldn't fetch (profile|session)|Failed to connect$|Failed to read from https?://"
     r"|Unable to (fetch|look up) (profile|user)|Error looking up profile|Failed to look up"
     r"|Ignoring chat session from .* due to missing Services public key",
     None, "offline account / Mojang services blocked"),
    # Audio and narrator are stubbed.
    (r"OpenAL|SoundSystem|sound system|Sound engine|SoundEngine|soundDevice|Error starting SoundSystem"
     r"|Failed to (open|load|create) (sound|OpenAL|device)|Missing sound for event|Unable to play unknown soundEvent"
     r"|does not exist, cannot add it to event|Unable to play empty soundEvent|Narrator|narrator"
     r"|Error while loading the narrator|Couldn't load narrator|SoundManager|paulscode|Sound System"
     r"|Unable to load sound|sounds/.*\.ogg|Failed to set (audio|sound)|audio device|Audio device"
     r"|Failed to list (audio|sound) devices|Could not find (default )?sound device"
     r"|Unable to parse version .* to a codename", None, "audio, narrator and OS probing"),
    # Rendering and windowing are stubbed by the HeadlessMC agent.
    (r"OpenGL|GL error|GLFW|OpenGL debug|glfw|GL_|Couldn't compile|shader|Shader|framebuffer|Framebuffer"
     r"|Failed to (create|load|upload) (texture|window|icon)|Couldn't set icon|Unable to (set|load) (window )?icon"
     r"|Failed to (load|set) the icon|Failed to load icon|Couldn't load icon|Failed to (get|query) display"
     r"|Failed to determine maximum texture size|Unable to (fit|stitch)|Missing texture|missing texture"
     r"|Unable to load (font|texture|model)|Using missing texture|Failed to load texture|Texture .* not found"
     r"|Unable to find (texture|font)|Couldn't load (texture|font|model)|Failed to read image"
     r"|Unable to (read|parse) (texture|image|png)|LWJGL|lwjgl|Display|display mode|Pixel format"
     r"|Unable to create (window|display|context)|Unable to get monitor|monitor|Monitor|VideoMode"
     r"|Vulkan|vulkan|SDL|graphics device|GPU|gpu|Tracy|tracy|TracyClient|Failed to load native"
     r"|VSync|vsync|Unable to (load|read) (splash|panorama)|panorama|Minecraft\.getInstance\(\)\.window"
     r"|\[HeadlessMC\]|Unable to resize|Couldn't resize|Failed to (switch|change) (to )?fullscreen"
     r"|Requested post effect does not exist|Fullscreen|fullscreen|Failed to (initialize|init) (the )?clipboard|Clipboard|clipboard"
     r"|Could not find (a )?(suitable )?(pixel|context)|Mipmap|mipmap|Unable to load (pack )?icon"
     r"|Couldn't (get|find) (display|screen)|Screen size|screen resolution",
     None, "rendering, window and input are stubbed"),
    # Missing (deliberately not downloaded) assets and resource pack quirks.
    (r"Missing language|Couldn't (read|load) language|Failed to load translations|Failed to load language"
     r"|Unable to load language|Missing translation|missing translation|Unable to load lang"
     r"|Unable to read (language|lang)|Couldn't find lang|Language file|Skipping lang"
     r"|Failed to load (resource )?pack (metadata|icon)|Couldn't load pack|Couldn't (find|read) pack"
     r"|Pack .* (is missing|was missing)|Missing (resource )?pack|Failed to (load|read) (assets|asset index)"
     r"|Couldn't load (assets|asset index)|Unable to (load|read) resource pack|resource pack .* incompatible"
     r"|Resource reload|Reloading ResourceManager", None, "assets not downloaded / resource packs"),
    # JVM, OS and launcher environment.
    (r"Unable to (determine|get) (system|hardware|CPU|GPU|memory|processor)|oshi|OSHI|JNA|jna|Failed to (load|get) "
     r"(hardware|system) info|Unable to get (hardware|system|VRAM)|Couldn't get (hardware|system)"
     r"|Could not (determine|retrieve) (CPU|hardware)|Unable to (read|get) .*sysctl|sysctl|IOKit|Unable to read process"
     r"|Unable to determine (process|uptime|load)|Failed to (get|read) (process|uptime)|java.awt|AWT|awt"
     r"|Unable to load native|native (library|transport)|NativeLibrary|KQueue|kqueue|Epoll|epoll"
     r"|Unable to (create|write) (crash|report|screenshot)|Snooper|snooper|Failed to (send|post) (snoop|data)"
     r"|Failed to start the minecraft server|Failed to (create|delete|write) (temp|cache|directory)"
     r"|Failed to save (options|servers|hotbar|profile|cache)|Couldn't (save|write) (options|servers)"
     r"|Failed to (load|read) (options|servers\.dat|hotbar|realms persistence)|Unknown option|Skipping bad option"
     r"|Failed to parse option|Could not parse option|Unable to parse option|option .* (invalid|unknown)"
     r"|Unknown (key|setting) .*options|DataFixer|datafixer|Datafixer|optimizations took"
     r"|Unable to (load|save) (user|realms|quick ?play)|quick ?[Pp]lay log|QuickPlay|quickPlay"
     r"|Mojang Studios|Failed to (open|lock) (file|session)|session\.lock|Failed to (check|read) (latest|update)"
     r"|Exception in thread \"Timer hack thread\"|Reflections|Failed to load the (player )?(heads|skin) cache"
     r"|Unable to determine (OS|operating system) version|Failed to load (crash|jfr)|JFR|jfr"
     r"|Profiler|profiler|TickTimeTracker|Can't keep up|Running \d+ms|server overloaded|Failed to stop"
     r"|Couldn't (get|read) (current|system) (time|locale)|Locale|locale|Font|font|Glyph|glyph|Unifont|unifont",
     None, "JVM / OS / environment"),
]
NOISE = [(re.compile(message), re.compile(logger) if logger else None, note) for message, logger, note in NOISE]

# Anything that means the client could not process what it was sent.
FAILURE = re.compile(
    r"DecoderException|EncoderException|CorruptedFrameException|Internal Exception|Error decoding|"
    r"Failed to decode|Failed to encode|Received invalid packet|Bad packet|Unknown packet|bad packet id|"
    r"Packet .* (was|is) larger than|extra bytes|bytes extra|Packet too big|Failed to handle packet|"
    r"Error handling packet|Exception while handling|Error while handling|handlePacket|PacketUtils|"
    r"ClientPacketListener|ClientCommonPacketListener|ClientConfigurationPacketListener|NetHandlerPlayClient|"
    r"NetHandlerLoginClient|ClientHandshakePacketListener|PacketDecoder|PacketEncoder|PacketInflater|"
    r"PacketSplitter|Varint21|NettyCompression|CompressionDecoder|NetworkManager|net\.minecraft\.network|"
    r"io\.netty|netty|Connection reset|Lost connection|Connection lost|Disconnected|disconnected|kicked|"
    r"Timed out|timed out|registr|Registry|Unknown (registry|dimension|biome|block|item|entity|tag)|"
    r"Missing (registry|key|element)|Failed to (load|parse|sync) (registries|registry|tags|recipes|advancements|"
    r"commands|dimension)|Codec|codec|Failed to (parse|read|load) (chunk|level|world|nbt|NBT|component|text)|"
    r"chunk|Chunk|ReportedException|Crash Report|crash report|Unreported exception|Exception in server tick loop|"
    r"Failed to (read|parse) (json|JSON|text component)|JsonParseException|JsonSyntaxException|"
    r"NullPointerException|IndexOutOfBounds|ArrayIndexOutOfBounds|IllegalStateException|"
    r"IllegalArgumentException|ClassCastException|BufferUnderflow|NegativeArraySize|EOFException|"
    r"Can't (find|keep up|read)|Unable to (decode|parse|read)|Invalid|invalid|Malformed|malformed|"
    r"Unexpected|unexpected|unknown entity|Unknown entity|Skipping Entity|Skipping BlockEntity|"
    r"Received passengers for unknown|Unknown custom packet|Unknown (payload|channel)|Failed to (spawn|add) entity|"
    r"Duplicate|duplicate|out of bounds|Not a valid|not a valid|recipe|Recipe|advancement|Advancement|"
    r"Unknown tag|Failed to (load|apply) (tag|data|pack)|Couldn't (parse|decode|read|apply)")
# Threads that only do networking: any warning there is about the connection.
NETWORK_THREAD = re.compile(r"Netty|Network|IO-Worker|Server Connector|Server Pinger")

# Non-WARN lines that still decide the outcome.
CONNECTING = re.compile(r"Connecting to (?P<host>\S+?),? (?P<port>\d+)")
DISCONNECT = re.compile(
    r"Client disconnected with reason: (?P<reason>.*)|Disconnected from server(?:: (?P<reason2>.*))?|"
    r"(?P<reason3>(Outdated (server|client)|Incompatible client|multiplayer\.disconnect\.\S+|"
    r"disconnect\.\S+|Internal Exception: .*))")


def classify(entry):
    """'noise', 'failure', 'unknown' or 'ok' for one log entry.

    WARN/ERROR/FATAL entries are failures when they mention networking,
    packets, registries or data the server sent; known headless noise is
    allowlisted first; any other warning is 'unknown' (reported for triage).
    """
    level = entry.level
    if level not in ("WARN", "ERROR", "FATAL"):
        return "ok"
    blob = entry.blob()
    for message, logger, _note in NOISE:
        if message.search(entry.message) and (logger is None or logger.search(entry.logger)):
            # A stack trace through the network stack is never noise.
            if not re.search(r"net\.minecraft\.network|io\.netty|PacketListener|NetHandler|DecoderException",
                             blob) or "Mojang services" in _note or "offline" in _note:
                return "noise"
    if level == "FATAL" or FAILURE.search(blob) or NETWORK_THREAD.search(entry.thread):
        return "failure"
    return "unknown"


def noise_note(entry):
    for message, logger, note in NOISE:
        if message.search(entry.message) and (logger is None or logger.search(entry.logger)):
            return note
    return None


# --------------------------------------------------------------------------
# Running one client


@dataclass
class Result:
    version: str
    host: str
    port: int
    name: str
    ok: bool = False
    reason: str = ""
    joined: bool = False
    seconds_to_join: float | None = None
    stayed: float = 0.0
    disconnect: str | None = None
    failures: list = field(default_factory=list)
    unknown: list = field(default_factory=list)
    noise: dict = field(default_factory=dict)
    chat: list = field(default_factory=list)
    evidence: list = field(default_factory=list)
    peak_rss_mb: int = 0
    log_dir: str = ""
    client_log: str = ""
    exit_code: int | None = None

    def report(self):
        lines = [f"{'PASS' if self.ok else 'FAIL'} {self.version} -> {self.host}:{self.port} as {self.name}: "
                 f"{self.reason}"]
        if self.seconds_to_join is not None:
            lines.append(f"  joined after {self.seconds_to_join:.1f}s, stayed {self.stayed:.1f}s, "
                         f"peak RSS {self.peak_rss_mb} MB")
        if self.disconnect:
            lines.append(f"  disconnect: {self.disconnect}")
        for item in self.evidence:
            lines.append(f"  evidence: {item}")
        for failure in self.failures[:5]:
            lines.append("  failure: " + failure.replace("\n", "\n    "))
        for unknown in self.unknown[:20]:
            lines.append("  unknown warning (triage): " + unknown.replace("\n", "\n    "))
        if self.noise:
            lines.append("  allowlisted noise: " + ", ".join(f"{k} x{v}" for k, v in self.noise.items()))
        lines.append(f"  logs: {self.log_dir}")
        return "\n".join(lines)


@contextmanager
def client_slot(timeout=1800):
    """Hold one of CLIENT_SLOTS machine-wide slots (flock on locks/client-<n>)."""
    deadline = time.monotonic() + timeout
    announced = False
    while True:
        for slot in range(CLIENT_SLOTS):
            with file_lock(f"client-{slot}", blocking=False) as held:
                if held is not None:
                    yield slot
                    return
        if time.monotonic() > deadline:
            raise TimeoutError(f"no free real-client slot after {timeout}s ({LOCKS}/client-*)")
        if not announced:
            log(f"waiting for a free client slot ({CLIENT_SLOTS} in use machine-wide)")
            announced = True
        time.sleep(1)


# Kills the client's process group if the harness disappears (SIGKILL, crash)
# so that no headless game is left running in the background.
WATCHDOG = """
import os, signal, sys, time
parent, group = int(sys.argv[1]), int(sys.argv[2])
def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
while alive(group) and alive(parent) and os.getppid() != 1:
    time.sleep(1)
if alive(group):
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(group, sig)
        except ProcessLookupError:
            break
        time.sleep(3)
"""


class ClientProcess:
    def __init__(self, cmd, game_dir):
        self.game_dir = game_dir
        self.log_path = game_dir / "client.log"
        self.output = open(self.log_path, "w")
        self.proc = subprocess.Popen(cmd, cwd=game_dir, stdin=subprocess.DEVNULL, stdout=self.output,
                                     stderr=subprocess.STDOUT, start_new_session=True)
        self.watchdog = subprocess.Popen([sys.executable, "-I", "-c", WATCHDOG, str(os.getpid()),
                                          str(self.proc.pid)], stdin=subprocess.DEVNULL,
                                         start_new_session=True)
        self.reader = open(self.log_path, errors="replace")
        self.buffer = ""
        self.started = time.monotonic()

    def new_lines(self):
        self.buffer += self.reader.read()
        *lines, self.buffer = self.buffer.split("\n")
        return lines

    def alive(self):
        return self.proc.poll() is None

    def rss_mb(self):
        try:
            out = subprocess.run(["ps", "-o", "rss=", "-g", str(self.proc.pid)], capture_output=True,
                                 text=True, timeout=5).stdout
            return sum(int(value) for value in out.split()) // 1024
        except (OSError, ValueError, subprocess.SubprocessError):
            return 0

    def connected(self, host, port):
        """True/False whether the client holds an established TCP connection to port (None: unknown)."""
        try:
            out = subprocess.run(["lsof", "-nP", "-a", "-p", str(self.proc.pid), f"-iTCP:{port}",
                                  "-sTCP:ESTABLISHED"], capture_output=True, text=True, timeout=10).stdout
        except (OSError, subprocess.SubprocessError):
            return None
        return any(f":{port} (ESTABLISHED)" in line for line in out.splitlines())

    def stop(self):
        try:
            if self.alive():
                for sig, grace in ((signal.SIGTERM, 8), (signal.SIGKILL, 10)):
                    try:
                        os.killpg(self.proc.pid, sig)
                    except ProcessLookupError:
                        break
                    try:
                        self.proc.wait(timeout=grace)
                        break
                    except subprocess.TimeoutExpired:
                        continue
            try:  # stray children in the group (none expected)
                os.killpg(self.proc.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        finally:
            self.proc.wait()
            if self.watchdog.poll() is None:
                self.watchdog.kill()
            self.watchdog.wait()
            self.output.close()


def run_dir(version, log_dir=None):
    base = Path(log_dir) if log_dir else CACHE / "runs"
    stamp = time.strftime("%Y%m%d-%H%M%S")
    directory = base / f"{version}-{stamp}-{os.getpid()}-{random.randrange(1 << 16):04x}"
    (directory / "logs").mkdir(parents=True)
    return directory


def join(version, host, port, name=None, stay=20, log_dir=None, join_timeout=240, on_joined=None,
         expect_chat=(), heap="1G", strict=False, keep_game_dir=False, before_stop=None):
    """Start a headless vanilla `version` client, join host:port, stay `stay` seconds.

    on_joined(result) runs once the client is in the world (e.g. to issue
    server commands); expect_chat lists substrings that must appear in [CHAT]
    lines before the stay ends. Returns a Result; result.ok is True only when
    the client joined, stayed connected for the whole time, saw every expected
    chat line and logged no network/packet/registry errors (with strict=True,
    also no unknown warnings). Logs are kept under result.log_dir.
    """
    name = name or default_name()
    prepared = setup(version)
    game_dir = run_dir(version, log_dir)
    result = Result(version, host, int(port), name, log_dir=str(game_dir),
                    client_log=str(game_dir / "client.log"))
    write_options(game_dir, data_version(prepared))
    cmd = command(prepared, game_dir, name, host, port, heap)
    (game_dir / "command.txt").write_text(" ".join(cmd) + "\n")
    expected = list(expect_chat)
    entries, joined_at, deadline = [], None, None
    # The server's join broadcast reaches the joining client too; the
    # advancements packet is sent right after login by every vanilla server.
    join_marker = re.compile(rf"\[CHAT\] (?:\S+ )?{re.escape(name)} joined the game|Loaded \d+ advancements")
    with client_slot():
        client = ClientProcess(cmd, game_dir)
        missed_connection = 0
        last_probe = 0.0
        try:
            while True:
                now = time.monotonic()
                fresh = parse_log(client.new_lines())
                entries.extend(fresh)
                for entry in fresh:
                    kind = classify(entry)
                    if kind == "noise":
                        note = noise_note(entry)
                        result.noise[note] = result.noise.get(note, 0) + 1
                    elif kind == "failure":
                        result.failures.append(entry.text())
                    elif kind == "unknown":
                        result.unknown.append(entry.text())
                    if "[CHAT]" in entry.message:
                        chat = entry.message.split("[CHAT]", 1)[1].strip()
                        result.chat.append(chat)
                        expected = [text for text in expected if text not in chat]
                    if (match := DISCONNECT.search(entry.message)) and result.disconnect is None and (
                            entry.level != "INFO" or "disconnect" in entry.message.lower()):
                        result.disconnect = next(group for group in match.groups() if group) \
                            if any(match.groups()) else entry.message
                    if joined_at is None and join_marker.search(entry.message):
                        joined_at = now
                        result.joined = True
                        result.seconds_to_join = round(now - client.started, 1)
                        result.evidence.append(f"client: {entry.message.strip()}")
                        deadline = now + stay
                        if on_joined:
                            on_joined(result)
                if now - last_probe > 2:
                    last_probe = now
                    result.peak_rss_mb = max(result.peak_rss_mb, client.rss_mb())
                    if joined_at is not None:
                        state = client.connected(host, port)
                        missed_connection = missed_connection + 1 if state is False else 0
                        if missed_connection >= 2 and result.disconnect is None:
                            result.disconnect = "TCP connection closed"
                if not client.alive():
                    result.exit_code = client.proc.returncode
                    result.reason = f"client exited with code {result.exit_code}"
                    break
                if result.failures or result.disconnect or (strict and result.unknown):
                    # Give the client a moment to log the rest of the story.
                    time.sleep(2)
                    for entry in parse_log(client.new_lines()):
                        if classify(entry) == "failure":
                            result.failures.append(entry.text())
                        if result.disconnect is None and (match := DISCONNECT.search(entry.message)):
                            result.disconnect = entry.message
                    break
                if joined_at is None and now - client.started > join_timeout:
                    result.reason = f"did not join within {join_timeout}s"
                    break
                if deadline is not None and now >= deadline:
                    result.stayed = round(now - joined_at, 1)
                    break
                time.sleep(0.25)
            if joined_at is not None:
                result.stayed = round(time.monotonic() - joined_at, 1)
            if before_stop:
                before_stop(result)
        finally:
            client.stop()
    finish(result, entries, stay, expected, strict)
    if result.ok and not keep_game_dir:
        prune(game_dir)
    return result


def finish(result, entries, stay, expected, strict):
    if result.disconnect:
        result.reason = f"disconnected: {result.disconnect}"
    elif result.failures:
        result.reason = "client logged a network/packet error: " + result.failures[0].splitlines()[0]
    elif strict and result.unknown:
        result.reason = "unknown warning (strict): " + result.unknown[0].splitlines()[0]
    elif not result.joined:
        result.reason = result.reason or "never joined"
        connecting = [entry.message for entry in entries if CONNECTING.search(entry.message)]
        if not connecting:
            result.reason += " (no 'Connecting to' line: the client did not reach the connect step)"
    elif result.stayed + 0.5 < stay:
        result.reason = result.reason or f"left after {result.stayed}s"
    elif expected:
        result.reason = f"missing expected chat lines: {expected}"
    else:
        result.ok = True
        result.reason = f"joined and stayed {result.stayed:.0f}s without errors"
    if not result.ok and result.failures and "failure" not in result.reason:
        result.reason += " | first error: " + result.failures[0].splitlines()[0]


def prune(game_dir):
    """Drop the bulky parts of a passing run's game directory, keep logs and options."""
    for name in ("natives", "tmp", "saves", "resourcepacks", "screenshots", "downloads", "debug",
                 "assets", "config"):
        shutil.rmtree(game_dir / name, ignore_errors=True)


# --------------------------------------------------------------------------
# Self-test against pinned vanilla servers

SERVERS = json.loads((HERE / "servers.json").read_text())
SERVER_JARS = VIA / "servers"


def server_fixture(version):
    for name, fixture in SERVERS.items():
        if fixture["version"] == version and fixture["url"].startswith("https://piston-data.mojang.com/"):
            return name, fixture
    raise ValueError(f"no pinned vanilla server for {version} in tests/servers.json")


def version_key(version):
    """Sortable key: 1.20.1 -> (1, 20, 1); 26.3 -> (26, 3)."""
    return tuple(int(part) for part in re.findall(r"\d+", version)[:3])


class Server:
    """A pinned vanilla server in offline mode on a free loopback port."""

    def __init__(self, version, directory, compression=True):
        self.version = version
        self.name, fixture = server_fixture(version)
        jar = SERVER_JARS / fixture["filename"]
        with file_lock(f"server-jar-{fixture['filename']}"):
            fetch(fixture["url"], jar, fixture["algorithm"], fixture["checksum"])
        self.directory = directory
        directory.mkdir(parents=True, exist_ok=True)
        self.port = free_port()
        old = version_key(version) < (1, 13)
        properties = {
            "server-ip": "127.0.0.1", "server-port": self.port, "motd": f"rift-real-client-{version}",
            "online-mode": "false", "enforce-secure-profile": "false", "prevent-proxy-connections": "false",
            "network-compression-threshold": 256 if compression else -1, "view-distance": 3,
            "simulation-distance": 5, "gamemode": 1 if old else "creative", "difficulty": 0 if old else "peaceful",
            "level-type": "FLAT" if old else ("flat" if version_key(version) < (1, 19) else "minecraft:flat"),
            "generate-structures": "false", "spawn-protection": 0, "max-tick-time": -1,
            "spawn-monsters": "false", "spawn-animals": "false", "spawn-npcs": "false",
            "pause-when-empty-seconds": 0, "sync-chunk-writes": "false", "player-idle-timeout": 0,
            "max-players": 10, "white-list": "false", "enforce-whitelist": "false", "allow-flight": "true", "rate-limit": 0, "snooper-enabled": "false",
        }
        (directory / "eula.txt").write_text("eula=true\n")
        (directory / "server.properties").write_text("".join(f"{k}={v}\n" for k, v in properties.items()))
        java = find_java(fixture["java"])
        self.log_path = directory / "server.log"
        self.output = open(self.log_path, "w")
        self.proc = subprocess.Popen(
            [java, "-Xms256M", "-Xmx1G", "-XX:+UseSerialGC", "-XX:ActiveProcessorCount=2",
             "-Djava.awt.headless=true", "-Dlog4j2.formatMsgNoLookups=true", "-jar", str(jar), "nogui"],
            cwd=directory, stdin=subprocess.PIPE, stdout=self.output, stderr=subprocess.STDOUT, text=True,
            start_new_session=True)
        self.watchdog = subprocess.Popen([sys.executable, "-I", "-c", WATCHDOG, str(os.getpid()),
                                          str(self.proc.pid)], stdin=subprocess.DEVNULL, start_new_session=True)
        deadline = time.monotonic() + 240
        while "Done (" not in self.log():
            if self.proc.poll() is not None or time.monotonic() > deadline:
                self.stop()
                raise RuntimeError(f"server {version} did not start: {self.log_path}\n{self.log()[-3000:]}")
            time.sleep(0.2)

    def log(self):
        return self.log_path.read_text(errors="replace")

    def command(self, line):
        self.proc.stdin.write(line + "\n")
        self.proc.stdin.flush()

    def wait_for(self, pattern, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if match := re.search(pattern, self.log()):
                return match
            time.sleep(0.2)
        return None

    def stop(self):
        try:
            if self.proc.poll() is None:
                try:
                    self.command("stop")
                    self.proc.wait(timeout=30)
                except (BrokenPipeError, OSError, subprocess.TimeoutExpired):
                    os.killpg(self.proc.pid, signal.SIGKILL)
                    self.proc.wait()
        finally:
            if self.watchdog.poll() is None:
                self.watchdog.kill()
            self.watchdog.wait()
            self.output.close()


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def scenario_commands(version, name):
    """Console commands that make the server send varied play packets."""
    modern = version_key(version) >= (1, 13)
    item = "minecraft:diamond_sword" if modern else "minecraft:diamond_sword 1"
    commands = [
        f"give {name} {item}", f"give {name} minecraft:oak_log 16" if modern else f"give {name} minecraft:log 16",
        f"tp {name} 8 10 8", "time set 6000", "weather rain", "summon minecraft:pig 4 5 4",
        "summon minecraft:armor_stand 2 5 2", f"gamemode survival {name}", f"gamemode creative {name}",
        f"title {name} title {{\"text\":\"rift\"}}" if version_key(version) < (1, 21, 5)
        else f"title {name} title \"rift\"", "weather clear", f"xp add {name} 5" if modern else f"xp 5 {name}",
    ]
    if modern:
        commands.append(f"effect give {name} minecraft:speed 30 1")
    return commands


def selftest_one(version, stay, root, name=None):
    """Join a matching pinned vanilla server directly; returns (ok, summary lines)."""
    name = name or default_name()
    directory = root / f"server-{version}"
    server = Server(version, directory)
    nonce = f"rift-selftest-{random.randrange(1 << 30):08x}"
    evidence, problems = [], []
    snapshot = {}

    def on_joined(_result):
        def drive():
            if not server.wait_for(rf"{re.escape(name)} joined the game", 30):
                problems.append("server never logged the join")
                return
            time.sleep(2)
            server.command(f"say {nonce}")
            for line in scenario_commands(version, name):
                time.sleep(1)
                server.command(line)
            server.command(f"say {nonce}-after-commands")
        threading.Thread(target=drive, daemon=True).start()

    def before_stop(_result):
        snapshot["log"] = server.log()

    try:
        result = join(version, "127.0.0.1", server.port, name=name, stay=stay, log_dir=root,
                      on_joined=on_joined, expect_chat=[nonce, f"{nonce}-after-commands"],
                      before_stop=before_stop)
    finally:
        server.stop()
    server_log = snapshot.get("log") or server.log()
    if match := re.search(rf".*{re.escape(name)} joined the game.*", server_log):
        evidence.append("server: " + match.group(0).strip())
    else:
        problems.append("server log has no join line")
    for match in re.finditer(rf".*{re.escape(name)} (lost connection|left the game).*", server_log):
        problems.append("server (before the harness stopped the client): " + match.group(0).strip())
    for match in re.finditer(r".*(Exception|ERROR\]|Can't keep up).*", server_log):
        evidence.append("server note: " + match.group(0).strip()[:200])
    chat = [line for line in result.chat if nonce in line]
    evidence += [f"client: [CHAT] {line}" for line in chat]
    ok = result.ok and not problems
    lines = [result.report()] + [f"  {item}" for item in evidence] + [f"  PROBLEM: {item}" for item in problems]
    lines.append(f"  server log: {server.log_path}")
    return ok, result, lines


# --------------------------------------------------------------------------
# Negative test helper: a relay that can corrupt one clientbound play frame


def read_varint_from(buffer, offset):
    value = shift = 0
    while True:
        if offset >= len(buffer):
            return None, offset
        byte = buffer[offset]
        offset += 1
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, offset
        shift += 7
        if shift > 35:
            raise ValueError("varint too long")


def encode_varint(value):
    value &= 0xFFFFFFFF
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        out.append(byte | (0x80 if value else 0))
        if not value:
            return bytes(out)


def corrupt_frame(payload, mode):
    """Corrupt one uncompressed frame payload (packet id + body)."""
    packet_id, offset = read_varint_from(payload, 0)
    body = payload[offset:]
    if mode == "badid":
        return encode_varint(0x3FFF) + body  # no protocol has this many packets
    flipped = bytearray(body)
    for index in range(min(len(flipped), 48)):
        flipped[index] ^= 0xFF
    return payload[:offset] + bytes(flipped)


class Relay:
    """TCP relay for one client; after arm() it corrupts the next suitable clientbound frame.

    Frames are parsed by their length prefix only, so the server must run
    without compression (network-compression-threshold=-1); offline mode
    means there is no encryption.
    """

    def __init__(self, target_port, mode="badid", min_size=64):
        self.target_port = target_port
        self.mode = mode
        self.min_size = min_size
        self.armed = threading.Event()
        self.corrupted = None
        self.listener = socket.create_server(("127.0.0.1", 0))
        self.port = self.listener.getsockname()[1]
        self.sockets = []
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self):
        try:
            while True:
                client, _ = self.listener.accept()
                upstream = socket.create_connection(("127.0.0.1", self.target_port))
                self.sockets += [client, upstream]
                threading.Thread(target=self.pipe, args=(client, upstream, False), daemon=True).start()
                threading.Thread(target=self.pipe, args=(upstream, client, True), daemon=True).start()
        except OSError:
            return

    def pipe(self, source, destination, clientbound):
        pending = b""
        try:
            while data := source.recv(65536):
                if not clientbound:
                    destination.sendall(data)
                    continue
                pending += data
                out = bytearray()
                while True:
                    length, offset = read_varint_from(pending, 0)
                    if length is None or len(pending) < offset + length:
                        break
                    frame, pending = pending[offset:offset + length], pending[offset + length:]
                    if self.armed.is_set() and self.corrupted is None and length >= self.min_size:
                        packet_id, _ = read_varint_from(frame, 0)
                        frame = corrupt_frame(frame, self.mode)
                        self.corrupted = f"clientbound packet id 0x{packet_id:02x}, {length} bytes ({self.mode})"
                    out += encode_varint(len(frame)) + frame
                destination.sendall(bytes(out))
        except OSError:
            pass
        finally:
            for sock in (source, destination):
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def close(self):
        self.listener.close()
        for sock in self.sockets:
            sock.close()


def negative_mismatch(client_version, server_version, root):
    """Client of one version against a server of another: must fail with the version message."""
    server = Server(server_version, root / f"server-{server_version}-for-{client_version}")
    try:
        result = join(client_version, "127.0.0.1", server.port, stay=10, log_dir=root, join_timeout=120)
    finally:
        server.stop()
    return result, server


def negative_corrupt(version, root, mode="badid"):
    """Corrupt one clientbound play frame after join: the client must report a decoder error."""
    server = Server(version, root / f"server-{version}-corrupt", compression=False)
    relay = Relay(server.port, mode=mode)

    def on_joined(_result):
        def arm():
            time.sleep(3)
            relay.armed.set()
            server.command("say corrupt-me-" + "x" * 80)  # guarantees a large frame soon
        threading.Thread(target=arm, daemon=True).start()

    try:
        result = join(version, "127.0.0.1", relay.port, stay=25, log_dir=root, on_joined=on_joined)
    finally:
        relay.close()
        server.stop()
    return result, relay


# --------------------------------------------------------------------------
# CLI


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--accept-eula", action="store_true",
                        help="accept the Minecraft EULA (https://aka.ms/MinecraftEULA); required")
    parser.add_argument("--json", action="store_true", help="print results as JSON on stdout")
    commands = parser.add_subparsers(dest="action", required=True)
    setup_parser = commands.add_parser("setup", help="download and verify client files")
    setup_parser.add_argument("versions", nargs="+")
    join_parser = commands.add_parser("join", help="join a server with a headless client")
    join_parser.add_argument("--version", required=True)
    join_parser.add_argument("--address", required=True, help="host:port (offline-mode server or proxy)")
    join_parser.add_argument("--name", default=None, help="player name (default: unique Rift<pid><n>)")
    join_parser.add_argument("--stay", type=float, default=20, help="seconds to stay after joining (45: long mode)")
    join_parser.add_argument("--log-dir", default=None, help=f"parent directory for run logs (default {CACHE}/runs)")
    join_parser.add_argument("--join-timeout", type=float, default=240)
    join_parser.add_argument("--expect-chat", action="append", default=[],
                             help="substring that must appear in a [CHAT] line (repeatable)")
    join_parser.add_argument("--strict", action="store_true", help="also fail on unknown warnings")
    join_parser.add_argument("--heap", default="1G")
    self_parser = commands.add_parser("selftest", help="join matching pinned vanilla servers directly")
    self_parser.add_argument("--versions", nargs="+", default=["26.3", "1.21.4", "1.20.1", "1.16.5", "1.12.2"])
    self_parser.add_argument("--stay", type=float, default=40,
                             help="seconds in the world (>30 covers two keep-alive intervals)")
    self_parser.add_argument("--log-dir", default=None)
    self_parser.add_argument("--negative", action="store_true",
                             help="also run the version-mismatch and corrupted-frame negative tests")
    args = parser.parse_args(argv)
    if not args.accept_eula:
        parser.error("pass --accept-eula to accept the Minecraft EULA (https://aka.ms/MinecraftEULA)")

    def terminate(signum, _frame):
        raise SystemExit(128 + signum)
    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGHUP, terminate)

    if args.action == "setup":
        for version in args.versions:
            prepared = setup(version)
            log(f"{version}: ready (Java {prepared.java_major}, {len(prepared.classpath)} jars, "
                f"assets {prepared.asset_index})")
        helpers()
        return 0
    if args.action == "join":
        host, _, port = args.address.rpartition(":")
        result = join(args.version, host or "127.0.0.1", int(port), name=args.name, stay=args.stay,
                      log_dir=args.log_dir, join_timeout=args.join_timeout, expect_chat=args.expect_chat,
                      heap=args.heap, strict=args.strict)
        print(json.dumps(asdict(result), indent=2) if args.json else result.report())
        return 0 if result.ok else 1
    root = Path(args.log_dir) if args.log_dir else CACHE / "runs" / f"selftest-{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}"
    root.mkdir(parents=True, exist_ok=True)
    outcomes, failed = [], 0
    for version in args.versions:
        try:
            ok, result, lines = selftest_one(version, args.stay, root)
        except Exception as exc:  # report and continue with the next version
            ok, result, lines = False, None, [f"FAIL {version}: {type(exc).__name__}: {exc}"]
        failed += not ok
        outcomes.append({"version": version, "ok": ok, "result": asdict(result) if result else None,
                         "report": lines})
        if not args.json:
            print("\n".join(lines), flush=True)
    if args.negative:
        modern = args.versions[0]
        other = "1.20.1" if modern != "1.20.1" else "1.21.4"
        result, _ = negative_mismatch(modern, other, root)
        passed = not result.ok and bool(result.disconnect)
        failed += not passed
        outcomes.append({"negative": f"{modern} client -> {other} server", "detected": passed,
                         "result": asdict(result)})
        if not args.json:
            print(f"NEGATIVE version mismatch {modern} -> {other}: "
                  f"{'detected' if passed else 'NOT DETECTED'}\n" + result.report(), flush=True)
        for version in args.versions:
            result, relay = negative_corrupt(version, root)
            passed = not result.ok and relay.corrupted is not None and bool(result.failures or result.disconnect)
            failed += not passed
            outcomes.append({"negative": f"corrupted frame {version}", "detected": passed,
                             "corrupted": relay.corrupted, "result": asdict(result)})
            if not args.json:
                print(f"NEGATIVE corrupted frame {version} ({relay.corrupted}): "
                      f"{'detected' if passed else 'NOT DETECTED'}\n" + result.report(), flush=True)
    if args.json:
        print(json.dumps(outcomes, indent=2))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
