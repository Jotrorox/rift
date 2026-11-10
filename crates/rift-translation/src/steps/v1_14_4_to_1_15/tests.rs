use super::support::harness::{Harness, body, hex, replay, truncations_fail};
use super::{NEWER, OLDER, down, up};
use crate::{Direction, data};

fn upgrading() -> Harness {
    Harness::new((up::STEP.unwrap())(), NEWER, OLDER)
}

fn downgrading() -> Harness {
    Harness::new((down::STEP.unwrap())(), OLDER, NEWER)
}

// The `login` each official server sent in the recorded sessions.
const LOGIN_1_14_4: &str = "00000001 01 00000000 14 04666c6174 02 00";
const LOGIN_1_15: &str = "00000001 01 00000000 108046903f960996 14 04666c6174 02 00 01";

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
}

#[test]
fn login_gains_and_loses_the_seed_and_respawn_screen() {
    let sent = downgrading().one(Direction::Clientbound, "login", &hex(LOGIN_1_15), "login");
    assert_eq!(sent, hex(LOGIN_1_14_4));
    let sent = upgrading().one(Direction::Clientbound, "login", &hex(LOGIN_1_14_4), "login");
    // As the 1.15 server sent it, but with an unknown (zero) seed.
    assert_eq!(
        sent,
        hex("00000001 01 00000000 0000000000000000 14 04666c6174 02 00 01")
    );
}

#[test]
fn respawn_gains_and_loses_the_seed() {
    let old = body(|w| {
        w.i32(-1).u8(0).string("default");
    });
    let new = body(|w| {
        w.i32(-1).i64(0).u8(0).string("default");
    });
    assert_eq!(
        upgrading().one(Direction::Clientbound, "respawn", &old, "respawn"),
        new
    );
    let seeded = body(|w| {
        w.i32(-1).i64(0x1234).u8(0).string("default");
    });
    assert_eq!(
        downgrading().one(Direction::Clientbound, "respawn", &seeded, "respawn"),
        old
    );
}

#[test]
fn immediate_respawn_is_followed_and_emulated() {
    let mut step = downgrading();
    let enabled = body(|w| {
        w.u8(11).f32(1.0);
    });
    assert!(step.clientbound("game_event", &enabled).unwrap().is_empty());
    let dead = body(|w| {
        w.f32(0.0).varint(20).f32(5.0);
    });
    let sent = step.clientbound("set_health", &dead).unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].name, "set_health");
    assert_eq!(sent[0].data, dead);
    assert_eq!(
        (
            sent[1].direction,
            sent[1].name.as_str(),
            sent[1].data.as_slice()
        ),
        (Direction::Serverbound, "client_command", &[0][..])
    );
    // Alive, or with the rule off again, nothing is asked for.
    let alive = body(|w| {
        w.f32(3.0).varint(20).f32(5.0);
    });
    assert_eq!(step.clientbound("set_health", &alive).unwrap().len(), 1);
    let disabled = body(|w| {
        w.u8(11).f32(0.0);
    });
    assert!(
        step.clientbound("game_event", &disabled)
            .unwrap()
            .is_empty()
    );
    assert_eq!(step.clientbound("set_health", &dead).unwrap().len(), 1);
    // A login with the respawn screen disabled turns it on, too.
    step.clientbound(
        "login",
        &hex("00000001 01 00000000 108046903f960996 14 04666c6174 02 00 00"),
    )
    .unwrap();
    assert_eq!(step.clientbound("set_health", &dead).unwrap().len(), 2);
    // Other events pass unchanged.
    let rain = body(|w| {
        w.u8(1).f32(0.0);
    });
    assert_eq!(
        step.one(Direction::Clientbound, "game_event", &rain, "game_event"),
        rain
    );
}

#[test]
fn explosions_bring_their_sound_to_old_clients() {
    let explode = body(|w| {
        w.f32(1.5).f32(64.0).f32(-2.0).f32(4.0).i32(1);
        w.i8(1).i8(0).i8(-1);
        w.f32(0.0).f32(0.0).f32(0.0);
    });
    let sent = downgrading().clientbound("explode", &explode).unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        (sent[0].name.as_str(), &sent[0].data),
        ("explode", &explode)
    );
    assert_eq!(sent[1].name, "sound");
    let sound = data::version(OLDER)
        .expect_registry("sound_event")
        .id("entity.generic.explode")
        .unwrap();
    let expected = body(|w| {
        w.varint(sound)
            .varint(4)
            .i32(12)
            .i32(512)
            .i32(-16)
            .f32(4.0)
            .f32(0.7);
    });
    assert_eq!(sent[1].data, expected);
    // A 1.15 client plays it itself.
    let sent = upgrading().clientbound("explode", &explode).unwrap();
    assert_eq!(sent.len(), 1);
}

#[test]
fn malformed_packets_fail() {
    truncations_fail(
        &mut downgrading(),
        Direction::Clientbound,
        "login",
        &hex(LOGIN_1_15),
    );
    truncations_fail(
        &mut upgrading(),
        Direction::Clientbound,
        "login",
        &hex(LOGIN_1_14_4),
    );
    let respawn = body(|w| {
        w.i32(0).u8(0).string("default");
    });
    truncations_fail(
        &mut upgrading(),
        Direction::Clientbound,
        "respawn",
        &respawn,
    );
    let explode = body(|w| {
        w.f32(0.0).f32(0.0).f32(0.0).f32(4.0).i32(0);
        w.f32(0.0).f32(0.0).f32(0.0);
    });
    truncations_fail(
        &mut downgrading(),
        Direction::Clientbound,
        "explode",
        &explode,
    );
    let huge = body(|w| {
        w.f32(0.0).f32(0.0).f32(0.0).f32(4.0).i32(i32::MAX);
    });
    assert!(downgrading().clientbound("explode", &huge).is_err());
    let negative = body(|w| {
        w.f32(0.0).f32(0.0).f32(0.0).f32(4.0).i32(-1);
    });
    assert!(downgrading().clientbound("explode", &negative).is_err());
    truncations_fail(
        &mut downgrading(),
        Direction::Clientbound,
        "game_event",
        &[1, 0, 0, 0, 0],
    );
}

#[test]
fn recorded_sessions_pass_through() {
    let Some(count) = replay(&mut downgrading(), NEWER, Direction::Clientbound) else {
        eprintln!("no corpus for {NEWER}; skipped");
        return;
    };
    assert!(count > 1000);
    replay(&mut downgrading(), OLDER, Direction::Serverbound).unwrap();
    replay(&mut upgrading(), OLDER, Direction::Clientbound).unwrap();
    replay(&mut upgrading(), NEWER, Direction::Serverbound).unwrap();
}
