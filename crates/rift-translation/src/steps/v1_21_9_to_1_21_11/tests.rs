use super::*;
use crate::{
    Direction, Packet, State,
    codec::testing::corpus,
    data,
    step::{Cx, Step},
    wire::Writer,
};
use std::collections::VecDeque;

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

/// Send one packet by name through a fresh step; what comes out, by name.
fn send(
    make: Make,
    client: i32,
    backend: i32,
    direction: Direction,
    name: &str,
    body: &[u8],
) -> io::Result<Vec<(Direction, String, Vec<u8>)>> {
    let mut step: Box<dyn Step> = make();
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
        .map(|emitted| {
            let protocol = match emitted.direction {
                Direction::Clientbound => client,
                Direction::Serverbound => backend,
            };
            let name = data::version(protocol)
                .packet_name(emitted.state, emitted.direction, emitted.packet.id)
                .unwrap()
                .to_owned();
            (emitted.direction, name, emitted.packet.data)
        })
        .collect())
}

/// A 1.21.11 client on a 1.21.9 backend.
fn up(
    direction: Direction,
    name: &str,
    body: &[u8],
) -> io::Result<Vec<(Direction, String, Vec<u8>)>> {
    send(UP.unwrap(), NEWER, OLDER, direction, name, body)
}

/// A 1.21.9 client on a 1.21.11 backend.
fn down(
    direction: Direction,
    name: &str,
    body: &[u8],
) -> io::Result<Vec<(Direction, String, Vec<u8>)>> {
    send(DOWN.unwrap(), OLDER, NEWER, direction, name, body)
}

fn one(direction: Direction, name: &str, data: Vec<u8>) -> Vec<(Direction, String, Vec<u8>)> {
    vec![(direction, name.to_owned(), data)]
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
    // Both protocols name the same packets.
    for state in [State::Login, State::Configuration, State::Play] {
        for direction in [Direction::Clientbound, Direction::Serverbound] {
            assert_eq!(
                data::version(OLDER).packets(state, direction).names(),
                data::version(NEWER).packets(state, direction).names()
            );
        }
    }
}

/// What the 1.21.10 and 1.21.11 servers sent for the same default border.
const BORDER_1_21_10: &str =
    "00000000000000000000000000000000418c9c3700000000418c9c370000000000f086a70e050f";
const BORDER_1_21_11: &str =
    "00000000000000000000000000000000418c9c3700000000418c9c370000000000f086a70e05ac02";

#[test]
fn border_times_convert_between_milliseconds_seconds_and_ticks() {
    use Direction::Clientbound;
    let sent = up(Clientbound, "initialize_border", &hex(BORDER_1_21_10)).unwrap();
    assert_eq!(
        sent,
        one(Clientbound, "initialize_border", hex(BORDER_1_21_11))
    );
    let sent = down(Clientbound, "initialize_border", &hex(BORDER_1_21_11)).unwrap();
    assert_eq!(
        sent,
        one(Clientbound, "initialize_border", hex(BORDER_1_21_10))
    );

    // A border shrinking from 100 to 10 blocks over 30 seconds.
    let lerp = |time: i64| {
        let mut w = Writer::new();
        w.f64(100.0).f64(10.0).varlong(time);
        w.into_inner()
    };
    let sent = up(Clientbound, "set_border_lerp_size", &lerp(30_000)).unwrap();
    assert_eq!(sent, one(Clientbound, "set_border_lerp_size", lerp(600)));
    let sent = down(Clientbound, "set_border_lerp_size", &lerp(600)).unwrap();
    assert_eq!(sent, one(Clientbound, "set_border_lerp_size", lerp(30_000)));
    // Remaining milliseconds round to the nearest tick.
    let sent = up(Clientbound, "set_border_lerp_size", &lerp(29_976)).unwrap();
    assert_eq!(sent[0].2, lerp(600));

    let delay = |time: i32| {
        let mut w = Writer::new();
        w.varint(time);
        w.into_inner()
    };
    let sent = up(Clientbound, "set_border_warning_delay", &delay(15)).unwrap();
    assert_eq!(
        sent,
        one(Clientbound, "set_border_warning_delay", delay(300))
    );
    let sent = down(Clientbound, "set_border_warning_delay", &delay(300)).unwrap();
    assert_eq!(
        sent,
        one(Clientbound, "set_border_warning_delay", delay(15))
    );
    let sent = down(Clientbound, "set_border_warning_delay", &delay(5)).unwrap();
    assert_eq!(sent[0].2, delay(0));

    for (name, bad) in [
        ("initialize_border", &hex(BORDER_1_21_10)[..20]),
        ("set_border_lerp_size", &lerp(-1)[..]),
        ("set_border_warning_delay", &[0x80][..]),
    ] {
        assert!(up(Clientbound, name, bad).is_err(), "{name}");
        assert!(down(Clientbound, name, bad).is_err(), "{name}");
    }
    let mut trailing = hex(BORDER_1_21_10);
    trailing.push(0);
    assert!(up(Clientbound, "initialize_border", &trailing).is_err());
}

#[test]
fn real_borders_convert_into_what_the_other_server_sends() {
    let border = |protocol: i32| {
        let packets = corpus(protocol)?;
        packets
            .into_iter()
            .find(|packet| packet.name(protocol) == "initialize_border")
            .map(|packet| packet.data)
    };
    let (Some(older), Some(newer)) = (border(OLDER), border(NEWER)) else {
        eprintln!("no corpus for {OLDER} and {NEWER}; skipped");
        return;
    };
    assert_eq!(Border::ToTicks.initialize(&older).unwrap(), newer);
    assert_eq!(Border::FromTicks.initialize(&newer).unwrap(), older);
}

#[test]
fn spear_stabs_do_not_reach_an_older_server() {
    use Direction::Serverbound;
    let action = |action: i32| {
        let mut w = Writer::new();
        w.varint(action).i64(0).u8(1).varint(12);
        w.into_inner()
    };
    assert!(
        up(Serverbound, "player_action", &action(STAB))
            .unwrap()
            .is_empty()
    );
    for kept in [0, 2, 6] {
        assert_eq!(
            up(Serverbound, "player_action", &action(kept)).unwrap(),
            one(Serverbound, "player_action", action(kept))
        );
    }
    assert!(up(Serverbound, "player_action", &action(8)).is_err());
    assert!(up(Serverbound, "player_action", &action(0)[..5]).is_err());
    // An older client never stabs; its actions pass through untouched.
    assert_eq!(
        down(Serverbound, "player_action", &action(3)).unwrap(),
        one(Serverbound, "player_action", action(3))
    );
}

/// Drive a translator through login and configuration into play.
fn playing(client: i32, backend: i32) -> crate::Translator {
    use Direction::{Clientbound, Serverbound};
    let mut translator = crate::Translator::new(client, backend).unwrap().unwrap();
    let mut out = crate::Output::default();
    for (direction, state, name) in [
        (Clientbound, State::Login, "login_finished"),
        (Serverbound, State::Login, "login_acknowledged"),
        (Clientbound, State::Configuration, "finish_configuration"),
        (Serverbound, State::Configuration, "finish_configuration"),
    ] {
        let protocol = match direction {
            Clientbound => backend,
            Serverbound => client,
        };
        let id = data::version(protocol).expect_packet(state, direction, name);
        let body = if name == "login_finished" {
            let mut w = Writer::new();
            w.uuid(1).string("Steve").varint(0);
            w.into_inner()
        } else {
            Vec::new()
        };
        let packet = Packet::new(id, body);
        match direction {
            Clientbound => translator.clientbound(packet, &mut out).unwrap(),
            Serverbound => translator.serverbound(packet, &mut out).unwrap(),
        }
    }
    translator
}

#[test]
fn chains_across_1_21_7_to_1_21_11_convert_what_each_pair_changed() {
    // A 1.21.11 client on a 1.21.8 server, and the reverse, for the same
    // world spawn and default border both servers sent.
    let spawn_1_21_8 = hex("0000000000000fc400000000");
    let spawn_1_21_10 =
        hex("136d696e6563726166743a6f766572776f726c640000000000000fc40000000000000000");
    for (client, backend, border, spawn, expected_border, expected_spawn) in [
        (
            NEWER,
            772,
            hex(BORDER_1_21_10),
            spawn_1_21_8.clone(),
            hex(BORDER_1_21_11),
            spawn_1_21_10.clone(),
        ),
        (
            772,
            NEWER,
            hex(BORDER_1_21_11),
            spawn_1_21_10.clone(),
            hex(BORDER_1_21_10),
            spawn_1_21_8.clone(),
        ),
    ] {
        let mut translator = playing(client, backend);
        for (name, body, expected) in [
            ("initialize_border", border.clone(), expected_border.clone()),
            (
                "set_default_spawn_position",
                spawn.clone(),
                expected_spawn.clone(),
            ),
        ] {
            let id =
                data::version(backend).expect_packet(State::Play, Direction::Clientbound, name);
            let mut out = crate::Output::default();
            translator
                .clientbound(Packet::new(id, body), &mut out)
                .unwrap();
            assert_eq!(out.client.len(), 1, "{name} {client} on {backend}");
            let sent = &out.client[0].packet;
            assert_eq!(
                data::version(client).packet_name(State::Play, Direction::Clientbound, sent.id),
                Some(name)
            );
            assert_eq!(sent.data, expected, "{name} {client} on {backend}");
        }
    }
}
