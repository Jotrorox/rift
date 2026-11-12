use super::*;
use crate::{
    Direction, Packet, State,
    codec::testing::{Recorded, corpus},
    data,
    step::{Cx, Emitted, Step},
};
use std::collections::VecDeque;

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

/// A packet out of a step: its name in the receiver's protocol.
#[derive(Debug, PartialEq)]
struct Out {
    direction: Direction,
    state: State,
    name: String,
    data: Vec<u8>,
}

/// One connection's step, fed packets by name.
struct Harness {
    step: Box<dyn Step>,
    client: i32,
    backend: i32,
}

impl Harness {
    /// A 1.21.9 client on a 1.21.7 backend.
    fn up() -> Self {
        Self {
            step: UP.unwrap()(),
            client: NEWER,
            backend: OLDER,
        }
    }

    /// A 1.21.7 client on a 1.21.9 backend.
    fn down() -> Self {
        Self {
            step: DOWN.unwrap()(),
            client: OLDER,
            backend: NEWER,
        }
    }

    fn send(
        &mut self,
        direction: Direction,
        state: State,
        name: &str,
        body: &[u8],
    ) -> io::Result<Vec<Out>> {
        let source = match direction {
            Direction::Clientbound => self.backend,
            Direction::Serverbound => self.client,
        };
        let id = data::version(source).expect_packet(state, direction, name);
        let mut emitted = VecDeque::new();
        let mut cx = Cx {
            emitted: &mut emitted,
            client_protocol: self.client,
            backend_protocol: self.backend,
        };
        let packet = Packet::new(id, body.to_vec());
        match direction {
            Direction::Clientbound => self.step.clientbound(&mut cx, state, packet)?,
            Direction::Serverbound => self.step.serverbound(&mut cx, state, packet)?,
        }
        Ok(emitted
            .into_iter()
            .map(
                |Emitted {
                     direction,
                     state,
                     packet,
                 }| {
                    let protocol = match direction {
                        Direction::Clientbound => self.client,
                        Direction::Serverbound => self.backend,
                    };
                    Out {
                        direction,
                        state,
                        name: data::version(protocol)
                            .packet_name(state, direction, packet.id)
                            .unwrap_or("?")
                            .to_owned(),
                        data: packet.data,
                    }
                },
            )
            .collect())
    }

    fn clientbound(&mut self, state: State, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
        self.send(Direction::Clientbound, state, name, body)
    }

    fn serverbound(&mut self, state: State, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
        self.send(Direction::Serverbound, state, name, body)
    }
}

fn out(direction: Direction, state: State, name: &str, data: Vec<u8>) -> Out {
    Out {
        direction,
        state,
        name: name.to_owned(),
        data,
    }
}

fn to_client(name: &str, data: Vec<u8>) -> Out {
    out(Direction::Clientbound, State::Play, name, data)
}

fn to_backend(name: &str, data: Vec<u8>) -> Out {
    out(Direction::Serverbound, State::Play, name, data)
}

/// The clientbound packets named `name` that `protocol`'s server sent.
fn recorded(protocol: i32, name: &str) -> Option<Vec<Recorded>> {
    let packets = corpus(protocol)?;
    Some(
        packets
            .into_iter()
            .filter(|packet| {
                packet.direction == Direction::Clientbound && packet.name(protocol) == name
            })
            .collect(),
    )
}

#[test]
fn every_packet_has_a_route() {
    assert_eq!(up::Up::routes().problems(), &[] as &[String]);
    assert_eq!(down::Down::routes().problems(), &[] as &[String]);
}

fn vector_bytes(vector: [f64; 3]) -> Vec<u8> {
    let mut w = Writer::new();
    lp_vec3::write(&mut w, vector);
    w.into_inner()
}

fn read_vector(bytes: &[u8]) -> [f64; 3] {
    let mut r = Reader::new(bytes);
    let vector = lp_vec3::read(&mut r).unwrap();
    r.finish().unwrap();
    vector
}

#[test]
fn packed_vectors_match_the_servers_encoding() {
    // From the 1.21.10 server: an item entity's motion and a zero vector.
    assert_eq!(read_vector(&hex("00")), [0.0; 3]);
    let motion = read_vector(&hex("f9ff7ffeebed"));
    assert_eq!(vector_bytes(motion), hex("f9ff7ffeebed"));
    assert!(motion[0].abs() < 1e-4 && (motion[1] + 0.0783).abs() < 1e-4 && motion[2].abs() < 1e-4);
    // Tiny vectors are zero; scales from 4 need the VarInt continuation.
    assert_eq!(vector_bytes([1e-6, -1e-6, 0.0]), [0]);
    for vector in [
        [3.9, -3.9, 0.5],
        [100.25, 0.0, -7.0],
        [-0.125, 0.0625, 0.0],
        [1.7e10, -1.7e10, 1.0],
    ] {
        let bytes = vector_bytes(vector);
        let back = read_vector(&bytes);
        let scale = vector.iter().fold(0.0f64, |m, c| m.max(c.abs())).ceil();
        for (a, b) in vector.iter().zip(back) {
            assert!((a - b).abs() <= scale / 16383.0, "{vector:?} -> {back:?}");
        }
        assert_eq!(vector_bytes(back), bytes, "{vector:?}");
    }
    assert_eq!(vector_bytes([f64::NAN, 0.0, 0.0]), [0]);
    assert!(lp_vec3::read(&mut Reader::new(&hex("f9ff7f"))).is_err());
    assert!(lp_vec3::read(&mut Reader::new(&hex("fcffffffff"))).is_err());
}

#[test]
fn entity_motion_converts_between_shorts_and_packed_vectors() {
    // A 1.21.8 server's falling item (entity 0x2b): x 282, y 0, z 226.
    let mut up = Harness::up();
    let sent = up
        .clientbound(State::Play, "set_entity_motion", &hex("2b011a000000e2"))
        .unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].name, "set_entity_motion");
    let mut r = Reader::new(&sent[0].data);
    assert_eq!(r.varint().unwrap(), 0x2b);
    let motion = lp_vec3::read(&mut r).unwrap();
    r.finish().unwrap();
    for (component, short) in motion.iter().zip([282.0, 0.0, 226.0]) {
        assert!(
            (component - short / 8000.0).abs() < 1.0 / 16000.0,
            "{motion:?}"
        );
    }

    // A 1.21.10 server's falling item, and its zero motion.
    let mut down = Harness::down();
    let sent = down
        .clientbound(State::Play, "set_entity_motion", &hex("12f9ff7ffeebed"))
        .unwrap();
    assert_eq!(
        sent,
        [to_client("set_entity_motion", hex("120000fd8e0000"))]
    );
    let sent = down
        .clientbound(State::Play, "set_entity_motion", &hex("1300"))
        .unwrap();
    assert_eq!(
        sent,
        [to_client("set_entity_motion", hex("13000000000000"))]
    );
    // Faster than 1.21.8 can say: clamped to 3.9 blocks per tick.
    let mut body = vec![0x05];
    body.extend(vector_bytes([10.0, -10.0, 0.0]));
    let sent = down
        .clientbound(State::Play, "set_entity_motion", &body)
        .unwrap();
    assert_eq!(sent[0].data, hex("0579e086200000"));

    for bad in ["2b011a0000", "2b011a000000e200", "12f9ff7ffeebed00"] {
        let error = up
            .clientbound(State::Play, "set_entity_motion", &hex(bad))
            .and(down.clientbound(State::Play, "set_entity_motion", &hex(bad)));
        assert!(error.is_err(), "{bad}");
    }
}

#[test]
fn entity_motion_of_real_sessions_survives_both_ways() {
    for (protocol, newer) in [(OLDER, false), (NEWER, true)] {
        let Some(packets) = recorded(protocol, "set_entity_motion") else {
            eprintln!("no corpus for {protocol}; skipped");
            continue;
        };
        assert!(!packets.is_empty());
        for packet in packets {
            if newer {
                // The writer reproduces the server's bytes exactly.
                let mut r = Reader::new(&packet.data);
                r.varint().unwrap();
                let start = packet.data.len() - r.remaining();
                let vector = lp_vec3::read(&mut r).unwrap();
                r.finish().unwrap();
                assert_eq!(vector_bytes(vector), packet.data[start..]);
                let shorts = motion_to_shorts(&packet.data).unwrap();
                let back = motion_to_vector(&shorts).unwrap();
                let again = read_vector(&back[start..]);
                for (a, b) in vector.iter().zip(again) {
                    assert!((a - b).abs() < 2.0 / 8000.0, "{vector:?} {again:?}");
                }
            } else {
                // Truncating like the server may lose one unit on the way.
                let vector = motion_to_vector(&packet.data).unwrap();
                let back = motion_to_shorts(&vector).unwrap();
                assert_eq!(back[..back.len() - 6], packet.data[..back.len() - 6]);
                let shorts = |data: &[u8]| {
                    let mut r = Reader::new(&data[data.len() - 6..]);
                    [r.i16().unwrap(), r.i16().unwrap(), r.i16().unwrap()]
                };
                for (a, b) in shorts(&back).iter().zip(shorts(&packet.data)) {
                    assert!((a - b).abs() <= 1, "{back:?} {:?}", packet.data);
                }
            }
        }
    }
}

#[test]
fn player_rotation_gains_and_loses_relative_flags() {
    let rotation = |yaw: f32, pitch: f32| {
        let mut w = Writer::new();
        w.f32(yaw).f32(pitch);
        w.into_inner()
    };
    let flagged = |yaw: f32, relative_yaw: bool, pitch: f32, relative_pitch: bool| {
        let mut w = Writer::new();
        w.f32(yaw)
            .bool(relative_yaw)
            .f32(pitch)
            .bool(relative_pitch);
        w.into_inner()
    };
    let mut up = Harness::up();
    let sent = up
        .clientbound(State::Play, "player_rotation", &rotation(90.0, -10.0))
        .unwrap();
    assert_eq!(
        sent,
        [to_client(
            "player_rotation",
            flagged(90.0, false, -10.0, false)
        )]
    );

    let mut down = Harness::down();
    let sent = down
        .clientbound(
            State::Play,
            "player_rotation",
            &flagged(45.0, false, 5.0, false),
        )
        .unwrap();
    assert_eq!(sent, [to_client("player_rotation", rotation(45.0, 5.0))]);
    // The client turned since; relative angles add to what it reported.
    let mut moved = Writer::new();
    moved.f64(1.0).f64(64.0).f64(2.0).f32(170.0).f32(20.0).u8(1);
    let sent = down
        .serverbound(State::Play, "move_player_pos_rot", &moved.buf)
        .unwrap();
    assert_eq!(sent, [to_backend("move_player_pos_rot", moved.buf.clone())]);
    let sent = down
        .clientbound(
            State::Play,
            "player_rotation",
            &flagged(15.0, true, 30.0, false),
        )
        .unwrap();
    assert_eq!(sent, [to_client("player_rotation", rotation(185.0, 30.0))]);
    let mut turned = Writer::new();
    turned.f32(-90.0).f32(-45.0).u8(0);
    down.serverbound(State::Play, "move_player_rot", &turned.buf)
        .unwrap();
    let sent = down
        .clientbound(
            State::Play,
            "player_rotation",
            &flagged(0.0, true, -5.0, true),
        )
        .unwrap();
    assert_eq!(sent, [to_client("player_rotation", rotation(-90.0, -50.0))]);
    // A teleport turns the player before the client reports back.
    let mut teleport = Writer::new();
    teleport.varint(7);
    for _ in 0..6 {
        teleport.f64(0.0);
    }
    teleport.f32(10.0).f32(80.0).i32(1 << 3);
    let sent = down
        .clientbound(State::Play, "player_position", &teleport.buf)
        .unwrap();
    assert_eq!(sent, [to_client("player_position", teleport.buf.clone())]);
    let sent = down
        .clientbound(
            State::Play,
            "player_rotation",
            &flagged(5.0, true, 1.0, true),
        )
        .unwrap();
    assert_eq!(sent, [to_client("player_rotation", rotation(-75.0, 81.0))]);

    assert!(
        down.clientbound(State::Play, "player_rotation", &hex("00000000020000000000"))
            .is_err()
    );
    assert!(
        up.clientbound(State::Play, "player_rotation", &hex("0000000000000000ff"))
            .is_err()
    );
}

#[test]
fn spawn_position_becomes_respawn_data_and_back() {
    // What the 1.21.8 and 1.21.10 servers sent for the same world spawn.
    let old = hex("0000000000000fc400000000");
    let new = hex("136d696e6563726166743a6f766572776f726c640000000000000fc40000000000000000");
    let mut up = Harness::up();
    let sent = up
        .clientbound(State::Play, "set_default_spawn_position", &old)
        .unwrap();
    assert_eq!(sent, [to_client("set_default_spawn_position", new.clone())]);
    let mut down = Harness::down();
    let sent = down
        .clientbound(State::Play, "set_default_spawn_position", &new)
        .unwrap();
    assert_eq!(sent, [to_client("set_default_spawn_position", old.clone())]);

    // After a respawn into the nether, the spawn position is placed there.
    let mut respawn = Writer::new();
    respawn
        .varint(1)
        .string("minecraft:the_nether")
        .i64(42)
        .u8(0)
        .i8(-1)
        .bool(false)
        .bool(false)
        .bool(false)
        .varint(0)
        .varint(32)
        .u8(0);
    let sent = up
        .clientbound(State::Play, "respawn", &respawn.buf)
        .unwrap();
    assert_eq!(sent, [to_client("respawn", respawn.buf.clone())]);
    let sent = up
        .clientbound(State::Play, "set_default_spawn_position", &old)
        .unwrap();
    let mut expected = Writer::new();
    expected
        .string("minecraft:the_nether")
        .i64(0xfc4)
        .f32(0.0)
        .f32(0.0);
    assert_eq!(sent[0].data, expected.buf);

    assert!(
        up.clientbound(State::Play, "set_default_spawn_position", &old[..8])
            .is_err()
    );
    assert!(
        down.clientbound(State::Play, "set_default_spawn_position", &old)
            .is_err()
    );
}

#[test]
fn real_logins_name_their_dimension_and_spawn_positions_convert_exactly() {
    for protocol in [OLDER, NEWER] {
        let Some(logins) = recorded(protocol, "login") else {
            eprintln!("no corpus for {protocol}; skipped");
            continue;
        };
        assert_eq!(
            spawn_dimension(&logins[0].data, false).unwrap(),
            "minecraft:overworld"
        );
    }
    let (Some(old), Some(new)) = (
        recorded(OLDER, "set_default_spawn_position"),
        recorded(NEWER, "set_default_spawn_position"),
    ) else {
        eprintln!("no corpus for {OLDER} and {NEWER}; skipped");
        return;
    };
    let (old, new) = (&old[0].data, &new[0].data);
    assert_eq!(&spawn_to_respawn_data(old, DEFAULT_DIMENSION).unwrap(), new);
    assert_eq!(&respawn_data_to_spawn(new).unwrap(), old);
}

#[test]
fn an_older_client_accepts_the_code_of_conduct_through_the_step() {
    let mut down = Harness::down();
    let mut body = Writer::new();
    body.string("Be excellent to each other.");
    let sent = down
        .clientbound(State::Configuration, "code_of_conduct", &body.buf)
        .unwrap();
    assert_eq!(
        sent,
        [out(
            Direction::Serverbound,
            State::Configuration,
            "accept_code_of_conduct",
            Vec::new()
        )]
    );
    assert!(
        down.clientbound(State::Configuration, "code_of_conduct", &[0x05, b'a'])
            .is_err()
    );
    let mut up = Harness::up();
    assert!(
        up.serverbound(State::Configuration, "accept_code_of_conduct", &[])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn debug_value_packets_do_not_reach_an_older_client() {
    let mut down = Harness::down();
    for name in [
        "debug/block_value",
        "debug/chunk_value",
        "debug/entity_value",
        "debug/event",
        "game_test_highlight_pos",
    ] {
        assert!(
            down.clientbound(State::Play, name, &[1, 2, 3])
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn tick_time_subscriptions_convert_and_keep_their_lifetimes() {
    let tick_time = data::version(NEWER)
        .expect_registry("debug_subscription")
        .id(TICK_TIME_SUBSCRIPTION)
        .unwrap();
    assert_eq!(tick_time, 0);
    let request = |ids: &[i32]| {
        let mut w = Writer::new();
        w.varint(ids.len() as i32);
        for id in ids {
            w.varint(*id);
        }
        w.into_inner()
    };

    // A 1.21.9 client subscribes once; the step renews for it.
    let mut up = Harness::up();
    let sent = up
        .serverbound(State::Play, "debug_subscription_request", &request(&[3, 0]))
        .unwrap();
    assert_eq!(sent, [to_backend("debug_sample_subscription", vec![0])]);
    let mut renewals = 0;
    for _ in 0..RENEW_AFTER_TICKS * 2 {
        let sent = up.serverbound(State::Play, "client_tick_end", &[]).unwrap();
        assert_eq!(sent[0], to_backend("client_tick_end", Vec::new()));
        renewals += sent.len() - 1;
    }
    assert_eq!(renewals, 2);
    let sent = up
        .serverbound(State::Play, "debug_subscription_request", &request(&[]))
        .unwrap();
    assert!(sent.is_empty());
    for _ in 0..RENEW_AFTER_TICKS * 2 {
        assert_eq!(
            up.serverbound(State::Play, "client_tick_end", &[])
                .unwrap()
                .len(),
            1
        );
    }
    assert!(
        up.serverbound(State::Play, "debug_subscription_request", &request(&[2]))
            .unwrap()
            .is_empty()
    );
    assert!(
        up.serverbound(State::Play, "debug_subscription_request", &hex("0201"))
            .is_err()
    );

    // A 1.21.7 client renews every five seconds; the step tells the server
    // once, and stops the subscription when the renewals stop.
    let mut down = Harness::down();
    let sent = down
        .serverbound(State::Play, "debug_sample_subscription", &[0])
        .unwrap();
    assert_eq!(
        sent,
        [to_backend("debug_subscription_request", request(&[0]))]
    );
    for _ in 0..RENEW_AFTER_TICKS {
        assert_eq!(
            down.serverbound(State::Play, "client_tick_end", &[])
                .unwrap()
                .len(),
            1
        );
    }
    assert!(
        down.serverbound(State::Play, "debug_sample_subscription", &[0])
            .unwrap()
            .is_empty()
    );
    let mut stopped = Vec::new();
    for _ in 0..EXPIRE_AFTER_TICKS {
        let mut sent = down
            .serverbound(State::Play, "client_tick_end", &[])
            .unwrap();
        stopped.extend(sent.drain(1..));
    }
    assert_eq!(
        stopped,
        [to_backend("debug_subscription_request", request(&[]))]
    );
    assert!(
        down.serverbound(State::Play, "debug_sample_subscription", &[1])
            .is_err()
    );
}

/// Registries whose IDs travel in packets; every entry of either version
/// needs a counterpart or a stand-in in the other. Block entity types of
/// shelves and copper golem statues stay unmapped on purpose: their
/// stand-in blocks have no block entity, so codecs drop that data.
const WIRE_REGISTRIES: &[&str] = &[
    "block_state",
    "block",
    "item",
    "entity_type",
    "particle_type",
    "sound_event",
    "mob_effect",
    "menu",
    "attribute",
    "potion",
    "custom_stat",
    "villager_profession",
    "villager_type",
    "game_event",
    "stat_type",
    "recipe_serializer",
    "command_argument_type",
    "data_component_type",
    "debug_subscription",
];

#[test]
fn rules_leave_nothing_on_the_wire_unmapped() {
    for (from, to) in [(OLDER, NEWER), (NEWER, OLDER)] {
        for registry in WIRE_REGISTRIES {
            let unmapped = crate::mapping::unmapped(from, to, registry);
            assert!(unmapped.is_empty(), "{registry} {from}->{to}: {unmapped:?}");
        }
        let unmapped = crate::mapping::unmapped(from, to, "block_entity_type");
        let names: Vec<&str> = unmapped.iter().map(|(_, name)| name.as_str()).collect();
        let expected: &[&str] = if from == NEWER {
            &["shelf", "copper_golem_statue"]
        } else {
            &[]
        };
        assert_eq!(names, expected);
    }
}
