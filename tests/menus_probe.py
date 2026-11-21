#!/usr/bin/env python3
"""Ask real servers to send a player's inventory again, without changing it.

Inventory menus (docs/menus-internals.md, "The inventory request") rely on
one request: after the player disturbed their own inventory rows on the
client while a Rift menu was open, Rift sends the backend a click that does nothing, and the
backend answers with the player's whole inventory. This probe starts pinned
servers from tests/servers.json, joins each one directly as a scripted client
in the server's own protocol (no Rift in between), and measures what the
server does with the requests in RECIPES:

    baseline   items given by console command; the inventory the server sent
    request    the recipe for the server's protocol, twice; the answer, in order
    cursor     a real pickup click puts a stack on the cursor; the request must
               report it and keep it; a click puts the stack back
    legacy     (<= 1.16.5) a claimed empty item, the rejection, clicks before
               and after the acknowledgement, a second acknowledgement
    claim      the "claim every slot" request on every protocol from 1.17
    rate       20 requests in one second, then 5 a second for 10 seconds, sent
               blindly (<= 1.16.5: a request that reaches the server before
               the acknowledgement of the previous rejection is ignored)
    final      the request again and the server's own /clear and /data view
    drop       slot -999 with a stack on the cursor, which drops it

A server PASSes when every request of its recipe made it send the whole
inventory and the cursor, and nothing in the inventory or on the cursor
changed. Each run writes result.json, the client's packet log (both
directions, decoded) and the server log under --out.

Usage (Python 3.11+; use /opt/homebrew/bin/python3 on macOS):

    tests/menus_probe.py --accept-eula --server vanilla-1.16.5
    tests/menus_probe.py --accept-eula --server paper-1.21.11 --server vanilla-1.17
    tests/menus_probe.py --accept-eula --all --jobs 2
    tests/menus_probe.py --list

Servers, JDKs and caches are the translation matrix's (tests/matrix_env.py,
tests/matrix_fixtures.py): ~/.cache/rift-via, RIFT_JAVA_<n>, and at most
RIFT_VIA_MAX_SERVERS servers machine-wide.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
import time
import traceback

sys.path.insert(0, str(Path(__file__).resolve().parent))

import matrix_env as env  # noqa: E402
import matrix_fixtures as fixtures  # noqa: E402
from matrix_client import Session, SessionError, jsonable  # noqa: E402
from matrix_scenario import Commands  # noqa: E402

ALL = ["vanilla-1.12.2", "vanilla-1.16.5", "vanilla-1.17", "vanilla-1.17.1", "vanilla-1.20.4",
       "vanilla-1.21.4", "vanilla-1.21.5", "vanilla-1.21.11", "vanilla-26.3",
       "paper-1.12.2", "paper-1.16.5", "paper-1.17.1", "paper-1.21.11", "paper-26.3",
       "spigot-1.12.2", "spigot-1.16.5", "spigot-1.21.11", "spigot-26.3", "pumpkin"]
# One server per protocol family between those above.
MORE = ["vanilla-1.13.2", "vanilla-1.14.4", "vanilla-1.15.2", "vanilla-1.18.2", "vanilla-1.19.4",
        "vanilla-1.20.6", "vanilla-1.21.1"]
OUT = fixtures.ROOT / "target" / "minecraft" / "menus-probe"

SLOTS = 46  # Container 0 from 1.9: result, 4 crafting, 4 armor, 27 main, 9 hotbar, offhand.
STATE_IDS = 756  # 1.17.1: clicks carry a state ID.
CHANGED_SLOTS = 755  # 1.17: clicks list the slots they changed; no transactions.
HASHED = 770  # 1.21.5: clicks carry hashed stacks.


# --- Stacks in each protocol's layout ----------------------------------------

def empty(protocol):
    """An empty stack as a click names it."""
    if protocol >= HASHED:
        return None  # An absent optional hashed stack.
    if protocol >= 766:
        return {"itemCount": 0}
    if protocol >= 404:
        return {"present": False}
    if protocol >= 393:
        return {"itemId": -1}
    return {"blockId": -1}


def stack(protocol, item, count, damage=0):
    """A stack without NBT or components; every era's field names at once."""
    if protocol >= HASHED:
        return {"itemId": item, "itemCount": count, "components": [], "removeComponents": []}
    return {"present": True, "blockId": item, "itemId": item, "itemCount": count, "itemDamage": damage}


def from_record(protocol, value):
    """The stack a decoded slot holds, re-encodable as a click's stack."""
    if key(value) is None:
        return empty(protocol)
    item = value.get("itemId", value.get("blockId"))
    return stack(protocol, item, value["itemCount"], value.get("itemDamage", 0))


# A stack no slot holds: 99 stone (item 1 in every protocol; no stack of
# stone is larger than 64).
def garbage(protocol):
    return stack(protocol, 1, 99)


# --- The requests ---------------------------------------------------------------
#
# Everything a backend is sent for "send me the inventory again" is here. The
# Rust implementation ports these field by field.

WRONG_STATE_ID = -1  # Never a real state ID: servers count them 0..32767.
REQUEST_ACTION = -1  # The action number of the <= 1.16.5 request (any short).


def click(session, slot, button=0, mode=0, state=None, changed=(), cursor=None, action=0, item=None):
    """A container_click on container 0 in the session's protocol."""
    protocol = session.protocol
    value = {"windowId": 0, "slot": slot, "mouseButton": button, "mode": mode}
    if protocol >= CHANGED_SLOTS:
        value["changedSlots"] = [{"location": location, "item": entry} for location, entry in changed]
        value["cursorItem"] = empty(protocol) if cursor is None else cursor
        if protocol >= STATE_IDS:
            value["stateId"] = state if state is not None else 0
    else:
        value["action"] = action
        value["item"] = empty(protocol) if item is None else item
    session.send_packet("window_click", value)


def request_state_id(session):
    """1.17.1 and newer: a left click outside the slots (slot -1), with a state
    ID the server does not expect, no changed slots and an empty cursor. The
    server answers a wrong state ID with its whole container."""
    click(session, -1, state=WRONG_STATE_ID)


def request_transaction(session, action=REQUEST_ACTION, claimed=None):
    """1.16.5 and older: a left click outside the slots, claiming that it
    picked up a stack. The click computes nothing, so the server rejects it
    and sends the whole container; it then ignores this player's clicks until
    the client acknowledges `action` (acknowledge())."""
    click(session, -1, action=action, item=garbage(session.protocol) if claimed is None else claimed)


def acknowledge(session, action=REQUEST_ACTION):
    """What a client answers to a rejected transaction (<= 1.16.5)."""
    session.send_packet("transaction", {"windowId": 0, "action": action, "accepted": True})


def request_claim_all(session, state=None):
    """1.17 (and accepted by every newer version): a left click outside the
    slots that claims every slot of container 0 and the cursor hold a stack
    they cannot hold. The server records the claim as what the client shows
    and sends every slot that differs: all of them, one container_set_slot
    each, and the cursor."""
    protocol = session.protocol
    click(session, -1, state=state, changed=[(slot, garbage(protocol)) for slot in range(SLOTS)],
          cursor=garbage(protocol))


def request(session, recipe):
    {"state_id": request_state_id, "transaction": request_transaction,
     "claim_all": request_claim_all}[recipe](session)


def recipe_for(protocol):
    """The request Rift should send a backend of this protocol."""
    if protocol >= STATE_IDS:
        return "state_id"
    if protocol >= CHANGED_SLOTS:
        return "claim_all"
    return "transaction"


# --- What the client knows about container 0 ---------------------------------------

INVENTORY = {"window_items", "set_slot", "set_cursor_item", "set_player_inventory", "transaction"}
NOISE = {"keep_alive", "update_time", "entity_velocity", "rel_entity_move", "entity_move_look",
         "entity_look", "entity_head_rotation", "entity_teleport", "sync_entity_position",
         "entity_metadata", "update_attributes", "map_chunk", "unload_chunk", "chunk_batch_start",
         "chunk_batch_finished", "update_light", "bundle_delimiter", "player_info",
         "entity_update_attributes", "ping", "ticking_state", "ticking_step", "light_update",
         "update_view_position", "experience", "update_health", "system_chat", "chat", "player_chat",
         "move_minecart", "block_change", "multi_block_change", "sound_effect", "entity_sound_effect"}


def key(value):
    """A comparable form of a decoded stack; None for an empty one."""
    if not isinstance(value, dict) or value.get("present") is False:
        return None
    item = value.get("itemId", value.get("blockId"))
    if item is None or item == -1 or not value.get("itemCount"):
        return None
    return json.dumps(jsonable({k: v for k, v in value.items() if k != "present"}), sort_keys=True)


def short(value):
    """A stack for humans: item ID x count, + if it has NBT or components."""
    if key(value) is None:
        return "-"
    item = value.get("itemId", value.get("blockId"))
    extra = "+" if value.get("nbtData") or value.get("components") else ""
    damage = f":{value['itemDamage']}" if value.get("itemDamage") else ""
    return f"{item}{damage}x{value['itemCount']}{extra}"


def menu_slot(index):
    """The container 0 slot of an inventory index (set_player_inventory, window -2)."""
    if index < 9:
        return 36 + index
    if index < 36:
        return index
    if index < 40:
        return 8 - (index - 36)
    return 45


class Inventory:
    """Container 0 as the server last described it to the client."""

    def __init__(self):
        self.slots = [None] * SLOTS
        self.carried = None
        self.state = None

    def apply(self, name, value):
        window = value.get("windowId")
        if name == "window_items" and window == 0:
            items = value["items"]
            self.slots = [key(item) for item in items[:SLOTS]] + [None] * (SLOTS - len(items))
            if "carriedItem" in value:
                self.carried = key(value["carriedItem"])
            self.state = value.get("stateId", self.state)
        elif name == "set_slot":
            if window == 0 and 0 <= value["slot"] < SLOTS:
                self.slots[value["slot"]] = key(value["item"])
                self.state = value.get("stateId", self.state)
            elif window == -1 and value["slot"] == -1:
                self.carried = key(value["item"])
            elif window == -2 and 0 <= value["slot"] <= 40:
                self.slots[menu_slot(value["slot"])] = key(value["item"])
        elif name == "set_cursor_item":
            self.carried = key(value["contents"])
        elif name == "set_player_inventory" and 0 <= value["slotId"] <= 40:
            self.slots[menu_slot(value["slotId"])] = key(value["contents"])

    def snapshot(self):
        return {"slots": list(self.slots), "carried": self.carried}

    def filled(self):
        return {index: json.loads(entry) for index, entry in enumerate(self.slots) if entry}


def inventory(session, until=None):
    model = Inventory()
    with session.lock:
        records = session.records[:until]
    for record in records:
        if record.name in INVENTORY and record.value is not None:
            model.apply(record.name, record.value)
    return model


def describe(record):
    """One received packet with the fields that matter here."""
    value = record.value or {}
    name = record.name
    if name == "window_items":
        filled = sum(1 for item in value.get("items", []) if key(item))
        text = f"window_items(window={value.get('windowId')}, state={value.get('stateId')}, " \
               f"{len(value.get('items', []))} slots, {filled} filled"
        if "carriedItem" in value:
            text += f", carried={short(value['carriedItem'])}"
        return text + ")"
    if name == "set_slot":
        state = f", state={value['stateId']}" if "stateId" in value else ""
        return f"set_slot(window={value.get('windowId')}{state}, slot={value.get('slot')}, " \
               f"{short(value.get('item'))})"
    if name == "transaction":
        return f"transaction(window={value.get('windowId')}, action={value.get('action')}, " \
               f"accepted={value.get('accepted')})"
    if name == "set_cursor_item":
        return f"set_cursor_item({short(value.get('contents'))})"
    if name == "set_player_inventory":
        return f"set_player_inventory(slot={value.get('slotId')}, {short(value.get('contents'))})"
    if name == "kick_disconnect":
        return f"kick_disconnect({Session.reason(value.get('reason'))})"
    return name or f"?{record.id:#04x}"


def condense(lines):
    """Runs of equal packet kinds folded: 46 set_slot(window=0, ...) -> one line."""
    out = []
    for line in lines:
        kind = line
        if line.startswith("set_slot(window=0"):
            kind = re.sub(r"slot=\d+, [^)]*", "slot=*", re.sub(r"state=\d+", "state=*", line))
        if out and out[-1][0] == kind:
            out[-1][1] += 1
        else:
            out.append([kind, 1])
    return [kind if count == 1 else f"{count} x {kind}" for kind, count in out]


class Probe(Session):
    """A session whose acknowledgement of rejected transactions can be held back."""

    auto_ack = True

    def on_play(self, record):
        if record.name == "transaction" and record.value is not None and not self.auto_ack:
            return
        super().on_play(record)


# --- One server -----------------------------------------------------------------------

class Run:
    def __init__(self, server, directory):
        self.server = server
        self.directory = directory
        self.protocol = server.protocol
        self.software = server.software
        self.cmd = Commands(server.protocol, server.software)
        self.session = None
        self.kind = "recipe"  # Checks of the recipe decide PASS; "experiment" checks are reported.
        self.result = dict(server=server.name, version=server.fixture["version"], protocol=self.protocol,
                           recipe=recipe_for(self.protocol), status="FAIL", checks=[], phases={},
                           log=[], notes=[])

    # Plumbing

    def note(self, text):
        self.result["notes"].append(text)
        env.log(f"[{self.server.name}] {text}")

    def check(self, name, ok, detail=""):
        self.result["checks"].append(dict(name=name, ok=bool(ok), detail=detail, kind=self.kind))
        env.log(f"[{self.server.name}] {'ok  ' if ok else 'FAIL'} {name}{': ' + detail if detail else ''}")
        return bool(ok)

    def console(self, line, pause=0.0):
        self.server.console(line)
        if pause:
            time.sleep(pause)

    def collect(self, mark, quiet=0.6, limit=4.0, minimum=0.3):
        """Packets since `mark`, once no inventory packet came for `quiet` seconds."""
        begun = time.monotonic()
        while True:
            time.sleep(0.05)
            self.session.check()
            now = time.monotonic()
            records = self.session.packets(since=mark)
            relevant = [r for r in records if r.name in INVENTORY or r.name == "kick_disconnect"]
            last = relevant[-1].time if relevant else begun
            if now - begun >= minimum and now - last >= quiet or now - begun >= limit:
                return records

    def phase(self, name, mark, log_offset, records=None, sent=None):
        """Record what the server sent (and logged) since `mark`."""
        records = self.session.packets(since=mark) if records is None else records
        lines = [describe(r) for r in records if r.name not in NOISE]
        log = self.log_lines(log_offset)
        entry = dict(received=condense(lines), log=log)
        if sent:
            entry["sent"] = sent
        self.result["phases"][name] = entry
        return entry

    def log_lines(self, offset):
        text = self.server.text()[offset:]
        pattern = re.compile(r"WARN|ERROR|Exception|kick|lost connection|disconnect|invalid|click|"
                             r"desync|moved|too (many|fast|quickly)", re.I)
        noise = re.compile(r"Can't keep up|Running \d+ms|logged in with entity id|joined the game")
        return [line.strip() for line in text.splitlines() if pattern.search(line) and not noise.search(line)][-12:]

    def answer(self, name, recipe, expected, repeat=1):
        """Send the request; check the answer covers the inventory and matches
        `expected` (a snapshot); return the received packets."""
        ok = True
        for attempt in range(repeat):
            label = f"{name}#{attempt + 1}" if repeat > 1 else name
            mark, offset = self.session.mark(), self.server.offset()
            request(self.session, recipe)
            records = self.collect(mark)
            self.phase(label, mark, offset)
            ok &= self.judge(label, recipe, records, mark, expected)
        return ok

    def judge(self, label, recipe, records, mark, expected):
        after = inventory(self.session).snapshot()
        names = [r for r in records if r.value is not None]
        full = [r for r in names if r.name == "window_items" and r.value.get("windowId") == 0]
        slots = {r.value["slot"] for r in names if r.name == "set_slot" and r.value.get("windowId") == 0}
        cursor = [r for r in names if (r.name == "set_slot" and r.value.get("windowId") == -1)
                  or r.name == "set_cursor_item"
                  or (r.name == "window_items" and r.value.get("windowId") == 0 and "carriedItem" in r.value)]
        kicked = [r for r in names if r.name == "kick_disconnect"]
        if recipe == "claim_all":
            covered = len(slots) == SLOTS or bool(full)
        else:
            covered = bool(full)
        detail = f"{len(full)} window_items(0), {len(slots)} distinct set_slot(0), {len(cursor)} cursor"
        good = self.check(f"{label}: the whole inventory and the cursor came back",
                          bool(covered and cursor and not kicked), detail)
        same = after == expected
        diff = [i for i in range(SLOTS) if after["slots"][i] != expected["slots"][i]]
        cdiff = after["carried"] != expected["carried"]
        good &= self.check(f"{label}: inventory and cursor unchanged", same,
                           "" if same else f"slots {diff} differ, cursor {'differs' if cdiff else 'same'}")
        return good

    def count(self, item):
        """How many of `item` the server says the player has (/clear with 0)."""
        if self.software == "pumpkin":
            return None  # Pumpkin answers, but its piped console output arrives only at exit.
        offset = self.server.offset()
        pattern = self.cmd.clear_count_pattern(self.session.name, item)
        self.console(self.cmd.clear_count(self.session.name, item))
        line = self.server.wait_log(f"{pattern}|No items were found|Could not clear|No items", 6, offset)
        if line is None:
            return None
        match = re.search(pattern, line)
        return int(match.group(1)) if match else 0

    def data(self):
        """The server's Inventory NBT for the player (1.13+), as logged."""
        if self.protocol < 393 or self.software == "pumpkin":
            return None
        offset = self.server.offset()
        self.console(f"data get entity {self.session.name} Inventory")
        line = self.server.wait_log(r"has the following entity data|No entity was found|Found no elements",
                                    6, offset)
        return line.split("]: ", 1)[-1] if line else None

    def server_view(self):
        return dict(diamond=self.count("diamond"), iron_ingot=self.count("iron_ingot"), data=self.data())

    # Real clicks

    def pick_up(self, slot, held):
        """A correct left click on `slot` holding the decoded stack `held`: the
        stack moves to the cursor."""
        p = self.protocol
        model = inventory(self.session)
        if p >= CHANGED_SLOTS:
            click(self.session, slot, state=model.state, changed=[(slot, empty(p))], cursor=from_record(p, held))
        else:
            click(self.session, slot, action=self.next_action(), item=from_record(p, held))

    def put_down(self, slot, held):
        """A correct left click on the empty `slot` with `held` on the cursor."""
        p = self.protocol
        model = inventory(self.session)
        if p >= CHANGED_SLOTS:
            click(self.session, slot, state=model.state, changed=[(slot, from_record(p, held))])
        else:
            click(self.session, slot, action=self.next_action(), item=empty(p))

    action = 0

    def next_action(self):
        self.action += 1
        return self.action

    # The run

    def run(self):
        pumpkin = self.software == "pumpkin"
        name = f"Probe{os.getpid() % 10000}"
        self.session = Probe(self.server.port, name, self.protocol, label=name, pumpkin=pumpkin)
        self.session.join()
        time.sleep(1.0)
        self.console(self.cmd.gamemode(name, "survival"), 0.5)
        self.console(f"clear {name}", 0.5)
        self.baseline()
        if self.result.get("abort"):
            return
        recipe = recipe_for(self.protocol)
        self.request_phase(recipe)
        self.cursor_phase(recipe)
        self.kind = "experiment"
        if self.protocol < CHANGED_SLOTS:
            self.legacy_phase()
        if self.protocol >= CHANGED_SLOTS:
            self.claim_phase()
        if self.protocol >= STATE_IDS:
            self.stale_phase()
        self.rate_phase(recipe)
        self.kind = "recipe"
        self.final_phase(recipe)
        self.kind = "experiment"
        self.drop_phase(recipe)

    def baseline(self):
        session, name = self.session, self.session.name
        mark, offset = session.mark(), self.server.offset()
        self.console(f"give {name} minecraft:diamond 5")
        try:
            record = session.wait(lambda r: r.value is not None and (
                (r.name == "set_slot" and r.value.get("windowId") in (0, -2) and key(r.value.get("item")))
                or (r.name == "set_player_inventory" and key(r.value.get("contents")))
                or (r.name == "window_items" and r.value.get("windowId") == 0
                    and any(key(item) for item in r.value["items"]))), "the given diamonds", 10, mark)
        except SessionError as error:
            self.check("baseline: /give produced an inventory update", False, str(error))
            self.result["abort"] = True
            return
        time.sleep(0.5)
        self.console(f"give {name} minecraft:iron_ingot 3", 1.0)
        model = inventory(session)
        filled = model.filled()
        self.phase("baseline", mark, offset)
        self.result["baseline"] = {str(i): short(v) for i, v in filled.items()}
        self.result["baseline_state_id"] = model.state
        self.check("baseline: two stacks given", len(filled) == 2, json.dumps(self.result["baseline"]))
        del record
        diamond = min(filled)  # The first stack given lands in the first hotbar slot.
        self.diamond_slot, self.diamond = diamond, filled[diamond]
        self.expected = model.snapshot()
        self.result["server_view_before"] = self.server_view()

    def request_phase(self, recipe):
        self.answer("request", recipe, self.expected, repeat=2)

    def cursor_phase(self, recipe):
        session, slot = self.session, self.diamond_slot
        mark, offset = session.mark(), self.server.offset()
        self.pick_up(slot, self.diamond)
        self.collect(mark)
        self.phase("cursor: real pickup click", mark, offset)
        held = dict(self.expected)
        held["slots"] = list(self.expected["slots"])
        held["slots"][slot] = None
        held["carried"] = key(self.diamond)
        self.answer("cursor: request with a stack on the cursor", recipe, held, repeat=2)
        mark, offset = session.mark(), self.server.offset()
        self.put_down(slot, self.diamond)
        self.collect(mark)
        self.phase("cursor: real click puts the stack back", mark, offset)
        self.answer("cursor: request after putting it back", recipe, self.expected)

    def legacy_phase(self):
        """<= 1.16.5: accepted and rejected transactions, the acknowledgement."""
        session, p = self.session, self.protocol
        # A claimed empty item matches what slot -1 computes: accepted, nothing sent.
        mark, offset = session.mark(), self.server.offset()
        request_transaction(session, action=-2, claimed=empty(p))
        records = self.collect(mark)
        entry = self.phase("legacy: slot -1 claiming an empty item", mark, offset, records)
        accepted = [r for r in records if r.name == "transaction" and r.value and r.value["accepted"]]
        full = [r for r in records if r.name == "window_items"]
        self.check("legacy: a claimed empty item is accepted and resends nothing",
                   accepted and not full, "; ".join(entry["received"]))
        # Rejection without an acknowledgement.
        session.auto_ack = False
        try:
            mark, offset = session.mark(), self.server.offset()
            request_transaction(session, action=-3)
            records = self.collect(mark)
            entry = self.phase("legacy: request, not acknowledged", mark, offset, records)
            rejected = [r for r in records if r.name == "transaction" and r.value and not r.value["accepted"]]
            self.check("legacy: the request is rejected (accepted=false, action echoed) and answered",
                       rejected and rejected[0].value["action"] == -3
                       and any(r.name == "window_items" for r in records), "; ".join(entry["received"]))
            mark, offset = session.mark(), self.server.offset()
            request_transaction(session, action=-4)
            records = self.collect(mark, minimum=1.0)
            entry = self.phase("legacy: second request before the acknowledgement", mark, offset, records)
            self.result["legacy_second_request_answered"] = any(r.name in INVENTORY for r in records)
            self.check("legacy: a request before the acknowledgement is ignored",
                       not any(r.name in INVENTORY for r in records), "; ".join(entry["received"]) or "nothing")
            # A real click before the acknowledgement: ignored, the stack stays.
            mark, offset = session.mark(), self.server.offset()
            self.pick_up(self.diamond_slot, self.diamond)
            records = self.collect(mark, minimum=1.0)
            self.phase("legacy: real pickup click before the acknowledgement", mark, offset, records)
            # Acknowledge the wrong action first: the server must stay unsynced.
            mark, offset = session.mark(), self.server.offset()
            acknowledge(session, action=-4)
            request_transaction(session, action=-5)
            records = self.collect(mark, minimum=1.0)
            entry = self.phase("legacy: acknowledging another action, then a request", mark, offset, records)
            self.result["legacy_wrong_ack_unlocks"] = any(r.name == "window_items" for r in records)
            # The right acknowledgement, then the request again.
            mark, offset = session.mark(), self.server.offset()
            acknowledge(session, action=-3)
            records = self.collect(mark, minimum=0.8)
            entry = self.phase("legacy: the acknowledgement", mark, offset, records)
            self.check("legacy: the acknowledgement itself is answered with nothing",
                       not any(r.name in INVENTORY for r in records), "; ".join(entry["received"]) or "nothing")
        finally:
            session.auto_ack = True
        # Was the pickup click before the acknowledgement really ignored?
        self.answer("legacy: request after the acknowledgement", "transaction", self.expected)
        # A duplicated acknowledgement (client and Rift both answering) is harmless.
        mark, offset = session.mark(), self.server.offset()
        acknowledge(session, action=REQUEST_ACTION)
        acknowledge(session, action=REQUEST_ACTION)
        records = self.collect(mark, minimum=0.8)
        entry = self.phase("legacy: two extra acknowledgements", mark, offset, records)
        self.check("legacy: extra acknowledgements are ignored",
                   not any(r.name in INVENTORY or r.name == "kick_disconnect" for r in records),
                   "; ".join(entry["received"]) or "nothing")
        self.answer("legacy: request after extra acknowledgements", "transaction", self.expected)

    def claim_phase(self):
        """The claim-every-slot request, with a matching and a wrong state ID."""
        variants = [("claim: every slot claimed", None)]
        if self.protocol >= STATE_IDS:
            variants = [("claim: every slot claimed, current state ID", "current"),
                        ("claim: every slot claimed, wrong state ID", WRONG_STATE_ID)]
        for label, state in variants:
            mark, offset = self.session.mark(), self.server.offset()
            value = inventory(self.session).state if state == "current" else state
            request_claim_all(self.session, state=value)
            records = self.collect(mark)
            self.phase(label, mark, offset, records)
            self.judge(label, "claim_all", records, mark, self.expected)

    def stale_phase(self):
        """A state ID one behind (a client that missed one update) also counts as wrong."""
        mark, offset = self.session.mark(), self.server.offset()
        state = inventory(self.session).state
        click(self.session, -1, state=((state or 0) - 1) & 0x7FFF)
        records = self.collect(mark)
        self.phase("state ID one behind", mark, offset, records)
        self.judge("state ID one behind", "state_id", records, mark, self.expected)
        # And the same click with the current state ID: the server sees no difference.
        mark, offset = self.session.mark(), self.server.offset()
        click(self.session, -1, state=inventory(self.session).state)
        records = self.collect(mark, minimum=0.8)
        entry = self.phase("current state ID (expected: nothing sent)", mark, offset, records)
        self.result["current_state_id_answer"] = entry["received"]

    def rate_phase(self, recipe):
        session = self.session
        for label, count, interval in (("rate: 20 requests in 1 s", 20, 0.05),
                                       ("rate: 5 per second for 10 s", 50, 0.2)):
            mark, offset = session.mark(), self.server.offset()
            begun = time.monotonic()
            for index in range(count):
                request(session, recipe)
                delay = begun + (index + 1) * interval - time.monotonic()
                if delay > 0:
                    time.sleep(delay)
            records = self.collect(mark, quiet=1.0, limit=6.0)
            valid = [r for r in records if r.value is not None]
            full = sum(1 for r in valid if r.name == "window_items" and r.value.get("windowId") == 0)
            slot_updates = sum(1 for r in valid if r.name == "set_slot" and r.value.get("windowId") == 0)
            rejections = sum(1 for r in valid if r.name == "transaction" and not r.value["accepted"])
            kicked = [describe(r) for r in valid if r.name == "kick_disconnect"]
            answers = full if recipe != "claim_all" else slot_updates // SLOTS
            log = self.log_lines(offset)
            self.result["phases"][label] = dict(sent=count, window_items=full, set_slot=slot_updates,
                                                rejections=rejections, kicked=kicked, log=log)
            self.check(f"{label}: every request answered, no kick", answers == count and not kicked,
                       f"{answers}/{count} answered ({full} window_items, {slot_updates} set_slot, "
                       f"{rejections} rejections){'; kicked' if kicked else ''}"
                       f"{'; log: ' + ' | '.join(log[-3:]) if log else ''}")
            session.check()
            self.judge(f"{label}: after", recipe, records, mark, self.expected)

    def final_phase(self, recipe):
        self.answer("final request", recipe, self.expected)
        view = self.server_view()
        self.result["server_view_after"] = view
        before = self.result["server_view_before"]
        self.check("final: the server's own view (/clear count, /data get) is unchanged", view == before,
                   f"before {before}, after {view}" if view != before else
                   ("not available on this server" if all(v is None for v in view.values()) else ""))

    def drop_phase(self, recipe):
        """Slot -999 with a stack on the cursor drops it: not what we want."""
        session, slot, p = self.session, self.diamond_slot, self.protocol
        mark, offset = session.mark(), self.server.offset()
        self.pick_up(slot, self.diamond)
        time.sleep(0.5)
        if p >= CHANGED_SLOTS:
            click(session, -999, state=inventory(session).state)
        else:
            click(session, -999, action=self.next_action(), item=empty(p))
        self.collect(mark, minimum=1.0)
        request(session, recipe)
        records = self.collect(mark)
        self.phase("drop: slot -999 with a stack on the cursor", mark, offset, records)
        after = inventory(session).snapshot()
        spawned = any(r.name == "spawn_entity" for r in records)
        dropped = after["carried"] is None and after["slots"][slot] is None
        self.result["drop_999"] = dict(dropped=dropped, item_entity_spawned=spawned)
        self.check("drop: slot -999 drops the cursor stack (so -1 is the one to use)", dropped,
                   f"cursor {'empty' if after['carried'] is None else 'kept'}, "
                   f"item entity spawned: {spawned}")


def run_server(name, args):
    key_name = fixtures.resolve(name)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    directory = args.out / f"{stamp}-{key_name}"
    directory.mkdir(parents=True, exist_ok=True)
    workdir = Path(tempfile.mkdtemp(prefix=f"rift-menus-{key_name}-", dir=args.work))
    server = fixtures.Server(key_name, workdir / "server")
    probe = Run(server, directory)
    try:
        server.start()
        env.log(f"{server.name} (protocol {server.protocol}) ready in {server.started}s on port {server.port}")
        probe.run()
    except Exception as error:  # Recorded; the next server still runs.
        probe.kind = "recipe"
        probe.check("the probe ran to the end", False, f"{type(error).__name__}: {error}")
        probe.result["traceback"] = traceback.format_exc()[-3000:]
    finally:
        if probe.session is not None:
            probe.session.close()
            probe.session.write_log(directory / "client.jsonl")
        time.sleep(0.3)
        (directory / "server.log").write_text(server.text())
        server.stop()
        if args.keep:
            env.log(f"kept {workdir}")
        else:
            shutil.rmtree(workdir, ignore_errors=True)
    result = probe.result
    recipe_checks = [c for c in result["checks"] if c["kind"] == "recipe"]
    result["status"] = "PASS" if recipe_checks and all(c["ok"] for c in recipe_checks) else "FAIL"
    result["experiments_failed"] = [c["name"] for c in result["checks"] if not c["ok"] and c["kind"] != "recipe"]
    (directory / "result.json").write_text(json.dumps(result, indent=2, default=str) + "\n")
    result["directory"] = str(directory)
    return result


def summary(result):
    failed = [c["name"] for c in result["checks"] if not c["ok"]]
    line = (f"{result['status']:<4} {result['server']:<20} protocol {result['protocol']:<4} "
            f"recipe {result['recipe']:<11}")
    if result["status"] == "PASS":
        line += " the request made the server re-send the inventory and changed nothing"
    else:
        line += " " + "; ".join(failed[:4])
    if result["status"] == "PASS" and result["experiments_failed"]:
        line += f" (experiments that differ: {'; '.join(result['experiments_failed'])})"
    return line


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                     formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    parser.add_argument("--accept-eula", action="store_true",
                        help="accept https://aka.ms/MinecraftEULA for the local test servers")
    parser.add_argument("--server", action="append", default=[],
                        help="server fixture (repeat or comma-separate), e.g. vanilla-1.17, paper-26.3, pumpkin")
    parser.add_argument("--all", action="store_true", help="every server in ALL")
    parser.add_argument("--more", action="store_true", help="also one vanilla server per protocol family between")
    parser.add_argument("--jobs", type=int, default=1, help="servers to probe at once")
    parser.add_argument("--out", type=Path, default=OUT, help=f"results directory (default {OUT})")
    parser.add_argument("--work", type=Path, default=None, help="where server directories go (default: temp)")
    parser.add_argument("--keep", action="store_true", help="keep server directories")
    parser.add_argument("--json", type=Path, help="also write all results to this file")
    parser.add_argument("--list", action="store_true", help="list the server sets")
    args = parser.parse_args()
    if args.list:
        print("--all: " + " ".join(ALL))
        print("--more: " + " ".join(MORE))
        return
    servers = [name for entry in args.server for name in entry.split(",") if name]
    if args.all:
        servers += ALL
    if args.more:
        servers += MORE
    if not servers:
        parser.error("give --server, --all or --more")
    if not args.accept_eula:
        parser.error("--accept-eula is required to run the Minecraft servers")
    for name in servers:
        fixtures.resolve(name)
    if args.work:
        args.work.mkdir(parents=True, exist_ok=True)
    env.install_signal_handlers()
    results = []
    try:
        with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
            for result in pool.map(lambda name: run_server(name, args), servers):
                results.append(result)
                print(summary(result), flush=True)
    except KeyboardInterrupt:
        env.log("interrupted; stopping servers")
        env.PROCESSES.stop_all()
        raise SystemExit(130)
    if len(results) > 1:
        print("\n".join(summary(result) for result in results))
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(results, indent=2, default=str) + "\n")
    if any(result["status"] != "PASS" for result in results):
        raise SystemExit(1)


if __name__ == "__main__":
    os.environ.setdefault("PYTHONUNBUFFERED", "1")
    main()
