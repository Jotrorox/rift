use super::{
    NEWER, OLDER,
    down::Down,
    harness::{canonical, codec_of, entries, hex, mangle, one, recorded_login, send},
    play,
    up::Up,
};
use crate::{
    Direction, State,
    codec::testing::{Recorded, corpus},
    nbt::Nbt,
};

const CLIENTBOUND: Direction = Direction::Clientbound;
const SERVERBOUND: Direction = Direction::Serverbound;

/// Real packets of the official servers' sessions.
const POSITION_1_19_3: &str =
    "3ff8000000000000 c04e000000000000 bff8000000000000 00000000 00000000 00 01 00";
const POSITION_1_19_4: &str =
    "c00c000000000000 c04e000000000000 3ff8000000000000 00000000 00000000 00 01";
/// Vehicle 22 carries entity 19.
const PASSENGERS: &str = "16 01 13";

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

#[test]
fn recorded_logins_round_trip_through_their_own_layout() {
    for protocol in [OLDER, NEWER, 763] {
        let Some(login) = recorded_login(protocol) else {
            return;
        };
        let same = play::rewrite_login(&login, protocol, protocol, |_| Ok(())).unwrap();
        assert_eq!(same, login, "protocol {protocol}");
    }
}

/// Upgrading the 1.19.3 server's registry codec gives exactly what the
/// 1.19.4 server sends: same biomes, chat and dimension types, plus its
/// damage types and (with the 1.20 experiment off) empty trim registries.
#[test]
fn upgraded_codec_matches_the_official_1_19_4_one() {
    let (Some(old), Some(new)) = (recorded_login(OLDER), recorded_login(NEWER)) else {
        return;
    };
    let (name, body) = one(Up::routes(), &mut Up::new(), CLIENTBOUND, "login", &old);
    assert_eq!(name, "login");
    let (upgraded, official) = (codec_of(NEWER, &body), codec_of(NEWER, &new));
    let mut keys: Vec<_> = upgraded.iter().map(|(key, _)| key).collect();
    let mut expected: Vec<_> = official.iter().map(|(key, _)| key).collect();
    keys.sort();
    expected.sort();
    assert_eq!(keys, expected);
    for (key, value) in official.iter() {
        assert_eq!(
            canonical(upgraded.get(key).unwrap()),
            canonical(value),
            "{key}"
        );
    }
    // Everything after the codec is untouched.
    assert_eq!(
        body.len() - body_after_codec(&body),
        old.len() - body_after_codec(&old)
    );
}

/// The offset just past the registry codec of a join game packet.
fn body_after_codec(body: &[u8]) -> usize {
    let mut reader = crate::wire::Reader::new(body);
    reader.take(7).unwrap();
    for _ in 0..reader.count().unwrap() {
        reader.str().unwrap();
    }
    reader.nbt_bytes(true).unwrap();
    body.len() - reader.remaining()
}

/// Downgrading the 1.19.4 server's codec gives exactly the 1.19.3 one:
/// named precipitation (snow where it is cold) and inline sound events.
#[test]
fn downgraded_codec_matches_the_official_1_19_3_one() {
    let (Some(old), Some(new)) = (recorded_login(OLDER), recorded_login(NEWER)) else {
        return;
    };
    let (name, body) = one(Down::routes(), &mut Down::new(), CLIENTBOUND, "login", &new);
    assert_eq!(name, "login");
    let (downgraded, official) = (codec_of(OLDER, &body), codec_of(OLDER, &old));
    assert_eq!(
        canonical(&Nbt::Compound(downgraded)),
        canonical(&Nbt::Compound(official))
    );
}

#[test]
fn hurt_entity_events_become_damage_events_of_the_matching_type() {
    let Some(login) = recorded_login(OLDER) else {
        return;
    };
    let mut up = Up::new();
    let (_, body) = one(Up::routes(), &mut up, CLIENTBOUND, "login", &login);
    let codec = codec_of(NEWER, &body);
    let types = entries(&codec, "damage_type");
    let id = |name: &str| {
        let entry = types
            .iter()
            .find(|(entry, _)| *entry == format!("minecraft:{name}"))
            .unwrap();
        entry.1.int("id").unwrap() as u8
    };
    for (event, kind) in [
        (2, "generic"),
        (33, "thorns"),
        (36, "drown"),
        (37, "on_fire"),
        (44, "sweet_berry_bush"),
        (57, "freeze"),
    ] {
        let (name, body) = one(
            Up::routes(),
            &mut up,
            CLIENTBOUND,
            "entity_event",
            &[0, 0, 1, 0x2c, event],
        );
        assert_eq!(name, "damage_event");
        // Entity 300, the type, no cause, no direct cause, no position.
        assert_eq!(body, [0xac, 0x02, id(kind), 0, 0, 0], "event {event}");
    }
    // Other events, like this recorded one (24: operator level 0), pass.
    let recorded = hex("0000000118");
    assert_eq!(
        one(
            Up::routes(),
            &mut up,
            CLIENTBOUND,
            "entity_event",
            &recorded
        ),
        ("entity_event", recorded)
    );
}

#[test]
fn damage_events_become_hurt_entity_events() {
    let Some(login) = recorded_login(NEWER) else {
        return;
    };
    let mut down = Down::new();
    one(Down::routes(), &mut down, CLIENTBOUND, "login", &login);
    let codec = codec_of(NEWER, &login);
    let types = entries(&codec, "damage_type");
    for (kind, event) in [
        ("generic", 2),
        ("arrow", 2),
        ("thorns", 33),
        ("drown", 36),
        ("on_fire", 37),
        ("lava", 37),
        ("in_fire", 37),
        ("hot_floor", 37),
        ("sweet_berry_bush", 44),
        ("freeze", 57),
    ] {
        let id = types
            .iter()
            .find(|(entry, _)| *entry == format!("minecraft:{kind}"))
            .unwrap()
            .1
            .int("id")
            .unwrap() as u8;
        // Entity 300 hurt by entity 5 (sent plus one) from a position.
        let mut body = vec![0xac, 0x02, id, 6, 6, 1];
        body.extend_from_slice(&[0x3f, 0xf0, 0, 0, 0, 0, 0, 0].repeat(3));
        let (name, out) = one(
            Down::routes(),
            &mut down,
            CLIENTBOUND,
            "damage_event",
            &body,
        );
        assert_eq!(name, "entity_event");
        assert_eq!(out, [0, 0, 1, 0x2c, event], "{kind}");
    }
    // A type the join game did not list is a generic hurt.
    let (_, out) = one(
        Down::routes(),
        &mut down,
        CLIENTBOUND,
        "damage_event",
        &[1, 0x7f, 0, 0, 0],
    );
    assert_eq!(out, [0, 0, 0, 1, 2]);
}

#[test]
fn hurt_animations_convert_both_ways() {
    let up = one(
        Up::routes(),
        &mut Up::new(),
        CLIENTBOUND,
        "animate",
        &[0xac, 0x02, 1],
    );
    assert_eq!(up, ("hurt_animation", vec![0xac, 0x02, 0, 0, 0, 0]));
    // Swinging the main hand stays an animation.
    let swing = one(
        Up::routes(),
        &mut Up::new(),
        CLIENTBOUND,
        "animate",
        &[7, 0],
    );
    assert_eq!(swing, ("animate", vec![7, 0]));
    let down = one(
        Down::routes(),
        &mut Down::new(),
        CLIENTBOUND,
        "hurt_animation",
        &[0xac, 0x02, 0x42, 0xb4, 0, 0],
    );
    assert_eq!(down, ("animate", vec![0xac, 0x02, 1]));
}

#[test]
fn player_positions_lose_and_regain_the_dismount_flag() {
    let (old, new) = (hex(POSITION_1_19_3), hex(POSITION_1_19_4));
    let up = one(
        Up::routes(),
        &mut Up::new(),
        CLIENTBOUND,
        "player_position",
        &old,
    );
    assert_eq!(up, ("player_position", old[..old.len() - 1].to_vec()));
    let down = one(
        Down::routes(),
        &mut Down::new(),
        CLIENTBOUND,
        "player_position",
        &new,
    );
    let mut expected = new.clone();
    expected.push(0);
    assert_eq!(down, ("player_position", expected));
}

/// A teleport that dismounts the player becomes a passenger update without
/// the player (keeping the others), then the teleport.
#[test]
fn dismounting_teleports_update_the_vehicle_first() {
    let Some(login) = recorded_login(OLDER) else {
        return;
    };
    let mut up = Up::new();
    one(Up::routes(), &mut up, CLIENTBOUND, "login", &login);
    let player = i32::from_be_bytes(login[..4].try_into().unwrap());
    assert!(player < 64, "the recorded player ID fits one byte");
    // Vehicle 22 carries entity 19 and the player.
    let passengers = [0x16, 2, 0x13, player as u8];
    one(
        Up::routes(),
        &mut up,
        CLIENTBOUND,
        "set_passengers",
        &passengers,
    );
    let mut position = hex(POSITION_1_19_3);
    *position.last_mut().unwrap() = 1;
    let out = send(
        Up::routes(),
        &mut up,
        CLIENTBOUND,
        "player_position",
        &position,
    )
    .unwrap();
    let out: Vec<_> = out.iter().map(|out| (out.name, out.data.clone())).collect();
    assert_eq!(
        out,
        [
            ("set_passengers", hex("16 01 13")),
            ("player_position", position[..position.len() - 1].to_vec()),
        ]
    );
    // Nothing to dismount from any more.
    let out = send(
        Up::routes(),
        &mut up,
        CLIENTBOUND,
        "player_position",
        &position,
    )
    .unwrap();
    assert_eq!(out.len(), 1);
    // Riding again, then the vehicle disappears.
    one(
        Up::routes(),
        &mut up,
        CLIENTBOUND,
        "set_passengers",
        &passengers,
    );
    one(
        Up::routes(),
        &mut up,
        CLIENTBOUND,
        "remove_entities",
        &[1, 0x16],
    );
    let out = send(
        Up::routes(),
        &mut up,
        CLIENTBOUND,
        "player_position",
        &position,
    )
    .unwrap();
    assert_eq!(out.len(), 1);
    // Passengers of other vehicles do not matter.
    let other = hex(PASSENGERS);
    one(Up::routes(), &mut up, CLIENTBOUND, "set_passengers", &other);
    let out = send(
        Up::routes(),
        &mut up,
        CLIENTBOUND,
        "player_position",
        &position,
    )
    .unwrap();
    assert_eq!(out.len(), 1);
}

#[test]
fn bundle_delimiters_and_biome_updates_do_not_reach_1_19_3() {
    let mut down = Down::new();
    let out = send(
        Down::routes(),
        &mut down,
        CLIENTBOUND,
        "bundle_delimiter",
        &[],
    )
    .unwrap();
    assert!(out.is_empty());
    // One chunk at (1, -1) with a biome payload.
    let biomes = hex("01 ffffffff00000001 02 aabb");
    let out = send(
        Down::routes(),
        &mut down,
        CLIENTBOUND,
        "chunks_biomes",
        &biomes,
    )
    .unwrap();
    assert!(out.is_empty());
}

#[test]
fn serverbound_packets_pass_unchanged() {
    // Client information of the recorded 1.19.3 session.
    let body = hex("05656e5f75730c0001 7f 01 00 01");
    for (routes_name, out) in [
        (
            "up",
            one(
                Up::routes(),
                &mut Up::new(),
                SERVERBOUND,
                "client_information",
                &body,
            ),
        ),
        (
            "down",
            one(
                Down::routes(),
                &mut Down::new(),
                SERVERBOUND,
                "client_information",
                &body,
            ),
        ),
    ] {
        assert_eq!(out, ("client_information", body.clone()), "{routes_name}");
    }
}

#[test]
fn malformed_packets_fail_without_panicking() {
    let (old, new) = (hex(POSITION_1_19_3), hex(POSITION_1_19_4));
    mangle(Up::routes(), Up::new, CLIENTBOUND, "player_position", &old);
    mangle(
        Down::routes(),
        Down::new,
        CLIENTBOUND,
        "player_position",
        &new,
    );
    mangle(
        Up::routes(),
        Up::new,
        CLIENTBOUND,
        "entity_event",
        &[0, 0, 0, 1, 2],
    );
    mangle(Up::routes(), Up::new, CLIENTBOUND, "animate", &[1, 1]);
    mangle(
        Down::routes(),
        Down::new,
        CLIENTBOUND,
        "damage_event",
        &[1, 2, 0, 0, 0],
    );
    mangle(
        Down::routes(),
        Down::new,
        CLIENTBOUND,
        "hurt_animation",
        &[1, 0, 0, 0, 0],
    );
    if let (Some(old), Some(new)) = (recorded_login(OLDER), recorded_login(NEWER)) {
        mangle(Up::routes(), Up::new, CLIENTBOUND, "login", &old);
        mangle(Down::routes(), Down::new, CLIENTBOUND, "login", &new);
    }
    // A codec that is not one, and biomes without their fields.
    let bad = hex("00000001 00 00 ff 00 08 0000 0003 616263");
    assert!(send(Up::routes(), &mut Up::new(), CLIENTBOUND, "login", &bad).is_err());
}

/// Every play packet of both recorded sessions goes through the step for
/// its sender without an error: the backend's packets through the step
/// whose backend side it is, the client's through the one whose client side.
#[test]
fn every_recorded_packet_translates() {
    let (Some(old), Some(new)) = (corpus(OLDER), corpus(NEWER)) else {
        return;
    };
    let mut count = 0;
    let mut run = |protocol: i32, packets: &[Recorded], up: &mut Up, down: &mut Down| {
        for packet in packets.iter().filter(|packet| packet.state == State::Play) {
            let name = packet.name(protocol);
            let upgrading = (protocol == OLDER) == (packet.direction == CLIENTBOUND);
            let result = if upgrading {
                send(Up::routes(), up, packet.direction, name, &packet.data)
            } else {
                send(Down::routes(), down, packet.direction, name, &packet.data)
            };
            result.unwrap_or_else(|error| panic!("{protocol} {name}: {error}"));
            count += 1;
        }
    };
    let (mut up, mut down) = (Up::new(), Down::new());
    run(OLDER, &old, &mut up, &mut down);
    run(NEWER, &new, &mut up, &mut down);
    assert!(count > 3000, "{count}");
}
