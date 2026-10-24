#!/usr/bin/env python3
"""Real-server acceptance test for protocol translation.

A pinned vanilla server runs one protocol. Two offline clients play on it at
the same time: a reference client connects directly in the server's protocol,
and a translated client connects through Rift in the other protocol. A
scripted scenario then produces every kind of packet that carries an item or
sound ID (inventories with nested data components, recipes, the recipe book,
advancements, tags, statistics, entity metadata and equipment, particles,
sounds, explosions, villager trades and ghost recipes).

Every packet both clients receive is decoded with the pinned minecraft-data
schema for that client's protocol (tests/protodef.py, independent of Rift's
layouts), with item and sound IDs replaced by registry names from each
version's own Mojang data reports. The translated session must then match the
reference packet for packet. Serverbound translation is checked by the server
itself: creative inventory items arrive under the intended names, and hashed
container clicks match without a resync.

    python3 tests/translation_minecraft.py --accept-eula
    python3 tests/translation_minecraft.py --accept-eula --mode downgrade

`upgrade` runs a 1.21.8 client on a 1.21.6 server; `downgrade` the reverse.
`network` switches a 1.21.8 client between a native 1.21.8 lobby and a
translated 1.21.6 survival server with the network acceptance checks.
Requires Java 21+ and Python 3.11+. Logs are kept under target/minecraft/runs/.
"""

import argparse
from contextlib import ExitStack
import hashlib
import io
import json
from pathlib import Path
import struct
import sys
import tempfile
import threading
import time
import uuid

from minecraft import (CACHE, ROOT, Client, configure_server, client_settings, process,
                       read_varint, status_ready, string, unused_port, varint, wait_ready)
import protodef

sys.path.insert(0, str(ROOT / "scripts"))
import translation_data  # noqa: E402

MODES = {
    # (server fixture, server version, client version through Rift)
    "upgrade": ("vanilla-1.21.6", "1.21.6", "1.21.8"),
    "downgrade": ("vanilla-1.21.8", "1.21.8", "1.21.6"),
}
FIXTURE = {"1.21.6": "vanilla-1.21.6", "1.21.8": "vanilla-1.21.8"}
PROTOCOL = {"1.21.6": 771, "1.21.8": 772}


class Registries:
    """One version's registry names and decoders."""

    def __init__(self, version):
        registries, _ = translation_data.reports(FIXTURE[version])
        self.version = version
        self.names = {key: translation_data.ordered(value) for key, value in registries.items()}
        self.ids = {key: {name: index for index, name in enumerate(names)}
                    for key, names in self.names.items()}
        self.schema = protodef.schema(version)

    def id(self, registry, name):
        return self.ids[f"minecraft:{registry}"][f"minecraft:{name}"]

    def name(self, registry, value):
        names = self.names[f"minecraft:{registry}"]
        if not isinstance(value, int) or not 0 <= value < len(names):
            return f"<invalid {registry} {value}>"
        return names[value]

    def decoder(self, state, direction):
        decoder = protodef.Decoder(self.schema, state, direction)
        item = lambda value: self.name("item", value)
        sound = lambda value: self.name("sound_event", value)

        def slot(value):
            if value.get("itemCount", 0) > 0:
                value["itemId"] = item(value["itemId"])
        decoder.hooks["Slot"] = slot
        decoder.hooks["UntrustedSlot"] = slot
        decoder.hooks["HashedSlot"] = lambda value: value.update(itemId=item(value["itemId"]))

        def sound_holder(value):
            if "soundId" in value:
                value["soundId"] = sound(value["soundId"])
        decoder.hooks["ItemSoundHolder"] = sound_holder

        def slot_display(value):
            if value["type"] == "item":
                value["data"] = item(value["data"])
        decoder.hooks["SlotDisplay"] = slot_display

        def component(value):
            data = value.get("data")
            if value["type"] == "pot_decorations":
                data["decorations"] = [item(entry) for entry in data["decorations"]]
            elif value["type"] == "repairable":
                items(data["items"], item)
        decoder.hooks["SlotComponent"] = component
        return decoder


def items(holder_set, item):
    if "ids" in holder_set:
        holder_set["ids"] = [item(entry) for entry in holder_set["ids"]]


def normalize(registries, name, value):
    """Replace remaining item IDs that the schema does not name as a type."""
    item = lambda entry: registries.name("item", entry)
    if name == "tags":
        for registry in value["tags"]:
            if registry["tagType"] == "minecraft:item":
                for tag in registry["tags"]:
                    tag["entries"] = [item(entry) for entry in tag["entries"]]
    elif name == "trade_list":
        for trade in value["trades"]:
            for cost in (trade["inputItem1"], trade["inputItem2"]):
                if cost is not None:
                    cost["itemId"] = item(cost["itemId"])
    elif name == "recipe_book_add":
        for entry in value["entries"]:
            for requirement in entry["recipe"]["craftingRequirements"] or []:
                items(requirement, item)
    elif name == "declare_recipes":
        for recipe in value["recipes"]:
            recipe["items"] = [item(entry) for entry in recipe["items"]]
        for recipe in value["stoneCutterRecipes"]:
            items(recipe["input"], item)
    elif name == "statistics":
        stat_types = registries.names["minecraft:stat_type"]
        for entry in value["entries"]:
            stat = stat_types[entry["categoryId"]]
            if stat in ("minecraft:crafted", "minecraft:used", "minecraft:broken",
                        "minecraft:picked_up", "minecraft:dropped"):
                entry["statisticId"] = item(entry["statisticId"])
    return value


class Player(Client):
    """A protocol client that records every packet on a reader thread."""

    def __init__(self, port, name, registries):
        self.registries = registries
        self.protocol = PROTOCOL[registries.version]
        self.lock = threading.Lock()  # The handshake below already sends.
        super().__init__(port, 2, self.protocol)
        self.name = name
        self.packets = []  # (state, packet ID, body)
        self.error = None
        self.state = "login"
        self.ids = {}
        for state in ("configuration", "play"):
            for direction in ("clientbound", "serverbound"):
                decoder = protodef.Decoder(registries.schema, state, direction)
                self.ids[state, direction] = {name: packet_id for packet_id, name in decoder.names.items()}

    def id(self, state, direction, name):
        return self.ids[state, direction][name]

    def send(self, packet_id, body=b""):
        with self.lock:
            super().send(packet_id, body)

    def play(self, name, body=b""):
        self.send(self.id("play", "serverbound", name), body)

    def join(self):
        player_id = uuid.UUID(bytes=hashlib.md5(f"OfflinePlayer:{self.name}".encode()).digest(), version=3)
        self.send(0, string(self.name) + player_id.bytes)
        while True:
            packet_id, body = self.receive()
            if packet_id == 3:
                self.threshold = read_varint(io.BytesIO(body))
            elif packet_id == 2:
                break
            else:
                raise AssertionError(f"{self.name}: unexpected login packet {packet_id}: {body[:200]!r}")
        self.send(3)  # Login acknowledged.
        self.state = "configuration"
        configuration = lambda name: self.id("configuration", "serverbound", name)
        self.send(configuration("settings"), client_settings(self.protocol))
        self.send(configuration("custom_payload"), string("minecraft:brand") + string("rift-test"))
        clientbound = self.ids["configuration", "clientbound"]
        while True:
            packet_id, body = self.receive()
            self.packets.append(("configuration", packet_id, body))
            if packet_id == clientbound["select_known_packs"]:
                self.send(configuration("select_known_packs"), varint(0))
            elif packet_id == clientbound["keep_alive"]:
                self.send(configuration("keep_alive"), body)
            elif packet_id == clientbound["ping"]:
                self.send(configuration("pong"), body)
            elif packet_id == clientbound["disconnect"]:
                raise AssertionError(f"{self.name}: configuration disconnect {body[:300]!r}")
            elif packet_id == clientbound["finish_configuration"]:
                self.send(configuration("finish_configuration"))
                break
        self.state = "play"
        self.deadline = time.monotonic() + 900
        self.read_timeout = 60
        threading.Thread(target=self.run, daemon=True).start()

    def run(self):
        clientbound = self.ids["play", "clientbound"]
        loaded = False
        try:
            while True:
                packet_id, body = self.receive()
                with self.lock:
                    self.packets.append(("play", packet_id, body))
                if packet_id == clientbound["keep_alive"]:
                    self.play("keep_alive", body)
                elif packet_id == clientbound["ping"]:
                    self.play("pong", body)
                elif packet_id == clientbound["chunk_batch_finished"]:
                    self.play("chunk_batch_received", struct.pack(">f", 10.0))
                elif packet_id == clientbound["position"]:
                    self.play("teleport_confirm", varint(read_varint(io.BytesIO(body))))
                    if not loaded:
                        self.play("player_loaded")
                        loaded = True
                elif packet_id == clientbound["kick_disconnect"]:
                    raise AssertionError(f"{self.name}: disconnected {body[:300]!r}")
        except Exception as error:  # Surface reader failures to the test.
            self.error = error

    def snapshot(self):
        with self.lock:
            if self.error:
                raise AssertionError(f"{self.name}: {self.error}")
            return list(self.packets)

    def wait(self, predicate, what, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for packet in self.snapshot():
                if predicate(*packet):
                    return packet
            time.sleep(0.05)
        raise AssertionError(f"{self.name}: timed out waiting for {what}")

    def marker(self, text):
        chat = self.id("play", "clientbound", "system_chat")
        encoded = text.encode()
        return self.wait(lambda state, packet_id, body: packet_id == chat and encoded in body,
                         f"marker {text}")

    def decoded(self, start=None, end=None):
        """Decode the recorded packets, between two markers if given."""
        packets = self.snapshot()
        if start is not None:
            packets = packets[packets.index(start):packets.index(end) + 1]
        decoders = {state: self.registries.decoder(state, "clientbound")
                    for state in ("configuration", "play")}
        result = []
        for state, packet_id, body in packets:
            try:
                name, value = decoders[state].packet(packet_id, body)
            except protodef.DecodeError as error:
                raise AssertionError(
                    f"{self.name} ({self.registries.version}): {state} packet {packet_id:#x}: {error}"
                ) from None
            result.append((state, name, normalize(self.registries, name, value)))
        return result


def no_components():
    return varint(0) + varint(0)


def creative_container(registries, contents):
    """A shulker box whose container component holds `contents`, untrusted encoding."""
    stacks = b"".join(varint(1) + varint(registries.id("item", name)) + no_components()
                      for name in contents)
    value = varint(len(contents)) + stacks
    container = registries.id("data_component_type", "container")
    return (varint(1) + varint(registries.id("item", "shulker_box"))
            + varint(1) + varint(0) + varint(container) + varint(len(value)) + value)


SCENARIO = [
    "tp @a 0 4 0",
    "reload",  # Play-state tags and recipes.
    "recipe give @a *",
    "advancement grant @a everything",
    "give @a minecraft:music_disc_wait",
    # Only on 1.21.7+; an older server rejects these, which both sessions see.
    "give @a minecraft:music_disc_lava_chicken",
    'give @a minecraft:shulker_box[container=[{slot:0,item:{id:"minecraft:music_disc_lava_chicken",count:1}}]]',
    "playsound minecraft:music_disc.lava_chicken master @a 0 5 0",
    "give @a minecraft:trident[enchantments={loyalty:3}]",
    'give @a minecraft:shulker_box[container=[{slot:0,item:{id:"minecraft:trident",count:1}},'
    '{slot:1,item:{id:"minecraft:music_disc_strad",count:1}}]]',
    'give @a minecraft:bundle[bundle_contents=[{id:"minecraft:phantom_membrane",count:2}]]',
    'give @a minecraft:crossbow[charged_projectiles=[{id:"minecraft:firework_rocket",count:1,'
    'components:{"minecraft:fireworks":{flight_duration:2,explosions:[{shape:"star",colors:[I;1]}]}}}]]',
    'give @a minecraft:decorated_pot[pot_decorations=["minecraft:sheaf_pottery_sherd","minecraft:brick",'
    '"minecraft:snort_pottery_sherd","minecraft:brick"]]',
    'give @a minecraft:diamond_sword[repairable={items:["minecraft:nautilus_shell","minecraft:trident"]}]',
    'give @a minecraft:apple[consumable={consume_seconds:1,sound:"minecraft:music_disc.wait",'
    'on_consume_effects:[{type:"play_sound",sound:"minecraft:music_disc.otherside"},'
    '{type:"apply_effects",effects:[{id:"minecraft:speed",duration:20}]}]},'
    'use_remainder={id:"minecraft:trident",count:1}]',
    'give @a minecraft:leather_helmet[equippable={slot:"head",equip_sound:"minecraft:music_disc.pigstep"}]',
    'give @a minecraft:goat_horn[instrument={sound_event:"minecraft:music_disc.wait",use_duration:1,'
    'range:32,description:"horn"}]',
    'give @a minecraft:goat_horn[instrument="minecraft:ponder_goat_horn"]',
    'give @a minecraft:shield[blocks_attacks={block_sound:"minecraft:music_disc.wait",'
    'disable_sound:"minecraft:music_disc.relic"}]',
    'give @a minecraft:stick[break_sound="minecraft:music_disc.wait"]',
    'give @a minecraft:stone[can_place_on={blocks:"minecraft:stone"},'
    'can_break={blocks:"minecraft:chest",state:{facing:"north"},components:{"minecraft:container":'
    '[{slot:0,item:{id:"minecraft:trident",count:1}}]}}]',
    'give @a minecraft:written_book[written_book_content={title:"t",author:"a",pages:["p"]}]',
    'give @a minecraft:potion[potion_contents={potion:"minecraft:swiftness",'
    'custom_effects:[{id:"minecraft:speed",duration:100,show_icon:true}]}]',
    'give @a minecraft:white_banner[banner_patterns=[{pattern:"minecraft:creeper",color:"red"}]]',
    'give @a minecraft:iron_chestplate[trim={material:"minecraft:gold",pattern:"minecraft:wild"}]',
    'give @a minecraft:stick[attribute_modifiers=[{type:"minecraft:attack_damage",id:"rift:test",'
    'amount:1,operation:"add_value",display:{type:"override",value:"hi"}}],'
    'tooltip_display={hidden_components:["minecraft:enchantments"]},'
    'tool={rules:[{blocks:"#minecraft:mineable/axe",speed:5}]},custom_model_data={floats:[1.5]}]',
    'give @a minecraft:compass[lodestone_tracker={target:{dimension:"minecraft:overworld",pos:[I;1,2,3]},'
    'tracked:false}]',
    'give @a minecraft:painting[painting/variant="minecraft:kebab"]',
    'give @a minecraft:chicken_spawn_egg[chicken/variant="minecraft:warm"]',
    "summon minecraft:item 0 5 0 {Item:{id:\"minecraft:trident\",count:1},NoGravity:1b,PickupDelay:32767}",
    "summon minecraft:item_display 1 5 0 {item:{id:\"minecraft:music_disc_wait\",count:1}}",
    "summon minecraft:armor_stand 2 4 0 {equipment:{mainhand:{id:\"minecraft:trident\",count:1},"
    "head:{id:\"minecraft:turtle_helmet\",count:1}},NoGravity:1b}",
    "particle minecraft:item{item:{id:\"minecraft:trident\"}} 0 5 0 0 0 0 0 1 force @a",
    "particle minecraft:dust{color:[1.0,0.0,0.0],scale:1.0} 0 5 0 0 0 0 0 1 force @a",
    "particle minecraft:vibration{destination:{type:\"block\",pos:[0,5,0]},arrival_in_ticks:10} "
    "0 6 0 0 0 0 0 1 force @a",
    "particle minecraft:block{block_state:\"minecraft:stone\"} 0 5 0 0 0 0 0 1 force @a",
    "playsound minecraft:music_disc.wait master @a 0 5 0",
    "playsound minecraft:block.anvil.land master @a 0 5 0",
    "summon minecraft:tnt 0 4 12 {fuse:0}",
    "summon minecraft:villager 1 4 1 {Invulnerable:1b,VillagerData:{profession:\"minecraft:armorer\",level:2,"
    "type:\"minecraft:plains\"},NoAI:1b,Offers:{Recipes:[{buy:{id:\"minecraft:trident\",count:1},"
    "sell:{id:\"minecraft:music_disc_wait\",count:1},buyB:{id:\"minecraft:phantom_membrane\",count:3}}]}}",
]

# Packets compared between the sessions after decoding and normalisation.
COMPARED = {"window_items", "set_slot", "set_cursor_item", "set_player_inventory", "entity_metadata",
            "entity_equipment", "world_particles", "sound_effect", "entity_sound_effect",
            "explosion", "trade_list", "craft_recipe_response", "recipe_book_add", "declare_recipes",
            "advancements", "tags"}
# World events either client may also see outside the shared scenario.
WINDOWED = {"sound_effect", "entity_sound_effect", "world_particles", "explosion"}
# Every one of these must occur in the scenario, or the comparison proves nothing.
REQUIRED = COMPARED - {"set_cursor_item", "set_player_inventory", "entity_sound_effect"}


def comparable(name, value):
    value = json.loads(json.dumps(value, default=lambda raw: raw.hex()))
    if name in ("window_items", "set_slot"):
        value.pop("stateId", None)
    if name == "advancements":
        # Progress is per player and carries no IDs; definitions hold the icons.
        value.pop("progressMapping")
    return value


def console(server, command):
    server.stdin.write(command + "\n")
    server.stdin.flush()


def wait_log(log, text, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        content = log.read_text()
        if text in content:
            return content
        time.sleep(0.05)
    raise AssertionError(f"server log never contained {text!r}")


def entity_ids(player, packets, types):
    """Entity IDs of spawned entities of the given types."""
    wanted = {player.registries.id("entity_type", name) for name in types}
    return {value["entityId"]: value["type"] for _, name, value in packets
            if name == "spawn_entity" and value["type"] in wanted}


def difference(a, b, path=""):
    """The first path at which two decoded values differ."""
    if isinstance(a, dict) and isinstance(b, dict) and a.keys() == b.keys():
        for key in a:
            if a[key] != b[key]:
                return difference(a[key], b[key], f"{path}.{key}")
    if isinstance(a, list) and isinstance(b, list) and len(a) == len(b):
        for index, (x, y) in enumerate(zip(a, b)):
            if x != y:
                return difference(x, y, f"{path}[{index}]")
    return path or ".", a, b


def substitute(value, stand_ins):
    """Replace registry names; inline sound events keep their own names."""
    if isinstance(value, str):
        return stand_ins.get(value, value)
    if isinstance(value, dict):
        return {key: entry if key == "soundName" else substitute(entry, stand_ins)
                for key, entry in value.items()}
    if isinstance(value, list):
        return [substitute(entry, stand_ins) for entry in value]
    return value


def compare(ref_packets, via_packets, ref_window, via_window, ref_ids, via_ids, stand_ins):
    """Compare the sessions; world events only while both clients are present.
    Reference names the translated client lacks become their stand-ins."""
    def select(packets, window, ids, stand_ins):
        selected = {}
        for source, windowed in ((packets, False), (window, True)):
            for _, name, value in source:
                if name not in COMPARED or (name in WINDOWED) != windowed:
                    continue
                if name in ("entity_metadata", "entity_equipment") and value["entityId"] not in ids:
                    continue
                selected.setdefault(name, []).append(substitute(comparable(name, value), stand_ins))
        return selected
    ref = select(ref_packets, ref_window, ref_ids, stand_ins)
    via = select(via_packets, via_window, via_ids, {})
    missing = REQUIRED - set(ref)
    if missing:
        raise AssertionError(f"scenario did not produce {sorted(missing)}")
    for name in sorted(COMPARED):
        if ref.get(name, []) != via.get(name, []):
            for index, (a, b) in enumerate(zip(ref.get(name, []), via.get(name, []))):
                if a != b:
                    path, a, b = difference(a, b)
                    raise AssertionError(f"{name} #{index} differs at {path}:\n"
                                         f" reference {json.dumps(a)[:800]}\n"
                                         f"translated {json.dumps(b)[:800]}")
            extra = lambda a, b: [json.dumps(x)[:300] for x in a if x not in b]
            raise AssertionError(f"{name}: only reference {extra(ref.get(name, []), via.get(name, []))}, "
                                 f"only translated {extra(via.get(name, []), ref.get(name, []))}; "
                                 f"{len(ref.get(name, []))} reference packets, "
                                 f"{len(via.get(name, []))} translated")
    return {name: len(entries) for name, entries in ref.items()}


def strings(value):
    """String values, except inline sound events' names, which are not references."""
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for key, entry in value.items():
            if key != "sound_id":
                yield from strings(entry)
    elif isinstance(value, list):
        for entry in value:
            yield from strings(entry)


def foreign_references(packets, client, server):
    """Built-in item or sound names in registry data that the client lacks."""
    foreign = {name for registry in ("minecraft:item", "minecraft:sound_event")
               for name in set(server.names[registry]) - set(client.names[registry])}
    return {text for state, name, value in packets
            if state == "configuration" and name == "registry_data"
            for text in strings(value) if text in foreign}


def raw_item_ids(player, packets):
    """Item IDs exactly as received in inventory updates, before naming."""
    found = set()
    decoder = protodef.Decoder(player.registries.schema, "play", "clientbound")
    decoder.hooks["Slot"] = lambda value: found.add(value["itemId"]) if value.get("itemCount") else None
    for state, packet_id, body in packets:
        if state == "play" and decoder.names.get(packet_id) in ("window_items", "set_slot"):
            decoder.packet(packet_id, body)
    return found


def check(binary, mode, accept_eula):
    if not accept_eula:
        raise SystemExit("Pass --accept-eula to accept the Minecraft EULA for the test servers.")
    fixture, server_version, client_version = MODES[mode]
    server_registries, client_registries = Registries(server_version), Registries(client_version)
    runs = CACHE / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix=f"translation-{mode}-", dir=runs))
    print(f"Translation acceptance logs: {directory}", flush=True)
    backend, proxy = unused_port(), unused_port()
    while proxy == backend:
        proxy = unused_port()
    with ExitStack() as stack:
        (directory / "server").mkdir()
        command = configure_server(fixture, directory / "server", backend, True)
        log = directory / "server" / "server.log"
        server = stack.enter_context(process(command, directory / "server", "server.log", server=True))
        wait_ready(server, lambda: "Done (" in log.read_text()
                   and status_ready(backend, PROTOCOL[server_version]), log)
        config = directory / "rift.lua"
        config.write_text(f"""local config = require("rift.config")
config.listeners.public = "127.0.0.1:{proxy}"
config.backends.lobby = {{ address = "127.0.0.1:{backend}", version = "{server_version}" }}
config.routes.public = "lobby"
""")
        rift = stack.enter_context(process([str(binary), "--config", str(config)], directory, "rift.log"))
        wait_ready(rift, lambda: "rift:" in (directory / "rift.log").read_text(), directory / "rift.log")

        suffix = uuid.uuid4().hex[:6]
        reference = stack.enter_context(Player(backend, "Ref" + suffix, server_registries))
        translated = stack.enter_context(Player(proxy, "Via" + suffix, client_registries))
        reference.join()
        translated.join()
        for player in (reference, translated):
            player.wait(lambda state, packet_id, body, player=player:
                        packet_id == player.id("play", "clientbound", "position"), "spawn")
        console(server, "gamerule sendCommandFeedback true")
        console(server, 'tellraw @a {"text":"RIFT-START"}')
        start = [player.marker("RIFT-START") for player in (reference, translated)]
        for command in SCENARIO:
            console(server, command)
        time.sleep(1)
        # Statistics and trades need client requests; ghost recipes a placement.
        for player in (reference, translated):
            # Drop the selected music disc for an item statistic, then request them.
            player.play("block_dig", varint(4) + bytes(8) + b"\0" + varint(0))
        time.sleep(0.5)
        for player in (reference, translated):
            player.play("client_command", varint(1))
        console(server, 'tellraw @a {"text":"RIFT-SPAWNED"}')
        for player in (reference, translated):
            player.marker("RIFT-SPAWNED")
            packets = player.decoded()
            # A 2x2 recipe the empty inventory crafting grid shows as a ghost.
            book = [entry for _, name, value in packets if name == "recipe_book_add"
                    for entry in value["entries"]]
            recipe = next(entry["recipe"]["displayId"] for entry in book
                          if entry["recipe"]["display"]["type"] == "crafting_shaped"
                          and entry["recipe"]["display"]["data"]["width"] <= 2
                          and entry["recipe"]["display"]["data"]["height"] <= 2)
            player.play("craft_recipe_request", varint(0) + varint(recipe) + b"\0")
            player.wait(lambda state, packet_id, body, player=player:
                        packet_id == player.id("play", "clientbound", "craft_recipe_response"),
                        "ghost recipe")
            villager = next(iter(entity_ids(player, packets, ["villager"])))
            # Interact with the main hand, not sneaking; this opens window 1.
            player.play("use_entity", varint(villager) + varint(0) + varint(0) + b"\0")
            player.wait(lambda state, packet_id, body, player=player:
                        packet_id == player.id("play", "clientbound", "trade_list"), "trades")
            player.play("close_window", varint(1))
        console(server, 'tellraw @a {"text":"RIFT-END"}')
        end = [player.marker("RIFT-END") for player in (reference, translated)]

        # Whole sessions must decode in each client's own protocol.
        ref_all, via_all = reference.decoded(), translated.decoded()
        foreign = foreign_references(via_all, client_registries, server_registries)
        assert not foreign, f"registry data names entries the client's version lacks: {foreign}"
        if mode == "downgrade":
            # The server's own reference, proving the rewrite was exercised.
            assert foreign_references(ref_all, client_registries, server_registries)
        configuration = lambda packets: [(name, comparable(name, value))
                                         for state, name, value in packets
                                         if state == "configuration" and name == "tags"]
        assert configuration(ref_all) == configuration(via_all), "configuration tags differ"
        # Join-time packets (recipes, tags, initial advancements) count too.
        ref_play = [packet for packet in ref_all if packet[0] == "play"]
        via_play = [packet for packet in via_all if packet[0] == "play"]
        ref_window = reference.decoded(start[0], end[0])
        via_window = translated.decoded(start[1], end[1])
        types = ["item", "item_display", "armor_stand", "villager", "tnt"]
        invalid = [(name, text) for packets in (ref_all, via_all) for _, name, value in packets
                   for text in strings(value) if text.startswith("<invalid")]
        assert not invalid, f"IDs outside their registries: {invalid[:5]}"
        # Names only the server's version has, as the translated client sees them.
        stand_ins = {name: stand_in for registry, entries in translation_data.FALLBACKS.items()
                     for name, stand_in in entries.items()
                     if name in server_registries.ids[registry]
                     and name not in client_registries.ids[registry]}
        counts = compare(ref_play, via_play, ref_window, via_window,
                         entity_ids(reference, ref_all, types), entity_ids(translated, via_all, types),
                         stand_ins)
        # Item statistics (per player, so compared by the items they name).
        item_stats = lambda packets: {(entry["categoryId"], entry["statisticId"])
                                      for _, name, value in packets if name == "statistics"
                                      for entry in value["entries"]
                                      if isinstance(entry["statisticId"], str)}
        assert item_stats(via_all), "the translated client received no item statistics"
        assert item_stats(ref_all) == item_stats(via_all), "item statistics differ"
        # Proof the comparison is not trivial: some raw IDs really changed.
        ref_raw, via_raw = raw_item_ids(reference, reference.snapshot()), raw_item_ids(translated, translated.snapshot())
        assert ref_raw != via_raw, "translation did not renumber any item"
        print(f"PASS clientbound {mode}: {counts}", flush=True)

        # Serverbound: creative items, including one the server's version lacks.
        lacks = "music_disc_lava_chicken" if mode == "upgrade" else None
        contents = ["trident", "music_disc_wait"] + ([lacks] if lacks else [])
        translated.play("set_creative_slot", struct.pack(">h", 44)
                        + creative_container(client_registries, contents))
        translated.play("set_creative_slot", struct.pack(">h", 43)
                        + varint(1) + varint(client_registries.id("item", "trident")) + no_components())
        time.sleep(1)
        console(server, f"data get entity {translated.name} Inventory")
        logged = wait_log(log, f"{translated.name} has the following entity data")
        line = next(line for line in reversed(logged.splitlines())
                    if f"{translated.name} has the following entity data" in line)
        for name in ["shulker_box", "trident", "music_disc_wait"] + (["music_disc_creator"] if lacks else []):
            assert f'"minecraft:{name}"' in line, f"server inventory lacks {name}: {line}"
        if lacks:
            # The creative item the server lacks arrived as its stand-in.
            assert "lava_chicken" not in line, line

        # Hashed clicks: pick up the plain trident in slot 43. The server
        # compares the client's claimed stacks with its own and resends any
        # that differ, so a wrongly translated item ID causes a resync.
        resync = {translated.id("play", "clientbound", name)
                  for name in ("set_cursor_item", "set_slot", "window_items")}

        def click(slot_claim, carried, marker):
            state_id = max(value["stateId"] for _, name, value in translated.decoded()
                           if name in ("window_items", "set_slot") and value["windowId"] == 0)
            before = len(translated.snapshot())
            translated.play("window_click", varint(0) + varint(state_id) + struct.pack(">hb", 43, 0)
                            + varint(0) + varint(1) + struct.pack(">h", 43) + slot_claim + carried)
            console(server, f'tellraw @a {{"text":"{marker}"}}')
            translated.marker(marker)
            return [packet for packet in translated.snapshot()[before:] if packet[1] in resync]

        hashed = lambda name: (b"\1" + varint(client_registries.id("item", name)) + varint(1)
                               + varint(0) + varint(0))
        assert not click(b"\0", hashed("trident"), "RIFT-CLICK"), \
            "server resynced the inventory after a translated hashed click"
        # Control: claiming the wrong item does make the server resync.
        assert click(hashed("stick"), hashed("stick"), "RIFT-CONTROL"), \
            "a wrong hashed claim was not resynced; the click check is not meaningful"
        # Neither connection was dropped or failed to parse along the way.
        reference.snapshot()
        translated.snapshot()
        print(f"PASS serverbound {mode}: creative items and hashed clicks", flush=True)


def check_network(binary, accept_eula):
    """A 1.21.8 client switches between a native 1.21.8 lobby and a translated
    1.21.6 survival server on one socket, with the network acceptance checks."""
    if not accept_eula:
        raise SystemExit("Pass --accept-eula to accept the Minecraft EULA for the test servers.")
    from network_minecraft import RealClient, verify_backend
    runs = CACHE / "runs"
    runs.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="translation-network-", dir=runs))
    print(f"Translation network logs: {directory}", flush=True)
    ports = {}
    while len(set(ports.values())) < 3:
        ports = {name: unused_port() for name in ("lobby", "survival", "proxy")}
    servers = {"lobby": "1.21.8", "survival": "1.21.6"}
    with ExitStack() as stack:
        processes = {}
        for name, version in servers.items():
            (directory / name).mkdir()
            command = configure_server(FIXTURE[version], directory / name, ports[name], True)
            if name == "survival":
                properties = directory / name / "server.properties"
                properties.write_text(properties.read_text().replace("gamemode=creative", "gamemode=survival"))
            processes[name] = stack.enter_context(
                process(command, directory / name, "server.log", server=True))
        for name, version in servers.items():
            log = directory / name / "server.log"
            wait_ready(processes[name], lambda log=log, name=name, version=version: (
                "Done (" in log.read_text() and status_ready(ports[name], PROTOCOL[version])), log)
            console(processes[name], "worldborder set " + ("128" if name == "lobby" else "256"))
        config = directory / "rift.lua"
        config.write_text(f"""local config = require("rift.config")
config.listeners.public = "127.0.0.1:{ports['proxy']}"
config.backends.lobby = "127.0.0.1:{ports['lobby']}"
config.backends.survival = {{ address = "127.0.0.1:{ports['survival']}", version = "1.21.6" }}
config.routes.public = "lobby"
config.network = {{ initial = {{ "lobby" }}, hubs = {{ "lobby" }} }}
""")
        proxy = stack.enter_context(process([str(binary), "--config", str(config)], directory, "proxy.log"))
        wait_ready(proxy, lambda: "rift:" in (directory / "proxy.log").read_text(), directory / "proxy.log")
        name = "RiftVia" + uuid.uuid4().hex[:6]
        with RealClient(ports["proxy"], name, PROTOCOL["1.21.8"]) as client:
            client.deadline = time.monotonic() + 240
            client.ready(1, 0, 0)
            verify_backend(client, processes["lobby"], directory / "lobby" / "server.log", "lobby-initial")
            for generation, (command, target) in enumerate(
                    [("server survival", "survival"), ("hub", "lobby"), ("server survival", "survival")],
                    start=2):
                chunks, positions = client.chunks, client.positions
                client.command(command, signed=command == "hub")
                client.ready(generation, chunks, positions)
                verify_backend(client, processes[target], directory / target / "server.log", target)
            assert client.transitions == 3
    print("PASS network: 1.21.8 client, native 1.21.8 lobby <-> translated 1.21.6 survival", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--binary", type=Path, default=ROOT / "target" / "release" / "rift")
    parser.add_argument("--mode", choices=sorted(MODES) + ["network"], action="append")
    parser.add_argument("--accept-eula", action="store_true")
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)  # Processes run in their own directories.
    for mode in args.mode or sorted(MODES) + ["network"]:
        if mode == "network":
            check_network(args.binary, args.accept_eula)
        else:
            check(args.binary, mode, args.accept_eula)


if __name__ == "__main__":
    main()
