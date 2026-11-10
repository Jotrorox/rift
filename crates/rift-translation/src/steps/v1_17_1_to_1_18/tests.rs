use super::{NEWER, OLDER, chunk, down::Down, down::truncate, up::Up};
use crate::{
    Direction, State,
    step::Step,
    steps::v1_16_4_to_1_17::common::{
        self, Login,
        testing::{Out, awaiting_codec, recorded, replay, send, send_id, with_stand_ins},
    },
    wire::{Reader, Writer},
};
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

/// The light part of a 1.17.1 `light_update`, after its coordinates.
fn light_of(update: &[u8]) -> ((i32, i32), Vec<u8>) {
    let mut r = Reader::new(update);
    let chunk = (r.varint().unwrap(), r.varint().unwrap());
    let light = chunk::light(&mut r).unwrap().to_vec();
    r.finish().unwrap();
    (chunk, light)
}

#[test]
fn light_travels_inside_the_chunk_going_up() {
    let packets = crate::codec::testing::corpus(OLDER);
    let Some(packets) = packets else {
        eprintln!("no corpus for {OLDER}; skipped");
        return;
    };
    with_stand_ins(|| {
        let mut up = up();
        let (mut chunks, mut lit, mut updates) = (0, 0, 0);
        let mut last_light = std::collections::HashMap::new();
        for packet in packets
            .iter()
            .filter(|packet| packet.direction == Clientbound)
        {
            let name = packet.name(OLDER);
            if name == "light_update" {
                let (chunk, light) = light_of(&packet.data);
                last_light.insert(chunk, light);
            }
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
                "level_chunk" => {
                    let out = one(outs);
                    assert_eq!(out.name, "level_chunk_with_light");
                    let mut r = Reader::new(&out.data);
                    let chunk = (r.i32().unwrap(), r.i32().unwrap());
                    chunk::column_1_17(&mut r).unwrap();
                    let light = chunk::light(&mut r).unwrap();
                    r.finish().unwrap();
                    chunks += 1;
                    if let Some(expected) = last_light.remove(&chunk) {
                        assert_eq!(light, expected);
                        lit += 1;
                    }
                }
                "light_update" => updates += outs.len(),
                _ => {}
            }
        }
        // Vanilla sends each chunk's light right before it.
        assert!(chunks > 0);
        assert_eq!(lit, chunks);
        eprintln!("{chunks} chunks, {updates} later light updates passed on");
    });
}

#[test]
fn chunks_split_into_light_then_chunk_going_down() {
    let bodies = recorded(NEWER, Clientbound, "level_chunk_with_light");
    with_stand_ins(|| {
        let mut down = down();
        for body in &bodies {
            let outs = send(
                &mut *down,
                DOWN,
                Clientbound,
                Play,
                "level_chunk_with_light",
                body,
            )
            .unwrap();
            let names: Vec<_> = outs.iter().map(|out| out.name.as_str()).collect();
            assert_eq!(names, ["light_update", "level_chunk"]);
            let (chunk, light) = light_of(&outs[0].data);
            let mut r = Reader::new(body);
            assert_eq!(chunk, (r.i32().unwrap(), r.i32().unwrap()));
            let column = chunk::column_1_18(&mut r).unwrap();
            assert_eq!(chunk::light(&mut r).unwrap(), light);
            assert_eq!(outs[1].data, [&body[..8], column].concat());
        }
    });
    let mut down = down();
    for body in bodies.iter().take(1) {
        let error = send(
            &mut *down,
            DOWN,
            Clientbound,
            Play,
            "level_chunk_with_light",
            body,
        )
        .unwrap_err();
        assert!(awaiting_codec(&error), "{error}");
    }
}

#[test]
fn light_for_loaded_chunks_passes_and_unloaded_chunks_wait_again() {
    with_stand_ins(|| {
        let mut up = up();
        let mut chunk = Writer::new();
        chunk
            .i32(3)
            .i32(-4)
            .varint(0)
            .u8(10)
            .u16(0)
            .u8(0)
            .varint(0)
            .varint(0)
            .varint(0);
        let out = one(send(&mut *up, UP, Clientbound, Play, "level_chunk", &chunk.buf).unwrap());
        assert!(out.data.ends_with(&chunk::no_light()));
        let mut update = Writer::new();
        update.varint(3).varint(-4).bytes(&chunk::no_light());
        let out = one(send(&mut *up, UP, Clientbound, Play, "light_update", &update.buf).unwrap());
        assert_eq!(
            (out.name.as_str(), out.data.as_slice()),
            ("light_update", &update.buf[..])
        );
        let mut forget = Writer::new();
        forget.i32(3).i32(-4);
        one(send(
            &mut *up,
            UP,
            Clientbound,
            Play,
            "forget_level_chunk",
            &forget.buf,
        )
        .unwrap());
        assert!(
            send(&mut *up, UP, Clientbound, Play, "light_update", &update.buf)
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
                &update.buf[..4]
            )
            .is_err()
        );
    });
}

#[test]
fn logins_convert_both_ways_on_real_sessions() {
    let (mut up, mut down) = (up(), down());
    for body in recorded(NEWER, Clientbound, "login") {
        let out = one(send(&mut *down, DOWN, Clientbound, Play, "login", &body).unwrap());
        let back = one(send(&mut *up, UP, Clientbound, Play, "login", &out.data).unwrap());
        // All but the mountain category survive; the simulation distance
        // was the view distance.
        let mut expected = Login::read(NEWER, &body).unwrap();
        expected
            .edit_biomes(|biome| common::known_category(biome, common::CATEGORIES_1_17))
            .unwrap();
        assert_eq!(back.data, expected.write(NEWER));
    }
    for body in recorded(OLDER, Clientbound, "login") {
        let out = one(send(&mut *up, UP, Clientbound, Play, "login", &body).unwrap());
        let login = Login::read(NEWER, &out.data).unwrap();
        assert_eq!(login.simulation_distance, login.view_distance);
        let mut login = login;
        login
            .edit_biomes(|biome| {
                assert!(!biome.contains("depth") && !biome.contains("scale"));
                Ok(())
            })
            .unwrap();
        one(send(&mut *down, DOWN, Clientbound, Play, "login", &out.data).unwrap());
    }
}

#[test]
fn display_objectives_fit_1_17_1() {
    let mut down = down();
    let mut long = Writer::new();
    long.i8(1).string("an_objective_name_longer_than_16");
    let out = one(send(
        &mut *down,
        DOWN,
        Clientbound,
        Play,
        "set_display_objective",
        &long.buf,
    )
    .unwrap());
    let mut expected = Writer::new();
    expected.i8(1).string("an_objective_nam");
    assert_eq!(out.data, expected.buf);
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
    for stand_ins in [false, true] {
        let run = || {
            for (name, mut step, sides, clientbound, serverbound) in [
                ("up", up(), UP, OLDER, NEWER),
                ("down", down(), DOWN, NEWER, OLDER),
            ] {
                for (direction, recorded) in
                    [(Clientbound, clientbound), (Serverbound, serverbound)]
                {
                    let Some(result) = replay(&mut *step, sides, recorded, direction) else {
                        eprintln!("no corpus for {recorded}; skipped");
                        continue;
                    };
                    assert!(
                        result.failed.is_empty(),
                        "{name} {direction:?}: {:?}",
                        result.failed
                    );
                    if stand_ins {
                        assert!(result.awaiting_codec.is_empty());
                    }
                }
            }
        };
        if stand_ins {
            with_stand_ins(run);
        } else {
            run();
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
    with_stand_ins(|| {
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
    });
}
