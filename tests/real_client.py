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
import zlib

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
    """Sounds and music (~370 MB) and translations are never needed by a muted English client.

    sounds.json is left out too: without it the client registers no sound
    files, instead of warning about each of thousands of missing ones.
    """
    if name.endswith(".ogg") or name in ("minecraft/sounds.json", "sounds.json"):
        return False
    if name.startswith(("minecraft/lang/", "lang/")) and not re.search(r"/en_us\.(json|lang)$", name.lower()):
        return False
    return True


# --------------------------------------------------------------------------
# Setup


SETUP_VERSION = 2  # bump when the set of prepared files changes


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
        stamp = CACHE / "meta" / "ready" / f"{version}-v{SETUP_VERSION}"
        client = data["downloads"]["client"]
        index = data["assetIndex"]
        # The client gets an index of exactly the objects downloaded here.
        headless_index = f"{index['id']}-headless"
        client_jar = CACHE / "versions" / version / f"{version}.jar"
        libs = libraries(data)
        if not stamp.exists():
            if not quiet:
                log(f"preparing {version} (client, {len(libs)} libraries, assets {data['assetIndex']['id']})")
            jobs = [(client["url"], client_jar, "sha1", client["sha1"], client["size"])]
            jobs += [(url, CACHE / "libraries" / path, "sha1" if sha1 else None, sha1, size)
                     for path, url, sha1, size in libs]
            index_json = fetch_json(index["url"], CACHE / "assets" / "indexes" / f"{index['id']}.json",
                                    index["sha1"])
            kept = {name: entry for name, entry in index_json["objects"].items() if wanted_asset(name)}
            for entry in kept.values():
                sha1 = entry["hash"]
                jobs.append((f"https://resources.download.minecraft.net/{sha1[:2]}/{sha1}",
                             CACHE / "assets" / "objects" / sha1[:2] / sha1, "sha1", sha1, entry["size"]))
            with ThreadPoolExecutor(12) as pool:
                for future in [pool.submit(fetch, *job) for job in jobs]:
                    future.result()
            filtered = dict(index_json, objects=kept)
            target = CACHE / "assets" / "indexes" / f"{headless_index}.json"
            target.with_suffix(".tmp").write_text(json.dumps(filtered))
            os.replace(target.with_suffix(".tmp"), target)
            stamp.parent.mkdir(parents=True, exist_ok=True)
            stamp.write_text(json.dumps({"files": len(jobs), "time": time.time()}))
        tool("headlessmc-lwjgl")
        find_java(java_major(data))
    return Prepared(version, java_major(data), data["mainClass"], client_jar,
                    [CACHE / "libraries" / path for path, *_ in libs] + [client_jar],
                    headless_index, data)


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
# Second agent, loaded after HeadlessMC's. It chains HeadlessMC's
# MacosMenuAgent (not its jar's Premain-Class; SDL-era clients crash in the
# native menu bar code without it) and fixes texture loading for 1.13-1.16:
# those read PNG sizes with stbi_info_from_callbacks, which the stubbed STB
# cannot answer, so every texture would be missing and block models null
# (chunk meshing then crashes). The class doing that (found by its error
# message, the name is obfuscated) gets a constructor that parses the IHDR
# chunk in Java instead. And the game's macOS integration class (MacosUtil,
# found by its use of the Objective-C bridge) gets empty void methods: its
# Dock icon call (1.19.4-1.20.x) registers the JVM as a GUI app with a Dock
# tile, and the menu/fullscreen calls have no business in a headless run.
# It uses the ASM copy shaded into HeadlessMC's jar.
HELPER_AGENT = """package rift.headless;

import io.github.headlesshq.headlessmc.lwjgl.agent.MacosMenuAgent;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.ClassReader;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.ClassWriter;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.Opcodes;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.tree.ClassNode;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.tree.FieldInsnNode;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.tree.FieldNode;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.tree.InsnList;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.tree.InsnNode;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.tree.MethodInsnNode;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.tree.MethodNode;
import io.github.headlesshq.headlessmc.lwjgl.shadow.asm.tree.VarInsnNode;
import java.io.IOException;
import java.io.InputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.List;

public final class HeadlessAgent implements ClassFileTransformer {
    private static final byte[] MARKER = "Could not read info from the PNG file".getBytes(StandardCharsets.UTF_8);
    private static final byte[] OBJC = "ca/weblite/objc/".getBytes(StandardCharsets.UTF_8);
    private static final String CONSTRUCTOR = "(Ljava/lang/String;Ljava/io/InputStream;)V";

    public static void premain(String arguments, Instrumentation instrumentation) {
        MacosMenuAgent.premain(arguments, instrumentation);
        instrumentation.addTransformer(new HeadlessAgent());
    }

    /** Width and height from a PNG's IHDR chunk. */
    public static int[] pngSize(InputStream in) throws IOException {
        byte[] header = new byte[24];
        int read = 0;
        while (read < header.length) {
            int count = in.read(header, read, header.length - read);
            if (count < 0) {
                throw new IOException("Could not read info from the PNG file: truncated");
            }
            read += count;
        }
        if ((header[0] & 0xFF) != 0x89 || header[1] != 'P' || header[2] != 'N' || header[3] != 'G') {
            throw new IOException("Could not read info from the PNG file: not a PNG");
        }
        return new int[] {integer(header, 16), integer(header, 20)};
    }

    private static int integer(byte[] data, int offset) {
        return (data[offset] & 0xFF) << 24 | (data[offset + 1] & 0xFF) << 16
            | (data[offset + 2] & 0xFF) << 8 | data[offset + 3] & 0xFF;
    }

    @Override
    public byte[] transform(ClassLoader loader, String name, Class<?> redefined, ProtectionDomain domain,
                            byte[] bytes) {
        if (name == null || loader == null || name.startsWith("org/lwjgl/") || name.startsWith("java")
                || name.startsWith("io/github/headlesshq/") || name.startsWith("ca/weblite/")
                || name.startsWith("com/mojang/text2speech/")) {
            return null;
        }
        if (contains(bytes, OBJC)) {
            return silenceMacos(name, bytes);
        }
        if (!contains(bytes, MARKER)) {
            return null;
        }
        try {
            ClassNode node = new ClassNode();
            new ClassReader(bytes).accept(node, 0);
            List<FieldNode> ints = new ArrayList<>();
            for (FieldNode field : node.fields) {
                if ("I".equals(field.desc) && (field.access & Opcodes.ACC_STATIC) == 0) {
                    ints.add(field);
                }
            }
            boolean changed = false;
            for (MethodNode method : node.methods) {
                if (!"<init>".equals(method.name) || !CONSTRUCTOR.equals(method.desc) || ints.size() != 2) {
                    continue;
                }
                InsnList code = new InsnList();
                code.add(new VarInsnNode(Opcodes.ALOAD, 0));
                code.add(new MethodInsnNode(Opcodes.INVOKESPECIAL, node.superName, "<init>", "()V", false));
                code.add(new VarInsnNode(Opcodes.ALOAD, 2));
                code.add(new MethodInsnNode(Opcodes.INVOKESTATIC, "rift/headless/HeadlessAgent", "pngSize",
                                            "(Ljava/io/InputStream;)[I", false));
                code.add(new VarInsnNode(Opcodes.ASTORE, 3));
                for (int index = 0; index < 2; index++) {
                    code.add(new VarInsnNode(Opcodes.ALOAD, 0));
                    code.add(new VarInsnNode(Opcodes.ALOAD, 3));
                    code.add(new InsnNode(index == 0 ? Opcodes.ICONST_0 : Opcodes.ICONST_1));
                    code.add(new InsnNode(Opcodes.IALOAD));
                    code.add(new FieldInsnNode(Opcodes.PUTFIELD, node.name, ints.get(index).name, "I"));
                }
                code.add(new InsnNode(Opcodes.RETURN));
                method.instructions = code;
                method.tryCatchBlocks.clear();
                method.localVariables = null;
                method.visibleLocalVariableAnnotations = null;
                method.invisibleLocalVariableAnnotations = null;
                method.maxLocals = 4;
                method.maxStack = 4;
                changed = true;
            }
            if (!changed) {
                return null;
            }
            ClassWriter writer = new ClassWriter(ClassWriter.COMPUTE_MAXS);
            node.accept(writer);
            System.out.println("[rift-headless] PNG sizes are read in Java by " + name);
            return writer.toByteArray();
        } catch (Throwable error) {
            System.out.println("[rift-headless] could not patch " + name + ": " + error);
            return null;
        }
    }

    private static byte[] silenceMacos(String name, byte[] bytes) {
        try {
            ClassNode node = new ClassNode();
            new ClassReader(bytes).accept(node, 0);
            List<String> silenced = new ArrayList<>();
            for (MethodNode method : node.methods) {
                if (method.name.startsWith("<") || !method.desc.endsWith(")V")
                        || (method.access & (Opcodes.ACC_ABSTRACT | Opcodes.ACC_NATIVE)) != 0) {
                    continue;
                }
                InsnList code = new InsnList();
                code.add(new InsnNode(Opcodes.RETURN));
                method.instructions = code;
                method.tryCatchBlocks.clear();
                method.localVariables = null;
                method.visibleLocalVariableAnnotations = null;
                method.invisibleLocalVariableAnnotations = null;
                method.maxStack = 0;
                silenced.add(method.name + method.desc);
            }
            if (silenced.isEmpty()) {
                return null;
            }
            ClassWriter writer = new ClassWriter(ClassWriter.COMPUTE_MAXS);
            node.accept(writer);
            System.out.println("[rift-headless] macOS integration disabled in " + name + ": " + silenced);
            return writer.toByteArray();
        } catch (Throwable error) {
            System.out.println("[rift-headless] could not patch " + name + ": " + error);
            return null;
        }
    }

    private static boolean contains(byte[] haystack, byte[] needle) {
        outer:
        for (int start = 0; start <= haystack.length - needle.length; start++) {
            for (int index = 0; index < needle.length; index++) {
                if (haystack[start + index] != needle[index]) {
                    continue outer;
                }
            }
            return true;
        }
        return false;
    }
}
"""
HELPERS_VERSION = 4


def helpers():
    """(HeadlessMC agent, helper agent, silent narrator jar), built once into the cache."""
    agent = tool("headlessmc-lwjgl")
    directory = CACHE / "tools" / f"helpers-{HELPERS_VERSION}"
    helper, narrator = directory / "rift-headless-agent.jar", directory / "silent-narrator.jar"
    if helper.exists() and narrator.exists():
        return agent, helper, narrator
    with file_lock("clients-helpers"):
        if helper.exists() and narrator.exists():
            return agent, helper, narrator
        directory.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=directory) as temporary:
            temporary = Path(temporary)
            sources = {
                "rift/headless/HeadlessAgent.java": HELPER_AGENT,
                "com/mojang/text2speech/Narrator.java": NARRATOR_PLACEHOLDER,
                "com/mojang/text2speech/NarratorOSX.java": SILENT_NARRATOR.format(name="NarratorOSX"),
                "com/mojang/text2speech/NarratorMac.java": SILENT_NARRATOR.format(name="NarratorMac"),
            }
            for path, text in sources.items():
                (temporary / "src" / path).parent.mkdir(parents=True, exist_ok=True)
                (temporary / "src" / path).write_text(text)
            classes = temporary / "classes"
            javac = find_javac()
            release = ["-source", "8", "-target", "8"] if "jdk8" in javac else ["--release", "8"]
            compiled = subprocess.run([javac, *release, "-nowarn", "-cp", str(agent), "-d", str(classes),
                                       *(str(temporary / "src" / path) for path in sources)],
                                      capture_output=True, text=True)
            if compiled.returncode:
                raise RuntimeError(f"javac failed:\n{compiled.stdout}{compiled.stderr}")
            with zipfile.ZipFile(temporary / "helper.jar", "w") as jar:
                jar.writestr("META-INF/MANIFEST.MF", (
                    "Manifest-Version: 1.0\r\nPremain-Class: rift.headless.HeadlessAgent\r\n"
                    f"Class-Path: {os.path.relpath(agent, directory)}\r\n\r\n"))
                jar.write(classes / "rift/headless/HeadlessAgent.class", "rift/headless/HeadlessAgent.class")
            with zipfile.ZipFile(temporary / "narrator.jar", "w") as jar:
                for name in ("NarratorOSX", "NarratorMac"):
                    jar.write(classes / f"com/mojang/text2speech/{name}.class",
                              f"com/mojang/text2speech/{name}.class")
            os.replace(temporary / "helper.jar", helper)
            os.replace(temporary / "narrator.jar", narrator)
    return agent, helper, narrator


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

# Old clients log why the connection broke ("Internal Exception: ...
# DecoderException") only at DEBUG: the root logger runs at DEBUG and a
# configuration-level filter passes INFO+ plus those few DEBUG messages.
LOG4J_CONFIG = """<?xml version="1.0" encoding="UTF-8"?>
<Configuration status="WARN">
  <Filters>
    <ThresholdFilter level="info" onMatch="ACCEPT" onMismatch="NEUTRAL"/>
    <RegexFilter regex="{debug}" onMatch="ACCEPT" onMismatch="DENY"/>
  </Filters>
  <Appenders>
    <Console name="SysOut" target="SYSTEM_OUT"><PatternLayout pattern="{pattern}"/></Console>
    <File name="File" fileName="{file}" append="false"><PatternLayout pattern="{pattern}"/></File>
  </Appenders>
  <Loggers>
    <Root level="debug">
      <filters><MarkerFilter marker="NETWORK_PACKETS" onMatch="DENY" onMismatch="NEUTRAL"/></filters>
      <AppenderRef ref="SysOut"/><AppenderRef ref="File"/>
    </Root>
  </Loggers>
</Configuration>
"""
DEBUG_PASS = (r"(?s).*(Internal Exception|DecoderException|EncoderException|Failed to (sent|send|handle|decode) packet"
              r"|Error (sending|receiving|handling) packet|Packet processing error|Network Protocol Error"
              r"|disconnect\.|Disconnected|Connection reset|Timed out|Bad packet).*")
# {nolookups} keeps old clients' log4j 2.8 from evaluating JNDI lookups.
LOG_PATTERN = "[%d{HH:mm:ss.SSS}] [%t/%level] [%logger{1}]: %msg{nolookups}%n"


def xml_attribute(text):
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")


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
    agent, helper, narrator = helpers()
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
        f"-javaagent:{agent}", f"-javaagent:{helper}",
        "-Djava.awt.headless=true", "-Dapple.awt.UIElement=true",
        f"-Dlog4j.configurationFile={game_dir / 'log4j2.xml'}", "-Dlog4j2.formatMsgNoLookups=true",
        "-Dio.netty.transport.noNative=true", "-Dfml.earlyprogresswindow=false",
        f"-Djava.io.tmpdir={game_dir / 'tmp'}",
    ] + [f"-Dminecraft.api.{service}.host={DEAD_HOST}" for service in ("auth", "account", "session", "services")]
    if prepared.java_major >= 9:
        options.append("-XX:+IgnoreUnrecognizedVMOptions")
    (game_dir / "tmp").mkdir(exist_ok=True)
    (game_dir / "log4j2.xml").write_text(LOG4J_CONFIG.format(
        pattern=LOG_PATTERN, file=game_dir / "logs" / "latest.log", debug=xml_attribute(DEBUG_PASS)))
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


# "com.example.FooException: message" right after a log entry is its trace.
EXCEPTION_HEADER = re.compile(r"^(?:[a-z_$][\w$]*\.)+[\w$]*(?:Exception|Error|Throwable)\b")


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
        elif entries and (line.startswith(("\tat ", "\t... ", "Caused by:", "\t", "    at "))
                          or EXCEPTION_HEADER.match(line)):
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
     r"|Ignoring chat session from .* due to missing Services public key"
     r"|AuthenticationUnavailableException|Cannot contact authentication server|InvalidCredentialsException"
     r"|MinecraftClientException|authlib",
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
     r"|Requested post effect does not exist|Not setting icon|Fullscreen|fullscreen|Failed to (initialize|init) (the )?clipboard|Clipboard|clipboard"
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
     r"|Error parsing option value|Unknown (key|setting) .*options|DataFixer|datafixer|Datafixer|optimizations took"
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
    r"chunk|Chunk|ReportedException|Reported exception|Game crashed|Crash Report|crash report|Unreported exception|Exception in server tick loop|"
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
    keepalives: list = field(default_factory=list)
    corrupted: str | None = None

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


def keep_awake(pid):
    """macOS idle sleep freezes clients and servers mid-test (seen on battery):
    hold a user-level no-idle-sleep assertion until `pid` exits."""
    if sys.platform == "darwin" and shutil.which("caffeinate"):
        subprocess.Popen(["caffeinate", "-i", "-w", str(pid)], stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)


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
        keep_awake(self.proc.pid)
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

    def gui_presence(self):
        """macOS: the LaunchServices entry of the client, if it registered as an app (it must not)."""
        if sys.platform != "darwin" or not shutil.which("lsappinfo"):
            return None
        try:
            out = subprocess.run(["lsappinfo", "find", f"pid={self.proc.pid}"], capture_output=True,
                                 text=True, timeout=10).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return None
        return out

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


# Old clients started with --server connect while the first resource reload
# is still running and then mesh chunks without baked models (1.16.5 crashes
# with "Tesselating block in world"). The relay holds such connections until
# the reload has created its texture atlases and gone quiet.
RELOAD_ATLAS = re.compile(r"Created: \d+x\d+\S* .*atlas")


def needs_gate(prepared):
    return not supports_quick_play(prepared.version_json) and version_key(prepared.version) >= (1, 13)


def join(version, host, port, name=None, stay=20, log_dir=None, join_timeout=240, on_joined=None,
         expect_chat=(), heap="1G", strict=False, keep_game_dir=False, before_stop=None, relay=True,
         corrupt=None, corrupt_after=3.0):
    """Start a headless vanilla `version` client, join host:port, stay `stay` seconds.

    The client connects through a local observing Relay (see there; pass
    relay=False to connect directly, which loses packet-level evidence).
    on_joined(result) runs once the client is in the world (e.g. to issue
    server commands); expect_chat lists substrings that must appear in
    [CHAT] lines before the stay ends; corrupt ("badid" or "flip") corrupts
    one clientbound play packet corrupt_after seconds after joining (negative
    tests). Returns a Result: ok only when the client joined, stayed
    connected for the whole time, saw every expected chat line and logged no
    network/packet/registry errors (strict=True: no unknown warnings either).
    Logs are kept under result.log_dir.
    """
    name = name or default_name()
    prepared = setup(version)
    game_dir = run_dir(version, log_dir)
    result = Result(version, host, int(port), name, log_dir=str(game_dir),
                    client_log=str(game_dir / "client.log"))
    observer = Relay(host, port, corrupt=corrupt) if relay or corrupt else None
    gate = observer is not None and needs_gate(prepared)
    if observer is not None and not gate:
        observer.ready.set()
    connect = ("127.0.0.1", observer.port) if observer else (host, int(port))
    write_options(game_dir, data_version(prepared))
    cmd = command(prepared, game_dir, name, *connect, heap)
    (game_dir / "command.txt").write_text(" ".join(cmd) + "\n")
    expected = list(expect_chat)
    entries, joined_at, deadline, last_atlas = [], None, None, None
    # Fallbacks without a relay: the server's join broadcast (1.13+ clients
    # receive their own) and the advancements packet right after login.
    join_marker = re.compile(rf"\[CHAT\] (?:\S+ )?{re.escape(name)} joined the game|Loaded \d+ advancements")

    def joined(now, evidence):
        nonlocal joined_at, deadline
        joined_at = now
        result.joined = True
        result.seconds_to_join = round(now - client.started, 1)
        result.evidence.append(evidence)
        deadline = now + stay
        if on_joined:
            on_joined(result)

    try:
        with client_slot():
            client = ClientProcess(cmd, game_dir)
            missed_connection, last_probe = 0, 0.0
            try:
                while True:
                    now = time.monotonic()
                    fresh = parse_log(client.new_lines())
                    entries.extend(fresh)
                    for entry in fresh:
                        record(result, entry)
                        if "[CHAT]" in entry.message:
                            chat = entry.message.split("[CHAT]", 1)[1].strip()
                            expected = [text for text in expected if text not in chat]
                        if RELOAD_ATLAS.search(entry.message):
                            last_atlas = now
                        if joined_at is None and observer is None and join_marker.search(entry.message):
                            joined(now, f"client log: {entry.message.strip()}")
                    if gate and not observer.ready.is_set() and (
                            (last_atlas and now - last_atlas > 2) or now - client.started > 120):
                        observer.ready.set()
                        result.evidence.append(f"relay held the connection until resources were loaded "
                                               f"({now - client.started:.1f}s)")
                    if observer is not None:
                        if joined_at is None and observer.joined_at is not None:
                            joined(now, f"relay: Join Game packet (protocol {observer.protocol}) "
                                        f"{observer.joined_at - client.started:.1f}s after start")
                        if corrupt and joined_at is not None and now - joined_at >= corrupt_after:
                            observer.armed.set()
                        if observer.disconnect and result.disconnect is None:
                            result.disconnect = observer.disconnect
                        if observer.closed_at is not None and result.disconnect is None:
                            result.disconnect = (f"connection closed by the {observer.closed_by}"
                                                 + ("" if joined_at else " before joining"))
                    if now - last_probe > 2:
                        last_probe = now
                        result.peak_rss_mb = max(result.peak_rss_mb, client.rss_mb())
                        if observer is None and joined_at is not None:
                            state = client.connected(host, port)
                            missed_connection = missed_connection + 1 if state is False else 0
                            if missed_connection >= 2 and result.disconnect is None:
                                result.disconnect = "TCP connection closed"
                    if not client.alive():
                        result.exit_code = client.proc.returncode
                        result.reason = f"client exited with code {result.exit_code}"
                        break
                    if result.failures or result.disconnect or (strict and result.unknown):
                        time.sleep(2)  # let the client log the rest of the story
                        for entry in parse_log(client.new_lines()):
                            entries.append(entry)
                            record(result, entry)
                        break
                    if joined_at is None and now - client.started > join_timeout:
                        result.reason = f"did not join within {join_timeout:.0f}s"
                        break
                    if deadline is not None and now >= deadline:
                        break
                    time.sleep(0.25)
                if joined_at is not None:
                    result.stayed = round(time.monotonic() - joined_at, 1)
                presence = client.gui_presence() if client.alive() else None
                if presence:
                    result.failures.append(f"[harness/ERROR] the client registered as a GUI app: {presence}")
                elif presence == "":
                    result.evidence.append("macOS: the client never registered as a GUI app (lsappinfo)")
                if before_stop:
                    before_stop(result)
            finally:
                client.stop()
    finally:
        if observer is not None:
            observer.close()
    if observer is not None:
        if observer.keepalives:
            result.evidence.append(f"relay: client echoed {observer.answered} of {observer.keepalives} "
                                   f"keep-alives while in the world")
        result.keepalives = [observer.keepalives, observer.answered]
        result.corrupted = observer.corrupted
        result.evidence += [f"relay: {error}" for error in observer.errors]
    finish(result, entries, stay, expected, strict)
    if result.ok and not keep_game_dir:
        prune(game_dir)
    return result


def record(result, entry):
    kind = classify(entry)
    if kind == "noise":
        note = noise_note(entry)
        result.noise[note] = result.noise.get(note, 0) + 1
    elif kind == "failure":
        result.failures.append(entry.text())
    elif kind == "unknown":
        result.unknown.append(entry.text())
    if "[CHAT]" in entry.message:
        result.chat.append(entry.message.split("[CHAT]", 1)[1].strip())
    if entry.level == "DEBUG":
        # Only connection failures pass the DEBUG filter; trust them only from
        # the network stack, where old clients report why they disconnect.
        if "Internal Exception" not in entry.message and not NETWORK_THREAD.search(entry.thread):
            return
        result.failures.append(entry.text())
    if result.disconnect is None and entry.level != "INFO" and (match := DISCONNECT.search(entry.message)):
        result.disconnect = "client: " + next((group for group in match.groups() if group), entry.message)


def headline(text):
    """First line of a failure, plus its exception line when the message alone says little."""
    lines = text.splitlines()
    if len(lines) > 1 and not lines[1].startswith("at ") and EXCEPTION_HEADER.match(lines[1]):
        return f"{lines[0]} :: {lines[1]}"
    return lines[0]


def finish(result, entries, stay, expected, strict):
    if result.disconnect:
        result.reason = f"disconnected: {result.disconnect}"
    elif result.failures:
        result.reason = "client logged a network/packet error: " + headline(result.failures[0])
    elif strict and result.unknown:
        result.reason = "unknown warning (strict): " + result.unknown[0].splitlines()[0]
    elif not result.joined:
        result.reason = result.reason or "never joined"
        if not any(CONNECTING.search(entry.message) for entry in entries):
            result.reason += " (no 'Connecting to' line: the client did not reach the connect step)"
    elif result.stayed + 0.5 < stay:
        result.reason = result.reason or f"left after {result.stayed}s"
    elif expected:
        result.reason = f"missing expected chat lines: {expected}"
    elif result.keepalives and result.keepalives[0] and result.keepalives[1] + 1 < result.keepalives[0]:
        result.reason = f"client echoed only {result.keepalives[1]} of {result.keepalives[0]} keep-alives"
    else:
        result.ok = True
        result.reason = f"joined and stayed {result.stayed:.0f}s without errors"
    if not result.ok and result.failures and "network/packet error" not in result.reason:
        result.reason += " | first client error: " + headline(result.failures[0])


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
        keep_awake(self.proc.pid)
        deadline = time.monotonic() + 600  # generous: shared machines can be heavily loaded
        while "Done (" not in self.log():
            if self.proc.poll() is not None or time.monotonic() > deadline:
                self.stop()
                raise RuntimeError(f"server {version} did not start: {self.log_path}\n{self.log()[-3000:]}")
            time.sleep(0.2)

    def log(self):
        return self.log_path.read_text(errors="replace")

    def command(self, line):
        try:
            self.proc.stdin.write(line + "\n")
            self.proc.stdin.flush()
        except (BrokenPipeError, OSError, ValueError):
            pass  # the server is gone; the outcome checks report why

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
            try:
                self.proc.stdin.close()
            except OSError:
                pass


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
    joined_at = server_log.find(f"{name} joined the game")
    for match in re.finditer(r".*(Exception|ERROR\]|Can't keep up).*", server_log[max(joined_at, 0):]):
        evidence.append("server note: " + match.group(0).strip()[:200])
    chat = [line for line in result.chat if nonce in line]
    evidence += [f"client: [CHAT] {line}" for line in chat]
    ok = result.ok and not problems
    lines = [result.report()] + [f"  {item}" for item in evidence] + [f"  PROBLEM: {item}" for item in problems]
    lines.append(f"  server log: {server.log_path}")
    return ok, result, lines


# --------------------------------------------------------------------------
# The observing relay between client and target


def read_varint_from(buffer, offset):
    """(value, next offset), or (None, offset) when the buffer ends inside the varint."""
    start = offset
    value = shift = 0
    while True:
        if offset >= len(buffer):
            return None, start
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


def encode_string(text):
    raw = text.encode()
    return encode_varint(len(raw)) + raw


def read_nbt(data, offset, tag):
    """Network NBT payload of `tag` -> (python value, next offset)."""
    def take(count):
        nonlocal offset
        chunk = data[offset:offset + count]
        offset += count
        return chunk
    if tag == 1:
        return int.from_bytes(take(1), "big", signed=True), offset
    if tag == 2:
        return int.from_bytes(take(2), "big", signed=True), offset
    if tag == 3:
        return int.from_bytes(take(4), "big", signed=True), offset
    if tag == 4:
        return int.from_bytes(take(8), "big", signed=True), offset
    if tag in (5, 6):
        take(4 if tag == 5 else 8)
        return 0, offset
    if tag in (7, 11, 12):
        count = int.from_bytes(take(4), "big", signed=True)
        take(count * {7: 1, 11: 4, 12: 8}[tag])
        return [], offset
    if tag == 8:
        length = int.from_bytes(take(2), "big")
        return take(length).decode("utf-8", "replace"), offset
    if tag == 9:
        kind = take(1)[0]
        count = int.from_bytes(take(4), "big", signed=True)
        items = []
        for _ in range(max(count, 0)):
            item, offset = read_nbt(data, offset, kind)
            items.append(item)
        return items, offset
    if tag == 10:
        result = {}
        while (kind := take(1)[0]) != 0:
            name_length = int.from_bytes(take(2), "big")
            name = take(name_length).decode("utf-8", "replace")
            result[name], offset = read_nbt(data, offset, kind)
        return result, offset
    raise ValueError(f"bad NBT tag {tag}")


# English text of the vanilla disconnect keys a harness user is likely to meet.
TRANSLATIONS = {
    "multiplayer.disconnect.incompatible": "Incompatible client! Please use %s",
    "multiplayer.disconnect.outdated_client": "Incompatible client! Please use %s",
    "multiplayer.disconnect.outdated_server": "Incompatible client! Please use %s",
    "disconnect.timeout": "Timed out",
    "disconnect.genericReason": "%s",
    "multiplayer.disconnect.kicked": "Kicked by an operator",
    "multiplayer.disconnect.not_whitelisted": "You are not white-listed on this server!",
    "multiplayer.disconnect.server_full": "The server is full!",
    "multiplayer.disconnect.duplicate_login": "You logged in from another location",
    "multiplayer.disconnect.invalid_packet": "Server sent an invalid packet",
    "multiplayer.disconnect.packet_processing_error": "Packet processing error",
    "disconnect.packetError": "Network Protocol Error",
}


def component_text(value):
    """Flatten a chat component (JSON-decoded or NBT-decoded) to plain text."""
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "".join(component_text(item) for item in value)
    if not isinstance(value, dict):
        return str(value)
    text = value.get("text", "")
    if "translate" in value:
        arguments = [component_text(item) for item in value.get("with", [])]
        template = TRANSLATIONS.get(value["translate"])
        if template and template.count("%s") == len(arguments):
            text += template % tuple(arguments) + f" ({value['translate']})"
        else:
            text += value["translate"] + (f" {arguments}" if arguments else "")
    return text + "".join(component_text(item) for item in value.get("extra", []))


def decode_reason(body, protocol, nbt):
    """Disconnect packet body -> readable reason (JSON string or, since 1.20.3, NBT)."""
    try:
        if nbt:
            value, _ = read_nbt(body, 1, body[0])
            return component_text(value)
        length, offset = read_varint_from(body, 0)
        raw = body[offset:offset + length].decode("utf-8", "replace")
        try:
            return component_text(json.loads(raw))
        except ValueError:
            return raw
    except (ValueError, IndexError, TypeError):
        return body[:200].hex()


def protocol_table(protocol):
    """Play packet IDs for `protocol` from tests/minecraft.py, or None for unknown protocols."""
    try:
        sys.path.insert(0, str(HERE))
        from minecraft import PROTOCOLS
    except Exception:
        return None
    return PROTOCOLS.get(protocol)


def corrupt_payload(payload, mode):
    """Corrupt one decoded packet (id + body)."""
    packet_id, offset = read_varint_from(payload, 0)
    body = payload[offset:]
    if mode == "badid":
        return encode_varint(0x3FFF) + body  # no protocol has that many packets
    flipped = bytearray(body)
    for index in range(min(len(flipped), 48)):
        flipped[index] ^= 0xFF
    return payload[:offset] + bytes(flipped)


class Relay:
    """Loopback relay between the client and the target that watches the protocol.

    Bytes pass through unchanged except: the handshake's server address and
    port are rewritten to the real target (so the backend sees exactly what a
    direct connection would send), and, in negative tests, one clientbound
    play packet is corrupted after arm(). It follows compression and login
    state to report the Join Game packet, keep-alives echoed by the client and
    disconnect reasons. Online-mode (encrypted) sessions cannot be observed.
    The connection is accepted at once but only forwarded once `ready` is
    set, which lets old clients finish loading resources before they join.
    """

    def __init__(self, host, port, rewrite_handshake=True, corrupt=None, min_corrupt_size=64):
        self.target = (host, int(port))
        self.rewrite_handshake = rewrite_handshake
        self.corrupt = corrupt
        self.min_corrupt_size = min_corrupt_size
        self.ready = threading.Event()
        self.armed = threading.Event()
        self.lock = threading.Lock()
        self.protocol = None
        self.state = None
        self.joined_at = None
        self.closed_at = None
        self.closed_by = None
        self.disconnect = None
        self.corrupted = None
        self.keepalives = 0
        self.answered = 0
        self.pending = []
        self.errors = []
        self.connections = 0
        self.listener = socket.create_server(("127.0.0.1", 0))
        self.port = self.listener.getsockname()[1]
        self.sockets = []
        threading.Thread(target=self.accept, daemon=True).start()

    # -- plumbing

    def accept(self):
        try:
            while True:
                client, _ = self.listener.accept()
                self.sockets.append(client)
                threading.Thread(target=self.serve, args=(client,), daemon=True).start()
        except OSError:
            return

    def serve(self, client):
        self.ready.wait(300)
        try:
            upstream = socket.create_connection(self.target, timeout=15)
        except OSError as exc:
            self.disconnect = self.disconnect or f"relay could not reach {self.target[0]}:{self.target[1]}: {exc}"
            client.close()
            return
        upstream.settimeout(None)
        self.sockets.append(upstream)
        self.connections += 1
        session = {"compression": -1, "state": "handshake", "raw": False}
        threading.Thread(target=self.pump, args=(client, upstream, False, session), daemon=True).start()
        self.pump(upstream, client, True, session)

    def pump(self, source, destination, clientbound, session):
        pending = b""
        try:
            while data := source.recv(65536):
                if session["raw"]:
                    destination.sendall(data)
                    continue
                pending += data
                out = bytearray()
                while pending:
                    if not clientbound and session["state"] == "handshake" and pending[0] == 0xFE:
                        session["raw"] = True  # legacy server list ping
                        break
                    length, offset = read_varint_from(pending, 0)
                    if length is None or len(pending) < offset + length:
                        break
                    raw, frame = pending[:offset + length], pending[offset:offset + length]
                    pending = pending[offset + length:]
                    try:
                        out += self.frame(frame, raw, clientbound, session)
                    except Exception as exc:  # never let observation break the stream
                        self.errors.append(f"observer lost sync: {type(exc).__name__}: {exc}")
                        session["raw"] = True
                        out += raw
                if session["raw"]:
                    out += pending
                    pending = b""
                destination.sendall(bytes(out))
        except OSError:
            pass
        finally:
            with self.lock:
                if self.closed_at is None and self.state is not None:
                    self.closed_at = time.monotonic()
                    self.closed_by = "server" if clientbound else "client"
            for sock in (source, destination):
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    # -- protocol

    def decode(self, frame, session):
        if session["compression"] < 0:
            return frame
        size, offset = read_varint_from(frame, 0)
        return frame[offset:] if size == 0 else zlib.decompress(frame[offset:])

    def encode(self, payload, session):
        if session["compression"] < 0:
            body = payload
        elif len(payload) >= session["compression"]:
            body = encode_varint(len(payload)) + zlib.compress(payload)
        else:
            body = encode_varint(0) + payload
        return encode_varint(len(body)) + body

    def frame(self, frame, raw, clientbound, session):
        state = session["state"]
        if state == "encrypted":
            session["raw"] = True
            return raw
        payload = self.decode(frame, session)
        packet_id, offset = read_varint_from(payload, 0)
        body = payload[offset:]
        if not clientbound:
            if state == "handshake":
                return self.handshake(body, raw, session)
            if self.joined_at is not None and body in self.pending:
                self.pending.remove(body)
                self.answered += 1
            return raw
        if state == "login":
            if packet_id == 0x03:
                session["compression"], _ = read_varint_from(body, 0)
            elif packet_id == 0x02:
                session["state"] = self.state = "configuration" if self.protocol >= 764 else "play"
            elif packet_id == 0x01:
                session["state"] = self.state = "encrypted"
            elif packet_id == 0x00:
                self.disconnect = self.disconnect or "login: " + decode_reason(body, self.protocol, False)
            return raw
        table = protocol_table(self.protocol) or {}
        if packet_id == table.get("join") and self.joined_at is None:
            session["state"] = self.state = "play"
            self.joined_at = time.monotonic()
        elif state == "configuration" and packet_id == (0x01 if self.protocol < 766 else 0x02):
            self.disconnect = self.disconnect or "configuration: " + decode_reason(body, self.protocol, True)
        elif self.state == "play" and packet_id == table.get("disconnect"):
            self.disconnect = self.disconnect or "play: " + decode_reason(body, self.protocol, self.protocol >= 765)
        elif self.state == "play" and packet_id == table.get("keepalive"):
            self.keepalives += 1
            self.pending = (self.pending + [body])[-16:]
        if (self.corrupt and self.armed.is_set() and self.corrupted is None and self.state == "play"
                and len(payload) >= self.min_corrupt_size):
            self.corrupted = f"clientbound play packet 0x{packet_id:02x}, {len(payload)} bytes ({self.corrupt})"
            return self.encode(corrupt_payload(payload, self.corrupt), session)
        return raw

    def handshake(self, body, raw, session):
        protocol, offset = read_varint_from(body, 0)
        length, offset = read_varint_from(body, offset)
        host = body[offset:offset + length].decode("utf-8", "replace")
        offset += length + 2
        intent, _ = read_varint_from(body, offset)
        self.protocol = protocol
        session["state"] = "status" if intent == 1 else "login"
        if session["state"] == "login":
            self.state = "login"
        if session["state"] == "status":
            session["raw"] = True
        if not self.rewrite_handshake:
            return raw
        # Keep anything after a NUL (e.g. forwarding data) as the client sent it.
        suffix = host[host.index("\0"):] if "\0" in host else ""
        payload = (encode_varint(0x00) + encode_varint(protocol) + encode_string(self.target[0] + suffix)
                   + self.target[1].to_bytes(2, "big") + encode_varint(intent))
        return encode_varint(len(payload)) + payload

    def close(self):
        self.listener.close()
        for sock in self.sockets:
            try:
                sock.close()
            except OSError:
                pass


def negative_mismatch(client_version, server_version, root):
    """Client of one version against a server of another: must fail with the version message."""
    server = Server(server_version, root / f"server-{server_version}-for-{client_version}")
    try:
        result = join(client_version, "127.0.0.1", server.port, stay=10, log_dir=root, join_timeout=120)
    finally:
        server.stop()
    return result, server


def negative_corrupt(version, root, mode="badid"):
    """Corrupt one clientbound play packet after join: the client must notice."""
    server = Server(version, root / f"server-{version}-corrupt-{mode}")

    def on_joined(_result):
        def chat():
            for _ in range(6):  # large frames to corrupt, whatever else is quiet
                time.sleep(1)
                server.command("say corrupt-me-" + "x" * 120)
        threading.Thread(target=chat, daemon=True).start()

    try:
        result = join(version, "127.0.0.1", server.port, stay=25, log_dir=root, on_joined=on_joined,
                      corrupt=mode, corrupt_after=2.0)
    finally:
        server.stop()
    return result


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
                             help="also run the version-mismatch and corrupted-packet negative tests")
    self_parser.add_argument("--mode", choices=("badid", "flip"), default="badid",
                             help="corruption: replace the packet id, or invert the first 48 body bytes")
    negative_parser = commands.add_parser("negative", help="only the negative tests (they must fail)")
    negative_parser.add_argument("--versions", nargs="+", default=["26.3", "1.21.4", "1.20.1", "1.16.5", "1.12.2"])
    negative_parser.add_argument("--mode", choices=("badid", "flip"), default="badid")
    negative_parser.add_argument("--no-mismatch", action="store_true", help="skip the version-mismatch run")
    negative_parser.add_argument("--log-dir", default=None)
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
    root = Path(args.log_dir) if args.log_dir else \
        CACHE / "runs" / f"{args.action}-{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}"
    root.mkdir(parents=True, exist_ok=True)
    outcomes, failed = [], 0
    for version in args.versions if args.action == "selftest" else ():
        try:
            ok, result, lines = selftest_one(version, args.stay, root)
        except Exception as exc:  # report and continue with the next version
            ok, result, lines = False, None, [f"FAIL {version}: {type(exc).__name__}: {exc}"]
        failed += not ok
        outcomes.append({"version": version, "ok": ok, "result": asdict(result) if result else None,
                         "report": lines})
        if not args.json:
            print("\n".join(lines), flush=True)
    if args.action == "negative" or args.negative:
        failed += negatives(args, root, outcomes)
    if args.json:
        print(json.dumps(outcomes, indent=2))
    return 1 if failed else 0


def negatives(args, root, outcomes):
    """Run the negative tests; returns how many were NOT detected."""
    failed = 0
    if not getattr(args, "no_mismatch", False):
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
        result = negative_corrupt(version, root, args.mode)
        passed = not result.ok and result.corrupted is not None and bool(result.failures or result.disconnect)
        failed += not passed
        outcomes.append({"negative": f"corrupted packet {version}", "detected": passed,
                         "corrupted": result.corrupted, "result": asdict(result)})
        if not args.json:
            print(f"NEGATIVE corrupted packet {version} ({result.corrupted}): "
                  f"{'detected' if passed else 'NOT DETECTED'}\n" + result.report(), flush=True)
    return failed


if __name__ == "__main__":
    sys.exit(main())
