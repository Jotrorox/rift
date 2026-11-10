use super::*;
use crate::{
    codec::testing::{Recorded, corpus},
    data,
    legacy::legacy,
    step::Emitted,
    wire::{Position, Rewriter},
};
use world::chunk::{Column, Format};

const OLD: i32 = super::super::OLDER;
const NEW: i32 = super::super::NEWER;

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A packet the step emitted, named in the protocol it is in.
#[derive(Debug, PartialEq, Eq)]
struct Out {
    direction: Direction,
    state: State,
    name: String,
    data: Vec<u8>,
}

impl Out {
    fn hex(&self) -> String {
        to_hex(&self.data)
    }
}

/// With `RIFT_S02_DUMP` set, every packet the tests see the step produce is
/// appended to that file as a JSON line, for `schema_check.py`.
fn dump(direction: Direction, state: State, name: &str, data: &[u8]) {
    let Some(path) = std::env::var_os("RIFT_S02_DUMP") else {
        return;
    };
    use std::io::Write as _;
    let line = serde_json::json!({
        "to": if direction == Direction::Clientbound { "client" } else { "backend" },
        "s": match state {
            State::Login => "login",
            State::Configuration => "configuration",
            _ => "play",
        },
        "name": name,
        "hex": to_hex(data),
    });
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

fn collect(emitted: VecDeque<Emitted>) -> Vec<Out> {
    let outs: Vec<Out> = emitted
        .into_iter()
        .map(|emitted| {
            let protocol = match emitted.direction {
                Direction::Clientbound => NEW,
                Direction::Serverbound => OLD,
            };
            Out {
                direction: emitted.direction,
                state: emitted.state,
                name: data::version(protocol)
                    .packet_name(emitted.state, emitted.direction, emitted.packet.id)
                    .unwrap_or("?")
                    .to_owned(),
                data: emitted.packet.data,
            }
        })
        .collect();
    for out in &outs {
        dump(out.direction, out.state, &out.name, &out.data);
    }
    outs
}

/// Send the packet `name` (in the sender's protocol) through `step`.
fn send(
    step: &mut Up,
    direction: Direction,
    state: State,
    name: &str,
    body: &[u8],
) -> io::Result<Vec<Out>> {
    let source = match direction {
        Direction::Clientbound => OLD,
        Direction::Serverbound => NEW,
    };
    let id = data::version(source).expect_packet(state, direction, name);
    send_id(step, direction, state, id, body)
}

fn send_id(
    step: &mut Up,
    direction: Direction,
    state: State,
    id: i32,
    body: &[u8],
) -> io::Result<Vec<Out>> {
    let mut emitted = VecDeque::new();
    let mut cx = Cx {
        emitted: &mut emitted,
        client_protocol: NEW,
        backend_protocol: OLD,
    };
    let packet = Packet::new(id, body.to_vec());
    match direction {
        Direction::Clientbound => step.clientbound(&mut cx, state, packet)?,
        Direction::Serverbound => step.serverbound(&mut cx, state, packet)?,
    }
    Ok(collect(emitted))
}

fn clientbound(step: &mut Up, name: &str, body: &[u8]) -> Vec<Out> {
    send(step, Direction::Clientbound, State::Play, name, body).unwrap()
}

fn serverbound(step: &mut Up, name: &str, body: &[u8]) -> Vec<Out> {
    send(step, Direction::Serverbound, State::Play, name, body).unwrap()
}

fn only(outs: Vec<Out>) -> Out {
    assert_eq!(outs.len(), 1, "{outs:?}");
    outs.into_iter().next().unwrap()
}

fn string(text: &str) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.string(text);
    writer.buf
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
}

#[test]
fn login_is_followed_by_recipes_tags_and_commands() {
    let mut step = Up::new();
    // The 1.12.2 server's join game packet; 1.13 sends the same bytes.
    let login = hex("000000000100000000001404666c617400");
    let outs = clientbound(&mut step, "login", &login);
    let names: Vec<&str> = outs.iter().map(|out| out.name.as_str()).collect();
    assert_eq!(
        names,
        ["login", "update_recipes", "update_tags", "commands"]
    );
    assert_eq!(outs[0].data, login);
    assert_eq!(
        outs[3].hex(),
        "0200010116000461726773106272696761646965723a737472696e6702146d696e6563726166743a61736b5f73657276657200"
    );
    // Malformed: a level type longer than 1.12 allows.
    let mut long = hex("00000000010000000000");
    long.extend(string("12345678901234567"));
    long.push(0);
    assert!(
        send(
            &mut step,
            Direction::Clientbound,
            State::Play,
            "login",
            &long
        )
        .is_err()
    );
}

/// Parse a 1.13 `update_recipes` body completely; returns the recipe names.
fn parse_recipes(body: &[u8]) -> Vec<String> {
    let items = data::version(NEW).expect_registry("item").len() as i16;
    let mut reader = Reader::new(body);
    let stack = |reader: &mut Reader<'_>| {
        let id = reader.i16().unwrap();
        assert!((1..items).contains(&id), "item {id}");
        assert!(reader.i8().unwrap() > 0);
        assert_eq!(reader.u8().unwrap(), 0);
    };
    let ingredient = |reader: &mut Reader<'_>| {
        for _ in 0..reader.count().unwrap() {
            stack(reader);
        }
    };
    let mut names = Vec::new();
    for _ in 0..reader.count().unwrap() {
        names.push(reader.str().unwrap().to_owned());
        match reader.str().unwrap() {
            "crafting_shaped" => {
                let (width, height) = (reader.varint().unwrap(), reader.varint().unwrap());
                assert!((1..=3).contains(&width) && (1..=3).contains(&height));
                reader.str().unwrap();
                for _ in 0..width * height {
                    ingredient(&mut reader);
                }
            }
            "crafting_shapeless" => {
                reader.str().unwrap();
                for _ in 0..reader.count().unwrap() {
                    ingredient(&mut reader);
                }
            }
            other => panic!("unexpected recipe type {other}"),
        }
        stack(&mut reader);
    }
    reader.finish().unwrap();
    names
}

#[test]
fn recipes_cover_every_crafting_recipe_with_flattened_items() {
    let names = parse_recipes(&recipes::declare().unwrap());
    // 432 JSON recipes in 1.12.2, all crafting recipes with 1.13 items.
    assert_eq!(names.len(), 432);
    assert_eq!(names[0], "minecraft:yellow_wool");
    assert!(names.contains(&"minecraft:red_bed".to_owned()));
    // The stick takes any planks: all six 1.13 planks are alternatives.
    let declared = recipes::declare().unwrap();
    let planks: Vec<i32> = ["oak", "spruce", "birch", "jungle", "acacia", "dark_oak"]
        .iter()
        .map(|wood| {
            data::version(NEW)
                .expect_registry("item")
                .id(&format!("{wood}_planks"))
                .unwrap()
        })
        .collect();
    let stick = to_hex(&string("minecraft:stick"));
    let at = to_hex(&declared).find(&stick).unwrap() / 2;
    let mut reader = Reader::new(&declared[at..]);
    assert_eq!(reader.str().unwrap(), "minecraft:stick");
    assert_eq!(reader.str().unwrap(), "crafting_shaped");
    assert_eq!((reader.varint().unwrap(), reader.varint().unwrap()), (1, 2));
    reader.str().unwrap();
    assert_eq!(reader.count().unwrap(), 6);
    for plank in planks {
        assert_eq!(i32::from(reader.i16().unwrap()), plank);
        reader.i8().unwrap();
        reader.u8().unwrap();
    }
}

#[test]
fn recipe_ids_become_names_and_back() {
    let mut step = Up::new();
    // 1.12.2 unlocks recipe 101 (red carpet in its numbering).
    let out = only(clientbound(&mut step, "recipe", &hex("0100000165")));
    let mut expected = hex("010000000001");
    expected.extend(string("minecraft:red_carpet"));
    assert_eq!(out.data, expected);
    // Init with the special recipe 0 (armour dyeing): left out.
    let out = only(clientbound(&mut step, "recipe", &hex("00010002000b010c")));
    let mut expected = hex("000100000001");
    expected.extend(string("minecraft:yellow_wool"));
    expected.push(1);
    expected.extend(string("minecraft:yellow_stained_hardened_clay"));
    assert_eq!(out.data, expected);
    // The ghost recipe and the recipe book click.
    let out = only(clientbound(&mut step, "place_ghost_recipe", &hex("0165")));
    let mut expected = hex("01");
    expected.extend(string("minecraft:red_carpet"));
    assert_eq!(out.data, expected);
    let mut click = hex("01");
    click.extend(string("minecraft:red_carpet"));
    click.push(1);
    let out = only(serverbound(&mut step, "place_recipe", &click));
    assert_eq!(
        (out.name.as_str(), out.hex()),
        ("place_recipe", "016501".into())
    );
    let mut seen = hex("00");
    seen.extend(string("minecraft:red_carpet"));
    let out = only(serverbound(&mut step, "recipe_book_update", &seen));
    assert_eq!(out.hex(), "0000000065");
    let out = only(serverbound(
        &mut step,
        "recipe_book_update",
        &hex("0101000100"),
    ));
    assert_eq!(out.hex(), "010100");
    // Unknown and special recipes are not passed on.
    let mut unknown = hex("01");
    unknown.extend(string("minecraft:armordye"));
    unknown.push(0);
    assert!(serverbound(&mut step, "place_recipe", &unknown).is_empty());
    assert!(clientbound(&mut step, "place_ghost_recipe", &hex("0100")).is_empty());
    assert!(
        send(
            &mut step,
            Direction::Clientbound,
            State::Play,
            "recipe",
            &hex("03000000")
        )
        .is_err()
    );
}

#[test]
fn every_recipe_unlocked_by_the_real_server_is_declared() {
    let Some(packets) = corpus(OLD) else {
        eprintln!("no corpus for {OLD}: skipped");
        return;
    };
    let mut step = Up::new();
    let mut unlocked = 0;
    for packet in packets.iter().filter(|packet| packet.name(OLD) == "recipe") {
        let out = only(
            send_id(
                &mut step,
                packet.direction,
                packet.state,
                packet.id,
                &packet.data,
            )
            .unwrap(),
        );
        let mut reader = Reader::new(&out.data);
        let action = reader.varint().unwrap();
        for _ in 0..4 {
            reader.bool().unwrap();
        }
        for _ in 0..if action == 0 { 2 } else { 1 } {
            for _ in 0..reader.count().unwrap() {
                reader.str().unwrap();
                unlocked += 1;
            }
        }
        reader.finish().unwrap();
    }
    // `recipe give @p *` unlocks all 432 crafting recipes.
    assert_eq!(unlocked, 432);
}

#[test]
fn tags_match_the_real_1_13_server() {
    let body = tags::packet().unwrap();
    let read = |body: &[u8]| {
        let mut reader = Reader::new(body);
        let mut sections = Vec::new();
        for _ in 0..3 {
            let mut tags = std::collections::BTreeMap::new();
            for _ in 0..reader.count().unwrap() {
                let name = reader.str().unwrap().to_owned();
                let mut ids: Vec<i32> = (0..reader.count().unwrap())
                    .map(|_| reader.varint().unwrap())
                    .collect();
                ids.sort();
                tags.insert(name, ids);
            }
            sections.push(tags);
        }
        reader.finish().unwrap();
        sections
    };
    let ours = read(&body);
    assert_eq!(ours[2]["minecraft:water"], [1, 2]);
    assert_eq!(ours[0].len(), 36);
    let Some(packets) = corpus(NEW) else {
        eprintln!("no corpus for {NEW}: skipped");
        return;
    };
    let real = packets
        .iter()
        .find(|packet| packet.name(NEW) == "update_tags")
        .unwrap();
    assert_eq!(read(&real.data), ours);
}

#[test]
fn plugin_channels_get_namespaced_names_and_back() {
    let mut step = Up::new();
    // The real brand packets of both servers and clients.
    let out = only(clientbound(
        &mut step,
        "custom_payload",
        &hex("084d437c4272616e640776616e696c6c61"),
    ));
    assert_eq!(
        out.hex(),
        "0f6d696e6563726166743a6272616e640776616e696c6c61"
    );
    let out = only(serverbound(
        &mut step,
        "custom_payload",
        &hex("0f6d696e6563726166743a6272616e6409726966742d74657374"),
    ));
    assert_eq!(out.hex(), "084d437c4272616e6409726966742d74657374");

    // Registered channels: an invalid name is escaped, a plain one
    // namespaced, BungeeCord renamed; and they come back as they were.
    let mut register = string("REGISTER");
    register.extend(b"WECUI\0foo\0BungeeCord\0fml:hs");
    let out = only(clientbound(&mut step, "custom_payload", &register));
    let mut expected = string("minecraft:register");
    expected.extend(b"rift:legacy/_57_45_43_55_49\0minecraft:foo\0bungeecord:main\0fml:hs");
    assert_eq!(out.data, expected);
    let mut back = string("minecraft:register");
    back.extend(b"rift:legacy/_57_45_43_55_49\0foo\0bungeecord:main\0a_very_long_mod:channel_name");
    let out = only(serverbound(&mut step, "custom_payload", &back));
    let mut expected = string("REGISTER");
    expected.extend(b"WECUI\0foo\0BungeeCord");
    assert_eq!(out.data, expected);
    let mut message = string("rift:legacy/_57_45_43_55_49");
    message.extend(b"payload");
    let out = only(serverbound(&mut step, "custom_payload", &message));
    let mut expected = string("WECUI");
    expected.extend(b"payload");
    assert_eq!(out.data, expected);
    // A name 1.12 cannot take is not sent; a malformed one fails.
    let mut long = string("fabric:registry/sync");
    long.push(1);
    assert_eq!(
        channels::upgrade("fabric:registry/sync"),
        "fabric:registry/sync"
    );
    assert!(
        serverbound(
            &mut step,
            "custom_payload",
            &string("fabric:registry/sync/long")
        )
        .is_empty()
    );
    assert!(
        send(
            &mut step,
            Direction::Serverbound,
            State::Play,
            "custom_payload",
            &string("Bad Name")
        )
        .is_err()
    );
    // Escapes decode without remembered names.
    for name in ["WECUI", "MC|Struct", "Some_Channel", "ünï"] {
        assert_eq!(channels::downgrade(&channels::upgrade(name)).unwrap(), name);
    }
    assert_eq!(channels::downgrade("rift:legacy/_zz"), None);
}

#[test]
fn stop_sound_becomes_a_packet() {
    let mut step = Up::new();
    let mut body = string("MC|StopSound");
    body.extend(string("hostile"));
    body.extend(string("minecraft:entity.pig.ambient"));
    let out = only(clientbound(&mut step, "custom_payload", &body));
    assert_eq!(out.name, "stop_sound");
    let mut expected = hex("0305");
    expected.extend(string("minecraft:entity.pig.ambient"));
    assert_eq!(out.data, expected);
    // A renamed vanilla sound gets its 1.13 name; empty fields are absent.
    let mut body = string("MC|StopSound");
    body.extend(string(""));
    body.extend(string("minecraft:block.cloth.break"));
    let out = only(clientbound(&mut step, "custom_payload", &body));
    let mut expected = hex("02");
    expected.extend(string("minecraft:block.wool.break"));
    assert_eq!(out.data, expected);
    let mut body = string("MC|StopSound");
    body.extend(string(""));
    body.extend(string(""));
    assert_eq!(
        only(clientbound(&mut step, "custom_payload", &body)).hex(),
        "00"
    );
}

#[test]
fn tab_completion_gets_ranges_and_transaction_ids() {
    let mut step = Up::new();
    let mut request = hex("07");
    request.extend(string("/tp No"));
    let out = only(serverbound(&mut step, "command_suggestion", &request));
    let mut expected = string("/tp No");
    expected.extend([0, 0]);
    assert_eq!(out.data, expected);
    let mut answer = hex("01");
    answer.extend(string("Notch"));
    let out = only(clientbound(&mut step, "command_suggestions", &answer));
    let mut expected = hex("07040201");
    expected.extend(string("Notch"));
    expected.push(0);
    assert_eq!(out.data, expected);
    // The command name: 1.12 answers with slashes the range excludes.
    let mut request = hex("08");
    request.extend(string("/ti"));
    serverbound(&mut step, "command_suggestion", &request);
    let mut answer = hex("02");
    answer.extend(string("/time"));
    answer.extend(string("/title"));
    let out = only(clientbound(&mut step, "command_suggestions", &answer));
    let mut expected = hex("08010202");
    expected.extend(string("time"));
    expected.push(0);
    expected.extend(string("title"));
    expected.push(0);
    assert_eq!(out.data, expected);
    // An answer nobody asked for is not passed on.
    assert!(clientbound(&mut step, "command_suggestions", &hex("00")).is_empty());
}

#[test]
fn new_serverbound_packets_become_plugin_messages() {
    let mut step = Up::new();
    let message = |out: Out, channel: &str| {
        assert_eq!(out.name, "custom_payload");
        let mut reader = Reader::new(&out.data);
        assert_eq!(reader.string(20).unwrap(), channel);
        to_hex(reader.rest())
    };
    assert_eq!(
        message(
            only(serverbound(&mut step, "pick_item", &hex("05"))),
            "MC|PickItem"
        ),
        "05"
    );
    assert_eq!(
        message(
            only(serverbound(&mut step, "rename_item", &string("Sword"))),
            "MC|ItemName"
        ),
        to_hex(&string("Sword"))
    );
    assert_eq!(
        message(
            only(serverbound(&mut step, "select_trade", &hex("02"))),
            "MC|TrSel"
        ),
        "00000002"
    );
    assert_eq!(
        message(
            only(serverbound(&mut step, "set_beacon", &hex("0100"))),
            "MC|Beacon"
        ),
        "0000000100000000"
    );
    // Command block at (1, 2, 3), "say hi", redstone mode, conditional.
    let position = Position { x: 1, y: 2, z: 3 };
    let mut body = Writer::new();
    body.position_legacy(position)
        .string("say hi")
        .varint(2)
        .u8(2);
    let mut expected = Writer::new();
    expected
        .i32(1)
        .i32(2)
        .i32(3)
        .string("say hi")
        .bool(false)
        .string("REDSTONE")
        .bool(true)
        .bool(false);
    assert_eq!(
        message(
            only(serverbound(&mut step, "set_command_block", &body.buf)),
            "MC|AutoCmd"
        ),
        to_hex(&expected.buf)
    );
    let mut body = Writer::new();
    body.varint(42).string("say hi").bool(true);
    let mut expected = Writer::new();
    expected.u8(1).i32(42).string("say hi").bool(true);
    assert_eq!(
        message(
            only(serverbound(&mut step, "set_command_minecart", &body.buf)),
            "MC|AdvCmd"
        ),
        to_hex(&expected.buf)
    );
    let mut body = Writer::new();
    body.position_legacy(position)
        .varint(1) // Save.
        .varint(0) // Save mode.
        .string("house")
        .i8(0)
        .i8(1)
        .i8(-2)
        .i8(5)
        .i8(6)
        .i8(7)
        .varint(2)
        .varint(3)
        .string("")
        .f32(0.5)
        .varlong(99)
        .u8(5);
    let mut expected = Writer::new();
    expected
        .i32(1)
        .i32(2)
        .i32(3)
        .u8(2)
        .string("SAVE")
        .string("house")
        .i32(0)
        .i32(1)
        .i32(-2)
        .i32(5)
        .i32(6)
        .i32(7)
        .string("FRONT_BACK")
        .string("COUNTERCLOCKWISE_90")
        .string("")
        .bool(true)
        .bool(false)
        .bool(true)
        .f32(0.5)
        .varlong(99);
    assert_eq!(
        message(
            only(serverbound(&mut step, "set_structure_block", &body.buf)),
            "MC|Struct"
        ),
        to_hex(&expected.buf)
    );
    let mut query = hex("01");
    query.extend(0i64.to_be_bytes());
    assert!(serverbound(&mut step, "block_entity_tag_query", &query).is_empty());
    assert!(serverbound(&mut step, "entity_tag_query", &hex("0102")).is_empty());
    let out = send(
        &mut step,
        Direction::Serverbound,
        State::Login,
        "custom_query_answer",
        &hex("0100"),
    )
    .unwrap();
    assert!(out.is_empty());
    // Out-of-range enums fail.
    let mut bad = Writer::new();
    bad.position_legacy(position).string("x").varint(3).u8(0);
    assert!(
        send(
            &mut step,
            Direction::Serverbound,
            State::Play,
            "set_command_block",
            &bad.buf
        )
        .is_err()
    );
}

#[test]
fn effects_show_their_icon_when_they_show_particles() {
    let mut step = Up::new();
    // The real 1.12.2 speed effect: entity 0, speed (1), amplifier 1.
    let out = only(clientbound(
        &mut step,
        "update_mob_effect",
        &hex("0001019a0302"),
    ));
    assert_eq!(out.hex(), "0001019a0306");
    let out = only(clientbound(
        &mut step,
        "update_mob_effect",
        &hex("0001019a0301"),
    ));
    assert_eq!(out.hex(), "0001019a0301");
}

fn state(name: &str) -> i32 {
    data::version(NEW).block_states.parse(name).unwrap()
}

fn block_update(position: Position, state: i32) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.position_legacy(position).varint(state);
    writer.buf
}

#[test]
fn block_entities_complete_beds_banners_skulls_pots_and_note_blocks() {
    let mut step = Up::new();
    let position = Position {
        x: -346,
        y: 4,
        z: -377,
    };
    // A 1.12 bed (block 26, head facing south) arrives as a red bed...
    let out = only(clientbound(
        &mut step,
        "block_update",
        &block_update(position, 26 << 4 | 8),
    ));
    assert_eq!(
        out.data,
        block_update(position, state("red_bed[part=head,facing=south]"))
    );
    // ...and its block entity says it is blue (11): the client is told.
    let mut tag = crate::nbt::Compound::new();
    tag.insert("color", crate::nbt::Nbt::Int(11));
    tag.insert("x", crate::nbt::Nbt::Int(position.x));
    tag.insert("y", crate::nbt::Nbt::Int(position.y));
    tag.insert("z", crate::nbt::Nbt::Int(position.z));
    tag.insert("id", crate::nbt::Nbt::String("minecraft:bed".into()));
    let mut body = Writer::new();
    body.position_legacy(position).u8(11);
    crate::nbt::Nbt::Compound(tag).write(&mut body, true);
    let outs = clientbound(&mut step, "block_entity_data", &body.buf);
    assert_eq!(outs.len(), 2, "{outs:?}");
    assert_eq!(outs[0].name, "block_entity_data");
    assert_eq!(outs[1].name, "block_update");
    let blue = state("blue_bed[part=head,facing=south]");
    assert_eq!(outs[1].data, block_update(position, blue));
    // Later updates of the same bed stay blue; another block forgets it.
    let out = only(clientbound(
        &mut step,
        "block_update",
        &block_update(position, 26 << 4 | 9),
    ));
    assert_eq!(
        out.data,
        block_update(position, state("blue_bed[part=head,facing=west]"))
    );
    only(clientbound(
        &mut step,
        "block_update",
        &block_update(position, 0),
    ));
    let out = only(clientbound(
        &mut step,
        "block_update",
        &block_update(position, 26 << 4 | 8),
    ));
    assert_eq!(
        out.data,
        block_update(position, state("red_bed[part=head,facing=south]"))
    );

    // The real flower pot: block, then its plant (a poppy).
    let pot = Position {
        x: -349,
        y: 4,
        z: -377,
    };
    let out = only(clientbound(
        &mut step,
        "block_update",
        &hex("ffffa8c013fffe87c011"),
    ));
    assert_eq!(out.data, block_update(pot, state("flower_pot")));
    let outs = clientbound(
        &mut step,
        "block_entity_data",
        &hex(concat!(
            "ffffa8c013fffe87050a00000800044974656d00146d696e6563726166743a7265645f666c6f",
            "77657203000178fffffea303000179000000040300017afffffe870300044461746100000000",
            "080002696400146d696e6563726166743a666c6f7765725f706f7400"
        )),
    );
    assert_eq!(outs[1].data, block_update(pot, state("potted_poppy")));

    // A note block event tells instrument and note.
    let note = Position { x: 0, y: 64, z: 0 };
    only(clientbound(
        &mut step,
        "block_update",
        &block_update(note, 25 << 4),
    ));
    let mut event = Writer::new();
    event.position_legacy(note).u8(4).u8(12).varint(25);
    let outs = clientbound(&mut step, "block_event", &event.buf);
    assert_eq!(outs.len(), 2, "{outs:?}");
    assert_eq!(
        outs[1].data,
        block_update(
            note,
            state("note_block[instrument=bass,note=12,powered=false]")
        )
    );
}

/// The 1.13 chunk a chunk codec would make of a 1.12 one: states
/// flattened, biome bytes widened to ints, block entities kept.
fn flatten_chunk(body: &[u8], sky: bool) -> Vec<u8> {
    let mut column = Column::read(body, Format::Legacy, sky).unwrap();
    for section in column.sections.iter_mut().flatten() {
        for index in 0..world::chunk::SECTION_BLOCKS {
            let state = i32::from(section.get(index));
            let flat = legacy().flatten_block(state).unwrap_or(0);
            section.set(index, flat as u16);
        }
    }
    if let Some(biomes) = &mut column.biomes {
        *biomes = biomes
            .iter()
            .flat_map(|biome| i32::from(*biome).to_be_bytes())
            .collect();
    }
    column.write(Format::Flattened)
}

/// Run the world completion on a chunk as if a codec had produced `body`.
fn complete_chunk(step: &mut Up, original: &[u8], converted: Vec<u8>) -> Vec<u8> {
    let id = data::version(OLD).expect_packet(State::Play, Direction::Clientbound, "level_chunk");
    world::tap_chunk(step, &Packet::new(id, original.to_vec())).unwrap();
    let mut emitted = VecDeque::from([Emitted {
        direction: Direction::Clientbound,
        state: State::Play,
        packet: Packet::new(packets::ids().level_chunk, converted),
    }]);
    let mut cx = Cx {
        emitted: &mut emitted,
        client_protocol: NEW,
        backend_protocol: OLD,
    };
    world::complete(step, &mut cx, 0).unwrap();
    let completed = emitted.pop_front().unwrap().packet.data;
    dump(
        Direction::Clientbound,
        State::Play,
        "level_chunk",
        &completed,
    );
    collect(emitted);
    completed
}

#[test]
fn chunk_sections_round_trip_in_both_formats() {
    for (protocol, format) in [(OLD, Format::Legacy), (NEW, Format::Flattened)] {
        let Some(packets) = corpus(protocol) else {
            eprintln!("no corpus for {protocol}: skipped");
            continue;
        };
        let mut checked = 0;
        for packet in packets
            .iter()
            .filter(|packet| packet.name(protocol) == "level_chunk")
        {
            let column = Column::read(&packet.data, format, true).unwrap();
            assert_eq!(column.write(format), packet.data);
            // Re-encoding every section from its states parses back the same.
            let mut changed = column.clone();
            for section in changed.sections.iter_mut().flatten() {
                let first = section.get(0);
                section.set(0, first.wrapping_add(1));
                section.set(0, first);
            }
            let reread = Column::read(&changed.write(format), format, true).unwrap();
            for (a, b) in column.sections.iter().zip(&reread.sections) {
                match (a, b) {
                    (Some(a), Some(b)) => {
                        assert_eq!(a.states, b.states);
                        assert_eq!(a.light, b.light);
                    }
                    (None, None) => {}
                    _ => panic!("section presence changed"),
                }
            }
            checked += 1;
        }
        assert!(checked > 40, "{protocol}: {checked} chunks");
    }
}

/// A 1.12 chunk with `blocks` set and their block entities added.
fn with_blocks(body: &[u8], blocks: &[(Position, i32, crate::nbt::Compound)]) -> Vec<u8> {
    let mut column = Column::read(body, Format::Legacy, true).unwrap();
    let mut reader = Reader::new(&column.block_entities);
    let count = reader.varint().unwrap();
    let mut entities = Writer::new();
    entities
        .varint(count + blocks.len() as i32)
        .bytes(reader.rest());
    for (position, state, tag) in blocks {
        let section = column.sections[(position.y >> 4) as usize]
            .as_mut()
            .unwrap();
        let index = ((position.y & 15) << 8 | (position.z & 15) << 4 | (position.x & 15)) as usize;
        section.set(index, *state as u16);
        let mut tag = tag.clone();
        tag.insert("x", crate::nbt::Nbt::Int(position.x));
        tag.insert("y", crate::nbt::Nbt::Int(position.y));
        tag.insert("z", crate::nbt::Nbt::Int(position.z));
        crate::nbt::Nbt::Compound(tag).write(&mut entities, true);
    }
    column.block_entities = entities.buf;
    column.write(Format::Legacy)
}

fn compound(entries: &[(&str, crate::nbt::Nbt)]) -> crate::nbt::Compound {
    let mut tag = crate::nbt::Compound::new();
    for (key, value) in entries {
        tag.insert(*key, value.clone());
    }
    tag
}

#[test]
fn real_chunks_get_their_block_entity_states() {
    use crate::nbt::Nbt;
    let Some(packets) = corpus(OLD) else {
        eprintln!("no corpus for {OLD}: skipped");
        return;
    };
    let original = &packets
        .iter()
        .find(|packet| packet.name(OLD) == "level_chunk")
        .unwrap()
        .data;
    let x = i32::from_be_bytes(original[..4].try_into().unwrap()) << 4;
    let z = i32::from_be_bytes(original[4..8].try_into().unwrap()) << 4;
    let at = |dx, y, dz| Position {
        x: x + dx,
        y,
        z: z + dz,
    };
    let id = |name: &str| Nbt::String(format!("minecraft:{name}"));
    let blocks = [
        // A floor skull (Notch's head, rotation 7) and a wall skull facing
        // east (a creeper head).
        (
            at(1, 5, 1),
            144 << 4 | 1,
            compound(&[
                ("id", id("skull")),
                ("SkullType", Nbt::Byte(3)),
                ("Rot", Nbt::Byte(7)),
            ]),
        ),
        (
            at(2, 5, 1),
            144 << 4 | 5,
            compound(&[
                ("id", id("skull")),
                ("SkullType", Nbt::Byte(4)),
                ("Rot", Nbt::Byte(0)),
            ]),
        ),
        // A standing banner, Base 14 (orange in dye damage order).
        (
            at(3, 5, 1),
            176 << 4 | 4,
            compound(&[("id", id("banner")), ("Base", Nbt::Int(14))]),
        ),
        // A flower pot with a dark oak sapling, a green bed foot, a note
        // block on G (note 13).
        (
            at(4, 5, 1),
            140 << 4,
            compound(&[
                ("id", id("flower_pot")),
                ("Item", id("sapling")),
                ("Data", Nbt::Int(5)),
            ]),
        ),
        (
            at(5, 5, 1),
            26 << 4 | 1,
            compound(&[("id", id("bed")), ("color", Nbt::Int(13))]),
        ),
        (
            at(6, 5, 1),
            25 << 4,
            compound(&[
                ("id", id("noteblock")),
                ("note", Nbt::Byte(13)),
                ("powered", Nbt::Byte(0)),
            ]),
        ),
    ];
    let original = with_blocks(original, &blocks);
    let mut step = Up::new();
    let converted = flatten_chunk(&original, true);
    let completed = complete_chunk(&mut step, &original, converted);
    let column = Column::read(&completed, Format::Flattened, true).unwrap();
    let states = &data::version(NEW).block_states;
    let found: Vec<String> = blocks
        .iter()
        .map(|(position, _, _)| {
            let section = column.sections[(position.y >> 4) as usize]
                .as_ref()
                .unwrap();
            let index =
                ((position.y & 15) << 8 | (position.z & 15) << 4 | (position.x & 15)) as usize;
            states.describe(i32::from(section.get(index))).unwrap()
        })
        .collect();
    assert_eq!(
        found,
        [
            "player_head[rotation=7]",
            "creeper_wall_head[facing=east]",
            "orange_banner[rotation=4]",
            "potted_dark_oak_sapling",
            "green_bed[facing=west,occupied=false,part=foot]",
            "note_block[instrument=harp,note=13,powered=false]",
        ]
    );
    // Untouched columns are passed on byte for byte.
    let plain = &packets
        .iter()
        .filter(|packet| packet.name(OLD) == "level_chunk")
        .nth(3)
        .unwrap()
        .data;
    let converted = flatten_chunk(plain, true);
    assert_eq!(
        complete_chunk(&mut step, plain, converted.clone()),
        converted
    );
}

#[test]
fn malformed_input_fails_without_panicking() {
    let inputs: Vec<(Direction, &str, Vec<u8>)> = vec![
        (
            Direction::Clientbound,
            "login",
            hex("000000000100000000001404666c617400"),
        ),
        (
            Direction::Clientbound,
            "custom_payload",
            hex("084d437c4272616e640776616e696c6c61"),
        ),
        (Direction::Clientbound, "recipe", hex("00010002000b010c")),
        (Direction::Clientbound, "place_ghost_recipe", hex("0165")),
        (
            Direction::Clientbound,
            "update_mob_effect",
            hex("0001019a0302"),
        ),
        (
            Direction::Clientbound,
            "block_update",
            hex("ffffa8c013fffe87c011"),
        ),
        (
            Direction::Clientbound,
            "chunk_blocks_update",
            hex("ffffffeaffffffe8026604f006560400"),
        ),
        (
            Direction::Clientbound,
            "block_event",
            hex("ffffa8c013fffe8704050019"),
        ),
        (
            Direction::Clientbound,
            "level_chunk",
            hex("ffffffeaffffffe801018832"),
        ),
        (
            Direction::Serverbound,
            "custom_payload",
            hex("0f6d696e6563726166743a6272616e6409726966742d74657374"),
        ),
        (
            Direction::Serverbound,
            "set_structure_block",
            hex("00000000000000000100"),
        ),
        (Direction::Serverbound, "command_suggestion", hex("0703")),
    ];
    for (direction, name, body) in inputs {
        for cut in 0..body.len() {
            let mut step = Up::new();
            let _ = send(&mut step, direction, State::Play, name, &body[..cut]);
        }
        let mut step = Up::new();
        let mut long = body.clone();
        long.extend([0xff; 7]);
        let _ = send(&mut step, direction, State::Play, name, &long);
    }
}

#[test]
fn translation_keys_follow_1_13_names() {
    use translations::upgrade_translation_key as upgrade;
    assert_eq!(
        upgrade("tile.stone.stone.name"),
        Some("block.minecraft.stone")
    );
    assert_eq!(
        upgrade("entity.Creeper.name"),
        Some("entity.minecraft.creeper")
    );
    assert_eq!(
        upgrade("item.swordStone.name"),
        Some("item.minecraft.stone_sword")
    );
    assert_eq!(upgrade("chat.type.text"), None); // Unchanged.
    assert_eq!(upgrade("no.such.key"), None);
}

#[test]
fn whole_session_replays_without_step_errors() {
    let Some(packets) = corpus(OLD) else {
        eprintln!("no corpus for {OLD}: skipped");
        return;
    };
    let mut step = Up::new();
    let mut failures = Vec::new();
    for Recorded {
        direction,
        state,
        id,
        data,
    } in packets
        .iter()
        .filter(|packet| packet.direction == Direction::Clientbound)
        .cloned()
    {
        let name = data::version(OLD)
            .packet_name(state, direction, id)
            .unwrap_or("?");
        if let Err(error) = send_id(&mut step, direction, state, id, &data) {
            failures.push(format!("{name}: {error}"));
        }
    }
    // Only codec-owned packets may fail until the codecs land; none of the
    // step's own.
    let own = [
        "login",
        "custom_payload",
        "recipe",
        "place_ghost_recipe",
        "command_suggestions",
        "update_mob_effect",
        "block_update",
        "chunk_blocks_update",
    ];
    for failure in &failures {
        let name = failure.split(':').next().unwrap();
        assert!(!own.contains(&name), "{failure}");
    }
    let _ = Rewriter::new(&[]);
}

/// A full column at (x, z) whose section 0 holds `blocks` (1.13 states)
/// in air, as the 1.12 server sent it and as a chunk codec converted it.
fn column(x: i32, z: i32, blocks: &[((i32, i32, i32), &str)]) -> (Vec<u8>, Vec<u8>) {
    let mut states = Box::new([0u16; world::chunk::SECTION_BLOCKS]);
    for ((bx, by, bz), name) in blocks {
        states[(by << 8 | bz << 4 | bx) as usize] = state(name) as u16;
    }
    let mut sections: [Option<world::chunk::Section>; 16] = Default::default();
    sections[0] = Some(world::chunk::Section::new(states, vec![0xff; 4096]));
    let mut column = Column {
        x,
        z,
        full: true,
        sections,
        biomes: Some(vec![1; 256]),
        block_entities: vec![0],
    };
    let original = column.write(Format::Legacy);
    column.biomes = Some([0, 0, 0, 1].repeat(256));
    (original, column.write(Format::Flattened))
}

/// Load a synthetic column through the world completion; returns its
/// completed 1.13 states by name at `probes`, and the extra packets.
fn load(step: &mut Up, x: i32, z: i32, blocks: &[((i32, i32, i32), &str)]) -> (Column, Vec<Out>) {
    let (original, converted) = column(x, z, blocks);
    let id = data::version(OLD).expect_packet(State::Play, Direction::Clientbound, "level_chunk");
    world::tap_chunk(step, &Packet::new(id, original)).unwrap();
    let mut emitted = VecDeque::from([Emitted {
        direction: Direction::Clientbound,
        state: State::Play,
        packet: Packet::new(packets::ids().level_chunk, converted),
    }]);
    let mut cx = Cx {
        emitted: &mut emitted,
        client_protocol: NEW,
        backend_protocol: OLD,
    };
    world::complete(step, &mut cx, 0).unwrap();
    let chunk = emitted.pop_front().unwrap().packet.data;
    dump(Direction::Clientbound, State::Play, "level_chunk", &chunk);
    (
        Column::read(&chunk, Format::Flattened, true).unwrap(),
        collect(emitted),
    )
}

fn name_at(column: &Column, (x, y, z): (i32, i32, i32)) -> String {
    let section = column.sections[(y >> 4) as usize].as_ref().unwrap();
    let state = section.get(((y & 15) << 8 | (z & 15) << 4 | (x & 15)) as usize);
    data::version(NEW)
        .block_states
        .describe(i32::from(state))
        .unwrap()
}

#[test]
fn fences_panes_and_walls_connect_in_chunks_and_on_updates() {
    let mut step = Up::new();
    let (column, extra) = load(
        &mut step,
        0,
        0,
        &[
            ((1, 1, 1), "oak_fence"),
            ((2, 1, 1), "oak_fence"),
            ((2, 1, 2), "oak_fence"),
            ((0, 1, 1), "stone"),
            ((3, 1, 1), "nether_brick_fence"),
            ((5, 1, 5), "glass_pane"),
            ((6, 1, 5), "iron_bars"),
            ((5, 1, 4), "glass"),
            ((8, 1, 8), "cobblestone_wall"),
            ((8, 1, 9), "cobblestone_wall"),
            ((8, 1, 10), "cobblestone_wall"),
            ((9, 1, 8), "oak_fence_gate[facing=north]"),
        ],
    );
    assert!(extra.is_empty(), "{extra:?}");
    let at = |position| name_at(&column, position);
    // East to the other oak fence and west to stone; not to the nether
    // fence, which is another material.
    assert_eq!(
        at((1, 1, 1)),
        "oak_fence[east=true,north=false,south=false,waterlogged=false,west=true]"
    );
    assert_eq!(
        at((2, 1, 1)),
        "oak_fence[east=false,north=false,south=true,waterlogged=false,west=true]"
    );
    // Panes join each other but not glass blocks (vanilla 1.12 excepts it).
    assert_eq!(
        at((5, 1, 5)),
        "glass_pane[east=true,north=false,south=false,waterlogged=false,west=false]"
    );
    // A straight wall with air above has no post; the corner has one and
    // joins the gate along the gate's line.
    assert_eq!(
        at((8, 1, 9)),
        "cobblestone_wall[east=false,north=true,south=true,up=false,waterlogged=false,west=false]"
    );
    assert_eq!(
        at((8, 1, 8)),
        "cobblestone_wall[east=true,north=false,south=true,up=true,waterlogged=false,west=false]"
    );
    assert_eq!(
        at((9, 1, 8)),
        "oak_fence_gate[facing=north,in_wall=true,open=false,powered=false]"
    );

    // The backend removes the stone: the fence beside it is updated.
    let outs = clientbound(
        &mut step,
        "block_update",
        &block_update(Position { x: 0, y: 1, z: 1 }, 0),
    );
    assert_eq!(outs.len(), 2, "{outs:?}");
    assert_eq!(outs[0].data, block_update(Position { x: 0, y: 1, z: 1 }, 0));
    assert_eq!(
        outs[1].data,
        block_update(
            Position { x: 1, y: 1, z: 1 },
            state("oak_fence[east=true,north=false,south=false,waterlogged=false,west=false]")
        )
    );
    // A fence placed (1.12 block 85) arrives connected.
    let outs = clientbound(
        &mut step,
        "block_update",
        &block_update(Position { x: 1, y: 1, z: 0 }, 85 << 4),
    );
    assert_eq!(
        outs[0].data,
        block_update(
            Position { x: 1, y: 1, z: 0 },
            state("oak_fence[east=false,north=false,south=true,waterlogged=false,west=false]")
        )
    );
    assert_eq!(outs[1].name, "block_update");
}

#[test]
fn neighbour_columns_are_reconnected_along_their_edges() {
    let mut step = Up::new();
    load(&mut step, 0, 0, &[((15, 2, 7), "oak_fence")]);
    let (column, extra) = load(&mut step, 1, 0, &[((0, 2, 7), "oak_fence")]);
    assert_eq!(
        name_at(&column, (16, 2, 7)),
        "oak_fence[east=false,north=false,south=false,waterlogged=false,west=true]"
    );
    let out = only(extra);
    assert_eq!(out.name, "block_update");
    assert_eq!(
        out.data,
        block_update(
            Position { x: 15, y: 2, z: 7 },
            state("oak_fence[east=true,north=false,south=false,waterlogged=false,west=false]")
        )
    );
    // Unloading the second column leaves the first as the client has it.
    let mut forget = Writer::new();
    forget.i32(1).i32(0);
    clientbound(&mut step, "forget_level_chunk", &forget.buf);
    let outs = clientbound(
        &mut step,
        "block_update",
        &block_update(Position { x: 15, y: 3, z: 7 }, 1 << 4),
    );
    assert_eq!(outs.len(), 1, "{outs:?}");
}

#[test]
fn stairs_redstone_doors_plants_chests_and_more_get_their_shapes() {
    let mut step = Up::new();
    let (column, _) = load(
        &mut step,
        0,
        0,
        &[
            // An outer corner: stairs facing east with stairs facing north
            // behind them.
            ((1, 1, 1), "oak_stairs[facing=east,half=bottom]"),
            ((2, 1, 1), "oak_stairs[facing=north,half=bottom]"),
            // Redstone running east into a repeater facing west, and up a
            // stone block to wire on top.
            ((4, 1, 4), "redstone_wire"),
            ((5, 1, 4), "repeater[facing=west]"),
            ((4, 1, 5), "stone"),
            ((4, 2, 5), "redstone_wire"),
            // Door halves as 1.12 flattens them: facing on the lower half,
            // hinge on the upper.
            ((7, 1, 7), "oak_door[half=lower,facing=east,open=true]"),
            ((7, 2, 7), "oak_door[half=upper,hinge=right]"),
            // A lilac whose upper half 1.12 sent without its type.
            ((9, 1, 9), "lilac[half=lower]"),
            ((9, 2, 9), "sunflower[half=upper]"),
            // Two chests facing south side by side.
            ((11, 1, 11), "chest[facing=south]"),
            ((12, 1, 11), "chest[facing=south]"),
            // A grown melon stem by a melon, chorus on end stone, snow on
            // grass, fire beside planks over air, a vine under stone.
            ((1, 1, 12), "melon_stem[age=7]"),
            ((1, 1, 13), "melon"),
            ((3, 1, 12), "end_stone"),
            ((3, 2, 12), "chorus_plant"),
            ((3, 3, 12), "chorus_flower"),
            ((5, 1, 12), "grass_block"),
            ((5, 2, 12), "snow"),
            ((7, 4, 12), "fire"),
            ((8, 4, 12), "oak_planks"),
            ((10, 4, 12), "vine[north=true]"),
            ((10, 5, 12), "stone"),
            ((12, 1, 1), "tripwire"),
            ((13, 1, 1), "tripwire_hook[facing=west]"),
        ],
    );
    let at = |position| name_at(&column, position);
    assert_eq!(
        at((1, 1, 1)),
        "oak_stairs[facing=east,half=bottom,shape=outer_left,waterlogged=false]"
    );
    assert_eq!(
        at((4, 1, 4)),
        "redstone_wire[east=side,north=none,power=0,south=up,west=none]"
    );
    assert_eq!(
        at((7, 1, 7)),
        "oak_door[facing=east,half=lower,hinge=right,open=true,powered=false]"
    );
    assert_eq!(
        at((7, 2, 7)),
        "oak_door[facing=east,half=upper,hinge=right,open=true,powered=false]"
    );
    assert_eq!(at((9, 2, 9)), "lilac[half=upper]");
    assert_eq!(
        at((11, 1, 11)),
        "chest[facing=south,type=right,waterlogged=false]"
    );
    assert_eq!(
        at((12, 1, 11)),
        "chest[facing=south,type=left,waterlogged=false]"
    );
    assert_eq!(at((1, 1, 12)), "attached_melon_stem[facing=south]");
    assert_eq!(
        at((3, 2, 12)),
        "chorus_plant[down=true,east=false,north=false,south=false,up=true,west=false]"
    );
    assert_eq!(at((5, 1, 12)), "grass_block[snowy=true]");
    assert_eq!(
        at((7, 4, 12)),
        "fire[age=0,east=true,north=false,south=false,up=false,west=false]"
    );
    assert_eq!(
        at((10, 4, 12)),
        "vine[east=false,north=true,south=false,up=true,west=false]"
    );
    assert_eq!(
        at((12, 1, 1)),
        "tripwire[attached=false,disarmed=false,east=true,north=false,powered=false,south=false,west=false]"
    );

    // The melon is harvested: the stem straightens again.
    let outs = clientbound(
        &mut step,
        "block_update",
        &block_update(Position { x: 1, y: 1, z: 13 }, 0),
    );
    assert_eq!(outs.len(), 2, "{outs:?}");
    assert_eq!(
        outs[1].data,
        block_update(Position { x: 1, y: 1, z: 12 }, state("melon_stem[age=7]"))
    );
    // An explosion takes the planks: the fire beside them goes out of the
    // wall and the client is told.
    let mut explode = Writer::new();
    explode
        .f32(8.5)
        .f32(4.5)
        .f32(12.5)
        .f32(2.0)
        .i32(1)
        .i8(0)
        .i8(0)
        .i8(0)
        .f32(0.0)
        .f32(0.0)
        .f32(0.0);
    let outs = clientbound(&mut step, "explode", &explode.buf);
    assert_eq!(outs.len(), 2, "{outs:?}");
    assert_eq!(
        outs[1].data,
        block_update(
            Position { x: 7, y: 4, z: 12 },
            state("fire[age=0,east=false,north=false,south=false,up=false,west=false]")
        )
    );
}

#[test]
fn the_block_store_stays_small_on_real_chunks() {
    let Some(packets) = corpus(OLD) else {
        eprintln!("no corpus for {OLD}: skipped");
        return;
    };
    let mut step = Up::new();
    let mut columns = 0;
    for packet in packets
        .iter()
        .filter(|packet| packet.name(OLD) == "level_chunk")
    {
        complete_chunk(&mut step, &packet.data, flatten_chunk(&packet.data, true));
        columns += 1;
    }
    let bytes = step.world.store_bytes();
    eprintln!("{columns} columns in {bytes} bytes");
    assert!(
        bytes / columns < 4096,
        "{} bytes per column",
        bytes / columns
    );
}
