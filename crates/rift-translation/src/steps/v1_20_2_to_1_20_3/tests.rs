use super::support::{Harness, hex, names, truncations};
use super::{NEWER, OLDER, down, pack_uuid, up};
use crate::{
    Direction, State,
    codec::testing::corpus,
    wire::{Reader, Writer},
};

fn up() -> Harness {
    Harness::new(up::STEP.unwrap(), NEWER, OLDER)
}

fn down() -> Harness {
    Harness::new(down::STEP.unwrap(), OLDER, NEWER)
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
}

// Both servers' `set_score` for the same event: Corpus has 7 kills.
const OLDER_SCORE: &str = "06436f72707573 00 056b696c6c73 07";
const NEWER_SCORE: &str = "06436f72707573 056b696c6c73 07 00 00";

#[test]
fn scores_change_layout_and_removal_becomes_its_own_packet() {
    let out = up()
        .clientbound(State::Play, "set_score", &hex(OLDER_SCORE))
        .unwrap();
    assert_eq!(names(&out), ["set_score"]);
    assert_eq!(out[0].data, hex(NEWER_SCORE));
    let out = down()
        .clientbound(State::Play, "set_score", &hex(NEWER_SCORE))
        .unwrap();
    assert_eq!(out[0].data, hex(OLDER_SCORE));

    // A display name and a fixed number format are dropped going down.
    let mut formatted = hex("06436f72707573 056b696c6c73 07");
    formatted.extend(hex("01 08 0003 616263 01 02 08 0001 78"));
    let out = down()
        .clientbound(State::Play, "set_score", &formatted)
        .unwrap();
    assert_eq!(out[0].data, hex(OLDER_SCORE));

    // Removing a score from one objective, and from every objective.
    let out = up()
        .clientbound(
            State::Play,
            "set_score",
            &hex("06436f72707573 01 056b696c6c73"),
        )
        .unwrap();
    assert_eq!(names(&out), ["reset_score"]);
    assert_eq!(out[0].data, hex("06436f72707573 01 056b696c6c73"));
    let out = up()
        .clientbound(State::Play, "set_score", &hex("06436f72707573 01 00"))
        .unwrap();
    assert_eq!(out[0].data, hex("06436f72707573 00"));
    let out = down()
        .clientbound(State::Play, "reset_score", &hex("06436f72707573 00"))
        .unwrap();
    assert_eq!(names(&out), ["set_score"]);
    assert_eq!(out[0].data, hex("06436f72707573 01 00"));
    let out = down()
        .clientbound(
            State::Play,
            "reset_score",
            &hex("06436f72707573 01 056b696c6c73"),
        )
        .unwrap();
    assert_eq!(out[0].data, hex("06436f72707573 01 056b696c6c73"));

    for body in truncations(&hex(OLDER_SCORE)) {
        assert!(up().clientbound(State::Play, "set_score", body).is_err());
    }
    for body in truncations(&formatted) {
        assert!(down().clientbound(State::Play, "set_score", body).is_err());
    }
    assert!(
        up().clientbound(State::Play, "set_score", &hex("00 02 00"))
            .is_err()
    );
    assert!(
        down()
            .clientbound(State::Play, "set_score", &hex("00 00 00 00 01 03"))
            .is_err()
    );
}

// The official 1.20.2 server's Join Game, and a Respawn into the same
// world in its layout (overworld, survival, nothing kept).
const OLDER_LOGIN: &str = "00000001 00 03 136d696e6563726166743a6f766572776f726c64 \
    116d696e6563726166743a7468655f656e64 146d696e6563726166743a7468655f6e6574686572 \
    14 02 02 00 01 00 136d696e6563726166743a6f766572776f726c64 \
    136d696e6563726166743a6f766572776f726c64 ec1aa6535db52fd3 01 ff 00 01 00 00";
const OLDER_RESPAWN: &str = "136d696e6563726166743a6f766572776f726c64 \
    136d696e6563726166743a6f766572776f726c64 ec1aa6535db52fd3 00 01 00 00 00 00 00";

#[test]
fn level_loading_starts_after_login_and_respawn_going_up() {
    let mut up = up();
    for (name, body) in [("login", OLDER_LOGIN), ("respawn", OLDER_RESPAWN)] {
        let body = hex(body);
        let out = up.clientbound(State::Play, name, &body).unwrap();
        assert_eq!(names(&out), [name, "game_event"]);
        assert_eq!(out[0].data, body);
        // What the 1.20.3 server itself sends: event 13, value 0.
        assert_eq!(out[1].data, hex("0d00000000"));
    }
}

#[test]
fn level_loading_start_resends_the_spawn_position_going_down() {
    let mut down = down();
    let spawn = hex("0000000000000fc400000000");
    // Before any spawn position, there is nothing to resend.
    let out = down
        .clientbound(State::Play, "game_event", &hex("0d00000000"))
        .unwrap();
    assert!(out.is_empty());
    let out = down
        .clientbound(State::Play, "set_default_spawn_position", &spawn)
        .unwrap();
    assert_eq!(out[0].data, spawn);
    let out = down
        .clientbound(State::Play, "game_event", &hex("0d00000000"))
        .unwrap();
    assert_eq!(names(&out), ["set_default_spawn_position"]);
    assert_eq!(out[0].data, spawn);
    let out = down
        .clientbound(State::Play, "game_event", &hex("033f800000"))
        .unwrap();
    assert_eq!(
        (names(&out), &out[0].data),
        (vec!["game_event"], &hex("033f800000"))
    );
    assert!(
        down.clientbound(State::Play, "game_event", &hex("0d000000"))
            .is_err()
    );
    assert!(
        down.clientbound(State::Play, "set_default_spawn_position", &spawn[1..])
            .is_err()
    );
}

#[test]
fn tick_rate_and_crafter_slots_are_dropped() {
    assert!(
        down()
            .clientbound(State::Play, "ticking_state", &hex("41a0000000"))
            .unwrap()
            .is_empty()
    );
    assert!(
        down()
            .clientbound(State::Play, "ticking_step", &hex("00"))
            .unwrap()
            .is_empty()
    );
    let out = up()
        .serverbound(
            State::Play,
            "container_slot_state_changed",
            &hex("00 01 01"),
        )
        .unwrap();
    assert!(out.is_empty());
}

fn push(uuid: Option<u128>, url: &str, hash: &str, required: bool) -> Vec<u8> {
    let mut body = Writer::new();
    if let Some(uuid) = uuid {
        body.uuid(uuid);
    }
    body.string(url).string(hash).bool(required).bool(false);
    body.buf
}

#[test]
fn resource_packs_gain_ids_going_up() {
    const URL: &str = "https://example.com/pack.zip";
    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";
    for state in [State::Configuration, State::Play] {
        let mut up = up();
        let out = up
            .clientbound(state, "resource_pack_push", &push(None, URL, HASH, true))
            .unwrap();
        assert_eq!(names(&out), ["resource_pack_pop", "resource_pack_push"]);
        assert_eq!(out[0].data, [0]);
        assert_eq!(out[1].data, push(Some(pack_uuid(URL)), URL, HASH, true));
        assert!(out.iter().all(|out| out.state == state));

        let status = |status: i32| {
            let mut body = Writer::new();
            body.uuid(pack_uuid(URL)).varint(status);
            body.buf
        };
        for (newer, older) in [
            (3, Some(3)),
            (4, None),
            (0, Some(0)),
            (1, Some(1)),
            (2, Some(2)),
        ]
        .into_iter()
        .chain((5..=7).map(|status| (status, Some(2))))
        {
            let out = up
                .serverbound(state, "resource_pack", &status(newer))
                .unwrap();
            assert_eq!(
                out.first().map(|out| out.data.clone()),
                older.map(|status| vec![status])
            );
        }
        assert!(up.serverbound(state, "resource_pack", &status(8)).is_err());
        for body in truncations(&push(None, URL, HASH, true)) {
            assert!(up.clientbound(state, "resource_pack_push", body).is_err());
        }
    }
    // Different URLs get different IDs, each a version 3 UUID.
    let (a, b) = (pack_uuid("a"), pack_uuid("b"));
    assert_ne!(a, b);
    assert_eq!((a >> 76) & 0xf, 3);
    assert_eq!((a >> 62) & 0b11, 0b10);
}

#[test]
fn resource_pack_answers_name_the_last_pack_going_down() {
    const ID: u128 = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef;
    for state in [State::Configuration, State::Play] {
        let mut down = down();
        let out = down
            .clientbound(state, "resource_pack_push", &push(Some(ID), "u", "", false))
            .unwrap();
        assert_eq!(names(&out), ["resource_pack_push"]);
        assert_eq!(out[0].data, push(None, "u", "", false));
        let out = down.serverbound(state, "resource_pack", &[3]).unwrap();
        let mut reader = Reader::new(&out[0].data);
        assert_eq!((reader.uuid().unwrap(), reader.varint().unwrap()), (ID, 3));
        assert!(down.serverbound(state, "resource_pack", &[4]).is_err());
        let mut pop = vec![1];
        pop.extend(ID.to_be_bytes());
        assert!(
            down.clientbound(state, "resource_pack_pop", &pop)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn jigsaw_priorities_are_dropped_and_defaulted() {
    let mut older = hex("0000000000000000");
    for text in [
        "minecraft:a",
        "minecraft:b",
        "minecraft:pool",
        "minecraft:stone",
        "rollable",
    ] {
        let mut writer = Writer::new();
        writer.string(text);
        older.extend(writer.buf);
    }
    let mut newer = older.clone();
    newer.extend([5, 7]);
    let out = up()
        .serverbound(State::Play, "set_jigsaw_block", &newer)
        .unwrap();
    assert_eq!(out[0].data, older);
    let mut defaulted = older.clone();
    defaulted.extend([0, 0]);
    let out = down()
        .serverbound(State::Play, "set_jigsaw_block", &older)
        .unwrap();
    assert_eq!(out[0].data, defaulted);
    for body in truncations(&newer) {
        assert!(
            up().serverbound(State::Play, "set_jigsaw_block", body)
                .is_err()
        );
    }
}

/// Every packet of both recorded sessions goes through both steps: the
/// sender's clientbound traffic and the client's serverbound traffic. Codec
/// packets pass untouched until the codecs exist; the step's own must not
/// fail.
#[test]
fn recorded_sessions_go_through_both_steps() {
    for (make, client, backend) in [
        (up::STEP.unwrap(), NEWER, OLDER),
        (down::STEP.unwrap(), OLDER, NEWER),
    ] {
        let (Some(from_backend), Some(from_client)) = (corpus(backend), corpus(client)) else {
            eprintln!("corpus for {backend} or {client} not captured: skipped");
            continue;
        };
        let mut harness = Harness::new(make, client, backend);
        let packets = from_backend
            .iter()
            .filter(|packet| packet.direction == Direction::Clientbound)
            .map(|packet| (packet, backend))
            .chain(
                from_client
                    .iter()
                    .filter(|packet| packet.direction == Direction::Serverbound)
                    .map(|packet| (packet, client)),
            );
        for (packet, protocol) in packets {
            harness
                .send_id(packet.direction, packet.state, packet.id, &packet.data)
                .unwrap_or_else(|error| {
                    panic!(
                        "{protocol} {:?} {}: {error}",
                        packet.state,
                        packet.name(protocol)
                    )
                });
        }
    }
}

/// The pair's stand-ins keep a block's shape and state where they can.
#[test]
fn new_blocks_stand_in_with_their_states() {
    let (newer, older) = (crate::data::version(NEWER), crate::data::version(OLDER));
    let mapping = crate::mapping::mapping(NEWER, OLDER, "block_state");
    for (from, to) in [
        (
            "waxed_weathered_copper_door[facing=east,half=upper,hinge=right,open=true,powered=false]",
            "warped_door[facing=east,half=upper,hinge=right,open=true,powered=false]",
        ),
        (
            "tuff_brick_stairs[facing=south,half=top,shape=outer_left,waterlogged=true]",
            "stone_brick_stairs[facing=south,half=top,shape=outer_left,waterlogged=true]",
        ),
        (
            "tuff_slab[type=double,waterlogged=false]",
            "andesite_slab[type=double,waterlogged=false]",
        ),
        (
            "copper_bulb[lit=true,powered=false]",
            "redstone_lamp[lit=true]",
        ),
        ("short_grass", "grass"),
    ] {
        let id = newer
            .block_states
            .parse(from)
            .unwrap_or_else(|| panic!("{from}"));
        let mapped = mapping
            .get(id)
            .unwrap_or_else(|| panic!("{from} is unmapped"));
        assert_eq!(older.block_states.describe(mapped).unwrap(), to);
    }
}
