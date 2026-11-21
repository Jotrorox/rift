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
    redraw      showing the open menu's id again: two changed slots send
                exactly two container_set_slot with one rising state ID; the
                unchanged definition sends nothing; a new title open_screen
                with the same container ID and every slot; new rows a new
                container ID; never a close event; clicks carry the new state
    update_menu slots (one emptied), keys, title and state; nothing sent for
                a closed menu, an unknown id or another plugin's call
    pages       the paged list of the design, page in state, payloads
    stale_click from 1.17.1 a click naming the state before an action changed
                is undone and not reported, a close button does not close;
                a slot that only looks different still reports
    redraw_burst 20 clicks in a second on a button that redraws its menu
    endings     player, plugin (close button and close_menu), replaced,
                server (death), switch (rift.players.transfer to a second
                backend) and disconnect (seen by a second player), exactly one
                close event per menu
    creative    set_creative_mode_slot never reaches the backend while a menu
                is open or within a second after Rift closed one; it does
                right after the player closed one
    screen_packets  rename, trade, beacon, recipe, button, transaction
                acknowledgement and slot state for Rift's chest stay away from
                the backend; without a menu a trade selection reaches it
    anvil       defaults, buttons in all three slots, fast renames (a result
                and a cost of 0 for each, the result built at most every
                50 ms, the final text's last), typing = true (at most 5 text
                events a second, always the final text), the result click
                (slots, cost, set_experience, cursor), a shift-click from the
                inventory rows, over-long and filtered renames, redraw, a new
                title, update_menu with text, every ending with the text, a
                late rename, none of it at the backend
    plugin_stop a plugin restarted, then reloaded with a new file, while its
                menu is open: Rift closes the menu, no instance gets an event
                of the old menu, the new instance's menus work
    alive       the client stays connected and decodes every packet, Rift logs
                no error or panic, the server no warning about the player; the
                backend never received a packet naming Rift's containers, and
                Rift's inventory requests were at least 250 ms apart

What the backend receives is recorded by a relay between Rift and the main
backend (BackendTap), so "nothing reaches the backend" is checked on the
wire, also on servers without a console view (Pumpkin).

The default mode (--mode plugin) shows the menus from a persistent Lua
plugin, driven over its http hook through Rift's web interface; it works with
any build. --mode script shows them from rift.lua instead. rift.lua player
hooks need online mode, so the clients log in online against a fake session
server on 127.0.0.1 (pure-Python RSA and AES/CFB8) and Rift forwards to the
offline test backends in BungeeCord mode, which vanilla ignores. The Rift
binary must therefore be built with `--features test-session-server` (it
reads RIFT_TEST_SESSION_URL); the script checks that first.

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

--only NAME,... runs only some steps (they expect the steps before them, so
use it to iterate, not to judge). Steps in OPTIONAL run only with --with
NAME,...; none is optional now. Both modes also load a second persistent
plugin, `othertest`, that tries to patch and close the test's menus by id and
shows a menu of its own for the plugin_stop step.

Downloads: a server jar that tests/minecraft.py already put into
target/minecraft/downloads (CI caches that directory) is copied into the
shared cache instead of being downloaded again.
"""

import argparse
import http.server
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import socket
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
from minecraft import CACHE as HARNESS_CACHE, varint  # noqa: E402
import protodef  # noqa: E402

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
SLOT_COUNT = {"chest": None, "hopper": 5, "dispenser": 9, "anvil": 3}
GLASS_SLOT = ("glass_pane", 1, None, [])
# The "live" chest's '#' slots in 3 rows, and its buttons.
LIVE_GLASS = list(range(0, 10)) + list(range(17, 27))
LIVE_A, LIVE_B, LIVE_C, LIVE_D = 10, 12, 14, 16


def live_spec(variant):
    """What the Lua function live(variant) shows, as MENUS describes a menu."""
    rows = 4 if variant == "live_rows" else 3
    slots = {slot: GLASS_SLOT for slot in list(range(0, 10)) + list(range(17, rows * 9))}
    slots.update({LIVE_A: ("emerald", 1, "Alpha", []), LIVE_B: ("stone", 1, "Bravo", []),
                  LIVE_C: ("compass", 1, "Charlie", []), LIVE_D: ("barrier", 1, "Close", [])})
    title = "Live menu"
    if variant != "live":
        slots[LIVE_A] = ("emerald", 1, "Alpha two", [])
        slots[LIVE_B] = ("diamond", 1, "Bravo two", [])
    if variant == "live_stale":
        slots[LIVE_B] = ("stone", 1, "Bravo", [])
        slots[LIVE_C] = ("barrier", 1, "Close now", [])
    if variant in ("live_title", "live_rows"):
        title = "Live menu renamed"
    return ("chest", rows, title, slots)


KITS = [["k1a", "k1b", "k1c"], ["k2a", "k2b"], ["k3a"]]
KITS_BACK, KITS_NEXT = 45, 53


def kits_spec(page):
    slots = {index: ("chest", 1, f"Kit {kit}", []) for index, kit in enumerate(KITS[page - 1])}
    slots[KITS_BACK] = ("arrow", 1, "Back", [])
    slots[KITS_NEXT] = ("arrow", 1, "Next", [])
    return ("chest", 6, f"Kits, page {page}", slots)


COUNTER_SLOT = 13
COUNTER = ("chest", 3, "Counter", {COUNTER_SLOT: ("clock", 1, "Count 0", [])})

# The menus, the event report and the menu handlers: the same Lua in rift.lua
# (script mode) and in a persistent Lua plugin (plugin mode).
MENUS_LUA = r'''
local function s(v) if v == nil then return "nil" end return tostring(v) end
local function enc(v)
    if type(v) ~= "table" then return s(v) end
    if v.tag ~= nil then return s(v.tag) .. ":" .. s(v.n) end
    -- Other tables (the test's have one key): key:value pairs without spaces.
    -- No table.sort: rift.lua's sandbox has no table library.
    local text = ""
    for k, value in pairs(v) do
        text = text .. (text == "" and "" or ",") .. tostring(k) .. ":" .. s(value)
    end
    return text
end

-- Every event, every field, to every player: the clients read it as chat.
rift.on("menu", function(ctx)
    rift.players.broadcast("MENUEV who=" .. s(ctx.name) .. " menu=" .. s(ctx.menu)
        .. " kind=" .. s(ctx.kind) .. " action=" .. s(ctx.action) .. " slot=" .. s(ctx.slot)
        .. " click=" .. s(ctx.click) .. " key=" .. s(ctx.key) .. " payload=" .. enc(ctx.payload)
        .. " reason=" .. s(ctx.reason) .. " state=" .. enc(ctx.state) .. " server=" .. s(ctx.server)
        -- Last, so a text with spaces stays one field.
        .. " text=" .. s(ctx.text))
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

-- "live": a chest that is redrawn and patched. Variants of one definition.
local function live(variant)
    local rows = { "#########", "#a b c d#", "#########" }
    local keys = {
        ["#"] = GLASS,
        a = { item = "emerald", name = "Alpha", id = "a", payload = { tag = "a", n = 1 } },
        b = { item = "stone", name = "Bravo", id = "b", payload = { tag = "b", n = 1 } },
        c = { item = "compass", name = "Charlie", id = "c", payload = { tag = "c", n = 1 } },
        d = { item = "barrier", name = "Close", close = true },
    }
    local menu = { id = "live", title = "Live menu", layout = rows, keys = keys, state = { tag = "st-live", n = 0 } }
    if variant ~= "live" then
        -- Slot 10 only looks different; slot 12 looks different and does something else.
        keys.a = { item = "emerald", name = "Alpha two", id = "a", payload = { tag = "a", n = 1 } }
        keys.b = { item = "diamond", name = "Bravo two", id = "b2", payload = { tag = "b2", n = 2 } }
        menu.state = { tag = "st-live", n = 1 }
    end
    if variant == "live_stale" then
        -- Slot 12 only does something else; slot 14 becomes a close button.
        keys.b = { item = "stone", name = "Bravo", id = "b2", payload = { tag = "b2", n = 2 } }
        keys.c = { item = "barrier", name = "Close now", close = true }
    end
    if variant == "live_title" or variant == "live_rows" then
        menu.title = "Live menu renamed"
        menu.state = { tag = "st-live", n = 2 }
    end
    if variant == "live_rows" then
        menu.layout = { rows[1], rows[2], rows[3], "#########" }
        menu.state = { tag = "st-live", n = 3 }
    end
    return menu
end

-- The paged list of docs/menus.md ("State and payloads").
local KITS = { { "k1a", "k1b", "k1c" }, { "k2a", "k2b" }, { "k3a" } }
local function kits(player, page)
    local buttons = {}
    for i, kit in ipairs(KITS[page] or {}) do
        buttons[#buttons + 1] = { slot = i - 1, item = "chest", name = "Kit " .. kit, id = "kit", payload = { kit = kit } }
    end
    buttons[#buttons + 1] = { slot = 45, item = "arrow", name = "Back", id = "page", payload = { step = -1 } }
    buttons[#buttons + 1] = { slot = 53, item = "arrow", name = "Next", id = "page", payload = { step = 1 } }
    return rift.players.menu(player, {
        id = "kits", rows = 6, title = "Kits, page " .. page, slots = buttons, state = { page = page },
    })
end
rift.menus.on("kits", {
    click = { page = function(ctx) kits(ctx.uuid, ctx.state.page + ctx.payload.step) end },
})

-- A counter that redraws itself from its click handler.
local function counter(player, n)
    return rift.players.menu(player, {
        id = "counter", rows = 3, title = "Counter", state = { tag = "count", n = n },
        slots = { { slot = 13, item = "clock", name = "Count " .. n, id = "inc" } },
    })
end
rift.menus.on("counter", {
    click = { inc = function(ctx) counter(ctx.uuid, ctx.state.n + 1) end },
})

-- Anvils. "anvil" and its variants share an id, so showing a variant redraws it.
local function anvil(variant)
    local menu = { id = "anvil", type = "anvil", title = "Rift anvil", text = "Steve", state = { tag = "st-anvil", n = 1 } }
    if variant == "anvil_redraw" or variant == "anvil_title" then
        menu.slots = { { slot = 1, item = "emerald", name = "Right", id = "right", payload = { tag = "right", n = 2 } } }
        menu.state = { tag = "st-anvil", n = 2 }
    end
    if variant == "anvil_title" then
        menu.title = "Rift anvil renamed"
        menu.state = { tag = "st-anvil", n = 3 }
    end
    return menu
end
local ANVIL_TYPING = { id = "anvilt", type = "anvil", title = "Typing anvil", text = "Alex", typing = true,
                       state = { tag = "st-anvilt", n = 1 } }
local ANVIL_CUSTOM = {
    id = "anvilc", type = "anvil", title = "Custom anvil", text = "Custom", state = { tag = "st-anvilc", n = 1 },
    layout = { "LRX" },
    keys = {
        L = { item = "name_tag", name = "Not this name", id = "left", payload = { tag = "left", n = 1 } },
        R = { item = "emerald", name = "Right", id = "right", payload = { tag = "right", n = 2 } },
        X = { item = "diamond", name = "Confirm", id = "ok", payload = { tag = "ok", n = 3 } },
    },
}

-- What the test asks for by name: a command (script mode) or the http hook (plugin mode).
local ACTIONS = {
    anvil = function(p) return rift.players.menu(p, anvil("anvil")) end,
    anvil_redraw = function(p) return rift.players.menu(p, anvil("anvil_redraw")) end,
    anvil_title = function(p) return rift.players.menu(p, anvil("anvil_title")) end,
    anvil_typing = function(p) return rift.players.menu(p, ANVIL_TYPING) end,
    anvil_custom = function(p) return rift.players.menu(p, ANVIL_CUSTOM) end,
    anvil_patch_text = function(p) return rift.players.update_menu(p, "anvil", { text = "Patched" }) end,
    anvil_patch_close = function(p)
        return rift.players.update_menu(p, "anvilc", { slots = {
            { slot = 2, item = "barrier", name = "Cancel", close = true } } })
    end,
    anvil_close = function(p) return rift.players.close_menu(p, "anvil") end,
    chest = function(p) return rift.players.menu(p, MENUS.chest) end,
    live = function(p) return rift.players.menu(p, live("live")) end,
    live_two = function(p) return rift.players.menu(p, live("live_two")) end,
    live_title = function(p) return rift.players.menu(p, live("live_title")) end,
    live_rows = function(p) return rift.players.menu(p, live("live_rows")) end,
    live_stale = function(p) return rift.players.menu(p, live("live_stale")) end,
    patch_slots = function(p)
        return rift.players.update_menu(p, "live", { slots = {
            { slot = 10, item = "gold_ingot", name = "Patched", id = "pa", payload = { tag = "pa", n = 9 } },
            { slot = 14, empty = true },
        } })
    end,
    patch_keys = function(p)
        return rift.players.update_menu(p, "live", { keys = {
            ["#"] = { item = "iron_bars", name = " " },
            b = { item = "diamond", name = "Bravo keyed", id = "b", payload = { tag = "b", n = 1 } },
        } })
    end,
    patch_title = function(p) return rift.players.update_menu(p, "live", { title = "Live patched" }) end,
    patch_state = function(p) return rift.players.update_menu(p, "live", { state = { tag = "st-live", n = 7 } }) end,
    patch_unknown = function(p) return rift.players.update_menu(p, "nosuch", { title = "Nobody" }) end,
    kits = function(p) return kits(p, 1) end,
    counter = function(p) return counter(p, 0) end,
}
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
rift.command("raction", { permission = "", run = function(ctx)
    local action = ACTIONS[ctx.args]
    if not action then return { message = "ACTRESULT " .. s(ctx.args) .. " unknown" } end
    return { message = "ACTRESULT " .. ctx.args .. " " .. tostring(action(ctx.uuid)) }
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
    elseif w[1] == "act" then
        if not ACTIONS[w[3]] then return { status = 400, body = "unknown action" } end
        ok = ACTIONS[w[3]](w[2])
    else
        return { status = 400, body = "unknown verb" }
    end
    return { body = "queued=" .. tostring(ok) }
end)
'''

# A second persistent plugin in both modes: it must not be able to change or
# close the test's menus by naming their id, and never gets their events.
OTHER_LUA = r'''
rift.on("http", function(request)
    if request.method ~= "POST" or request.path ~= "/ext/other" then return nil end
    local w = {}
    for word in string.gmatch(request.body or "", "%S+") do w[#w + 1] = word end
    local ok
    if w[1] == "patch" then
        ok = rift.players.update_menu(w[2], w[3], { title = "Hijacked",
            slots = { { slot = 10, item = "tnt", name = "Hijacked", id = "evil" } } })
    elseif w[1] == "close" then
        ok = rift.players.close_menu(w[2], w[3])
    elseif w[1] == "show" then
        -- Its own menu, for the test of a plugin that stops while it is open.
        ok = rift.players.menu(w[2], { id = "otherm", rows = 1, title = "Other plugin",
            slots = { { slot = 4, item = "gold_ingot", name = "Other", id = "otherclick" } } })
    else
        return { status = 400, body = "unknown verb" }
    end
    return { body = "queued=" .. tostring(ok) }
end)
rift.on("menu", function(ctx)
    rift.players.broadcast("OTHEREV menu=" .. tostring(ctx.menu) .. " kind=" .. tostring(ctx.kind)
        .. " reason=" .. tostring(ctx.reason) .. " who=" .. tostring(ctx.name) .. " gen=@GEN@")
end)
'''

PLUGIN_TOML = '''name = "@NAME@"
version = "0.1.0"
api = "0.3"
engine = "lua"

[capabilities]
network = false
env = []
'''


# --- What the backend receives ----------------------------------------------------------

def _read_varint(data, offset):
    """(value, next offset), or (None, offset) when `data` ends mid-VarInt."""
    value = 0
    for shift in range(0, 35, 7):
        if offset >= len(data):
            return None, offset
        byte = data[offset]
        offset += 1
        value |= (byte & 127) << shift
        if not byte & 128:
            return value, offset
    raise ValueError("invalid VarInt")


# Serverbound packets the tap keeps with their bodies; of the others only the name.
TAPPED = {"window_click", "close_window", "transaction", "craft_recipe_request", "enchant_item", "set_slot_state",
          "name_item", "select_trade", "set_beacon_effect", "set_creative_slot", "custom_payload"}


class BackendTap:
    """A loopback relay between Rift and a backend that records, per player,
    every serverbound play packet the backend receives: the evidence that a
    menu's packets never reach it. Bytes pass unchanged; it follows the
    handshake, compression and the login, configuration and play states.
    The backend runs offline, so nothing is encrypted."""

    def __init__(self, target_port):
        self.target = ("127.0.0.1", target_port)
        self.lock = threading.Lock()
        self.records = []        # (monotonic time, player, name, body or None)
        self.errors = []
        self.decoders = {}
        self.listener = socket.create_server(("127.0.0.1", 0))
        self.port = self.listener.getsockname()[1]
        self.sockets = []
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self):
        try:
            while True:
                client, _ = self.listener.accept()
                threading.Thread(target=self.serve, args=(client,), daemon=True).start()
        except OSError:
            return

    def serve(self, client):
        try:
            upstream = socket.create_connection(self.target, timeout=15)
        except OSError as error:
            self.errors.append(f"tap could not reach the backend: {error}")
            client.close()
            return
        upstream.settimeout(None)
        self.sockets += [client, upstream]
        session = {"compression": False, "state": "handshake", "protocol": None, "player": None,
                   "raw_clientbound": False, "raw": False}
        threading.Thread(target=self.pump, args=(upstream, client, True, session), daemon=True).start()
        self.pump(client, upstream, False, session)

    def pump(self, source, destination, clientbound, session):
        pending = b""
        try:
            while data := source.recv(65536):
                if session["raw"] or (clientbound and session["raw_clientbound"]):
                    destination.sendall(data)
                    continue
                pending += data
                out = bytearray()
                while pending:
                    length, offset = _read_varint(pending, 0)
                    if length is None or len(pending) < offset + length:
                        break
                    raw, frame = pending[:offset + length], pending[offset:offset + length]
                    pending = pending[offset + length:]
                    try:
                        self.frame(frame, clientbound, session)
                    except Exception as error:  # Observation never breaks the stream.
                        self.errors.append(f"tap lost sync ({'clientbound' if clientbound else 'serverbound'}): "
                                           f"{type(error).__name__}: {error}")
                        session["raw"] = True
                    out += raw
                    if clientbound and session["raw_clientbound"]:
                        out += pending
                        pending = b""
                        break
                destination.sendall(bytes(out))
        except OSError:
            pass
        finally:
            # Pass the end on as a half-close, as the peer did: Rift closes
            # its side first and then waits for the backend's end.
            try:
                destination.shutdown(socket.SHUT_WR)
            except OSError:
                pass
            with self.lock:
                session["ended"] = session.get("ended", 0) + 1
                done = session["ended"] == 2
            if done:
                for sock in (source, destination):
                    try:
                        sock.close()
                    except OSError:
                        pass

    def payload(self, frame, session):
        if not session["compression"]:
            return frame
        size, offset = _read_varint(frame, 0)
        return frame[offset:] if size == 0 else zlib.decompress(frame[offset:])

    def names(self, protocol, state):
        if (protocol, state) not in self.decoders:
            self.decoders[(protocol, state)] = protodef.Decoder(protodef.schema_for(protocol), state, "serverbound")
        return self.decoders[(protocol, state)]

    def frame(self, frame, clientbound, session):
        if clientbound:
            # Only the login matters: compression, then success.
            if session["state"] in ("handshake", "status"):
                session["raw_clientbound"] = session["state"] == "status"
                return
            packet_id, _ = _read_varint(self.payload(frame, session), 0)
            if packet_id == 0x03:
                session["compression"] = True
            elif packet_id == 0x02:
                if session["protocol"] < 764:
                    session["state"] = "play"
                session["raw_clientbound"] = True
            return
        payload = self.payload(frame, session) if session["state"] != "handshake" else frame
        packet_id, offset = _read_varint(payload, 0)
        state = session["state"]
        if state == "handshake":
            protocol, offset = _read_varint(payload, offset)
            size, offset = _read_varint(payload, offset)
            offset += size + 2
            following, _ = _read_varint(payload, offset)
            session["protocol"] = protocol
            session["state"] = "login" if following in (2, 3) else "status"
            if session["state"] == "status":
                session["raw"] = True
            return
        if state == "login":
            if packet_id == 0x00 and session["player"] is None:
                size, start = _read_varint(payload, offset)
                session["player"] = payload[start:start + size].decode(errors="replace")
            elif packet_id == 0x03 and session["protocol"] >= 764:
                session["state"] = "configuration"
            return
        decoder = self.names(session["protocol"], state)
        name = decoder.names.get(packet_id, f"0x{packet_id:02x}")
        if state == "configuration":
            if name == "finish_configuration":
                session["state"] = "play"
            return
        if name == "configuration_acknowledged":
            session["state"] = "configuration"
        with self.lock:
            self.records.append((time.monotonic(), session["player"], name,
                                 payload[offset:] if name in TAPPED else None, session["protocol"]))

    def received(self, player, names=None, since=0.0, until=None):
        """[(time, name, value)] the backend got from `player`, decoded."""
        names = {names} if isinstance(names, str) else names
        with self.lock:
            records = [r for r in self.records if r[1] == player and r[0] >= since
                       and (until is None or r[0] <= until) and (names is None or r[2] in names)]
        found = []
        for when, _, name, body, protocol in records:
            value = None
            if body is not None:
                decoder = self.names(protocol, "play")
                try:
                    value = self.decode(decoder, name, body)
                except Exception as error:  # Kept, undecoded: still evidence.
                    value = {"undecoded": str(error)}
            found.append((when, name, value))
        return found

    @staticmethod
    def decode(decoder, name, body):
        packet_id = next(i for i, n in decoder.names.items() if n == name)
        return decoder.packet(packet_id, body)[1]

    def stop(self):
        try:
            self.listener.close()
        except OSError:
            pass
        for sock in self.sockets:
            try:
                sock.close()
            except OSError:
                pass


# --- Rift ------------------------------------------------------------------------------

class Rift:
    """Rift with the test's menus: in rift.lua (script mode, online, needs a
    test-session-server build) or in a persistent Lua plugin (plugin mode)."""

    def __init__(self, binary, directory, mode, main, second, session_url, tap):
        self.binary, self.directory, self.mode = binary, Path(directory), mode
        self.main, self.second, self.session_url = main, second, session_url
        self.tap = tap  # Between Rift and the main backend.
        self.log = self.directory / "rift.log"
        self.proc = self.port = self.web = None
        self.token = "menus-" + uuid.uuid4().hex

    def start(self):
        self.port = env.allocate_port()
        transfer = 'rift.players.message(ctx.uuid, "no second backend")'
        lines = ['local config = require("rift.config")',
                 f'config.listeners.public = "127.0.0.1:{self.port}"',
                 f'config.backends.main = {{ address = "127.0.0.1:{self.tap.port}", version = "{self.main.release}" }}']
        if self.second is not None:
            lines.append(f'config.backends.second = {{ address = "127.0.0.1:{self.second.port}", '
                         f'version = "{self.second.release}" }}')
            transfer = 'rift.players.transfer(ctx.uuid, "second")'
        lines.append('config.routes.public = "main"')
        menus = MENUS_LUA.replace("@TRANSFER@", transfer)
        environment = dict(os.environ)
        self.web = env.allocate_port()
        lines.append(f'config.web = {{ enabled = true, listen = "127.0.0.1:{self.web}", ui = false, '
                     f'token = "{self.token}" }}')
        plugins = {"othertest": OTHER_LUA.replace("@GEN@", "1")}
        if self.mode == "script":
            lines += ["config.authentication = { online_mode = true }", 'config.forwarding = { mode = "bungeecord" }',
                      menus, SCRIPT_LUA]
            environment["RIFT_TEST_SESSION_URL"] = self.session_url
        else:
            plugins = {"menutest": menus + PLUGIN_LUA, **plugins}
        for name, source in plugins.items():
            lines.append(f'rift.plugin("{name}")')
            folder = self.directory / "plugins" / name
            folder.mkdir(parents=True, exist_ok=True)
            (folder / "plugin.toml").write_text(PLUGIN_TOML.replace("@NAME@", name))
            (folder / "init.lua").write_text(source)
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

    def post(self, body, path="/ext/menu"):
        """One request to a plugin's http hook; (status, body)."""
        return self.request("POST", path, body)

    def request(self, method, path, body=None):
        """One request to Rift's web interface with the administrator token; (status, body)."""
        request = urllib.request.Request(f"http://127.0.0.1:{self.web}{path}",
                                         data=None if body is None else body.encode(), method=method,
                                         headers={"Authorization": f"Bearer {self.token}",
                                                  "Content-Type": "application/json"})
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

    def act(self, name):
        """Run one of the Lua ACTIONS (show a live menu, redraw, patch); the
        Lua call's result as text, None if it did not answer."""
        if self.rift.mode == "script":
            mark = self.client.mark()
            self.command(f"raction {name}")
            deadline = time.monotonic() + 6
            while time.monotonic() < deadline:
                for record in self.client.packets(CHAT, mark):
                    text = chat_text(record)
                    if f"ACTRESULT {name} " in text:
                        result = text.split(f"ACTRESULT {name} ", 1)[1].split()[0]
                        self.check(f"{name}: the call in rift.lua ran", result in ("true", "false"), text)
                        return result
                self.client.check()
                time.sleep(0.05)
            self.check(f"{name}: the call in rift.lua ran", False, "no answer to the command")
            return None
        status, body = self.rift.post(f"act {self.client.name} {name}")
        self.check(f"{name}: the plugin's call ran", status == 200 and "queued=" in body, f"HTTP {status} {body[-300:]}")
        return body.split("queued=", 1)[1].strip() if "queued=" in body else None

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
            body, separator, typed = text[index + 7:].partition(" text=")
            fields = dict(part.split("=", 1) for part in body.split() if "=" in part)
            fields["text"] = typed.strip() if separator else "nil"
            fields["_at"] = record.index
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
            names = {"chest": "minecraft:chest", "hopper": "minecraft:hopper", "dispenser": "minecraft:dispenser",
                     "anvil": "minecraft:anvil"}
            # An anvil is announced with 0 slots; with any other number the client shows a chest.
            slots = rows * 9 if kind == "chest" else 0 if kind == "anvil" else SLOT_COUNT[kind]
            ok = value.get("inventoryType") == names[kind] and value.get("slotCount") == slots
            return ok, f"{value.get('inventoryType')} with {value.get('slotCount')} slots"
        names = {"chest": f"minecraft:generic_9x{rows}", "hopper": "minecraft:hopper",
                 "dispenser": "minecraft:generic_3x3", "anvil": "minecraft:anvil"}
        got = self.registries.name("menu", value.get("inventoryType")) if self.registries else None
        if got is None or got.startswith("<"):
            return True, f"menu ID {value.get('inventoryType')} (no registry names to compare)"
        return got == names[kind], f"menu {got} (ID {value.get('inventoryType')})"

    def expect_open(self, menu_id, mark, timeout=8, spec=None, same=None, count=True, label=None):
        """open_screen and one container_set_slot per filled slot. `spec`
        replaces MENUS[menu_id]; `same` True wants the container ID of the
        open menu again (a new title), False a new one (new rows); `count`
        False for a redraw, which is not another menu."""
        kind, rows, title, slots = spec or MENUS[menu_id]
        label = label or menu_id
        previous = self.container
        try:
            record = self.client.wait_packet("open_window", f"open_screen for {menu_id}", timeout, mark)
        except SessionError as error:
            self.check(f"{label}: open_screen arrives", False, error)
            return False
        value = record.value
        container = value["windowId"]
        self.container, self.menu = container, menu_id
        self.base = rows * 9 if kind == "chest" else SLOT_COUNT[kind]
        if count:
            self.opened.append(menu_id)
        self.check(f"{label}: open_screen with a container ID in 101..127", 101 <= container <= 127,
                   f"container {container}")
        if same is not None:
            self.check(f"{label}: open_screen with {'the same' if same else 'a new'} container ID",
                       (container == previous) == same, f"container {container}, before {previous}")
        ok, detail = self.window_type_ok(value, kind, rows)
        self.check(f"{label}: window type for the client's version", ok, detail)
        self.check(f"{label}: title", title in plain(value.get("windowTitle")),
                   f"title {plain(value.get('windowTitle'))!r}")
        time.sleep(0.8)
        sets = [r for r in self.client.packets("set_slot", record.index)
                if r.value.get("windowId") == container]
        by_slot = {}
        for r in sets:
            by_slot.setdefault(r.value["slot"], []).append(r.value)
        self.check(f"{label}: one container_set_slot per filled slot",
                   sorted(by_slot) == sorted(slots) and all(len(v) == 1 for v in by_slot.values()),
                   f"slots sent {sorted((s, len(v)) for s, v in by_slot.items())}, filled {sorted(slots)}")
        problems = []
        for slot, expected in slots.items():
            if slot not in by_slot:
                continue
            problems += [f"slot {slot}: {p}" for p in self.judge_stack(menu_id, by_slot[slot][0]["item"], expected)]
        self.check(f"{label}: every stack has its item, count, name and lore", not problems, "; ".join(problems))
        self.stacks = {slot: values[0]["item"] for slot, values in by_slot.items()}
        return True

    def expect_diff(self, label, mark, before, expected, changed, wait=1.0):
        """A redraw or patch of the open menu: exactly the slots `changed`
        as container_set_slot, with what `expected` (slot: spec or None for
        empty) says, one rising state ID from 1.17.1, and no open_screen."""
        time.sleep(wait)
        self.client.check()
        opens = [r.value for r in self.client.packets("open_window", mark)]
        self.check(f"{label}: no open_screen", not opens, f"open_screen {opens}")
        sets = [r.value for r in self.client.packets("set_slot", mark) if r.value.get("windowId") == self.container]
        sent = sorted(v["slot"] for v in sets)
        self.check(f"{label}: exactly the changed slots come as container_set_slot", sent == sorted(changed),
                   f"sent {sent}, changed {sorted(changed)}")
        problems = []
        for v in sets:
            slot, want = v["slot"], expected.get(v["slot"])
            if want is None:
                if key(v["item"]) is not None:
                    problems.append(f"slot {slot}: {short(v['item'])}, not empty")
            else:
                problems += [f"slot {slot}: {p}" for p in self.judge_stack(label, v["item"], want)]
            self.stacks[slot] = v["item"]
        self.check(f"{label}: every changed slot has its new item, count and name", not problems, "; ".join(problems))
        if self.protocol >= STATE_IDS and sets:
            states = sorted({v.get("stateId") for v in sets})
            self.check(f"{label}: one state ID, above the one before", len(states) == 1 and states[0] > before,
                       f"state IDs {states}, before {before}")

    def expect_nothing(self, label, mark, wait=1.2):
        """Nothing for any of Rift's containers, and no event."""
        time.sleep(wait)
        self.client.check()
        sent = [f"{r.name}({r.value.get('windowId')}, {r.value.get('slot', '')})"
                for r in self.client.packets(("open_window", "set_slot", "close_window", "window_items"), mark)
                if (r.value.get("windowId") or 0) >= 101]
        self.check(f"{label}: nothing is sent to the client", not sent, ", ".join(sent[:10]))
        self.check(f"{label}: no event", not self.events(mark), self.events(mark))

    def expect_no_close(self, label, mark, wait=0.3):
        time.sleep(wait)
        closes = [e for e in self.events(mark) if e.get("kind") == "close"]
        self.check(f"{label}: no close event", not closes, closes)

    def close_by_player(self, label, state):
        mark = self.client.mark()
        self.client.send_packet("close_window", {"windowId": self.container})
        self.expect_event(f"{label}: reason player", mark, menu=self.menu, kind="close", reason="player",
                          state=state)

    # Clicks

    def state_id(self):
        state = 0
        for record in self.client.packets(("set_slot", "window_items")):
            if record.value.get("windowId") == self.container and "stateId" in record.value:
                state = record.value["stateId"]
        return state

    def send_click(self, slot, button, mode, changed=(), cursor=None, item=None, container=None, state=None):
        p = self.protocol
        value = {"windowId": self.container if container is None else container,
                 "slot": slot, "mouseButton": button, "mode": mode}
        if p >= CHANGED_SLOTS:
            value["changedSlots"] = [{"location": location, "item": entry} for location, entry in changed]
            value["cursorItem"] = empty(p) if cursor is None else cursor
            if p >= STATE_IDS:
                value["stateId"] = self.state_id() if state is None else state
        else:
            self.action += 1
            value["action"] = self.action
            value["item"] = empty(p) if item is None else item
        self.client.send_packet("window_click", value)

    def stack_at(self, slot):
        return from_record(self.protocol, self.stacks.get(slot))

    def hotbar(self, index):
        return self.base + 27 + index

    def real_click(self, kind, slot, key=None, state=None):
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
        self.send_click(slot, button, mode, changed=changed, cursor=cursor, item=held, state=state)

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
                # The undo comes first; a handler may redraw the slot after it.
                resent.setdefault(v["slot"], v["item"])
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

    def click_case(self, label, kind, slot, report=None, key=None, state=None):
        """One click: the event (or none), the undo, the inventory rows.
        `state` is the state ID the click names (1.17.1+; default the latest)."""
        self.client.check()
        mark = self.client.mark()
        self.real_click(kind, slot, key, state)
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

    # What the backend received (the tap between Rift and it)

    def backend_got(self, names, since, until=None, where=None):
        """[(time, name, value)] the backend received from this player."""
        return [(t, n, v) for t, n, v in self.rift.tap.received(self.client.name, names, since, until)
                if where is None or where(n, v or {})]

    def screen_leaks(self, since, until=None):
        """Packets of a game screen the backend received: renames, trades,
        beacons, and the 1.12 plugin messages for them."""
        def screen(name, value):
            if name == "custom_payload":
                return value.get("channel") in ("MC|ItemName", "MC|TrSel", "MC|Beacon")
            return True
        return self.backend_got({"name_item", "select_trade", "set_beacon_effect", "custom_payload"}, since, until,
                                screen)

    def rift_container_leaks(self, since=0.0, until=None):
        """Packets naming one of Rift's containers (101..127) the backend received."""
        def rifts(name, value):
            window = value.get("windowId", value.get("window_id"))
            return isinstance(window, int) and 101 <= window <= 127
        return self.backend_got({"window_click", "close_window", "transaction", "craft_recipe_request",
                                 "enchant_item", "set_slot_state"}, since, until, rifts)

    def requests_sent(self, since=0.0, until=None):
        """Times of Rift's inventory requests: a click on container 0 between the slots."""
        return [t for t, _, v in self.backend_got("window_click", since, until,
                                                 lambda n, v: v.get("windowId") == 0 and v.get("slot") == -1)]

    # Anvils

    def rename(self, text):
        """What the client sends for a new text in an anvil."""
        if self.client.has("name_item"):
            self.client.send_packet("name_item", {"name": text})
        else:
            data = text.encode()
            self.client.send_packet("custom_payload", {"channel": "MC|ItemName", "data": varint(len(data)) + data})

    def container_packets(self, mark, names=("set_slot", "craft_progress_bar")):
        return [r for r in self.client.packets(names, mark) if r.value.get("windowId") == self.container]

    def anvil_answers(self, mark):
        """The result slots and costs sent for the anvil since `mark`."""
        records = self.container_packets(mark)
        results = [r for r in records if r.name == "set_slot" and r.value.get("slot") == 2]
        costs = [r for r in records if r.name == "craft_progress_bar"]
        return results, costs

    def track(self, mark):
        """Remember what the client now shows in the open menu's slots."""
        for r in self.container_packets(mark, ("set_slot",)):
            self.stacks[r.value["slot"]] = r.value["item"]

    def cursor_packets(self, mark):
        cursor = []
        for r in self.client.packets(("set_slot", "set_cursor_item"), mark):
            v = r.value
            if r.name == "set_cursor_item":
                cursor.append(("set_cursor_item", key(v.get("contents")) is None))
            elif v.get("windowId") in (-1, 255) and v.get("slot") == -1:
                cursor.append(("set_slot(-1)", key(v.get("item")) is None))
        return cursor

    def check_cursor(self, label, mark):
        wanted = "set_cursor_item" if self.protocol >= WIDE else "set_slot(-1)"
        cursor = self.cursor_packets(mark)
        self.check(f"{label}: the cursor is emptied with {wanted}", (wanted, True) in cursor, f"cursor packets {cursor}")

    def check_costs(self, label, costs):
        bad = [(r.value.get("property"), r.value.get("value")) for r in costs
               if (r.value.get("property"), r.value.get("value")) != (0, 0)]
        self.check(f"{label}: every cost packet is property 0, value 0", costs and not bad,
                   f"{len(costs)} cost packets, wrong: {bad}")

    def anvil_click(self, slot, mode=0, button=0):
        """A click on an anvil slot as the client sends it."""
        p, here = self.protocol, self.stack_at(slot)
        if slot == 2:
            # Taking the result empties the inputs on the client.
            changed = [(2, empty(p)), (0, empty(p)), (1, empty(p))]
        else:
            changed = [(slot, empty(p))]
        self.send_click(slot, button, mode, changed=changed, cursor=here, item=here)

    def after_anvil_click(self, label, mark, result=False):
        """After a click in an anvil: its three slots and the cost again, the
        backend's last set_experience after a click on the result, and an
        empty cursor."""
        time.sleep(0.6)
        self.client.check()
        records = self.container_packets(mark)
        slots = sorted({r.value["slot"] for r in records if r.name == "set_slot"})
        self.check(f"{label}: the anvil's three slots come back", {0, 1, 2} <= set(slots), f"slots sent {slots}")
        last_slot = max((r.index for r in records if r.name == "set_slot"), default=-1)
        costs = [r for r in records if r.name == "craft_progress_bar" and r.index > last_slot]
        self.check_costs(f"{label}: a cost of 0 follows the slots", costs)
        if result:
            before = self.client.packets("experience", 0)
            before = [r for r in before if r.index < mark]
            after = self.client.packets("experience", mark)
            if before:
                self.check(f"{label}: the backend's last set_experience follows a click on the result",
                           after and after[-1].value == before[-1].value,
                           f"before {before[-1].value}, after {[r.value for r in after]}")
            else:
                self.note("the backend sent no set_experience before: none expected after the result click")
                self.check(f"{label}: no set_experience without one from the backend", not after,
                           f"{[r.value for r in after]}")
        self.check_cursor(label, mark)
        self.track(mark)

    def stack_text(self, slot):
        return plain(self.stacks.get(slot))

    def rows_back(self, label, mark):
        """After a click that disturbed the inventory rows: the backend sends
        the inventory again with the same items (a 1.17 backend: only when the
        menu closes). Positions are not compared: the backend may have moved
        items with real clicks the client's model never heard of."""
        full = lambda: [r for r in self.client.packets("window_items", mark) if r.value.get("windowId") == 0]
        if self.backend == 755:
            time.sleep(0.8)
            self.check(f"{label}: a 1.17 backend is not asked while the menu is open", not full(),
                       f"{len(full())} container_set_content(0)")
            return
        deadline = time.monotonic() + 4
        while not full() and time.monotonic() < deadline:
            time.sleep(0.1)
        got = full()
        items = lambda entries: sorted(short(json.loads(e)) for e in entries if e)
        ok = bool(got)
        detail = f"{len(got)} container_set_content(0)"
        if ok:
            after = inventory(self.client).snapshot()
            ok = items(after["slots"][9:45]) == items(self.baseline["slots"][9:45])
            detail += f"; items now {items(after['slots'][9:45])}, before {items(self.baseline['slots'][9:45])}"
        self.check(f"{label}: the backend's inventory comes back with the same items", ok, detail)

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
            if self.options.only and step.__name__ not in self.options.only | ALWAYS:
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
        # What the backend received over the whole pair.
        tap = self.rift.tap
        self.check("the tap between Rift and the backend followed every connection", not tap.errors, tap.errors[:3])
        leaks = self.rift_container_leaks()
        self.check("the backend never received a packet naming one of Rift's containers", not leaks, leaks[:3])
        sent = self.requests_sent()
        gaps = [round(b - a, 3) for a, b in zip(sent, sent[1:])]
        # The tap reads what Rift wrote at once; 20 ms for scheduling.
        self.check("two inventory requests are at least 250 ms apart", all(g >= 0.23 for g in gaps),
                   f"{len(sent)} requests, gaps below 250 ms: {[g for g in gaps if g < 0.25]}")
        self.note(f"{len(sent)} inventory requests, shortest gap {min(gaps) if gaps else None} s")


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
    first = time.monotonic()
    for _ in range(10):
        pair.real_click("shift_left", BUY)
        last = time.monotonic()
        time.sleep(0.05)
    time.sleep(2.0)
    events = [e for e in pair.events(mark) if e.get("click") == "shift_left"]
    pair.check("a burst of ten shift-clicks: ten events", len(events) == 10, f"{len(events)} events")
    answers = [r for r in pair.client.packets("window_items", mark) if r.value.get("windowId") == 0]
    if pair.backend != 755:
        # At most one request on its way and at least 250 ms between two,
        # with one that always follows the last click.
        sent = pair.requests_sent(first)
        allowed = int((last - first) / 0.25) + 2
        offsets = [round(t - first, 3) for t in sent]
        pair.note(f"a burst of ten shift-clicks in {last - first:.2f} s: {len(sent)} requests at {offsets}, "
                  f"{len(answers)} inventory answers")
        pair.check("a burst of ten shift-clicks: the backend gets a few requests, not one per click",
                   1 <= len(sent) <= allowed, f"{len(sent)} requests at {offsets} (at most {allowed})")
        pair.check("a burst of ten shift-clicks: a request follows the last click", sent and sent[-1] >= last,
                   f"requests at {offsets}, last click at {round(last - first, 3)}")
        pair.check("a burst of ten shift-clicks: the inventory comes back, once per request at most",
                   1 <= len(answers) <= max(len(sent), 1), f"{len(answers)} container_set_content(0)")
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
    while a menu is open and for a second after Rift itself closed the menu.
    After the player closed it, the packet passes at once. Evidence: the tap
    (every server) and the backend's own view (not on Pumpkin)."""
    name, p = pair.client.name, pair.protocol
    console = pair.software != "pumpkin"
    pair.console(pair.cmd.gamemode(name, "creative"), 0.8)
    stone = (pair.registries.id("item", "stone") if pair.registries else None) or 1
    if p >= 766:
        item = {"itemCount": 1, "itemId": stone, "addedComponentCount": 0, "removedComponentCount": 0,
                "components": [], "removeComponents": []}
    else:
        item = stack(p, stone, 1)

    def creative(label, passes):
        since = time.monotonic()
        # Hotbar slot 9, which is empty: the control really puts the stone there.
        pair.client.send_packet("set_creative_slot", {"slot": 44, "item": item})
        time.sleep(1.0)
        pair.client.check()
        got = pair.backend_got("set_creative_slot", since)
        pair.check(f"{label}: the creative-mode slot {'reaches' if passes else 'does not reach'} the backend",
                   bool(got) == passes, f"the backend received {len(got)}")
        if console:
            has = pair.item_in_slot(8, "stone")
            pair.check(f"{label}: the backend {'has' if passes else 'has no'} stone in the last hotbar slot",
                       has == passes, f"stone there: {has}")
            if has:
                pair.console(f"clear {name} {'minecraft:' if pair.backend >= 393 else ''}stone", 0.5)

    try:
        open_menu(pair, "chest")
        creative("while a menu is open", False)
        mark = pair.client.mark()
        pair.client.send_packet("close_window", {"windowId": pair.container})
        pair.expect_event("closed by the player in creative mode: reason player", mark, menu="chest",
                          kind="close", reason="player")
        time.sleep(0.2)
        creative("just after the player closed the menu", True)
        open_menu(pair, "chest")
        mark = pair.client.mark()
        pair.close_any()
        pair.client.wait_packet("close_window", "Rift's container_close", 6, mark)
        creative("within a second after Rift closed the menu", False)
        close_expected(pair, "closed by close_menu in creative mode", mark, "chest", "plugin", "st-chest:1", True)
        time.sleep(0.3)
        creative("more than a second after Rift closed the menu", True)
    finally:
        pair.console(pair.cmd.gamemode(name, "survival"), 0.5)


def step_same_id(pair):
    """Showing the open menu again, unchanged, is a redraw with nothing to
    send: no packet, no close event, and the menu keeps working."""
    open_menu(pair, "chest")
    mark = pair.client.mark()
    pair.show("chest")
    pair.expect_nothing("the same definition again", mark, wait=1.5)
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
    # An anvil, so the close event also shows the text typed last.
    open_anvil(pair, "anvil", "anvil", anvil_spec("Rift anvil", "Steve"), label="the anvil left open")
    renames(pair, "a rename before leaving", ["Bye"], "Bye")
    mark = observer.mark()
    pair.left = True
    pair.client.close()
    event = pair.wait_event(mark, timeout=10, session=observer, menu="anvil", kind="close")
    pair.check("leaving with an anvil open: reason disconnect with the text (seen by another player)",
               event is not None and event.get("reason") == "disconnect" and event.get("text") == "Bye",
               f"close event {event}")


def step_one_close(pair):
    """Every menu that opened ended with exactly one close event."""
    events = pair.events(0, session=pair.observer or pair.client) if pair.left else pair.events(0)
    if pair.left and pair.observer is not None:
        events = pair.events(0) + [e for e in pair.events(0, session=pair.observer)
                                   if e.get("reason") == "disconnect"]
    closes = [e["menu"] for e in events if e.get("kind") == "close"]
    pair.check("exactly one close event per menu, in order", closes == pair.opened,
               f"opened {pair.opened}, closed {closes}")


def open_live(pair, variant="live"):
    mark = pair.client.mark()
    pair.act(variant)
    if not pair.expect_open("live", mark, spec=live_spec(variant), label=variant):
        raise Skip(f"{variant} did not open")


def click_report(action, slot, payload, state, click="left"):
    return dict(action=action, slot=slot, payload=payload, state=state, click=click, key="nil")


def step_redraw(pair):
    """Showing the open menu's id again: two changed slots send exactly those
    two container_set_slot; a new title sends open_screen with the same
    container ID and every filled slot; another number of rows opens anew
    with a new container ID. None is a close; clicks afterwards report with
    the new state."""
    open_live(pair)
    mark, before = pair.client.mark(), pair.state_id()
    pair.act("live_two")
    pair.expect_diff("a redraw that changes two slots", mark, before, live_spec("live_two")[3], [LIVE_A, LIVE_B])
    pair.expect_no_close("a redraw that changes two slots", mark)
    pair.click_case("after the redraw: left on the button that does something else", "left", LIVE_B,
                    report=click_report("b2", LIVE_B, "b2:2", "st-live:1"))
    mark = pair.client.mark()
    pair.act("live_title")
    pair.expect_open("live", mark, spec=live_spec("live_title"), same=True, count=False,
                     label="a redraw with a new title")
    pair.expect_no_close("a redraw with a new title", mark)
    pair.click_case("after the new title: left on a button", "left", LIVE_A,
                    report=click_report("a", LIVE_A, "a:1", "st-live:2"))
    mark = pair.client.mark()
    pair.act("live_rows")
    pair.expect_open("live", mark, spec=live_spec("live_rows"), same=False, count=False,
                     label="a redraw with four rows instead of three")
    pair.expect_no_close("a redraw with four rows instead of three", mark)
    pair.click_case("after the new rows: left on a button", "left", LIVE_A,
                    report=click_report("a", LIVE_A, "a:1", "st-live:3"))
    pair.close_by_player("the redrawn menu closed", "st-live:3")


def step_update_menu(pair):
    """update_menu with slots (one emptied), keys, a title and a state; for a
    menu the player closed, for an unknown id and from another plugin
    (nothing is sent in those three cases)."""
    open_live(pair)
    slots = dict(live_spec("live")[3])
    mark, before = pair.client.mark(), pair.state_id()
    pair.act("patch_slots")
    slots[LIVE_A], slots[LIVE_C] = ("gold_ingot", 1, "Patched", []), None
    pair.expect_diff("update_menu with slots", mark, before, slots, [LIVE_A, LIVE_C])
    pair.click_case("after the slots patch: left on the patched button", "left", LIVE_A,
                    report=click_report("pa", LIVE_A, "pa:9", "st-live:0"))
    pair.click_case("after the slots patch: left on the emptied slot", "left", LIVE_C)
    mark, before = pair.client.mark(), pair.state_id()
    pair.act("patch_keys")
    for slot in LIVE_GLASS:
        slots[slot] = ("iron_bars", 1, None, [])
    slots[LIVE_B] = ("diamond", 1, "Bravo keyed", [])
    pair.expect_diff("update_menu with keys", mark, before, slots, LIVE_GLASS + [LIVE_B])
    mark = pair.client.mark()
    pair.act("patch_title")
    pair.expect_open("live", mark, spec=("chest", 3, "Live patched", {s: v for s, v in slots.items() if v}),
                     same=True, count=False, label="update_menu with a title")
    pair.expect_no_close("update_menu with a title", mark)
    mark = pair.client.mark()
    pair.act("patch_state")
    pair.expect_nothing("update_menu with only a state", mark)
    pair.click_case("after the state patch: left on a keyed button", "left", LIVE_B,
                    report=click_report("b", LIVE_B, "b:1", "st-live:7"))
    pair.close_by_player("the patched menu closed", "st-live:7")
    mark = pair.client.mark()
    result = pair.act("patch_slots")
    pair.note(f"update_menu for a closed menu returned {result}")
    pair.expect_nothing("update_menu for a menu the player closed", mark)
    # Another plugin's and an unknown id.
    open_live(pair)
    mark = pair.client.mark()
    status, body = pair.rift.post(f"patch {pair.client.name} live", path="/ext/other")
    pair.note(f"another plugin's update_menu answered HTTP {status} {body[-100:]}")
    pair.expect_nothing("another plugin's update_menu for this id", mark)
    mark = pair.client.mark()
    result = pair.act("patch_unknown")
    pair.note(f"update_menu for an unknown id returned {result}")
    pair.expect_nothing("update_menu for an unknown id", mark)
    mark = pair.client.mark()
    pair.rift.post(f"close {pair.client.name} live", path="/ext/other")
    pair.expect_nothing("another plugin's close_menu for this id", mark)
    pair.click_case("after the other plugin's calls: left on a button", "left", LIVE_B,
                    report=click_report("b", LIVE_B, "b:1", "st-live:0"))
    pair.close_by_player("the live menu closed", "st-live:0")
    other = [chat_text(r) for r in pair.client.packets(CHAT) if "OTHEREV" in chat_text(r)]
    pair.check("the other plugin never got an event of these menus", not other, other[:5])


def step_pages(pair):
    """The paged list of docs/menus.md: next page twice, the page in `state`,
    the payloads of the clicks."""
    mark = pair.client.mark()
    pair.act("kits")
    if not pair.expect_open("kits", mark, spec=kits_spec(1), label="kits, page 1"):
        raise Skip("kits did not open")
    for page in (1, 2):
        mark = pair.client.mark()
        pair.real_click("left", KITS_NEXT)
        pair.expect_event(f"next on page {page}: reported with the page in state", mark, kind="click", menu="kits",
                          action="page", slot=KITS_NEXT, payload="step:1", state=f"page:{page}")
        pair.expect_open("kits", mark, spec=kits_spec(page + 1), same=True, count=False,
                         label=f"kits, page {page + 1}")
        pair.expect_no_close(f"kits, page {page + 1}", mark)
    pair.click_case("a kit on page 3", "left", 0, report=click_report("kit", 0, "kit:k3a", "page:3"))
    mark = pair.client.mark()
    pair.real_click("left", KITS_BACK)
    pair.expect_event("back on page 3: reported", mark, kind="click", menu="kits", action="page",
                      slot=KITS_BACK, payload="step:-1", state="page:3")
    pair.expect_open("kits", mark, spec=kits_spec(2), same=True, count=False, label="kits, back to page 2")
    pair.click_case("a kit on page 2", "left", 1, report=click_report("kit", 1, "kit:k2b", "page:2"))
    pair.close_by_player("the kits closed", "page:2")


def step_stale_click(pair):
    """A redraw changes what two slots do (one into a close button) and only
    the look of a third. From 1.17.1 a click that names the state ID before
    the redraw is undone and not reported on the first two, and does not
    close; it still reports on the third; with the current state ID it
    reports. Before 1.17.1 every click reports."""
    open_live(pair)
    mark, old = pair.client.mark(), pair.state_id()
    pair.act("live_stale")
    pair.expect_diff("a redraw that changes two actions and one look", mark, old, live_spec("live_stale")[3],
                     [LIVE_A, LIVE_B, LIVE_C])
    if pair.protocol >= STATE_IDS:
        pair.click_case("the old state ID on a slot whose action changed", "left", LIVE_B, state=old)
        mark = pair.client.mark()
        pair.click_case("the old state ID on a slot that became a close button", "left", LIVE_C, state=old)
        closes = [r.value for r in pair.client.packets("close_window", mark)]
        pair.check("the old state ID on a close button: the menu stays open", not closes, f"container_close {closes}")
        pair.click_case("the old state ID on a slot that only looks different", "left", LIVE_A, state=old,
                        report=click_report("a", LIVE_A, "a:1", "st-live:1"))
    else:
        pair.note("the client names no state ID: every click reports")
        pair.click_case("a slot whose action changed (no state ID)", "left", LIVE_B,
                        report=click_report("b2", LIVE_B, "b2:2", "st-live:1"))
    pair.click_case("the current state ID on a slot whose action changed", "left", LIVE_B,
                    report=click_report("b2", LIVE_B, "b2:2", "st-live:1"))
    pair.click_case("the current state ID on a slot that only looks different", "left", LIVE_A,
                    report=click_report("a", LIVE_A, "a:1", "st-live:1"))
    mark = pair.client.mark()
    pair.real_click("left", LIVE_C)
    close_expected(pair, "the current state ID on the new close button", mark, "live", "plugin", "st-live:1", True)


def step_redraw_burst(pair):
    """20 clicks in a second on a button whose handler redraws the menu: the
    client stays connected, every click is reported (the action never
    changes), and the picture agrees with the menu's state afterwards."""
    mark = pair.client.mark()
    pair.act("counter")
    if not pair.expect_open("counter", mark, spec=COUNTER):
        raise Skip("the counter did not open")
    mark = pair.client.mark()
    for _ in range(20):
        pair.real_click("left", COUNTER_SLOT)
        time.sleep(0.05)
    time.sleep(3.0)
    pair.check("a burst of 20 clicks on a redrawing button: the client stays connected", not pair.client.failure,
               pair.client.failure or "")
    pair.client.check()
    events = [e for e in pair.events(mark) if e.get("action") == "inc"]
    counts = [int(e["state"].split(":")[1]) for e in events if re.fullmatch(r"count:\d+", e.get("state", ""))]
    pair.check("a burst of 20 clicks: 20 events", len(events) == 20, f"{len(events)} events, states {counts}")
    pair.check("a burst of 20 clicks: the states the events carry never go back", counts == sorted(counts),
               f"states {counts}")
    opens = pair.client.packets("open_window", mark)
    pair.check("a burst of 20 clicks: the redraws send no open_screen", not opens, f"{len(opens)} open_screen")
    sets = [r.value for r in pair.client.packets("set_slot", mark)
            if r.value.get("windowId") == pair.container and r.value.get("slot") == COUNTER_SLOT]
    shown = re.search(r"Count (\d+)", plain(sets[-1]["item"])) if sets else None
    shown = int(shown.group(1)) if shown else None
    pair.note(f"a burst of 20 clicks: states {counts}, the slot shows {shown}")
    if counts:
        pair.check("a burst of 20 clicks: the slot shows the count after the last event", shown == counts[-1] + 1,
                   f"shown {shown}, last event's state {counts[-1]}")
    if sets:
        pair.stacks[COUNTER_SLOT] = sets[-1]["item"]
    pair.click_case("after the burst: a click carries the state the slot shows", "left", COUNTER_SLOT,
                    report=click_report("inc", COUNTER_SLOT, "nil", f"count:{shown}"))
    time.sleep(0.5)
    pair.close_by_player("the counter closed", f"count:{(shown or 0) + 1}")


def anvil_spec(title, text, extra=None):
    slots = {0: ("paper", 1, text, []), 2: ("paper", 1, text, [])}
    slots.update(extra or {})
    return ("anvil", None, title, slots)


RIGHT = {1: ("emerald", 1, "Right", [])}


def open_anvil(pair, action, menu_id, spec, label=None, same=None, count=True):
    """Show an anvil: open_screen, its filled slots, then a cost of 0."""
    mark = pair.client.mark()
    pair.act(action)
    if not pair.expect_open(menu_id, mark, spec=spec, label=label or action, same=same, count=count):
        raise Skip(f"{action} did not open")
    records = pair.container_packets(mark)
    last_slot = max((r.index for r in records if r.name == "set_slot"), default=-1)
    pair.check_costs(f"{label or action}: a cost of 0 after the slots",
                     [r for r in records if r.name == "craft_progress_bar" and r.index > last_slot])
    return mark


def anvil_report(text, state, action="submit", slot=2, payload="nil", click="left"):
    return dict(action=action, slot=slot, payload=payload, state=state, click=click, key="nil", text=text)


def click_result(pair, label, text, state):
    """A left click on the result: reported with the text; then the three
    slots, the cost, set_experience and an empty cursor."""
    mark = pair.client.mark()
    pair.anvil_click(2)
    pair.expect_event(f"{label}: reported with the text", mark, only=True, kind="click", menu=pair.menu,
                      **anvil_report(text, state))
    pair.after_anvil_click(label, mark, result=True)
    return mark


def renames(pair, label, texts, final, pause=0.0):
    """Renames as the client sends them; each gets the result slot and a cost
    of 0 back, the result for the final text comes last."""
    mark = pair.client.mark()
    since = time.monotonic()
    for text in texts:
        pair.rename(text)
        if pause:
            time.sleep(pause)
    time.sleep(0.6)
    pair.client.check()
    results, costs = pair.anvil_answers(mark)
    pair.check(f"{label}: every rename gets the result slot back", len(results) >= len(texts),
               f"{len(results)} result slots for {len(texts)} renames")
    pair.check(f"{label}: every rename gets a cost back", len(costs) >= len(texts),
               f"{len(costs)} costs for {len(texts)} renames")
    pair.check_costs(label, costs)
    last = plain(results[-1].value["item"]) if results else ""
    pair.check(f"{label}: the last result is named after the final text", final in last,
               f"last result {last!r}")
    leaks = pair.screen_leaks(since)
    pair.check(f"{label}: the backend received none of the renames", not leaks, leaks[:3])
    pair.track(mark)
    return mark, results


def step_anvil(pair):
    """Anvil menus (docs/menus.md, "Text input with an anvil")."""
    p = pair.protocol
    limit = 50 if p >= 755 else 35
    since = time.monotonic()
    # Defaults: paper named after the text left, its submit copy as the result.
    open_anvil(pair, "anvil", "anvil", anvil_spec("Rift anvil", "Steve"), label="anvil with defaults")
    pair.check("the anvil's right slot stays empty", 1 not in pair.stacks, sorted(pair.stacks))
    # Typing fast: the result is built at most every 50 ms, a rename in
    # between gets the last result, the final text always its own.
    texts = [f"Fast{n}" for n in range(1, 9)]
    mark, results = renames(pair, "eight renames at once", texts, "Fast8")
    names = [next((t for t in texts if t in plain(r.value["item"])), "?") for r in results]
    built = len(set(names))
    pair.note(f"eight renames at once: {len(results)} results named {names}")
    pair.check("eight renames at once: the result is not built again for every rename", built < len(texts),
               f"{built} distinct results for {len(texts)} renames")
    pair.expect_no_event("an anvil without typing reports no text event", mark, wait=0.3)
    click_result(pair, "the result after fast typing", "Fast8", "st-anvil:1")
    pair.check("after the click the left item is named after the typed text", "Fast8" in pair.stack_text(0),
               pair.stack_text(0))
    # An over-long rename is ignored; control characters and § are removed.
    mark = pair.client.mark()
    pair.rename("L" * (limit + 1))
    time.sleep(0.6)
    results, costs = pair.anvil_answers(mark)
    pair.check(f"a rename of {limit + 1} characters is ignored (nothing comes back)", not results and not costs,
               f"{len(results)} results, {len(costs)} costs")
    click_result(pair, "the result after an over-long rename", "Fast8", "st-anvil:1")
    renames(pair, f"a rename of exactly {limit} characters", ["M" * limit], "M" * limit)
    click_result(pair, f"the result after {limit} characters", "M" * limit, "st-anvil:1")
    renames(pair, "a rename with a section sign", ["Cl§ean"], "Clean")
    click_result(pair, "the result after a section sign", "Clean", "st-anvil:1")
    # A shift-click in the inventory rows moves an item into the anvil on the client.
    mark = pair.client.mark()
    row = pair.hotbar(0)
    held = from_record(p, json.loads(pair.baseline_slot(row))) if pair.baseline_slot(row) else stack(p, 1, 5)
    pair.send_click(row, 0, 1, changed=[(row, empty(p)), (1, held)], cursor=empty(p), item=held)
    pair.expect_no_event("a shift-click of an inventory item into the anvil: not reported", mark, wait=0.6)
    pair.after_anvil_click("a shift-click of an inventory item into the anvil", mark)
    pair.check("after the shift-click the right slot is empty again", key(pair.stacks.get(1)) is None,
               short(pair.stacks.get(1)))
    pair.rows_back("a shift-click of an inventory item into the anvil", mark)
    # A redraw keeps the typed text; a new right button brings the result along.
    mark, before = pair.client.mark(), pair.state_id()
    pair.act("anvil_redraw")
    pair.expect_diff("an anvil redrawn with a right button", mark, before,
                     {1: RIGHT[1], 2: ("paper", 1, "Clean", [])}, [1, 2])
    records = pair.container_packets(mark)
    pair.check("an anvil redrawn: a cost of 0 follows", records and records[-1].name == "craft_progress_bar",
               [r.name for r in records])
    pair.expect_no_close("an anvil redrawn", mark)
    click_result(pair, "the result after the redraw (the typed text stays)", "Clean", "st-anvil:2")
    mark = pair.client.mark()
    pair.anvil_click(1)
    pair.expect_event("the right button: reported with the text", mark, only=True, kind="click", menu="anvil",
                      **anvil_report("Clean", "st-anvil:2", action="right", slot=1, payload="right:2"))
    pair.after_anvil_click("the right button", mark)
    # A new title opens the screen again with the same container; the text stays.
    mark = open_anvil(pair, "anvil_title", "anvil", anvil_spec("Rift anvil renamed", "Clean", RIGHT),
                      label="an anvil with a new title", same=True, count=False)
    pair.expect_no_close("an anvil with a new title", mark)
    click_result(pair, "the result after the new title", "Clean", "st-anvil:3")
    # A patch with text replaces it: the left item renamed, the result after it.
    mark = pair.client.mark()
    pair.act("anvil_patch_text")
    time.sleep(0.8)
    records = pair.container_packets(mark)
    sets = [(r.value["slot"], plain(r.value["item"])) for r in records if r.name == "set_slot"]
    pair.check("update_menu with text: the left item and the result named after it",
               [slot for slot, _ in sets] == [0, 2] and all("Patched" in text for _, text in sets),
               f"sent {sets}")
    pair.check("update_menu with text: a cost of 0 follows", records and records[-1].name == "craft_progress_bar",
               [r.name for r in records])
    pair.track(mark)
    click_result(pair, "the result after the text patch", "Patched", "st-anvil:3")
    renames(pair, "a rename after the text patch", ["Other"], "Other")
    mark = pair.client.mark()
    pair.act("anvil_patch_text")
    time.sleep(0.8)
    records = pair.container_packets(mark)
    sets = [(r.value["slot"], key(r.value["item"]) is None, plain(r.value["item"])) for r in records
            if r.name == "set_slot"]
    pair.check("update_menu with the text the left item already has: an empty left slot first, then the item",
               [(slot, gone) for slot, gone, _ in sets] == [(0, True), (0, False), (2, False)]
               and all("Patched" in text for _, gone, text in sets if not gone), f"sent {sets}")
    pair.track(mark)
    click_result(pair, "the result after the second text patch", "Patched", "st-anvil:3")
    # Endings, each with the text.
    mark = pair.client.mark()
    pair.client.send_packet("close_window", {"windowId": pair.container})
    pair.expect_event("the anvil closed by the player: reason player with the text", mark, menu="anvil",
                      kind="close", reason="player", state="st-anvil:3", text="Patched")
    tap_mark = time.monotonic()
    mark = pair.client.mark()
    pair.rename("Late")
    time.sleep(0.6)
    pair.check("a late rename after the anvil closed: nothing comes back", not pair.anvil_answers(mark)[0])
    pair.check("a late rename after the anvil closed does not reach the backend", not pair.screen_leaks(tap_mark),
               pair.screen_leaks(tap_mark))
    open_anvil(pair, "anvil", "anvil", anvil_spec("Rift anvil", "Steve"), label="the anvil again")
    renames(pair, "a rename before close_menu", ["ByPlugin"], "ByPlugin")
    mark = pair.client.mark()
    pair.act("anvil_close")
    close_expected(pair, "the anvil closed by close_menu", mark, "anvil", "plugin", "st-anvil:1", True)
    pair.expect_event("the anvil closed by close_menu: the close event has the text", mark, menu="anvil",
                      kind="close", text="ByPlugin")
    open_anvil(pair, "anvil", "anvil", anvil_spec("Rift anvil", "Steve"), label="the anvil to be replaced")
    renames(pair, "a rename before another menu", ["Replaced"], "Replaced")
    mark = pair.client.mark()
    pair.act("chest")
    if pair.expect_open("chest", mark):
        pair.expect_event("the anvil replaced by a chest: reason replaced with the text", mark, menu="anvil",
                          kind="close", reason="replaced", state="st-anvil:1", text="Replaced")
        pair.close_by_player("the chest after the anvil", "st-chest:1")
    open_anvil(pair, "anvil", "anvil", anvil_spec("Rift anvil", "Steve"), label="the anvil before a death")
    renames(pair, "a rename before a death", ["Dead"], "Dead")
    mark = pair.client.mark()
    pair.console(f"kill {pair.client.name}", 1.0)
    try:
        pair.client.send_packet("client_command", {"actionId": "perform_respawn"})
    except Exception:
        pair.client.send_packet("client_command", {"actionId": 0})
    pair.expect_event("death and respawn with an anvil open: reason server with the text", mark, menu="anvil",
                      kind="close", reason="server", state="st-anvil:1", text="Dead")
    time.sleep(1.0)
    anvil_typing(pair, limit)
    anvil_custom(pair)
    # Control: after a chest (not an anvil) was the last screen, a rename is
    # the backend's again, and the tap sees it.
    mark = open_menu(pair, "dispenser")
    pair.close_by_player("the dispenser before the control rename", "st-disp:3")
    tap_mark = time.monotonic()
    pair.rename("Control")
    time.sleep(0.8)
    seen = pair.screen_leaks(tap_mark)
    pair.check("control: without Rift's anvil a rename reaches the backend (the tap sees it)", seen,
               "the backend received no rename")
    leaks = pair.rift_container_leaks(since)
    pair.check("no anvil packet naming Rift's container reached the backend", not leaks, leaks[:3])


def anvil_typing(pair, limit):
    """typing = true: text events at most 5 a second, always the final text."""
    open_anvil(pair, "anvil_typing", "anvilt", anvil_spec("Typing anvil", "Alex"), label="an anvil with typing")
    mark = pair.client.mark()
    started = time.monotonic()
    texts = [f"Type{n}" for n in range(1, 13)]
    renames(pair, "twelve renames in 1.2 s", texts, "Type12", pause=0.1)
    duration = time.monotonic() - started - 0.6  # renames() waits 0.6 s after the last.
    time.sleep(0.4)
    events = [e for e in pair.events(mark) if e.get("kind") == "text"]
    got = [e["text"] for e in events]
    allowed = int(duration * 5) + 2
    pair.note(f"twelve renames in {duration:.2f} s: text events {got}")
    pair.check("typing: at most 5 text events a second", 2 <= len(events) <= allowed,
               f"{len(events)} text events in {duration:.2f} s (at most {allowed}): {got}")
    pair.check("typing: the last text event has the final text", got[-1:] == ["Type12"], got)
    pair.check("typing: the text events come in the order typed", got == [t for t in texts if t in got], got)
    pair.check("typing: every text event names the menu and its state",
               all(e.get("menu") == "anvilt" and e.get("state") == "st-anvilt:1" for e in events), events[:3])
    mark = pair.client.mark()
    pair.rename("Type12")
    pair.expect_no_event("typing: a rename that does not change the text gives no event", mark, wait=0.6)
    mark = pair.client.mark()
    renames(pair, "six renames at once with typing", [f"Burst{n}" for n in range(1, 7)], "Burst6")
    time.sleep(0.4)
    got = [e["text"] for e in pair.events(mark) if e.get("kind") == "text"]
    pair.check("six renames at once: one text event at once and one for the final text",
               1 <= len(got) <= 2 and got[-1:] == ["Burst6"], got)
    mark = pair.client.mark()
    pair.rename("X" * (limit + 1))
    pair.expect_no_event("typing: an over-long rename gives no event", mark, wait=0.6)
    # The menu ends before the next text event is due: the close event has the final text.
    mark = pair.client.mark()
    pair.rename("Last1")
    pair.rename("Last2")
    pair.client.send_packet("close_window", {"windowId": pair.container})
    pair.expect_event("typing, closed right after typing: the close event has the final text", mark,
                      menu="anvilt", kind="close", reason="player", state="st-anvilt:1", text="Last2")
    late = [e["text"] for e in pair.events(mark) if e.get("kind") == "text"]
    pair.check("typing, closed right after typing: no text event after the close", "Last2" not in late, late)


def anvil_custom(pair):
    """Buttons of the plugin's own in all three slots."""
    spec = ("anvil", None, "Custom anvil", {0: ("name_tag", 1, "Custom", []), 1: RIGHT[1],
                                             2: ("diamond", 1, "Confirm", [])})
    open_anvil(pair, "anvil_custom", "anvilc", spec, label="an anvil with buttons in all three slots")
    pair.check("a custom left button is named after the text, not its own name",
               "Not this name" not in pair.stack_text(0), pair.stack_text(0))
    for slot, action, payload in ((0, "left", "left:1"), (1, "right", "right:2"), (2, "ok", "ok:3")):
        mark = pair.client.mark()
        pair.anvil_click(slot)
        pair.expect_event(f"custom anvil: the button in slot {slot} reported with the text", mark, only=True,
                          kind="click", menu="anvilc",
                          **anvil_report("Custom", "st-anvilc:1", action=action, slot=slot, payload=payload))
        pair.after_anvil_click(f"custom anvil: slot {slot}", mark, result=slot == 2)
    mark, results = renames(pair, "custom anvil: a rename", ["Mine"], "Confirm")
    pair.check("custom anvil: a rename keeps the plugin's result button",
               all("Confirm" in plain(r.value["item"]) for r in results), [plain(r.value["item"]) for r in results])
    mark = pair.client.mark()
    pair.anvil_click(2)
    pair.expect_event("custom anvil: the result after a rename", mark, only=True, kind="click", menu="anvilc",
                      **anvil_report("Mine", "st-anvilc:1", action="ok", payload="ok:3"))
    pair.after_anvil_click("custom anvil: the result after a rename", mark, result=True)
    mark, before = pair.client.mark(), pair.state_id()
    pair.act("anvil_patch_close")
    pair.expect_diff("custom anvil: a close button patched into the result slot", mark, before,
                     {2: ("barrier", 1, "Cancel", [])}, [2])
    mark = pair.client.mark()
    pair.anvil_click(2)
    close_expected(pair, "custom anvil: the close button", mark, "anvilc", "plugin", "st-anvilc:1", True)
    pair.expect_event("custom anvil: the close button's close event has the text", mark, menu="anvilc",
                      kind="close", text="Mine")
    clicks = [e for e in pair.events(mark) if e.get("kind") == "click"]
    pair.check("custom anvil: a close button without an id reports no click", not clicks, clicks)


def step_screen_packets(pair):
    """While a menu is open, the packets only a game screen sends (rename,
    trade, beacon) and those naming Rift's container (recipe, button,
    transaction acknowledgement, slot state) never reach the backend; after it
    closed a trade selection is the backend's again (the control)."""
    open_menu(pair, "chest")
    p, container = pair.protocol, pair.container
    since, events = time.monotonic(), pair.client.mark()
    pair.rename("Sneaky")
    if pair.client.has("select_trade"):
        pair.client.send_packet("select_trade", {"slot": 0})
        effects = {"primary_effect": 1, "secondary_effect": 1}
        if p >= 759:
            effects = {"primary_effect": 1, "secondary_effect": None}
        pair.client.send_packet("set_beacon_effect", effects)
    else:
        pair.client.send_packet("custom_payload", {"channel": "MC|TrSel", "data": (0).to_bytes(4, "big")})
        pair.client.send_packet("custom_payload", {"channel": "MC|Beacon",
                                                   "data": (1).to_bytes(4, "big") + (1).to_bytes(4, "big")})
    recipe = 1 if p < FLATTENING or p >= WIDE else "minecraft:stick"
    fields = {"windowId": container, "makeAll": False}
    fields["recipeId" if p >= WIDE else "recipe"] = recipe
    pair.client.send_packet("craft_recipe_request", fields)
    pair.client.send_packet("enchant_item", {"windowId": container, "enchantment": 0})
    if p < TRANSACTIONS_END:
        pair.client.send_packet("transaction", {"windowId": container, "action": 3, "accepted": True})
    if pair.client.has("set_slot_state"):
        pair.client.send_packet("set_slot_state", {"slot_id": 0, "window_id": container, "state": True})
    time.sleep(1.0)
    pair.client.check()
    leaks = pair.screen_leaks(since) + pair.rift_container_leaks(since)
    pair.check("screen packets and packets naming Rift's container do not reach the backend", not leaks, leaks[:4])
    pair.expect_no_event("screen packets: no event", events, wait=0.1)
    pair.close_by_player("the chest after the screen packets", "st-chest:1")
    time.sleep(0.3)
    control = time.monotonic()
    if pair.client.has("select_trade"):
        pair.client.send_packet("select_trade", {"slot": 0})
    else:
        pair.client.send_packet("custom_payload", {"channel": "MC|TrSel", "data": (0).to_bytes(4, "big")})
    time.sleep(0.8)
    pair.check("control: without a menu a trade selection reaches the backend (the tap sees it)",
               pair.screen_leaks(control), "the backend received nothing")


def step_plugin_stop(pair):
    """A plugin that stops while its menu is open (an operator restart, then a
    reload that replaces its files): Rift closes the menu on the client, the
    stopping plugin gets no close event, and the new instance gets no event of
    the old menu; the new instance's own menus work."""
    rift, name = pair.rift, pair.client.name
    spec = ("chest", 1, "Other plugin", {4: ("gold_ingot", 1, "Other", [])})

    def other_events(mark):
        return [chat_text(r)[chat_text(r).find("OTHEREV"):] for r in pair.client.packets(CHAT, mark)
                if "OTHEREV" in chat_text(r) and f"who={name}" in chat_text(r)]

    def show_other(label):
        mark = pair.client.mark()
        status, body = rift.post(f"show {name}", path="/ext/other")
        pair.check(f"{label}: the other plugin queued its menu", status == 200 and "queued=true" in body,
                   f"HTTP {status} {body[-200:]}")
        if not pair.expect_open("otherm", mark, spec=spec, label=label, count=False):
            raise Skip(f"{label}: the other plugin's menu did not open")

    def wait_running(label):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            status, body = rift.request("GET", "/api/plugins/othertest")
            if status == 200 and '"status":"running"' in body.replace(" ", ""):
                return True
            time.sleep(0.2)
        pair.check(f"{label}: the plugin runs again", False, f"HTTP {status} {body[-300:]}")
        return False

    for how, generation in (("restart", "1"), ("reload", "2")):
        show_other(f"{how}: the other plugin's menu")
        mark = pair.client.mark()
        pair.real_click("left", 4)
        deadline = time.monotonic() + 6
        while not any("kind=click" in e for e in other_events(mark)) and time.monotonic() < deadline:
            time.sleep(0.1)
        pair.check(f"{how}: the running plugin gets the click of its menu",
                   any("kind=click" in e and "menu=otherm" in e for e in other_events(mark)), other_events(mark))
        old = pair.container
        mark = pair.client.mark()
        if how == "restart":
            status, body = rift.request("POST", "/api/plugins/othertest/restart", "{}")
            pair.check("restart: POST /api/plugins/othertest/restart is accepted", status == 202,
                       f"HTTP {status} {body[-300:]}")
        else:
            folder = rift.directory / "plugins" / "othertest"
            (folder / "init.lua").write_text(OTHER_LUA.replace("@GEN@", generation))
            status, body = rift.request("POST", "/api/reload", "{}")
            pair.check("reload: POST /api/reload with a changed plugin file is accepted", status in (200, 202),
                       f"HTTP {status} {body[-300:]}")
        try:
            pair.client.wait_packet("close_window", "Rift's container_close", 15, mark,
                                    where=lambda v: v.get("windowId") == old)
            pair.check(f"{how}: Rift closes the stopped plugin's menu on the client", True)
        except SessionError as error:
            pair.check(f"{how}: Rift closes the stopped plugin's menu on the client", False, error)
        wait_running(how)
        # Late packets for the old menu: a click and a close.
        pair.real_click("left", 4)
        pair.client.send_packet("close_window", {"windowId": old})
        time.sleep(1.5)
        pair.client.check()
        events = other_events(mark)
        pair.check(f"{how}: neither instance gets an event of the old menu (no close event either)", not events,
                   events)
        leaks = pair.rift_container_leaks(since=0.0)
        pair.check(f"{how}: the late packets for the old menu do not reach the backend", not leaks, leaks[:3])
        # The new instance's own menu works.
        show_other(f"{how}: the new instance's menu")
        mark = pair.client.mark()
        pair.real_click("left", 4)
        pair.client.send_packet("close_window", {"windowId": pair.container})
        deadline = time.monotonic() + 6
        while len(other_events(mark)) < 2 and time.monotonic() < deadline:
            time.sleep(0.1)
        events = other_events(mark)
        wanted_gen = "1" if how == "restart" else generation
        pair.check(f"{how}: the new instance gets the click and the close of its own menu",
                   any("kind=click" in e for e in events) and any("kind=close" in e and "reason=player" in e
                                                                  for e in events)
                   and all(f"gen={wanted_gen}" in e for e in events), events)
        pair.container = pair.menu = None


STEPS = [step_clicks, step_backend, step_endings, step_real_click, step_same_id, step_redraw, step_update_menu,
         step_pages, step_stale_click, step_redraw_burst, step_backend_screen, step_creative, step_screen_packets,
         step_anvil, step_plugin_stop, step_switch, step_disconnect, step_one_close]
OPTIONAL = set()
ALWAYS = {"step_one_close"}  # Runs with --only too.


# --- Servers and pairs ------------------------------------------------------------------

def parse_pair(text):
    server, _, client = text.partition(":")
    key_name = fixtures.resolve(server)
    protocol = fixtures.SERVERS[key_name]["protocol"] if not client else fixtures.PROTOCOL_OF.get(client)
    if protocol is None:
        raise SystemExit(f"unknown client version {client!r}")
    return key_name, protocol


def seed_downloads(plan):
    """Server jars the other harnesses already downloaded (target/minecraft/
    downloads, CI's cache) are copied into the shared cache instead of being
    downloaded again; matrix_fixtures verifies the checksum of every copy."""
    keys = set(plan)
    for protocols in plan.values():
        for protocol in protocols:
            try:
                keys.add(fixtures.vanilla_for(protocol))  # Registry names come from its data generator.
            except (KeyError, SystemExit):
                pass
    for key_name in keys:
        fixture = fixtures.SERVERS.get(key_name, {})
        if "filename" not in fixture or fixture.get("build") == "buildtools":
            continue
        source = HARNESS_CACHE / "downloads" / fixture["filename"]
        target = env.HOME / "servers" / fixture["filename"]
        if source.is_file() and not target.exists():
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
            env.log(f"using {source} for {key_name}")


def run_server(key_name, protocols, args, session_server):
    stamp = time.strftime("%Y%m%d-%H%M%S")
    workdir = Path(tempfile.mkdtemp(prefix=f"menus-{key_name}-", dir=args.work))
    server = fixtures.Server(key_name, workdir / "main")
    second = None if args.no_switch else fixtures.Server(key_name, workdir / "second")
    rift = tap = None
    results = []
    try:
        server.start()
        env.log(f"{server.name} (protocol {server.protocol}) ready in {server.started}s on port {server.port}")
        if second is not None:
            second.start()
            env.log(f"second {second.name} ready on port {second.port}")
        tap = BackendTap(server.port)
        rift = Rift(args.binary, workdir, args.mode, server, second, session_server.url, tap).start()
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
                if pair.observer is not None:
                    pair.observer.write_log(directory / "observer.jsonl")
                time.sleep(0.5)
                (directory / "rift.log").write_text(rift.text()[pair.rift_offset:])
                (directory / "server.log").write_text(server.text()[pair.server_offset:])
                if second is not None:
                    (directory / "second.log").write_text(second.text()[pair.second_offset:])
                if pair.client is not None:
                    (directory / "backend.jsonl").write_text("".join(
                        json.dumps(dict(t=round(when, 3), name=name, value=value), default=str) + "\n"
                        for when, name, value in tap.received(pair.client.name)))
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
        if tap is not None:
            tap.stop()
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
    parser.add_argument("--only", default="", help="run only these steps (and one_close): "
                        + ",".join(step.__name__.removeprefix("step_") for step in STEPS))
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
    args.only = {f"step_{name.strip()}" for name in args.only.split(",") if name.strip()}
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
    seed_downloads(plan)
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
