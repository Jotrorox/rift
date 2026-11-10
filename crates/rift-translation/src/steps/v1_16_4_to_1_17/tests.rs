use super::{
    NEWER, OLDER,
    common::testing::{Out, awaiting_codec, hex, recorded, replay, send},
    down::Down,
    up::Up,
};
use crate::{Direction, State, step::Step, wire::Writer};
use Direction::{Clientbound, Serverbound};
use State::Play;

const UP: (i32, i32) = (NEWER, OLDER);
const DOWN: (i32, i32) = (OLDER, NEWER);

fn up() -> Box<dyn Step> {
    (super::UP.unwrap())()
}

fn down() -> Box<dyn Step> {
    (super::DOWN.unwrap())()
}

fn one(outs: Vec<Out>) -> Out {
    assert_eq!(outs.len(), 1, "{outs:?}");
    outs.into_iter().next().unwrap()
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

#[test]
fn titles_split_and_merge() {
    let (mut up, mut down) = (up(), down());
    let times = hex("03000000050000001400000005");
    let out = one(send(&mut *up, UP, Clientbound, Play, "set_titles", &times).unwrap());
    assert_eq!(out.name, "set_titles_animation");
    assert_eq!(out.data, hex("000000050000001400000005"));
    let back = one(send(&mut *down, DOWN, Clientbound, Play, &out.name, &out.data).unwrap());
    assert_eq!((back.name.as_str(), back.data), ("set_titles", times));

    for (action, reset) in [(4u8, 0u8), (5, 1)] {
        let out = one(send(&mut *up, UP, Clientbound, Play, "set_titles", &[action]).unwrap());
        assert_eq!(
            (out.name.as_str(), out.data.as_slice()),
            ("clear_titles", &[reset][..])
        );
        let back = one(send(&mut *down, DOWN, Clientbound, Play, &out.name, &out.data).unwrap());
        assert_eq!(back.data, [action]);
    }

    // Real 1.16.5 title packets; their text waits for the text codec.
    for body in recorded(OLDER, Clientbound, "set_titles") {
        match send(&mut *up, UP, Clientbound, Play, "set_titles", &body) {
            Ok(outs) => assert!(one(outs).name.starts_with("set_")),
            Err(error) => assert!(awaiting_codec(&error), "{error}"),
        }
    }
    assert!(send(&mut *up, UP, Clientbound, Play, "set_titles", &[6]).is_err());
    assert!(send(&mut *up, UP, Clientbound, Play, "set_titles", &[3, 0]).is_err());
    assert!(send(&mut *down, DOWN, Clientbound, Play, "clear_titles", &[1, 1]).is_err());
}

#[test]
fn world_border_splits_and_merges() {
    let (mut up, mut down) = (up(), down());
    // The initialisation a 1.16.5 server sent on join.
    let initialize =
        hex("0300000000000000000000000000000000418c9c3800000000418c9c380000000000f086a70e050f");
    let out = one(send(&mut *up, UP, Clientbound, Play, "set_border", &initialize).unwrap());
    assert_eq!(out.name, "initialize_border");
    assert_eq!(out.data, initialize[1..]);
    let back = one(send(&mut *down, DOWN, Clientbound, Play, &out.name, &out.data).unwrap());
    assert_eq!((back.name.as_str(), back.data), ("set_border", initialize));

    let size = hex("00407f400000000000");
    let out = one(send(&mut *up, UP, Clientbound, Play, "set_border", &size).unwrap());
    // What the 1.17 server itself sent for the same `/worldborder set`.
    assert_eq!(
        (out.name.as_str(), out.data),
        ("set_border_size", hex("407f400000000000"))
    );

    let mut lerp = Writer::new();
    lerp.varint(1).f64(10.0).f64(20.0).varlong(5000);
    let out = one(send(&mut *up, UP, Clientbound, Play, "set_border", &lerp.buf).unwrap());
    assert_eq!(out.name, "set_border_lerp_size");
    for action in [2, 4, 5] {
        let mut body = Writer::new();
        body.varint(action);
        if action == 2 {
            body.f64(1.0).f64(2.0);
        } else {
            body.varint(7);
        }
        let out = one(send(&mut *up, UP, Clientbound, Play, "set_border", &body.buf).unwrap());
        let back = one(send(&mut *down, DOWN, Clientbound, Play, &out.name, &out.data).unwrap());
        assert_eq!(back.data, body.buf);
    }
    assert!(send(&mut *up, UP, Clientbound, Play, "set_border", &[6]).is_err());
    assert!(send(&mut *up, UP, Clientbound, Play, "set_border", &[0, 1]).is_err());
    assert!(
        send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "set_border_size",
            &[0; 9]
        )
        .is_err()
    );
}

#[test]
fn combat_splits_and_merges() {
    let (mut up, mut down) = (up(), down());
    let out = one(send(&mut *up, UP, Clientbound, Play, "player_combat", &[0]).unwrap());
    assert_eq!(
        (out.name.as_str(), out.data.len()),
        ("player_combat_enter", 0)
    );
    let mut end = Writer::new();
    end.varint(1).varint(40).i32(17);
    let out = one(send(&mut *up, UP, Clientbound, Play, "player_combat", &end.buf).unwrap());
    assert_eq!(
        (out.name.as_str(), &out.data[..]),
        ("player_combat_end", &end.buf[1..])
    );
    let back = one(send(&mut *down, DOWN, Clientbound, Play, &out.name, &out.data).unwrap());
    assert_eq!(back.data, end.buf);
    let back = one(send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "player_combat_enter",
        &[],
    )
    .unwrap());
    assert_eq!((back.name.as_str(), back.data), ("player_combat", vec![0]));

    let mut kill = Writer::new();
    kill.varint(2).varint(1).i32(-1).string("{\"text\":\"x\"}");
    match send(&mut *up, UP, Clientbound, Play, "player_combat", &kill.buf) {
        Ok(outs) => assert_eq!(one(outs).name, "player_combat_kill"),
        Err(error) => assert!(awaiting_codec(&error), "{error}"),
    }
    assert!(send(&mut *up, UP, Clientbound, Play, "player_combat", &[3]).is_err());
    assert!(send(&mut *up, UP, Clientbound, Play, "player_combat", &[0, 0]).is_err());
}

#[test]
fn entity_removal_is_split_and_merged() {
    let (mut up, mut down) = (up(), down());
    assert!(
        send(
            &mut *up,
            UP,
            Clientbound,
            Play,
            "remove_entities",
            &hex("03020381")
        )
        .is_err()
    );
    let outs = send(
        &mut *up,
        UP,
        Clientbound,
        Play,
        "remove_entities",
        &hex("03020304"),
    )
    .unwrap();
    let ids: Vec<_> = outs
        .iter()
        .map(|out| (out.name.as_str(), out.data.clone()))
        .collect();
    assert_eq!(
        ids,
        [2u8, 3, 4].map(|id| ("remove_entity", vec![id])).to_vec()
    );
    let back = one(send(&mut *down, DOWN, Clientbound, Play, "remove_entity", &[2]).unwrap());
    // The 1.16.5 server's own packet for one entity.
    assert_eq!(
        (back.name.as_str(), back.data),
        ("remove_entities", hex("0102"))
    );
    assert!(
        send(
            &mut *up,
            UP,
            Clientbound,
            Play,
            "remove_entities",
            &hex("0502")
        )
        .is_err()
    );
    assert!(
        send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "remove_entity",
            &[0x80]
        )
        .is_err()
    );
}

#[test]
fn rejected_transactions_ride_on_pings() {
    let mut up = up();
    let accepted = hex("02000701");
    assert!(
        send(&mut *up, UP, Clientbound, Play, "container_ack", &accepted)
            .unwrap()
            .is_empty()
    );
    let rejected = hex("02fff900");
    let ping = one(send(&mut *up, UP, Clientbound, Play, "container_ack", &rejected).unwrap());
    assert_eq!(ping.name, "ping");
    let ack = one(send(&mut *up, UP, Serverbound, Play, "pong", &ping.data).unwrap());
    assert_eq!(
        (ack.name.as_str(), ack.data),
        ("container_ack", hex("02fff901"))
    );
    // A pong for a ping the step did not send has nothing to confirm.
    assert!(
        send(&mut *up, UP, Serverbound, Play, "pong", &[0, 0, 0, 1])
            .unwrap()
            .is_empty()
    );
    assert!(
        send(
            &mut *up,
            UP,
            Clientbound,
            Play,
            "container_ack",
            &hex("02fff902")
        )
        .is_err()
    );
}

#[test]
fn pings_ride_on_rejected_transactions() {
    let mut down = down();
    let mut transactions = Vec::new();
    for id in [7i32, -5, 1 << 20] {
        let out = one(send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "ping",
            &id.to_be_bytes(),
        )
        .unwrap());
        assert_eq!(out.name, "container_ack");
        assert_eq!((out.data[0], out.data[3]), (0, 0), "window 0, rejected");
        transactions.push(out.data);
    }
    // The client confirms the second: the skipped first is answered too.
    let mut answer = transactions[1].clone();
    answer[3] = 1;
    let pongs = send(
        &mut *down,
        DOWN,
        Serverbound,
        Play,
        "container_ack",
        &answer,
    )
    .unwrap();
    let ids: Vec<_> = pongs
        .iter()
        .map(|out| (out.name.as_str(), out.data.clone()))
        .collect();
    assert_eq!(
        ids,
        [
            ("pong", 7i32.to_be_bytes().to_vec()),
            ("pong", (-5i32).to_be_bytes().to_vec())
        ]
    );
    assert!(
        send(
            &mut *down,
            DOWN,
            Serverbound,
            Play,
            "container_ack",
            &answer
        )
        .unwrap()
        .is_empty()
    );
    let mut last = transactions[2].clone();
    last[3] = 1;
    let pong = one(send(&mut *down, DOWN, Serverbound, Play, "container_ack", &last).unwrap());
    assert_eq!(pong.data, (1i32 << 20).to_be_bytes());
}

#[test]
fn unanswered_pings_are_bounded() {
    let mut down = down();
    let mut pongs = 0;
    for id in 0..300i32 {
        for out in send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "ping",
            &id.to_be_bytes(),
        )
        .unwrap()
        {
            pongs += usize::from(out.name == "pong");
        }
    }
    assert_eq!(pongs, 300 - super::down::MAX_PINGS);
}

#[test]
fn positions_and_spawn_points_gain_and_lose_fields() {
    let (mut up, mut down) = (up(), down());
    let position = hex("405ee0000000000040100000000000004064d0000000000000000000000000000001");
    let out = one(send(
        &mut *up,
        UP,
        Clientbound,
        Play,
        "player_position",
        &position,
    )
    .unwrap());
    assert_eq!(out.data, [&position[..], &[0]].concat());
    let back = one(send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "player_position",
        &out.data,
    )
    .unwrap());
    assert_eq!(back.data, position);
    // The 1.17 server's own packet, with its dismount flag.
    let real = hex("c05da000000000004010000000000000c060f000000000000000000000000000000100");
    let back = one(send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "player_position",
        &real,
    )
    .unwrap());
    assert_eq!(back.data, real[..real.len() - 1]);

    let spawn = hex("00002000000a0004");
    let out = one(send(
        &mut *up,
        UP,
        Clientbound,
        Play,
        "set_default_spawn_position",
        &spawn,
    )
    .unwrap());
    assert_eq!(out.data, hex("00002000000a000400000000"));
    let back = one(send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "set_default_spawn_position",
        &out.data,
    )
    .unwrap());
    assert_eq!(back.data, spawn);
    assert!(
        send(
            &mut *up,
            UP,
            Clientbound,
            Play,
            "player_position",
            &position[1..]
        )
        .is_err()
    );
    assert!(
        send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "set_default_spawn_position",
            &spawn
        )
        .is_err()
    );
}

#[test]
fn client_settings_gain_and_lose_text_filtering() {
    let (mut up, mut down) = (up(), down());
    let mut old = Writer::new();
    old.string("en_us")
        .i8(10)
        .varint(0)
        .bool(true)
        .u8(0x7f)
        .varint(1);
    let out = one(send(
        &mut *down,
        DOWN,
        Serverbound,
        Play,
        "client_information",
        &old.buf,
    )
    .unwrap());
    assert_eq!(out.data, [&old.buf[..], &[0]].concat());
    let back = one(send(
        &mut *up,
        UP,
        Serverbound,
        Play,
        "client_information",
        &out.data,
    )
    .unwrap());
    assert_eq!(back.data, old.buf);
    assert!(
        send(
            &mut *up,
            UP,
            Serverbound,
            Play,
            "client_information",
            &old.buf
        )
        .is_err()
    );
}

fn stack(id: i32, count: i8) -> Vec<u8> {
    let mut w = Writer::new();
    w.bool(true).varint(id).i8(count).u8(0);
    w.into_inner()
}

#[test]
fn clicks_name_the_stack_the_backend_expects() {
    let mut up = up();
    // The backend fills slot 36 of the inventory.
    let mut content = Writer::new();
    content.u8(0).i16(46);
    for slot in 0..46 {
        if slot == 36 {
            content.bytes(&stack(1, 64));
        } else {
            content.u8(0);
        }
    }
    send(
        &mut *up,
        UP,
        Clientbound,
        Play,
        "container_set_content",
        &content.buf,
    )
    .unwrap();

    // A 1.17 pick-up of slot 36 without changed slots (the item codec is not
    // needed for an untracked report).
    let mut click = Writer::new();
    click
        .u8(5)
        .i16(36)
        .i8(0)
        .varint(0)
        .varint(0)
        .bytes(&stack(1, 64));
    let out = one(send(
        &mut *up,
        UP,
        Serverbound,
        Play,
        "container_click",
        &click.buf,
    )
    .unwrap());
    assert_eq!(
        out.data,
        [&hex("05002400000100"), &[0u8][..]].concat(),
        "window 5 is unknown"
    );

    let mut click = Writer::new();
    click
        .u8(0)
        .i16(36)
        .i8(0)
        .varint(0)
        .varint(0)
        .bytes(&stack(1, 64));
    let out = one(send(
        &mut *up,
        UP,
        Serverbound,
        Play,
        "container_click",
        &click.buf,
    )
    .unwrap());
    let mut expected = Writer::new();
    expected
        .u8(0)
        .i16(36)
        .i8(0)
        .i16(2)
        .varint(0)
        .bytes(&stack(1, 64));
    assert_eq!(out.data, expected.buf);

    // Reports of changed slots in a followed window need the item codec.
    let mut click = Writer::new();
    click
        .u8(0)
        .i16(36)
        .i8(0)
        .varint(0)
        .varint(1)
        .i16(36)
        .u8(0)
        .bytes(&stack(1, 64));
    match send(
        &mut *up,
        UP,
        Serverbound,
        Play,
        "container_click",
        &click.buf,
    ) {
        Ok(outs) => assert_eq!(one(outs).data[4..6], [0, 3]),
        Err(error) => assert!(awaiting_codec(&error), "{error}"),
    }
    // Throwing names no stack.
    let mut click = Writer::new();
    click.u8(0).i16(36).i8(0).varint(4).varint(0).u8(0);
    let out = one(send(
        &mut *up,
        UP,
        Serverbound,
        Play,
        "container_click",
        &click.buf,
    )
    .unwrap());
    assert_eq!(*out.data.last().unwrap(), 0);
    assert!(send(&mut *up, UP, Serverbound, Play, "container_click", &[0, 0]).is_err());
}

#[test]
fn old_clicks_report_the_picked_up_stack_as_carried() {
    let mut down = down();
    let mut click = Writer::new();
    click.u8(0).i16(-999).i8(0).i16(4).varint(0).u8(0);
    match send(
        &mut *down,
        DOWN,
        Serverbound,
        Play,
        "container_click",
        &click.buf,
    ) {
        Ok(outs) => assert_eq!(one(outs).data, hex("00fc1900000000")),
        Err(error) => assert!(awaiting_codec(&error), "{error}"),
    }
    assert!(
        send(
            &mut *down,
            DOWN,
            Serverbound,
            Play,
            "container_click",
            &[0, 0]
        )
        .is_err()
    );
}

#[test]
fn logins_convert_both_ways_on_real_sessions() {
    let (mut up, mut down) = (up(), down());
    for body in recorded(OLDER, Clientbound, "login") {
        let out = one(send(&mut *up, UP, Clientbound, Play, "login", &body).unwrap());
        let back = one(send(&mut *down, DOWN, Clientbound, Play, "login", &out.data).unwrap());
        assert_eq!(back.data, body);
    }
    for body in recorded(NEWER, Clientbound, "login") {
        let out = one(send(&mut *down, DOWN, Clientbound, Play, "login", &body).unwrap());
        one(send(&mut *up, UP, Clientbound, Play, "login", &out.data).unwrap());
    }
    for body in recorded(OLDER, Clientbound, "respawn") {
        let out = one(send(&mut *up, UP, Clientbound, Play, "respawn", &body).unwrap());
        let back = one(send(&mut *down, DOWN, Clientbound, Play, "respawn", &out.data).unwrap());
        assert_eq!(back.data, body);
    }
}

#[test]
fn recorded_sessions_replay_through_both_steps() {
    for (name, step, sides, clientbound, serverbound) in [
        ("up", up(), UP, OLDER, NEWER),
        ("down", down(), DOWN, NEWER, OLDER),
    ] {
        let mut step = step;
        for (direction, recorded) in [(Clientbound, clientbound), (Serverbound, serverbound)] {
            let Some(result) = replay(&mut *step, sides, recorded, direction) else {
                eprintln!("no corpus for {recorded}; skipped");
                continue;
            };
            eprintln!(
                "{name} {direction:?}: awaiting codecs {:?}",
                result.awaiting_codec
            );
            assert!(
                result.failed.is_empty(),
                "{name} {direction:?}: {:?}",
                result.failed
            );
            assert!(result.passed.values().sum::<usize>() > 2);
        }
    }
}

#[test]
fn garbage_never_panics() {
    let garbage: [&[u8]; 5] = [
        &[],
        &[0xff],
        &[0x80; 8],
        &[1, 0xff, 0xff, 0xff, 0x0f],
        &[0x7f; 40],
    ];
    for (mut step, sides, direction) in [
        (up(), UP, Clientbound),
        (up(), UP, Serverbound),
        (down(), DOWN, Clientbound),
        (down(), DOWN, Serverbound),
    ] {
        let source = match direction {
            Clientbound => sides.1,
            Serverbound => sides.0,
        };
        let names = crate::data::version(source)
            .packets(Play, direction)
            .names()
            .to_vec();
        for name in &names {
            for body in garbage {
                let _ = send(&mut *step, sides, direction, Play, name, body);
            }
        }
    }
}
