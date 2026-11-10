use super::{NEWER, OLDER, down::Down, down::truncate, up::Up};
use crate::{
    Direction, State,
    codec::{
        chunk::{
            light::{Light, LightUpdate},
            model::{Chunk, World},
        },
        registries::login::{self, Dimension},
        testing::corpus,
    },
    nbt::Compound,
    step::Step,
    steps::v1_16_4_to_1_17::common::testing::{Out, recorded, replay, send, send_id},
    wire::{Reader, Writer},
};
use Direction::{Clientbound, Serverbound};
use State::Play;
use std::collections::HashMap;

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

/// The overworld of the recorded sessions, as the chunk readers need it.
fn overworld(protocol: i32) -> World {
    World {
        sky_light: Some(true),
        min_section: Some(if protocol >= 757 { -4 } else { 0 }),
        biome_bits: None,
    }
}

/// A 1.17.1 server sends a chunk's light just before the chunk; a 1.18
/// client gets it inside the chunk, and later updates as they come.
#[test]
fn light_travels_inside_the_chunk_going_up() {
    let Some(packets) = corpus(OLDER) else {
        eprintln!("no corpus for {OLDER}; skipped");
        return;
    };
    let mut up = up();
    let (mut chunks, mut lit, mut passed) = (0, 0, 0);
    let mut held: HashMap<(i32, i32), Light> = HashMap::new();
    for packet in packets
        .iter()
        .filter(|packet| packet.direction == Clientbound)
    {
        let name = packet.name(OLDER);
        let outs = send_id(
            &mut *up,
            UP,
            Clientbound,
            packet.state,
            packet.id,
            &packet.data,
        )
        .unwrap();
        match name {
            "light_update" => {
                let update = LightUpdate::read(&packet.data, OLDER).unwrap();
                if outs.is_empty() {
                    held.insert((update.x, update.z), update.light);
                } else {
                    // Light for a chunk the client has passes on unchanged.
                    assert_eq!(one(outs).data, packet.data);
                    passed += 1;
                }
            }
            "level_chunk" => {
                let out = one(outs);
                assert_eq!(out.name, "level_chunk_with_light");
                let chunk = Chunk::read(&out.data, NEWER, &overworld(NEWER)).unwrap();
                assert_eq!(chunk.sections.len(), 16);
                chunks += 1;
                if let Some(light) = held.remove(&(chunk.x, chunk.z)) {
                    assert_eq!(chunk.light.as_ref(), Some(&light));
                    lit += 1;
                }
            }
            _ => {}
        }
    }
    // Vanilla sends each chunk's light right before it.
    assert!(chunks > 0);
    assert_eq!(lit, chunks);
    eprintln!("{chunks} chunks, {passed} later light updates passed on");
}

/// A 1.18 chunk going to a 1.17.1 client is split into its light, then the
/// chunk with every one of the taller world's sections.
#[test]
fn chunks_split_into_light_then_chunk_going_down() {
    let Some(packets) = corpus(NEWER) else {
        eprintln!("no corpus for {NEWER}; skipped");
        return;
    };
    let mut down = down();
    let mut chunks = 0;
    for packet in packets
        .iter()
        .filter(|packet| packet.direction == Clientbound)
    {
        let outs = send_id(
            &mut *down,
            DOWN,
            Clientbound,
            packet.state,
            packet.id,
            &packet.data,
        )
        .unwrap();
        if packet.name(NEWER) != "level_chunk_with_light" {
            continue;
        }
        let names: Vec<_> = outs.iter().map(|out| out.name.as_str()).collect();
        assert_eq!(names, ["light_update", "level_chunk"]);
        let original = Chunk::read(&packet.data, NEWER, &overworld(NEWER)).unwrap();
        let update = LightUpdate::read(&outs[0].data, OLDER).unwrap();
        assert_eq!((update.x, update.z), (original.x, original.z));
        assert_eq!(Some(update.light), original.light);
        let chunk = Chunk::read(&outs[1].data, OLDER, &overworld(NEWER)).unwrap();
        assert_eq!(chunk.sections.len(), 24);
        assert_eq!(chunk.block_entities.len(), original.block_entities.len());
        chunks += 1;
    }
    assert!(chunks > 0);
}

/// The light update and the chunk a 1.17.1 server recorded first.
fn first_light_and_chunk() -> Option<(Vec<u8>, Vec<u8>)> {
    let updates = recorded(OLDER, Clientbound, "light_update");
    let chunks = recorded(OLDER, Clientbound, "level_chunk");
    let chunk = chunks.first()?.clone();
    let mut r = Reader::new(&chunk);
    let at = (r.i32().unwrap(), r.i32().unwrap());
    let update = updates.into_iter().find(|update| {
        let update = LightUpdate::read(update, OLDER).unwrap();
        (update.x, update.z) == at
    })?;
    Some((update, chunk))
}

#[test]
fn light_for_loaded_chunks_passes_and_unloaded_chunks_wait_again() {
    let Some((update, chunk)) = first_light_and_chunk() else {
        eprintln!("no corpus for {OLDER}; skipped");
        return;
    };
    let mut up = up();
    assert!(
        send(&mut *up, UP, Clientbound, Play, "light_update", &update)
            .unwrap()
            .is_empty()
    );
    one(send(&mut *up, UP, Clientbound, Play, "level_chunk", &chunk).unwrap());
    let out = one(send(&mut *up, UP, Clientbound, Play, "light_update", &update).unwrap());
    assert_eq!((out.name.as_str(), &out.data), ("light_update", &update));
    let forget = chunk[..8].to_vec();
    one(send(
        &mut *up,
        UP,
        Clientbound,
        Play,
        "forget_level_chunk",
        &forget,
    )
    .unwrap());
    assert!(
        send(&mut *up, UP, Clientbound, Play, "light_update", &update)
            .unwrap()
            .is_empty()
    );
    assert!(
        send(
            &mut *up,
            UP,
            Clientbound,
            Play,
            "light_update",
            &update[..4]
        )
        .is_err()
    );
}

/// The registry parts of a `login` and what follows the world name: seed,
/// players, view distance, then the simulation distance from 1.18.
fn read_login(protocol: i32, data: &[u8]) -> (login::Login, i32, i32) {
    let mut r = Reader::new(data);
    let parsed = login::read_login(protocol, &mut r).unwrap();
    r.i64().unwrap();
    r.varint().unwrap();
    let view = r.varint().unwrap();
    let simulation = if protocol >= 757 {
        r.varint().unwrap()
    } else {
        view
    };
    for _ in 0..4 {
        r.bool().unwrap();
    }
    r.finish().unwrap();
    (parsed, view, simulation)
}

fn elements<'a>(login: &'a login::Login, registry: &str) -> Vec<&'a Compound> {
    login
        .codec
        .as_ref()
        .unwrap()
        .iter()
        .find(|known| known.name == registry)
        .unwrap()
        .entries
        .iter()
        .map(|entry| entry.element.as_ref().unwrap().as_compound().unwrap())
        .collect()
}

/// Biome categories a 1.17 client knows; any other fails its codec.
const CATEGORIES_1_17: &[&str] = &[
    "none",
    "taiga",
    "extreme_hills",
    "jungle",
    "mesa",
    "plains",
    "savanna",
    "icy",
    "the_end",
    "beach",
    "forest",
    "ocean",
    "desert",
    "river",
    "swamp",
    "mushroom",
    "nether",
    "underground",
];

#[test]
fn logins_convert_both_ways_on_real_sessions() {
    for body in recorded(NEWER, Clientbound, "login") {
        let mut down = down();
        let out = one(send(&mut *down, DOWN, Clientbound, Play, "login", &body).unwrap());
        let (login, _, _) = read_login(OLDER, &out.data);
        for biome in elements(&login, "worldgen/biome") {
            let category = biome.str("category").unwrap();
            assert!(CATEGORIES_1_17.contains(&category), "{category}");
            assert!(biome.contains("depth") && biome.contains("scale"));
        }
        // The 1.18 overworld keeps its height: the chunks carry 24 sections.
        let Dimension::Element(dimension) = &login.dimension else {
            panic!("{:?}", login.dimension);
        };
        assert_eq!(
            (dimension.int("min_y"), dimension.int("height")),
            (Some(-64), Some(384))
        );
        let overworld = elements(&login, "dimension_type")[0];
        assert_eq!(overworld.int("height"), Some(384));
    }
    for body in recorded(OLDER, Clientbound, "login") {
        let mut up = up();
        let out = one(send(&mut *up, UP, Clientbound, Play, "login", &body).unwrap());
        let (login, view, simulation) = read_login(NEWER, &out.data);
        assert_eq!(simulation, view);
        for biome in elements(&login, "worldgen/biome") {
            assert!(!biome.contains("depth") && !biome.contains("scale"));
        }
        let mut down = down();
        let back = one(send(&mut *down, DOWN, Clientbound, Play, "login", &out.data).unwrap());
        read_login(OLDER, &back.data);
    }
}

#[test]
fn respawns_keep_their_world() {
    for (protocol, mut step, sides) in [(NEWER, down(), DOWN), (OLDER, up(), UP)] {
        for body in recorded(protocol, Clientbound, "login") {
            send(&mut *step, sides, Clientbound, Play, "login", &body).unwrap();
        }
        for body in recorded(protocol, Clientbound, "respawn") {
            let out = one(send(&mut *step, sides, Clientbound, Play, "respawn", &body).unwrap());
            let target = if protocol == NEWER { OLDER } else { NEWER };
            let mut r = Reader::new(&out.data);
            let (_, world) = login::read_respawn(target, &mut r).unwrap();
            let (_, sent) = login::read_respawn(protocol, &mut Reader::new(&body)).unwrap();
            assert_eq!(world, sent);
            // Everything after the world name is the same.
            assert!(body.ends_with(r.peek()));
        }
    }
}

#[test]
fn scoreboard_names_fit_1_17_1() {
    let mut down = down();
    let long = "an_objective_name_longer_than_16";
    let mut display = Writer::new();
    display.i8(1).string(long);
    let out = one(send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "set_display_objective",
        &display.buf,
    )
    .unwrap());
    let mut expected = Writer::new();
    expected.i8(1).string("an_objective_nam");
    assert_eq!(out.data, expected.buf);

    let mut objective = Writer::new();
    objective
        .string(long)
        .i8(0)
        .string("{\"text\":\"x\"}")
        .varint(0);
    let out = one(send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "set_objective",
        &objective.buf,
    )
    .unwrap());
    let mut expected = Writer::new();
    expected
        .string("an_objective_nam")
        .i8(0)
        .string("{\"text\":\"x\"}")
        .varint(0);
    assert_eq!(out.data, expected.buf);

    let holder = "h".repeat(50);
    let mut score = Writer::new();
    score.string(&holder).varint(0).string(long).varint(7);
    let out = one(send(&mut *down, DOWN, Clientbound, Play, "set_score", &score.buf).unwrap());
    let mut expected = Writer::new();
    expected
        .string(&"h".repeat(40))
        .varint(0)
        .string("an_objective_nam")
        .varint(7);
    assert_eq!(out.data, expected.buf);

    let mut team = Writer::new();
    team.string("a_team_name_longer_than_16")
        .i8(0)
        .string("{\"text\":\"\"}")
        .i8(3)
        .string("always")
        .string("always")
        .varint(21)
        .string("{\"text\":\"\"}")
        .string("{\"text\":\"\"}")
        .varint(1)
        .string(&holder);
    let out = one(send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "set_player_team",
        &team.buf,
    )
    .unwrap());
    let mut r = Reader::new(&out.data);
    assert_eq!(r.string(16).unwrap(), "a_team_name_long");
    assert!(
        out.data.ends_with(&team.buf[27..]),
        "members keep their names"
    );

    // What a vanilla 1.18 server sends fits already and passes unchanged.
    for name in [
        "set_objective",
        "set_score",
        "set_player_team",
        "set_display_objective",
    ] {
        for body in recorded(NEWER, Clientbound, name) {
            assert_eq!(
                one(send(&mut *down, DOWN, Clientbound, Play, name, &body).unwrap()).data,
                body
            );
        }
    }
    assert!(
        send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "set_player_team",
            &team.buf[..30]
        )
        .is_err()
    );
    assert_eq!(truncate("ab😀cd", 3), "ab");
    let simulation = send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "set_simulation_distance",
        &[8],
    );
    assert!(simulation.unwrap().is_empty());
}

#[test]
fn client_settings_gain_and_lose_the_listing_choice() {
    let (mut up, mut down) = (up(), down());
    let mut old = Writer::new();
    old.string("en_us")
        .i8(12)
        .varint(0)
        .bool(true)
        .u8(0x7f)
        .varint(1)
        .bool(false);
    let out = one(send(
        &mut *down,
        DOWN,
        Serverbound,
        Play,
        "client_information",
        &old.buf,
    )
    .unwrap());
    assert_eq!(out.data, [&old.buf[..], &[1]].concat());
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

#[test]
fn recorded_sessions_replay_through_both_steps() {
    for (name, mut step, sides, clientbound, serverbound) in [
        ("up", up(), UP, OLDER, NEWER),
        ("down", down(), DOWN, NEWER, OLDER),
    ] {
        for (direction, recorded) in [(Clientbound, clientbound), (Serverbound, serverbound)] {
            let Some(result) = replay(&mut *step, sides, recorded, direction) else {
                eprintln!("no corpus for {recorded}; skipped");
                continue;
            };
            assert!(
                result.failed.is_empty(),
                "{name} {direction:?}: {:?}",
                result.failed
            );
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
        for name in crate::data::version(source)
            .packets(Play, direction)
            .names()
        {
            for body in garbage {
                let _ = send(&mut *step, sides, direction, Play, name, body);
            }
        }
    }
}
