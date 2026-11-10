//! Conversions of the 26.2 <-> 26.3 steps, on packets the official servers
//! and the 26.3 client sent where the corpus has them.
use super::{DOWN, NEWER, OLDER, UP};
use crate::{
    Direction::{self, Clientbound, Serverbound},
    State, data,
    steps::v1_21_11_to_26_1::testing::{Harness, corpus_through, hex},
    wire::{Position, Reader, Writer},
};

fn up() -> Harness {
    Harness::new(UP, NEWER, OLDER)
}

fn down() -> Harness {
    Harness::new(DOWN, OLDER, NEWER)
}

/// Join game from the official 26.2 and 26.3 servers: creative, no
/// previous game mode (a byte -1, then an optional VarInt 0).
const LOGIN_26_2: &str = concat!(
    "000000010003136d696e6563726166743a6f766572776f726c64116d696e6563726166743a7468",
    "655f656e64146d696e6563726166743a7468655f6e657468657214020200010000136d696e6563",
    "726166743a6f766572776f726c644ae77737d6c2002f01ff00010000c1ffffff0f0000"
);
const LOGIN_26_3: &str = concat!(
    "000000010003136d696e6563726166743a6f766572776f726c64116d696e6563726166743a7468",
    "655f656e64146d696e6563726166743a7468655f6e657468657214020200010000136d696e6563",
    "726166743a6f766572776f726c64ba3fe886e9522625010000010000c1ffffff0f0000"
);
/// The 26.3 server's spawn teleport (ID 1, absolute) and the official 26.3
/// client's acceptance of it.
const TELEPORT: &str = concat!(
    "01bfe0000000000000c04e000000000000400c0000000000000000000000000000000000",
    "00000000000000000000000000000000000000000000000000"
);
const ACCEPT_26_3: &str = "01bfe0000000000000c04e000000000000400c0000000000000000000000000000";

#[test]
fn join_game_converts_game_modes() {
    let (_, data) = up().one(Clientbound, "login", &hex(LOGIN_26_2));
    let mut expected = hex(LOGIN_26_3);
    // Only the seed hash differs between the two recordings.
    let seed = 92..100;
    expected[seed.clone()].copy_from_slice(&hex(LOGIN_26_2)[seed]);
    assert_eq!(data, expected);
    let (_, back) = down().one(Clientbound, "login", &data);
    assert_eq!(back, hex(LOGIN_26_2));
    // Respawn: survival, previously creative, keep all data.
    let mut w = Writer::new();
    w.varint(0).string("minecraft:the_nether").i64(9);
    w.u8(0)
        .i8(1)
        .bool(false)
        .bool(false)
        .bool(false)
        .varint(0)
        .varint(32)
        .u8(3);
    let (_, newer) = up().one(Clientbound, "respawn", &w.buf);
    let mut expected = Writer::new();
    expected.varint(0).string("minecraft:the_nether").i64(9);
    expected
        .varint(0)
        .varint(2)
        .bool(false)
        .bool(false)
        .bool(false)
        .varint(0)
        .varint(32)
        .u8(3);
    assert_eq!(newer, expected.buf);
    assert_eq!(down().one(Clientbound, "respawn", &newer).1, w.buf);
}

#[test]
fn arm_swings_move_between_animate_and_swing_animation() {
    let mut up = up();
    assert_eq!(
        up.one(Clientbound, "animate", &hex("07 00")),
        ("swing_animation".into(), hex("07 00 01 06"))
    );
    assert_eq!(
        up.one(Clientbound, "animate", &hex("07 03")),
        ("swing_animation".into(), hex("07 01 01 06"))
    );
    for (old, new) in [(2, 0), (4, 1), (5, 2)] {
        assert_eq!(
            up.one(Clientbound, "animate", &[7, old]),
            ("animate".into(), vec![7, new])
        );
    }
    assert!(up.clientbound("animate", &hex("07 01")).unwrap().is_empty());
    let mut down = down();
    assert_eq!(
        down.one(Clientbound, "swing_animation", &hex("07 01 02 0a")),
        ("animate".into(), hex("07 03"))
    );
    assert!(
        down.clientbound("swing_animation", &hex("07 00 00 06"))
            .unwrap()
            .is_empty()
    );
    assert!(
        down.clientbound("swing_animation", &hex("07 02 01 06"))
            .is_err()
    );
    for (new, old) in [(0, 2), (1, 4), (2, 5)] {
        assert_eq!(
            down.one(Clientbound, "animate", &[7, new]),
            ("animate".into(), vec![7, old])
        );
    }
    assert!(
        down.clientbound("animate", &hex("07 03"))
            .unwrap()
            .is_empty()
    );
    // Punches and swings.
    assert_eq!(
        up.one(Serverbound, "punch", &[]),
        ("swing".into(), hex("00"))
    );
    assert_eq!(
        down.one(Serverbound, "swing", &hex("00")),
        ("punch".into(), vec![])
    );
    assert!(down.serverbound("swing", &hex("01")).unwrap().is_empty());
}

#[test]
fn sign_sides_move_and_become_varints() {
    let position = Position { x: 3, y: -60, z: 7 }.to_modern();
    let mut old = Writer::new();
    old.i64(position).bool(false);
    let mut lines = Writer::new();
    for line in ["one", "", "three", "four"] {
        lines.string(line);
    }
    old.bytes(&lines.buf);
    let mut new = Writer::new();
    new.i64(position).bytes(&lines.buf).varint(0);
    assert_eq!(down().one(Serverbound, "sign_update", &old.buf).1, new.buf);
    assert_eq!(up().one(Serverbound, "sign_update", &new.buf).1, old.buf);
    let mut editor = Writer::new();
    editor.i64(position).bool(true);
    assert_eq!(
        up().one(Clientbound, "open_sign_editor", &editor.buf).1,
        [&position.to_be_bytes()[..], &[1]].concat()
    );
    let mut back = Writer::new();
    back.i64(position).varint(1);
    assert_eq!(
        down().one(Clientbound, "open_sign_editor", &back.buf).1,
        editor.buf
    );
}

fn level_event(event: i32, position: i64, data: i32) -> Vec<u8> {
    let mut w = Writer::new();
    w.i32(event).i64(position).i32(data).bool(false);
    w.into_inner()
}

fn block_state(protocol: i32, state: &str) -> i32 {
    data::version(protocol).block_states.parse(state).unwrap()
}

#[test]
fn level_events_follow_what_each_version_plays() {
    let at = Position { x: 1, y: 2, z: 3 }.to_modern();
    // A splash potion's particles; 26.3 plays the sound as its own event.
    let sent = up()
        .clientbound("level_event", &level_event(2002, at, 0xff00ff))
        .unwrap();
    let events: Vec<i32> = sent
        .iter()
        .map(|sent| Reader::new(&sent.data).i32().unwrap())
        .collect();
    assert_eq!(events, [2002, 1053]);
    assert!(
        down()
            .clientbound("level_event", &level_event(1053, at, 0))
            .unwrap()
            .is_empty()
    );
    assert!(
        down()
            .clientbound("level_event", &level_event(2019, at, 2))
            .unwrap()
            .is_empty()
    );
    // Waxing gets its sound.
    let sent = up()
        .clientbound("level_event", &level_event(3003, at, 0))
        .unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1].name, "sound");
    let mut r = Reader::new(&sent[1].data);
    let wax = data::version(NEWER)
        .expect_registry("sound_event")
        .id("item.honeycomb.wax_on")
        .unwrap();
    assert_eq!((r.varint().unwrap(), r.varint().unwrap()), (wax + 1, 4));
    assert_eq!(
        [r.i32().unwrap(), r.i32().unwrap(), r.i32().unwrap()],
        [12, 20, 28]
    );
    // Block states are mapped; 26.3's straw bed breaks like its stand-in.
    let stone = (block_state(OLDER, "stone"), block_state(NEWER, "stone"));
    assert_eq!(
        up().one(Clientbound, "level_event", &level_event(2001, at, stone.0))
            .1,
        level_event(2001, at, stone.1)
    );
    let straw = block_state(NEWER, "straw_bed[facing=east,occupied=false,part=head]");
    let bed = block_state(OLDER, "yellow_bed[facing=east,occupied=false,part=head]");
    assert_eq!(
        down()
            .one(Clientbound, "level_event", &level_event(2014, at, straw))
            .1,
        level_event(2001, at, bed)
    );
    assert!(
        down()
            .clientbound("level_event", &level_event(2001, at, i32::MAX))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn teleports_are_accepted_with_their_position() {
    let mut up = up();
    let sent = up
        .serverbound("accept_teleportation", &hex(ACCEPT_26_3))
        .unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        (sent[0].name.as_str(), sent[0].data.as_slice()),
        ("accept_teleportation", &[1][..])
    );
    assert_eq!(sent[1].name, "move_player_pos_rot");
    assert_eq!(sent[1].data, [&hex(ACCEPT_26_3)[1..], &[0]].concat());

    // An absolute teleport is accepted at once with its target.
    let mut down = down();
    down.clientbound("player_position", &hex(TELEPORT)).unwrap();
    assert_eq!(
        down.one(Serverbound, "accept_teleportation", &hex("01")),
        ("accept_teleportation".into(), hex(ACCEPT_26_3))
    );
    // A relative one before any known position waits for the movement
    // packet the 26.2 client sends right after accepting.
    let mut down = self::down();
    let mut relative = hex(TELEPORT);
    relative[0] = 2;
    let flags = relative.len() - 4;
    relative[flags..].copy_from_slice(&7i32.to_be_bytes());
    down.clientbound("player_position", &relative).unwrap();
    assert!(
        down.serverbound("accept_teleportation", &hex("02"))
            .unwrap()
            .is_empty()
    );
    let mut moved = Writer::new();
    moved.f64(4.0).f64(65.0).f64(-2.0).f32(90.0).f32(10.0).u8(1);
    let sent = down.serverbound("move_player_pos_rot", &moved.buf).unwrap();
    assert_eq!(sent.len(), 2);
    let mut accept = Writer::new();
    accept
        .varint(2)
        .f64(4.0)
        .f64(65.0)
        .f64(-2.0)
        .f32(90.0)
        .f32(10.0);
    assert_eq!(sent[0].data, accept.buf);
    assert_eq!(
        (sent[1].name.as_str(), &sent[1].data),
        ("move_player_pos_rot", &moved.buf)
    );
    // Now the position is known: a relative teleport resolves at once.
    relative[0] = 3;
    relative[1..9].copy_from_slice(&1.0f64.to_be_bytes());
    down.clientbound("player_position", &relative).unwrap();
    let (_, data) = down.one(Serverbound, "accept_teleportation", &hex("03"));
    let mut r = Reader::new(&data);
    assert_eq!(r.varint().unwrap(), 3);
    assert_eq!(
        [r.f64().unwrap(), r.f64().unwrap(), r.f64().unwrap()],
        [5.0, 5.0, 1.5]
    );
}

#[test]
fn destroy_actions_shift_and_mining_shows_progress() {
    let at = Position { x: 1, y: 64, z: 1 }.to_modern();
    let action = |action: i32, direction: u8| {
        let mut w = Writer::new();
        w.varint(action).i64(at).u8(direction).varint(5);
        w.into_inner()
    };
    let mut up = up();
    assert_eq!(
        up.one(Serverbound, "player_action", &action(0, 1)).1,
        action(0, 1)
    );
    assert!(
        up.serverbound("player_action", &action(1, 4))
            .unwrap()
            .is_empty()
    );
    let mut events = Vec::new();
    for _ in 0..4 {
        let sent = up.serverbound("client_tick_end", &[]).unwrap();
        assert_eq!(sent[0].name, "client_tick_end");
        assert_eq!(sent[1].name, "level_event");
        let mut r = Reader::new(&sent[1].data);
        events.push((r.i32().unwrap(), r.i64().unwrap(), r.i32().unwrap()));
    }
    assert_eq!(
        events,
        [(2019, at, 4), (2019, at, 4), (2019, at, 4), (2020, at, 4)]
    );
    assert_eq!(
        up.one(Serverbound, "player_action", &action(3, 1)).1,
        action(2, 1)
    );
    assert_eq!(up.serverbound("client_tick_end", &[]).unwrap().len(), 1);
    // A block update where the player mines also ends it.
    up.serverbound("player_action", &action(0, 1)).unwrap();
    let mut update = Writer::new();
    update.i64(at).varint(0);
    up.clientbound("block_update", &update.buf).unwrap();
    assert_eq!(up.serverbound("client_tick_end", &[]).unwrap().len(), 1);
    assert!(up.serverbound("player_action", &action(9, 1)).is_err());
    let mut down = down();
    assert_eq!(
        down.one(Serverbound, "player_action", &action(2, 1)).1,
        action(3, 1)
    );
    assert_eq!(
        down.one(Serverbound, "player_action", &action(0, 1)).1,
        action(0, 1)
    );
    assert!(down.serverbound("player_action", &action(8, 1)).is_err());
}

#[test]
fn brewing_stands_get_and_lose_their_totals() {
    let brewing = data::version(OLDER)
        .expect_registry("menu")
        .id("brewing_stand")
        .unwrap();
    let mut screen = Writer::new();
    screen.varint(3).varint(brewing);
    screen.bytes(&hex("0a0800047465787400016100"));
    let mut up = up();
    up.clientbound("open_screen", &screen.buf).ok();
    let sent = up
        .clientbound("container_set_data", &hex("03 0000 0000"))
        .unwrap();
    let data: Vec<&[u8]> = sent.iter().map(|sent| sent.data.as_slice()).collect();
    assert_eq!(
        data,
        [
            &hex("03 0000 0000")[..],
            &hex("03 0002 0190"),
            &hex("03 0003 0014")
        ]
    );
    assert_eq!(
        up.clientbound("container_set_data", &hex("03 0001 0005"))
            .unwrap()
            .len(),
        1
    );
    let brewing = data::version(NEWER)
        .expect_registry("menu")
        .id("brewing_stand")
        .unwrap();
    let mut screen = Writer::new();
    screen.varint(3).varint(brewing);
    screen.bytes(&hex("0a0800047465787400016100"));
    let mut down = down();
    down.clientbound("open_screen", &screen.buf).ok();
    assert!(
        down.clientbound("container_set_data", &hex("03 0002 0190"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        down.clientbound("container_set_data", &hex("03 0001 0005"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        down.clientbound("container_set_data", &hex("04 0002 0005"))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn packets_26_2_cannot_show_are_dropped() {
    let mut down = down();
    assert!(
        down.clientbound("add_transient_block", &hex("0000000000000000 01"))
            .unwrap()
            .is_empty()
    );
    assert!(
        down.clientbound("post_effects", &hex("00"))
            .unwrap()
            .is_empty()
    );
    assert!(
        down.send(
            Clientbound,
            State::Configuration,
            "post_effects",
            &hex("00")
        )
        .unwrap()
        .is_empty()
    );
}

/// Every movement, join and level event of real sessions converts both
/// ways and parses on the way back; 26.2's come back unchanged (velocity
/// aside, which 26.3 lacks).
#[test]
fn recorded_sessions_convert_and_round_trip() {
    const NAMES: &[&str] = &[
        "login",
        "move_entity_pos",
        "move_entity_pos_rot",
        "move_entity_rot",
        "entity_position_sync",
        "level_event",
    ];
    for (make, client, backend, back) in [(UP, NEWER, OLDER, DOWN), (DOWN, OLDER, NEWER, UP)] {
        let Some(converted) = corpus_through(make, client, backend, Direction::Clientbound, NAMES)
        else {
            eprintln!("no corpus for {backend}, skipped");
            continue;
        };
        assert!(converted.len() > 20, "{backend}: {}", converted.len());
        let mut reverse = Harness::new(back, backend, client);
        for (name, original, sent) in converted {
            for packet in sent.iter().filter(|sent| sent.name == name) {
                let returned = reverse.one(Clientbound, &packet.name, &packet.data).1;
                // Stepped 26.3 paths and moves are summed going down, so
                // only 26.2's packets come back byte for byte.
                if backend == NEWER {
                    continue;
                }
                if name == "entity_position_sync" {
                    assert_eq!(returned.len(), original.len(), "{name}");
                } else if !(name == "level_event" && sent.len() > 1) {
                    assert_eq!(returned, original, "{backend} {name}");
                }
            }
        }
    }
}
