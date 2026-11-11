"""Server fixtures for the translation matrix: vanilla, Paper, Spigot and
Pumpkin, prepared once in the shared cache and started in fresh directories.

Fixtures live in tests/servers.json. Vanilla and Paper jars and Pumpkin
binaries are downloaded by URL and verified by checksum. Paper's paperclip is
patched once per build (`-Dpaperclip.patchonly=true`) so servers start without
downloading or patching again. Spigot cannot be downloaded: entries with
`"build": "buildtools"` are built lazily with BuildTools (`--rev <version>`)
under ~/.cache/rift-via/spigot/<version>/, and build-info.json there records
the BuildTools checksum, the Spigot refs and the resulting jar's checksum.
"""

import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tempfile
import time
import urllib.request
import zipfile

import matrix_env as env
from minecraft import ROOT, SERVERS, pumpkin_config, status

# Every protocol translation connects, with the newest release speaking it.
RELEASES = [(int(protocol), name) for protocol, name in re.findall(
    r'\((\d+), "([^"]+)"\)', (ROOT / "crates" / "rift-translation" / "src" / "version.rs").read_text())]
PROTOCOL_OF = {name: protocol for protocol, name in RELEASES}
RELEASE_OF = dict(RELEASES)


def software(name):
    return name.split("-")[0]


def resolve(name):
    """The servers.json key for a fixture name such as vanilla-1.21.11,
    paper-26.3, spigot-1.12.2 or pumpkin (the build for this platform)."""
    if name == "pumpkin" or name.startswith("pumpkin-"):
        if platform.system() == "Darwin" and platform.machine() == "arm64":
            return "pumpkin-macos-arm64"
        if platform.system() == "Linux" and platform.machine() == "x86_64":
            return "pumpkin"
        raise SystemExit("no Pumpkin fixture for this platform (Linux x86_64 or macOS arm64)")
    if name in SERVERS:
        return name
    kind, _, version = name.partition("-")
    for key, fixture in SERVERS.items():
        if software(key) == kind and fixture["version"].split("-")[0] == version:
            return key
    raise SystemExit(f"unknown server fixture {name!r}; see tests/servers.json")


def vanilla_for(protocol):
    """The vanilla fixture for the newest release speaking `protocol`."""
    return resolve(f"vanilla-{RELEASE_OF[protocol]}")


def jar(name):
    """The fixture's verified artifact in the shared cache, built if needed."""
    fixture = SERVERS[name]
    if fixture.get("build") == "buildtools":
        return build_spigot(name)
    path = env.HOME / "servers" / fixture["filename"]
    env.fetch(fixture["url"], path, fixture["algorithm"], fixture["checksum"], name)
    if software(name) == "pumpkin":
        path.chmod(0o755)
    return path


def build_spigot(name):
    fixture = SERVERS[name]
    version = fixture["version"]
    directory = env.HOME / "spigot" / version
    output = directory / fixture["filename"]
    info = directory / "build-info.json"
    if info.exists() and output.exists():
        recorded = json.loads(info.read_text())
        if env.sha(output, "sha256") != recorded["sha256"]:
            raise ValueError(f"{output} no longer matches its build record")
        return output
    with env.locked(f"spigot-{version}", f"the Spigot {version} BuildTools build"):
        if info.exists() and output.exists():
            return output
        directory.mkdir(parents=True, exist_ok=True)
        tools = env.HOME / "spigot" / "BuildTools.jar"
        with env.locked("download-BuildTools.jar", "the BuildTools download"):
            if not tools.exists() or time.time() - tools.stat().st_mtime > 7 * 86400:
                env.log(f"downloading BuildTools from {fixture['url']}")
                request = urllib.request.Request(fixture["url"], headers={"User-Agent": env.USER_AGENT})
                with urllib.request.urlopen(request, timeout=300) as response:
                    data = response.read()
                with tempfile.NamedTemporaryFile(dir=tools.parent, delete=False) as pending:
                    pending.write(data)
                Path(pending.name).replace(tools)
        refs = json.loads(urllib.request.urlopen(urllib.request.Request(
            f"https://hub.spigotmc.org/versions/{version}.json",
            headers={"User-Agent": env.USER_AGENT}), timeout=60).read())["refs"]
        work = directory / "work"
        shutil.rmtree(work, ignore_errors=True)
        work.mkdir()
        log = directory / "buildtools.log"
        env.log(f"building Spigot {version} with BuildTools (several minutes; log: {log})")
        java = env.java(fixture["java"])
        with open(log, "wb") as out:
            result = subprocess.run(
                [java, "-Xmx1536M", "-jar", str(tools), "--rev", version, "--output-dir", str(directory),
                 "--compile", "spigot"], cwd=work, stdout=out, stderr=subprocess.STDOUT,
                stdin=subprocess.DEVNULL, timeout=3600,
                env=dict(os.environ, JAVA_HOME=str(Path(java).parent.parent)))
        if result.returncode or not output.exists():
            raise RuntimeError(f"BuildTools failed for Spigot {version}; see {log}")
        info.write_text(json.dumps({
            "version": version, "refs": refs, "buildtools_sha256": env.sha(tools, "sha256"),
            "sha256": env.sha(output, "sha256"), "built": time.strftime("%Y-%m-%dT%H:%M:%S")},
            indent=2) + "\n")
        shutil.rmtree(work, ignore_errors=True)  # Gigabytes of sources and Maven caches.
        return output


def paper_launch(name):
    """How to start a Paper build without patching at every start: an old
    paperclip's patched jar, or the new paperclip's patched jar and libraries
    with their main class."""
    fixture = SERVERS[name]
    source = jar(name)
    directory = env.HOME / "paper" / fixture["version"]
    done = directory / "launch.json"
    if not done.exists():
        with env.locked(f"paper-{fixture['version']}", f"patching Paper {fixture['version']}"):
            if not done.exists():
                shutil.rmtree(directory, ignore_errors=True)
                directory.mkdir(parents=True)
                env.log(f"patching Paper {fixture['version']} once (downloads the vanilla jar)")
                subprocess.run(
                    [env.java(fixture["java"]), "-Dpaperclip.patchonly=true", "-jar", str(source)],
                    cwd=directory, check=True, stdin=subprocess.DEVNULL, timeout=900,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                patched = sorted(directory.glob("cache/patched*.jar"))
                if patched:
                    launch = {"jar": str(patched[0])}
                else:
                    with zipfile.ZipFile(source) as archive:
                        main = archive.read("META-INF/main-class").decode().strip()
                        libraries = [line.split("\t")[2] for line in
                                     archive.read("META-INF/libraries.list").decode().splitlines() if line]
                        versions = [line.split("\t")[2] for line in
                                    archive.read("META-INF/versions.list").decode().splitlines() if line]
                    classpath = [directory / "versions" / entry for entry in versions] + \
                                [directory / "libraries" / entry for entry in libraries]
                    missing = [str(path) for path in classpath if not path.exists()]
                    if missing:
                        raise RuntimeError(f"paperclip did not produce {missing[:3]}")
                    launch = {"main": main, "classpath": [str(path) for path in classpath]}
                done.write_text(json.dumps(launch, indent=2) + "\n")
    return json.loads(done.read_text())


def surface(protocol, kind):
    """The Y coordinate a player stands at in the default superflat world."""
    if kind == "pumpkin":
        return None  # Known only from the spawn position.
    return -60 if protocol >= 757 else 4


JVM_FLAGS = ["-Xms256M", "-Xmx768M", "-XX:+UseSerialGC", "-XX:ActiveProcessorCount=3",
             "-XX:TieredStopAtLevel=1", "-Dlog4j2.formatMsgNoLookups=true",
             "-DIReallyKnowWhatIAmDoingISwear=true", "-Dpaper.playerconnection.keepalive=120",
             "-Djline.terminal=jline.UnsupportedTerminal", "-Dterminal.jline=false",
             "-Dterminal.ansi=false"]


def properties(protocol, port, compression=True):
    flat = "minecraft:flat" if protocol >= 759 else ("flat" if protocol >= 477 else "FLAT")
    values = {
        "server-ip": "127.0.0.1", "server-port": port, "motd": "rift-translation-matrix",
        "online-mode": "false", "enforce-secure-profile": "false", "prevent-proxy-connections": "false",
        "white-list": "false", "enforce-whitelist": "false",
        "network-compression-threshold": 256 if compression else -1,
        "gamemode": "creative" if protocol >= 477 else 1, "force-gamemode": "true",
        # Spawn properties stay on: some versions discard even summoned villagers
        # and animals when they are off. The doMobSpawning game rule stops
        # natural spawning instead.
        "difficulty": "easy" if protocol >= 477 else 1, "view-distance": 3,
        "simulation-distance": 3, "max-players": 20, "level-type": flat,
        "generate-structures": "false", "spawn-protection": 0, "max-tick-time": -1,
        "pause-when-empty-seconds": 0, "enable-command-block": "true", "op-permission-level": 4,
        "snooper-enabled": "false", "sync-chunk-writes": "false", "allow-flight": "true",
        "log-ips": "false", "enable-status": "true", "player-idle-timeout": 0,
        "allow-nether": "true", "enable-rcon": "false", "enable-query": "false",
    }
    # RIFT_VIA_PROPERTIES="key=value,key=value" overrides these, to check how
    # translation fares with other settings (enforce-secure-profile=true...).
    for item in filter(None, os.environ.get("RIFT_VIA_PROPERTIES", "").split(",")):
        key, _, value = item.partition("=")
        values[key.strip()] = value.strip()
    return "".join(f"{key}={value}\n" for key, value in values.items())


class Server:
    """A running fixture: console, log and port. Use as a context manager."""

    def __init__(self, name, directory, compression=True):
        self.name = resolve(name)
        self.fixture = SERVERS[self.name]
        self.software = software(self.name)
        self.protocol = self.fixture["protocol"]
        self.release = RELEASE_OF.get(self.protocol, self.fixture["version"])
        self.directory = Path(directory)
        self.compression = compression
        self.proc = self.slot = self.port = None
        self.log = self.directory / "server.log"
        self.started = None

    def command(self):
        if self.software == "pumpkin":
            (self.directory / "pumpkin.toml").write_text(
                pumpkin_config(self.port, self.compression, "rift-translation-matrix"))
            return [str(jar(self.name))]
        (self.directory / "eula.txt").write_text("eula=true\n")
        (self.directory / "server.properties").write_text(properties(self.protocol, self.port, self.compression))
        java = env.java(self.fixture["java"])
        if self.software in ("paper", "spigot"):
            (self.directory / "bukkit.yml").write_text(
                "settings:\n  allow-end: false\n  connection-throttle: -1\n"
                "  warn-on-overload: false\n")
            (self.directory / "spigot.yml").write_text(
                "settings:\n  bungeecord: false\n  restart-on-crash: false\n"
                "  timeout-time: 120\nstats:\n  disable-saving: true\n")
        if self.software == "paper":
            launch = paper_launch(self.name)
            if "jar" in launch:
                return [java, *JVM_FLAGS, "-jar", launch["jar"], "nogui"]
            return [java, *JVM_FLAGS, "-cp", os.pathsep.join(launch["classpath"]), launch["main"], "nogui"]
        return [java, *JVM_FLAGS, "-jar", str(jar(self.name)), "nogui"]

    def ready(self):
        if self.proc.poll() is not None:
            raise RuntimeError(f"{self.name} exited ({self.proc.returncode}) during startup:\n{self.tail()}")
        if self.software != "pumpkin" and "Done (" not in self.text():
            return False
        try:
            return status(self.port, self.protocol)["version"]["protocol"] == self.protocol
        except (OSError, EOFError, AssertionError, ValueError, KeyError):
            return False

    def start(self, timeout=None):
        timeout = timeout or float(os.environ.get("RIFT_VIA_START_TIMEOUT", "420"))
        # Prepare artifacts before taking a slot: downloads and builds may wait.
        self.directory.mkdir(parents=True, exist_ok=True)
        if self.software == "paper":
            paper_launch(self.name)
        else:
            jar(self.name)
        self.slot = env.acquire_server_slot(f"pid {os.getpid()} {self.name} {self.directory}")
        self.port = env.allocate_port()
        command = self.command()
        begun = time.monotonic()
        self.proc = env.PROCESSES.spawn(command, self.directory, self.log)
        deadline = time.monotonic() + timeout
        while not self.ready():
            if time.monotonic() > deadline:
                raise TimeoutError(f"{self.name} did not start within {timeout}s:\n{self.tail()}")
            time.sleep(0.25)
        self.started = round(time.monotonic() - begun, 1)
        return self

    def console(self, line):
        self.proc.stdin.write(line.encode() + b"\n")
        self.proc.stdin.flush()

    def text(self):
        try:
            return self.log.read_text(errors="replace")
        except FileNotFoundError:
            return ""

    def tail(self, size=4000):
        return self.text()[-size:]

    def wait_log(self, pattern, timeout=10, since=0):
        """The first log line after offset `since` matching `pattern` (a regex)."""
        expression = re.compile(pattern)
        deadline = time.monotonic() + timeout
        while True:
            text = self.text()[since:]
            for line in text.splitlines():
                if expression.search(line):
                    return line
            if time.monotonic() > deadline:
                return None
            if self.proc.poll() is not None:
                raise RuntimeError(f"{self.name} exited ({self.proc.returncode}):\n{self.tail()}")
            time.sleep(0.1)

    def offset(self):
        return len(self.text())

    def stop(self):
        if self.proc is not None:
            env.PROCESSES.stop(self.proc, grace=8,
                               command="stop" if self.proc.poll() is None else None)
            self.proc = None
        if self.port is not None:
            env.release_port(self.port)
            self.port = None
        if self.slot is not None:
            self.slot.release()
            self.slot = None

    def __enter__(self):
        return self.start()

    def __exit__(self, *_):
        self.stop()
