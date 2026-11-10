use super::{NEWER, OLDER, down::Down, entity_data, layout, trades, up::Up};
use crate::{
    Direction, Packet, State,
    codec::{Shared, testing::corpus},
    data,
    step::{Cx, Emitted, Step},
    wire::{Position, Reader, Writer},
};
use std::collections::VecDeque;

/// A packet a step emitted, by name in the protocol it is in.
#[derive(Debug, PartialEq, Eq)]
struct Out {
    direction: Direction,
    name: String,
    data: Vec<u8>,
}

fn protocols(step_is_up: bool) -> (i32, i32) {
    if step_is_up {
        (NEWER, OLDER)
    } else {
        (OLDER, NEWER)
    }
}

/// Send the packet `name` (in the sender's protocol) through `step`.
fn send(
    step: &mut dyn Step,
    up: bool,
    direction: Direction,
    name: &str,
    body: &[u8],
) -> std::io::Result<Vec<Out>> {
    let (client, backend) = protocols(up);
    let source = match direction {
        Direction::Clientbound => backend,
        Direction::Serverbound => client,
    };
    let id = data::version(source).expect_packet(State::Play, direction, name);
    let mut emitted = VecDeque::new();
    let mut cx = Cx {
        emitted: &mut emitted,
        client_protocol: client,
        backend_protocol: backend,
    };
    let packet = Packet::new(id, body.to_vec());
    match direction {
        Direction::Clientbound => step.clientbound(&mut cx, State::Play, packet)?,
        Direction::Serverbound => step.serverbound(&mut cx, State::Play, packet)?,
    }
    Ok(emitted
        .into_iter()
        .map(
            |Emitted {
                 direction, packet, ..
             }| {
                let protocol = match direction {
                    Direction::Clientbound => client,
                    Direction::Serverbound => backend,
                };
                Out {
                    direction,
                    name: data::version(protocol)
                        .packet_name(State::Play, direction, packet.id)
                        .unwrap_or("?")
                        .to_owned(),
                    data: packet.data,
                }
            },
        )
        .collect())
}

fn up(step: &mut Up, direction: Direction, name: &str, body: &[u8]) -> Vec<Out> {
    send(step, true, direction, name, body).unwrap()
}

fn down(step: &mut Down, direction: Direction, name: &str, body: &[u8]) -> Vec<Out> {
    send(step, false, direction, name, body).unwrap()
}

fn out(direction: Direction, name: &str, data: &[u8]) -> Out {
    Out {
        direction,
        name: name.to_owned(),
        data: data.to_vec(),
    }
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

use Direction::{Clientbound, Serverbound};

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

#[test]
fn join_game_moves_the_difficulty_and_gains_a_view_distance() {
    // The official 1.13.2 server's join game: entity 0, creative, overworld,
    // peaceful, 20 players, flat.
    let login_1_13 = hex("000000000100000000001404666c617400");
    let mut step = Up::new();
    let sent = up(&mut step, Clientbound, "login", &login_1_13);
    let mut expected = hex("0000000001000000001404666c6174");
    expected.extend([64, 0]);
    assert_eq!(
        sent,
        [
            out(Clientbound, "login", &expected),
            out(Clientbound, "change_difficulty", &[0, 0]),
        ]
    );

    // The official 1.14 server's join game (view distance 2) after it said
    // the difficulty is easy.
    let login_1_14 = hex("0000000101000000001404666c61740200");
    let mut step = Down::new();
    let sent = down(&mut step, Clientbound, "change_difficulty", &[1, 1]);
    assert_eq!(sent, [out(Clientbound, "change_difficulty", &[1])]);
    let sent = down(&mut step, Clientbound, "login", &login_1_14);
    assert_eq!(
        sent,
        [out(
            Clientbound,
            "login",
            &hex("00000001010000000001140466 6c617400"
                .replace(' ', "")
                .as_str())
        )]
    );
}

#[test]
fn respawn_moves_the_difficulty_both_ways() {
    let mut step = Up::new();
    // Nether, hard, survival, default.
    let mut body = Writer::new();
    body.i32(-1).u8(3).u8(0).string("default");
    let sent = up(&mut step, Clientbound, "respawn", &body.buf);
    let mut expected = Writer::new();
    expected.i32(-1).u8(0).string("default");
    assert_eq!(
        sent,
        [
            out(Clientbound, "respawn", &expected.buf),
            out(Clientbound, "change_difficulty", &[3, 0]),
        ]
    );

    let mut step = Down::new();
    down(&mut step, Clientbound, "change_difficulty", &[3, 0]);
    let sent = down(&mut step, Clientbound, "respawn", &expected.buf);
    assert_eq!(sent, [out(Clientbound, "respawn", &body.buf)]);
}

fn position(x: i32, y: i32, z: i32) -> Position {
    Position { x, y, z }
}

#[test]
fn block_positions_are_repacked_in_every_packet_that_has_one() {
    let at = position(-1234, 70, 98765);
    let legacy = at.to_legacy().to_be_bytes();
    let modern = at.to_modern().to_be_bytes();
    let with = |prefix: &[u8], position: &[u8], suffix: &[u8]| [prefix, position, suffix].concat();
    let cases: [(Direction, &str, &[u8], &[u8]); 8] = [
        (Clientbound, "block_destruction", &[5], &[7]),
        (Clientbound, "open_sign_editor", &[], &[]),
        (Clientbound, "set_default_spawn_position", &[], &[]),
        (Serverbound, "player_action", &[0], &[1]),
        (Serverbound, "block_entity_tag_query", &[9], &[]),
        (
            Serverbound,
            "set_command_block",
            &[],
            &[2, b'/', b'a', 1, 4],
        ),
        (
            Serverbound,
            "sign_update",
            &[],
            &[1, b'a', 0, 1, b'b', 3, b'c', b'c', b'c'],
        ),
        (
            Serverbound,
            "set_structure_block",
            &[],
            &[
                1, 2, 1, b's', 0, 1, 2, 3, 4, 5, 0, 0, 0, 0x3f, 0x80, 0, 0, 0, 1,
            ],
        ),
    ];
    for (direction, name, prefix, suffix) in cases {
        // The older side's packing on the way to the newer side and back.
        let (older, newer) = (with(prefix, &legacy, suffix), with(prefix, &modern, suffix));
        let (mut up_step, mut down_step) = (Up::new(), Down::new());
        let (from_up, from_down) = match direction {
            Clientbound => (
                up(&mut up_step, direction, name, &older),
                down(&mut down_step, direction, name, &newer),
            ),
            Serverbound => (
                up(&mut up_step, direction, name, &newer),
                down(&mut down_step, direction, name, &older),
            ),
        };
        let (to_up, to_down) = match direction {
            Clientbound => (newer.clone(), older.clone()),
            Serverbound => (older.clone(), newer.clone()),
        };
        assert_eq!(from_up, [out(direction, name, &to_up)], "{name} up");
        assert_eq!(from_down, [out(direction, name, &to_down)], "{name} down");
    }
}

#[test]
fn use_item_on_reorders_the_hand_and_drops_the_inside_flag() {
    let at = position(10, 64, -3);
    let mut newer = Writer::new();
    newer
        .varint(1)
        .position(at)
        .varint(4)
        .f32(0.5)
        .f32(1.0)
        .f32(0.25)
        .bool(true);
    let mut older = Writer::new();
    older
        .position_legacy(at)
        .varint(4)
        .varint(1)
        .f32(0.5)
        .f32(1.0)
        .f32(0.25);
    let sent = up(&mut Up::new(), Serverbound, "use_item_on", &newer.buf);
    assert_eq!(sent, [out(Serverbound, "use_item_on", &older.buf)]);
    let sent = down(&mut Down::new(), Serverbound, "use_item_on", &older.buf);
    let mut expected = newer.buf.clone();
    *expected.last_mut().unwrap() = 0;
    assert_eq!(sent, [out(Serverbound, "use_item_on", &expected)]);
}

#[test]
fn recipe_book_settings_gain_and_lose_the_new_books() {
    let older = [1, 1, 0, 1, 1];
    let newer = [1, 1, 0, 1, 1, 1, 1, 0, 1];
    let sent = up(&mut Up::new(), Serverbound, "recipe_book_update", &newer);
    assert_eq!(sent, [out(Serverbound, "recipe_book_update", &older)]);
    let sent = down(&mut Down::new(), Serverbound, "recipe_book_update", &older);
    assert_eq!(
        sent,
        [out(
            Serverbound,
            "recipe_book_update",
            &[1, 1, 0, 1, 1, 0, 0, 0, 0]
        )]
    );
    // Seeing a recipe is the same in both.
    let seen = [0, 4, b'a', b':', b'b', b'c'];
    let sent = up(&mut Up::new(), Serverbound, "recipe_book_update", &seen);
    assert_eq!(sent, [out(Serverbound, "recipe_book_update", &seen)]);
    assert!(
        send(
            &mut Up::new(),
            true,
            Serverbound,
            "recipe_book_update",
            &[2]
        )
        .is_err()
    );
}

#[test]
fn difficulty_and_jigsaw_packets_of_1_14_clients_are_dropped() {
    let mut step = Up::new();
    for (name, body) in [
        ("change_difficulty", &[2u8][..]),
        ("lock_difficulty", &[1]),
        ("set_jigsaw_block", &[0; 11]),
    ] {
        assert_eq!(up(&mut step, Serverbound, name, body), []);
    }
    assert_eq!(
        up(&mut step, Clientbound, "change_difficulty", &[2]),
        [out(Clientbound, "change_difficulty", &[2, 0])]
    );
}

fn center(x: i32, z: i32) -> Out {
    let mut w = Writer::new();
    w.varint(x).varint(z);
    out(Clientbound, "set_chunk_cache_center", &w.buf)
}

fn player_position(x: f64, y: f64, z: f64, relative: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.f64(x)
        .f64(y)
        .f64(z)
        .f32(0.0)
        .f32(0.0)
        .u8(relative)
        .varint(1);
    w.buf
}

#[test]
fn the_chunk_cache_centre_follows_the_player_going_up() {
    let mut step = Up::new();
    up(
        &mut step,
        Clientbound,
        "login",
        &hex("000000000100000000001404666c617400"),
    );
    // A 1.13 server sends chunks first: the first one sets a centre, the
    // rest are within the cache.
    let mut chunk = Writer::new();
    chunk.i32(9).i32(-16).bool(true);
    let sent = up(&mut step, Clientbound, "level_chunk", &chunk.buf);
    assert_eq!(sent[0], center(9, -16));
    assert_eq!(sent[1].name, "level_chunk");
    let mut near = Writer::new();
    near.i32(12).i32(-14).bool(true);
    assert_eq!(
        up(&mut step, Clientbound, "level_chunk", &near.buf).len(),
        1
    );
    // The teleport puts the player somewhere; walking across a chunk border
    // moves the centre, walking within a chunk does not.
    let teleport = player_position(150.5, 64.0, -250.5, 0);
    // It lands in the chunk the centre already is.
    let sent = up(&mut step, Clientbound, "player_position", &teleport);
    assert_eq!(sent, [out(Clientbound, "player_position", &teleport)]);
    let walk = |x: f64, z: f64| {
        let mut w = Writer::new();
        w.f64(x).f64(64.0).f64(z).bool(true);
        w.buf
    };
    let sent = up(
        &mut step,
        Serverbound,
        "move_player_pos",
        &walk(151.0, -250.0),
    );
    assert_eq!(
        sent,
        [out(Serverbound, "move_player_pos", &walk(151.0, -250.0))]
    );
    let sent = up(
        &mut step,
        Serverbound,
        "move_player_pos",
        &walk(160.0, -250.0),
    );
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1], center(10, -16));
    // A relative teleport adds to the known position.
    let sent = up(
        &mut step,
        Clientbound,
        "player_position",
        &player_position(-40.0, 0.0, 0.0, 0b111),
    );
    assert_eq!(sent[0], center(7, -16));
    // A chunk far outside the cache moves the centre to it.
    let mut far = Writer::new();
    far.i32(1000).i32(0).bool(true);
    let sent = up(&mut step, Clientbound, "level_chunk", &far.buf);
    assert_eq!(sent[0], center(1000, 0));
    // A respawn starts a new cache.
    let mut respawn = Writer::new();
    respawn.i32(-1).u8(1).u8(0).string("default");
    up(&mut step, Clientbound, "respawn", &respawn.buf);
    let sent = up(&mut step, Clientbound, "level_chunk", &near.buf);
    assert_eq!(sent[0], center(12, -14));
}

#[test]
fn chunk_cache_packets_are_dropped_going_down() {
    let mut step = Down::new();
    assert_eq!(
        down(&mut step, Clientbound, "set_chunk_cache_center", &[1, 2]),
        []
    );
    assert_eq!(
        down(&mut step, Clientbound, "set_chunk_cache_radius", &[8]),
        []
    );
}

#[test]
fn sleeping_moves_between_a_packet_and_entity_data() {
    let bed = position(100, 65, -20);
    let mut step = Up::new();
    up(
        &mut step,
        Clientbound,
        "login",
        &hex("000000070100000000001404666c617400"),
    );
    // Another player: sleeping position and the sleeping pose.
    let mut sleep = Writer::new();
    sleep.varint(42).position_legacy(bed);
    let sent = up(&mut step, Clientbound, "player_sleep", &sleep.buf);
    assert_eq!(
        sent,
        [out(
            Clientbound,
            "set_entity_data",
            &entity_data::sleep_data(42, Some(bed), Some(entity_data::SLEEPING))
        )]
    );
    // The player's own entity: the client works out its pose itself.
    let mut own = Writer::new();
    own.varint(7).position_legacy(bed);
    let sent = up(&mut step, Clientbound, "player_sleep", &own.buf);
    assert_eq!(
        sent,
        [out(
            Clientbound,
            "set_entity_data",
            &entity_data::sleep_data(7, Some(bed), None)
        )]
    );
    // Waking up clears both after the animation.
    let sent = up(&mut step, Clientbound, "animate", &[42, 2]);
    assert_eq!(
        sent,
        [
            out(Clientbound, "animate", &[42, 2]),
            out(
                Clientbound,
                "set_entity_data",
                &entity_data::sleep_data(42, None, Some(entity_data::STANDING))
            ),
        ]
    );
    assert_eq!(
        up(&mut step, Clientbound, "animate", &[42, 2]),
        [out(Clientbound, "animate", &[42, 2])]
    );

    // Going down, a player's sleeping position in entity data becomes
    // `player_sleep` after the data itself.
    let mut step = Down::new();
    down(
        &mut step,
        Clientbound,
        "login",
        &hex("0000000701000000001404666c61740200"),
    );
    let data = entity_data::sleep_data(7, Some(bed), None);
    let sent = down(&mut step, Clientbound, "set_entity_data", &data);
    let mut expected = Writer::new();
    expected.varint(7).position_legacy(bed);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].name, "set_entity_data");
    assert_eq!(sent[1], out(Clientbound, "player_sleep", &expected.buf));
    // Not for entities that are not players, nor when the bed is cleared.
    let data = entity_data::sleep_data(99, Some(bed), None);
    assert_eq!(
        down(&mut step, Clientbound, "set_entity_data", &data).len(),
        1
    );
    let data = entity_data::sleep_data(7, None, Some(0));
    assert_eq!(
        down(&mut step, Clientbound, "set_entity_data", &data).len(),
        1
    );
}

/// The official 1.14 server's `add_player` for the recording player, with
/// entity data that includes an item stack and other values to skip.
#[test]
fn a_player_coming_into_view_asleep_lies_in_bed_going_down() {
    let bed = position(-5, 70, 12);
    let mut body = Writer::new();
    body.varint(12)
        .uuid(0x1234)
        .f64(1.0)
        .f64(2.0)
        .f64(3.0)
        .i8(0)
        .i8(0);
    // Name (optional text), a float, a stack, the pose, then the bed.
    body.u8(2)
        .varint(5)
        .bool(true)
        .string("{\"text\":\"x\"}")
        .u8(8)
        .varint(2)
        .f32(20.0);
    body.u8(13).varint(6).bool(true).varint(1).i8(1).u8(0);
    body.u8(6).varint(18).varint(2);
    body.u8(12).varint(10).bool(true).position(bed).u8(0xff);
    let mut step = Down::new();
    let sent = down(&mut step, Clientbound, "add_player", &body.buf);
    let mut expected = Writer::new();
    expected.varint(12).position_legacy(bed);
    assert_eq!(sent[0], out(Clientbound, "add_player", &body.buf));
    assert_eq!(sent[1], out(Clientbound, "player_sleep", &expected.buf));
    // A later death clears their equipment for the 1.13 client.
    let sent = down(&mut step, Clientbound, "entity_event", &[0, 0, 0, 12, 3]);
    assert_eq!(sent.len(), 7);
    assert_eq!(sent[6], out(Clientbound, "set_equipped_item", &[12, 5, 0]));
}

#[test]
fn the_players_own_death_event_is_not_passed_down() {
    let mut step = Down::new();
    down(
        &mut step,
        Clientbound,
        "login",
        &hex("0000000701000000001404666c61740200"),
    );
    assert_eq!(
        down(&mut step, Clientbound, "entity_event", &[0, 0, 0, 7, 3]),
        []
    );
    assert_eq!(
        down(&mut step, Clientbound, "entity_event", &[0, 0, 0, 7, 9]).len(),
        1
    );
    assert_eq!(
        down(&mut step, Clientbound, "entity_event", &[0, 0, 0, 8, 3]).len(),
        1
    );
}

#[test]
fn entity_sounds_play_where_the_entity_is_going_down() {
    let mut step = Down::new();
    let mut spawn = Writer::new();
    spawn
        .varint(30)
        .uuid(1)
        .varint(73)
        .f64(10.0)
        .f64(64.5)
        .f64(-7.25)
        .i8(0)
        .i8(0)
        .i32(0)
        .i16(0)
        .i16(0)
        .i16(0);
    down(&mut step, Clientbound, "add_entity", &spawn.buf);
    let mut moved = Writer::new();
    moved.varint(30).i16(4096).i16(0).i16(-2048).bool(false);
    down(&mut step, Clientbound, "move_entity_pos", &moved.buf);
    let throw = data::version(NEWER)
        .expect_registry("sound_event")
        .id("item.trident.throw")
        .unwrap();
    let mut sound = Writer::new();
    sound.varint(throw).varint(7).varint(30).f32(1.0).f32(1.0);
    let sent = down(&mut step, Clientbound, "sound_entity", &sound.buf);
    let older = data::version(OLDER)
        .expect_registry("sound_event")
        .id("item.trident.throw")
        .unwrap();
    let mut expected = Writer::new();
    expected
        .varint(older)
        .varint(7)
        .i32(88)
        .i32(516)
        .i32(-62)
        .f32(1.0)
        .f32(1.0);
    assert_eq!(sent, [out(Clientbound, "sound", &expected.buf)]);
    // An entity the step does not know of, or that is gone, has no sound.
    let mut remove = Writer::new();
    remove.varint(1).varint(30);
    down(&mut step, Clientbound, "remove_entities", &remove.buf);
    assert_eq!(down(&mut step, Clientbound, "sound_entity", &sound.buf), []);
}

#[test]
fn trade_lists_book_signals_and_horse_windows_change_packets() {
    // An empty trade list keeps its window and gains the 1.14 fields.
    let mut step = Up::new();
    let mut payload = Writer::new();
    payload.string(trades::TRADER_LIST).i32(3).u8(0);
    let sent = up(&mut step, Clientbound, "custom_payload", &payload.buf);
    assert_eq!(
        sent,
        [out(Clientbound, "merchant_offers", &[3, 0, 0, 0, 0])]
    );
    let mut step = Down::new();
    let sent = down(&mut step, Clientbound, "merchant_offers", &[3, 0, 2, 9, 1]);
    assert_eq!(sent, [out(Clientbound, "custom_payload", &payload.buf)]);

    let mut book = Writer::new();
    book.string("minecraft:book_open").varint(1);
    let sent = up(&mut Up::new(), Clientbound, "custom_payload", &book.buf);
    assert_eq!(sent, [out(Clientbound, "open_book", &[1])]);
    let sent = down(&mut Down::new(), Clientbound, "open_book", &[1]);
    assert_eq!(sent, [out(Clientbound, "custom_payload", &book.buf)]);

    // Other channels, such as the brand, pass through.
    let brand = hex("0f6d696e6563726166743a6272616e640776616e696c6c61");
    let sent = up(&mut Up::new(), Clientbound, "custom_payload", &brand);
    assert_eq!(sent, [out(Clientbound, "custom_payload", &brand)]);

    let mut horse = Writer::new();
    horse
        .u8(4)
        .string("EntityHorse")
        .string(r#"{"translate":"entity.minecraft.horse"}"#)
        .u8(17)
        .i32(55);
    let mount = [4, 17, 0, 0, 0, 55];
    let sent = up(&mut Up::new(), Clientbound, "open_screen", &horse.buf);
    assert_eq!(sent, [out(Clientbound, "mount_screen_open", &mount)]);
    let sent = down(&mut Down::new(), Clientbound, "mount_screen_open", &mount);
    assert_eq!(sent, [out(Clientbound, "open_screen", &horse.buf)]);
}

/// A trade with item stacks needs the item codec for the stacks; until it is
/// there the conversion fails rather than passing on unconverted IDs.
#[test]
fn trades_fold_the_price_into_the_cost_for_1_13() {
    let mapping = |direction| {
        let mut shared = match direction {
            Clientbound => Shared::new(OLDER, NEWER),
            Serverbound => Shared::new(NEWER, OLDER),
        };
        shared.mapping(Clientbound, "item")
    };
    let emerald = |protocol: i32| {
        data::version(protocol)
            .expect_registry("item")
            .id("emerald")
            .unwrap()
    };
    let stack = |w: &mut Writer, id: i32, count: i8| {
        w.bool(true).varint(id).i8(count).u8(0);
    };
    let mut offers = Writer::new();
    offers.varint(2).u8(1);
    stack(&mut offers, emerald(NEWER), 10);
    stack(&mut offers, emerald(NEWER), 1);
    offers.bool(false).bool(false).i32(0).i32(12);
    // 4 experience, a special price of -3, multiplier 0.05.
    offers
        .i32(4)
        .i32(-3)
        .f32(0.05)
        .varint(2)
        .varint(30)
        .bool(true);
    let mut shared = Shared::new(OLDER, NEWER);
    match trades::convert(&mut shared, Clientbound, &offers.buf) {
        Ok(converted) => {
            let older = emerald(OLDER);
            assert_eq!(mapping(Clientbound).get(emerald(NEWER)), Some(older));
            let mut expected = Writer::new();
            expected.i32(2).u8(1);
            stack(&mut expected, older, 7);
            stack(&mut expected, older, 1);
            expected.bool(false).bool(false).i32(0).i32(12);
            assert_eq!(converted, expected.buf);
        }
        Err(error) => assert!(error.to_string().contains("not implemented"), "{error}"),
    }
}

#[test]
fn malformed_packets_fail_without_panicking() {
    let inputs: [&[u8]; 5] = [&[], &[0xff], &[0x80; 9], &[1, 2, 3], &[0xff; 40]];
    for (up_side, directions) in [
        (true, [Clientbound, Serverbound]),
        (false, [Clientbound, Serverbound]),
    ] {
        let (client, backend) = protocols(up_side);
        for direction in directions {
            let source = match direction {
                Clientbound => backend,
                Serverbound => client,
            };
            let names = data::version(source)
                .packets(State::Play, direction)
                .names();
            for name in names {
                for input in inputs {
                    let mut step: Box<dyn Step> = if up_side {
                        Box::new(Up::new())
                    } else {
                        Box::new(Down::new())
                    };
                    // Errors are expected for most; what matters is no panic.
                    let _ = send(step.as_mut(), up_side, direction, name, input);
                }
            }
        }
    }
    // Trailing bytes are refused by every handler of the pair.
    let mut step = Up::new();
    assert!(send(&mut step, true, Serverbound, "use_item_on", &[0; 30]).is_err());
    assert!(send(&mut step, true, Clientbound, "player_sleep", &[0; 10]).is_err());
}

/// Every packet of the official servers' recorded sessions goes through the
/// step: the ones the step handles convert to what the receiver's own server
/// sends, and nothing the step owns fails. Packets the shared codecs own
/// pass through unconverted until those codecs exist (and the one the step
/// hands to them, `light_update`, fails going down).
#[test]
fn recorded_sessions_go_through_both_steps() {
    for (up_side, protocol) in [(true, OLDER), (false, NEWER)] {
        let Some(packets) = corpus(protocol) else {
            eprintln!("no recorded {protocol} session; skipped");
            continue;
        };
        let mut step: Box<dyn Step> = if up_side {
            Box::new(Up::new())
        } else {
            Box::new(Down::new())
        };
        let (client, backend) = protocols(up_side);
        let mut checked = 0;
        for packet in packets.iter().filter(|packet| packet.state == State::Play) {
            // Serverbound packets were recorded in the backend's protocol;
            // only clientbound ones are in the sender's.
            if packet.direction != Clientbound {
                continue;
            }
            let name = packet.name(protocol);
            let mut emitted = VecDeque::new();
            let mut cx = Cx {
                emitted: &mut emitted,
                client_protocol: client,
                backend_protocol: backend,
            };
            let result = step.clientbound(
                &mut cx,
                State::Play,
                Packet::new(packet.id, packet.data.clone()),
            );
            match (result, name) {
                (Err(_), "light_update") if !up_side => {}
                (Err(error), _) => panic!("{protocol} {name}: {error}"),
                (Ok(()), _) => checked += 1,
            }
            for emitted in emitted {
                let target = data::version(client);
                let out = target
                    .packet_name(State::Play, emitted.direction, emitted.packet.id)
                    .unwrap();
                // What the step produced must parse for its own packets.
                if out == "login" {
                    let mut r = Reader::new(&emitted.packet.data);
                    r.i32().unwrap();
                    r.u8().unwrap();
                    r.i32().unwrap();
                    r.u8().unwrap();
                    if up_side {
                        r.string(16).unwrap();
                        r.varint().unwrap();
                    } else {
                        r.u8().unwrap();
                        r.string(16).unwrap();
                    }
                    r.bool().unwrap();
                    r.finish().unwrap();
                }
            }
        }
        assert!(checked > 1000, "{protocol}: {checked}");
    }
}

#[test]
fn spawn_positions_from_the_recordings_keep_their_coordinates() {
    for (protocol, other) in [(OLDER, NEWER), (NEWER, OLDER)] {
        let Some(packets) = corpus(protocol) else {
            continue;
        };
        let spawn = packets
            .iter()
            .find(|packet| packet.name(protocol) == "set_default_spawn_position")
            .unwrap();
        let converted = layout::position_only(protocol, other, &spawn.data).unwrap();
        let read = |protocol, data: &[u8]| {
            layout::read_position(&mut Reader::new(data), protocol).unwrap()
        };
        let (before, after) = (read(protocol, &spawn.data), read(other, &converted));
        assert_eq!(before, after);
        assert!(before.y > 0 && before.y < 256, "{before:?}");
    }
}

#[test]
fn rules_give_new_blocks_stand_ins_of_the_same_shape() {
    let older = &data::version(OLDER).block_states;
    let newer = &data::version(NEWER).block_states;
    let mapping = crate::mapping::mapping(NEWER, OLDER, "block_state");
    let check = |state: &str, expected: &str| {
        let id = newer.parse(state).unwrap();
        let mapped = mapping.get(id).unwrap();
        assert_eq!(older.describe(mapped).unwrap(), expected, "{state}");
    };
    check(
        "granite_stairs[facing=east,half=top,shape=outer_left,waterlogged=true]",
        "brick_stairs[facing=east,half=top,shape=outer_left,waterlogged=true]",
    );
    check(
        "andesite_slab[type=double,waterlogged=false]",
        "cobblestone_slab[type=double,waterlogged=false]",
    );
    check(
        "brick_wall[east=true,north=false,south=true,up=true,waterlogged=false,west=false]",
        "cobblestone_wall[east=true,north=false,south=true,up=true,waterlogged=false,west=false]",
    );
    check(
        "spruce_wall_sign[facing=west,waterlogged=true]",
        "wall_sign[facing=west,waterlogged=true]",
    );
    check(
        "scaffolding[bottom=false,distance=3,waterlogged=true]",
        "oak_trapdoor[facing=north,half=top,open=false,powered=false,waterlogged=true]",
    );
    check("lantern[hanging=true]", "end_rod[facing=down]");
    check(
        "smooth_stone_slab[type=top,waterlogged=false]",
        "stone_slab[type=top,waterlogged=false]",
    );
}
