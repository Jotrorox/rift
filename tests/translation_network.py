#!/usr/bin/env python3
"""Backend switching across protocol versions through Rift.

A real network runs its lobby and its game servers on different versions.
Each cell starts two or three backends of DIFFERENT versions (any fixture of
tests/servers.json: vanilla, Paper, Spigot, Pumpkin) behind one Rift, joins
one schema-driven client of a given version (tests/matrix_client.py) and
switches lobby -> survival -> lobby (-> third -> lobby). Rift attaches a new
translator per backend; for clients from 1.20.2 it sends Start Configuration
itself, for older clients it uses a Join Game + Respawn sequence.

After every switch the cell checks what tests/network_minecraft.py checks for
the same-version case: a fresh world (Join Game), chunks, the join teleport,
Rift's command tree (`server` and `hub`, 1.13+), the new backend's game mode
(lobby creative, survival survival, third adventure) and world border size,
a console message of the new backend, and chat accepted by the new backend.
Every packet the client receives during the whole session is decoded
strictly with the minecraft-data schema of the client's version. Finally a
refused switch (the target bans the player) and, with a third backend, a
failed one (the target is stopped) must leave the player on the old backend.

    /opt/homebrew/bin/python3 tests/translation_network.py --accept-eula --cell 1.21.4-26.3-1.16.5
    /opt/homebrew/bin/python3 tests/translation_network.py --accept-eula --client 1.20.1 \\
        --lobby vanilla-1.20.4 --survival vanilla-1.19.4 [--third paper-26.3] [--detect]
    /opt/homebrew/bin/python3 tests/translation_network.py --list

--detect leaves the backends' versions out of Rift's config, so Rift asks
each backend which version it runs. Logs (Rift, servers, the client's decoded
packets) are kept under target/minecraft/runs/<timestamp>-network-<cell>/.
The exit status is non-zero if any cell fails.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
import time
import traceback
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))

import matrix_env as env  # noqa: E402
import matrix_fixtures as fixtures  # noqa: E402
from matrix_client import Session, SessionError  # noqa: E402
from matrix_scenario import CHAT, Commands, send_chat  # noqa: E402
from translation_matrix import RUNS, client_protocol, decode_report, release  # noqa: E402

ROOT = fixtures.ROOT
ROLES = ("lobby", "survival", "third")
# What tells the backends apart after a switch: game mode (0 survival,
# 1 creative, 2 adventure) and world border diameter.
GAMEMODE = {"lobby": 1, "survival": 0, "third": 2}
BORDER = {"lobby": 128.0, "survival": 256.0, "third": 384.0}

CELLS = {
    "1.21.4-26.3-1.16.5": dict(client="1.21.4", lobby="paper-26.3", survival="vanilla-1.16.5"),
    "26.3-pumpkin-1.20.1-1.12.2": dict(client="26.3", lobby="pumpkin", survival="vanilla-1.20.1",
                                       third="vanilla-1.12.2"),
    "1.16.5-26.3-1.12.2": dict(client="1.16.5", lobby="vanilla-26.3", survival="vanilla-1.12.2"),
    "1.12.2-26.3-1.16.5": dict(client="1.12.2", lobby="vanilla-26.3", survival="vanilla-1.16.5"),
    "1.20.1-1.20.4-1.19.4": dict(client="1.20.1", lobby="vanilla-1.20.4", survival="vanilla-1.19.4"),
    # One backend speaks the client's own version (no translator), the others do not.
    "1.20.4-native-1.12.2-26.3": dict(client="1.20.4", lobby="vanilla-1.20.4", survival="vanilla-1.12.2",
                                      third="paper-26.3"),
}


class Backend(fixtures.Server):
    """A fixture in one network role: its own game mode, and a marker in its MOTD."""

    def __init__(self, role, name, directory):
        super().__init__(name, directory)
        self.role = role

    def command(self):
        command = super().command()
        if self.software != "pumpkin":
            path = self.directory / "server.properties"
            mode = {0: "survival", 1: "creative", 2: "adventure"}[GAMEMODE[self.role]]
            if self.protocol < 477:
                mode = str(GAMEMODE[self.role])
            text = re.sub(r"(?m)^gamemode=.*$", f"gamemode={mode}", path.read_text())
            path.write_text(text.replace("motd=rift-translation-matrix", f"motd=rift-network-{self.role}"))
        return command


class Failure(AssertionError):
    pass


MODES = {"survival": 0, "creative": 1, "adventure": 2, "spectator": 3}


def mode_number(value):
    """A game mode as a number: schemas give it as a number or a name."""
    if isinstance(value, str):
        return MODES.get(value)
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return int(value) & 7
    return None


def gamemode_of(value):
    """The game mode a decoded Join Game or Respawn packet carries."""
    if isinstance(value, dict):
        for key in ("gameMode", "gamemode"):
            if mode_number(value.get(key)) is not None:
                return mode_number(value[key])
        for entry in value.values():
            found = gamemode_of(entry)
            if found is not None:
                return found
    return None


def border_of(record):
    """The world border diameter a decoded border packet sets, if any."""
    value = record.value
    if record.name == "initialize_world_border":
        return value.get("newDiameter")
    if record.name == "world_border_size":
        return value.get("diameter")
    if record.name == "world_border":
        if value.get("action") == 0:
            return value.get("radius")
        if value.get("action") == 3:
            return value.get("new_radius")
    return None


def string_bytes(text):
    data = text.encode()
    return bytes([len(data)]) + data


class Network:
    """Two or three backends of different versions behind one Rift."""

    def __init__(self, cell, binary, directory, detect=False):
        self.cell, self.binary, self.directory, self.detect = cell, binary, Path(directory), detect
        self.workdir = Path(tempfile.mkdtemp(prefix="rift-network-"))
        self.backends = {}
        self.rift = self.port = None
        self.log = self.directory / "rift.log"

    def start(self):
        for role in ROLES:
            if self.cell.get(role):
                backend = Backend(role, self.cell[role], self.workdir / role)
                self.backends[role] = backend
                backend.start()
                env.log(f"{role}: {backend.name} ({backend.release}, protocol {backend.protocol}) "
                        f"on port {backend.port}")
                if backend.software != "pumpkin":
                    backend.console(f"worldborder set {int(BORDER[role])}")
                    backend.console("gamerule " + ("spawn_mobs false" if backend.protocol >= 777
                                                   else "doMobSpawning false"))
        self.port = env.allocate_port()
        lines = ['local config = require("rift.config")',
                 f'config.listeners.public = "127.0.0.1:{self.port}"']
        for role, backend in self.backends.items():
            address = f'"127.0.0.1:{backend.port}"'
            lines.append(f"config.backends.{role} = " + (
                address if self.detect else f'{{ address = {address}, version = "{backend.release}" }}'))
        lines += ['config.routes.public = "lobby"',
                  'config.network = { initial = { "lobby" }, hubs = { "lobby" } }']
        config = self.directory / "rift.lua"
        config.write_text("\n".join(lines) + "\n")
        self.rift = env.PROCESSES.spawn([str(self.binary), "--config", str(config)], self.directory, self.log,
                                        stdin=False)
        deadline = time.monotonic() + 60
        while "rift: listening on" not in self.text():
            if self.rift.poll() is not None:
                raise RuntimeError(f"rift exited ({self.rift.returncode}):\n{self.text()[-3000:]}")
            if time.monotonic() > deadline:
                raise TimeoutError(f"rift did not start:\n{self.text()[-3000:]}")
            time.sleep(0.05)
        return self

    def text(self):
        try:
            return self.log.read_text(errors="replace")
        except FileNotFoundError:
            return ""

    def stop(self):
        if self.rift is not None:
            env.PROCESSES.stop(self.rift, grace=3)
            self.rift = None
        if self.port is not None:
            env.release_port(self.port)
            self.port = None
        for role, backend in self.backends.items():
            text = backend.text()
            backend.stop()
            (self.directory / f"{role}-{backend.name}.log").write_text(text)
        shutil.rmtree(self.workdir, ignore_errors=True)


class Switching:
    """One client walking through the network."""

    def __init__(self, network, protocol):
        self.network, self.protocol = network, protocol
        self.client = None
        self.where = None
        self.worlds = 0  # Join Game packets so far.
        self.checks = []

    def note(self, message):
        self.checks.append(message)
        env.log(f"  {message}")

    def connect(self):
        pumpkin = self.network.backends["lobby"].software == "pumpkin"
        self.client = Session(self.network.port, "RiftNet" + uuid.uuid4().hex[:7], self.protocol,
                              label="client", pumpkin=pumpkin)
        self.client.step = "lobby-initial"
        self.client.join(timeout=90)
        self.arrived("lobby", 0, "initial join")

    def command(self, text):
        session = self.client
        if session.has("chat_command"):
            session.send_packet("chat_command", {"command": text, "timestamp": int(time.time() * 1000)})
        else:
            session.send_packet("chat", {"message": "/" + text})

    def logins(self, since):
        return [record for record in self.client.records[since:] if record.name == "login"]

    def arrived(self, role, mark, what, timeout=60):
        """Check that the client now plays on `role`, whose world started
        after record `mark`."""
        session, backend = self.client, self.network.backends[role]
        login = session.wait_packet("login", f"Join Game of {role} ({what})", timeout, mark)
        # Legacy switching: Join Game into another dimension, then Respawn
        # into the real one; the mode and teleport that count come last.
        if self.protocol < 764 and mark:
            session.wait_packet("respawn", f"Respawn after the Join Game of {role}", timeout, login.index)
        session.wait_packet("position", f"join teleport on {role}", timeout, login.index)
        deadline = time.monotonic() + timeout
        while not session.packets("map_chunk", login.index):
            session.check()
            if time.monotonic() > deadline:
                raise Failure(f"{role}: no chunks after the switch ({what})")
            time.sleep(0.1)
        if self.protocol >= 393:
            session.wait(lambda record: record.name == "declare_commands" and record.index > login.index
                         and string_bytes("server") in record.body and string_bytes("hub") in record.body,
                         f"Rift's command tree on {role}", timeout, login.index)
        self.worlds += 1
        modes = [gamemode_of(record.value) for record in session.packets(("login", "respawn"), login.index)]
        modes += [mode_number(record.value.get("gameMode"))
                  for record in session.packets("game_state_change", login.index)
                  if record.value.get("reason") in (3, "change_game_mode")]
        modes = [mode for mode in modes if mode is not None]
        expected = GAMEMODE[role] if backend.software != "pumpkin" else 1
        if not modes or modes[-1] != expected:
            raise Failure(f"{role}: game mode {modes}, expected {expected} ({what})")
        if backend.software != "pumpkin":
            deadline = time.monotonic() + 10
            while True:
                borders = [border_of(record) for record in session.packets(None, login.index)
                           if record.value is not None]
                borders = [value for value in borders if value is not None]
                if borders and abs(borders[-1] - BORDER[role]) < 0.01:
                    break
                if time.monotonic() > deadline:
                    raise Failure(f"{role}: world border {borders}, expected {BORDER[role]} ({what})")
                time.sleep(0.1)
        self.verify(role, what)
        self.where = role
        self.note(f"on {role} ({backend.name}) after {what}: Join Game, teleport, chunks, "
                  f"{'commands, ' if self.protocol >= 393 else ''}mode {expected}, border, console and chat")

    def verify(self, role, what):
        """The client hears `role`'s console and `role` accepts its chat."""
        session, backend = self.client, self.network.backends[role]
        marker = f"{role}-{uuid.uuid4().hex[:8]}"
        mark = session.mark()
        backend.console(Commands(backend.protocol, backend.software).tellraw(session.name, marker))
        session.wait(lambda record: record.name in CHAT and marker.encode() in record.body,
                     f"{role}'s console message ({what})", 15, mark)
        text = f"rift-net-chat-{uuid.uuid4().hex[:8]}"
        since = backend.offset()
        send_chat(session, text)
        if backend.wait_log(re.escape(text), 10, since) is None:
            raise Failure(f"{role} did not accept chat ({what})")

    def switch(self, role, command=None):
        session = self.client
        session.step = f"to-{role}"
        mark = session.mark()
        self.command(command or ("hub" if role == "lobby" else f"server {role}"))
        self.arrived(role, mark, f"switch from {self.where}")

    def refused(self, role, why):
        """A switch to `role` must fail and leave the player where it is."""
        session, old = self.client, self.where
        session.step = f"refused-{role}"
        mark = session.mark()
        self.command(f"server {role}")
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            session.check()
            records = session.packets(CHAT, mark)
            if any(b"ban" in record.body.lower() or b"could not" in record.body.lower()
                   or b"unable" in record.body.lower() or b"fail" in record.body.lower()
                   or b"refused" in record.body.lower() or b"unavailable" in record.body.lower()
                   for record in records):
                break
            time.sleep(0.2)
        else:
            raise Failure(f"no message about the refused switch to {role} ({why})")
        time.sleep(1)
        if self.logins(mark) or session.packets("start_configuration", mark):
            raise Failure(f"the refused switch to {role} ({why}) still started a new world")
        self.verify(old, f"after the refused switch to {role}")
        self.note(f"switch to {role} refused ({why}): still on {old}, console and chat work")


def run(cell_name, cell, binary, detect):
    stamp = time.strftime("%Y%m%d-%H%M%S")
    directory = RUNS / f"{stamp}-network-{cell_name}{'-detect' if detect else ''}"
    directory.mkdir(parents=True, exist_ok=True)
    protocol = client_protocol(cell["client"])
    result = dict(cell=cell_name, client=release(protocol), backends={role: cell[role] for role in ROLES
                                                                      if cell.get(role)},
                  detect=detect, status="FAIL", logs=str(directory), checks=[])
    began = time.monotonic()
    network = Network(cell, binary, directory, detect)
    walk = Switching(network, protocol)
    try:
        network.start()
        walk.connect()
        walk.switch("survival")
        walk.switch("lobby")
        if "third" in network.backends:
            walk.switch("third")
            walk.switch("survival")
            walk.switch("lobby")
        survival = network.backends["survival"]
        survival.console(f"ban {walk.client.name} rift network ban")
        time.sleep(1)
        walk.refused("survival", "banned on the target")
        survival.console(f"pardon {walk.client.name}")
        time.sleep(1)
        walk.switch("survival")
        if "third" in network.backends:
            walk.switch("lobby")
            third = network.backends["third"]
            env.PROCESSES.stop(third.proc, grace=8, command="stop")
            third.proc = None
            walk.refused("third", "target stopped")
        result["status"] = "PASS"
    except (Failure, SessionError, TimeoutError, RuntimeError) as error:
        result["error"] = f"{type(error).__name__}: {error}"
    except Exception as error:
        result["error"] = f"{type(error).__name__}: {error}"
        result["traceback"] = traceback.format_exc()[-3000:]
    finally:
        client = walk.client
        if client is not None:
            client.close()
            client.write_log(directory / "client.jsonl")
            report = decode_report(client)
            failures = sum(entry["count"] for entry in report if entry["level"] == "FAIL")
            result.update(packets=len(client.records), decode_failures=failures,
                          failures=report, worlds=walk.worlds)
            if failures:
                result["status"] = "FAIL"
            if client.failure:
                result["status"] = "FAIL"
                result.setdefault("error", client.failure)
        network.stop()
    result["checks"] = walk.checks
    errors = [line for line in network.text().splitlines()
              if "error" in line.lower() or "failed" in line or "panicked" in line]
    if errors:
        result["rift_errors"] = errors[-20:]
    result["seconds"] = round(time.monotonic() - began, 1)
    (directory / "result.json").write_text(json.dumps(result, indent=2, default=str) + "\n")
    return result


def summary(result):
    line = (f"{result['status']:4} network {result['cell']}{' (detect)' if result['detect'] else ''}: "
            f"client {result['client']}, " + ", ".join(f"{role} {name}" for role, name in result["backends"].items())
            + f"; worlds {result.get('worlds', 0)}, packets {result.get('packets', 0)}, "
            f"decode failures {result.get('decode_failures', 0)}; {result['seconds']}s; {result['logs']}")
    if result["status"] != "PASS":
        if result.get("error"):
            line += f"\n    {result['error']}"
        for entry in result.get("failures", [])[:8]:
            line += (f"\n    [{entry['level']}] {entry['step']}: could not decode {entry['state']} "
                     f"{entry['packet']} (id {entry['id']:#04x}, x{entry['count']}): {entry['error']}")
            line += "".join("\n        " + text for text in entry["hexdump"].splitlines()[:4])
        for text in result.get("rift_errors", [])[-6:]:
            line += f"\n    rift: {text}"
    return line


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                     formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    parser.add_argument("--accept-eula", action="store_true",
                        help="accept https://aka.ms/MinecraftEULA for the local test servers")
    parser.add_argument("--cell", action="append", default=[], help="predefined cell (repeat; 'all')")
    parser.add_argument("--client", help="client version of a custom cell")
    for role in ROLES:
        parser.add_argument(f"--{role}", help=f"server fixture of the {role} backend")
    parser.add_argument("--detect", action="store_true", help="let Rift detect the backends' versions")
    parser.add_argument("--binary", type=Path, default=ROOT / "target" / "release" / "rift")
    parser.add_argument("--json", type=Path, help="write results as JSON")
    parser.add_argument("--list", action="store_true", help="list the predefined cells")
    args = parser.parse_args()
    if args.list:
        for name, cell in CELLS.items():
            print(f"  {name:<28} client {cell['client']:<7} "
                  + ", ".join(f"{role} {cell[role]}" for role in ROLES if cell.get(role)))
        return
    if not args.accept_eula:
        parser.error("--accept-eula is required to run the Minecraft servers")
    cells = {}
    for name in args.cell:
        for entry in (CELLS if name == "all" else [name]):
            if entry not in CELLS:
                parser.error(f"unknown cell {entry!r}; see --list")
            cells[entry] = CELLS[entry]
    if args.client:
        if not (args.lobby and args.survival):
            parser.error("a custom cell needs --client, --lobby and --survival")
        custom = dict(client=args.client, lobby=args.lobby, survival=args.survival, third=args.third)
        cells["-".join(filter(None, [args.client, args.lobby, args.survival, args.third]))] = custom
    if not cells:
        parser.error("give --cell or --client/--lobby/--survival")
    for cell in cells.values():
        for role in ROLES:
            if cell.get(role):
                fixtures.resolve(cell[role])
    binary = args.binary.resolve(strict=True)
    env.install_signal_handlers()
    RUNS.mkdir(parents=True, exist_ok=True)
    results = []
    try:
        for name, cell in cells.items():
            result = run(name, cell, binary, args.detect)
            results.append(result)
            print(summary(result), flush=True)
    except KeyboardInterrupt:
        env.PROCESSES.stop_all()
        raise SystemExit(130)
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(results, indent=2, default=str) + "\n")
    passed = sum(result["status"] == "PASS" for result in results)
    print(f"{passed}/{len(results)} network cells passed")
    if passed != len(results):
        raise SystemExit(1)


if __name__ == "__main__":
    os.environ.setdefault("PYTHONUNBUFFERED", "1")
    main()
