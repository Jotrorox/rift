#!/usr/bin/env python3
"""End-to-end test of inventory menus: a scripted client, the real Rift binary
and real servers.

Inventory menus (docs/menus.md) are screens Rift draws itself; the backend
never learns one is open. For each pair of (backend server, client version)
this script starts the server, starts Rift with a generated rift.lua that
shows menus from a join handler and from commands, joins through Rift with a
scripted client in the client's own protocol, and checks with PASS/FAIL:

    open        open_screen (window type, container ID 101..127, title) and
                one container_set_slot per filled slot (item, count, name,
                lore), for a chest, a hopper and a dispenser
    click       real clicks in the client's packet layout: which are
                reported, with every event field (the menu handler sends the
                player a chat line that encodes them)
    undo        the slots a click could have changed and an empty cursor come
                back in the packet of the client's version
    rows        clicks that touch the inventory rows make the backend send the
                inventory again (a 1.17 backend: when the menu closes); the
                backend's own view (/clear count, /data get or /testfor) never
                changes; after a menu a real click on the inventory works
    backend     nothing reaches the backend: a late click for a closed menu is
                dropped, a single-slot update while a menu is open is repaired
                when it closes
    endings     player, plugin (close button and close_menu), replaced,
                server (death), switch (rift.players.transfer to a second
                backend) and disconnect (seen by a second player), exactly one
                close event per menu
    alive       the client stays connected and decodes every packet, Rift logs
                no error or panic, the server no warning about the player

rift.lua player hooks need online mode, so the clients log in online against
a fake session server on 127.0.0.1 (pure-Python RSA and AES/CFB8) and Rift
forwards to the offline test backends in BungeeCord mode, which vanilla
ignores. The Rift binary must therefore be built with
`--features test-session-server` (it reads RIFT_TEST_SESSION_URL); the script
checks that first.

Usage (Python 3.11+; use /opt/homebrew/bin/python3 on macOS):

    tests/menus_minecraft.py --accept-eula --binary target/debug/rift --pair vanilla-1.16.5
    tests/menus_minecraft.py --accept-eula --binary BIN --pair vanilla-26.3:1.12.2,vanilla-26.3:1.20.4
    tests/menus_minecraft.py --accept-eula --binary BIN --set same       # SAME below
    tests/menus_minecraft.py --accept-eula --binary BIN --set translated # TRANSLATED below
    tests/menus_minecraft.py --list

A pair is SERVER[:CLIENT]; CLIENT defaults to the server's own version.
--no-switch skips the second backend (one server instead of two per pair).
Servers, JDKs and downloads are the translation matrix's (tests/matrix_env.py,
tests/matrix_fixtures.py) in ~/.cache/rift-via; at most RIFT_VIA_MAX_SERVERS
(here 2 by default) servers run at once. Each pair's client packet log, Rift
log, server log and result.json go under --out.

Checks for what later steps of docs/menus.md add (redraws that send only the
difference, update_menu, the stale-click rule, anvils) are steps in
OPTIONAL; --with NAME,... turns them on once the binary has them.
"""

import argparse
import http.server
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import sys
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parent))
os.environ.setdefault("RIFT_VIA_MAX_SERVERS", "2")

import matrix_env as env  # noqa: E402
import matrix_fixtures as fixtures  # noqa: E402
from matrix_client import Session, SessionError, offline_uuid, text_of  # noqa: E402
import matrix_registries  # noqa: E402
from matrix_scenario import CHAT, Commands  # noqa: E402
from menus_probe import CHANGED_SLOTS, STATE_IDS, empty, from_record, inventory, key, short, stack  # noqa: E402
from minecraft import varint  # noqa: E402

OUT = env.HOME / "menus-e2e"

SAME = ["vanilla-1.12.2", "vanilla-1.16.5", "vanilla-1.17", "vanilla-1.17.1", "vanilla-1.20.4",
        "vanilla-1.21.1", "vanilla-1.21.4", "vanilla-1.21.11", "vanilla-26.3", "paper-1.16.5",
        "paper-26.3", "spigot-1.12.2", "pumpkin"]
MORE = ["vanilla-1.13.2", "vanilla-1.14.4", "vanilla-1.18.2", "vanilla-1.19.4", "vanilla-1.20.6",
        "vanilla-1.21.5"]
TRANSLATED = ["vanilla-26.3:1.12.2", "vanilla-26.3:1.16.5", "vanilla-26.3:1.20.4",
              "vanilla-1.16.5:26.3", "vanilla-1.12.2:26.3", "vanilla-1.17:26.3"]

TRANSACTIONS_END = 755  # 1.17: no more transactions; clicks list changed slots.
OFFHAND = 735           # 1.16: F swaps with the off hand.
MENU_REGISTRY = 477     # 1.14: open_screen names a menu registry entry.
WIDE = 768              # 1.21.2: set_cursor_item, VarInt containers.
FLATTENING = 393        # 1.13

# --- Pure-Python cryptography for an online login ----------------------------
#
# A test client needs RSA PKCS#1 v1.5 encryption (the shared secret and the
# verify token) and AES-128 in CFB8 mode (the stream). Neither is in the
# standard library; both are short. Only encryption of single AES blocks is
# needed: CFB8 uses it for both directions.


def _aes_tables():
    sbox = [0] * 256
    p = q = 1
    while True:
        p = p ^ ((p << 1) & 0xFF) ^ (0x1B if p & 0x80 else 0)
        q ^= q << 1
        q ^= q << 2
        q ^= q << 4
        q &= 0xFF
        if q & 0x80:
            q ^= 0x09
        rot = lambda value, shift: ((value << shift) | (value >> (8 - shift))) & 0xFF
        sbox[p] = (q ^ rot(q, 1) ^ rot(q, 2) ^ rot(q, 3) ^ rot(q, 4) ^ 0x63) & 0xFF
        if p == 1:
            break
    sbox[0] = 0x63
    xtime = lambda a: ((a << 1) ^ (0x1B if a & 0x80 else 0)) & 0xFF
    tables = [[], [], [], []]
    for a in range(256):
        s = sbox[a]
        word = (xtime(s) << 24) | (s << 16) | (s << 8) | (xtime(s) ^ s)
        for index in range(4):
            shift = 8 * index
            tables[index].append(((word >> shift) | (word << (32 - shift))) & 0xFFFFFFFF if shift else word)
    return sbox, tables


SBOX, (T0, T1, T2, T3) = _aes_tables()


class Aes128:
    def __init__(self, key):
        words = [int.from_bytes(key[i:i + 4], "big") for i in range(0, 16, 4)]
        rcon = 1
        for i in range(4, 44):
            t = words[i - 1]
            if i % 4 == 0:
                t = ((t << 8) | (t >> 24)) & 0xFFFFFFFF
                t = (SBOX[t >> 24] << 24) | (SBOX[(t >> 16) & 255] << 16) | (SBOX[(t >> 8) & 255] << 8) \
                    | SBOX[t & 255]
                t ^= rcon << 24
                rcon = ((rcon << 1) ^ (0x1B if rcon & 0x80 else 0)) & 0xFF
            words.append(words[i - 4] ^ t)
        self.rk = words

    def first_byte(self, block):
        """The first byte of the encrypted block: all CFB8 needs."""
        rk = self.rk
        s0 = int.from_bytes(block[0:4], "big") ^ rk[0]
        s1 = int.from_bytes(block[4:8], "big") ^ rk[1]
        s2 = int.from_bytes(block[8:12], "big") ^ rk[2]
        s3 = int.from_bytes(block[12:16], "big") ^ rk[3]
        for k in range(4, 40, 4):
            s0, s1, s2, s3 = (
                T0[s0 >> 24] ^ T1[(s1 >> 16) & 255] ^ T2[(s2 >> 8) & 255] ^ T3[s3 & 255] ^ rk[k],
                T0[s1 >> 24] ^ T1[(s2 >> 16) & 255] ^ T2[(s3 >> 8) & 255] ^ T3[s0 & 255] ^ rk[k + 1],
                T0[s2 >> 24] ^ T1[(s3 >> 16) & 255] ^ T2[(s0 >> 8) & 255] ^ T3[s1 & 255] ^ rk[k + 2],
                T0[s3 >> 24] ^ T1[(s0 >> 16) & 255] ^ T2[(s1 >> 8) & 255] ^ T3[s2 & 255] ^ rk[k + 3])
        return SBOX[s0 >> 24] ^ (rk[40] >> 24)

    def block(self, block):
        """One whole encrypted block (for the self-test)."""
        rk = self.rk
        s = [int.from_bytes(block[i:i + 4], "big") ^ rk[i // 4] for i in range(0, 16, 4)]
        for k in range(4, 40, 4):
            s = [T0[s[i] >> 24] ^ T1[(s[(i + 1) % 4] >> 16) & 255] ^ T2[(s[(i + 2) % 4] >> 8) & 255]
                 ^ T3[s[(i + 3) % 4] & 255] ^ rk[k + i] for i in range(4)]
        out = b""
        for i in range(4):
            word = (SBOX[s[i] >> 24] << 24) | (SBOX[(s[(i + 1) % 4] >> 16) & 255] << 16) \
                | (SBOX[(s[(i + 2) % 4] >> 8) & 255] << 8) | SBOX[s[(i + 3) % 4] & 255]
            out += (word ^ rk[40 + i]).to_bytes(4, "big")
        return out


class Cfb8:
    """AES/CFB8 with the shared secret as key and IV, as Minecraft uses it."""

    def __init__(self, secret, decrypt):
        self.aes = Aes128(secret)
        self.register = bytearray(secret)
        self.decrypting = decrypt

    def update(self, data):
        out = bytearray(len(data))
        register, first = self.register, self.aes.first_byte
        for index, byte in enumerate(data):
            value = byte ^ first(bytes(register))
            out[index] = value
            del register[0]
            register.append(byte if self.decrypting else value)
        return bytes(out)


def _der(data, offset):
    """(tag, content start, content end) of the DER element at `offset`."""
    tag, length = data[offset], data[offset + 1]
    offset += 2
    if length & 0x80:
        count = length & 0x7F
        length = int.from_bytes(data[offset:offset + count], "big")
        offset += count
    return tag, offset, offset + length


def rsa_public_key(der):
    """(n, e) of an X.509 SubjectPublicKeyInfo holding an RSA key."""
    _, start, _ = _der(der, 0)                 # SubjectPublicKeyInfo
    _, _, algorithm_end = _der(der, start)     # AlgorithmIdentifier
    tag, bits, _ = _der(der, algorithm_end)    # BIT STRING
    assert tag == 0x03, "not an RSA public key"
    _, inner, _ = _der(der, bits + 1)          # RSAPublicKey SEQUENCE
    _, n_start, n_end = _der(der, inner)
    _, e_start, e_end = _der(der, n_end)
    return int.from_bytes(der[n_start:n_end], "big"), int.from_bytes(der[e_start:e_end], "big")


def rsa_encrypt(public_key, message):
    n, e = public_key
    size = (n.bit_length() + 7) // 8
    padding = bytearray()
    while len(padding) < size - len(message) - 3:
        padding += bytes(b for b in secrets.token_bytes(16) if b)
    block = b"\0\2" + bytes(padding[:size - len(message) - 3]) + b"\0" + message
    return pow(int.from_bytes(block, "big"), e, n).to_bytes(size, "big")


def crypto_self_test():
    """FIPS-197's AES-128 vector; a CFB8 round trip."""
    aes = Aes128(bytes(range(16)))
    assert aes.block(bytes.fromhex("00112233445566778899aabbccddeeff")).hex() == \
        "69c4e0d86a7b0430d8cdb78070b4c55a", "AES self-test failed"
    secret = bytes(range(16, 32))
    data = bytes(range(200))
    assert Cfb8(secret, True).update(Cfb8(secret, False).update(data)) == data


# --- The fake session server ----------------------------------------------------

class SessionServer:
    """Answers Rift's hasJoined for any name with the offline UUID, so the
    backend (which ignores forwarding) and Rift agree on who the player is."""

    def __init__(self):
        outer = self
        self.requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                query = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
                name = query.get("username", [""])[0]
                outer.requests.append(name)
                body = json.dumps({"id": offline_uuid(name).hex, "name": name, "properties": []}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/session/minecraft/hasJoined"

    def stop(self):
        self.server.shutdown()
        self.server.server_close()


# --- An online client -------------------------------------------------------------

class DecryptingReader:
    def __init__(self, raw, cipher):
        self.raw, self.cipher = raw, cipher

    def read(self, size):
        return self.cipher.update(self.raw.read(size))

    def close(self):
        self.raw.close()


class Player(Session):
    """A matrix client that logs in to an online-mode Rift."""

    def __init__(self, *args, **kwargs):
        self.encryptor = None
        super().__init__(*args, **kwargs)

    def on_login(self, record):
        if record.name == "encryption_begin" and record.value is not None:
            value = record.value
            secret = secrets.token_bytes(16)
            key = rsa_public_key(value["publicKey"])
            reply = {"sharedSecret": rsa_encrypt(key, secret),
                     "verifyToken": rsa_encrypt(key, value["verifyToken"])}
            if self.protocol in (759, 760):
                reply = {"sharedSecret": reply["sharedSecret"], "hasVerifyToken": True,
                         "crypto": {"verifyToken": reply["verifyToken"]}}
            self.send_packet("encryption_begin", reply)
            self.reader = DecryptingReader(self.reader, Cfb8(secret, True))
            self.encryptor = Cfb8(secret, False)
            return
        super().on_login(record)

    def send(self, packet_id, body=b""):
        packet = varint(packet_id) + body
        if self.threshold is not None:
            self.sent_compressed_packets += len(packet) >= self.threshold
            packet = (varint(len(packet)) + zlib.compress(packet)
                      if len(packet) >= self.threshold else b"\0" + packet)
        frame = varint(len(packet)) + packet
        if self.encryptor is not None:
            frame = self.encryptor.update(frame)
        self.socket.sendall(frame)

    def close(self):
        with self.lock:
            self.closing = True
        try:
            self.socket.shutdown(2)
        except OSError:
            pass
        self.ended.wait(5)
        try:
            self.reader.close()
        except OSError:
            pass
        self.socket.close()


# --- Texts and stacks ----------------------------------------------------------------

LEGACY_CODE = re.compile("§.")


def strings(value):
    """Every string inside a decoded value, JSON text expanded."""
    if isinstance(value, bytes):
        return
    if isinstance(value, str):
        stripped = value.strip()
        if stripped[:1] in "{[\"":
            try:
                yield from strings(json.loads(stripped))
                return
            except ValueError:
                pass
        yield LEGACY_CODE.sub("", value)
    elif isinstance(value, dict):
        for entry in value.values():
            yield from strings(entry)
    elif isinstance(value, (list, tuple)):
        for entry in value:
            yield from strings(entry)


def plain(value):
    return "\n".join(strings(value))


def chat_text(record):
    value = record.value or {}
    parts = [text_of(value.get(field)) for field in ("message", "content", "plainMessage",
                                                    "formattedMessage", "unsignedChatContent")
             if value.get(field) is not None]
    return " ".join(parts) or plain(value)


def item_id(value):
    return value.get("itemId", value.get("blockId")) if isinstance(value, dict) else None


# --- The configuration --------------------------------------------------------------

# Chest layout: slot numbers of the buttons.
BUY, DECORATION, ANY, REPLACE, CLOSE_BY_PLUGIN, CLOSE_BUTTON, SWITCH = 11, 13, 15, 19, 21, 23, 25

MENUS = {
    # id: (kind, rows or None, title, {slot: (item, count, name, lore)})
    "welcome": ("chest", 1, "Welcome", {4: ("diamond", 1, "Hello", [])}),
    "chest": ("chest", 3, "Rift test chest", {
        **{slot: ("glass_pane", 1, None, []) for slot in (0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 17, 18, 26)},
        BUY: ("emerald", 3, "Buy", ["Line one", "Line two"]),
        DECORATION: ("stone", 1, "Decoration", []),
        ANY: ("compass", 1, "Any click", []),
        REPLACE: ("paper", 1, "Replace", []),
        CLOSE_BY_PLUGIN: ("gold_ingot", 1, "Close by plugin", []),
        CLOSE_BUTTON: ("barrier", 1, "Close", []),
        SWITCH: ("arrow", 1, "Switch", []),
    }),
    "hopper": ("hopper", None, "Rift test hopper", {
        0: ("glass_pane", 1, None, []), 2: ("diamond", 1, "Hop", []), 4: ("glass_pane", 1, None, [])}),
    "dispenser": ("dispenser", None, "Rift test dispenser", {4: ("gold_ingot", 2, "Gold", [])}),
}
SLOT_COUNT = {"chest": None, "hopper": 5, "dispenser": 9}

# The menus, the event report and the menu handlers: the same Lua in rift.lua
# (script mode) and in a persistent Lua plugin (plugin mode).
MENUS_LUA = r'''
local function s(v) if v == nil then return "nil" end return tostring(v) end
local function enc(v)
    if type(v) == "table" then return s(v.tag) .. ":" .. s(v.n) end
    return s(v)
end

-- Every event, every field, to every player: the clients read it as chat.
rift.on("menu", function(ctx)
    rift.players.broadcast("MENUEV who=" .. s(ctx.name) .. " menu=" .. s(ctx.menu)
        .. " kind=" .. s(ctx.kind) .. " action=" .. s(ctx.action) .. " slot=" .. s(ctx.slot)
        .. " click=" .. s(ctx.click) .. " key=" .. s(ctx.key) .. " payload=" .. enc(ctx.payload)
        .. " reason=" .. s(ctx.reason) .. " state=" .. enc(ctx.state) .. " server=" .. s(ctx.server))
end)

local GLASS = { item = "glass_pane", name = " " }
local HOPPER = {
    id = "hopper", type = "hopper", title = "Rift test hopper", state = { tag = "st-hopper", n = 2 },
    layout = { "H E H" },
    keys = { H = GLASS, E = { item = "diamond", name = "Hop", id = "hop", payload = { tag = "hop", n = 2 } } },
}
local MENUS = {
    welcome = {
        id = "welcome", rows = 1, title = "Welcome", state = { tag = "st-welcome", n = 0 },
        slots = { { slot = 4, item = "diamond", name = "Hello", id = "hello" } },
    },
    chest = {
        id = "chest", title = { mini = "<dark_gray>Rift test chest" }, state = { tag = "st-chest", n = 1 },
        layout = { "#########", "# B D A #", "#R P X T#" },
        keys = {
            ["#"] = GLASS,
            B = { item = "emerald", count = 3, name = { mini = "<green>Buy" }, lore = { "Line one", "Line two" },
                  id = "buy", payload = { tag = "buy", n = 5 } },
            D = { item = "stone", name = "Decoration" },
            A = { item = "compass", name = "Any click", id = "any", payload = { tag = "any", n = 7 }, clicks = "all" },
            R = { item = "paper", name = "Replace", id = "replace" },
            P = { item = "gold_ingot", name = "Close by plugin", id = "closeme" },
            X = { item = "barrier", name = { mini = "<red>Close" }, close = true },
            T = { item = "arrow", name = "Switch", id = "switch" },
        },
    },
    hopper = HOPPER,
    dispenser = {
        id = "dispenser", type = "dispenser", title = "Rift test dispenser", state = { tag = "st-disp", n = 3 },
        layout = { "   ", " G ", "   " },
        keys = { G = { item = "gold_ingot", count = 2, name = "Gold", id = "gold", payload = { tag = "gold", n = 3 } } },
    },
}

rift.menus.on("chest", {
    click = {
        replace = function(ctx) rift.players.menu(ctx.uuid, HOPPER) end,
        closeme = function(ctx) rift.players.close_menu(ctx.uuid, "chest") end,
        switch = function(ctx) @TRANSFER@ end,
    },
})
'''

# Script mode: rift.lua shows menus from a join handler and from commands.
SCRIPT_LUA = r'''
rift.command("rmenu", { permission = "", run = function(ctx)
    local menu = MENUS[ctx.args]
    if not menu then return { message = "no menu " .. s(ctx.args) } end
    if not rift.players.menu(ctx.uuid, menu) then return { message = "MENUFAIL " .. ctx.args } end
end })
rift.command("rclose", { permission = "", run = function(ctx)
    local id = ctx.args
    if id == "" then id = nil end
    rift.players.close_menu(ctx.uuid, id)
end })
rift.on("join", function(ctx)
    if ctx.server == "main" then rift.players.menu(ctx.uuid, MENUS.welcome) end
end)
'''

# Plugin mode: the plugin's http hook (not a player hook, so it runs without
# online mode) takes "show PLAYER MENU" and "close PLAYER [MENU]".
PLUGIN_LUA = r'''
local function words(text)
    local list, start = {}, 1
    for index = 1, #text + 1 do
        local byte = string.byte(text, index)
        if byte == nil or byte == 32 then
            if index > start then list[#list + 1] = string.sub(text, start, index - 1) end
            start = index + 1
        end
    end
    return list
end
rift.on("http", function(request)
    if request.method ~= "POST" or request.path ~= "/ext/menu" then return nil end
    local w = words(request.body or "")
    local ok
    if w[1] == "show" then
        ok = rift.players.menu(w[2], MENUS[w[3]])
    elseif w[1] == "close" then
        ok = rift.players.close_menu(w[2], w[3])
    else
        return { status = 400, body = "unknown verb" }
    end
    return { body = "queued=" .. tostring(ok) }
end)
'''

PLUGIN_TOML = '''name = "menutest"
version = "0.1.0"
api = "0.2"
engine = "lua"

[capabilities]
network = false
env = []
'''


# --- Rift ------------------------------------------------------------------------------

class Rift:
    """Rift with the test's menus: in rift.lua (script mode, online, needs a
    test-session-server build) or in a persistent Lua plugin (plugin mode)."""

    def __init__(self, binary, directory, mode, main, second, session_url):
        self.binary, self.directory, self.mode = binary, Path(directory), mode
        self.main, self.second, self.session_url = main, second, session_url
        self.log = self.directory / "rift.log"
        self.proc = self.port = self.web = None
        self.token = "menus-" + uuid.uuid4().hex

    def start(self):
        self.port = env.allocate_port()
        transfer = 'rift.players.message(ctx.uuid, "no second backend")'
        lines = ['local config = require("rift.config")',
                 f'config.listeners.public = "127.0.0.1:{self.port}"',
                 f'config.backends.main = {{ address = "127.0.0.1:{self.main.port}", version = "{self.main.release}" }}']
        if self.second is not None:
            lines.append(f'config.backends.second = {{ address = "127.0.0.1:{self.second.port}", '
                         f'version = "{self.second.release}" }}')
            transfer = 'rift.players.transfer(ctx.uuid, "second")'
        lines.append('config.routes.public = "main"')
        menus = MENUS_LUA.replace("@TRANSFER@", transfer)
        environment = dict(os.environ)
        if self.mode == "script":
            lines += ["config.authentication = { online_mode = true }", 'config.forwarding = { mode = "bungeecord" }',
                      menus, SCRIPT_LUA]
            environment["RIFT_TEST_SESSION_URL"] = self.session_url
        else:
            self.web = env.allocate_port()
            lines += [f'config.web = {{ enabled = true, listen = "127.0.0.1:{self.web}", ui = false, '
                      f'token = "{self.token}" }}', 'rift.plugin("menutest")']
            folder = self.directory / "plugins" / "menutest"
            folder.mkdir(parents=True, exist_ok=True)
            (folder / "plugin.toml").write_text(PLUGIN_TOML)
            (folder / "init.lua").write_text(menus + PLUGIN_LUA)
        config = self.directory / "rift.lua"
        config.write_text("\n".join(lines) + "\n")
        self.proc = env.PROCESSES.spawn([str(self.binary), "--config", str(config)], self.directory,
                                        self.log, env=environment, stdin=False)
        deadline = time.monotonic() + 60
        while "rift: listening on" not in self.text():
            if self.proc.poll() is not None:
                raise RuntimeError(f"rift exited ({self.proc.returncode}):\n{self.text()[-3000:]}")
            if time.monotonic() > deadline:
                raise TimeoutError(f"rift did not start:\n{self.text()[-3000:]}")
            time.sleep(0.05)
        if self.mode == "script" and "RIFT_TEST_SESSION_URL is set" not in self.text():
            raise SystemExit("this Rift binary ignores RIFT_TEST_SESSION_URL: build it with "
                             "`cargo build --features test-session-server` for --mode script (rift.lua player "
                             "hooks need online mode, and the test clients cannot log in with Mojang)")
        return self

    def post(self, body):
        """Plugin mode: one request to the plugin's http hook; (status, body)."""
        request = urllib.request.Request(f"http://127.0.0.1:{self.web}/ext/menu", data=body.encode(),
                                         method="POST", headers={"Authorization": f"Bearer {self.token}"})
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                return response.status, response.read().decode(errors="replace")
        except urllib.error.HTTPError as error:
            return error.code, error.read().decode(errors="replace")
        except OSError as error:
            return 0, str(error)

    def text(self):
        try:
            return self.log.read_text(errors="replace")
        except FileNotFoundError:
            return ""

    def offset(self):
        return len(self.text())

    def stop(self):
        if self.proc is not None:
            env.PROCESSES.stop(self.proc, grace=3)
            self.proc = None
        for port in (self.port, self.web):
            if port is not None:
                env.release_port(port)
        self.port = self.web = None


RIFT_PROBLEM = re.compile(r"rift: error|panicked|RUST_BACKTRACE|rift: warning", re.I)
RIFT_EXPECTED = re.compile(r"RIFT_TEST_SESSION_URL is set|This build is for tests only")


# --- One pair -----------------------------------------------------------------------------

class Skip(Exception):
    pass


class Pair:
    """One client protocol on a running server (and Rift)."""

    def __init__(self, server, second, rift, protocol, directory, options):
        self.server, self.second, self.rift = server, second, rift
        self.protocol = protocol  # The client's.
        self.backend = server.protocol
        self.software = server.software
        self.cmd = Commands(server.protocol, server.software)
        self.directory = directory
        self.options = options
        self.label = f"{server.name}<-{fixtures.RELEASE_OF[protocol]}"
        self.client = self.observer = None
        self.registries = None
        self.checks = []
        self.notes = []
        self.section = "setup"
        self.container = None   # Rift's container of the open menu.
        self.menu = None        # Its id.
        self.base = None        # First inventory-row slot of the open menu's window.
        self.opened = []        # Menu ids in the order they opened.
        self.action = 0
        self.baseline = None
        self.view = None
        self.left = False       # The client disconnected on purpose.

    # Plumbing

    def check(self, name, ok, detail=""):
        self.checks.append(dict(section=self.section, name=name, ok=bool(ok), detail=str(detail)[:1500]))
        env.log(f"[{self.label}] {'PASS' if ok else 'FAIL'} {self.section}: {name}"
                f"{': ' + str(detail)[:400] if detail and not ok else ''}")
        return bool(ok)

    def note(self, text):
        self.notes.append(f"{self.section}: {text}")
        env.log(f"[{self.label}] note {self.section}: {text}")

    def console(self, line, pause=0.0, server=None):
        (server or self.server).console(line)
        if pause:
            time.sleep(pause)

    def settle(self, seconds=0.8):
        time.sleep(seconds)
        self.client.check()

    def show(self, menu_id):
        """Ask for a menu: a command in script mode, the plugin's http hook in plugin mode."""
        if self.rift.mode == "script":
            self.command(f"rmenu {menu_id}")
            return
        status, body = self.rift.post(f"show {self.client.name} {menu_id}")
        self.check(f"{menu_id}: rift.players.menu queued it", status == 200 and "queued=true" in body,
                   f"HTTP {status} {body[-300:]}")

    def close_any(self):
        """close_menu without an id."""
        if self.rift.mode == "script":
            self.command("rclose")
            return
        status, body = self.rift.post(f"close {self.client.name}")
        self.check("close_menu queued", status == 200 and "queued=true" in body, f"HTTP {status} {body[-300:]}")

    def command(self, text, session=None):
        session = session or self.client
        if session.has("chat_command"):
            session.send_packet("chat_command", {"command": text, "timestamp": int(time.time() * 1000)})
        else:
            session.send_packet("chat", {"message": "/" + text})

    # Events

    def events(self, since=0, session=None, who=None):
        session = session or self.client
        who = who or self.client.name
        found = []
        for record in session.packets(CHAT, since):
            text = chat_text(record)
            index = text.find("MENUEV ")
            if index < 0:
                continue
            fields = dict(part.split("=", 1) for part in text[index + 7:].split() if "=" in part)
            if fields.get("who") == who:
                found.append(fields)
        return found

    def wait_event(self, mark, timeout=6.0, session=None, who=None, **wanted):
        deadline = time.monotonic() + timeout
        while True:
            for event in self.events(mark, session, who):
                if all(event.get(k) == str(v) for k, v in wanted.items()):
                    return event
            if time.monotonic() > deadline:
                return None
            (session or self.client).check()
            time.sleep(0.05)

    def expect_event(self, name, mark, only=False, **wanted):
        """Exactly one event since `mark` matches; with `only`, no other came either."""
        event = self.wait_event(mark, **wanted)
        time.sleep(0.2)
        seen = self.events(mark)
        matching = [e for e in seen if all(e.get(k) == str(v) for k, v in wanted.items())]
        ok = len(matching) == 1 and (not only or len(seen) == 1)
        return self.check(name, ok, f"wanted {wanted}, got {seen}"), event

    def expect_no_event(self, name, mark, wait=1.2):
        time.sleep(wait)
        seen = self.events(mark)
        return self.check(name, not seen, f"got {seen}")

    # Stacks

    def item_name(self, value):
        if self.registries is None:
            return None
        return self.registries.name("item", item_id(value))

    def judge_stack(self, what, value, expected):
        item, count, name, lore = expected
        problems = []
        got = self.item_name(value)
        if got is not None and got != f"minecraft:{item}":
            problems.append(f"item {got} (id {item_id(value)}), not {item}")
        if not isinstance(value, dict) or value.get("itemCount") != count:
            problems.append(f"count {value.get('itemCount') if isinstance(value, dict) else value}, not {count}")
        text = plain(value)
        for part in ([name] if name else []) + list(lore):
            if part not in text:
                problems.append(f"{part!r} not in the stack's texts {text[:200]!r}")
        return problems

    # Opening

    def window_type_ok(self, value, kind, rows):
        if self.protocol < MENU_REGISTRY:
            names = {"chest": "minecraft:chest", "hopper": "minecraft:hopper", "dispenser": "minecraft:dispenser"}
            slots = rows * 9 if kind == "chest" else SLOT_COUNT[kind]
            ok = value.get("inventoryType") == names[kind] and value.get("slotCount") == slots
            return ok, f"{value.get('inventoryType')} with {value.get('slotCount')} slots"
        names = {"chest": f"minecraft:generic_9x{rows}", "hopper": "minecraft:hopper",
                 "dispenser": "minecraft:generic_3x3"}
        got = self.registries.name("menu", value.get("inventoryType")) if self.registries else None
        if got is None or got.startswith("<"):
            return True, f"menu ID {value.get('inventoryType')} (no registry names to compare)"
        return got == names[kind], f"menu {got} (ID {value.get('inventoryType')})"

    def expect_open(self, menu_id, mark, timeout=8):
        kind, rows, title, slots = MENUS[menu_id]
        try:
            record = self.client.wait_packet("open_window", f"open_screen for {menu_id}", timeout, mark)
        except SessionError as error:
            self.check(f"{menu_id}: open_screen arrives", False, error)
            return False
        value = record.value
        container = value["windowId"]
        self.container, self.menu = container, menu_id
        self.base = rows * 9 if kind == "chest" else SLOT_COUNT[kind]
        self.opened.append(menu_id)
        self.check(f"{menu_id}: open_screen with a container ID in 101..127", 101 <= container <= 127,
                   f"container {container}")
        ok, detail = self.window_type_ok(value, kind, rows)
        self.check(f"{menu_id}: window type for the client's version", ok, detail)
        self.check(f"{menu_id}: title", title in plain(value.get("windowTitle")),
                   f"title {plain(value.get('windowTitle'))!r}")
        time.sleep(0.8)
        sets = [r for r in self.client.packets("set_slot", record.index)
                if r.value.get("windowId") == container]
        by_slot = {}
        for r in sets:
            by_slot.setdefault(r.value["slot"], []).append(r.value)
        self.check(f"{menu_id}: one container_set_slot per filled slot",
                   sorted(by_slot) == sorted(slots) and all(len(v) == 1 for v in by_slot.values()),
                   f"slots sent {sorted((s, len(v)) for s, v in by_slot.items())}, filled {sorted(slots)}")
        problems = []
        for slot, expected in slots.items():
            if slot not in by_slot:
                continue
            problems += [f"slot {slot}: {p}" for p in self.judge_stack(menu_id, by_slot[slot][0]["item"], expected)]
        self.check(f"{menu_id}: every stack has its item, count, name and lore", not problems, "; ".join(problems))
        self.stacks = {slot: values[0]["item"] for slot, values in by_slot.items()}
        return True

    # Clicks

    def state_id(self):
        state = 0
        for record in self.client.packets(("set_slot", "window_items")):
            if record.value.get("windowId") == self.container and "stateId" in record.value:
                state = record.value["stateId"]
        return state

    def send_click(self, slot, button, mode, changed=(), cursor=None, item=None, container=None):
        p = self.protocol
        value = {"windowId": self.container if container is None else container,
                 "slot": slot, "mouseButton": button, "mode": mode}
        if p >= CHANGED_SLOTS:
            value["changedSlots"] = [{"location": location, "item": entry} for location, entry in changed]
            value["cursorItem"] = empty(p) if cursor is None else cursor
            if p >= STATE_IDS:
                value["stateId"] = self.state_id()
        else:
            self.action += 1
            value["action"] = self.action
            value["item"] = empty(p) if item is None else item
        self.client.send_packet("window_click", value)

    def stack_at(self, slot):
        return from_record(self.protocol, self.stacks.get(slot))

    def hotbar(self, index):
        return self.base + 27 + index

    def real_click(self, kind, slot, key=None):
        """A click as the real client sends it: what it predicted is in
        `changed` and `cursor` (1.17+), the clicked stack in `item` (older)."""
        p, here = self.protocol, self.stack_at(slot) if 0 <= slot < self.base else None
        held = here or empty(p)
        if slot >= self.base:
            held = from_record(p, json.loads(self.baseline_slot(slot)) if self.baseline_slot(slot) else None)
        layouts = {
            "left": (0, 0, [(slot, empty(p))], held),
            "right": (1, 0, [(slot, empty(p))], held),
            "shift_left": (0, 1, [(slot, empty(p)), (self.hotbar(8), held)], None),
            "shift_right": (1, 1, [(slot, empty(p)), (self.hotbar(8), held)], None),
            "drop": (0, 4, [(slot, empty(p))], None),
            "drop_all": (1, 4, [(slot, empty(p))], None),
            "number_key": ((key or 1) - 1, 2, [(slot, empty(p)), (self.hotbar((key or 1) - 1), held)], None),
            "offhand": (40, 2, [(slot, empty(p))], None),
            "middle": (2, 3, [], None),
            "outside": (0, 0, [], None),
        }
        button, mode, changed, cursor = layouts[kind]
        if kind == "outside":
            slot = -999
        self.send_click(slot, button, mode, changed=changed, cursor=cursor, item=held)

    def baseline_slot(self, window_slot):
        """The baseline's entry for an inventory-row slot of the open window."""
        index = window_slot - self.base  # 0..26 main, 27..35 hotbar
        return self.baseline["slots"][9 + index] if 0 <= index < 36 else None

    def touches_rows(self, kind):
        return kind in ("shift_left", "shift_right", "number_key", "offhand", "inventory")

    def judge_undo(self, label, mark, slots):
        """The clicked menu slots again with their stacks, and an empty cursor."""
        time.sleep(0.6)
        records = self.client.packets(("set_slot", "set_cursor_item"), mark)
        resent = {}
        cursor = []
        for r in records:
            v = r.value
            if r.name == "set_cursor_item":
                cursor.append(("set_cursor_item", key(v.get("contents")) is None))
            elif v.get("windowId") in (-1, 255) and v.get("slot") == -1:  # 1.20.5-1.21.1 schemas read a u8.
                cursor.append(("set_slot(-1)", key(v.get("item")) is None))
            elif v.get("windowId") == self.container:
                resent[v["slot"]] = v["item"]
        problems = []
        for slot in slots:
            if slot not in resent:
                problems.append(f"slot {slot} not sent again")
            elif key(resent[slot]) != key(self.stacks.get(slot)):
                problems.append(f"slot {slot} sent as {short(resent[slot])}, shown {short(self.stacks.get(slot))}")
        self.check(f"{label}: the menu slots it could change come back", not problems,
                   "; ".join(problems) + f" (sent {sorted(resent)})")
        wanted = "set_cursor_item" if self.protocol >= WIDE else "set_slot(-1)"
        self.check(f"{label}: the cursor is emptied with {wanted}", (wanted, True) in cursor, f"cursor packets {cursor}")

    def judge_rows(self, label, mark, touched):
        """After a click that touched the inventory rows, the backend's whole
        inventory again; after one that did not, no request."""
        claim_all = self.backend == 755
        full = lambda: [r for r in self.client.packets("window_items", mark) if r.value.get("windowId") == 0]
        if touched and not claim_all:
            deadline = time.monotonic() + 4
            while not full() and time.monotonic() < deadline:
                time.sleep(0.1)
            got = full()
            ok = bool(got)
            detail = f"{len(got)} container_set_content(0)"
            if ok:
                after = inventory(self.client).snapshot()
                ok = after["slots"] == self.baseline["slots"]
                if not ok:
                    detail += f"; inventory now {self.describe_inventory(after)}, baseline " \
                              f"{self.describe_inventory(self.baseline)}"
            self.check(f"{label}: the backend's inventory comes back with the real items", ok, detail)
        elif touched:
            time.sleep(0.8)
            self.check(f"{label}: a 1.17 backend is not asked while the menu is open", not full(),
                       f"{len(full())} container_set_content(0)")
        else:
            time.sleep(0.4)
            self.check(f"{label}: no inventory request for a click that leaves the rows alone", not full(),
                       f"{len(full())} container_set_content(0)")
        leaked = [r.value for r in self.client.packets("transaction", mark)
                  if r.value.get("windowId") == 0 and r.value.get("action") == -1]
        if self.protocol < TRANSACTIONS_END:
            self.check(f"{label}: Rift's rejection does not reach the client", not leaked, leaked)

    @staticmethod
    def describe_inventory(snapshot):
        return {i: short(json.loads(v)) for i, v in enumerate(snapshot["slots"]) if v}

    def click_case(self, label, kind, slot, report=None, key=None):
        """One click: the event (or none), the undo, the inventory rows."""
        self.client.check()
        mark = self.client.mark()
        self.real_click(kind, slot, key)
        menu_slots = [slot] if 0 <= slot < self.base else []
        if self.protocol >= CHANGED_SLOTS and kind == "inventory":
            menu_slots = []
        if report:
            self.expect_event(f"{label}: reported", mark, only=True, kind="click", menu=self.menu, **report)
        else:
            self.expect_no_event(f"{label}: not reported", mark, wait=0.8)
        self.judge_undo(label, mark, menu_slots)
        self.judge_rows(label, mark, self.touches_rows(kind) or (kind == "left" and slot >= self.base))

    # The backend's view

    def server_view(self):
        if self.software == "pumpkin":
            return None  # Pumpkin's console output arrives only when it exits.
        name = self.client.name
        view = {}
        for item in ("diamond", "iron_ingot"):
            offset = self.server.offset()
            pattern = self.cmd.clear_count_pattern(name, item)
            self.console(self.cmd.clear_count(name, item))
            line = self.server.wait_log(f"{pattern}|No items were found|Could not clear|No items", 6, offset)
            match = re.search(pattern, line or "")
            view[item] = int(match.group(1)) if match else (0 if line else None)
        if self.backend >= FLATTENING:
            offset = self.server.offset()
            self.console(f"data get entity {name} Inventory")
            line = self.server.wait_log(r"has the following entity data|No entity was found", 6, offset)
            view["data"] = re.sub(r"^.*?\]: ", "", line or "")
        return view

    def item_in_slot(self, slot, item):
        """Whether the backend says the player has `item` in inventory slot `slot` (NBT numbering)."""
        name = self.client.name
        offset = self.server.offset()
        if self.backend < FLATTENING:
            self.console(f'testfor {name} {{Inventory:[{{Slot:{slot}b,id:"minecraft:{item}"}}]}}')
            line = self.server.wait_log(rf"Found {name}|did not match|not match the required", 6, offset)
            return line is not None and f"Found {name}" in line
        self.console(f"data get entity {name} Inventory")
        line = self.server.wait_log(r"has the following entity data", 6, offset) or ""
        for entry in re.findall(r"\{[^{}]*\}", line):
            if re.search(rf"Slot: {slot}b", entry) and f'"minecraft:{item}"' in entry:
                return True
        return False

    # The run

    def run(self):
        name = "M" + uuid.uuid4().hex[:9]
        pumpkin = self.software == "pumpkin"
        try:
            self.registries = matrix_registries.registries(self.protocol)
        except Exception as error:  # Only item and menu names need them.
            self.note(f"no registry names for the client's version: {error}")
        self.section = "open"
        self.client = Player(self.rift.port, name, self.protocol, label=name, pumpkin=pumpkin)
        mark = 0
        self.client.join()
        # Script mode: the join handler shows a 1-row chest as soon as the
        # player is in a world. Plugin mode: the http hook, at once.
        if self.rift.mode == "plugin":
            self.show("welcome")
        if self.expect_open("welcome", mark, timeout=10):
            close_mark = self.client.mark()
            self.client.send_packet("close_window", {"windowId": self.container})
            self.section = "endings"
            self.expect_event("welcome closed by the player: reason player", close_mark,
                              menu="welcome", kind="close", reason="player", state="st-welcome:0")
        self.prepare()
        for step in STEPS:
            if step.__name__ in OPTIONAL and step.__name__ not in self.options.with_:
                continue
            self.section = step.__name__.removeprefix("step_")
            try:
                step(self)
            except Skip as skip:
                self.note(f"skipped: {skip}")
            except SessionError as error:
                self.check("the step ran to the end", False, error)
                if self.client.failure:
                    break
        self.section = "alive"
        self.final_checks()

    def prepare(self):
        self.section = "setup"
        name = self.client.name
        # Peaceful: a survival player on Paper's flat world is otherwise killed by slimes mid-test.
        for line in ("difficulty peaceful", self.cmd.gamemode(name, "survival"), "gamerule keepInventory true",
                     "gamerule keep_inventory true", f"clear {name}"):
            self.console(line, 0.3)
        mark = self.client.mark()
        self.console(f"give {name} minecraft:diamond 5", 0.6)
        self.console(f"give {name} minecraft:iron_ingot 3", 1.0)
        model = inventory(self.client)
        filled = model.filled()
        self.check("the player has the two stacks given", len(filled) == 2,
                   f"{ {i: short(v) for i, v in filled.items()} }")
        if len(filled) != 2:
            raise SessionError(f"inventory setup failed: {filled} (packets since {mark})")
        self.baseline = model.snapshot()
        self.diamond_slot = min(filled)
        self.view = self.server_view()

    def final_checks(self):
        client = self.client
        if not self.left:
            self.check("the client was never disconnected", not client.failure, client.failure or "")
        else:
            self.check("the client was never disconnected before it left", True)
        failures = client.failures()
        self.check("the client decoded every packet", not failures,
                   "; ".join(f"{r.name}: {r.error}" for r in failures[:3]))
        text = self.rift.text()[self.rift_offset:]
        problems = [line for line in text.splitlines() if RIFT_PROBLEM.search(line) and not RIFT_EXPECTED.search(line)]
        self.check("Rift logged no error, warning or panic", not problems, " | ".join(problems[-6:]))
        log = self.server.text()[self.server_offset:]
        if self.second is not None:
            log += self.second.text()[self.second_offset:]
        pattern = re.compile(r"WARN|ERROR|Exception|kick|moved (too quickly|wrongly)|lost connection|"
                             r"Disconnecting|invalid|desync", re.I)
        noise = re.compile(r"Can't keep up|Running \d+ms|logged in with entity id|joined the game|"
                           r"lost connection: Disconnected|Unknown or incomplete command|Unknown command|"
                           r"Incorrect argument|No player was found|Expected whitespace|"
                           r"id-mapper|legacy|deprecated|SLF4J|tps|overloaded|Unable to find spawn|"
                           r"Invalid game rule|Unknown game ?rule|left the game")
        bad, after_death = [], 0
        for line in log.splitlines():
            if f"Killed {self.client.name}" in line or f"{self.client.name} fell out of the world" in line:
                after_death = 4  # The scripted client reports its old position once after respawning.
                continue
            after_death = max(0, after_death - 1)
            if after_death and "moved too quickly" in line:
                continue
            if pattern.search(line) and not noise.search(line) and (
                    self.client.name in line or "Exception" in line or "ERROR" in line):
                bad.append(line.strip())
        self.check("the server logged no warning about the player", not bad, " | ".join(bad[-5:]))


# --- The steps ---------------------------------------------------------------------------

def open_menu(pair, menu_id):
    mark = pair.client.mark()
    pair.show(menu_id)
    if not pair.expect_open(menu_id, mark):
        raise Skip(f"{menu_id} did not open")
    return mark


def close_expected(pair, label, mark, menu, reason, state, rift_closes):
    """The close event, and for an ending the client does not know of, Rift's container_close."""
    pair.expect_event(f"{label}: reason {reason}", mark, menu=menu, kind="close", reason=reason, state=state)
    if rift_closes:
        closes = [r.value for r in pair.client.packets("close_window", mark)]
        pair.check(f"{label}: Rift closes the screen on the client",
                   any(v.get("windowId") == pair.container for v in closes), f"container_close {closes}")


def step_clicks(pair):
    """The chest: every kind of click on a reporting button, decoration, a
    button with clicks = "all", the inventory rows and outside the window."""
    open_menu(pair, "chest")
    p = pair.protocol
    buy = dict(action="buy", slot=BUY, payload="buy:5", state="st-chest:1", key="nil")
    pair.click_case("left on a button", "left", BUY, report=dict(buy, click="left"))
    pair.click_case("right on a button", "right", BUY, report=dict(buy, click="right"))
    pair.click_case("shift-left on a button", "shift_left", BUY, report=dict(buy, click="shift_left"))
    pair.click_case("shift-right on a button", "shift_right", BUY, report=dict(buy, click="shift_right"))
    pair.click_case("left on decoration", "left", DECORATION)
    pair.click_case("Q on a default button", "drop", BUY)
    pair.click_case("number key 3 on a default button", "number_key", BUY, key=3)
    if p >= OFFHAND:
        pair.click_case("F on a default button", "offhand", BUY)
    anything = dict(action="any", slot=ANY, payload="any:7", state="st-chest:1")
    pair.click_case("left on clicks=all", "left", ANY, report=dict(anything, click="left", key="nil"))
    pair.click_case("Q on clicks=all", "drop", ANY, report=dict(anything, click="drop", key="nil"))
    pair.click_case("Ctrl+Q on clicks=all", "drop_all", ANY, report=dict(anything, click="drop_all", key="nil"))
    pair.click_case("number key 5 on clicks=all", "number_key", ANY,
                    report=dict(anything, click="number_key", key="5"), key=5)
    if p >= OFFHAND:
        pair.click_case("F on clicks=all", "offhand", ANY, report=dict(anything, click="offhand", key="nil"))
    pair.click_case("middle click on clicks=all", "middle", ANY, report=dict(anything, click="middle", key="nil"))
    pair.click_case("left on an inventory-row slot", "left", pair.hotbar(0))
    pair.click_case("click outside the window", "outside", -999)
    spread_cases(pair)
    burst(pair)


def burst(pair):
    """Ten shift-clicks within half a second, then one more: Rift keeps at
    most one inventory request on its way and asks again afterwards, so the
    last click still gets the inventory back (a request that never got its
    answer would block every later one)."""
    mark = pair.client.mark()
    for _ in range(10):
        pair.real_click("shift_left", BUY)
        time.sleep(0.05)
    time.sleep(2.0)
    events = [e for e in pair.events(mark) if e.get("click") == "shift_left"]
    pair.check("a burst of ten shift-clicks: ten events", len(events) == 10, f"{len(events)} events")
    answers = [r for r in pair.client.packets("window_items", mark) if r.value.get("windowId") == 0]
    if pair.backend != 755:
        pair.check("a burst of ten shift-clicks: the inventory comes back, at most a few times",
                   1 <= len(answers) <= 10, f"{len(answers)} container_set_content(0)")
    pair.click_case("a shift-click after the burst", "shift_left", BUY,
                    report=dict(action="buy", slot=BUY, payload="buy:5", state="st-chest:1", key="nil",
                                click="shift_left"))


def spread_cases(pair):
    """A drag over two menu slots and a double click in the inventory rows:
    never reported; before 1.17 Rift sends every menu slot again and asks for
    the inventory, from 1.17 the click's list of changed slots decides."""
    p = pair.protocol
    diamonds = from_record(p, json.loads(pair.baseline["slots"][36])) if pair.baseline["slots"][36] else stack(p, 1, 5)
    legacy = p < CHANGED_SLOTS
    mark = pair.client.mark()
    pair.send_click(-999, 0, 5, cursor=diamonds, item=empty(p))
    pair.send_click(BUY, 1, 5, cursor=diamonds, item=empty(p))
    pair.send_click(DECORATION, 1, 5, cursor=diamonds, item=empty(p))
    pair.send_click(-999, 2, 5, changed=[(BUY, diamonds), (DECORATION, diamonds)], item=empty(p))
    pair.expect_no_event("a drag over two menu slots: not reported", mark, wait=0.8)
    pair.judge_undo("a drag over two menu slots", mark, list(range(pair.base)) if legacy else [BUY, DECORATION])
    pair.judge_rows("a drag over two menu slots", mark, legacy)
    mark = pair.client.mark()
    row = pair.hotbar(0)
    pair.send_click(row, 0, 6, changed=[(row, empty(p))], cursor=diamonds, item=diamonds)
    pair.expect_no_event("a double click in the inventory rows: not reported", mark, wait=0.8)
    pair.judge_undo("a double click in the inventory rows", mark, list(range(pair.base)) if legacy else [])
    pair.judge_rows("a double click in the inventory rows", mark, True)


def step_backend(pair):
    """Nothing reached the backend; a single-slot update while the menu is
    open is repaired when it closes; a late click for the closed menu is
    dropped; the 1.17 backend is asked when the menu closes."""
    if pair.menu != "chest" or pair.container is None:
        raise Skip("the chest is not open")
    view = pair.server_view()
    pair.check("the backend's own view of the inventory is unchanged by the clicks", view == pair.view,
               f"before {pair.view}, after {view}" if view != pair.view else
               ("" if view is not None else "no console view on this server"))
    # A give while the menu is open: one container_set_slot, dropped by
    # clients up to 1.21.1 for slots outside the hotbar.
    name = pair.client.name
    pair.console(f"give {name} minecraft:gold_nugget 1", 0.8)
    mark = pair.client.mark()
    old = pair.container
    pair.real_click("left", CLOSE_BUTTON)
    close_expected(pair, "the close button", mark, "chest", "plugin", "st-chest:1", True)
    pair.judge_undo("the close button", mark, [])
    time.sleep(1.5)
    after = inventory(pair.client)
    nuggets = [i for i, v in after.filled().items() if pair.item_name(v) == "minecraft:gold_nugget"
               or (pair.registries is None and i not in (36, 37))]
    pair.check("an item given while the menu was open is in the inventory after it closed", nuggets,
               pair.describe_inventory(after.snapshot()))
    rows_ok = [s for s in range(9, 45) if after.slots[s] != pair.baseline["slots"][s]
               and not (after.slots[s] and "gold" in str(pair.item_name(json.loads(after.slots[s]))))]
    pair.check("the inventory rows are right after the menu closed", not rows_ok,
               f"slots {rows_ok} differ: {pair.describe_inventory(after.snapshot())}")
    pair.baseline = after.snapshot()
    pair.view = pair.server_view()
    # A late click for the menu that is gone.
    mark = pair.client.mark()
    pair.send_click(BUY, 0, 0, changed=[(BUY, empty(pair.protocol))], cursor=pair.stack_at(BUY),
                    item=pair.stack_at(BUY), container=old)
    pair.expect_no_event("a late click for the closed menu: not reported", mark, wait=1.5)
    pair.client.check()
    view = pair.server_view()
    pair.check("a late click changes nothing on the backend", view == pair.view, f"before {pair.view}, after {view}")


def step_endings(pair):
    """close_menu, replaced (by a button and by a command), death."""
    open_menu(pair, "chest")
    mark = pair.client.mark()
    pair.real_click("left", CLOSE_BY_PLUGIN)
    pair.expect_event("the close_menu button is reported", mark, kind="click", action="closeme")
    close_expected(pair, "rift.players.close_menu", mark, "chest", "plugin", "st-chest:1", True)

    open_menu(pair, "chest")
    mark = pair.client.mark()
    pair.real_click("left", REPLACE)
    pair.expect_event("the replace button is reported", mark, kind="click", action="replace")
    if pair.expect_open("hopper", mark):
        pair.wait_event(mark, menu="chest", kind="close")
        pair.expect_event("the chest replaced by the hopper: reason replaced", mark, menu="chest", kind="close",
                          reason="replaced", state="st-chest:1")
        pair.click_case("hopper: left on its button", "left", 2,
                        report=dict(action="hop", slot=2, click="left", payload="hop:2", state="st-hopper:2",
                                    key="nil"))
        pair.click_case("hopper: shift-left on its button", "shift_left", 2,
                        report=dict(action="hop", slot=2, click="shift_left", payload="hop:2",
                                    state="st-hopper:2", key="nil"))
    mark = open_menu(pair, "dispenser")
    pair.expect_event("the hopper replaced by a command: reason replaced", mark, menu="hopper", kind="close",
                      reason="replaced", state="st-hopper:2")
    pair.click_case("dispenser: right on its button", "right", 4,
                    report=dict(action="gold", slot=4, click="right", payload="gold:3", state="st-disp:3", key="nil"))
    pair.click_case("dispenser: number key 2 on its button", "number_key", 4, key=2)
    mark = pair.client.mark()
    pair.client.send_packet("close_window", {"windowId": pair.container})
    pair.expect_event("the dispenser closed by the player: reason player", mark, menu="dispenser", kind="close",
                      reason="player", state="st-disp:3")
    if pair.backend == 755:
        time.sleep(1.5)
        sets = [r for r in pair.client.packets(("set_slot", "window_items"), mark) if r.value.get("windowId") == 0]
        pair.check("a 1.17 backend is asked for the inventory when the menu closes", sets,
                   f"{len(sets)} inventory packets after the close")
    # rclose without an id closes any menu.
    open_menu(pair, "hopper")
    mark = pair.client.mark()
    pair.close_any()
    close_expected(pair, "close_menu without an id", mark, "hopper", "plugin", "st-hopper:2", True)
    # Death: the respawn ends the menu.
    open_menu(pair, "chest")
    mark = pair.client.mark()
    pair.console(f"kill {pair.client.name}", 1.0)
    try:
        pair.client.send_packet("client_command", {"actionId": "perform_respawn"})
    except Exception:  # A schema that wants the number.
        pair.client.send_packet("client_command", {"actionId": 0})
    pair.expect_event("death and respawn: reason server", mark, menu="chest", kind="close", reason="server",
                      state="st-chest:1")
    time.sleep(1.0)


def step_real_click(pair):
    """After the menus (and on a backend up to 1.16.5 after Rift acknowledged
    its own rejections) a real click on the player's own inventory works."""
    move_diamonds(pair, "after the menus")


def move_diamonds(pair, label):
    """Two real clicks move the diamonds to an empty slot; the backend must
    act on them (it ignores clicks while it waits for an acknowledgement, or
    while it thinks another container is open)."""
    p = pair.protocol
    pair.client.check()
    model = inventory(pair.client)
    # The server confirms an accepted click without resending the slots, so
    # where the diamonds are is what the last move made of it.
    slot = getattr(pair, "diamonds_at", pair.diamond_slot)
    held = json.loads(pair.baseline["slots"][pair.diamond_slot])
    target = next(i for i in range(9, 36) if model.slots[i] is None and i != slot)
    mark = pair.client.mark()
    saved = (pair.container, pair.base)
    pair.container, pair.base = 0, 46
    try:
        state = model.state or 0
        if p >= CHANGED_SLOTS:
            value = {"windowId": 0, "slot": slot, "mouseButton": 0, "mode": 0,
                     "changedSlots": [{"location": slot, "item": empty(p)}], "cursorItem": from_record(p, held),
                     "stateId": state}
            if p < STATE_IDS:
                value.pop("stateId")
            pair.client.send_packet("window_click", value)
            time.sleep(0.4)
            value = {"windowId": 0, "slot": target, "mouseButton": 0, "mode": 0,
                     "changedSlots": [{"location": target, "item": from_record(p, held)}],
                     "cursorItem": empty(p), "stateId": inventory(pair.client).state or state}
            if p < STATE_IDS:
                value.pop("stateId")
            pair.client.send_packet("window_click", value)
        else:
            pair.action += 1
            first = pair.action
            pair.client.send_packet("window_click", {"windowId": 0, "slot": slot, "mouseButton": 0, "mode": 0,
                                                     "action": first, "item": from_record(p, held)})
            time.sleep(0.4)
            pair.action += 1
            pair.client.send_packet("window_click", {"windowId": 0, "slot": target, "mouseButton": 0, "mode": 0,
                                                     "action": pair.action, "item": empty(p)})
            time.sleep(0.8)
            if pair.backend < TRANSACTIONS_END:
                accepted = [r.value for r in pair.client.packets("transaction", mark)
                            if r.value.get("windowId") == 0 and r.value.get("accepted")]
                pair.check(f"{label}: the backend accepts real clicks (transactions confirmed)",
                           len(accepted) >= 2, f"transactions {[r.value for r in pair.client.packets('transaction', mark)]}")
    finally:
        pair.container, pair.base = saved
    time.sleep(1.0)
    if pair.software == "pumpkin":
        raise Skip("Pumpkin has no console view of the inventory")
    nbt_slot = target  # Container 0 slots 9..35 are NBT slots 9..35.
    moved = pair.item_in_slot(nbt_slot, "diamond")
    if moved:
        pair.diamonds_at = target
    pair.check(f"{label}: real clicks moved the diamonds on the backend", moved, f"slot {slot} -> {target}")


def summon_container(pair):
    """A chest minecart at the player, opened with a real interaction: the
    backend's own container. Its entity ID, or Skip."""
    mark = pair.client.mark()
    x, y, z = (int(v // 1) for v in pair.client.position[:3])
    entity = "minecraft:chest_minecart"
    pair.console(f"summon {entity} {x} {y} {z}" + (" {NoGravity:1b}" if pair.backend >= 393 else ""), 1.0)
    spawned = [r.value for r in pair.client.packets(("spawn_entity",), mark)]
    if not spawned:
        raise Skip("the chest minecart did not spawn")
    return spawned[-1]["entityId"]


def interact(pair, entity):
    if pair.protocol >= 775:
        pair.client.send_packet("use_entity", {"target": entity, "hand": "main_hand", "location": None,
                                               "sneaking": False})
    elif pair.protocol >= OFFHAND:
        pair.client.send_packet("use_entity", {"target": entity, "mouse": 0, "hand": 0, "sneaking": False})
    else:
        pair.client.send_packet("use_entity", {"target": entity, "mouse": 0, "hand": 0})


def step_backend_screen(pair):
    """A backend screen and a menu: the menu replaces the backend's chest
    (Rift closes it on the backend, so real clicks work afterwards), and a
    backend screen that opens over the menu ends it with reason server."""
    if pair.software == "pumpkin":
        raise Skip("Pumpkin: no chest minecart")
    entity = summon_container(pair)
    mark = pair.client.mark()
    interact(pair, entity)
    try:
        opened = pair.client.wait_packet("open_window", "the minecart's screen", 6, mark)
    except SessionError as error:
        raise Skip(f"the minecart did not open: {error}")
    backend = opened.value["windowId"]
    pair.check("the backend's own container has an ID below 101", 0 < backend < 101, f"container {backend}")
    open_menu(pair, "dispenser")
    mark = pair.client.mark()
    pair.client.send_packet("close_window", {"windowId": pair.container})
    pair.expect_event("a menu over the backend's chest, closed by the player: reason player", mark,
                      menu="dispenser", kind="close", reason="player")
    time.sleep(0.5)
    move_diamonds(pair, "after a menu replaced the backend's chest")
    # The backend opens its chest while a menu is open.
    open_menu(pair, "chest")
    mark = pair.client.mark()
    interact(pair, entity)
    pair.expect_event("the backend opens a screen over the menu: reason server", mark, menu="chest",
                      kind="close", reason="server")
    try:
        opened = pair.client.wait_packet("open_window", "the minecart's screen", 6, mark)
        pair.client.send_packet("close_window", {"windowId": opened.value["windowId"]})
    except SessionError as error:
        pair.check("the backend's screen reaches the client", False, error)
    pair.console(f"kill @e[type={'minecraft:' if pair.backend >= 393 else ''}chest_minecart]", 0.5)


def step_creative(pair):
    """A client in creative mode that drops an item from a menu also asks the
    backend to create it (set_creative_mode_slot); Rift drops that packet
    while a menu is open."""
    if pair.software == "pumpkin":
        raise Skip("Pumpkin has no console view of the inventory")
    name, p = pair.client.name, pair.protocol
    pair.console(pair.cmd.gamemode(name, "creative"), 0.8)
    try:
        open_menu(pair, "chest")
        stone = (pair.registries.id("item", "stone") if pair.registries else None) or 1
        if p >= 766:
            item = {"itemCount": 1, "itemId": stone, "addedComponentCount": 0, "removedComponentCount": 0,
                    "components": [], "removeComponents": []}
        else:
            item = stack(p, stone, 1)
        pair.client.send_packet("set_creative_slot", {"slot": 36, "item": item})
        time.sleep(1.0)
        pair.client.check()
        pair.check("a creative-mode slot while a menu is open does not reach the backend",
                   not pair.item_in_slot(0, "stone"), "the backend has stone in the first hotbar slot")
        mark = pair.client.mark()
        pair.client.send_packet("close_window", {"windowId": pair.container})
        pair.expect_event("closed by the player in creative mode: reason player", mark, menu="chest",
                          kind="close", reason="player")
        # The control: without a menu the backend takes the same packet.
        time.sleep(0.5)
        pair.client.send_packet("set_creative_slot", {"slot": 36, "item": item})
        time.sleep(1.0)
        pair.check("the control: without a menu the same creative-mode slot reaches the backend",
                   pair.item_in_slot(0, "stone"), "the backend has no stone in the first hotbar slot")
        pair.console(f"clear {name} {'minecraft:' if pair.backend >= 393 else ''}stone", 0.5)
    finally:
        pair.console(pair.cmd.gamemode(name, "survival"), 0.5)


def step_same_id(pair):
    """Showing the open menu's id again is not an ending: no close event, and
    the menu keeps working. (This build opens it anew; the design redraws only
    what changed, which --with redraw is for.)"""
    open_menu(pair, "chest")
    mark = pair.client.mark()
    pair.show("chest")
    time.sleep(1.5)
    reopened = [r for r in pair.client.packets("open_window", mark)]
    pair.note(f"showing the same id again sent {len(reopened)} open_screen")
    if reopened:
        pair.container = reopened[-1].value["windowId"]
    pair.check("the same id again: no close event", not pair.events(mark), pair.events(mark))
    pair.click_case("the same id again: left on a button", "left", BUY,
                    report=dict(action="buy", slot=BUY, payload="buy:5", state="st-chest:1", key="nil", click="left"))
    mark = pair.client.mark()
    pair.client.send_packet("close_window", {"windowId": pair.container})
    pair.expect_event("the same id again, then closed: reason player", mark, menu="chest", kind="close",
                      reason="player")


def step_switch(pair):
    """A transfer to the second backend closes the menu with reason switch."""
    if pair.second is None:
        raise Skip("no second backend (--no-switch)")
    open_menu(pair, "chest")
    mark = pair.client.mark()
    pair.real_click("left", SWITCH)
    pair.expect_event("the switch button is reported", mark, kind="click", action="switch")
    event = pair.wait_event(mark, timeout=20, menu="chest", kind="close")
    pair.check("the transfer: reason switch", event is not None and event.get("reason") == "switch",
               f"close event {event}; all {pair.events(mark)}")
    deadline = time.monotonic() + 30
    while pair.client.dimension_changes == 0 and time.monotonic() < deadline:
        pair.client.check()
        time.sleep(0.2)
    time.sleep(2.0)
    pair.client.check()
    pair.check("the player arrived on the second backend", pair.client.name in pair.second.text(),
               "the second server never saw the player")


def step_disconnect(pair):
    """The player leaves with a menu open; a second player sees the close event."""
    observer = Player(pair.rift.port, "O" + uuid.uuid4().hex[:9], pair.protocol, label="observer",
                      pumpkin=pair.software == "pumpkin")
    pair.observer = observer
    observer.join()
    time.sleep(1.5)
    observer.send_packet("close_window", {"windowId": 101})  # Its own welcome menu, if it got one.
    open_menu(pair, "hopper")
    mark = observer.mark()
    pair.left = True
    pair.client.close()
    event = pair.wait_event(mark, timeout=10, session=observer, menu="hopper", kind="close")
    pair.check("leaving with a menu open: reason disconnect (seen by another player)",
               event is not None and event.get("reason") == "disconnect", f"close event {event}")


def step_one_close(pair):
    """Every menu that opened ended with exactly one close event."""
    events = pair.events(0, session=pair.observer or pair.client) if pair.left else pair.events(0)
    if pair.left and pair.observer is not None:
        events = pair.events(0) + [e for e in pair.events(0, session=pair.observer)
                                   if e.get("reason") == "disconnect"]
    closes = [e["menu"] for e in events if e.get("kind") == "close"]
    pair.check("exactly one close event per menu, in order", closes == pair.opened,
               f"opened {pair.opened}, closed {closes}")


# Later steps of the design; turn on with --with when the binary has them.

def step_redraw(pair):
    """Showing the open menu's id again sends only the changed slots."""
    raise Skip("needs a second definition of the same id; write when redraws land")


def step_update_menu(pair):
    raise Skip("update_menu is not in this build")


def step_stale_click(pair):
    raise Skip("the stale-click rule is not in this build")


def step_anvil(pair):
    raise Skip("anvil menus are not in this build")


STEPS = [step_clicks, step_backend, step_endings, step_real_click, step_same_id, step_backend_screen, step_creative, step_redraw, step_update_menu,
         step_stale_click, step_anvil, step_switch, step_disconnect, step_one_close]
OPTIONAL = {"step_redraw", "step_update_menu", "step_stale_click", "step_anvil"}


# --- Servers and pairs ------------------------------------------------------------------

def parse_pair(text):
    server, _, client = text.partition(":")
    key_name = fixtures.resolve(server)
    protocol = fixtures.SERVERS[key_name]["protocol"] if not client else fixtures.PROTOCOL_OF.get(client)
    if protocol is None:
        raise SystemExit(f"unknown client version {client!r}")
    return key_name, protocol


def run_server(key_name, protocols, args, session_server):
    stamp = time.strftime("%Y%m%d-%H%M%S")
    workdir = Path(tempfile.mkdtemp(prefix=f"menus-{key_name}-", dir=args.work))
    server = fixtures.Server(key_name, workdir / "main")
    second = None if args.no_switch else fixtures.Server(key_name, workdir / "second")
    rift = None
    results = []
    try:
        server.start()
        env.log(f"{server.name} (protocol {server.protocol}) ready in {server.started}s on port {server.port}")
        if second is not None:
            second.start()
            env.log(f"second {second.name} ready on port {second.port}")
        rift = Rift(args.binary, workdir, args.mode, server, second, session_server.url).start()
        for protocol in protocols:
            directory = args.out / f"{stamp}-{key_name}-client-{fixtures.RELEASE_OF[protocol]}"
            directory.mkdir(parents=True, exist_ok=True)
            pair = Pair(server, second, rift, protocol, directory, args)
            pair.rift_offset, pair.server_offset = rift.offset(), server.offset()
            pair.second_offset = second.offset() if second else 0
            try:
                pair.run()
            except Exception as error:
                pair.check("the pair ran to the end", False, f"{type(error).__name__}: {error}")
                pair.notes.append(traceback.format_exc()[-2000:])
                try:
                    pair.section = "alive"
                    pair.final_checks()
                except Exception:  # The client may not exist.
                    pass
            finally:
                for session in (pair.client, pair.observer):
                    if session is not None:
                        session.close()
                if pair.client is not None:
                    pair.client.write_log(directory / "client.jsonl")
                time.sleep(0.5)
                (directory / "rift.log").write_text(rift.text()[pair.rift_offset:])
                (directory / "server.log").write_text(server.text()[pair.server_offset:])
                if second is not None:
                    (directory / "second.log").write_text(second.text()[pair.second_offset:])
            result = dict(mode=args.mode, server=key_name, server_protocol=server.protocol, client=fixtures.RELEASE_OF[protocol],
                          client_protocol=protocol, checks=pair.checks, notes=pair.notes, logs=str(directory))
            result["passed"] = sum(c["ok"] for c in pair.checks)
            result["failed"] = sum(not c["ok"] for c in pair.checks)
            result["status"] = "PASS" if pair.checks and not result["failed"] else "FAIL"
            (directory / "result.json").write_text(json.dumps(result, indent=2, default=str) + "\n")
            results.append(result)
            print(summary(result), flush=True)
    except SystemExit:
        raise
    except Exception as error:
        for protocol in protocols[len(results):]:
            results.append(dict(server=key_name, client=fixtures.RELEASE_OF[protocol], client_protocol=protocol,
                                status="ERROR", passed=0, failed=1, notes=[traceback.format_exc()[-2000:]],
                                checks=[dict(section="setup", name=str(error), ok=False, detail="")]))
            print(summary(results[-1]), flush=True)
    finally:
        if rift is not None:
            rift.stop()
        for instance in (second, server):
            if instance is not None:
                instance.stop()
        if args.keep:
            env.log(f"kept {workdir}")
        else:
            shutil.rmtree(workdir, ignore_errors=True)
    return results


def summary(result):
    line = (f"{result['status']:<5} {result['server']:<20} <- client {result['client']:<8} "
            f"{result['passed']} passed, {result['failed']} failed")
    failed = [f"{c['section']}: {c['name']}" for c in result["checks"] if not c["ok"]]
    if failed:
        line += "\n      " + "\n      ".join(failed)
    if result.get("logs"):
        line += f"\n      logs {result['logs']}"
    return line


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                     formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    parser.add_argument("--binary", type=Path, required=False, help="the rift binary (test-session-server build)")
    parser.add_argument("--accept-eula", action="store_true",
                        help="accept https://aka.ms/MinecraftEULA for the local test servers")
    parser.add_argument("--pair", action="append", default=[], help="SERVER[:CLIENT], repeat or comma-separate")
    parser.add_argument("--set", action="append", default=[], choices=["same", "more", "translated", "all"],
                        help="predefined pairs")
    parser.add_argument("--mode", choices=["plugin", "script"], default="plugin",
                        help="plugin: menus from a persistent Lua plugin, offline (any build); script: from "
                             "rift.lua, online against a fake session server (a test-session-server build)")
    parser.add_argument("--no-switch", action="store_true", help="no second backend, no switch check")
    parser.add_argument("--with", dest="with_", default="", help="optional steps to run: "
                        + ",".join(sorted(name.removeprefix("step_") for name in OPTIONAL)))
    parser.add_argument("--out", type=Path, default=OUT, help=f"results directory (default {OUT})")
    parser.add_argument("--work", type=Path, default=None, help="where server directories go")
    parser.add_argument("--keep", action="store_true", help="keep server directories")
    parser.add_argument("--json", type=Path, help="also write all results to this file")
    parser.add_argument("--list", action="store_true", help="list the predefined pairs")
    args = parser.parse_args()
    if args.list:
        for name, pairs in (("same", SAME), ("more", MORE), ("translated", TRANSLATED)):
            print(f"--set {name}: {' '.join(pairs)}")
        return
    args.with_ = {f"step_{name.strip()}" for name in args.with_.split(",") if name.strip()}
    pairs = [entry for value in args.pair for entry in value.split(",") if entry]
    for name in args.set:
        pairs += {"same": SAME, "more": MORE, "translated": TRANSLATED, "all": SAME + MORE + TRANSLATED}[name]
    if not pairs:
        parser.error("give --pair or --set")
    if not args.accept_eula:
        parser.error("--accept-eula is required to run the Minecraft servers")
    if args.binary is None:
        parser.error("--binary is required")
    args.binary = args.binary.resolve(strict=True)
    crypto_self_test()
    plan = {}
    for text in pairs:
        key_name, protocol = parse_pair(text)
        plan.setdefault(key_name, [])
        if protocol not in plan[key_name]:
            plan[key_name].append(protocol)
    args.out.mkdir(parents=True, exist_ok=True)
    args.work = args.work or (env.HOME / "menus-e2e-work")
    args.work.mkdir(parents=True, exist_ok=True)
    env.install_signal_handlers()
    session_server = SessionServer()
    results = []
    try:
        for key_name, protocols in plan.items():
            results += run_server(key_name, protocols, args, session_server)
    except KeyboardInterrupt:
        env.PROCESSES.stop_all()
        raise SystemExit(130)
    finally:
        session_server.stop()
    print()
    print(f"{'server':<22} {'client':<8} {'result':<6} {'passed':>6} {'failed':>6}")
    for result in results:
        print(f"{result['server']:<22} {result['client']:<8} {result['status']:<6} "
              f"{result['passed']:>6} {result['failed']:>6}")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(results, indent=2, default=str) + "\n")
    if any(result["status"] != "PASS" for result in results):
        raise SystemExit(1)


if __name__ == "__main__":
    os.environ.setdefault("PYTHONUNBUFFERED", "1")
    main()
