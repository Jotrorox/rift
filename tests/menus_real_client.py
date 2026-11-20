#!/usr/bin/env python3
"""Real vanilla clients and Rift's inventory menus (docs/menus.md).

Unit tests check the menu packets against our own decoders. This script asks
the real, unmodified client of each version whether it accepts them. For every
(client, backend) pair it starts a pinned vanilla server, puts Rift in front
of it, and joins the headless client of tests/real_client.py. A persistent Lua
plugin written for the run (plugins/menutest, with an `http` hook) shows menus
to the player when the script POSTs to its endpoint, one step every 2.5 seconds
so that the client handles each:

    1 a 6-row chest with every button field of the "Buttons" table
    2 the same id again with changes (re-open), 3 another id (replace)
    4 a hopper, 5 a dispenser, 6 rift.players.close_menu
    7 a chest opened again and left open while the client stays; it leaves

Why a plugin and HTTP: Rift's script hooks (jobs, join events, and rift.players
in rift.lua) need authentication.online_mode, which an offline headless client
cannot pass (Mojang's session server); a persistent plugin has rift.players
without it and its `http` hook is not a player hook. This tests the same menu
packets, built by the same code, as a rift.lua would.

Every step also sends the player a chat line "MENUTEST step N ... queued=true"
(the return value of the call), which the script requires in the client's log.

A run passes when real_client.py says the client joined, stayed the whole
time and logged no network, packet, decoder or registry error; no unknown
client warning (the stubbed head texture download aside) is tolerated; Rift
logged no error; every step answered queued=true; and the relay between
client and Rift saw the packets: one open_window for Rift's container IDs
(101-127) per step that opens a menu, window contents for each, and the close
for step 6. The relay's packets are decoded with tests/protodef.py (an
oracle independent of Rift), so a layout the schema rejects is reported too.

    /opt/homebrew/bin/python3 tests/menus_real_client.py --accept-eula \\
        --binary target/debug/rift [--client 1.20.4,26.3] [--cross] [--only-cross]

--client takes client versions (each on a backend of its own version unless
--server names one); --cross adds 1.12.2 and 1.20.4 on a 26.3 backend, so the
translator is in the path. To add steps (anvil menus, update_menu), append to
STEPS and OPENS/CLOSES. Logs: the client's under ~/.cache/rift-via/clients/runs,
Rift's, the server's, the plugin and result.json under --out (default
target/minecraft/runs/<timestamp>-menus/<server>-<client>/).
"""

import argparse
import base64
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))

import matrix_env as env  # noqa: E402
import matrix_fixtures as fixtures  # noqa: E402
import protodef  # noqa: E402
import real_client  # noqa: E402
from translation_matrix import RUNS, client_protocol  # noqa: E402

ALL_CLIENTS = ["1.12.2", "1.13.2", "1.14.4", "1.15.2", "1.16.5", "1.17.1", "1.18.2", "1.19.4", "1.20.1",
               "1.20.4", "1.20.6", "1.21.1", "1.21.3", "1.21.4", "1.21.5", "1.21.8", "1.21.11", "26.3"]
CROSS = [("26.3", "1.12.2"), ("26.3", "1.20.4")]  # (backend, client)
WORLD_SECONDS = 4  # in the world before the first menu
STEP_SECONDS = 2.5  # between steps
HOLD_SECONDS = 10  # the last menu stays open
RIFT_FIRST = 101  # Rift's container IDs are 101-127 (docs/menus.md)

# Anything in the client's log that a menu could have caused. Unknown warnings
# (real_client.py reports them for triage only) are failures here when they
# match; the stubbed graphics only ever show up as noise or as a crash.
SCREEN_PROBLEM = re.compile(
    r"Screen|Container|Menu|Inventory|Slot|ItemStack|Tooltip|Item |Component|Exception|Error|"
    r"Unable|Failed|Couldn't|Invalid|Unknown|Missing|Rendering|render", re.I)

# The stubbed audio engine of old clients (not a screen): a WARN from the sound loader thread.
STUBBED_AUDIO = re.compile(r"^\[(Sound Library Loader|Sound engine)")
HEAD_TEXTURE = re.compile(r"Couldn't download http texture\n.*(Connection refused|SocketException)", re.S)
TEXTURE = base64.b64encode(json.dumps({"textures": {"SKIN": {
    "url": "http://textures.minecraft.net/texture/" + "3f8a8c2d0e5e1b6c5d1e5f4a9b7c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f708192"}}}
).encode()).decode()

# --- The steps ---------------------------------------------------------------
# One entry per tick: (label, Lua expression that shows something and returns
# the call's result). `p` is the player table; `rift.players.menu(p.uuid, {...})`
# returns true once queued. Add new steps (anvil menus, update_menu) here.

CHEST_A = r"""{
    id = "a", type = "chest", rows = 6,
    title = { mini = "<red>Every <gold>button <green>field <aqua>of <light_purple>A" },
    slots = {
        { slot = 0, item = "stone" },
        { slot = 1, item = "diamond", count = 1 },
        { slot = 2, item = "bread", count = 64 },
        { slot = 3, item = "arrow", count = 99 },
        { slot = 4, item = "diamond_sword", name = { text = "Colored sword", color = "gold", bold = true },
          lore = { "Plain line", { text = "Red line", color = "red" },
                   { mini = "<green>Mini <b>bold</b> <#ff8800>hex <u>under</u>" } } },
        { slot = 5, item = "name_tag", name = { mini = "<gradient:red:blue>Gradient name</gradient>" },
          lore = { { mini = "<i>italic</i> <st>struck</st> <obf>x</obf>" } } },
        { slot = 6, item = "stick", glint = true },
        { slot = 7, item = "enchanted_golden_apple", glint = false },
        { slot = 8, item = "paper", model = 1234 },
        { slot = 9, item = "paper", model = { floats = { 1.5, 2.5 }, flags = { true, false },
                                              strings = { "a", "b" }, colors = { "#ff0000", 255 } } },
        { slot = 10, item = "paper", item_model = "minecraft:diamond" },
        { slot = 11, item = "player_head", head = { texture = "%(texture)s" }, name = "Texture head" },
        { slot = 12, item = "player_head", head = { player = "%(name)s" }, name = "Player head" },
        { slot = 13, item = "leather_chestplate", color = "#ff00aa" },
        { slot = 14, item = "potion", color = "#00ffcc" },
        { slot = 15, item = "diamond_sword", details = false, name = "No details" },
        { slot = 16, item = "netherite_sword", tooltip = false },
        { slot = 17, item = "diamond_sword", details = true, glint = true, name = "Details shown" },
        { slot = 18, item = "mace", name = "Stand-in: mace" },
        { slot = 19, item = "pale_oak_planks" },
        { slot = 20, item = "trial_key", lore = { "Stand-in: trial key" } },
        { slot = 21, item = "wind_charge", count = 16 },
        { slot = 22, item = "armadillo_scute" },
        { slot = 23, item = "dried_kelp", count = 2 },
        { slot = 24, item = "shield", name = { text = "Shield", color = "blue" } },
        { slot = 25, item = "elytra", glint = true },
        { slot = 26, item = "emerald", id = "buy", payload = { price = 5 }, clicks = "all",
          name = { mini = "<green>Buy" } },
        { slot = 52, item = "barrier", id = "close", close = true, name = { mini = "<red>Close" } },
        { slot = 53, item = "gray_stained_glass_pane", name = " " },
    },
    state = { page = 1 },
}"""

# The same id again: another title, changed slots, one emptied.
CHEST_A2 = r"""{
    id = "a", type = "chest", rows = 6,
    title = { text = "A again", color = "dark_green", bold = true },
    slots = {
        { slot = 0, item = "diamond_block", count = 2, name = { mini = "<aqua>Changed" } },
        { slot = 1, empty = true },
        { slot = 4, item = "golden_sword", glint = true },
        { slot = 26, item = "redstone", id = "buy", name = "Sell" },
        { slot = 40, item = "compass", lore = { "New slot" } },
        { slot = 53, item = "gray_stained_glass_pane", name = " " },
    },
}"""

CHEST_B = r"""{
    id = "b", type = "chest",
    title = "Another menu",
    layout = { "#########", "# S C M #", "####X####" },
    keys = {
        ["#"] = { item = "gray_stained_glass_pane", name = " " },
        S = { item = "grass_block", name = { mini = "<green>Survival" }, lore = { "Click to join" },
              id = "join", payload = { server = "survival" } },
        C = { item = "bricks", name = { mini = "<gold>Creative" }, id = "join", payload = { server = "creative" } },
        M = { item = "bow", name = { mini = "<red>Minigames" }, id = "join", payload = { server = "minigames" } },
        X = { item = "barrier", name = { mini = "<red>Close" }, close = true },
    },
}"""

HOPPER = r"""{
    id = "hop", type = "hopper", title = { mini = "<yellow>Hopper" },
    slots = {
        { slot = 0, item = "iron_ingot", count = 3 },
        { slot = 1, item = "gold_ingot", name = { mini = "<gold>Gold" } },
        { slot = 2, item = "diamond", glint = true, id = "pick", payload = { n = 2 } },
        { slot = 4, item = "barrier", close = true },
    },
}"""

DISPENSER = r"""{
    id = "disp", type = "dispenser", title = { mini = "<blue>Dispenser" },
    layout = { "ABC", "D E", "FGH" },
    keys = {
        A = { item = "arrow", count = 64 }, B = { item = "bow" }, C = { item = "tnt", id = "boom" },
        D = { item = "redstone" }, E = { item = "lever" },
        F = { item = "oak_planks", count = 5 }, G = { item = "chest", name = { mini = "<red>Chest" } },
        H = { item = "hopper", lore = { "last" } },
    },
}"""

STAY = r"""{
    id = "stay", type = "chest", rows = 3, title = { mini = "<rainbow>Left open" },
    slots = {
        { slot = 4, item = "nether_star", name = { mini = "<gold>Still open" }, glint = true },
        { slot = 13, item = "diamond_sword", id = "x" },
        { slot = 22, item = "player_head", head = { player = "%(name)s" } },
    },
}"""

STEPS = [
    ("chest A: every button field", "rift.players.menu(p.uuid, %s)" % CHEST_A),
    ("chest A again (re-open, changes)", "rift.players.menu(p.uuid, %s)" % CHEST_A2),
    ("chest B (replace, layout)", "rift.players.menu(p.uuid, %s)" % CHEST_B),
    ("hopper", "rift.players.menu(p.uuid, %s)" % HOPPER),
    ("dispenser", "rift.players.menu(p.uuid, %s)" % DISPENSER),
    ("close_menu", 'rift.players.close_menu(p.uuid, "disp")'),
    ("menu left open", "rift.players.menu(p.uuid, %s)" % STAY),
]
# Steps that open a screen for the client (the rest only changes or closes one).
OPENS = {0: 1, 1: 1, 2: 1, 3: 1, 4: 1, 6: 1}
CLOSES = {5}


PLUGIN_TOML = """name = "menutest"
version = "0.1.0"
api = "0.2"
engine = "lua"

[capabilities]
network = false
env = []
"""


def plugin_lua(name):
    """init.lua of the persistent Lua plugin that runs step N for the first player.

    Rift's player hooks (jobs, join events, `rift.players` in rift.lua) need
    online-mode authentication, which an offline headless client cannot pass.
    A persistent plugin has `rift.players` without it, and its `http` hook is
    not a player hook.
    """
    steps = []
    for index, (label, call) in enumerate(STEPS, 1):
        body = (call % {"texture": TEXTURE, "name": name}) if "%(" in call else call
        steps.append(f"    [{index}] = {{ {json.dumps(label)}, function(p) return {body} end }},")
    return (
        "local steps = {\n" + "\n".join(steps) + "\n}\n"
        'rift.on("http", function(request)\n'
        '    if request.method ~= "POST" or request.path ~= "/ext/step" then return nil end\n'
        "    local list = rift.players.list()\n"
        '    if #list == 0 then return { status = 409, body = "no player in a world" } end\n'
        "    local p = list[1]\n"
        "    local n = tonumber(request.body)\n"
        "    local step = steps[n]\n"
        '    if not step then return { status = 404, body = "no such step" } end\n'
        "    local ok = step[2](p)\n"
        '    rift.log("info", "MENUTEST step " .. n .. " " .. step[1] .. " queued=" .. tostring(ok))\n'
        '    rift.players.message(p.uuid, "MENUTEST step " .. n .. " " .. step[1] .. " queued=" .. tostring(ok))\n'
        '    return { body = "step " .. n .. " queued=" .. tostring(ok) }\n'
        "end)\n")


def config_lua():
    return (
        'local config = require("rift.config")\n'
        'config.listeners.public = "127.0.0.1:@PORT@"\n'
        'config.backends.main = { address = "127.0.0.1:@BACKEND@", version = "@VERSION@" }\n'
        'config.routes.public = "main"\n'
        'config.web = { enabled = true, listen = "127.0.0.1:@WEB@", ui = false, token = "@TOKEN@" }\n'
        'rift.plugin("menutest")\n')


def start_rift(binary, server, directory, name):
    directory.mkdir(parents=True, exist_ok=True)
    port, web, token = env.allocate_port(), env.allocate_port(), "menutest-" + uuid.uuid4().hex
    config = directory / "rift.lua"
    folder = directory / "plugins" / "menutest"
    folder.mkdir(parents=True, exist_ok=True)
    (folder / "plugin.toml").write_text(PLUGIN_TOML)
    (folder / "init.lua").write_text(plugin_lua(name))
    text = config_lua().replace("@PORT@", str(port)).replace("@BACKEND@", str(server.port))
    text = text.replace("@WEB@", str(web)).replace("@TOKEN@", token)
    config.write_text(text.replace("@VERSION@", str(server.release)))
    log = directory / "rift.log"
    proc = env.PROCESSES.spawn([str(binary), "--config", str(config)], directory, log, stdin=False)
    deadline = time.monotonic() + 60
    while "rift: listening on" not in (log.read_text(errors="replace") if log.exists() else ""):
        if proc.poll() is not None:
            raise RuntimeError(f"rift exited ({proc.returncode}):\n{log.read_text(errors='replace')[-3000:]}")
        if time.monotonic() > deadline:
            raise TimeoutError("rift did not start")
        time.sleep(0.05)
    return proc, port, web, token, log


def post_step(web, token, number):
    """Run step `number` in Rift; (HTTP status, body)."""
    request = urllib.request.Request(f"http://127.0.0.1:{web}/ext/step", data=str(number).encode(), method="POST",
                                     headers={"Authorization": f"Bearer {token}"})
    try:
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, response.read().decode(errors="replace")
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode(errors="replace")
    except OSError as error:
        return 0, str(error)


class Packets:
    """What the relay saw on its way to the client: window packets, decoded."""

    def __init__(self):
        self.events = []  # (seconds, packet name, window id)
        self.problems = []
        self.notes = []
        self.started = time.monotonic()
        self.decoders = {}
        self.total = 0

    def see(self, protocol, packet_id, body):
        self.total += 1
        if protocol not in self.decoders:
            try:
                decoder = protodef.Decoder(protodef.schema_for(protocol), "play", "clientbound")
            except Exception as exc:
                self.problems.append(f"no schema for protocol {protocol}: {exc}")
                decoder = None
            self.decoders[protocol] = decoder
        decoder = self.decoders[protocol]
        name = decoder.names.get(packet_id) if decoder else None
        if name not in ("open_window", "window_items", "set_slot", "close_window", "set_cursor_item"):
            return
        try:
            _, value = decoder.packet(packet_id, body)
        except Exception as exc:
            note = f"{name} does not decode with the independent schema: {exc}"
            # The pinned minecraft-data schema of 1.21.2/1.21.3 reads custom_model_data in the layout of
            # 1.21.4 (floats, flags, ...); the game has the VarInt until 1.21.3, and the real 1.21.3
            # client accepts what Rift sends. The client is the judge: a note, not a problem.
            (self.notes if protocol == 768 and "custom_model_data" in str(exc) else self.problems).append(note)
            return
        window = value.get("windowId", value.get("containerId")) if isinstance(value, dict) else None
        extra = {"open_window": value.get("inventoryType", value.get("windowType", value.get("windowTitle"))),
                 "window_items": len(value.get("items", ())), "set_slot": value.get("slot")}.get(name)
        self.events.append((round(time.monotonic() - self.started, 1), name, window, extra))

    def rift(self, name):
        return [event for event in self.events if event[1] == name and event[2] is not None
                and event[2] >= RIFT_FIRST]


PACKETS = Packets()


def observe_relay():
    """Wrap real_client.Relay.frame to feed PACKETS (real_client.py stays untouched)."""
    original = real_client.Relay.frame

    def frame(self, data, raw, clientbound, session):
        try:
            if clientbound and session["state"] == "play":
                payload = self.decode(data, session)
                packet_id, offset = real_client.read_varint_from(payload, 0)
                PACKETS.see(self.protocol, packet_id, payload[offset:])
        except Exception as exc:  # observation must never break the stream
            PACKETS.problems.append(f"relay observer: {type(exc).__name__}: {exc}")
        return original(self, data, raw, clientbound, session)

    real_client.Relay.frame = frame


def run(binary, server, client, stay, directory):
    global PACKETS
    PACKETS = Packets()
    name = "Menu" + uuid.uuid4().hex[:8]
    directory.mkdir(parents=True, exist_ok=True)
    log_start = server.offset()
    proc, port, web, token, rift_log = start_rift(binary, server, directory, name)
    answers = []

    def drive(_result):
        def go():
            time.sleep(WORLD_SECONDS)
            for number in range(1, len(STEPS) + 1):
                answers.append((number, *post_step(web, token, number)))
                time.sleep(STEP_SECONDS)
        threading.Thread(target=go, daemon=True).start()

    try:
        result = real_client.join(client, "127.0.0.1", port, name=name, stay=stay,
                                  expect_chat=[f"MENUTEST step {len(STEPS)} "], on_joined=drive)
    finally:
        env.PROCESSES.stop(proc, grace=3)
        env.release_port(port)
        env.release_port(web)
    server_log = server.text()[log_start:]
    rift_text = rift_log.read_text(errors="replace")
    (directory / "server.log").write_text(server_log)
    problems = []
    for entry in result.unknown:
        if STUBBED_AUDIO.search(entry):
            continue
        if HEAD_TEXTURE.search(entry):
            continue  # a head with a texture: the client's own fetch of the skin, through the dead proxy
        if SCREEN_PROBLEM.search(entry):
            problems.append("client log: " + entry.splitlines()[0][:300])
    chat = [line for line in result.chat if "MENUTEST" in line]
    queued = [answer for answer in answers if answer[1] == 200 and "queued=true" in answer[2]]
    for number, status, body in answers:
        if (status, "queued=true" in body) != (200, True):
            problems.append(f"step {number} was not queued: HTTP {status} {body[-600:]}")
    if len(chat) < len(STEPS):
        problems.append(f"the client logged {len(chat)} of {len(STEPS)} MENUTEST chat lines")
    if len(queued) < len(STEPS):
        problems.append(f"only {len(queued)} of {len(STEPS)} steps reported queued=true")
    for line in rift_text.splitlines():
        if re.search(r"(?i)\b(error|panic|panicked|failed)\b", line):
            problems.append("rift: " + line.strip()[:300])
    problems += PACKETS.problems[:5]
    opens, closes = PACKETS.rift("open_window"), PACKETS.rift("close_window")
    contents = PACKETS.rift("window_items") + PACKETS.rift("set_slot")
    if len(opens) < sum(OPENS.values()):
        problems.append(f"the client was sent {len(opens)} open_window packets for Rift containers, "
                        f"expected {sum(OPENS.values())}")
    if not contents:
        problems.append("no window contents were sent for a Rift container")
    if len(closes) < len(CLOSES):
        problems.append("no close_window for Rift's container after close_menu")
    ok = result.ok and not problems
    report = dict(server=server.name, client=client, ok=ok, reason=result.reason, joined=result.joined,
                  stayed=result.stayed, failures=result.failures, unknown=result.unknown, problems=problems,
                  chat=chat, rift_open_window=len(opens), rift_close_window=len(closes),
                  rift_contents=len(contents), play_packets_seen=PACKETS.total, notes=PACKETS.notes, events=PACKETS.events, client_logs=result.log_dir)
    (directory / "result.json").write_text(json.dumps(report, indent=2, default=str) + "\n")
    text = result.report().replace("PASS" if result.ok else "FAIL", "PASS" if ok else "FAIL", 1)
    text += (f"\n  menus: {len(queued)}/{len(STEPS)} steps queued, relay saw {len(opens)} open_window, "
             f"{len(contents)} content packets, {len(closes)} close_window for Rift containers"
             f"\n  server {server.name}; logs {directory}")
    text += "".join(f"\n  NOTE: {note}" for note in PACKETS.notes[:2])
    text += "".join(f"\n  PROBLEM: {problem}" for problem in problems[:10])
    return ok, report, text


def plan(args):
    """(backend, [clients]) groups: one backend start serves its clients in turn."""
    pairs = []
    if args.server:
        pairs += [(args.server, client) for client in args.client]
    else:
        pairs += [(client, client) for client in args.client]
    if args.cross:
        pairs += [(backend, client) for backend, client in CROSS]
    groups = {}
    for backend, client in pairs:
        groups.setdefault(backend, []).append(client)
    return list(groups.items())


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                     formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    parser.add_argument("--accept-eula", action="store_true")
    parser.add_argument("--binary", type=Path, default=fixtures.ROOT / "target" / "debug" / "rift")
    parser.add_argument("--client", default=",".join(ALL_CLIENTS), help="client versions, comma-separated")
    parser.add_argument("--server", help="one backend version (vanilla-<version> or a fixture name) for all clients")
    parser.add_argument("--cross", action="store_true", help="also 1.12.2 and 1.20.4 on a 26.3 backend")
    parser.add_argument("--only-cross", action="store_true", help="only the --cross runs")
    parser.add_argument("--stay", type=float, default=0, help="seconds in the world (default: long enough)")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()
    if not args.accept_eula:
        parser.error("--accept-eula is required to run the Minecraft servers and clients")
    args.client = [client.strip() for client in args.client.split(",") if client.strip()]
    if args.only_cross:
        args.client = []
        args.cross = True
    for client in args.client:
        client_protocol(client)
    binary = args.binary.resolve(strict=True)
    stay = args.stay or WORLD_SECONDS + len(STEPS) * STEP_SECONDS + HOLD_SECONDS
    out = args.out or RUNS / (time.strftime("%Y%m%d-%H%M%S") + "-menus")
    env.install_signal_handlers()
    observe_relay()
    reports, total = [], 0
    for backend, clients in plan(args):
        fixture = backend if "-" in backend or backend == "vanilla" else f"vanilla-{backend}"
        workdir = Path(tempfile.mkdtemp(prefix="rift-menus-"))
        server = fixtures.Server(fixture, workdir / "server")
        try:
            server.start()
            env.log(f"{server.name} ({server.release}, protocol {server.protocol}) on port {server.port}")
            for client in clients:
                total += 1
                ok, report, text = run(binary, server, client, stay, out / f"{server.name}-{client}")
                reports.append(report)
                print(text, flush=True)
        finally:
            server.stop()
            shutil.rmtree(workdir, ignore_errors=True)
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(reports, indent=2, default=str) + "\n")
    passed = sum(report["ok"] for report in reports)
    print(f"{'client':8} {'backend':16} result")
    for report in reports:
        print(f"{report['client']:8} {report['server']:16} {'PASS' if report['ok'] else 'FAIL'}  {report['reason'][:100]}")
    print(f"{passed}/{total} real client runs passed")
    if passed != total:
        raise SystemExit(1)


if __name__ == "__main__":
    os.environ.setdefault("PYTHONUNBUFFERED", "1")
    main()
