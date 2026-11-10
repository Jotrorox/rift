//! Tests of both steps: layouts with real packets as vectors, the phase
//! bridges against the orderings a session can produce, and whole recorded
//! sessions from the corpus when it is present.
use super::{NEWER, OLDER, down::Down, layout, up::Up};
use crate::{
    Direction, Output, Packet, State, Translator,
    codec::testing::{Recorded, corpus},
    data,
    nbt::Nbt,
    wire::{Reader, Writer},
};
use std::io;

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

fn id(protocol: i32, state: State, direction: Direction, name: &str) -> i32 {
    data::version(protocol).expect_packet(state, direction, name)
}

/// A packet the translator produced, named in the receiver's protocol.
#[derive(Clone, Debug, PartialEq)]
struct Out {
    state: State,
    name: String,
    data: Vec<u8>,
}

/// A translator for one connection plus what a native server of the
/// client's protocol could not have sent, checked on every output.
struct Session {
    translator: Translator,
    client: i32,
    backend: i32,
    to_client: Vec<Out>,
    to_backend: Vec<Out>,
    /// The phase of the client's own packets, from the client's view.
    client_phase: State,
    /// The client was told to change phase and has not acknowledged yet.
    awaiting_ack: bool,
    /// The phase of packets the backend reads, from its view.
    backend_reads: State,
    /// An older client has had its Join Game.
    older_joined: bool,
}

impl Session {
    fn new(client: i32, backend: i32) -> Self {
        Self {
            translator: Translator::new(client, backend).unwrap().unwrap(),
            client,
            backend,
            to_client: Vec::new(),
            to_backend: Vec::new(),
            client_phase: State::Login,
            awaiting_ack: false,
            backend_reads: State::Login,
            older_joined: false,
        }
    }

    fn up() -> Self {
        Self::new(NEWER, OLDER)
    }

    fn down() -> Self {
        Self::new(OLDER, NEWER)
    }

    /// Feed a backend packet by name, in the backend's current phase.
    fn backend(&mut self, name: &str, body: &[u8]) -> io::Result<(Vec<Out>, Vec<Out>)> {
        let state = self.translator.backend_phase();
        let id = id(self.backend, state, Direction::Clientbound, name);
        self.backend_id(id, body)
    }

    fn backend_id(&mut self, id: i32, body: &[u8]) -> io::Result<(Vec<Out>, Vec<Out>)> {
        let state = self.translator.backend_phase();
        let name = data::version(self.backend)
            .packet_name(state, Direction::Clientbound, id)
            .unwrap()
            .to_owned();
        match (state, name.as_str()) {
            (State::Login, "login_finished") if self.backend >= 764 => {
                self.backend_reads = State::Login;
            }
            (State::Login, "login_finished") => self.backend_reads = State::Play,
            (State::Play, "start_configuration") => {}
            _ => {}
        }
        let mut out = Output::default();
        self.translator
            .clientbound(Packet::new(id, body.to_vec()), &mut out)?;
        Ok(self.record(out))
    }

    /// Feed a client packet by name, in the client's current phase.
    fn client(&mut self, name: &str, body: &[u8]) -> io::Result<(Vec<Out>, Vec<Out>)> {
        let state = self.translator.client_phase();
        let id = id(self.client, state, Direction::Serverbound, name);
        match (state, name) {
            (State::Login, "login_acknowledged") => {
                self.client_phase = State::Configuration;
                self.awaiting_ack = false;
            }
            (State::Configuration, "finish_configuration") => {
                self.client_phase = State::Play;
                self.awaiting_ack = false;
            }
            (State::Play, "configuration_acknowledged") => {
                self.client_phase = State::Configuration;
                self.awaiting_ack = false;
            }
            _ => {}
        }
        let mut out = Output::default();
        self.translator
            .serverbound(Packet::new(id, body.to_vec()), &mut out)?;
        Ok(self.record(out))
    }

    fn record(&mut self, out: Output) -> (Vec<Out>, Vec<Out>) {
        let name = |protocol: i32, state, direction, packet: &Packet| {
            data::version(protocol)
                .packet_name(state, direction, packet.id)
                .unwrap_or_else(|| panic!("{state:?} {direction:?} {:#x} unknown", packet.id))
                .to_owned()
        };
        let to_client: Vec<Out> = out
            .client
            .iter()
            .map(|sent| Out {
                state: sent.state,
                name: name(
                    self.client,
                    sent.state,
                    Direction::Clientbound,
                    &sent.packet,
                ),
                data: sent.packet.data.clone(),
            })
            .collect();
        let to_backend: Vec<Out> = out
            .backend
            .iter()
            .map(|sent| Out {
                state: sent.state,
                name: name(
                    self.backend,
                    sent.state,
                    Direction::Serverbound,
                    &sent.packet,
                ),
                data: sent.packet.data.clone(),
            })
            .collect();
        for packet in &to_client {
            self.check_clientbound(packet);
        }
        for packet in &to_backend {
            self.check_serverbound(packet);
        }
        self.to_client.extend(to_client.iter().cloned());
        self.to_backend.extend(to_backend.iter().cloned());
        (to_client, to_backend)
    }

    /// What a native server of the client's protocol sends when: nothing
    /// while the client owes an acknowledgement, configuration packets only
    /// to a configuring client, play packets only to a playing one.
    fn check_clientbound(&mut self, packet: &Out) {
        if packet.name == "disconnect" || packet.name == "login_disconnect" {
            return;
        }
        assert!(
            !self.awaiting_ack,
            "{packet:?} sent before an acknowledgement"
        );
        match packet.state {
            State::Login => {
                assert_eq!(self.client_phase, State::Login, "{packet:?}");
                if packet.name == "login_finished" && self.client >= 764 {
                    self.awaiting_ack = true;
                }
            }
            State::Configuration => {
                assert_eq!(self.client_phase, State::Configuration, "{packet:?}");
                if packet.name == "finish_configuration" {
                    self.awaiting_ack = true;
                }
            }
            State::Play => {
                if self.client >= 764 {
                    assert_eq!(self.client_phase, State::Play, "{packet:?}");
                } else {
                    // A client without configuration waits for Join Game.
                    assert!(
                        self.older_joined || packet.name == "login",
                        "{packet:?} before Join Game"
                    );
                    self.older_joined = true;
                }
                if packet.name == "start_configuration" {
                    self.awaiting_ack = true;
                }
            }
            _ => panic!("{packet:?} in an impossible state"),
        }
    }

    /// The backend reads each packet in the phase the step claims.
    fn check_serverbound(&mut self, packet: &Out) {
        if self.backend < 764 {
            assert_ne!(packet.state, State::Configuration, "{packet:?}");
            return;
        }
        match (packet.state, packet.name.as_str()) {
            (State::Login, "login_acknowledged") => self.backend_reads = State::Configuration,
            (State::Configuration, "finish_configuration") => self.backend_reads = State::Play,
            (State::Play, "configuration_acknowledged") => {
                self.backend_reads = State::Configuration;
            }
            (state, _) => assert_eq!(state, self.backend_reads, "{packet:?}"),
        }
    }

    fn names(packets: &[Out]) -> Vec<&str> {
        packets.iter().map(|packet| packet.name.as_str()).collect()
    }
}

fn varint(value: i32) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(value);
    w.buf
}

/// A minimal 1.20.1 Join Game with the 1.20.1 registries from `codec`.
fn join_older(entity: i32, codec: &Nbt) -> Vec<u8> {
    join(entity).write_older(codec)
}

fn join(entity: i32) -> layout::Join {
    layout::Join {
        entity_id: entity,
        hardcore: false,
        game_mode: 1,
        previous_game_mode: -1,
        worlds: vec!["minecraft:overworld".into(), "minecraft:the_nether".into()],
        dimension_type: "minecraft:overworld".into(),
        dimension: "minecraft:overworld".into(),
        seed: 0x0123_4567_89ab_cdef,
        max_players: 20,
        view_distance: 10,
        simulation_distance: 8,
        reduced_debug_info: false,
        respawn_screen: true,
        limited_crafting: false,
        debug: false,
        flat: true,
        death: Some(("minecraft:the_nether".into(), 0x7f)),
        portal_cooldown: 0,
    }
}

/// A small registry codec with a trim pattern, enough for the bridges.
fn codec() -> Nbt {
    let element: crate::nbt::Compound = [
        ("asset_id".to_owned(), Nbt::from("minecraft:coast")),
        ("decal".to_owned(), Nbt::Byte(0)),
    ]
    .into_iter()
    .collect();
    let entry: crate::nbt::Compound = [
        ("name".to_owned(), Nbt::from("minecraft:coast")),
        ("id".to_owned(), Nbt::Int(0)),
        ("element".to_owned(), Nbt::Compound(element)),
    ]
    .into_iter()
    .collect();
    let registry: crate::nbt::Compound = [
        ("type".to_owned(), Nbt::from("minecraft:trim_pattern")),
        ("value".to_owned(), Nbt::List(vec![Nbt::Compound(entry)])),
    ]
    .into_iter()
    .collect();
    Nbt::Compound(
        [("minecraft:trim_pattern".to_owned(), Nbt::Compound(registry))]
            .into_iter()
            .collect(),
    )
}

fn older_codec() -> Nbt {
    let mut codec = codec();
    layout::convert_registries(&mut codec, false);
    codec
}

const LOGIN_FINISHED: &str = "888b6130823b387bb012d6dc24e47d5906436f7270757300";
const BRAND: &str = "0f6d696e6563726166743a6272616e640776616e696c6c61";
const FEATURES: &str = "01116d696e6563726166743a76616e696c6c61";
const CLIENT_INFORMATION: &str = "05656e5f75730200017f010001";

#[test]
fn every_packet_has_a_route() {
    assert_eq!(Up::routes().problems(), &[] as &[String]);
    assert_eq!(Down::routes().problems(), &[] as &[String]);
}

// Layouts.

#[test]
fn login_start_gains_or_loses_the_optional_uuid() {
    // Recorded from the official 1.20.1 and 1.20.2 clients' sessions.
    let older = hex("06436f7270757301888b6130823b387bb012d6dc24e47d59");
    let newer = hex("06436f72707573888b6130823b387bb012d6dc24e47d59");
    assert_eq!(layout::hello(&newer, false).unwrap(), older);
    assert_eq!(layout::hello(&older, true).unwrap(), newer);
    // Without a UUID the offline-mode UUID stands in; "Corpus" is the
    // corpus player, recorded on an offline server.
    assert!(layout::hello(&hex("06436f72707573"), true).is_err());
    assert!(layout::hello(&hex("06436f7270757301888b"), true).is_err());
    assert_eq!(
        layout::hello(&hex("06436f7270757300"), true).unwrap(),
        newer
    );
    assert_eq!(
        layout::offline_uuid("Notch"),
        0xb50ad385_829d_3141_a216_7e7d7539ba7f
    );
    assert_eq!(
        layout::offline_uuid(""),
        0xfc5bc365_aedf_30a8_8b89_04e462e29bde
    );
}

#[test]
fn join_game_round_trips_and_converts() {
    let codec = older_codec();
    let join = join(7);
    let older = join.write_older(&codec);
    let (read, read_codec) = layout::Join::read_older(&older).unwrap();
    assert_eq!((read.clone(), read_codec), (join.clone(), codec));
    let newer = join.write_newer();
    assert_eq!(layout::Join::read_newer(&newer).unwrap(), join);
    assert!(layout::Join::read_newer(&older).is_err());
    assert!(layout::Join::read_older(&newer).is_err());
    let mut longer = newer.clone();
    longer.push(0);
    assert!(layout::Join::read_newer(&longer).is_err());
}

#[test]
fn respawn_moves_the_kept_data_flags() {
    let mut w = Writer::new();
    w.string("minecraft:overworld")
        .string("minecraft:overworld")
        .i64(42)
        .u8(0)
        .i8(-1)
        .bool(false)
        .bool(true);
    let head = w.buf.clone();
    let tail = hex("01146d696e6563726166743a7468655f6e6574686572000000000000007f05");
    let older = [head.clone(), vec![3], tail.clone()].concat();
    let newer = [head, tail.clone(), vec![3]].concat();
    assert_eq!(layout::respawn(&older, true).unwrap(), newer);
    assert_eq!(layout::respawn(&newer, false).unwrap(), older);
    // Without a death location.
    let older = [&older[..older.len() - tail.len()], &[0, 0][..]].concat();
    let newer = layout::respawn(&older, true).unwrap();
    assert_eq!(&newer[newer.len() - 3..], [0, 0, 3]);
    assert_eq!(layout::respawn(&newer, false).unwrap(), older);
    for end in 0..older.len() {
        assert!(layout::respawn(&older[..end], true).is_err());
        assert!(layout::respawn(&newer[..end], false).is_err());
    }
}

#[test]
fn tag_query_roots_lose_or_gain_their_name() {
    // Transaction 5, then a compound named "name" holding x: 7.
    let named = hex("050a00046e616d65030001780000000700");
    let nameless = hex("050a030001780000000700");
    assert_eq!(layout::tag_query(&named, true).unwrap(), nameless);
    // The root name is dropped and comes back empty.
    assert_eq!(
        layout::tag_query(&nameless, false).unwrap(),
        hex("050a0000030001780000000700")
    );
    // An absent tag is a lone TAG_End either way.
    assert_eq!(layout::tag_query(&hex("0500"), true).unwrap(), hex("0500"));
    assert_eq!(layout::tag_query(&hex("0500"), false).unwrap(), hex("0500"));
    assert!(layout::tag_query(&hex("050a0000030001"), true).is_err());
    assert_eq!(
        layout::tag_query(&hex("050a00"), false).unwrap(),
        hex("050a000000")
    );
    assert!(layout::tag_query(&hex("050a"), false).is_err());
    assert!(layout::tag_query(&hex("050a00"), true).is_err());
}

#[test]
fn display_objective_slots_widen_to_a_varint() {
    // Recorded: sidebar (1) for objective "corpus".
    let older = hex("0106636f72707573");
    assert_eq!(
        layout::display_objective(&older, true).unwrap(),
        Some(older.clone())
    );
    assert_eq!(
        layout::display_objective(&older, false).unwrap(),
        Some(older.clone())
    );
    let team_sidebar = hex("1206636f72707573");
    assert_eq!(
        layout::display_objective(&team_sidebar, false).unwrap(),
        Some(team_sidebar)
    );
    // Slot 19 does not exist for a 1.20.1 client.
    assert_eq!(
        layout::display_objective(&hex("1306636f72707573"), false).unwrap(),
        None
    );
    let negative = hex("ff06636f72707573");
    assert_eq!(
        layout::display_objective(&negative, true).unwrap(),
        Some(hex("ffffffff0f06636f72707573"))
    );
    assert!(layout::display_objective(&hex("01"), true).is_err());
}

#[test]
fn spawn_player_and_add_entity_convert_both_ways() {
    let player = layout::player_type(NEWER);
    assert_eq!(layout::player_type(OLDER), player);
    let spawn = {
        let mut w = Writer::new();
        w.varint(5)
            .uuid(9)
            .f64(1.5)
            .f64(64.0)
            .f64(-2.5)
            .i8(40)
            .i8(-10);
        w.buf
    };
    let added = layout::add_player(&spawn).unwrap();
    let entity = layout::AddEntity::read(&added).unwrap();
    assert_eq!(
        entity,
        layout::AddEntity {
            id: 5,
            uuid: 9,
            kind: player,
            position: [1.5, 64.0, -2.5],
            pitch: -10,
            yaw: 40,
            head_yaw: 40,
            data: 0,
            velocity: [0; 3],
        }
    );
    assert_eq!(entity.write(), added);
    assert_eq!(entity.spawn_player(), spawn);
    for end in 0..spawn.len() {
        assert!(layout::add_player(&spawn[..end]).is_err());
    }
}

#[test]
fn beacon_effects_follow_the_mob_effect_registry() {
    // Speed is 1 in 1.20.1 and 0 in 1.20.2; haste is 3 and 2.
    let mut up = Up::new_for_test();
    let mut down = Down::new_for_test();
    let newer = hex("01000102");
    let older = hex("01010103");
    let (out, _) = run(
        &mut up,
        Direction::Serverbound,
        State::Play,
        "set_beacon",
        &newer,
    );
    assert_eq!(out[0].data, older);
    let (out, _) = run(
        &mut down,
        Direction::Serverbound,
        State::Play,
        "set_beacon",
        &older,
    );
    assert_eq!(out[0].data, newer);
    let (out, _) = run(
        &mut up,
        Direction::Serverbound,
        State::Play,
        "set_beacon",
        &hex("0000"),
    );
    assert_eq!(out[0].data, hex("0000"));
}

#[test]
fn brand_payloads_are_trimmed_for_newer_clients() {
    let brand = hex(BRAND);
    assert_eq!(layout::custom_payload_to_newer(&brand).unwrap(), brand);
    let padded = [brand.clone(), vec![1, 2, 3]].concat();
    assert_eq!(layout::custom_payload_to_newer(&padded).unwrap(), brand);
    let other = hex("0b726966743a6f74686572010203");
    assert_eq!(layout::custom_payload_to_newer(&other).unwrap(), other);
    assert!(layout::custom_payload_to_newer(&hex("0f6d696e6563726166743a6272616e64")).is_err());
}

#[test]
fn trim_patterns_gain_and_lose_their_decal_flag() {
    let mut codec = codec();
    layout::convert_registries(&mut codec, false);
    let entry = |codec: &Nbt| {
        codec
            .as_compound()
            .unwrap()
            .compound("minecraft:trim_pattern")
            .unwrap()
            .list("value")
            .unwrap()[0]
            .as_compound()
            .unwrap()
            .compound("element")
            .unwrap()
            .clone()
    };
    assert!(!entry(&codec).contains("decal"));
    layout::convert_registries(&mut codec, true);
    assert_eq!(entry(&codec).get("decal"), Some(&Nbt::Byte(0)));
    assert_eq!(codec, self::codec());
}

/// Run one packet straight through a step, without a translator.
fn run<S: crate::step::Step>(
    step: &mut S,
    direction: Direction,
    state: State,
    name: &str,
    body: &[u8],
) -> (Vec<Out>, Vec<Out>) {
    try_run(step, direction, state, name, body).unwrap()
}

fn try_run<S: crate::step::Step>(
    step: &mut S,
    direction: Direction,
    state: State,
    name: &str,
    body: &[u8],
) -> io::Result<(Vec<Out>, Vec<Out>)> {
    let (client, backend) = if std::any::type_name::<S>().ends_with("Up") {
        (NEWER, OLDER)
    } else {
        (OLDER, NEWER)
    };
    let source = match direction {
        Direction::Clientbound => backend,
        Direction::Serverbound => client,
    };
    let mut emitted = std::collections::VecDeque::new();
    let mut cx = crate::step::Cx {
        emitted: &mut emitted,
        client_protocol: client,
        backend_protocol: backend,
    };
    let packet = Packet::new(id(source, state, direction, name), body.to_vec());
    match direction {
        Direction::Clientbound => step.clientbound(&mut cx, state, packet)?,
        Direction::Serverbound => step.serverbound(&mut cx, state, packet)?,
    }
    let mut out = (Vec::new(), Vec::new());
    for emitted in emitted {
        let protocol = match emitted.direction {
            Direction::Clientbound => client,
            Direction::Serverbound => backend,
        };
        let packet = Out {
            state: emitted.state,
            name: data::version(protocol)
                .packet_name(emitted.state, emitted.direction, emitted.packet.id)
                .unwrap()
                .to_owned(),
            data: emitted.packet.data,
        };
        // The direction a packet leaves in decides the list, whatever the
        // direction of the packet that caused it.
        match emitted.direction {
            d if d == direction => out.0.push(packet),
            _ => out.1.push(packet),
        }
    }
    Ok(out)
}

// The 1.20.2 client on a 1.20.1 backend.

/// Up to the backend's Join Game and the client's acknowledgement of Login
/// Success, in that order.
fn up_joined(session: &mut Session) {
    session
        .client(
            "hello",
            &hex("06436f72707573888b6130823b387bb012d6dc24e47d59"),
        )
        .unwrap();
    session
        .backend("login_finished", &hex(LOGIN_FINISHED))
        .unwrap();
    session
        .backend("login", &join_older(1, &older_codec()))
        .unwrap();
}

#[test]
fn up_holds_the_backend_until_login_is_acknowledged() {
    let mut s = Session::up();
    let (_, backend) = s
        .client(
            "hello",
            &hex("06436f72707573888b6130823b387bb012d6dc24e47d59"),
        )
        .unwrap();
    assert_eq!(
        backend[0].data,
        hex("06436f7270757301888b6130823b387bb012d6dc24e47d59")
    );
    let (client, _) = s.backend("login_finished", &hex(LOGIN_FINISHED)).unwrap();
    assert_eq!(Session::names(&client), ["login_finished"]);
    // The backend is in play and floods its join; nothing may reach the
    // client before it acknowledges.
    for (name, body) in [
        ("login", join_older(1, &older_codec())),
        ("update_enabled_features", hex(FEATURES)),
        ("custom_payload", hex(BRAND)),
        ("change_difficulty", hex("0000")),
        ("update_tags", varint(0)),
        (
            "player_position",
            hex("400c000000000000c04e000000000000401e00000000000000000000000000000001"),
        ),
        ("keep_alive", hex("0000000000000007")),
    ] {
        let (client, backend) = s.backend(name, &body).unwrap();
        assert!(client.is_empty() && backend.is_empty(), "{name}");
    }
    let (client, backend) = s.client("login_acknowledged", &[]).unwrap();
    assert!(backend.is_empty());
    assert_eq!(
        Session::names(&client),
        [
            "registry_data",
            "update_enabled_features",
            "custom_payload",
            "update_tags",
            "finish_configuration"
        ]
    );
    let registries = Nbt::from_bytes(&client[0].data, false).unwrap().unwrap();
    assert_eq!(registries, codec());
    assert!(
        client
            .iter()
            .all(|packet| packet.state == State::Configuration)
    );
    // Settings in configuration reach the backend, which is in play, at once.
    let (_, backend) = s
        .client("client_information", &hex(CLIENT_INFORMATION))
        .unwrap();
    assert_eq!(
        backend,
        [Out {
            state: State::Play,
            name: "client_information".into(),
            data: hex(CLIENT_INFORMATION)
        }]
    );
    let (client, backend) = s.client("finish_configuration", &[]).unwrap();
    assert!(backend.is_empty());
    assert_eq!(
        Session::names(&client),
        [
            "login",
            "change_difficulty",
            "player_position",
            "keep_alive"
        ]
    );
    assert_eq!(layout::Join::read_newer(&client[0].data).unwrap(), join(1));
    assert!(client.iter().all(|packet| packet.state == State::Play));
}

#[test]
fn up_finishes_configuration_on_the_first_packet_after_the_preamble() {
    let mut s = Session::up();
    up_joined(&mut s);
    let (client, _) = s.client("login_acknowledged", &[]).unwrap();
    assert_eq!(Session::names(&client), ["registry_data"]);
    let (client, _) = s.backend("update_recipes", &varint(0)).unwrap();
    assert!(client.is_empty());
    // No tags came: an empty list stands in.
    let (client, _) = s
        .backend("set_time", &hex("00000000000000020000000000000002"))
        .unwrap();
    assert_eq!(
        Session::names(&client),
        ["update_tags", "finish_configuration"]
    );
    assert_eq!(client[0].data, [0]);
    let (client, _) = s.client("finish_configuration", &[]).unwrap();
    assert_eq!(
        Session::names(&client),
        ["login", "update_recipes", "set_time"]
    );
    // Tags sent later go out in play.
    let (client, _) = s.backend("update_tags", &varint(0)).unwrap();
    assert_eq!(
        client,
        [Out {
            state: State::Play,
            name: "update_tags".into(),
            data: vec![0]
        }]
    );
}

#[test]
fn up_converts_keep_alives_and_pings_in_configuration() {
    let mut s = Session::up();
    s.client(
        "hello",
        &hex("06436f72707573888b6130823b387bb012d6dc24e47d59"),
    )
    .unwrap();
    s.backend("login_finished", &hex(LOGIN_FINISHED)).unwrap();
    s.client("login_acknowledged", &[]).unwrap();
    // Before Join Game the configuring client answers keep-alives itself.
    let keep_alive = hex("0000000000000007");
    let (client, _) = s.backend("keep_alive", &keep_alive).unwrap();
    assert_eq!(
        client,
        [Out {
            state: State::Configuration,
            name: "keep_alive".into(),
            data: keep_alive.clone()
        }]
    );
    let (_, backend) = s.client("keep_alive", &keep_alive).unwrap();
    assert_eq!(
        backend,
        [Out {
            state: State::Play,
            name: "keep_alive".into(),
            data: keep_alive.clone()
        }]
    );
    let (client, _) = s.backend("ping", &hex("00000005")).unwrap();
    assert_eq!(Session::names(&client), ["ping"]);
    assert_eq!(client[0].state, State::Configuration);
    let (_, backend) = s.client("pong", &hex("00000005")).unwrap();
    assert_eq!(Session::names(&backend), ["pong"]);
    assert_eq!(backend[0].state, State::Play);
    let (_, backend) = s.client("custom_payload", &hex(BRAND)).unwrap();
    assert_eq!(backend[0].state, State::Play);
    s.backend("login", &join_older(1, &older_codec())).unwrap();
    // After Join Game a keep-alive ends the configuration and waits for play.
    let (client, _) = s.backend("keep_alive", &keep_alive).unwrap();
    assert_eq!(
        Session::names(&client),
        ["update_tags", "finish_configuration"]
    );
    let (client, _) = s.client("finish_configuration", &[]).unwrap();
    assert_eq!(Session::names(&client), ["login", "keep_alive"]);
}

#[test]
fn up_delivers_disconnects_in_every_phase() {
    let reason = hex("0f7b2274657874223a22627965227d"); // {"text":"bye"}
    let mut s = Session::up();
    s.client(
        "hello",
        &hex("06436f72707573888b6130823b387bb012d6dc24e47d59"),
    )
    .unwrap();
    s.backend("login_finished", &hex(LOGIN_FINISHED)).unwrap();
    let (client, _) = s.backend("disconnect", &reason).unwrap();
    assert_eq!(
        client,
        [Out {
            state: State::Configuration,
            name: "disconnect".into(),
            data: reason.clone()
        }]
    );

    let mut s = Session::up();
    up_joined(&mut s);
    s.client("login_acknowledged", &[]).unwrap();
    let (client, _) = s.backend("disconnect", &reason).unwrap();
    assert_eq!(client[0].state, State::Configuration);

    let mut s = Session::up();
    up_joined(&mut s);
    s.client("login_acknowledged", &[]).unwrap();
    s.backend("set_time", &hex("00000000000000020000000000000002"))
        .unwrap();
    // Finishing: the client reads play from the finish on.
    let (client, _) = s.backend("disconnect", &reason).unwrap();
    assert_eq!(
        client,
        [Out {
            state: State::Play,
            name: "disconnect".into(),
            data: reason
        }]
    );
}

#[test]
fn up_reconfigures_for_a_second_join_game() {
    let mut s = Session::up();
    up_joined(&mut s);
    s.client("login_acknowledged", &[]).unwrap();
    s.backend("update_enabled_features", &hex(FEATURES))
        .unwrap();
    s.backend("update_tags", &hex("010e6d696e6563726166743a6974656d00"))
        .unwrap();
    s.client("finish_configuration", &[]).unwrap();
    s.backend("level_chunk_with_light", &[1]).unwrap();
    // A proxy behind the backend switches servers.
    let (client, _) = s.backend("login", &join_older(2, &older_codec())).unwrap();
    assert_eq!(
        Session::names(&client),
        ["chunk_batch_finished", "start_configuration"]
    );
    let (client, _) = s
        .backend("set_time", &hex("00000000000000020000000000000002"))
        .unwrap();
    assert!(client.is_empty());
    // The client keeps sending play packets until it acknowledges.
    let (_, backend) = s.client("swing", &varint(0)).unwrap();
    assert_eq!(Session::names(&backend), ["swing"]);
    let (client, _) = s.client("configuration_acknowledged", &[]).unwrap();
    assert_eq!(
        Session::names(&client),
        [
            "registry_data",
            "update_enabled_features",
            "update_tags",
            "finish_configuration"
        ]
    );
    assert_eq!(client[1].data, hex(FEATURES));
    assert_eq!(client[2].data, hex("010e6d696e6563726166743a6974656d00"));
    let (client, _) = s.client("finish_configuration", &[]).unwrap();
    assert_eq!(Session::names(&client), ["login", "set_time"]);
    assert_eq!(
        layout::Join::read_newer(&client[0].data).unwrap().entity_id,
        2
    );
    // And a third time.
    let (client, _) = s.backend("login", &join_older(3, &older_codec())).unwrap();
    assert_eq!(Session::names(&client), ["start_configuration"]);
}

#[test]
fn up_resends_the_resource_pack_to_a_reconfiguring_1_20_2_client() {
    let pack = {
        let mut w = Writer::new();
        w.string("https://example.invalid/pack.zip")
            .string("")
            .bool(false)
            .bool(false);
        w.buf
    };
    let mut s = Session::up();
    up_joined(&mut s);
    s.client("login_acknowledged", &[]).unwrap();
    s.backend("update_tags", &varint(0)).unwrap();
    s.client("finish_configuration", &[]).unwrap();
    s.backend("resource_pack_push", &pack).unwrap();
    s.client("resource_pack", &varint(3)).unwrap();
    s.backend("login", &join_older(2, &older_codec())).unwrap();
    s.backend("set_time", &hex("00000000000000020000000000000002"))
        .unwrap();
    let (client, _) = s.client("configuration_acknowledged", &[]).unwrap();
    assert_eq!(
        Session::names(&client),
        [
            "registry_data",
            "update_tags",
            "resource_pack_push",
            "finish_configuration"
        ]
    );
    assert_eq!(client[2].data, pack);
    // The client's answers to the repeat are not the backend's business.
    let (_, backend) = s.client("resource_pack", &varint(3)).unwrap();
    assert!(backend.is_empty());
    s.client("finish_configuration", &[]).unwrap();
    let (_, backend) = s.client("resource_pack", &varint(0)).unwrap();
    assert_eq!(Session::names(&backend), ["resource_pack"]);
}

#[test]
fn up_wraps_chunks_in_batches_and_answers_pings() {
    let mut s = Session::up();
    up_joined(&mut s);
    s.client("login_acknowledged", &[]).unwrap();
    s.backend("update_tags", &varint(0)).unwrap();
    s.client("finish_configuration", &[]).unwrap();
    let mut names = Vec::new();
    for name in [
        "level_chunk_with_light",
        "level_chunk_with_light",
        "set_time",
        "level_chunk_with_light",
    ] {
        let body = if name == "set_time" {
            hex("00000000000000020000000000000002")
        } else {
            vec![7]
        };
        let (client, _) = s.backend(name, &body).unwrap();
        names.extend(client);
    }
    assert_eq!(
        names
            .iter()
            .map(|out| out.name.as_str())
            .collect::<Vec<_>>(),
        [
            "chunk_batch_start",
            "level_chunk_with_light",
            "level_chunk_with_light",
            "chunk_batch_finished",
            "set_time",
            "chunk_batch_start",
            "level_chunk_with_light"
        ]
    );
    assert_eq!(names[3].data, [2]);
    let (client, backend) = s.client("chunk_batch_received", &hex("41200000")).unwrap();
    assert!(client.is_empty() && backend.is_empty());
    let (client, backend) = s.client("ping_request", &hex("0000000000000102")).unwrap();
    assert!(backend.is_empty());
    assert_eq!(
        client,
        [Out {
            state: State::Play,
            name: "pong_response".into(),
            data: hex("0000000000000102")
        }]
    );
    assert!(s.client("ping_request", &hex("00")).is_err());
}

#[test]
fn up_tolerates_acknowledgements_that_arrive_early() {
    // A replayed client may send everything before the backend leaves login.
    let mut s = Session::up();
    let mut out = Output::default();
    let translator = &mut s.translator;
    for (state, name, body) in [
        (
            State::Login,
            "hello",
            hex("06436f72707573888b6130823b387bb012d6dc24e47d59"),
        ),
        (State::Login, "login_acknowledged", vec![]),
        (
            State::Configuration,
            "client_information",
            hex(CLIENT_INFORMATION),
        ),
        (State::Configuration, "finish_configuration", vec![]),
    ] {
        translator
            .serverbound(
                Packet::new(id(NEWER, state, Direction::Serverbound, name), body),
                &mut out,
            )
            .unwrap();
    }
    assert_eq!(out.backend.len(), 1);
    for (name, body) in [
        ("login_finished", hex(LOGIN_FINISHED)),
        ("login", join_older(1, &older_codec())),
        ("update_tags", varint(0)),
    ] {
        let state = translator.backend_phase();
        translator
            .clientbound(
                Packet::new(id(OLDER, state, Direction::Clientbound, name), body),
                &mut out,
            )
            .unwrap();
    }
    let names: Vec<_> = out
        .client
        .iter()
        .map(|sent| {
            data::version(NEWER)
                .packet_name(sent.state, Direction::Clientbound, sent.packet.id)
                .unwrap()
        })
        .collect();
    assert_eq!(
        names,
        [
            "login_finished",
            "registry_data",
            "update_tags",
            "finish_configuration",
            "login"
        ]
    );
    assert_eq!(out.backend[1].state, State::Play);
}

#[test]
fn up_rejects_acknowledgements_out_of_place() {
    let mut up = Up::new_for_test();
    assert!(
        try_run(
            &mut up,
            Direction::Serverbound,
            State::Login,
            "login_acknowledged",
            &[1]
        )
        .is_err()
    );
    assert!(
        try_run(
            &mut up,
            Direction::Serverbound,
            State::Play,
            "configuration_acknowledged",
            &[]
        )
        .is_err()
    );
    let mut s = Session::up();
    up_joined(&mut s);
    s.client("login_acknowledged", &[]).unwrap();
    s.backend("update_tags", &varint(0)).unwrap();
    s.client("finish_configuration", &[]).unwrap();
    assert!(s.client("configuration_acknowledged", &[]).is_err());
}

#[test]
fn up_holds_a_bounded_amount() {
    let mut s = Session::up();
    s.client(
        "hello",
        &hex("06436f72707573888b6130823b387bb012d6dc24e47d59"),
    )
    .unwrap();
    s.backend("login_finished", &hex(LOGIN_FINISHED)).unwrap();
    let chunk = vec![0; 1 << 20];
    let mut failed = false;
    for _ in 0..80 {
        if s.backend("level_chunk_with_light", &chunk).is_err() {
            failed = true;
            break;
        }
    }
    assert!(
        failed,
        "a client that never acknowledges must not grow the queue forever"
    );
}

#[test]
fn up_converts_play_packets_that_changed() {
    let mut up = Up::new_for_test();
    // Spawn Player became Add Entity.
    let spawn = {
        let mut w = Writer::new();
        w.varint(5)
            .uuid(9)
            .f64(0.0)
            .f64(64.0)
            .f64(0.0)
            .i8(0x28)
            .i8(0);
        w.buf
    };
    let (out, _) = run(
        &mut up,
        Direction::Clientbound,
        State::Play,
        "add_player",
        &spawn,
    );
    assert_eq!(out[0].name, "add_entity");
    assert_eq!(layout::AddEntity::read(&out[0].data).unwrap().yaw, 0x28);
    let (out, _) = run(
        &mut up,
        Direction::Clientbound,
        State::Play,
        "set_display_objective",
        &hex("0106636f72707573"),
    );
    assert_eq!(out[0].data, hex("0106636f72707573"));
    let (out, _) = run(
        &mut up,
        Direction::Clientbound,
        State::Play,
        "update_enabled_features",
        &hex(FEATURES),
    );
    assert!(out.is_empty());
    let respawn = {
        let mut w = Writer::new();
        w.string("minecraft:overworld")
            .string("minecraft:overworld")
            .i64(0);
        w.u8(1)
            .i8(-1)
            .bool(false)
            .bool(true)
            .u8(1)
            .bool(false)
            .varint(0);
        w.buf
    };
    let (out, _) = run(
        &mut up,
        Direction::Clientbound,
        State::Play,
        "respawn",
        &respawn,
    );
    assert_eq!(out[0].data, layout::respawn(&respawn, true).unwrap());
    assert_eq!(&out[0].data[out[0].data.len() - 3..], [0, 0, 1]);
    for (name, body) in [("add_player", &spawn), ("respawn", &respawn)] {
        for end in 0..body.len() {
            assert!(
                try_run(
                    &mut up,
                    Direction::Clientbound,
                    State::Play,
                    name,
                    &body[..end]
                )
                .is_err()
            );
        }
    }
}

// The 1.20.1 client on a 1.20.2 backend.

fn down_configuring(s: &mut Session) {
    s.client(
        "hello",
        &hex("06436f7270757301888b6130823b387bb012d6dc24e47d59"),
    )
    .unwrap();
    let (client, backend) = s.backend("login_finished", &hex(LOGIN_FINISHED)).unwrap();
    assert_eq!(Session::names(&client), ["login_finished"]);
    assert_eq!(
        backend,
        [Out {
            state: State::Login,
            name: "login_acknowledged".into(),
            data: vec![]
        }]
    );
}

#[test]
fn down_runs_the_configuration_for_the_client() {
    let mut s = Session::down();
    down_configuring(&mut s);
    let pack = {
        let mut w = Writer::new();
        w.string("https://example.invalid/pack.zip")
            .string("")
            .bool(true)
            .bool(false);
        w.buf
    };
    let mut to_backend = Vec::new();
    for (name, body) in [
        ("custom_payload", hex(BRAND)),
        ("update_enabled_features", hex(FEATURES)),
        ("registry_data", codec().to_bytes(false)),
        ("update_tags", varint(0)),
        ("keep_alive", hex("0000000000000009")),
        ("ping", hex("00000003")),
        ("resource_pack_push", pack.clone()),
        ("finish_configuration", vec![]),
    ] {
        let (client, backend) = s.backend(name, &body).unwrap();
        assert!(
            client.is_empty(),
            "{name} reached a client that waits for Join Game"
        );
        to_backend.extend(backend);
    }
    assert_eq!(
        Session::names(&to_backend),
        [
            "keep_alive",
            "pong",
            "resource_pack",
            "resource_pack",
            "finish_configuration"
        ]
    );
    assert_eq!(to_backend[0].data, hex("0000000000000009"));
    assert_eq!(to_backend[1].data, hex("00000003"));
    assert_eq!(
        (to_backend[2].data.clone(), to_backend[3].data.clone()),
        (vec![3], vec![0])
    );
    assert!(
        to_backend
            .iter()
            .all(|packet| packet.state == State::Configuration)
    );
    let (client, _) = s.backend("login", &join(1).write_newer()).unwrap();
    assert_eq!(
        Session::names(&client),
        [
            "login",
            "update_enabled_features",
            "custom_payload",
            "update_tags",
            "resource_pack_push"
        ]
    );
    let (read, registries) = layout::Join::read_older(&client[0].data).unwrap();
    assert_eq!((read, registries), (join(1), older_codec()));
    assert_eq!(client[4].data, pack);
    // Play from here on, with what changed converted.
    let (_, backend) = s
        .client("client_information", &hex(CLIENT_INFORMATION))
        .unwrap();
    assert_eq!(backend[0].state, State::Play);
    let (client, backend) = s.backend("chunk_batch_start", &[]).unwrap();
    assert!(client.is_empty() && backend.is_empty());
    let (client, backend) = s.backend("chunk_batch_finished", &varint(9)).unwrap();
    assert!(client.is_empty());
    assert_eq!(
        backend,
        [Out {
            state: State::Play,
            name: "chunk_batch_received".into(),
            data: hex("42800000")
        }]
    );
    let (client, _) = s
        .backend("pong_response", &hex("0000000000000001"))
        .unwrap();
    assert!(client.is_empty());
}

#[test]
fn down_drops_client_packets_while_the_backend_configures() {
    let mut s = Session::down();
    down_configuring(&mut s);
    let (_, backend) = s
        .client("client_information", &hex(CLIENT_INFORMATION))
        .unwrap();
    assert_eq!(
        backend,
        [Out {
            state: State::Configuration,
            name: "client_information".into(),
            data: hex(CLIENT_INFORMATION)
        }]
    );
    let (_, backend) = s
        .client(
            "custom_payload",
            &hex("0f6d696e6563726166743a6272616e6409726966742d74657374"),
        )
        .unwrap();
    assert_eq!(backend[0].state, State::Configuration);
    for (name, body) in [
        ("keep_alive", hex("0000000000000001")),
        ("swing", varint(0)),
        ("pong", hex("00000001")),
    ] {
        let (_, backend) = s.client(name, &body).unwrap();
        assert!(backend.is_empty(), "{name}");
    }
}

#[test]
fn down_disconnects_during_configuration_in_play() {
    let reason = hex("0f7b2274657874223a22627965227d");
    let mut s = Session::down();
    down_configuring(&mut s);
    let (client, _) = s.backend("disconnect", &reason).unwrap();
    assert_eq!(
        client,
        [Out {
            state: State::Play,
            name: "disconnect".into(),
            data: reason
        }]
    );
}

#[test]
fn down_needs_registries_before_join_game() {
    let mut s = Session::down();
    down_configuring(&mut s);
    s.backend("finish_configuration", &[]).unwrap();
    assert!(s.backend("login", &join(1).write_newer()).is_err());
}

#[test]
fn down_follows_a_backend_back_into_configuration() {
    let mut s = Session::down();
    down_configuring(&mut s);
    s.backend("registry_data", &codec().to_bytes(false))
        .unwrap();
    s.backend("update_enabled_features", &hex(FEATURES))
        .unwrap();
    s.backend("finish_configuration", &[]).unwrap();
    let (client, _) = s.backend("login", &join(1).write_newer()).unwrap();
    assert_eq!(
        Session::names(&client),
        ["login", "update_enabled_features"]
    );
    let (client, backend) = s.backend("start_configuration", &[]).unwrap();
    assert!(client.is_empty());
    assert_eq!(
        backend,
        [Out {
            state: State::Play,
            name: "configuration_acknowledged".into(),
            data: vec![]
        }]
    );
    // The client is still in its old world and keeps moving.
    let movement = {
        let mut w = Writer::new();
        w.f64(0.5).f64(64.0).f64(0.5).bool(true);
        w.buf
    };
    let (_, backend) = s.client("move_player_pos", &movement).unwrap();
    assert!(backend.is_empty());
    let (_, backend) = s
        .client("client_information", &hex(CLIENT_INFORMATION))
        .unwrap();
    assert_eq!(backend[0].state, State::Configuration);
    s.backend("keep_alive", &hex("0000000000000002")).unwrap();
    s.backend("update_tags", &varint(0)).unwrap();
    s.backend("finish_configuration", &[]).unwrap();
    let (client, _) = s.backend("login", &join(2).write_newer()).unwrap();
    // The registries of the first configuration carry over; features are
    // only repeated when the backend repeats them.
    assert_eq!(Session::names(&client), ["login", "respawn", "update_tags"]);
    assert_eq!(
        layout::Join::read_older(&client[0].data)
            .unwrap()
            .0
            .entity_id,
        2
    );
    assert_eq!(client[1].data, join(2).respawn_older());
    assert!(s.backend("start_configuration", &[1]).is_err());
}

#[test]
fn down_converts_play_packets_that_changed() {
    let mut down = Down::new_for_test();
    // A player's Add Entity splits into Spawn Player, head rotation and
    // motion.
    let entity = layout::AddEntity {
        id: 5,
        uuid: 9,
        kind: layout::player_type(NEWER),
        position: [1.5, 64.0, -2.5],
        pitch: -10,
        yaw: 40,
        head_yaw: 50,
        data: 0,
        velocity: [0, 100, 0],
    };
    let (out, _) = run(
        &mut down,
        Direction::Clientbound,
        State::Play,
        "add_entity",
        &entity.write(),
    );
    assert_eq!(
        out.iter().map(|out| out.name.as_str()).collect::<Vec<_>>(),
        ["add_player", "rotate_head", "set_entity_motion"]
    );
    assert_eq!(out[0].data, entity.spawn_player());
    assert_eq!(out[1].data, [5, 50]);
    assert_eq!(out[2].data, [5, 0, 0, 0, 100, 0, 0]);
    // Anything else keeps its layout.
    let zombie = layout::AddEntity {
        kind: data::version(NEWER)
            .expect_registry("entity_type")
            .id("zombie")
            .unwrap(),
        velocity: [0; 3],
        ..entity.clone()
    };
    let (out, _) = run(
        &mut down,
        Direction::Clientbound,
        State::Play,
        "add_entity",
        &zombie.write(),
    );
    assert_eq!(out[0].data, zombie.write());
    // Limited crafting is not a 1.20.1 game event.
    let (out, _) = run(
        &mut down,
        Direction::Clientbound,
        State::Play,
        "game_event",
        &hex("0c3f800000"),
    );
    assert!(out.is_empty());
    let (out, _) = run(
        &mut down,
        Direction::Clientbound,
        State::Play,
        "game_event",
        &hex("0b3f800000"),
    );
    assert_eq!(out.len(), 1);
    let (out, _) = run(
        &mut down,
        Direction::Clientbound,
        State::Play,
        "set_display_objective",
        &hex("1306636f72707573"),
    );
    assert!(out.is_empty());
    let (out, _) = run(
        &mut down,
        Direction::Clientbound,
        State::Play,
        "tag_query",
        &hex("050a030001780000000700"),
    );
    assert_eq!(out[0].data, hex("050a0000030001780000000700"));
    let body = entity.write();
    for end in 0..body.len() {
        assert!(
            try_run(
                &mut down,
                Direction::Clientbound,
                State::Play,
                "add_entity",
                &body[..end]
            )
            .is_err()
        );
    }
    assert!(
        try_run(
            &mut down,
            Direction::Clientbound,
            State::Play,
            "chunk_batch_finished",
            &[]
        )
        .is_err()
    );
}

// Whole recorded sessions.

/// The recorded packets of one side and direction.
fn recorded(protocol: i32, direction: Direction) -> Option<Vec<Recorded>> {
    Some(
        corpus(protocol)?
            .into_iter()
            .filter(|packet| packet.direction == direction)
            .collect(),
    )
}

#[test]
fn up_carries_a_recorded_1_20_1_session_to_a_1_20_2_client() {
    let (Some(backend), Some(client), Some(native)) = (
        recorded(OLDER, Direction::Clientbound),
        recorded(NEWER, Direction::Serverbound),
        recorded(NEWER, Direction::Clientbound),
    ) else {
        eprintln!("corpus for {OLDER}/{NEWER} not present; skipping");
        return;
    };
    let mut s = Session::up();
    let hello = &client[0];
    assert_eq!(hello.name(NEWER), "hello");
    s.client("hello", &hello.data).unwrap();
    let client_configuration: Vec<_> = client
        .iter()
        .filter(|packet| {
            packet.state == State::Configuration && packet.name(NEWER) != "finish_configuration"
        })
        .collect();
    let mut chunks = 0;
    for packet in &backend {
        if packet.state == State::Play && packet.name(OLDER) == "level_chunk_with_light" {
            chunks += 1;
        }
        let (to_client, _) = s.backend_id(packet.id, &packet.data).unwrap();
        // Behave like the 1.20.2 client.
        for out in to_client {
            match out.name.as_str() {
                "login_finished" => {
                    s.client("login_acknowledged", &[]).unwrap();
                    for packet in &client_configuration {
                        s.client(packet.name(NEWER), &packet.data).unwrap();
                    }
                }
                "finish_configuration" => {
                    s.client("finish_configuration", &[]).unwrap();
                }
                "chunk_batch_finished" => {
                    let (client, backend) =
                        s.client("chunk_batch_received", &hex("41200000")).unwrap();
                    assert!(client.is_empty() && backend.is_empty());
                }
                _ => {}
            }
        }
    }
    let to_client = s.to_client.clone();
    let configuration: Vec<_> = to_client
        .iter()
        .filter(|packet| packet.state == State::Configuration)
        .collect();
    assert_eq!(
        configuration
            .iter()
            .map(|packet| packet.name.as_str())
            .collect::<Vec<_>>(),
        [
            "registry_data",
            "update_enabled_features",
            "custom_payload",
            "update_tags",
            "finish_configuration"
        ]
    );
    // The registries a 1.20.2 server sends, byte for byte.
    let native_registries = native
        .iter()
        .find(|packet| {
            packet.state == State::Configuration && packet.name(NEWER) == "registry_data"
        })
        .unwrap();
    assert_eq!(configuration[0].data, native_registries.data);
    let native_features = native
        .iter()
        .find(|packet| packet.name(NEWER) == "update_enabled_features")
        .unwrap();
    assert_eq!(configuration[1].data, native_features.data);
    // Join Game first in play, in the 1.20.2 layout with the same world.
    let play: Vec<_> = to_client
        .iter()
        .filter(|packet| packet.state == State::Play)
        .collect();
    assert_eq!(play[0].name, "login");
    let older_join = backend
        .iter()
        .find(|packet| packet.state == State::Play && packet.name(OLDER) == "login")
        .unwrap();
    let (expected, _) = layout::Join::read_older(&older_join.data).unwrap();
    assert_eq!(layout::Join::read_newer(&play[0].data).unwrap(), expected);
    let native_join = native
        .iter()
        .find(|packet| packet.state == State::Play && packet.name(NEWER) == "login")
        .unwrap();
    let native_join = layout::Join::read_newer(&native_join.data).unwrap();
    let sorted = |worlds: &[String]| {
        let mut worlds = worlds.to_vec();
        worlds.sort();
        worlds
    };
    assert_eq!(
        (
            sorted(&native_join.worlds),
            native_join.dimension.clone(),
            native_join.limited_crafting
        ),
        (sorted(&expected.worlds), expected.dimension.clone(), false)
    );
    // Every chunk inside a batch, every batch closed and counted.
    let mut open = None;
    let mut counted = 0;
    for packet in &play {
        match packet.name.as_str() {
            "chunk_batch_start" => {
                assert!(open.is_none());
                open = Some(0);
            }
            "level_chunk_with_light" => *open.as_mut().expect("chunk outside a batch") += 1,
            "chunk_batch_finished" => {
                let count = Reader::new(&packet.data).varint().unwrap();
                assert_eq!(Some(count), open.take());
                counted += count;
            }
            _ => assert!(open.is_none(), "{} inside a chunk batch", packet.name),
        }
    }
    assert_eq!(counted, chunks);
    // Nothing the 1.20.1 server sent was lost, apart from what 1.20.2 has
    // no play form for.
    let expected = backend
        .iter()
        .filter(|packet| packet.state == State::Play)
        .count();
    let produced = to_client
        .iter()
        .filter(|packet| {
            !packet.name.starts_with("chunk_batch")
                && packet.name != "registry_data"
                && packet.name != "finish_configuration"
        })
        .filter(|packet| packet.state != State::Login)
        .count();
    assert_eq!(produced, expected);
    // The backend heard only play packets after login.
    assert!(
        s.to_backend
            .iter()
            .all(|packet| packet.state != State::Configuration)
    );
    assert_eq!(s.to_backend[0].data, corpus(OLDER).unwrap()[0].data);
}

#[test]
fn down_carries_a_recorded_1_20_2_session_to_a_1_20_1_client() {
    let (Some(backend), Some(client), Some(native)) = (
        recorded(NEWER, Direction::Clientbound),
        recorded(OLDER, Direction::Serverbound),
        recorded(OLDER, Direction::Clientbound),
    ) else {
        eprintln!("corpus for {OLDER}/{NEWER} not present; skipping");
        return;
    };
    let mut s = Session::down();
    s.client("hello", &client[0].data).unwrap();
    assert_eq!(s.to_backend[0].data, corpus(NEWER).unwrap()[0].data);
    let mut batches = 0;
    for packet in &backend {
        if packet.name(NEWER) == "chunk_batch_finished" && packet.state == State::Play {
            batches += 1;
        }
        let (to_client, _) = s.backend_id(packet.id, &packet.data).unwrap();
        for out in to_client {
            if out.name == "login" {
                for packet in client.iter().filter(|packet| packet.state == State::Play) {
                    if packet.name(OLDER) != "accept_teleportation" {
                        s.client(packet.name(OLDER), &packet.data).unwrap();
                    }
                }
            }
        }
    }
    let to_client = s.to_client.clone();
    assert_eq!(
        to_client
            .iter()
            .take(6)
            .map(|packet| packet.name.as_str())
            .collect::<Vec<_>>(),
        [
            "login_compression",
            "login_finished",
            "login",
            "update_enabled_features",
            "custom_payload",
            "update_tags"
        ]
    );
    // The registries a 1.20.1 server sends, byte for byte.
    let native_join = native
        .iter()
        .find(|packet| packet.state == State::Play && packet.name(OLDER) == "login")
        .unwrap();
    let (native_join, native_codec) = layout::Join::read_older(&native_join.data).unwrap();
    let (join, codec) = layout::Join::read_older(&to_client[2].data).unwrap();
    assert_eq!(codec.to_bytes(true), native_codec.to_bytes(true));
    let (mut worlds, mut native_worlds) = (join.worlds, native_join.worlds);
    worlds.sort();
    native_worlds.sort();
    assert_eq!(
        (worlds, join.dimension_type),
        (native_worlds, native_join.dimension_type)
    );
    let native_features = native
        .iter()
        .find(|packet| packet.name(OLDER) == "update_enabled_features")
        .unwrap();
    assert_eq!(to_client[3].data, native_features.data);
    assert_eq!(to_client[4].data, hex(BRAND));
    // Configuration answered, then play; every batch acknowledged.
    let names = Session::names(&s.to_backend);
    assert_eq!(&names[..2], ["hello", "login_acknowledged"]);
    assert!(names.contains(&"finish_configuration"));
    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "chunk_batch_received")
            .count(),
        batches
    );
    assert!(
        !to_client
            .iter()
            .any(|packet| packet.name.starts_with("chunk_batch"))
    );
}
