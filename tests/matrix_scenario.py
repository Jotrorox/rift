"""The translation matrix scenario: named, independent steps.

Each step drives the server console and both clients, then checks
  * that the reference client (direct, server's protocol) saw the effect: if
    not, the scenario itself is broken for that server and the step fails
    with a SCENARIO note (on Pumpkin, which implements fewer commands, it is
    skipped instead);
  * that the translated client (through the proxy, client's protocol) saw the
    same thing after normalising registry IDs to names. A name the client's
    version lacks is expected to arrive as a stand-in: INFO. An ID outside the
    client's registry, or a name the client knows arriving as another one, is
    a failure;
  * for serverbound behaviour, that the server reacted (console log, packets
    the reference client sees).
A step declares the oldest server and client protocol it applies to and the
server software it supports. Console commands are written per server version
(Commands); the step code never formats version-dependent syntax itself.
"""

import json
import re
import struct
import time

from matrix_client import SessionError, text_of
import matrix_registries


class StepFailure(AssertionError):
    pass


class Skip(Exception):
    pass


ALL = ("vanilla", "paper", "spigot", "pumpkin")
JAVA = ("vanilla", "paper", "spigot")


class Step:
    def __init__(self, function, min_server=0, min_client=0, software=JAVA, needs=(), doc=None):
        self.name = function.__name__.removeprefix("step_")
        self.function = function
        self.min_server = min_server
        self.min_client = min_client
        self.software = software
        self.doc = (doc or function.__doc__ or "").strip().splitlines()[0] if (doc or function.__doc__) else ""

    def applies(self, server_protocol, client_protocol, software):
        if software not in self.software:
            return f"not supported on {software}"
        if server_protocol < self.min_server:
            return f"needs a server >= protocol {self.min_server}"
        if client_protocol < self.min_client:
            return f"needs a client >= protocol {self.min_client}"
        return None


STEPS = []


def step(min_server=0, min_client=0, software=JAVA):
    def register(function):
        STEPS.append(Step(function, min_server, min_client, software))
        return function
    return register


# --- Console command syntax by server version --------------------------------

def quote_json(text):
    return json.dumps({"text": text})


SINGLE_QUOTED = re.compile(r"'([^']*)'")


class Commands:
    """Console commands in the syntax of one server protocol."""

    def nbt(self, command):
        """1.13 parses only double-quoted SNBT strings (single quotes came in 1.14)."""
        if 393 <= self.p < 477:
            return SINGLE_QUOTED.sub(lambda match: json.dumps(match.group(1)), command)
        return command

    def __init__(self, protocol, software):
        self.p = protocol
        self.software = software
        self.legacy = protocol < 393      # 1.12: numeric data values, old command names.
        self.components = protocol >= 766  # 1.20.5: item data components.
        self.snbt_text = protocol >= 770  # 1.21.5: text components as SNBT in commands.

    def count(self, n):
        return f"count:{n}" if self.components else f"Count:{n}b"

    def text(self, value):
        """A text component inside NBT: plain (1.12), JSON string, or SNBT."""
        if self.legacy:
            return json.dumps(value)
        if self.snbt_text:
            return json.dumps(value)
        return "'" + json.dumps(value) + "'"

    def give(self, target, item):
        """`item`: dict with legacy=(name, damage, nbt), nbt=(name, nbt),
        components=(name, comps), snbt=(name, comps)."""
        if self.legacy:
            name, damage, nbt = item["legacy"]
            return f"give {target} minecraft:{name} {item.get('n', 1)} {damage} {nbt}".rstrip()
        if not self.components:
            name, nbt = item["nbt"]
            if self.p < 477 and "nbt13" in item:
                name, nbt = item["nbt13"]
            return self.nbt(f"give {target} minecraft:{name}{nbt} {item.get('n', 1)}")
        name, comps = item["snbt"] if self.snbt_text and "snbt" in item else item["components"]
        return f"give {target} minecraft:{name}{'[' + comps + ']' if comps else ''} {item.get('n', 1)}"

    def item_name(self, item):
        """The item `give` names in this protocol, or None without a spec."""
        if self.legacy:
            return "minecraft:" + item["legacy"][0] if "legacy" in item else None
        if item["min"] > self.p:
            return None
        if not self.components:
            name = item["nbt13"][0] if self.p < 477 and "nbt13" in item else item["nbt"][0]
        else:
            name = (item["snbt"] if self.snbt_text and "snbt" in item else item["components"])[0]
        return "minecraft:" + name

    def block_name(self, block):
        """The block `setblock` places in this protocol."""
        spec = block["legacy"] if self.legacy else block["flat"]
        if callable(spec):
            spec = spec(self)
        return re.split(r"[\[{ ]", spec, maxsplit=1)[0]

    def setblock(self, x, y, z, block):
        """`block`: dict with legacy=spec and flat=spec (or a function of self)."""
        spec = block["legacy"] if self.legacy else block["flat"]
        if callable(spec):
            spec = spec(self)
        return self.nbt(f"setblock {x} {y} {z} {spec}")

    def summon(self, entity, x, y, z, nbt=""):
        return self.nbt(f"summon minecraft:{entity} {x} {y} {z} {nbt}".rstrip())

    def gamemode(self, target, mode):
        if self.legacy:
            mode = {"survival": 0, "creative": 1, "adventure": 2, "spectator": 3}[mode]
        return f"gamemode {mode} {target}"

    def effect(self, target, effect, seconds, amplifier=0):
        if self.legacy:
            return f"effect {target} minecraft:{effect} {seconds} {amplifier}"
        return f"effect give {target} minecraft:{effect} {seconds} {amplifier}"

    def title(self, target, kind, text):
        return f"title {target} {kind} {quote_json(text)}"

    def tellraw(self, target, text):
        return f"tellraw {target} {quote_json(text)}"

    def particle(self, kind, x, y, z):
        """kind: heart, dust, block, item."""
        if self.legacy:
            name = {"heart": "heart", "dust": "reddust", "block": "blockcrack", "item": "iconcrack"}[kind]
            # 1.12 reads data parameters only after the viewing players.
            extra = {"block": " @a 1", "item": " @a 264"}.get(kind, "")
            return f"particle {name} {x} {y} {z} 0 0 0 0 1 force{extra}"
        if kind == "heart":
            spec = "minecraft:heart"
        elif kind == "dust":
            spec = ("minecraft:dust{color:[1.0,0.0,0.0],scale:1.0}" if self.components
                    else "minecraft:dust 1 0 0 1")
        elif kind == "block":
            spec = ('minecraft:block{block_state:"minecraft:stone"}' if self.components
                    else "minecraft:block minecraft:stone")
        else:
            spec = ('minecraft:item{item:"minecraft:diamond"}' if self.components
                    else "minecraft:item minecraft:diamond")
        return f"particle {spec} {x} {y} {z} 0 0 0 0 1 force"

    def playsound(self, sound, target, x, y, z):
        return f"playsound minecraft:{sound} master {target} {x} {y} {z}"

    def xp(self, target, levels):
        return f"xp {levels}L {target}" if self.legacy else f"experience add {target} {levels} levels"

    def team(self, *args):
        return ("scoreboard teams " if self.legacy else "team ") + " ".join(args)

    def team_option(self, team, option, value):
        if self.legacy:
            return f"scoreboard teams option {team} {option} {value}"
        return f"team modify {team} {option} {value}"

    def clear_count(self, target, item):
        """A command that reports how many `item` the player has."""
        if self.legacy:
            return f"clear {target} minecraft:{item} -1 0"
        return f"clear {target} minecraft:{item} 0"

    def clear_count_pattern(self, target, item):
        if self.legacy:
            return rf"{target} has (\d+) items? that match"
        return rf"Found (\d+) matching item.* on player {target}"

    def damage(self, target, amount):
        if self.p >= 762:
            return f"damage {target} {amount}"
        return self.effect(target, "instant_damage", 1, 0)

    def gamerule(self, old, new, value):
        """Game rules are registry entries with snake_case names from 1.21.11."""
        return f"gamerule {new if self.p >= 774 else old} {value}"


# --- Items, blocks and entities by era --------------------------------------

def lore(cmd):
    return '"Rift lore"' if cmd.p < 477 else "'" + quote_json("Rift lore") + "'"


ITEMS = [
    dict(key="diamond_sword", min=0,
         legacy=("diamond_sword", 0, '{ench:[{id:16s,lvl:5s}],display:{Name:"Rift Sword",Lore:["Rift lore"]}}'),
         nbt=("diamond_sword", '{Enchantments:[{id:"minecraft:sharpness",lvl:5s}],display:{Name:\''
              + quote_json("Rift Sword") + '\',Lore:[\'' + quote_json("Rift lore") + '\']}}'),
         nbt13=("diamond_sword", '{Enchantments:[{id:"minecraft:sharpness",lvl:5s}],display:{Name:\''
                + quote_json("Rift Sword") + '\',Lore:["Rift lore"]}}'),
         components=("diamond_sword", 'enchantments={levels:{"minecraft:sharpness":5}},custom_name=\''
                     + quote_json("Rift Sword") + '\',lore=[\'' + quote_json("Rift lore") + '\']'),
         snbt=("diamond_sword", 'enchantments={"minecraft:sharpness":5},custom_name="Rift Sword",lore=["Rift lore"]')),
    dict(key="written_book", min=0,
         legacy=("written_book", 0, '{title:"Rift",author:"Rift",pages:["{\\"text\\":\\"page\\"}"]}'),
         nbt=("written_book", '{title:"Rift",author:"Rift",pages:[\'' + quote_json("page") + '\']}'),
         components=("written_book", 'written_book_content={title:"Rift",author:"Rift",pages:[\'"page"\']}'),
         snbt=("written_book", 'written_book_content={title:"Rift",author:"Rift",pages:["page"]}')),
    dict(key="potion", min=0, legacy=("potion", 0, '{Potion:"minecraft:swiftness"}'),
         nbt=("potion", '{Potion:"minecraft:swiftness"}'),
         components=("potion", 'potion_contents={potion:"minecraft:swiftness"}')),
    dict(key="firework_rocket", min=0,
         legacy=("fireworks", 0, '{Fireworks:{Flight:1b,Explosions:[{Type:1b,Colors:[I;16711680]}]}}'),
         nbt=("firework_rocket", '{Fireworks:{Flight:1b,Explosions:[{Type:1b,Colors:[I;16711680]}]}}'),
         components=("firework_rocket",
                     'fireworks={flight_duration:1,explosions:[{shape:"large_ball",colors:[I;16711680]}]}')),
    # Pumpkin 0.2.0 writes a banner pattern holder without its +1 offset
    # (translation_matrix.SERVER_DEVIATIONS): the official 26.3 client cannot
    # read such a slot either and disconnects, so it is not given there.
    dict(key="red_banner", min=0, not_on=("pumpkin",),
         legacy=("banner", 1, '{BlockEntityTag:{Base:1,Patterns:[{Pattern:"cre",Color:15}]}}'),
         nbt=("red_banner", '{BlockEntityTag:{Patterns:[{Pattern:"cre",Color:15}]}}'),
         components=("red_banner", 'banner_patterns=[{pattern:"minecraft:creeper",color:"black"}]')),
    dict(key="shield", min=0, legacy=("shield", 0, '{BlockEntityTag:{Base:4}}'),
         nbt=("shield", '{BlockEntityTag:{Base:4}}'), components=("shield", 'base_color="yellow"')),
    dict(key="player_head", min=0, legacy=("skull", 3, ""), nbt=("player_head", ""),
         components=("player_head", "")),
    dict(key="purple_shulker_box", min=0,
         legacy=("purple_shulker_box", 0,
                 '{BlockEntityTag:{Items:[{Slot:0b,id:"minecraft:diamond",Count:2b,Damage:0s}]}}'),
         nbt=("purple_shulker_box", '{BlockEntityTag:{Items:[{Slot:0b,id:"minecraft:diamond",Count:2b}]}}'),
         components=("purple_shulker_box", 'container=[{slot:0,item:{id:"minecraft:diamond",count:2}}]')),
    dict(key="map", min=0, legacy=("map", 0, ""), nbt=("map", ""), components=("map", "")),
    dict(key="leather_chestplate", min=0, legacy=("leather_chestplate", 0, "{display:{color:16711680}}"),
         nbt=("leather_chestplate", "{display:{color:16711680}}"),
         components=("leather_chestplate", "dyed_color={rgb:16711680}"),
         snbt=("leather_chestplate", "dyed_color=16711680")),
    dict(key="enchanted_book", min=0, legacy=("enchanted_book", 0, "{StoredEnchantments:[{id:0s,lvl:1s}]}"),
         nbt=("enchanted_book", '{StoredEnchantments:[{id:"minecraft:protection",lvl:1s}]}'),
         components=("enchanted_book", 'stored_enchantments={levels:{"minecraft:protection":1}}'),
         snbt=("enchanted_book", 'stored_enchantments={"minecraft:protection":1}')),
    dict(key="pig_spawn_egg", min=0, legacy=("spawn_egg", 0, '{EntityTag:{id:"minecraft:pig"}}'),
         nbt=("pig_spawn_egg", ""), components=("pig_spawn_egg", "")),
    dict(key="golden_apple", min=0, n=3, legacy=("golden_apple", 0, ""), nbt=("golden_apple", ""),
         components=("golden_apple", "")),
    dict(key="trident", min=393, nbt=("trident", '{Enchantments:[{id:"minecraft:loyalty",lvl:3s}]}'),
         components=("trident", 'enchantments={levels:{"minecraft:loyalty":3}}'),
         snbt=("trident", 'enchantments={"minecraft:loyalty":3}')),
    dict(key="crossbow", min=477, nbt=("crossbow", ""), components=("crossbow", "")),
    dict(key="bundle", min=768, components=("bundle", 'bundle_contents=[{id:"minecraft:diamond",count:2}]')),
]


def sign_text(cmd):
    if cmd.p < 393:
        return 'minecraft:standing_sign 0 replace {Text1:"{\\"text\\":\\"Rift\\"}"}'
    block = "sign" if cmd.p < 477 else "oak_sign"
    if cmd.p < 763:
        return f"minecraft:{block}" + "{Text1:'" + quote_json("Rift") + "'}"
    if cmd.p < 770:
        return "minecraft:oak_sign{front_text:{messages:['\"Rift\"','\"\"','\"\"','\"\"']}}"
    return 'minecraft:oak_sign{front_text:{messages:["Rift","","",""]}}'


def chest_items(cmd):
    return "{Items:[{Slot:0b,id:\"minecraft:diamond\"," + cmd.count(3) + "}]}"


def banner(cmd):
    if cmd.components:
        return 'minecraft:red_banner{patterns:[{pattern:"minecraft:creeper",color:"black"}]}'
    return 'minecraft:red_banner{Patterns:[{Pattern:"cre",Color:15}]}'


# (name, dx, dy, dz, block spec). The build stands on the surface east of spawn.
BLOCKS = [
    ("stone", 4, 0, 3, dict(legacy="minecraft:stone 0", flat="minecraft:stone")),
    ("stairs", 5, 0, 3, dict(legacy="minecraft:oak_stairs 1", flat="minecraft:oak_stairs[facing=west,half=bottom]")),
    ("slab", 6, 0, 3, dict(legacy="minecraft:stone_slab 0", flat="minecraft:stone_slab[type=bottom]")),
    ("door_lower", 7, 0, 3, dict(legacy="minecraft:wooden_door 1", flat="minecraft:oak_door[half=lower,facing=north]")),
    ("door_upper", 7, 1, 3, dict(legacy="minecraft:wooden_door 8", flat="minecraft:oak_door[half=upper,facing=north]")),
    ("redstone", 8, 0, 3, dict(legacy="minecraft:redstone_wire 0", flat="minecraft:redstone_wire")),
    ("lever", 9, 0, 3, dict(legacy="minecraft:lever 13", flat="minecraft:lever[face=floor,powered=true]")),
    ("sign", 10, 0, 3, dict(legacy=sign_text, flat=sign_text)),
    ("chest", 4, 0, 5, dict(legacy=lambda c: "minecraft:chest 2 replace {Items:[{Slot:0b,id:\"minecraft:diamond\",Count:3b,Damage:0s}]}",
                             flat=lambda c: "minecraft:chest[facing=north]" + chest_items(c))),
    ("banner", 11, 0, 3, dict(legacy='minecraft:standing_banner 0 replace {Base:1,Patterns:[{Pattern:"cre",Color:15}]}',
                              flat=banner)),
    ("skull", 12, 0, 3, dict(legacy="minecraft:skull 1 replace {SkullType:1b,Rot:0b}",
                             flat="minecraft:wither_skeleton_skull")),
    ("bed_foot", 13, 0, 3, dict(legacy="minecraft:bed 0", flat="minecraft:red_bed[part=foot,facing=south]")),
    ("bed_head", 13, 0, 4, dict(legacy="minecraft:bed 8", flat="minecraft:red_bed[part=head,facing=south]")),
    ("flower_pot", 14, 0, 3, dict(legacy='minecraft:flower_pot 0 replace {Item:"minecraft:red_flower",Data:0}',
                                  flat="minecraft:potted_poppy")),
    ("piston", 15, 0, 3, dict(legacy="minecraft:piston 1", flat="minecraft:piston[facing=up]")),
    ("wool", 16, 0, 3, dict(legacy="minecraft:wool 14", flat="minecraft:red_wool")),
    ("fence", 17, 0, 3, dict(legacy="minecraft:fence 0", flat="minecraft:oak_fence")),
    ("furnace", 5, 0, 5, dict(legacy="minecraft:furnace 2", flat="minecraft:furnace[facing=north]")),
    ("hopper", 6, 0, 5, dict(legacy="minecraft:hopper 0", flat="minecraft:hopper")),
    ("dispenser", 7, 0, 5, dict(legacy="minecraft:dispenser 1", flat="minecraft:dispenser[facing=up]")),
    ("crafting_table", 8, 0, 5, dict(legacy="minecraft:crafting_table 0", flat="minecraft:crafting_table")),
    ("enchanting_table", 9, 0, 5, dict(legacy="minecraft:enchanting_table 0", flat="minecraft:enchanting_table")),
    ("anvil", 10, 0, 5, dict(legacy="minecraft:anvil 0", flat="minecraft:anvil")),
    ("break_target", 3, 0, 2, dict(legacy="minecraft:gold_block 0", flat="minecraft:gold_block")),
]
# Containers the scenario opens: block name -> position key above.
CONTAINERS = ["chest", "furnace", "hopper", "dispenser", "crafting_table", "enchanting_table", "anvil"]


def entity_nbt(cmd, name):
    count = cmd.count(1)
    custom = (lambda text: f"CustomName:{json.dumps(text)}" if cmd.legacy or cmd.snbt_text
              else "CustomName:'" + json.dumps(text) + "'")
    if name == "zombie":
        if cmd.p >= 770:
            gear = (f'equipment:{{head:{{id:"minecraft:diamond_helmet",{count}}},'
                    f'mainhand:{{id:"minecraft:iron_sword",{count}}}}}')
        else:
            gear = (f'ArmorItems:[{{}},{{}},{{}},{{id:"minecraft:diamond_helmet",{count}}}],'
                    f'HandItems:[{{id:"minecraft:iron_sword",{count}}},{{}}]')
        return f"{{NoAI:1b,Silent:1b,{custom('RiftZombie')},CustomNameVisible:1b,{gear}}}"
    if name == "item":
        return f'{{Item:{{id:"minecraft:diamond",{count}}},PickupDelay:32767,NoGravity:1b}}'
    if name == "item_frame":
        facing = "Facing:0b" if cmd.legacy else "Facing:3b"
        return f'{{{facing},Item:{{id:"minecraft:diamond",{count}}}}}'
    if name == "painting":
        if cmd.legacy:
            return '{Motive:"Kebab",Facing:0b}'
        if cmd.p < 759:
            return '{Motive:"minecraft:kebab",Facing:3b}'
        return '{variant:"minecraft:kebab",facing:3b}'
    if name == "falling_block":
        if cmd.legacy:
            return '{Block:"minecraft:sand",Time:1,NoGravity:1b}'
        if cmd.p >= 777:  # 26.3 reads a block state string or {id:...}.
            return '{BlockState:"minecraft:sand",Time:1,NoGravity:1b}'
        return '{BlockState:{Name:"minecraft:sand"},Time:1,NoGravity:1b}'
    if name == "pig_rider":
        return f'{{NoAI:1b,{custom("RiftPig")},Passengers:[{{id:"minecraft:chicken",NoAI:1b}}]}}'
    if name == "leashed":
        return "{NoAI:1b}"
    return "{NoGravity:1b}"


# (label, entity id by era, dx, dz, nbt key)
ENTITIES = [
    ("zombie", lambda c: "zombie", 2, 6, "zombie"),
    ("armor_stand", lambda c: "armor_stand", 3, 6, "armor_stand"),
    ("item_frame", lambda c: "item_frame", 4, 4, "item_frame"),
    ("painting", lambda c: "painting", 5, 4, "painting"),
    ("falling_block", lambda c: "falling_block", 6, 6, "falling_block"),
    ("boat", lambda c: "oak_boat" if c.p >= 768 else "boat", 7, 6, "boat"),
    ("minecart", lambda c: "minecart", 8, 6, "minecart"),
    ("arrow", lambda c: "arrow", 9, 6, "arrow"),
    ("experience_orb", lambda c: "xp_orb" if c.legacy else "experience_orb", 10, 6, "orb"),
    ("item", lambda c: "item", 11, 6, "item"),
    ("pig", lambda c: "pig", 12, 6, "pig_rider"),
    ("cow", lambda c: "cow", 16, 4, "leashed"),
]


# --- Views: what a client saw, by registry name -----------------------------

def slot_item(registries, slot):
    """(name, count) of a slot value in any version's layout, or None."""
    if not isinstance(slot, dict):
        return None
    item_id = slot.get("itemId", slot.get("blockId"))
    count = slot.get("itemCount", 0)
    if item_id is None or item_id == -1 or not count or slot.get("present") is False:
        return None
    return registries.name("item", item_id), count


def items_seen(session, registries, since):
    seen = []
    for record in session.packets({"set_slot", "window_items", "set_player_inventory"}, since):
        value = record.value
        slots = value["items"] if record.name == "window_items" else [value.get("item") or value.get("contents")]
        for slot in slots:
            item = slot_item(registries, slot)
            if item:
                seen.append(item[0])
    return seen


def entity_types_seen(session, registries, since):
    seen = {}
    for record in session.packets(None, since):
        value, name = record.value, record.name
        if value is None:
            continue
        if name == "spawn_entity":
            seen[value["entityId"]] = registries.entity(value["type"], living=False)
        elif name == "spawn_entity_living":
            seen[value["entityId"]] = registries.entity(value["type"])
        elif name == "spawn_entity_painting":
            seen[value["entityId"]] = "minecraft:painting"
        elif name == "spawn_entity_experience_orb":
            seen[value["entityId"]] = "minecraft:xp_orb" if registries.protocol < 393 else "minecraft:experience_orb"
        elif name == "named_entity_spawn":
            seen[value["entityId"]] = "minecraft:player"
    return seen


def block_updates(registries, name, value):
    """{(x, y, z): block name} of one block_change or multi_block_change."""
    if name == "block_change":
        location = value["location"]
        return {(location["x"], location["y"], location["z"]): registries.block(value["type"])}
    seen = {}
    if "chunkX" in value:  # Up to 1.16.1: per-record horizontal position and Y.
        for entry in value["records"]:
            x = value["chunkX"] * 16 + (entry["horizontalPos"] >> 4)
            z = value["chunkZ"] * 16 + (entry["horizontalPos"] & 15)
            seen[(x, entry["y"], z)] = registries.block(entry["blockId"])
    else:  # 1.16.2+: a section position and packed VarLongs.
        section = value["chunkCoordinates"]
        for packed in value["records"]:
            state, local = packed >> 12, packed & 0xFFF
            x = section["x"] * 16 + (local >> 8 & 15)
            z = section["z"] * 16 + (local >> 4 & 15)
            y = section["y"] * 16 + (local & 15)
            seen[(x, y, z)] = registries.block(state)
    return seen


def blocks_seen(session, registries, since):
    seen = {}
    for record in session.packets({"block_change", "multi_block_change"}, since):
        seen.update(block_updates(registries, record.name, record.value))
    return seen


CHAT = {"chat", "system_chat", "player_chat", "profileless_chat"}


def chat_seen(session, since):
    texts = []
    for record in session.packets(CHAT, since):
        value = record.value
        for key in ("message", "content", "plainMessage", "signedChatContent", "unsignedChatContent",
                    "formattedMessage"):
            if value.get(key):
                texts.append(text_of(value[key]))
    return texts


def titles_seen(session, since):
    texts = []
    for record in session.packets({"title", "set_title_text", "set_title_subtitle", "action_bar"}, since):
        if record.value.get("text") is not None:
            texts.append(text_of(record.value["text"]))
    for record in session.packets("system_chat", since):
        if record.value.get("isActionBar") or record.value.get("type") == 2:
            texts.append(text_of(record.value.get("content")))
    return texts


def player_names(value):
    """Profile names anywhere in a player_info value."""
    names = []
    if isinstance(value, dict):
        for key, entry in value.items():
            if key == "name" and isinstance(entry, str):
                names.append(entry)
            else:
                names.extend(player_names(entry))
    elif isinstance(value, list):
        for entry in value:
            names.extend(player_names(entry))
    return names


def effect_names(session, registries, since, entity):
    names = []
    for record in session.packets("entity_effect", since):
        if record.value["entityId"] == entity:
            names.append(registries.name("mob_effect", record.value["effectId"]))
    return names


def sounds_seen(session, registries, since):
    names = []
    for record in session.packets({"sound_effect", "named_sound_effect", "entity_sound_effect"}, since):
        value = record.value
        if "soundName" in value:
            names.append("minecraft:" + value["soundName"].removeprefix("minecraft:"))
        sound = value.get("sound", value)
        if isinstance(sound, dict) and isinstance(sound.get("soundId"), int):
            names.append(registries.name("sound_event", sound["soundId"]))
        elif isinstance(sound, dict) and isinstance(sound.get("data"), dict):
            names.append(sound["data"].get("soundName") or sound["data"].get("resource") or "<inline sound>")
    return names


# --- The cell context ---------------------------------------------------------

class Context:
    """What a step sees: the server, both sessions, registries and findings."""

    def __init__(self, server, reference, translated, ref_registries, via_registries, base):
        self.server = server
        self.cmd = Commands(server.protocol, server.software)
        self.ref, self.via = reference, translated
        self.ref_reg, self.via_reg = ref_registries, via_registries
        self.base = base  # (x, y, z) block the players stand on top of.
        self.findings = []
        self.step = None
        self.marks = {}
        self.state = {}  # Shared between steps (positions of things built, IDs).

    # Findings

    def note(self, level, message, **details):
        self.findings.append(dict(level=level, step=self.step, message=message, **details))

    def info(self, message, **details):
        self.note("INFO", message, **details)

    def fail(self, message, **details):
        self.note("FAIL", message, **details)

    def scenario(self, message):
        """The reference client did not see what the step set up."""
        if self.server.software == "pumpkin":
            raise Skip(f"{self.server.software} did not produce it: {message}")
        raise StepFailure(f"SCENARIO (reference client, no translation involved): {message}")

    # Driving

    def console(self, *lines, pause=0.0):
        for line in lines:
            self.server.console(line)
        if pause:
            time.sleep(pause)

    def sync(self, timeout=10):
        """Wait until the server processed all console commands so far (and
        both clients received what they produced)."""
        token = f"rift-sync-{time.monotonic_ns()}"
        marks = (self.ref.mark(), self.via.mark())
        self.server.console(self.cmd.tellraw("@a", token))
        for session, mark in zip((self.ref, self.via), marks):
            session.wait(lambda record: record.name in CHAT and token.encode() in record.body,
                         f"console sync {token}", timeout, since=mark)

    def begin(self):
        self.marks = {"ref": self.ref.mark(), "via": self.via.mark(), "log": self.server.offset()}

    def since(self, who):
        return self.marks[who]

    def surface(self, dx, dy, dz):
        x, y, z = self.base
        return x + dx, y + dy, z + dz

    # Comparisons

    def compare_names(self, what, registry, reference, translated, source=None):
        """Every name the reference saw must reach the translated client: the
        same name if the client's version knows it, else any stand-in (INFO).
        IDs outside the client's registry fail. `source` is the protocol
        that names the reference entries (the server's by default)."""
        translated = list(translated)
        invalid = sorted({name for name in translated if name.startswith("<invalid")})
        if invalid:
            self.fail(f"{what}: IDs outside the client's registries: {invalid[:5]}")
        remaining = list(translated)
        for name in reference:
            target = matrix_registries.renamed(registry, name, source or self.ref.protocol, self.via.protocol)
            if target in remaining:
                remaining.remove(target)
            elif name.startswith("<"):
                continue
            elif target is None or not self.via_reg.knows(registry, target):
                self.info(f"{what}: {name} does not exist in the client's version; a stand-in is expected")
            else:
                self.fail(f"{what}: the translated client did not receive {name} "
                          f"(reference saw {sorted(reference)[:12]}, translated saw {sorted(translated)[:12]})")

    def expect_both(self, names, what, where=None, timeout=8):
        """Both clients receive a packet named one of `names` in this step."""
        values = []
        for who, session in (("ref", self.ref), ("via", self.via)):
            try:
                values.append(session.wait_packet(names, what, timeout, self.since(who), where).value)
            except SessionError:
                session.check()
                names_ = names if not isinstance(names, str) else {names}
                if any(record.name in names_ and record.error and "unverified" in record.error
                       for record in session.packets(None, self.since(who))):
                    raise Skip(f"{what}: the layout is unverified for protocol {session.protocol}") from None
                if who == "ref":
                    self.scenario(f"no {what}")
                raise StepFailure(f"the translated client did not receive {what}")
        return values


# --- Steps -------------------------------------------------------------------

PLAYER_SPAWNS = {"named_entity_spawn", "spawn_entity"}


def reappear(cx, timeout=12):
    """After a respawn or dimension change the server re-adds the translated
    player to the reference's view only once it moves: nudge it until the
    reference has seen it spawn again (since this step began)."""
    def visible():
        state = False
        for record in cx.ref.packets(PLAYER_SPAWNS | {"entity_destroy"}, cx.since("ref")):
            if record.name == "entity_destroy":
                if cx.via.entity_id in record.value.get("entityIds", []):
                    state = False
            elif record.value.get("entityId") == cx.via.entity_id:
                state = True
        return state
    deadline = time.monotonic() + timeout
    while not visible():
        cx.ref.check()
        if time.monotonic() > deadline:
            raise StepFailure("the reference client never saw the translated player again")
        cx.via.move(dx=0.0625)
        time.sleep(0.15)
        cx.via.move(dx=-0.0625)
        time.sleep(0.6)


@step(software=ALL)
def step_teleport(cx):
    """Teleport both players to the build area; positions must match."""
    # Let the join teleport's confirmation land first: Pumpkin kicks a client
    # whose confirmation arrives after a newer teleport ("Wrong teleport id").
    time.sleep(1)
    x, y, z = cx.base
    marks = (cx.ref.mark(), cx.via.mark())
    cx.console(f"tp {cx.ref.name} {x + 0.5} {y} {z + 0.5}", f"tp {cx.via.name} {x + 2.5} {y} {z + 0.5}")
    for session, mark, expected in zip((cx.ref, cx.via), marks, (x + 0.5, x + 2.5)):
        record = session.wait_packet("position", "teleport", 10, mark,
                                     where=lambda value, e=expected: abs(value["x"] - e) < 0.01
                                     or value.get("flags") not in (0, None))
        if abs(session.position[0] - expected) > 0.01 or abs(session.position[1] - y) > 0.01:
            raise StepFailure(f"{session.label} ended at {session.position[:3]}, expected x={expected} y={y}")
    # Blocks set in a chunk the client has not received yet arrive inside the
    # chunk data, not as block updates: wait for the chunks around the build.
    wanted = {((x >> 4) + cx_, (z >> 4) + cz) for cx_ in range(-1, 2) for cz in range(-1, 2)}
    for session in (cx.ref, cx.via):
        deadline = time.monotonic() + 20
        while not wanted <= session.chunks:
            session.check()
            if time.monotonic() > deadline:
                missing = sorted(wanted - session.chunks)
                if session is cx.ref:
                    cx.scenario(f"chunks {missing} never arrived")
                raise StepFailure(f"the translated client never received chunks {missing}")
            time.sleep(0.1)
    cx.sync()


@step(software=ALL)
def step_op(cx):
    """Make the translated player an operator; the client learns its op level."""
    cx.console(f"op {cx.via.name}")
    line = cx.server.wait_log(rf"(Made {cx.via.name} a server operator|Opped {cx.via.name})",
                              10, cx.since("log"))
    if line is None and cx.server.software != "pumpkin":
        cx.scenario("the server never confirmed the op")
    record = cx.via.wait_packet("entity_status", "op level entity status", 8, cx.since("via"),
                                where=lambda value: value.get("entityStatus") in (24, 25, 26, 27, 28)
                                and value["entityId"] == cx.via.entity_id) if cx.server.software != "pumpkin" else None
    if record is not None and record.value["entityStatus"] != 28:
        cx.fail(f"op level status {record.value['entityStatus']}, expected 28 (level 4)")
    cx.console(cx.cmd.gamerule("doMobSpawning", "spawn_mobs", "false"),
               cx.cmd.gamerule("doDaylightCycle", "advance_time", "false"),
               cx.cmd.gamerule("sendCommandFeedback", "send_command_feedback", "true"),
               cx.cmd.gamerule("keepInventory", "keep_inventory", "true"))


@step(software=ALL)
def step_blocks(cx):
    """Blocks with states and block entities via /setblock; names must match."""
    positions = {}
    for name, dx, dy, dz, block in BLOCKS:
        x, y, z = cx.surface(dx, dy, dz)
        positions[name] = (x, y, z)
        cx.console(cx.cmd.setblock(x, y, z, block))
    cx.state["blocks"] = positions
    cx.sync()
    # Block updates go out at the end of the tick, chat at once: wait for them.
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        ref = blocks_seen(cx.ref, cx.ref_reg, cx.since("ref"))
        via = blocks_seen(cx.via, cx.via_reg, cx.since("via"))
        if all(position in ref and position in via for position in positions.values()):
            break
        time.sleep(0.2)
    ref = blocks_seen(cx.ref, cx.ref_reg, cx.since("ref"))
    via = blocks_seen(cx.via, cx.via_reg, cx.since("via"))
    missing = [name for name, position in positions.items() if position not in ref]
    if missing:
        cx.scenario(f"no block updates for {missing}")
    flattens = cx.server.protocol < 393 <= cx.via.protocol
    # Across the flattening names differ legitimately (wooden_door is
    # oak_door; a 1.12 bed, banner, skull or flower pot takes its colour,
    # kind or plant from block entity data): expect what a server of the
    # client's own version places for the same block.
    crosses = (cx.server.protocol < 393) != (cx.via.protocol < 393)
    named = flattened_names(cx.via.protocol)
    client_cmd = Commands(named, cx.server.software)
    specs = {name: block for name, _, _, _, block in BLOCKS}
    for name, position in positions.items():
        if position not in via:
            cx.fail(f"no block update for {name} at {position}")
            continue
        expected = client_cmd.block_name(specs[name]) if crosses else ref[position]
        cx.compare_names(f"block {name}", "block_state", [expected], [via[position]],
                         source=named if crosses else None)
    entities = {"tile_entity_data", "block_entity_data"}
    ref_entities = cx.ref.packets(entities, cx.since("ref"))
    if not ref_entities:
        cx.scenario("no block entity data (sign, chest, banner, skull)")
    if cx.ref.protocol >= 757 and cx.via.protocol >= 757:
        # From 1.18 the packet names a block entity type; one the client's
        # version lacks (26.2 dropped beds') has nothing to arrive as.
        def known(record):
            name = cx.ref_reg.name("block_entity_type", record.value.get("action"))
            return name.startswith("<") or cx.via_reg.knows("block_entity_type", name)
        lacking = [record for record in ref_entities if not known(record)]
        if lacking:
            cx.info(f"{len(lacking)} block entity updates of types the client's version lacks")
        ref_entities = [record for record in ref_entities if known(record)]
    if flattens:
        # 1.13 made the plant part of the block: a 1.12 pot with a poppy is a
        # potted_poppy, and has no block entity any more.
        pot = positions["flower_pot"]
        ref_entities = [record for record in ref_entities
                        if (record.value or {}).get("location") != dict(zip("xyz", pot))]
    if len(cx.via.packets(entities, cx.since("via"))) < len(ref_entities):
        cx.fail("the translated client received fewer block entity updates than the reference")


def flattened_names(protocol):
    """The protocol whose names stand for 1.12's blocks and items in a
    client of `protocol` across the flattening: 1.13.2's from 1.13 on (the
    rules' renames carry them further: 1.12's stone_slab is 1.14's
    smooth_stone_slab, not its new stone_slab), the client's own before."""
    return min(protocol, 404) if protocol >= 393 else protocol


@step(software=ALL)
def step_items(cx):
    """Many kinds of items via /give; the inventory updates must name them."""
    given = [item for item in ITEMS if item["min"] <= cx.server.protocol
             and cx.server.software not in item.get("not_on", ())]
    for item in given:
        cx.console(cx.cmd.give(cx.ref.name, item), cx.cmd.give(cx.via.name, item))
    cx.sync()
    time.sleep(0.5)
    ref = items_seen(cx.ref, cx.ref_reg, cx.since("ref"))
    via = items_seen(cx.via, cx.via_reg, cx.since("via"))
    wanted = {cx.ref_reg.name("item", cx.ref_reg.id("item", item["key"])) for item in given
              if cx.ref_reg.id("item", item["key"]) is not None}
    absent = sorted(name for name in wanted if name not in ref)
    if absent:
        cx.scenario(f"/give produced no inventory update for {absent}")
    source = None
    if (cx.server.protocol < 393) != (cx.via.protocol < 393):
        # Across the flattening expect the item a server of the client's own
        # version gives for the same spec (banner 1 is red_banner).
        source = flattened_names(cx.via.protocol)
        client_cmd = Commands(source, cx.server.software)
        names = {cx.cmd.item_name(item): client_cmd.item_name(item) for item in given}
        ref = [names.get(name) or name for name in ref]
    cx.compare_names("inventory", "item", sorted(set(ref)), via, source=source)


@step(software=ALL)
def step_entities(cx):
    """Entities of many kinds (armour, names, passengers, leash, hanging)."""
    for label, entity, dx, dz, key in ENTITIES:
        x, y, z = cx.surface(dx, 0, dz)
        cx.console(cx.cmd.summon(entity(cx.cmd), x + 0.5, y, z + 0.5, entity_nbt(cx.cmd, key)))
    fence = cx.state.get("blocks", {}).get("fence")
    if fence and cx.server.protocol >= 393:
        leash = (f"[I;{fence[0]},{fence[1]},{fence[2]}]" if cx.server.protocol >= 766
                 else f"{{X:{fence[0]},Y:{fence[1]},Z:{fence[2]}}}")
        key = "leash" if cx.server.protocol >= 766 else "Leash"
        cx.console(cx.cmd.summon("pig", fence[0] + 1.5, fence[1], fence[2] + 0.5,
                                 f"{{NoAI:1b,{key}:{leash}}}"))
    cx.console(cx.cmd.effect("@e[type=minecraft:zombie]", "glowing", 100))
    cx.sync()
    time.sleep(1)
    ref = entity_types_seen(cx.ref, cx.ref_reg, cx.since("ref"))
    via = entity_types_seen(cx.via, cx.via_reg, cx.since("via"))
    # Only what the step summoned: natural spawns before doMobSpawning took
    # effect differ between two clients standing in different places.
    summoned = {f"minecraft:{entity(cx.cmd)}" for _, entity, _, _, _ in ENTITIES} | {"minecraft:chicken"}
    seen = sorted(name for name in set(ref.values()) if name in summoned)
    if len(seen) < 8:
        cx.scenario(f"only {seen} of the summoned entities spawned")
    cx.compare_names("spawned entities", "entity_type", seen, set(via.values()))
    for name in ("set_passengers", "entity_equipment", "entity_metadata"):
        if cx.ref.packets(name, cx.since("ref")) and not cx.via.packets(name, cx.since("via")):
            cx.fail(f"the translated client received no {name}")


@step(software=ALL)
def step_particles_and_sounds(cx):
    """Particles with data, and sounds; sound names must match."""
    x, y, z = cx.surface(2, 1, 2)
    for kind in ("heart", "dust", "block", "item"):
        cx.console(cx.cmd.particle(kind, x, y, z))
    cx.console(cx.cmd.playsound("entity.experience_orb.pickup", "@a", x, y, z),
               cx.cmd.playsound("block.note.harp" if cx.cmd.legacy else "block.note_block.harp", "@a", x, y, z))
    cx.sync()
    cx.expect_both({"world_particles"}, "particles")
    ref = sounds_seen(cx.ref, cx.ref_reg, cx.since("ref"))
    via = sounds_seen(cx.via, cx.via_reg, cx.since("via"))
    if not ref:
        cx.scenario("no sounds")
    cx.compare_names("sounds", "sound_event", ref, via)
    count = lambda session, who: len(session.packets("world_particles", cx.since(who)))
    if count(cx.via, "via") < count(cx.ref, "ref"):
        cx.fail(f"the translated client received {count(cx.via, 'via')} particle packets, "
                f"the reference {count(cx.ref, 'ref')}")


@step(software=ALL)
def step_explosion(cx):
    """A primed TNT explodes away from the build."""
    x, y, z = cx.surface(-6, 0, 10)
    cx.console(cx.cmd.summon("tnt", x, y, z, "{Fuse:0,fuse:0}"))
    cx.expect_both({"explosion"}, "explosion", timeout=10)


@step(software=ALL)
def step_weather_and_time(cx):
    """Rain, thunder, clear weather and the time of day."""
    # One at a time: the server reports weather changes from its tick, so
    # commands run in the same tick would cancel out.
    for command in ("weather rain", "weather thunder", "weather clear"):
        cx.begin()
        cx.console(command)
        cx.expect_both({"game_state_change"}, f"game state change after /{command}",
                       where=lambda value: value["reason"] not in ("change_game_mode", 3))
    cx.begin()
    cx.console("time set 6000")
    cx.expect_both({"update_time"}, "time update")


@step(software=ALL)
def step_world_border(cx):
    """World border size, centre and warning."""
    cx.console("worldborder set 2000", "worldborder center 0 0", "worldborder warning distance 7")
    cx.sync()
    names = {"world_border", "world_border_size", "world_border_center", "world_border_warning_reach",
             "world_border_lerp_size", "initialize_world_border", "world_border_warning_delay"}
    cx.expect_both(names, "world border update")


@step(software=ALL)
def step_titles(cx):
    """Title, subtitle and action bar text."""
    cx.console("title @a times 5 40 5", cx.cmd.title("@a", "subtitle", "RiftSub"), cx.cmd.title("@a", "title", "RiftTitle"),
               cx.cmd.title("@a", "actionbar", "RiftBar"))
    cx.sync()
    time.sleep(0.3)
    ref, via = titles_seen(cx.ref, cx.since("ref")), titles_seen(cx.via, cx.since("via"))
    for text in ("RiftTitle", "RiftSub", "RiftBar"):
        if text not in ref:
            cx.scenario(f"no title text {text} (saw {ref})")
        if text not in via:
            cx.fail(f"the translated client did not show {text!r} (saw {via})")


@step(min_server=393, software=ALL)
def step_boss_bar(cx):
    """A boss bar with a title and value."""
    cx.console("bossbar add rift:bar " + quote_json("RiftBoss"), "bossbar set rift:bar players @a",
               "bossbar set rift:bar value 50", "bossbar set rift:bar color red")
    cx.sync()
    for who, session in (("ref", cx.ref), ("via", cx.via)):
        titles = [text_of(record.value.get("title")) for record in session.packets("boss_bar", cx.since(who))
                  if record.value.get("title") is not None]
        if "RiftBoss" not in titles:
            if who == "ref":
                cx.scenario(f"no boss bar (titles {titles})")
            cx.fail(f"the translated client's boss bar titles are {titles}")


@step(software=ALL)
def step_scoreboard(cx):
    """A sidebar objective with a score, and a coloured team."""
    cx.console("scoreboard objectives add riftobj dummy", "scoreboard objectives setdisplay sidebar riftobj",
               f"scoreboard players set {cx.via.name} riftobj 7",
               cx.cmd.team("add", "riftteam"), cx.cmd.team_option("riftteam", "color", "red"),
               cx.cmd.team("join", "riftteam", cx.via.name))
    cx.sync()
    for who, session in (("ref", cx.ref), ("via", cx.via)):
        objectives = {record.value["name"] for record in session.packets("scoreboard_objective", cx.since(who))}
        teams = {record.value.get("team") for record in session.packets("teams", cx.since(who))}
        scores = session.packets({"scoreboard_score", "update_score"}, cx.since(who))
        problems = [what for what, ok in (("objective", "riftobj" in objectives),
                                         ("team", "riftteam" in teams), ("score", bool(scores))) if not ok]
        if problems:
            if who == "ref":
                cx.scenario(f"no {problems}")
            cx.fail(f"the translated client received no {problems}")


@step(software=ALL)
def step_tab_list(cx):
    """Both players are in the translated client's tab list.
    (Header and footer cannot be set from a vanilla console.)"""
    names = set()
    for record in cx.via.packets({"player_info", "player_info_update"}):
        names.update(player_names(record.value))
    missing = {cx.ref.name, cx.via.name} - names
    if missing:
        cx.fail(f"tab list lacks {sorted(missing)} (has {sorted(names)[:10]})")


@step(software=ALL)
def step_experience_and_health(cx):
    """Experience levels; damage in survival, then back to creative."""
    cx.console(cx.cmd.xp(cx.via.name, 5), cx.cmd.xp(cx.ref.name, 5))
    cx.sync()
    cx.expect_both({"experience"}, "experience", where=lambda value: value["level"] >= 5)
    cx.console(cx.cmd.gamemode(cx.via.name, "survival"), cx.cmd.gamemode(cx.ref.name, "survival"))
    cx.sync()
    cx.expect_both({"game_state_change"}, "game mode change",
                   where=lambda value: value["reason"] in ("change_game_mode", 3) and value["gameMode"] == 0)
    cx.console(cx.cmd.damage(cx.via.name, 3), cx.cmd.damage(cx.ref.name, 3))
    ref, via = cx.expect_both({"update_health"}, "damage", where=lambda value: value["health"] < 20)
    if abs(ref["health"] - via["health"]) > 0.01:
        cx.fail(f"health {via['health']} on the translated client, {ref['health']} on the reference")
    cx.console(cx.cmd.gamemode(cx.via.name, "creative"), cx.cmd.gamemode(cx.ref.name, "creative"),
               f"effect clear {cx.via.name}" if not cx.cmd.legacy else f"effect {cx.via.name} clear")
    cx.sync()


@step(software=ALL)
def step_effects(cx):
    """A potion effect on the player; effect names must match."""
    cx.console(cx.cmd.effect(cx.via.name, "speed", 60, 1), cx.cmd.effect(cx.ref.name, "speed", 60, 1))
    cx.sync()
    ref = effect_names(cx.ref, cx.ref_reg, cx.since("ref"), cx.ref.entity_id)
    via = effect_names(cx.via, cx.via_reg, cx.since("via"), cx.via.entity_id)
    if not ref:
        cx.scenario("no entity effect")
    cx.compare_names("effects", "mob_effect", ref, via)


@step(software=ALL)
def step_advancements_and_recipes(cx):
    """Every advancement and recipe granted."""
    cx.console(f"advancement grant {cx.via.name} everything", f"advancement grant {cx.ref.name} everything",
               f"recipe give {cx.via.name} *", f"recipe give {cx.ref.name} *")
    cx.sync(20)
    cx.expect_both({"advancements"}, "advancements", timeout=15)
    cx.expect_both({"unlock_recipes", "recipe_book_add"}, "recipe unlocks", timeout=15)


@step(software=ALL)
def step_tab_complete(cx):
    """A command suggestion request; the suggestions match the reference's."""
    def suggestions(session):
        mark = session.mark()
        if session.protocol < 393:
            session.send_packet("tab_complete", {"text": "/gamemo", "assumeCommand": False})
        else:
            # A 1.13 client leaves the slash out (its server would parse it
            # as part of the command); 1.13.1 servers skip one.
            text = "gamemo" if session.protocol == 393 else "/gamemo"
            session.send_packet("tab_complete", {"transactionId": 7, "text": text})
        record = session.wait_packet("tab_complete", "suggestions", 8, mark)
        return sorted(entry if isinstance(entry, str) else entry["match"] for entry in record.value["matches"])
    cx.console(f"op {cx.ref.name}")
    time.sleep(0.3)
    try:
        ref = suggestions(cx.ref)
    except SessionError:
        cx.ref.check()
        cx.scenario("the server answered no suggestion request")
    via = suggestions(cx.via)
    if not any("gamemode" in text for text in ref):
        cx.info(f"the server suggests {ref} for /gamemo even to the reference client")
    if [text.lstrip("/") for text in ref] != [text.lstrip("/") for text in via]:
        cx.fail(f"suggestions for /gamemo: translated {via}, reference {ref}")
    if cx.via.protocol >= 393 and not cx.via.packets("declare_commands"):
        cx.fail("the translated client never received the command tree")


@step(software=ALL)
def step_statistics(cx):
    """The statistics screen request."""
    mark = cx.via.mark()
    cx.via.send_packet("client_command", {"actionId": "request_stats"})
    cx.via.wait_packet("statistics", "statistics", 8, mark)


@step(min_server=401)
def step_dimension_change(cx):
    """To the nether and back with /execute in; respawn packets on both sides."""
    x, y, z = cx.base
    changes = cx.via.dimension_changes
    cx.console(f"execute in minecraft:the_nether run tp {cx.via.name} 0.5 100 0.5")
    cx.via.wait_packet("respawn", "respawn into the nether", 15, cx.since("via"))
    cx.via.wait_packet("position", "nether position", 15, cx.since("via"))
    time.sleep(1)
    mark = cx.via.mark()
    cx.console(f"execute in minecraft:overworld run tp {cx.via.name} {x + 2.5} {y} {z + 0.5}")
    cx.via.wait_packet("respawn", "respawn into the overworld", 15, mark)
    cx.via.wait_packet("position", "overworld position", 15, mark)
    if cx.via.dimension_changes < changes + 2:
        cx.fail("fewer than two dimension changes reached the translated client")
    reappear(cx)
    cx.sync()


@step()
def step_respawn(cx):
    """/kill in survival, then the client's respawn request."""
    x, y, z = cx.base
    cx.console(cx.cmd.gamemode(cx.via.name, "survival"))
    if cx.server.protocol >= 573:  # The rule exists from 1.15 on; off is the default.
        cx.console(cx.cmd.gamerule("doImmediateRespawn", "immediate_respawn", "false"))
    cx.console(f"kill {cx.via.name}")
    try:
        try:
            cx.via.wait_packet("update_health", "death", 10, cx.since("via"),
                               where=lambda value: value["health"] <= 0)
        except SessionError:
            cx.via.check()
            died = any(record.value.get("entityStatus") == 3 and record.value.get("entityId") == cx.via.entity_id
                       for record in cx.ref.packets("entity_status", cx.since("ref")))
            if not died:
                raise Skip("the server logged the kill but sent no death to either client") from None
            raise StepFailure("the reference saw the translated player die; the translated client did not")
        mark = cx.via.mark()
        cx.via.send_packet("client_command", {"actionId": "perform_respawn"})
        cx.via.wait_packet("respawn", "respawn", 10, mark)
        cx.via.wait_packet("update_health", "health after respawn", 10, mark,
                           where=lambda value: value["health"] > 0)
    finally:
        # Later steps need a creative player at the build, whatever happened.
        mark = cx.via.mark()
        cx.console(cx.cmd.gamemode(cx.via.name, "creative"), f"tp {cx.via.name} {x + 2.5} {y} {z + 0.5}")
        cx.via.wait_packet("position", "the teleport back", 10, mark)
    reappear(cx)
    cx.sync()


def creative_slot(cx, slot, item):
    """Put `item` into an inventory slot with a creative inventory action."""
    registries = cx.via_reg
    item_id = registries.id("item", item)
    if item_id is None:
        raise Skip(f"{item} has no ID in the client's version")
    # Every era's field names at once; the encoder writes those the client's
    # schema has (1.12 blockId/itemDamage, 1.13.2 present, 1.20.5 components).
    value = {"present": True, "blockId": item_id, "itemId": item_id, "itemCount": 1, "itemDamage": 0}
    cx.via.send_packet("set_creative_slot", {"slot": slot, "item": value})


def held_count(cx, item):
    """How many `item` the translated player has, from the server's /clear."""
    offset = cx.server.offset()
    pattern = cx.cmd.clear_count_pattern(cx.via.name, item)
    cx.console(cx.cmd.clear_count(cx.via.name, item))
    line = cx.server.wait_log(f"{pattern}|No items were found|Could not clear", 8, offset)
    if line is None:
        cx.scenario(f"the server did not answer /clear for {item}")
    match = re.search(pattern, line)
    return int(match.group(1)) if match else 0


@step(software=JAVA)
def step_creative_inventory(cx):
    """Serverbound creative inventory action: an emerald in the hotbar."""
    cx.console(f"clear {cx.via.name}")
    cx.sync()
    creative_slot(cx, 36, "emerald")
    time.sleep(0.5)
    count = held_count(cx, "emerald")
    if count != 1:
        cx.fail(f"the server holds {count} emeralds for the translated player, expected 1")


@step(software=JAVA)
def step_held_item_and_swing(cx):
    """Held item change and arm swing, as the reference client sees them."""
    creative_slot(cx, 37, "diamond_sword")
    time.sleep(0.3)
    mark = cx.ref.mark()
    cx.via.send_packet("held_item_slot", {"slotId": 1})
    sword = cx.ref_reg.id("item", "diamond_sword")

    def holds_sword(value):
        if value["entityId"] != cx.via.entity_id:
            return False
        entries = value.get("equipments") or [{"slot": value.get("slot"), "item": value.get("item")}]
        return any(entry["slot"] == 0 and (slot_item(cx.ref_reg, entry["item"]) or ("",))[0]
                   == cx.ref_reg.name("item", sword) for entry in entries)
    cx.ref.wait_packet("entity_equipment", "the translated player's held sword", 8, mark, where=holds_sword)
    mark = cx.ref.mark()
    # 26.3 replaced the swing packet with an empty punch packet.
    swing = "arm_animation" if cx.via.has("arm_animation") else "punch"
    cx.via.send_packet(swing, {"hand": 0})
    cx.ref.wait_packet({"animation", "swing_animation"}, "the translated player's arm swing", 8, mark,
                       where=lambda value: value["entityId"] == cx.via.entity_id)


@step(software=ALL)
def step_movement(cx):
    """The translated player walks; the reference client sees it move."""
    mark = cx.ref.mark()
    for _ in range(6):
        cx.via.move(dx=0.25)
        time.sleep(0.12)  # One position per tick at most (26.3 kicks otherwise).
    moves = {"rel_entity_move", "entity_move_look", "entity_teleport", "sync_entity_position"}
    cx.ref.wait_packet(moves, "the translated player's movement", 8, mark,
                       where=lambda value: value.get("entityId") == cx.via.entity_id)


def block_face_packet(cx, position, face):
    x, y, z = position
    value = {"location": {"x": x, "y": y, "z": z}, "direction": face, "hand": 0,
             "cursorX": 0.5, "cursorY": 0.5, "cursorZ": 0.5, "sequence": int(time.monotonic() * 10) % 100000}
    if cx.via.protocol < 477:
        value.update(cursorX=0.5, cursorY=1.0, cursorZ=0.5)
    cx.via.send_packet("block_place", value)


@step(software=JAVA)
def step_break_and_place(cx):
    """Creative block break and placement; the reference sees both."""
    target = cx.state.get("blocks", {}).get("break_target")
    if target is None:
        raise Skip("needs the blocks step")
    x, y, z = target
    # Creative players cannot break blocks while holding a sword.
    cx.via.send_packet("held_item_slot", {"slotId": 8})
    time.sleep(0.2)
    mark = cx.ref.mark()
    cx.via.send_packet("block_dig", {"status": 0, "location": {"x": x, "y": y, "z": z}, "face": 1,
                                     "sequence": 1})
    cx.ref.wait_packet({"block_change", "multi_block_change"}, "the broken block", 8, mark,
                       where=lambda value: blocks_seen_value(cx, value).get(target) == "minecraft:air")
    creative_slot(cx, 38, "stone")
    cx.via.send_packet("held_item_slot", {"slotId": 2})
    time.sleep(0.3)
    mark = cx.ref.mark()
    block_face_packet(cx, (x, y - 1, z), 1)  # On top of the ground where the gold was.
    cx.ref.wait_packet({"block_change", "multi_block_change"}, "the placed block", 8, mark,
                       where=lambda value: blocks_seen_value(cx, value).get(target) == "minecraft:stone")


def blocks_seen_value(cx, value):
    return block_updates(cx.ref_reg, "block_change" if "location" in value else "multi_block_change", value)


@step(software=JAVA)
def step_containers(cx):
    """Open, click and close containers; the chest's diamonds move to the
    player's inventory."""
    blocks = cx.state.get("blocks")
    if not blocks:
        raise Skip("needs the blocks step")
    x, y, z = cx.base
    # Within reach of every container (1.12 allows 8 blocks, 1.20.5+ about 6).
    cx.console(f"clear {cx.via.name}", f"tp {cx.via.name} {x + 7.5} {y} {z + 7.5}")
    cx.sync()
    time.sleep(0.5)
    cx.via.send_packet("held_item_slot", {"slotId": 8})  # An empty hand opens containers.
    opened = {}
    for name in CONTAINERS:
        mark = cx.via.mark()
        block_face_packet(cx, blocks[name], 1)
        try:
            record = cx.via.wait_packet("open_window", f"{name} window", 6, mark)
        except SessionError:
            cx.via.check()
            cx.fail(f"{name} did not open")
            continue
        window = record.value["windowId"]
        opened[name] = record.value.get("inventoryType")
        if name == "chest":
            cx.via.wait_packet("window_items", "chest contents", 6, mark,
                               where=lambda value: value["windowId"] == window)
            state = max([r.value.get("stateId", 0) for r in cx.via.packets({"window_items", "set_slot"}, mark)
                         if r.value.get("windowId") == window] or [0])
            click = {"windowId": window, "stateId": state, "slot": 0, "mouseButton": 0, "mode": 0,
                     "action": 1}
            cx.via.send_packet("window_click", click)  # Pick up the diamonds.
            time.sleep(0.3)
            click.update(slot=27 + 9, action=2)  # An empty player slot below the chest's 27.
            cx.via.send_packet("window_click", click)
            time.sleep(0.3)
        cx.via.send_packet("close_window", {"windowId": window})
        time.sleep(0.2)
    cx.info(f"opened {opened}")
    cx.console(f"tp {cx.via.name} {x + 2.5} {y} {z + 0.5}")
    if "chest" in opened:
        count = held_count(cx, "diamond")
        if count != 3:
            cx.fail(f"after moving the chest's diamonds the player holds {count}, expected 3")


@step(software=JAVA)
def step_merchant(cx):
    """Right-click a villager with trades; the offers arrive."""
    x, y, z = cx.surface(1, 0, 4)
    count = cx.cmd.count
    if cx.cmd.legacy:
        nbt = ('{NoAI:1b,Invulnerable:1b,Profession:0,Career:1,CareerLevel:1,Offers:{Recipes:[{buy:'
               '{id:"minecraft:emerald",Count:1b},sell:{id:"minecraft:diamond",Count:1b},maxUses:10}]}}')
    else:
        data = ('VillagerData:{profession:"minecraft:armorer",level:2,type:"minecraft:plains"}'
                if cx.server.protocol >= 477 else "Profession:3,Career:1,CareerLevel:1")
        nbt = (f'{{NoAI:1b,Invulnerable:1b,{data},Offers:{{Recipes:[{{buy:{{id:"minecraft:emerald",'
               f'{count(1)}}},sell:{{id:"minecraft:diamond",{count(1)}}},maxUses:10}}]}}}}')
    mark = cx.via.mark()
    cx.console(cx.cmd.summon("villager", x + 0.5, y, z + 0.5, nbt))
    cx.sync()
    spawned = entity_types_seen(cx.via, cx.via_reg, mark)
    villager = next((entity for entity, kind in spawned.items() if kind.endswith("villager")), None)
    if villager is None:
        raise StepFailure(f"no villager spawned on the translated client ({spawned})")
    mark = cx.via.mark()
    cx.via.send_packet("use_entity", {"target": villager, "mouse": 0, "hand": 0, "sneaking": False})
    cx.via.wait_packet("open_window", "merchant window", 8, mark)
    trades = cx.via.wait(lambda record: record.name == "trade_list" or (
        record.name == "custom_payload" and record.value
        and record.value.get("channel") in ("MC|TrList", "minecraft:trader_list")),
        "trade offers", 8, mark)
    del trades
    window = cx.via.packets("open_window", mark)[-1].value["windowId"]
    cx.via.send_packet("close_window", {"windowId": window})


@step(software=JAVA)
def step_map(cx):
    """Use an empty map; the server creates it and sends map data."""
    creative_slot(cx, 39, "map")
    cx.via.send_packet("held_item_slot", {"slotId": 3})
    time.sleep(0.3)
    mark = cx.via.mark()
    cx.via.send_packet("use_item", {"hand": 0, "sequence": 7})
    cx.via.wait_packet("map", "map data", 10, mark)


@step(software=ALL)
def step_chat(cx):
    """A chat message from the translated client reaches the log and the
    reference client."""
    text = f"rift chat {time.monotonic_ns() % 100000}"
    mark = cx.ref.mark()
    if cx.via.has("chat"):
        cx.via.send_packet("chat", {"message": text})
    else:
        cx.via.send_packet("chat_message", {"message": text, "timestamp": int(time.time() * 1000),
                                            "salt": 1})
    cx.ref.wait(lambda record: record.name in CHAT and text.encode() in record.body,
                "the translated player's chat message", 8, mark)
    if cx.server.wait_log(re.escape(text), 5, cx.since("log")) is None:
        cx.fail("the chat message is missing from the server log")


def send_chat(session, text):
    if session.has("chat"):
        session.send_packet("chat", {"message": text})
    else:
        session.send_packet("chat_message", {"message": text, "timestamp": int(time.time() * 1000),
                                             "salt": 1})


@step(software=JAVA)
def step_chat_received(cx):
    """Player chat reaches the translated client: its own message echoed by
    the server and a message of the reference player (signed-chat formats
    the client's version cannot verify arrive as other chat packets)."""
    for sender, what in ((cx.via, "its own chat message"), (cx.ref, "the reference player's chat message")):
        text = f"rift heard {time.monotonic_ns() % 100000}"
        mark = cx.via.mark()
        send_chat(sender, text)
        cx.via.wait(lambda record: record.name in CHAT and text.encode() in record.body, what, 8, mark)


@step(software=ALL)
def step_command(cx):
    """A command from the translated client runs on the server."""
    text = f"rift-cmd-{time.monotonic_ns() % 100000}"
    command = f"tellraw @a {quote_json(text)}"
    mark = cx.ref.mark()
    if cx.via.has("chat_command"):
        cx.via.send_packet("chat_command", {"command": command, "timestamp": int(time.time() * 1000)})
    else:
        cx.via.send_packet("chat", {"message": "/" + command})
    cx.ref.wait(lambda record: record.name in CHAT and text.encode() in record.body,
                "output of the translated player's command", 8, mark)


@step(software=JAVA)
def step_client_settings(cx):
    """New client settings (skin parts) show in the player's metadata."""
    mark = cx.ref.mark()
    state = "play"
    if cx.via.protocol >= 764 and not cx.via.has("settings"):
        raise Skip("no play-state settings packet")
    cx.via.send_packet("settings", {"locale": "en_us", "viewDistance": 3, "chatFlags": 0, "chatColors": True,
                                    "skinParts": 0x7E, "mainHand": 1, "enableServerListing": True}, state)

    def skin(value):
        return value["entityId"] == cx.via.entity_id and any(
            entry.get("value") == 0x7E for entry in value.get("metadata", []))
    cx.ref.wait_packet("entity_metadata", "the translated player's new skin parts", 8, mark, where=skin)


@step(software=ALL)
def step_plugin_message(cx):
    """A minecraft:brand plugin message in play (no visible effect on vanilla)."""
    brand = "minecraft:brand" if cx.via.protocol >= 393 else "MC|Brand"
    cx.via.send_packet("custom_payload", {"channel": brand, "data": b"\x09rift-test"})
    time.sleep(0.5)
    cx.via.check()
    cx.info("brand sent; the session stayed up")


@step(software=ALL)
def step_idle(cx):
    """Idle until both clients answered at least two keep-alives, and for a few
    seconds more (time updates, entity ticks)."""
    # Keep-alives come every 15 s of server time; a loaded machine stretches that.
    deadline = time.monotonic() + 90
    while min(cx.ref.keepalives, cx.via.keepalives) < 2:
        cx.ref.check()
        cx.via.check()
        if time.monotonic() > deadline:
            raise StepFailure(f"keep-alives: reference {cx.ref.keepalives}, translated {cx.via.keepalives}")
        time.sleep(0.25)
    mark = cx.via.mark()
    time.sleep(3)
    cx.via.check()
    if not cx.via.packets("update_time", mark) and cx.server.software != "pumpkin":
        cx.fail("no time updates while idle")


STEP_NAMES = [entry.name for entry in STEPS]
