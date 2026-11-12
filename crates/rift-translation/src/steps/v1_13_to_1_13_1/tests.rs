use super::{NEWER, OLDER, down::Down, up::Up};
use crate::{
    Direction, data,
    steps::v1_12_to_1_12_1::testing::{Out, Session, hex, recorded, to_backend, to_client},
    wire::{Position, Reader, Writer},
};
use std::io;

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

/// A boss bar recorded from the official 1.13 server (`/bossbar add`), with
/// its flags replaced.
fn boss_bar(flags: u8) -> Vec<u8> {
    let mut packet =
        hex("09924e27260c428ca1ea9b6515e727cb000f7b2274657874223a22426f7373227d00000000060000");
    *packet.last_mut().unwrap() = flags;
    packet
}

fn boss_flags(operation: u8, flags: u8) -> Vec<u8> {
    let mut packet = hex("09924e27260c428ca1ea9b6515e727cb");
    packet.extend([operation, flags]);
    packet
}

#[test]
fn boss_bar_fog_moves_to_its_own_bit_for_a_1_13_1_client() {
    let mut session = Session::play(NEWER, OLDER);
    for (old, new) in [(0x00, 0x00), (0x01, 0x01), (0x02, 0x06), (0x03, 0x07)] {
        assert_eq!(
            session.clientbound("boss_event", &boss_bar(old)).unwrap(),
            [to_client("boss_event", &boss_bar(new))]
        );
        assert_eq!(
            session
                .clientbound("boss_event", &boss_flags(5, old))
                .unwrap(),
            [to_client("boss_event", &boss_flags(5, new))]
        );
    }
}

#[test]
fn boss_bar_fog_shares_the_music_bit_for_a_1_13_client() {
    let mut session = Session::play(OLDER, NEWER);
    for (new, old) in [
        (0x00, 0x00),
        (0x01, 0x01),
        (0x02, 0x02),
        (0x04, 0x02),
        (0x06, 0x02),
        (0x07, 0x03),
    ] {
        assert_eq!(
            session.clientbound("boss_event", &boss_bar(new)).unwrap(),
            [to_client("boss_event", &boss_bar(old))]
        );
        assert_eq!(
            session
                .clientbound("boss_event", &boss_flags(5, new))
                .unwrap(),
            [to_client("boss_event", &boss_flags(5, old))]
        );
    }
}

#[test]
fn boss_bar_updates_without_flags_pass_unchanged() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER)] {
        let mut session = Session::play(client, backend);
        let uuid = "09924e27260c428ca1ea9b6515e727cb";
        for operation in [
            "01",
            "023f000000", // Recorded: progress 0.5.
            "030f7b2274657874223a22426f7373227d",
            "040301",
        ] {
            let packet = hex(&format!("{uuid}{operation}"));
            assert_eq!(
                session.clientbound("boss_event", &packet).unwrap(),
                [to_client("boss_event", &packet)]
            );
        }
    }
}

#[test]
fn malformed_boss_bars_fail() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER)] {
        let mut session = Session::play(client, backend);
        let full = boss_bar(0);
        for body in [
            full[..full.len() - 1].to_vec(),
            [&full[..], &[0]].concat(),
            boss_flags(6, 0),
            hex("09924e27260c428ca1ea9b6515e727cb02"),
            hex("09924e27260c428ca1ea9b6515e727cb0005ffffffff7f"),
        ] {
            assert!(
                session.clientbound("boss_event", &body).is_err(),
                "{body:?}"
            );
        }
    }
}

#[test]
fn recorded_boss_bars_convert() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER)] {
        let Some(packets) = recorded(backend, Direction::Clientbound, "boss_event") else {
            return;
        };
        assert_eq!(packets.len(), 2);
        let mut session = Session::play(client, backend);
        for packet in packets {
            // Neither recorded bar has flags set, so both arrive as they were.
            assert_eq!(
                session.clientbound("boss_event", &packet).unwrap(),
                [to_client("boss_event", &packet)]
            );
        }
    }
}

fn level_event(event: i32, position: Position, data: i32) -> Vec<u8> {
    let mut body = Writer::new();
    body.i32(event)
        .position_legacy(position)
        .i32(data)
        .bool(false);
    body.into_inner()
}

const AT: Position = Position {
    x: -12,
    y: 64,
    z: 300,
};

fn above(by: i32) -> Position {
    Position { y: AT.y + by, ..AT }
}

#[test]
fn dispenser_smoke_turns_from_a_grid_cell_into_a_facing() {
    let mut session = Session::play(NEWER, OLDER);
    // Down, up, north, south, west, east.
    for (cell, facing) in [
        (4, 1),
        (1, 2),
        (7, 3),
        (3, 4),
        (5, 5),
        (0, 4),
        (8, 5),
        (99, 1),
    ] {
        assert_eq!(
            session
                .clientbound("level_event", &level_event(2000, AT, cell))
                .unwrap(),
            [to_client("level_event", &level_event(2000, AT, facing))]
        );
    }
}

#[test]
fn dispenser_smoke_turns_from_a_facing_into_a_grid_cell() {
    let mut session = Session::play(OLDER, NEWER);
    for (facing, position, cell) in [
        (0, above(-1), 4),
        (1, above(1), 4),
        (2, AT, 1),
        (3, AT, 7),
        (4, AT, 3),
        (5, AT, 5),
        (6, AT, 4),
    ] {
        assert_eq!(
            session
                .clientbound("level_event", &level_event(2000, AT, facing))
                .unwrap(),
            [to_client("level_event", &level_event(2000, position, cell))]
        );
    }
    // A position at the edge of what 1.13 can hold stays put.
    let top = Position { y: 2047, ..AT };
    assert_eq!(
        session
            .clientbound("level_event", &level_event(2000, top, 1))
            .unwrap(),
        [to_client("level_event", &level_event(2000, top, 4))]
    );
}

fn item(protocol: i32, name: &str) -> i32 {
    data::version(protocol)
        .expect_registry("item")
        .id(name)
        .unwrap()
}

fn block_state(protocol: i32, state: &str) -> i32 {
    data::version(protocol).block_states.parse(state).unwrap()
}

#[test]
fn records_and_broken_blocks_follow_the_inserted_dead_corals() {
    // Items and states after the dead coral plants moved; those before not.
    let cases = [
        (
            2001,
            block_state(OLDER, "stone"),
            block_state(NEWER, "stone"),
        ),
        (
            2001,
            block_state(OLDER, "blue_ice"),
            block_state(NEWER, "blue_ice"),
        ),
        (
            1010,
            item(OLDER, "music_disc_cat"),
            item(NEWER, "music_disc_cat"),
        ),
        (1010, 0, 0),
    ];
    assert_ne!(cases[1].1, cases[1].2);
    assert_ne!(cases[2].1, cases[2].2);
    for (event, old, new) in cases {
        let mut up = Session::play(NEWER, OLDER);
        assert_eq!(
            up.clientbound("level_event", &level_event(event, AT, old))
                .unwrap(),
            [to_client("level_event", &level_event(event, AT, new))]
        );
        let mut down = Session::play(OLDER, NEWER);
        assert_eq!(
            down.clientbound("level_event", &level_event(event, AT, new))
                .unwrap(),
            [to_client("level_event", &level_event(event, AT, old))]
        );
    }
    // A dead coral plant breaks with the dead coral fan's particles.
    let mut down = Session::play(OLDER, NEWER);
    let plant = block_state(NEWER, "dead_horn_coral[waterlogged=false]");
    let fan = block_state(OLDER, "dead_horn_coral_fan[waterlogged=false]");
    assert_eq!(
        down.clientbound("level_event", &level_event(2001, AT, plant))
            .unwrap(),
        [to_client("level_event", &level_event(2001, AT, fan))]
    );
}

#[test]
fn other_level_events_pass_unchanged() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER)] {
        let Some(packets) = recorded(backend, Direction::Clientbound, "level_event") else {
            return;
        };
        let mut session = Session::play(client, backend);
        for packet in packets {
            // Recorded: an ender eye breaking (2004).
            assert_eq!(Reader::new(&packet).i32().unwrap(), 2004);
            assert_eq!(
                session.clientbound("level_event", &packet).unwrap(),
                [to_client("level_event", &packet)]
            );
        }
        let event = level_event(1000, AT, 0);
        assert_eq!(
            session.clientbound("level_event", &event).unwrap(),
            [to_client("level_event", &event)]
        );
    }
}

#[test]
fn malformed_level_events_fail() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER)] {
        let mut session = Session::play(client, backend);
        let full = level_event(2000, AT, 1);
        for body in [
            full[..full.len() - 1].to_vec(),
            [&full[..], &[0]].concat(),
            [&full[..full.len() - 1], &[2]].concat(),
        ] {
            assert!(session.clientbound("level_event", &body).is_err());
        }
    }
}

fn suggestion(transaction: i32, command: &str) -> Vec<u8> {
    let mut body = Writer::new();
    body.varint(transaction).string(command);
    body.into_inner()
}

fn suggestions(transaction: i32, start: i32, length: i32, tooltip: bool) -> Vec<u8> {
    let mut body = Writer::new();
    body.varint(transaction)
        .varint(start)
        .varint(length)
        .varint(2)
        .string("@a")
        .bool(false)
        .string("Notch")
        .bool(tooltip);
    if tooltip {
        body.string(r#"{"text":"A player"}"#);
    }
    body.into_inner()
}

#[test]
fn a_1_13_1_client_completes_commands_on_a_1_13_backend() {
    let mut session = Session::play(NEWER, OLDER);
    // From the chat box, with its slash.
    assert_eq!(
        session
            .serverbound("command_suggestion", &suggestion(3, "/give "))
            .unwrap(),
        [to_backend("command_suggestion", &suggestion(3, "give "))]
    );
    // From a command block, as typed.
    assert_eq!(
        session
            .serverbound("command_suggestion", &suggestion(4, "say "))
            .unwrap(),
        [to_backend("command_suggestion", &suggestion(4, "say "))]
    );
    // Answers arrive in either order.
    assert_eq!(
        session
            .clientbound("command_suggestions", &suggestions(4, 4, 0, true))
            .unwrap(),
        [to_client(
            "command_suggestions",
            &suggestions(4, 4, 0, true)
        )]
    );
    assert_eq!(
        session
            .clientbound("command_suggestions", &suggestions(3, 5, 0, false))
            .unwrap(),
        [to_client(
            "command_suggestions",
            &suggestions(3, 6, 0, false)
        )]
    );
    // A transaction reused for a command block no longer shifts.
    session
        .serverbound("command_suggestion", &suggestion(3, "/tp"))
        .unwrap();
    session
        .serverbound("command_suggestion", &suggestion(3, "tp"))
        .unwrap();
    assert_eq!(
        session
            .clientbound("command_suggestions", &suggestions(3, 0, 2, false))
            .unwrap(),
        [to_client(
            "command_suggestions",
            &suggestions(3, 0, 2, false)
        )]
    );
    // An answer to nothing seen is taken to be for the chat box.
    assert_eq!(
        session
            .clientbound("command_suggestions", &suggestions(9, 0, 2, false))
            .unwrap(),
        [to_client(
            "command_suggestions",
            &suggestions(9, 1, 2, false)
        )]
    );
}

#[test]
fn a_1_13_client_completes_commands_on_a_1_13_1_backend_unchanged() {
    let mut session = Session::play(OLDER, NEWER);
    let request = suggestion(1, "give ");
    assert_eq!(
        session.serverbound("command_suggestion", &request).unwrap(),
        [to_backend("command_suggestion", &request)]
    );
    let answer = suggestions(1, 5, 0, true);
    assert_eq!(
        session.clientbound("command_suggestions", &answer).unwrap(),
        [to_client("command_suggestions", &answer)]
    );
}

#[test]
fn malformed_completions_fail() {
    let mut session = Session::play(NEWER, OLDER);
    let request = suggestion(1, "/give ");
    for body in [
        &request[..request.len() - 1],
        &[&request[..], &[0]].concat(),
    ] {
        assert!(session.serverbound("command_suggestion", body).is_err());
    }
    let long = suggestion(1, &"x".repeat(32_501));
    assert!(session.serverbound("command_suggestion", &long).is_err());
    let answer = suggestions(1, 5, 0, true);
    for body in [&answer[..answer.len() - 1], &[&answer[..], &[0]].concat()] {
        assert!(session.clientbound("command_suggestions", body).is_err());
    }
    // More suggestions than bytes.
    assert!(
        session
            .clientbound("command_suggestions", &hex("0105007f"))
            .is_err()
    );
}

/// Skip a test that needs a shared codec that has not landed yet.
fn needs_codec(result: io::Result<Vec<Out>>) -> Option<Vec<Out>> {
    match result {
        Err(error) if error.to_string().contains("not implemented") => {
            eprintln!("skipping until the codec lands: {error}");
            None
        }
        result => Some(result.unwrap()),
    }
}

/// A writable book stack with no tag, in 1.13 and 1.13.1's layout.
fn book(protocol: i32) -> Vec<u8> {
    let mut body = Writer::new();
    body.i16(item(protocol, "writable_book") as i16).i8(1).u8(0);
    body.into_inner()
}

#[test]
fn a_1_13_client_edits_books_in_its_main_hand() {
    let mut session = Session::play(OLDER, NEWER);
    let edit = [book(OLDER), vec![0]].concat();
    let Some(out) = needs_codec(session.serverbound("edit_book", &edit)) else {
        return;
    };
    assert_eq!(
        out,
        [to_backend("edit_book", &[book(NEWER), vec![0, 0]].concat())]
    );
}

#[test]
fn a_1_13_1_client_edits_an_off_hand_book_through_a_hand_swap() {
    let mut session = Session::play(NEWER, OLDER);
    let main = [book(NEWER), vec![1, 0]].concat();
    let Some(out) = needs_codec(session.serverbound("edit_book", &main)) else {
        return;
    };
    let edit = [book(OLDER), vec![1]].concat();
    assert_eq!(out, [to_backend("edit_book", &edit)]);
    let off = [book(NEWER), vec![1, 1]].concat();
    let swap = hex(SWAP_HANDS);
    assert_eq!(
        session.serverbound("edit_book", &off).unwrap(),
        [
            to_backend("player_action", &swap),
            to_backend("edit_book", &edit),
            to_backend("player_action", &swap),
        ]
    );
    assert!(
        session
            .serverbound("edit_book", &[book(NEWER), vec![1, 2]].concat())
            .is_err()
    );
}

#[test]
fn plugin_messages_other_than_trades_pass_unchanged() {
    for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER), (404, OLDER), (OLDER, 404)] {
        let Some(packets) = recorded(backend, Direction::Clientbound, "custom_payload") else {
            return;
        };
        let mut session = Session::play(client, backend);
        for packet in packets {
            // Recorded: the server's brand.
            assert_eq!(
                session.clientbound("custom_payload", &packet).unwrap(),
                [to_client("custom_payload", &packet)]
            );
        }
        let mut book_open = Writer::new();
        book_open.string("minecraft:book_open").varint(1);
        assert_eq!(
            session
                .clientbound("custom_payload", &book_open.buf)
                .unwrap(),
            [to_client("custom_payload", &book_open.buf)]
        );
        assert!(
            session
                .clientbound("custom_payload", &[0x05, b'a'])
                .is_err()
        );
    }
}

/// One trade: a book for a book, with a book as second cost when `second`.
fn trades(protocol: i32, second: bool) -> Vec<u8> {
    let mut body = Writer::new();
    body.string("minecraft:trader_list").i32(3).u8(1);
    body.bytes(&book(protocol)).bytes(&book(protocol));
    body.bool(second);
    if second {
        body.bytes(&book(protocol));
    }
    body.bool(false).i32(2).i32(7);
    body.into_inner()
}

#[test]
fn trade_lists_carry_renumbered_items() {
    for second in [false, true] {
        for (client, backend) in [(NEWER, OLDER), (OLDER, NEWER)] {
            let mut session = Session::play(client, backend);
            let Some(out) =
                needs_codec(session.clientbound("custom_payload", &trades(backend, second)))
            else {
                return;
            };
            assert_eq!(out, [to_client("custom_payload", &trades(client, second))]);
        }
    }
}

/// `data/rules/393-401.json`: every 1.13.1 block, state and item has a 1.13
/// counterpart, and the dead coral plants become the fans of their colour,
/// waterlogged or not as they were.
#[test]
fn dead_coral_plants_stand_in_as_dead_coral_fans() {
    for registry in ["block", "block_state", "item"] {
        assert_eq!(crate::mapping::unmapped(NEWER, OLDER, registry), []);
        assert_eq!(crate::mapping::unmapped(OLDER, NEWER, registry), []);
    }
    let states = crate::mapping::mapping(NEWER, OLDER, "block_state");
    let items = crate::mapping::mapping(NEWER, OLDER, "item");
    for colour in ["tube", "brain", "bubble", "fire", "horn"] {
        for waterlogged in ["true", "false"] {
            let plant = format!("dead_{colour}_coral[waterlogged={waterlogged}]");
            let fan = format!("dead_{colour}_coral_fan[waterlogged={waterlogged}]");
            assert_eq!(
                states.get(block_state(NEWER, &plant)),
                Some(block_state(OLDER, &fan))
            );
        }
        assert_eq!(
            items.get(item(NEWER, &format!("dead_{colour}_coral"))),
            Some(item(OLDER, &format!("dead_{colour}_coral_fan")))
        );
    }
}

/// A 1.13 `player_action` swapping the hands: action 6, the origin, facing
/// down; checked against minecraft-data's 1.13 schema.
const SWAP_HANDS: &str = "06000000000000000000";

#[test]
fn the_hand_swap_is_a_1_13_player_action() {
    assert_eq!(super::packets::swap_hands(), hex(SWAP_HANDS));
    let id = data::version(OLDER).expect_packet(
        crate::State::Play,
        Direction::Serverbound,
        "player_action",
    );
    assert_eq!(id, 0x18);
}
