#!/usr/bin/env python3
"""Capture real server traffic as test vectors for the translation codecs.

For each requested pinned vanilla server this joins an offline test client
directly, runs console commands that produce the packet kinds translation has
to understand (items with tags or components, entities with metadata and
equipment, block entities, particles, sounds, titles, scoreboards, boss bars,
statistics, recipes, advancements, tags, chunks), and writes every packet of
both directions to the shared cache:

    ~/.cache/rift-via/corpus/<protocol>.jsonl

One JSON object per line: {"d": "c"|"s", "s": "login"|"configuration"|"play",
"id": <packet id>, "hex": "<packet body without the id>"}; "c" is clientbound.
Codec tests read whole sessions from there (and skip when a file is absent),
so a codec is checked against what official servers really send rather than
against hand-built packets alone.

    python3 tests/translation_corpus.py --accept-eula                 # every protocol 1.12-26.3
    python3 tests/translation_corpus.py --accept-eula --server vanilla-1.16.5
    python3 tests/translation_corpus.py --accept-eula --force         # recapture

Requires the fixtures' Java runtimes (see docs/network-protocol.md).
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time

from minecraft import (PROTOCOLS, SERVERS, configure_server, play, process, status_ready,
                       unused_port, wait_ready)

CORPUS = Path(os.environ.get("RIFT_TRANSLATION_CORPUS", Path.home() / ".cache/rift-via/corpus"))
JDKS = Path.home() / ".cache/rift-via/jdks"
LOCKS = Path.home() / ".cache/rift-via/locks"
NAME = "Corpus"


class Done(Exception):
    pass


def fixtures():
    """The newest vanilla fixture of every protocol from 1.12 on."""
    newest = {}
    for name, fixture in SERVERS.items():
        if name == "vanilla" or name.startswith("vanilla-"):
            if fixture["protocol"] >= 335:
                current = newest.get(fixture["protocol"])
                if current is None or version_key(fixture["version"]) > version_key(
                        SERVERS[current]["version"]):
                    newest[fixture["protocol"]] = name
    return [newest[protocol] for protocol in sorted(newest)]


def version_key(version):
    return [int(part) for part in version.split(".")]


def commands(protocol):
    """Console commands in every syntax era; a server ignores what it rejects."""
    at = (lambda command: f"execute {NAME} ~ ~ ~ {command}") if protocol < 393 else (
        lambda command: f"execute at {NAME} run {command}")
    lines = []
    if protocol < 393:
        lines += [
            f'give {NAME} minecraft:diamond_sword 1 0 {{ench:[{{id:16,lvl:5}}],display:{{Name:"Blade",Lore:["first","second"]}}}}',
            f"give {NAME} minecraft:wool 3 14",
            f"give {NAME} minecraft:potion 1 0 {{Potion:\"minecraft:swiftness\"}}",
            f'give {NAME} minecraft:written_book 1 0 {{title:"T",author:"A",pages:["{{\\"text\\":\\"page\\"}}"]}}',
            f"give {NAME} minecraft:banner 1 14 {{BlockEntityTag:{{Base:14,Patterns:[{{Pattern:\"cre\",Color:0}}]}}}}",
            f'give {NAME} minecraft:purple_shulker_box 1 0 {{BlockEntityTag:{{Items:[{{Slot:0b,id:"minecraft:stone",Count:3b,Damage:1s}}]}}}}',
            f"give {NAME} minecraft:skull 1 3 {{SkullOwner:\"Notch\"}}",
            f"give {NAME} minecraft:fireworks 1 0 {{Fireworks:{{Flight:1b,Explosions:[{{Type:1b,Colors:[I;255]}}]}}}}",
            f"give {NAME} minecraft:filled_map 1 0",
            at('summon zombie ~2 ~ ~ {CustomName:"Bob",CustomNameVisible:1b,NoAI:1b,ArmorItems:[{},{},{},{id:"minecraft:diamond_helmet",Count:1b}],HandItems:[{id:"minecraft:iron_sword",Count:1b},{}]}'),
            at("summon armor_stand ~-2 ~ ~ {ShowArms:1b}"),
            at('summon item ~ ~ ~2 {Item:{id:"minecraft:apple",Count:2b}}'),
            at("summon villager ~3 ~ ~ {NoAI:1b,Profession:2}"),
            at("summon cow ~ ~ ~3 {NoAI:1b}"),
            at("summon falling_block ~ ~5 ~4 {Block:\"minecraft:sand\",Time:1}"),
            at("summon boat ~4 ~ ~"),
            at("summon minecart ~4 ~ ~2"),
            at("summon xp_orb ~ ~ ~-2 {Value:5}"),
            at("summon item_frame ~ ~1 ~-3 {Item:{id:\"minecraft:clock\",Count:1b},Facing:0b}"),
            at("summon ender_crystal ~-4 ~ ~"),
            at('setblock ~2 ~ ~2 chest 0 replace {Items:[{Slot:0b,id:"minecraft:stone",Count:3b,Damage:1s}]}'),
            at('setblock ~3 ~ ~2 standing_sign 0 replace {Text1:"{\\"text\\":\\"hello\\"}"}'),
            at("setblock ~4 ~ ~2 oak_stairs 3"),
            at("setblock ~5 ~ ~2 wooden_door 0"),
            at("setblock ~6 ~ ~2 redstone_wire 5"),
            at("setblock ~2 ~ ~3 skull 1 replace {SkullType:3b,Owner:{Name:\"Notch\"}}"),
            at("setblock ~3 ~ ~3 flower_pot 0 replace {Item:\"minecraft:red_flower\",Data:0}"),
            at("setblock ~4 ~ ~3 sticky_piston 1"),
            at("setblock ~5 ~ ~3 standing_banner 0 replace {Base:1}"),
            at("setblock ~6 ~ ~3 bed 0"),
            at("setblock ~2 ~ ~4 mob_spawner 0"),
            at("particle reddust ~ ~1 ~ 0 0 0 1 5"),
            at("particle blockcrack ~ ~1 ~ 0 0 0 1 5 normal @a 1"),
            at("particle iconcrack ~ ~1 ~ 0 0 0 1 5 normal @a 264 0"),
            at("playsound minecraft:entity.pig.ambient master @a ~ ~ ~"),
            f"title {NAME} times 5 20 5",
            f'title {NAME} subtitle {{"text":"sub","color":"red"}}',
            f'title {NAME} title {{"text":"Title","bold":true}}',
            f'title {NAME} actionbar {{"text":"bar"}}',
            f"effect {NAME} minecraft:speed 30 1",
            f"xp 5L {NAME}",
            f"gamemode 0 {NAME}",
            f"gamemode 1 {NAME}",
            f"advancement grant {NAME} only minecraft:story/root",
            f"recipe give {NAME} *",
            "scoreboard objectives add kills dummy Kills",
            "scoreboard objectives setdisplay sidebar kills",
            f"scoreboard players set {NAME} kills 7",
            "scoreboard teams add red Red",
            "scoreboard teams option red color red",
            f"scoreboard teams join red {NAME}",
            "weather rain 60",
            "time set 6000",
            "worldborder set 500",
            f"tellraw {NAME} {{\"text\":\"raw\",\"color\":\"gold\",\"hoverEvent\":{{\"action\":\"show_text\",\"value\":\"hover\"}},\"clickEvent\":{{\"action\":\"run_command\",\"value\":\"/help\"}}}}",
            "say hello from console",
        ]
    else:
        lines += [
            # NBT tags (1.13-1.20.4).
            f"give {NAME} minecraft:diamond_sword{{Enchantments:[{{id:\"minecraft:sharpness\",lvl:5}}],display:{{Name:'{{\"text\":\"Blade\"}}',Lore:['\"first\"','\"second\"']}}}} 1",
            f"give {NAME} minecraft:potion{{Potion:\"minecraft:swiftness\"}} 1",
            f"give {NAME} minecraft:written_book{{title:\"T\",author:\"A\",pages:['{{\"text\":\"page\"}}']}} 1",
            f"give {NAME} minecraft:red_banner{{BlockEntityTag:{{Patterns:[{{Pattern:\"cre\",Color:0}}]}}}} 1",
            f"give {NAME} minecraft:purple_shulker_box{{BlockEntityTag:{{Items:[{{Slot:0b,id:\"minecraft:stone\",Count:3b}}]}}}} 1",
            f"give {NAME} minecraft:player_head{{SkullOwner:\"Notch\"}} 1",
            f"give {NAME} minecraft:firework_rocket{{Fireworks:{{Flight:1b,Explosions:[{{Type:1b,Colors:[I;255]}}]}}}} 1",
            f"give {NAME} minecraft:bundle{{Items:[{{id:\"minecraft:stone\",Count:3b}}]}} 1",
            # Data components (1.20.5+), JSON and SNBT text syntaxes.
            f"give {NAME} minecraft:diamond_sword[enchantments={{levels:{{\"minecraft:sharpness\":5}}}},custom_name='{{\"text\":\"Blade\"}}',lore=['\"first\"','\"second\"']] 1",
            f"give {NAME} minecraft:diamond_sword[enchantments={{\"minecraft:sharpness\":5}},custom_name={{text:\"Blade\",color:\"red\"}},lore=[\"first\",{{text:\"second\"}}]] 1",
            f"give {NAME} minecraft:potion[potion_contents={{potion:\"minecraft:swiftness\"}}] 1",
            f"give {NAME} minecraft:written_book[written_book_content={{title:\"T\",author:\"A\",pages:['\"page\"']}}] 1",
            f"give {NAME} minecraft:written_book[written_book_content={{title:\"T\",author:\"A\",pages:[\"page\"]}}] 1",
            f"give {NAME} minecraft:red_banner[banner_patterns=[{{pattern:\"minecraft:creeper\",color:\"white\"}}]] 1",
            f"give {NAME} minecraft:purple_shulker_box[container=[{{slot:0,item:{{id:\"minecraft:stone\",count:3}}}}]] 1",
            f"give {NAME} minecraft:player_head[profile=\"Notch\"] 1",
            f"give {NAME} minecraft:firework_rocket[fireworks={{flight_duration:1,explosions:[{{shape:\"large_ball\",colors:[I;255]}}]}}] 1",
            f"give {NAME} minecraft:bundle[bundle_contents=[{{id:\"minecraft:stone\",count:3}}]] 1",
            f"give {NAME} minecraft:golden_apple[food={{nutrition:4,saturation:1.0,can_always_eat:true}},max_stack_size=16,rarity=\"epic\"] 2",
            f"give {NAME} minecraft:stick[attribute_modifiers=[{{type:\"minecraft:attack_damage\",id:\"rift:test\",amount:3.0,operation:\"add_value\",slot:\"mainhand\"}}],unbreakable={{}}] 1",
            f"give {NAME} minecraft:crossbow[charged_projectiles=[{{id:\"minecraft:arrow\",count:1}}]] 1",
            f"give {NAME} minecraft:red_wool 3",
            f"give {NAME} minecraft:filled_map 1",
            f"give {NAME} minecraft:shield 1",
            f"give {NAME} minecraft:enchanted_book 1",
            at('summon minecraft:zombie ~2 ~ ~ {CustomName:\'{"text":"Bob"}\',CustomNameVisible:1b,NoAI:1b,ArmorItems:[{},{},{},{id:"minecraft:diamond_helmet",Count:1b}],HandItems:[{id:"minecraft:iron_sword",Count:1b},{}]}'),
            at('summon minecraft:zombie ~2 ~ ~1 {CustomName:"Bob",CustomNameVisible:1b,NoAI:1b,equipment:{head:{id:"minecraft:diamond_helmet",count:1},mainhand:{id:"minecraft:iron_sword",count:1}}}'),
            at("summon minecraft:armor_stand ~-2 ~ ~ {ShowArms:1b}"),
            at('summon minecraft:item ~ ~ ~2 {Item:{id:"minecraft:apple",Count:2b}}'),
            at('summon minecraft:item ~ ~ ~2 {Item:{id:"minecraft:apple",count:2}}'),
            at("summon minecraft:villager ~3 ~ ~ {NoAI:1b}"),
            at("summon minecraft:cow ~ ~ ~3 {NoAI:1b}"),
            at('summon minecraft:falling_block ~ ~5 ~4 {BlockState:{Name:"minecraft:sand"},Time:1}'),
            at("summon minecraft:boat ~4 ~ ~"),
            at("summon minecraft:oak_boat ~4 ~ ~1"),
            at("summon minecraft:minecart ~4 ~ ~2"),
            at("summon minecraft:experience_orb ~ ~ ~-2 {Value:5}"),
            at('summon minecraft:item_frame ~ ~1 ~-3 {Item:{id:"minecraft:clock",Count:1b},Facing:0b}'),
            at('summon minecraft:item_frame ~1 ~1 ~-3 {Item:{id:"minecraft:clock",count:1},Facing:0b}'),
            at("summon minecraft:end_crystal ~-4 ~ ~"),
            at("summon minecraft:painting ~-3 ~1 ~-3"),
            at('summon minecraft:text_display ~ ~2 ~ {text:\'{"text":"display"}\'}'),
            at('summon minecraft:text_display ~1 ~2 ~ {text:{text:"display"}}'),
            at('summon minecraft:block_display ~ ~3 ~ {block_state:{Name:"minecraft:oak_stairs"}}'),
            at('summon minecraft:item_display ~ ~4 ~ {item:{id:"minecraft:apple",count:1}}'),
            at("summon minecraft:axolotl ~-3 ~ ~2 {NoAI:1b}"),
            at("summon minecraft:frog ~-3 ~ ~3 {NoAI:1b}"),
            at("summon minecraft:cat ~-3 ~ ~4 {NoAI:1b}"),
            at("summon minecraft:wolf ~-4 ~ ~4 {NoAI:1b}"),
            at("summon minecraft:sheep ~-5 ~ ~4 {NoAI:1b,Color:14b}"),
            at('setblock ~2 ~ ~2 minecraft:chest{Items:[{Slot:0b,id:"minecraft:stone",Count:3b}]}'),
            at('setblock ~2 ~ ~2 minecraft:chest{Items:[{Slot:0b,id:"minecraft:stone",count:3}]}'),
            at('setblock ~3 ~ ~2 minecraft:sign{Text1:\'{"text":"hello"}\'}'),
            at('setblock ~3 ~ ~2 minecraft:oak_sign{Text1:\'{"text":"hello"}\'}'),
            at('setblock ~3 ~ ~2 minecraft:oak_sign{front_text:{messages:[\'{"text":"hello"}\',\'""\',\'""\',\'""\']}}'),
            at('setblock ~3 ~ ~2 minecraft:oak_sign{front_text:{messages:[{text:"hello"},"","",""]}}'),
            at("setblock ~4 ~ ~2 minecraft:oak_stairs[facing=east,half=top,waterlogged=true]"),
            at("setblock ~4 ~ ~2 minecraft:oak_stairs[facing=east,half=top]"),
            at("setblock ~5 ~ ~2 minecraft:oak_door[half=lower]"),
            at("setblock ~6 ~ ~2 minecraft:redstone_wire[power=5]"),
            at("setblock ~2 ~ ~3 minecraft:player_head{SkullOwner:\"Notch\"}"),
            at("setblock ~2 ~ ~3 minecraft:player_head{profile:\"Notch\"}"),
            at("setblock ~3 ~ ~3 minecraft:potted_poppy"),
            at("setblock ~4 ~ ~3 minecraft:sticky_piston[facing=up]"),
            at("setblock ~5 ~ ~3 minecraft:white_banner"),
            at("setblock ~6 ~ ~3 minecraft:red_bed"),
            at("setblock ~2 ~ ~4 minecraft:spawner"),
            at("setblock ~3 ~ ~4 minecraft:lectern"),
            at("setblock ~4 ~ ~4 minecraft:beehive"),
            at("setblock ~5 ~ ~4 minecraft:copper_bulb"),
            at("setblock ~6 ~ ~4 minecraft:decorated_pot"),
            at("setblock ~2 ~ ~5 minecraft:sculk_sensor"),
            at("setblock ~3 ~ ~5 minecraft:campfire"),
            at("setblock ~4 ~ ~5 minecraft:respawn_anchor"),
            at("setblock ~5 ~ ~5 minecraft:candle[candles=3,lit=true]"),
            at("setblock ~6 ~ ~5 minecraft:cherry_leaves"),
            at("fill ~-6 ~ ~-6 ~-4 ~1 ~-4 minecraft:glass"),
            at("particle minecraft:dust 1 0 0 1 ~ ~1 ~ 0 0 0 1 5"),
            at("particle minecraft:dust{color:[1.0,0.0,0.0],scale:1} ~ ~1 ~ 0 0 0 1 5"),
            at("particle minecraft:block minecraft:stone ~ ~1 ~ 0 0 0 1 5"),
            at('particle minecraft:block{block_state:"minecraft:stone"} ~ ~1 ~ 0 0 0 1 5'),
            at("particle minecraft:item minecraft:apple ~ ~1 ~ 0 0 0 1 5"),
            at('particle minecraft:item{item:"minecraft:apple"} ~ ~1 ~ 0 0 0 1 5'),
            at("particle minecraft:flame ~ ~1 ~ 0 0 0 1 5"),
            at("playsound minecraft:entity.pig.ambient master @a ~ ~ ~"),
            f"title {NAME} times 5 20 5",
            f'title {NAME} subtitle {{"text":"sub","color":"red"}}',
            f'title {NAME} title {{"text":"Title","bold":true}}',
            f'title {NAME} actionbar {{"text":"bar"}}',
            f"effect give {NAME} minecraft:speed 30 1",
            f"experience add {NAME} 5 levels",
            f"gamemode survival {NAME}",
            f"gamemode creative {NAME}",
            f"advancement grant {NAME} only minecraft:story/root",
            f"recipe give {NAME} *",
            "scoreboard objectives add kills dummy \"Kills\"",
            'scoreboard objectives add kills2 dummy {"text":"Kills"}',
            "scoreboard objectives setdisplay sidebar kills",
            f"scoreboard players set {NAME} kills 7",
            "team add red \"Red\"",
            'team add red2 {"text":"Red"}',
            "team modify red color red",
            f"team join red {NAME}",
            'bossbar add rift:bar "Boss"',
            'bossbar add rift:bar2 {"text":"Boss"}',
            f"bossbar set rift:bar players {NAME}",
            "bossbar set rift:bar value 50",
            "weather rain 60",
            "time set 6000",
            "worldborder set 500",
            f"attribute {NAME} minecraft:generic.max_health base set 30",
            f"attribute {NAME} minecraft:max_health base set 30",
            f"damage {NAME} 2",
            f"tellraw {NAME} {{\"text\":\"raw\",\"color\":\"gold\",\"hoverEvent\":{{\"action\":\"show_text\",\"value\":\"hover\"}},\"clickEvent\":{{\"action\":\"run_command\",\"value\":\"/help\"}}}}",
            f"tellraw {NAME} {{\"text\":\"raw\",\"color\":\"gold\",\"hoverEvent\":{{\"action\":\"show_item\",\"contents\":{{\"id\":\"minecraft:stone\",\"count\":2}}}}}}",
            f"tellraw {NAME} {{text:\"raw\",color:\"gold\",hover_event:{{action:\"show_text\",value:\"hover\"}},click_event:{{action:\"run_command\",command:\"/help\"}}}}",
            "say hello from console",
        ]
    return lines


def state_tracker(protocol):
    """Phase of the next packet in each direction, from this client's view."""
    packets = PROTOCOLS[protocol]
    finish = 3 if protocol >= 766 else 2
    phase = {"c": "login", "s": "login"}

    def record(direction, packet_id):
        state = phase[direction]
        if direction == "c":
            if state == "login" and packet_id == 2:
                if protocol >= 764:
                    phase["c"] = "configuration"
                else:
                    phase["c"] = phase["s"] = "play"
            elif state == "configuration" and packet_id == finish:
                phase["c"] = "play"
            elif state == "play" and packet_id == packets.get("start_configuration"):
                phase["c"] = "configuration"
        elif protocol >= 764:
            if state == "login" and packet_id == 3:
                phase["s"] = "configuration"
            elif state == "configuration" and packet_id == finish:
                phase["s"] = "play"
            elif state == "play" and packet_id == packets.get("configuration_ack"):
                phase["s"] = "configuration"
        return state
    return record


def capture(name, output):
    protocol = SERVERS[name]["protocol"]
    with tempfile.TemporaryDirectory(prefix="rift-corpus-") as temporary:
        directory = Path(temporary)
        port = unused_port()
        command = configure_server(name, directory, port, compression=True)
        with process(command, directory, "server.log", server=True) as server:
            wait_ready(server, lambda: status_ready(port, protocol), directory / "server.log")
            lines, track = [], state_tracker(protocol)
            progress = {"sent": None, "stop": None}

            def observe(client, info):
                if not info:
                    # Wrap the socket helpers once, before the first packet.
                    receive, send = client.receive, client.send

                    def recording_receive():
                        packet_id, body = receive()
                        lines.append({"d": "c", "s": track("c", packet_id), "id": packet_id,
                                      "hex": body.hex()})
                        return packet_id, body

                    def recording_send(packet_id, body=b""):
                        # The handshake precedes login and is not recorded.
                        lines.append({"d": "s", "s": track("s", packet_id), "id": packet_id,
                                      "hex": body.hex()})
                        send(packet_id, body)
                    client.receive, client.send = recording_receive, recording_send
                    return
                if progress["sent"] is None and info["joined"] and info["chunks"] and info["teleports"]:
                    progress["sent"] = time.monotonic()

                    def run():
                        for line in commands(protocol):
                            server.stdin.write(line + "\n")
                            server.stdin.flush()
                            time.sleep(0.05)
                    threading.Thread(target=run, daemon=True).start()
                elif progress["sent"] is not None and time.monotonic() - progress["sent"] > 12:
                    raise Done

            try:
                play(port, NAME, protocol, compression=True, observe=observe)
            except Done:
                pass
            server.stdin.write("stop\n")
            server.stdin.flush()
        temporary_output = output.with_suffix(".tmp")
        with temporary_output.open("w") as out:
            for line in lines:
                out.write(json.dumps(line, separators=(",", ":")) + "\n")
        temporary_output.replace(output)
        return len(lines)


def default_javas():
    for major in (8, 17, 21, 25):
        java = JDKS / f"jdk{major}" / "bin" / "java"
        if java.exists():
            os.environ.setdefault(f"RIFT_JAVA_{major}", str(java))


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--accept-eula", action="store_true", required=True)
    parser.add_argument("--server", action="append", help="fixture name; repeatable")
    parser.add_argument("--force", action="store_true", help="recapture existing files")
    args = parser.parse_args()
    default_javas()
    CORPUS.mkdir(parents=True, exist_ok=True)
    LOCKS.mkdir(parents=True, exist_ok=True)
    failed = []
    for name in args.server or fixtures():
        protocol = SERVERS[name]["protocol"]
        output = CORPUS / f"{protocol}.jsonl"
        # One capture per protocol at a time, machine-wide.
        with (LOCKS / f"corpus-{protocol}.lock").open("w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            if output.exists() and not args.force:
                print(f"{name}: {output} exists", flush=True)
                continue
            try:
                count = capture(name, output)
                print(f"{name}: {count} packets -> {output}", flush=True)
            except Exception as error:  # Keep going: one bad fixture must not stop the rest.
                failed.append(name)
                print(f"{name}: FAILED: {type(error).__name__}: {str(error)[:2000]}", flush=True)
    if failed:
        sys.exit(f"failed: {', '.join(failed)}")


if __name__ == "__main__":
    main()
