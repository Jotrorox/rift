//! Conversions of the 1.21.11 <-> 26.1 steps, on real packets where the
//! corpus has them (the recorded sessions send few serverbound packets).
use super::{
    DOWN, NEWER, OLDER, UP,
    join::{read_lp_vec3, write_lp_vec3},
    testing::{Harness, Sent, corpus_through, hex, print_for_schema_check},
};
use crate::{
    Direction::{self, Clientbound, Serverbound},
    State,
    wire::{Reader, Writer},
};

fn up() -> Harness {
    Harness::new(UP, NEWER, OLDER)
}

fn down() -> Harness {
    Harness::new(DOWN, OLDER, NEWER)
}

/// The official 26.1 server's first `set_time`: game time 12, the overworld
/// (0) and end (1) clocks at 12 ticks, running.
const SET_TIME_26_1: &str = "000000000000000c02000c000000003f800000010c000000003f800000";
/// Its `world_clock` registry: overworld, then the end.
const WORLD_CLOCKS_26_1: &str = concat!(
    "156d696e6563726166743a776f726c645f636c6f636b02136d696e6563726166743a",
    "6f766572776f726c64010a00116d696e6563726166743a7468655f656e64010a00"
);

#[test]
fn set_time_becomes_the_clocks_a_26_1_server_sends() {
    // What the official 1.21.11 server sends at game time 12 (day time 12,
    // running), as a 26.1 server would send it.
    let mut w = Writer::new();
    w.i64(12).i64(12).bool(true);
    let (name, data) = up().one(Clientbound, "set_time", &w.buf);
    assert_eq!(name, "set_time");
    assert_eq!(data, hex(SET_TIME_26_1));
    // A stopped day cycle stops both clocks.
    let mut w = Writer::new();
    w.i64(5).i64(6000).bool(false);
    let (_, data) = up().one(Clientbound, "set_time", &w.buf);
    let mut r = Reader::new(&data);
    assert_eq!((r.i64().unwrap(), r.varint().unwrap()), (5, 2));
    for clock in 0..2 {
        assert_eq!(r.varint().unwrap(), clock);
        assert_eq!(r.varlong().unwrap(), 6000);
        assert_eq!((r.f32().unwrap(), r.f32().unwrap()), (0.0, 0.0));
    }
    r.finish().unwrap();
}

#[test]
fn clocks_become_a_day_time_that_follows_the_game_time() {
    let mut step = down();
    step.send(
        Clientbound,
        State::Configuration,
        "registry_data",
        &hex(WORLD_CLOCKS_26_1),
    )
    .unwrap();
    let (_, data) = step.one(Clientbound, "set_time", &hex(SET_TIME_26_1));
    assert_eq!(data, hex("000000000000000c000000000000000c01"));
    // Later 26.1 packets carry no clocks: the day keeps running.
    let (_, data) = step.one(Clientbound, "set_time", &hex("000000000000001300"));
    assert_eq!(data, hex("0000000000000013000000000000001301"));
    // The end clock alone stops nothing in the overworld.
    let mut w = Writer::new();
    w.i64(40).varint(1).varint(1).varlong(99).f32(0.0).f32(0.0);
    let (_, data) = step.one(Clientbound, "set_time", &w.buf);
    assert_eq!(data, hex("0000000000000028000000000000002801"));
    // A stopped overworld clock stops the day.
    let mut w = Writer::new();
    w.i64(50)
        .varint(1)
        .varint(0)
        .varlong(1000)
        .f32(0.0)
        .f32(0.0);
    step.one(Clientbound, "set_time", &w.buf);
    let (_, data) = step.one(Clientbound, "set_time", &hex("000000000000006400"));
    assert_eq!(data, hex("000000000000006400000000000003e800"));
    assert!(
        step.clientbound("set_time", &hex("000000000000006401"))
            .is_err()
    );
}

#[test]
fn the_overworld_clock_is_found_by_name() {
    // A server that lists the end first.
    let mut w = Writer::new();
    w.string("minecraft:world_clock").varint(2);
    w.string("minecraft:the_end").bool(false);
    w.string("minecraft:overworld").bool(false);
    let mut step = down();
    step.send(Clientbound, State::Configuration, "registry_data", &w.buf)
        .unwrap();
    let mut w = Writer::new();
    w.i64(1).varint(2);
    w.varint(0).varlong(7).f32(0.0).f32(1.0);
    w.varint(1).varlong(9).f32(0.0).f32(1.0);
    let (_, data) = step.one(Clientbound, "set_time", &w.buf);
    assert_eq!(data, hex("0000000000000001000000000000000901"));
}

fn interact_26_1(entity: i32, hand: i32, at: [f64; 3], secondary: bool) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(entity).varint(hand);
    write_lp_vec3(&mut w, at);
    w.bool(secondary);
    w.into_inner()
}

#[test]
fn an_interaction_becomes_interact_at_then_interact() {
    let mut step = up();
    let sent = step
        .serverbound("interact", &interact_26_1(17, 1, [0.25, 1.5, -0.25], true))
        .unwrap();
    let names: Vec<&str> = sent.iter().map(|sent| sent.name.as_str()).collect();
    assert_eq!(names, ["interact", "interact"]);
    // The location within LpVec3's precision.
    let mut r = Reader::new(&sent[0].data);
    assert_eq!((r.varint().unwrap(), r.varint().unwrap()), (17, 2));
    for want in [0.25, 1.5, -0.25] {
        assert!((r.f32().unwrap() - want).abs() < 1e-3);
    }
    assert_eq!((r.varint().unwrap(), r.bool().unwrap()), (1, true));
    r.finish().unwrap();
    assert_eq!(sent[1].data, hex("11 00 01 01"));
    // The interaction left sneak held; an attack carries it.
    let (name, data) = step.one(Serverbound, "attack", &hex("11"));
    assert_eq!((name.as_str(), data), ("interact", hex("11 01 01")));
    step.serverbound("player_input", &hex("00")).unwrap();
    let (name, data) = step.one(Serverbound, "spectator_action", &hex("11"));
    assert_eq!((name.as_str(), data), ("interact", hex("11 01 00")));
    assert!(
        step.serverbound("interact", &interact_26_1(1, 2, [0.0; 3], false))
            .is_err()
    );
}

#[test]
fn interact_at_becomes_one_interaction_and_attacks_follow_the_game_mode() {
    let mut step = down();
    let mut at = Writer::new();
    at.varint(17)
        .varint(2)
        .f32(0.25)
        .f32(1.5)
        .f32(-0.25)
        .varint(0)
        .bool(false);
    let (name, data) = step.one(Serverbound, "interact", &at.buf);
    assert_eq!(name, "interact");
    let mut r = Reader::new(&data);
    assert_eq!((r.varint().unwrap(), r.varint().unwrap()), (17, 0));
    let location = read_lp_vec3(&mut r).unwrap();
    assert!(!r.bool().unwrap());
    r.finish().unwrap();
    for (got, want) in location.iter().zip([0.25, 1.5, -0.25]) {
        assert!((got - want).abs() < 1e-3, "{location:?}");
    }
    // The plain follow-up is already covered.
    assert!(
        step.serverbound("interact", &hex("11 00 00 00"))
            .unwrap()
            .is_empty()
    );
    // Without an interact_at before it, it interacts at the entity's origin.
    let (_, data) = step.one(Serverbound, "interact", &hex("11 00 00 00"));
    assert_eq!(data, interact_26_1(17, 0, [0.0; 3], false));
    assert_eq!(
        step.one(Serverbound, "interact", &hex("11 01 00")),
        ("attack".into(), hex("11"))
    );
    // Game event 3 (change game mode) to spectator.
    step.clientbound("game_event", &hex("03 40400000")).unwrap();
    assert_eq!(
        step.one(Serverbound, "interact", &hex("11 01 00")),
        ("spectator_action".into(), hex("11"))
    );
    assert!(step.serverbound("interact", &hex("11 03 00")).is_err());
    assert!(step.serverbound("interact", &hex("11 00 00")).is_err());
}

#[test]
fn game_rule_changes_become_commands_and_value_requests_are_dropped() {
    let mut w = Writer::new();
    w.varint(3);
    w.string("minecraft:keep_inventory").string("true");
    w.string("minecraft:random_tick_speed").string("-3");
    w.string("minecraft:bad rule").string("x");
    let sent = up().serverbound("set_game_rule", &w.buf).unwrap();
    let commands: Vec<String> = sent
        .iter()
        .map(|Sent { name, data, .. }| {
            assert_eq!(name, "chat_command");
            Reader::new(data).str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(
        commands,
        [
            "gamerule minecraft:keep_inventory true",
            "gamerule minecraft:random_tick_speed -3"
        ]
    );
    assert!(
        up().serverbound("client_command", &hex("02"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        up().one(Serverbound, "client_command", &hex("01")).1,
        hex("01")
    );
    assert!(
        down()
            .clientbound("game_rule_values", &hex("00"))
            .unwrap()
            .is_empty()
    );
    assert!(
        down()
            .clientbound("low_disk_space_warning", &[])
            .unwrap()
            .is_empty()
    );
}

/// Every `set_time` of a real session converts, in both orientations.
#[test]
fn recorded_sessions_convert() {
    for (make, client, backend, direction) in [
        (UP, NEWER, OLDER, Direction::Clientbound),
        (DOWN, OLDER, NEWER, Direction::Clientbound),
    ] {
        let Some(converted) = corpus_through(make, client, backend, direction, &["set_time"])
        else {
            eprintln!("no corpus for {backend}, skipped");
            continue;
        };
        assert!(converted.len() > 5, "{backend}: {}", converted.len());
        for (_, _, sent) in converted {
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].name, "set_time");
        }
    }
}

/// Print what the steps make of the serverbound packets the corpus lacks,
/// for an independent schema check: `cargo test dump_for_schema_check --
/// --ignored --nocapture`.
#[test]
#[ignore]
fn dump_for_schema_check() {
    let mut step = up();
    let print = |sent| print_for_schema_check(NEWER, OLDER, sent);
    print(
        step.serverbound("interact", &interact_26_1(5, 0, [0.5, 1.0, 0.5], false))
            .unwrap(),
    );
    print(step.serverbound("attack", &hex("05")).unwrap());
    print(step.serverbound("spectator_action", &hex("05")).unwrap());
    let mut w = Writer::new();
    w.varint(1)
        .string("minecraft:keep_inventory")
        .string("true");
    print(step.serverbound("set_game_rule", &w.buf).unwrap());
    let mut step = down();
    let print = |sent| print_for_schema_check(OLDER, NEWER, sent);
    let mut at = Writer::new();
    at.varint(5)
        .varint(2)
        .f32(0.5)
        .f32(1.0)
        .f32(0.5)
        .varint(1)
        .bool(true);
    print(step.serverbound("interact", &at.buf).unwrap());
    print(step.serverbound("interact", &hex("05 00 00 00")).unwrap());
    print(step.serverbound("interact", &hex("05 01 00")).unwrap());
    step.clientbound("game_event", &hex("03 40400000")).unwrap();
    print(step.serverbound("interact", &hex("05 01 00")).unwrap());
}
