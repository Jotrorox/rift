#!/usr/bin/env python3
"""Real vanilla clients on backends of other versions through Rift.

Starts one server fixture (vanilla, Paper, Spigot or Pumpkin; see
tests/translation_matrix.py --list), puts the Rift binary in front of it with
a generated config (the backend's version declared, or left for Rift to
detect with --detect), and joins the real, unmodified Minecraft client of
each requested version headlessly (tests/real_client.py). While a client is
connected, the server's console runs a scenario: items with data, blocks
and block entities, entities, particles, sounds, titles, a scoreboard, a
boss bar, effects, weather, time, world border, teleports, a trip to the
nether and back, and a death with respawn. A run passes when the client
joined, stayed connected for the whole stay, received the console messages
sent before and after the scenario, and logged no network, packet or
registry error; and neither Rift nor the server logged the player's
disconnect.

    /opt/homebrew/bin/python3 tests/translation_real_client.py --accept-eula \\
        --server vanilla-26.3 --client 1.12.2,1.16.5,1.20.1 [--stay 60] [--detect]

Logs: the client's under ~/.cache/rift-via/clients/runs (path printed),
Rift's and the server's under target/minecraft/runs/<timestamp>-real-<server>-<client>/.
At most two real clients and three servers run machine-wide; the tools wait
for a slot.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
import threading
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))

import matrix_env as env  # noqa: E402
import matrix_fixtures as fixtures  # noqa: E402
from matrix_scenario import ENTITIES, ITEMS, Commands, entity_nbt, quote_json  # noqa: E402
import real_client  # noqa: E402
from translation_matrix import RUNS, Proxy, client_protocol  # noqa: E402


class Direct:
    """Stands in for Rift with --direct: the client joins the server itself."""

    def __init__(self, server):
        self.port = server.port

    def offset(self):
        return 0

    def text(self):
        return ""

    def stop(self):
        pass


def scenario(server, client, name):
    """Console commands for one real client, in the server's syntax."""
    cmd = Commands(server.protocol, server.software)
    pumpkin = server.software == "pumpkin"
    surface = fixtures.surface(server.protocol, server.software)
    x, z = 8, 8
    y = surface if surface is not None else 80
    lines = []
    if not pumpkin:
        if y < 0 and client_protocol(client) < 755:
            # A client older than 1.17 sees nothing below y 0.
            lines.append(f"fill {x - 6} 0 {z - 6} {x + 14} 0 {z + 10} minecraft:stone")
            y = 1
        lines += [cmd.gamerule("doMobSpawning", "spawn_mobs", "false"), f"tp {name} {x} {y} {z}"]
    lines += [cmd.give(name, item) for item in ITEMS
              if item["min"] <= server.protocol and server.software not in item.get("not_on", ())]
    blocks = [("stone", "stone"), ("glass", "glass"),
              ("chest", "chest" if cmd.legacy else "chest{Items:[{Slot:0b,id:\"minecraft:diamond\","
                                                    + cmd.count(3) + "}]}"),
              ("furnace", "furnace"), ("oak sign", "standing_sign" if cmd.legacy else
                                       ("sign" if server.protocol < 477 else "oak_sign"))]
    for index, (_, block) in enumerate(blocks):
        lines.append(cmd.nbt(f"setblock {x + index} {y} {z + 3} minecraft:{block}"))
    lines.append(f"fill {x - 2} {y} {z + 5} {x} {y + 1} {z + 6} minecraft:"
                 + ("wool 14" if cmd.legacy else "red_wool"))
    for _, entity, dx, dz, key in ENTITIES:
        lines.append(cmd.summon(entity(cmd), x + dx + 0.5, y, z + dz + 0.5, entity_nbt(cmd, key)))
    for kind in ("heart", "dust", "block", "item"):
        lines.append(cmd.particle(kind, x, y + 1, z))
    lines += [cmd.playsound("entity.experience_orb.pickup", name, x, y, z),
              cmd.playsound("block.note.harp" if cmd.legacy else "block.note_block.harp", name, x, y, z),
              cmd.title(name, "title", "Rift"), cmd.title(name, "subtitle", "real client"),
              cmd.title(name, "actionbar", "action bar"),
              "scoreboard objectives add riftreal dummy", "scoreboard objectives setdisplay sidebar riftreal",
              f"scoreboard players set {name} riftreal 7", cmd.team("add", "riftrealteam"),
              cmd.team_option("riftrealteam", "color", "red"), cmd.team("join", "riftrealteam", name)]
    if server.protocol >= 393:
        lines += ["bossbar add rift:real " + quote_json("RiftBoss"), "bossbar set rift:real players @a",
                  "bossbar set rift:real value 50"]
    lines += [cmd.effect(name, "speed", 60, 1), cmd.effect(name, "night_vision", 60, 0),
              "weather rain", "weather thunder", "time set 13000", "weather clear", "time set 6000",
              cmd.xp(name, 5), "worldborder set 2000", "worldborder warning distance 7",
              cmd.gamemode(name, "survival"), cmd.damage(name, 2), cmd.gamemode(name, "creative")]
    if server.protocol >= 401 and not pumpkin:
        lines += [f"execute in minecraft:the_nether run tp {name} 0.5 100 0.5", "WAIT 4",
                  f"execute in minecraft:overworld run tp {name} {x} {y} {z}", "WAIT 3"]
    if not pumpkin and server.protocol >= 573:
        # Immediate respawn: the headless client cannot click the respawn button.
        lines += [cmd.gamerule("doImmediateRespawn", "immediate_respawn", "true"),
                  cmd.gamemode(name, "survival"), f"kill {name}", "WAIT 4",
                  cmd.gamemode(name, "creative"), f"tp {name} {x} {y} {z}"]
    return lines


def run(server, proxy, client, stay, directory):
    name = "Real" + uuid.uuid4().hex[:8]
    nonce = f"rift-real-{uuid.uuid4().hex[:8]}"
    commands = scenario(server, client, name)
    log_start, rift_start = server.offset(), proxy.offset()
    issued = []

    def on_joined(_result):
        def drive():
            if not server.wait_log(rf"{re.escape(name)}.*(joined the game|logged in)", 40, log_start):
                issued.append("(the server never logged the join)")
            time.sleep(2)
            server.console(f"say {nonce}")
            for line in commands:
                if line.startswith("WAIT "):
                    time.sleep(float(line.split()[1]))
                    continue
                server.console(line)
                issued.append(line)
                time.sleep(0.6)
            server.console(f"say {nonce}-after")
        threading.Thread(target=drive, daemon=True).start()

    result = real_client.join(client, "127.0.0.1", proxy.port, name=name, stay=stay,
                              on_joined=on_joined, expect_chat=[nonce, f"{nonce}-after"])
    time.sleep(1)
    # The next client must not join beside this one's mounted pig: 1.21.11+
    # clients warn about the passengers of a vehicle not spawned for them yet.
    server.console("kill @e[type=!player]")
    server_log = server.text()[log_start:]
    rift_log = proxy.text()[rift_start:]
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "server.log").write_text(server_log)
    (directory / "rift.log").write_text(rift_log)
    (directory / "commands.txt").write_text("\n".join(issued) + "\n")
    problems = []
    # Only what happened before the harness stopped the client counts.
    stopped = server_log.find(f"{name} lost connection: Disconnected")
    relevant = server_log if stopped < 0 else server_log[:stopped]
    for match in re.finditer(rf".*{re.escape(name)} (lost connection|was kicked).*", relevant):
        problems.append("server: " + match.group(0).strip())
    for line in rift_log.splitlines():
        if re.search(r"(?i)error|failed|panicked|translat.*(reject|invalid)", line) and "Disconnected" not in line:
            problems.append("rift: " + line.strip()[:300])
    ok = result.ok and not problems
    report = dict(server=server.name, server_version=server.release, client=client, ok=ok,
                  reason=result.reason, joined=result.joined, stayed=result.stayed,
                  failures=result.failures, unknown=result.unknown, problems=problems,
                  client_logs=result.log_dir, logs=str(directory), commands=len(issued))
    (directory / "result.json").write_text(json.dumps(report, indent=2, default=str) + "\n")
    text = result.report().replace("PASS" if result.ok else "FAIL", "PASS" if ok else "FAIL", 1)
    text += f"\n  {'direct' if isinstance(proxy, Direct) else 'via rift'}, server {server.name}, {len(issued)} console commands; logs {directory}"
    text += "".join(f"\n  PROBLEM: {problem}" for problem in problems[:10])
    return ok, report, text


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                     formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    parser.add_argument("--accept-eula", action="store_true")
    parser.add_argument("--server", required=True, help="server fixture, e.g. vanilla-26.3, paper-1.16.5, pumpkin")
    parser.add_argument("--client", required=True, help="real client versions, comma-separated")
    parser.add_argument("--stay", type=float, default=60)
    parser.add_argument("--detect", action="store_true", help="leave the backend's version out of Rift's config")
    parser.add_argument("--direct", action="store_true",
                        help="no Rift: the client joins the server itself (the untranslated reference)")
    parser.add_argument("--binary", type=Path, default=fixtures.ROOT / "target" / "release" / "rift")
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()
    if not args.accept_eula:
        parser.error("--accept-eula is required to run the Minecraft servers and clients")
    clients = [client.strip() for client in args.client.split(",") if client.strip()]
    for client in clients:
        client_protocol(client)
    binary = args.binary.resolve(strict=True)
    env.install_signal_handlers()
    workdir = Path(tempfile.mkdtemp(prefix="rift-real-"))
    server = fixtures.Server(args.server, workdir / "server")
    proxy = None
    reports, passed = [], 0
    try:
        server.start()
        env.log(f"{server.name} ({server.release}, protocol {server.protocol}) on port {server.port}")
        proxy = (Direct(server) if args.direct
                 else Proxy("rift-detect" if args.detect else "rift", server, workdir, binary=binary).start())
        stamp = time.strftime("%Y%m%d-%H%M%S")
        for client in clients:
            suffix = "-direct" if args.direct else "-detect" if args.detect else ""
            directory = RUNS / f"{stamp}-real-{server.name}-{client}{suffix}"
            ok, report, text = run(server, proxy, client, args.stay, directory)
            reports.append(report)
            passed += ok
            print(text, flush=True)
    finally:
        if proxy:
            proxy.stop()
        server.stop()
        shutil.rmtree(workdir, ignore_errors=True)
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(reports, indent=2, default=str) + "\n")
    print(f"{passed}/{len(clients)} real client runs passed")
    if passed != len(clients):
        raise SystemExit(1)


if __name__ == "__main__":
    os.environ.setdefault("PYTHONUNBUFFERED", "1")
    main()
