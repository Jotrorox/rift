use super::*;
use crate::wire::Position;
use crate::{
    codec::testing::corpus,
    data,
    step::{Emitted, Step},
    wire::Writer,
};
use std::{collections::BTreeSet, io::Write};

/// A packet the step sent, with its name in the protocol it is in.
#[derive(Debug, PartialEq)]
struct Out {
    direction: Direction,
    state: State,
    name: String,
    data: Vec<u8>,
}

/// Feed the step a packet by its name in the sender's protocol. Every
/// packet the step sends is also appended to `$RIFT_SCHEMA_DUMP`, when set,
/// for `data/down_schema_check.py` to decode with minecraft-data.
fn send(
    step: &mut Down,
    direction: Direction,
    state: State,
    name: &str,
    body: &[u8],
) -> io::Result<Vec<Out>> {
    let source = match direction {
        Direction::Clientbound => NEWER,
        Direction::Serverbound => OLDER,
    };
    let id = data::version(source).expect_packet(state, direction, name);
    send_id(step, direction, state, id, body)
}

fn send_id(
    step: &mut Down,
    direction: Direction,
    state: State,
    id: i32,
    body: &[u8],
) -> io::Result<Vec<Out>> {
    let mut emitted = VecDeque::new();
    let mut cx = Cx {
        emitted: &mut emitted,
        client_protocol: OLDER,
        backend_protocol: NEWER,
    };
    let packet = Packet::new(id, body.to_vec());
    match direction {
        Direction::Clientbound => step.clientbound(&mut cx, state, packet)?,
        Direction::Serverbound => step.serverbound(&mut cx, state, packet)?,
    }
    let out: Vec<Out> = emitted
        .into_iter()
        .map(
            |Emitted {
                 direction,
                 state,
                 packet,
             }| {
                let protocol = match direction {
                    Direction::Clientbound => OLDER,
                    Direction::Serverbound => NEWER,
                };
                Out {
                    direction,
                    state,
                    name: data::version(protocol)
                        .packet_name(state, direction, packet.id)
                        .unwrap_or("?")
                        .to_owned(),
                    data: packet.data,
                }
            },
        )
        .collect();
    dump(&out);
    Ok(out)
}

fn dump(out: &[Out]) {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let Some(path) = std::env::var_os("RIFT_SCHEMA_DUMP") else {
        return;
    };
    let mut lines = String::new();
    for packet in out {
        let (protocol, direction) = match packet.direction {
            Direction::Clientbound => (OLDER, "clientbound"),
            Direction::Serverbound => (NEWER, "serverbound"),
        };
        let state = match packet.state {
            State::Login => "login",
            _ => "play",
        };
        let id =
            data::version(protocol).expect_packet(packet.state, packet.direction, &packet.name);
        let hex: String = packet
            .data
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        lines += &format!(
            "{{\"protocol\":{protocol},\"state\":\"{state}\",\"direction\":\"{direction}\",\"id\":{id},\"name\":\"{}\",\"hex\":\"{hex}\"}}\n",
            packet.name
        );
    }
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(lines.as_bytes()))
        .expect("schema dump file");
}

fn clientbound(step: &mut Down, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
    send(step, Direction::Clientbound, State::Play, name, body)
}

fn serverbound(step: &mut Down, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
    send(step, Direction::Serverbound, State::Play, name, body)
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

fn payload(channel: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Writer::new();
    out.string(channel).bytes(data);
    out.into_inner()
}

fn one(out: Vec<Out>, direction: Direction, name: &str) -> Vec<u8> {
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(
        (out[0].direction, out[0].name.as_str()),
        (direction, name),
        "{out:?}"
    );
    out.into_iter().next().unwrap().data
}

fn sound_id(protocol: i32, name: &str) -> i32 {
    data::version(protocol)
        .expect_registry("sound_event")
        .id(name)
        .unwrap()
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

#[test]
fn login_plugin_requests_are_answered_as_not_understood() {
    let mut step = Down::new();
    let mut body = Writer::new();
    body.varint(7).string("velocity:player_info").u8(1);
    let out = send(
        &mut step,
        Direction::Clientbound,
        State::Login,
        "custom_query",
        &body.buf,
    )
    .unwrap();
    assert_eq!(
        out,
        [Out {
            direction: Direction::Serverbound,
            state: State::Login,
            name: "custom_query_answer".into(),
            data: vec![7, 0],
        }]
    );
}

#[test]
fn what_a_1_12_client_cannot_use_is_dropped() {
    let mut step = Down::new();
    // A command tree with only its root, empty tags and recipes, an NBT answer.
    assert!(
        clientbound(&mut step, "commands", &[1, 0, 0, 0])
            .unwrap()
            .is_empty()
    );
    assert!(
        clientbound(&mut step, "update_tags", &[0, 0, 0])
            .unwrap()
            .is_empty()
    );
    assert!(
        clientbound(&mut step, "update_recipes", &[0])
            .unwrap()
            .is_empty()
    );
    assert!(
        clientbound(&mut step, "tag_query", &[1, 0])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn chat_completes_tab_list_names_locally() {
    let mut step = Down::new();
    // Two players join the tab list (add action), one leaves.
    let mut add = Writer::new();
    add.varint(0).varint(2);
    for (uuid, name) in [(1u128, "Corpus"), (2, "cora")] {
        add.uuid(uuid).string(name).varint(1);
        add.string("textures")
            .string("e30=")
            .bool(true)
            .string("sig");
        add.varint(1).varint(20).bool(false);
    }
    let out = clientbound(&mut step, "player_info", &add.buf).unwrap();
    assert_eq!(out.len(), 1);
    let mut chat = Writer::new();
    chat.string("hello co").bool(false).bool(false);
    let answer = one(
        serverbound(&mut step, "command_suggestion", &chat.buf).unwrap(),
        Direction::Clientbound,
        "command_suggestions",
    );
    let mut expected = Writer::new();
    expected.varint(2).string("Corpus").string("cora");
    assert_eq!(answer, expected.buf);
    let mut remove = Writer::new();
    remove.varint(4).varint(1).uuid(2);
    clientbound(&mut step, "player_info", &remove.buf).unwrap();
    let answer = one(
        serverbound(&mut step, "command_suggestion", &chat.buf).unwrap(),
        Direction::Clientbound,
        "command_suggestions",
    );
    let mut expected = Writer::new();
    expected.varint(1).string("Corpus");
    assert_eq!(answer, expected.buf);
}

#[test]
fn commands_complete_through_the_backend() {
    let mut step = Down::new();
    let mut request = Writer::new();
    request.string("/gam").bool(false).bool(true).i64(0);
    let forwarded = one(
        serverbound(&mut step, "command_suggestion", &request.buf).unwrap(),
        Direction::Serverbound,
        "command_suggestion",
    );
    let mut expected = Writer::new();
    expected.varint(0).string("/gam");
    assert_eq!(forwarded, expected.buf);

    // A stale answer is dropped; the real one loses its tooltips and keeps
    // the slash of the word it replaces.
    let mut stale = Writer::new();
    stale.varint(9).varint(1).varint(3).varint(0);
    assert!(
        clientbound(&mut step, "command_suggestions", &stale.buf)
            .unwrap()
            .is_empty()
    );
    let mut answer = Writer::new();
    answer
        .varint(0)
        .varint(1)
        .varint(3)
        .varint(2)
        .string("gamemode")
        .bool(true)
        .string(r#"{"text":"tip"}"#)
        .string("gamerule")
        .bool(false);
    let words = one(
        clientbound(&mut step, "command_suggestions", &answer.buf).unwrap(),
        Direction::Clientbound,
        "command_suggestions",
    );
    let mut expected = Writer::new();
    expected.varint(2).string("/gamemode").string("/gamerule");
    assert_eq!(words, expected.buf);
    // Answered once: a repeat is stale.
    assert!(
        clientbound(&mut step, "command_suggestions", &answer.buf)
            .unwrap()
            .is_empty()
    );

    // A command block's input goes as typed, under the next ID.
    let mut block = Writer::new();
    block.string("sa").bool(true).bool(false);
    let forwarded = one(
        serverbound(&mut step, "command_suggestion", &block.buf).unwrap(),
        Direction::Serverbound,
        "command_suggestion",
    );
    let mut expected = Writer::new();
    expected.varint(1).string("sa");
    assert_eq!(forwarded, expected.buf);
}

#[test]
fn backend_channels_get_their_1_12_names() {
    let mut step = Down::new();
    // The brand as the official 1.13 server sends it.
    let brand = hex("0f6d696e6563726166743a6272616e640776616e696c6c61");
    let out = one(
        clientbound(&mut step, "custom_payload", &brand).unwrap(),
        Direction::Clientbound,
        "custom_payload",
    );
    assert_eq!(out, payload("MC|Brand", b"\x07vanilla"));
    let out = one(
        clientbound(
            &mut step,
            "custom_payload",
            &payload("minecraft:book_open", &[0]),
        )
        .unwrap(),
        Direction::Clientbound,
        "custom_payload",
    );
    assert_eq!(out, payload("MC|BOpen", &[0]));
    let out = one(
        clientbound(
            &mut step,
            "custom_payload",
            &payload("bungeecord:main", b"x"),
        )
        .unwrap(),
        Direction::Clientbound,
        "custom_payload",
    );
    assert_eq!(out, payload("BungeeCord", b"x"));
    // Debug channels, other vanilla channels and names over 20 characters
    // cannot reach a 1.12 client; short plugin channels pass.
    for channel in [
        "minecraft:debug/paths",
        "minecraft:whatever",
        "someplugin:a_long_channel_name",
    ] {
        assert!(
            clientbound(&mut step, "custom_payload", &payload(channel, b"x"))
                .unwrap()
                .is_empty()
        );
    }
    let out = one(
        clientbound(&mut step, "custom_payload", &payload("plugin:main", b"x")).unwrap(),
        Direction::Clientbound,
        "custom_payload",
    );
    assert_eq!(out, payload("plugin:main", b"x"));
    let out = one(
        clientbound(
            &mut step,
            "custom_payload",
            &payload(
                "minecraft:register",
                b"minecraft:brand\0plugin:main\0someplugin:a_long_channel_name",
            ),
        )
        .unwrap(),
        Direction::Clientbound,
        "custom_payload",
    );
    assert_eq!(out, payload("REGISTER", b"MC|Brand\0plugin:main"));
    assert!(
        clientbound(
            &mut step,
            "custom_payload",
            &payload("minecraft:unregister", b"minecraft:debug/paths")
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn client_channels_get_1_13_identifiers_and_keep_their_names_back() {
    let mut step = Down::new();
    // The brand and a plugin channel as the 1.12.2 corpus client sends them.
    let out = one(
        serverbound(
            &mut step,
            "custom_payload",
            &hex("084d437c4272616e6409726966742d74657374"),
        )
        .unwrap(),
        Direction::Serverbound,
        "custom_payload",
    );
    assert_eq!(out, payload("minecraft:brand", b"\x09rift-test"));
    let out = one(
        serverbound(&mut step, "custom_payload", &payload("WDL|INIT", b"x")).unwrap(),
        Direction::Serverbound,
        "custom_payload",
    );
    assert_eq!(out, payload("wdl:init", b"x"));
    let out = one(
        serverbound(
            &mut step,
            "custom_payload",
            &payload("REGISTER", b"WDL|CONTROL\0BungeeCord\0MC|Unknown\0bad name"),
        )
        .unwrap(),
        Direction::Serverbound,
        "custom_payload",
    );
    assert_eq!(
        out,
        payload("minecraft:register", b"wdl:control\0bungeecord:main")
    );
    // The backend answers on the renamed channel; the client hears its own.
    let out = one(
        clientbound(&mut step, "custom_payload", &payload("wdl:control", b"y")).unwrap(),
        Direction::Clientbound,
        "custom_payload",
    );
    assert_eq!(out, payload("WDL|CONTROL", b"y"));
    // Unknown vanilla channels and names no identifier can carry are dropped.
    for channel in ["MC|Unknown", "bad name"] {
        assert!(
            serverbound(&mut step, "custom_payload", &payload(channel, b"x"))
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn vanilla_client_channels_become_1_13_packets() {
    let mut step = Down::new();
    let mut send_payload = |channel: &str, data: &[u8]| {
        serverbound(&mut step, "custom_payload", &payload(channel, data)).unwrap()
    };
    let mut name = Writer::new();
    name.string("Sword");
    assert_eq!(
        one(
            send_payload("MC|ItemName", &name.buf),
            Direction::Serverbound,
            "rename_item"
        ),
        name.buf
    );
    assert_eq!(
        one(
            send_payload("MC|ItemName", &[]),
            Direction::Serverbound,
            "rename_item"
        ),
        [0]
    );
    assert_eq!(
        one(
            send_payload("MC|TrSel", &[0, 0, 1, 0]),
            Direction::Serverbound,
            "select_trade"
        ),
        [0x80, 2]
    );
    assert_eq!(
        one(
            send_payload("MC|PickItem", &[12]),
            Direction::Serverbound,
            "pick_item"
        ),
        [12]
    );
    // Speed and regeneration.
    assert_eq!(
        one(
            send_payload("MC|Beacon", &[0, 0, 0, 1, 0, 0, 0, 10]),
            Direction::Serverbound,
            "set_beacon"
        ),
        [1, 10]
    );

    let mut auto = Writer::new();
    auto.i32(1)
        .i32(64)
        .i32(-2)
        .string("say hi")
        .bool(true)
        .string("AUTO")
        .bool(false)
        .bool(true);
    let mut expected = Writer::new();
    expected
        .position_legacy(Position { x: 1, y: 64, z: -2 })
        .string("say hi")
        .varint(1)
        .u8(0b101);
    assert_eq!(
        one(
            send_payload("MC|AutoCmd", &auto.buf),
            Direction::Serverbound,
            "set_command_block"
        ),
        expected.buf
    );
    let mut bad_mode = Writer::new();
    bad_mode
        .i32(1)
        .i32(64)
        .i32(-2)
        .string("say hi")
        .bool(true)
        .string("SOMETIMES")
        .bool(false)
        .bool(true);
    assert!(send_payload("MC|AutoCmd", &bad_mode.buf).is_empty());

    let mut minecart = Writer::new();
    minecart.u8(1).i32(300).string("say hi").bool(false);
    let mut expected = Writer::new();
    expected.varint(300).string("say hi").bool(false);
    assert_eq!(
        one(
            send_payload("MC|AdvCmd", &minecart.buf),
            Direction::Serverbound,
            "set_command_minecart"
        ),
        expected.buf
    );
    let mut block = Writer::new();
    block
        .u8(0)
        .i32(1)
        .i32(2)
        .i32(3)
        .string("say hi")
        .bool(false);
    assert!(send_payload("MC|AdvCmd", &block.buf).is_empty());

    let mut structure = Writer::new();
    structure
        .i32(10)
        .i32(70)
        .i32(-10)
        .i8(2)
        .string("SAVE")
        .string("house");
    for value in [-40, 0, 5, 7, 33, 1] {
        structure.i32(value);
    }
    structure
        .string("FRONT_BACK")
        .string("CLOCKWISE_180")
        .string("chest")
        .bool(true)
        .bool(false)
        .bool(true)
        .f32(0.5)
        .varlong(42);
    let mut expected = Writer::new();
    expected
        .position_legacy(Position {
            x: 10,
            y: 70,
            z: -10,
        })
        .varint(1)
        .varint(0)
        .string("house");
    for value in [-32i8, 0, 5, 7, 32, 1] {
        expected.i8(value);
    }
    expected
        .varint(2)
        .varint(2)
        .string("chest")
        .f32(0.5)
        .varlong(42)
        .u8(0b101);
    assert_eq!(
        one(
            send_payload("MC|Struct", &structure.buf),
            Direction::Serverbound,
            "set_structure_block"
        ),
        expected.buf
    );
}

#[test]
fn stop_sound_becomes_the_legacy_channel() {
    let mut step = Down::new();
    let mut body = Writer::new();
    body.u8(3).varint(4).string("minecraft:block.wool.break");
    let out = one(
        clientbound(&mut step, "stop_sound", &body.buf).unwrap(),
        Direction::Clientbound,
        "custom_payload",
    );
    let mut expected = Writer::new();
    expected
        .string("block")
        .string("minecraft:block.cloth.break");
    assert_eq!(out, payload("MC|StopSound", &expected.buf));
    let out = one(
        clientbound(&mut step, "stop_sound", &[0]).unwrap(),
        Direction::Clientbound,
        "custom_payload",
    );
    assert_eq!(out, payload("MC|StopSound", &[0, 0]));
    // A sound 1.12 lacks stops its stand-in; a resource pack sound keeps its name.
    let mut body = Writer::new();
    body.u8(2).string("entity.dolphin.ambient");
    let out = one(
        clientbound(&mut step, "stop_sound", &body.buf).unwrap(),
        Direction::Clientbound,
        "custom_payload",
    );
    let mut expected = Writer::new();
    expected.string("").string("entity.squid.ambient");
    assert_eq!(out, payload("MC|StopSound", &expected.buf));
    let mut body = Writer::new();
    body.u8(2).string("pack:custom.sound");
    let out = one(
        clientbound(&mut step, "stop_sound", &body.buf).unwrap(),
        Direction::Clientbound,
        "custom_payload",
    );
    let mut expected = Writer::new();
    expected.string("").string("pack:custom.sound");
    assert_eq!(out, payload("MC|StopSound", &expected.buf));
    assert!(clientbound(&mut step, "stop_sound", &[1, 10]).is_err());
}

#[test]
fn named_sounds_take_1_12_names() {
    let mut step = Down::new();
    let mut body = Writer::new();
    body.string("minecraft:block.wool.place")
        .varint(4)
        .i32(8)
        .i32(16)
        .i32(24)
        .f32(1.0)
        .f32(0.8);
    let out = one(
        clientbound(&mut step, "custom_sound", &body.buf).unwrap(),
        Direction::Clientbound,
        "custom_sound",
    );
    let mut expected = Writer::new();
    expected
        .string("minecraft:block.cloth.place")
        .varint(4)
        .i32(8)
        .i32(16)
        .i32(24)
        .f32(1.0)
        .f32(0.8);
    assert_eq!(out, expected.buf);
    // The 1.13 corpus's /playsound comes out as the 1.12.2 server sends it,
    // but for where the player stood.
    if let (Some(packets), Some(reference)) = (corpus(NEWER), corpus(OLDER)) {
        for packet in packets
            .iter()
            .filter(|packet| packet.name(NEWER) == "custom_sound")
        {
            let out = one(
                send_id(
                    &mut step,
                    packet.direction,
                    packet.state,
                    packet.id,
                    &packet.data,
                )
                .unwrap(),
                Direction::Clientbound,
                "custom_sound",
            );
            let old = reference
                .iter()
                .find(|packet| packet.name(OLDER) == "custom_sound")
                .unwrap();
            let fields = |data: &[u8]| {
                let mut reader = Reader::new(data);
                let name = reader.str().unwrap().to_owned();
                let category = reader.varint().unwrap();
                reader.take(12).unwrap();
                (name, category, reader.rest().to_vec())
            };
            assert_eq!(fields(&out), fields(&old.data));
        }
    }
}

#[test]
fn recipe_unlocks_carry_1_12_ids() {
    let recipes = recipes::Recipes::get();
    // Checked against the 1.12.2 corpus, where these are unlocked one by one.
    assert_eq!(recipes.id("minecraft:red_bed"), Some(103));
    assert_eq!(recipes.id("painting"), Some(148));
    assert_eq!(
        recipes.id("minecraft:oak_slab"),
        recipes.name(0).and(recipes.id("oak_slab"))
    );
    assert_eq!(recipes.name(103).as_deref(), Some("minecraft:red_bed"));
    assert_eq!(
        recipes.id("minecraft:charcoal"),
        None,
        "smelting is new in 1.13"
    );

    let mut step = Down::new();
    let mut unlock = Writer::new();
    unlock
        .varint(1)
        .bool(true)
        .bool(false)
        .bool(true)
        .bool(true)
        .varint(3);
    unlock
        .string("minecraft:red_bed")
        .string("minecraft:charcoal")
        .string("minecraft:oak_slab");
    let out = one(
        clientbound(&mut step, "recipe", &unlock.buf).unwrap(),
        Direction::Clientbound,
        "recipe",
    );
    let mut expected = Writer::new();
    expected
        .varint(1)
        .bool(true)
        .bool(false)
        .varint(2)
        .varint(103)
        .varint(recipes.id("oak_slab").unwrap());
    assert_eq!(out, expected.buf);
    // Nothing 1.12 knows: nothing to send.
    let mut only_new = Writer::new();
    only_new
        .varint(1)
        .bool(false)
        .bool(false)
        .bool(false)
        .bool(false)
        .varint(1)
        .string("minecraft:charcoal");
    assert!(
        clientbound(&mut step, "recipe", &only_new.buf)
            .unwrap()
            .is_empty()
    );
    // The initial list always goes, with its second list.
    let mut init = Writer::new();
    init.varint(0)
        .bool(false)
        .bool(false)
        .bool(false)
        .bool(false)
        .varint(0)
        .varint(1)
        .string("minecraft:red_bed");
    let out = one(
        clientbound(&mut step, "recipe", &init.buf).unwrap(),
        Direction::Clientbound,
        "recipe",
    );
    assert_eq!(out, [0, 0, 0, 0, 1, 103]);

    let mut ghost = Writer::new();
    ghost.i8(1).string("minecraft:red_bed");
    assert_eq!(
        one(
            clientbound(&mut step, "place_ghost_recipe", &ghost.buf).unwrap(),
            Direction::Clientbound,
            "place_ghost_recipe"
        ),
        [1, 103]
    );
    let mut ghost = Writer::new();
    ghost.i8(1).string("minecraft:charcoal");
    assert!(
        clientbound(&mut step, "place_ghost_recipe", &ghost.buf)
            .unwrap()
            .is_empty()
    );

    let mut place = Writer::new();
    place.i8(1).varint(103).bool(true);
    let mut expected = Writer::new();
    expected.i8(1).string("minecraft:red_bed").bool(true);
    assert_eq!(
        one(
            serverbound(&mut step, "place_recipe", &place.buf).unwrap(),
            Direction::Serverbound,
            "place_recipe"
        ),
        expected.buf
    );
    assert!(
        serverbound(&mut step, "place_recipe", &[1, 0xe8, 0x07, 0])
            .unwrap()
            .is_empty()
    );

    let mut seen = Writer::new();
    seen.varint(0).i32(103);
    let mut expected = Writer::new();
    expected.varint(0).string("minecraft:red_bed");
    assert_eq!(
        one(
            serverbound(&mut step, "recipe_book_update", &seen.buf).unwrap(),
            Direction::Serverbound,
            "recipe_book_update"
        ),
        expected.buf
    );
    assert_eq!(
        one(
            serverbound(&mut step, "recipe_book_update", &[1, 1, 0]).unwrap(),
            Direction::Serverbound,
            "recipe_book_update"
        ),
        [1, 1, 0, 0, 0]
    );
    assert!(serverbound(&mut step, "recipe_book_update", &[2]).is_err());
}

/// The official servers unlock the same recipes in the corpus scenario: the
/// 1.13 unlocks, downgraded, are exactly the 1.12.2 server's.
#[test]
fn corpus_recipe_unlocks_match_the_1_12_server() {
    let (Some(new), Some(old)) = (corpus(NEWER), corpus(OLDER)) else {
        eprintln!("skipped: no corpus for {NEWER} and {OLDER}");
        return;
    };
    let ids = |packets: Vec<Vec<u8>>| -> BTreeSet<i32> {
        let mut ids = BTreeSet::new();
        for packet in packets {
            let mut reader = Reader::new(&packet);
            let action = reader.varint().unwrap();
            reader.bool().unwrap();
            reader.bool().unwrap();
            for _ in 0..if action == 0 { 2 } else { 1 } {
                for _ in 0..reader.count().unwrap() {
                    ids.insert(reader.varint().unwrap());
                }
            }
            reader.finish().unwrap();
        }
        ids
    };
    let mut step = Down::new();
    let mut converted = Vec::new();
    for packet in new.iter().filter(|packet| packet.name(NEWER) == "recipe") {
        for out in send_id(
            &mut step,
            packet.direction,
            packet.state,
            packet.id,
            &packet.data,
        )
        .unwrap()
        {
            converted.push(out.data);
        }
    }
    let recorded = old
        .iter()
        .filter(|packet| packet.name(OLDER) == "recipe")
        .map(|packet| packet.data.clone())
        .collect();
    let (converted, recorded) = (ids(converted), ids(recorded));
    assert_eq!(converted.len(), 432);
    assert_eq!(converted, recorded);
}

#[test]
fn the_pufferfish_sting_plays_its_sound_where_the_player_is() {
    let mut step = Down::new();
    let rain = [1, 0, 0, 0, 0];
    assert_eq!(
        one(
            clientbound(&mut step, "game_event", &rain).unwrap(),
            Direction::Clientbound,
            "game_event"
        ),
        rain
    );
    let sting = [9, 0, 0, 0, 0];
    assert!(
        clientbound(&mut step, "game_event", &sting)
            .unwrap()
            .is_empty(),
        "no position yet"
    );
    let mut teleport = Writer::new();
    teleport
        .f64(1.5)
        .f64(64.0)
        .f64(-2.0)
        .f32(0.0)
        .f32(0.0)
        .u8(0)
        .varint(1);
    clientbound(&mut step, "player_position", &teleport.buf).unwrap();
    let out = one(
        clientbound(&mut step, "game_event", &sting).unwrap(),
        Direction::Clientbound,
        "sound",
    );
    let mut expected = Writer::new();
    expected
        .varint(sound_id(OLDER, "enchant.thorns.hit"))
        .varint(6)
        .i32(12)
        .i32(512)
        .i32(-16)
        .f32(1.0)
        .f32(1.0);
    assert_eq!(out, expected.buf);
    assert!(clientbound(&mut step, "game_event", &[1, 0, 0, 0]).is_err());
}

#[test]
fn looking_at_a_point_is_a_rotation_only_teleport() {
    let mut step = Down::new();
    let mut look = Writer::new();
    look.varint(1).f64(0.5).f64(65.62).f64(10.5).bool(false);
    assert!(
        clientbound(&mut step, "player_look_at", &look.buf)
            .unwrap()
            .is_empty(),
        "no position yet"
    );
    let mut teleport = Writer::new();
    teleport
        .f64(0.5)
        .f64(64.0)
        .f64(0.5)
        .f32(0.0)
        .f32(0.0)
        .u8(0)
        .varint(1);
    clientbound(&mut step, "player_position", &teleport.buf).unwrap();
    // The client moves north-east a little; its own movement is followed.
    let mut moved = Writer::new();
    moved
        .f64(0.5)
        .f64(64.0)
        .f64(0.5)
        .f32(90.0)
        .f32(0.0)
        .bool(true);
    serverbound(&mut step, "move_player_pos_rot", &moved.buf).unwrap();
    let out = one(
        clientbound(&mut step, "player_look_at", &look.buf).unwrap(),
        Direction::Clientbound,
        "player_position",
    );
    let mut reader = Reader::new(&out);
    assert_eq!(
        [
            reader.f64().unwrap(),
            reader.f64().unwrap(),
            reader.f64().unwrap()
        ],
        [0.0; 3]
    );
    let (yaw, pitch) = (reader.f32().unwrap(), reader.f32().unwrap());
    assert!(
        yaw.abs() < 1e-3 && pitch.abs() < 1e-3,
        "south at eye level: {yaw} {pitch}"
    );
    assert_eq!(
        (reader.u8().unwrap(), reader.varint().unwrap()),
        (0b111, -1)
    );
    reader.finish().unwrap();
    // Looking at an entity's feet below and to the west, from the feet.
    let mut look = Writer::new();
    look.varint(0)
        .f64(-9.5)
        .f64(54.0)
        .f64(0.5)
        .bool(true)
        .varint(12)
        .varint(0);
    let out = one(
        clientbound(&mut step, "player_look_at", &look.buf).unwrap(),
        Direction::Clientbound,
        "player_position",
    );
    let mut reader = Reader::new(&out);
    reader.take(24).unwrap();
    let (yaw, pitch) = (reader.f32().unwrap(), reader.f32().unwrap());
    assert!(
        (yaw - 90.0).abs() < 1e-3 && (pitch - 45.0).abs() < 1e-3,
        "{yaw} {pitch}"
    );
    // The client confirms both; neither reaches the backend, a real one does.
    assert!(
        serverbound(&mut step, "accept_teleportation", &[0x7f])
            .unwrap()
            .len()
            == 1
    );
    let minus = |value: i32| {
        let mut out = Writer::new();
        out.varint(value);
        out.into_inner()
    };
    assert!(
        serverbound(&mut step, "accept_teleportation", &minus(-2))
            .unwrap()
            .is_empty()
    );
    assert!(
        serverbound(&mut step, "accept_teleportation", &minus(-1))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        serverbound(&mut step, "accept_teleportation", &minus(-1))
            .unwrap()
            .len(),
        1
    );
    // After a respawn the position is unknown until the next teleport.
    clientbound(
        &mut step,
        "respawn",
        &[
            0, 0, 0, 0, 0, 0, 7, b'd', b'e', b'f', b'a', b'u', b'l', b't',
        ],
    )
    .unwrap();
    assert!(
        clientbound(&mut step, "player_look_at", &look.buf)
            .unwrap()
            .is_empty()
    );
}

/// Every packet the step handles itself, from the official servers' own
/// sessions, converts without error.
#[test]
fn corpus_packets_the_step_owns_convert() {
    const OWN: &[&str] = &[
        "custom_query",
        "commands",
        "command_suggestions",
        "custom_payload",
        "stop_sound",
        "custom_sound",
        "recipe",
        "place_ghost_recipe",
        "update_recipes",
        "update_tags",
        "tag_query",
        "player_look_at",
        "game_event",
        "player_position",
        "player_info",
        "respawn",
        "login",
    ];
    const OWN_SERVERBOUND: &[&str] = &[
        "command_suggestion",
        "custom_payload",
        "place_recipe",
        "recipe_book_update",
        "accept_teleportation",
        "move_player_pos",
        "move_player_pos_rot",
    ];
    let (Some(new), Some(old)) = (corpus(NEWER), corpus(OLDER)) else {
        eprintln!("skipped: no corpus for {NEWER} and {OLDER}");
        return;
    };
    let mut step = Down::new();
    let mut converted = 0;
    let backend = new.iter().filter(|packet| {
        packet.direction == Direction::Clientbound && OWN.contains(&packet.name(NEWER))
    });
    let client = old.iter().filter(|packet| {
        packet.direction == Direction::Serverbound && OWN_SERVERBOUND.contains(&packet.name(OLDER))
    });
    for packet in backend.chain(client) {
        send_id(
            &mut step,
            packet.direction,
            packet.state,
            packet.id,
            &packet.data,
        )
        .unwrap_or_else(|error| {
            panic!(
                "{}: {error}",
                packet.name(if packet.direction == Direction::Clientbound {
                    NEWER
                } else {
                    OLDER
                })
            )
        });
        converted += 1;
    }
    assert!(converted > 600, "{converted}");
}

/// Cut and padded versions of valid packets fail or convert, never panic.
#[test]
fn malformed_packets_fail_without_panicking() {
    let mut command_block = Writer::new();
    command_block
        .i32(1)
        .i32(2)
        .i32(3)
        .string("c")
        .bool(true)
        .string("AUTO")
        .bool(true)
        .bool(true);
    let mut structure = Writer::new();
    structure
        .i32(1)
        .i32(2)
        .i32(3)
        .i8(1)
        .string("DATA")
        .string("n");
    for _ in 0..6 {
        structure.i32(1);
    }
    structure
        .string("NONE")
        .string("NONE")
        .string("")
        .bool(true)
        .bool(true)
        .bool(true)
        .f32(1.0)
        .varlong(1);
    let mut look = Writer::new();
    look.varint(1)
        .f64(1.0)
        .f64(2.0)
        .f64(3.0)
        .bool(true)
        .varint(1)
        .varint(1);
    let mut suggestions = Writer::new();
    suggestions
        .varint(0)
        .varint(0)
        .varint(1)
        .varint(1)
        .string("a")
        .bool(true)
        .string("{}");
    let mut sound = Writer::new();
    sound
        .string("a")
        .varint(4)
        .i32(0)
        .i32(0)
        .i32(0)
        .f32(1.0)
        .f32(1.0);
    let mut recipe = Writer::new();
    recipe
        .varint(0)
        .bool(false)
        .bool(false)
        .bool(false)
        .bool(false)
        .varint(1)
        .string("a")
        .varint(0);
    let mut position = Writer::new();
    position
        .f64(0.0)
        .f64(0.0)
        .f64(0.0)
        .f32(0.0)
        .f32(0.0)
        .u8(0)
        .varint(0);
    let mut player = Writer::new();
    player
        .varint(0)
        .varint(1)
        .uuid(1)
        .string("a")
        .varint(0)
        .varint(0)
        .varint(0)
        .bool(false);
    let cases: Vec<(Direction, &str, Vec<u8>)> = vec![
        (
            Direction::Clientbound,
            "custom_payload",
            payload("minecraft:register", b"a:b\0c:d"),
        ),
        (
            Direction::Clientbound,
            "stop_sound",
            vec![3, 1, 3, b'a', b':', b'b'],
        ),
        (Direction::Clientbound, "custom_sound", sound.into_inner()),
        (Direction::Clientbound, "recipe", recipe.into_inner()),
        (
            Direction::Clientbound,
            "place_ghost_recipe",
            vec![0, 1, b'a'],
        ),
        (Direction::Clientbound, "player_look_at", look.into_inner()),
        (Direction::Clientbound, "game_event", vec![9, 0, 0, 0, 0]),
        (
            Direction::Clientbound,
            "player_position",
            position.into_inner(),
        ),
        (Direction::Clientbound, "player_info", player.into_inner()),
        (
            Direction::Clientbound,
            "command_suggestions",
            suggestions.into_inner(),
        ),
        (
            Direction::Serverbound,
            "command_suggestion",
            vec![2, b'/', b'a', 0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
        ),
        (
            Direction::Serverbound,
            "custom_payload",
            payload("MC|AutoCmd", &command_block.buf),
        ),
        (
            Direction::Serverbound,
            "custom_payload",
            payload("MC|Struct", &structure.buf),
        ),
        (
            Direction::Serverbound,
            "custom_payload",
            payload("MC|AdvCmd", &[1, 0, 0, 0, 1, 1, b'c', 0]),
        ),
        (
            Direction::Serverbound,
            "custom_payload",
            payload("MC|Beacon", &[0, 0, 0, 1, 0, 0, 0, 2]),
        ),
        (Direction::Serverbound, "place_recipe", vec![0, 1, 0]),
        (
            Direction::Serverbound,
            "recipe_book_update",
            vec![0, 0, 0, 0, 1],
        ),
        (Direction::Serverbound, "move_player_pos", vec![0; 25]),
    ];
    for (direction, name, body) in cases {
        let mut whole = Down::new();
        let mut teleport = Writer::new();
        teleport
            .f64(0.0)
            .f64(64.0)
            .f64(0.0)
            .f32(0.0)
            .f32(0.0)
            .u8(0)
            .varint(1);
        clientbound(&mut whole, "player_position", &teleport.buf).unwrap();
        send(&mut whole, direction, State::Play, name, &body)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        for end in 0..body.len() {
            let mut step = Down::new();
            let _ = send(&mut step, direction, State::Play, name, &body[..end]);
        }
        // A backend channel's payload runs to the end of the packet, so a
        // trailing byte is part of it; everything else rejects one.
        let mut padded = body.clone();
        padded.push(0);
        let mut step = Down::new();
        let result = send(&mut step, direction, State::Play, name, &padded);
        if name != "custom_payload" || direction == Direction::Serverbound {
            assert!(result.is_err(), "{name} accepted a trailing byte");
        }
    }
    // Books carry an item stack, which only the item codec can read.
    // INTEGRATION: expect an `edit_book` here once the item codec lands.
    let mut step = Down::new();
    let _ = send(
        &mut step,
        Direction::Serverbound,
        State::Play,
        "custom_payload",
        &payload("MC|BEdit", &[0xff, 0xff]),
    );
}
