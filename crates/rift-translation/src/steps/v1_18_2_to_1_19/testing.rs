//! Running one of the era's steps on single packets, for tests.
use crate::{
    Direction, Packet, State,
    codec::testing::{Recorded, corpus},
    data,
    step::{Cx, Emitted, Step},
    steps::Make,
};
use std::{collections::VecDeque, io};

/// A packet a step emitted, with its name in the protocol it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Out {
    pub direction: Direction,
    pub state: State,
    pub name: String,
    pub data: Vec<u8>,
}

pub struct Harness {
    step: Box<dyn Step>,
    pub client: i32,
    pub backend: i32,
}

impl Harness {
    pub fn new(make: Make, client: i32, backend: i32) -> Self {
        Self {
            step: make(),
            client,
            backend,
        }
    }

    /// Send the packet `name` of the sender's protocol through the step.
    pub fn send(
        &mut self,
        direction: Direction,
        state: State,
        name: &str,
        data: &[u8],
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
        let packet = Packet::new(id, data.to_vec());
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
                    dump(protocol, direction, state, &packet, name);
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

    pub fn clientbound(&mut self, state: State, name: &str, data: &[u8]) -> io::Result<Vec<Out>> {
        self.send(Direction::Clientbound, state, name, data)
    }

    pub fn serverbound(&mut self, state: State, name: &str, data: &[u8]) -> io::Result<Vec<Out>> {
        self.send(Direction::Serverbound, state, name, data)
    }

    /// Send a packet that must produce exactly one, and return it.
    pub fn one(&mut self, direction: Direction, state: State, name: &str, data: &[u8]) -> Out {
        let mut out = self.send(direction, state, name, data).unwrap();
        assert_eq!(out.len(), 1, "{name} produced {out:?}");
        out.remove(0)
    }
}

/// With `RIFT_SIGNING_ERA_DUMP` set to a file, every packet a step produced
/// in a test is appended to it as a JSON line, for
/// `tests/translation_signing_era.py` to decode with minecraft-data schemas.
fn dump(protocol: i32, direction: Direction, state: State, packet: &Packet, source: &str) {
    static FILE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let Some(path) = std::env::var_os("RIFT_SIGNING_ERA_DUMP") else {
        return;
    };
    let line = serde_json::json!({
        "protocol": protocol,
        "d": if direction == Direction::Clientbound { "c" } else { "s" },
        "s": match state {
            State::Login => "login",
            State::Configuration => "configuration",
            _ => "play",
        },
        "id": packet.id,
        "name": data::version(protocol).packet_name(state, direction, packet.id),
        "from": source,
        "hex": packet.data.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
    });
    let _guard = FILE.lock().unwrap();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("dump file");
    writeln!(file, "{line}").expect("dump line");
}

/// The bodies of every recorded `name` packet of `protocol`'s session, or
/// `None` (with a note) when the corpus is not on this machine.
pub fn recorded(
    protocol: i32,
    direction: Direction,
    state: State,
    name: &str,
) -> Option<Vec<Vec<u8>>> {
    let Some(session) = corpus(protocol) else {
        eprintln!("no corpus for protocol {protocol}: skipped");
        return None;
    };
    Some(
        session
            .into_iter()
            .filter(|packet: &Recorded| {
                packet.direction == direction
                    && packet.state == state
                    && packet.name(protocol) == name
            })
            .map(|packet| packet.data)
            .collect(),
    )
}

/// The first recorded `name` packet, which the corpus must contain.
pub fn first(protocol: i32, direction: Direction, state: State, name: &str) -> Option<Vec<u8>> {
    let packets = recorded(protocol, direction, state, name)?;
    Some(
        packets
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("the {protocol} corpus has no {name}")),
    )
}

pub fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

/// Every truncation of a valid packet fails cleanly instead of panicking
/// or passing something short on.
pub fn truncations_fail(
    make: Make,
    client: i32,
    backend: i32,
    direction: Direction,
    state: State,
    name: &str,
    data: &[u8],
) {
    for length in 0..data.len() {
        let mut harness = Harness::new(make, client, backend);
        let result = harness.send(direction, state, name, &data[..length]);
        assert!(
            result.is_err(),
            "{name} truncated to {length} bytes passed: {result:?}"
        );
    }
}

/// The registries of a 1.16–1.20.1 `login` body.
pub fn codec(protocol: i32, login: &[u8]) -> Vec<crate::codec::registries::Registry> {
    crate::codec::registries::login::read_login(protocol, &mut crate::wire::Reader::new(login))
        .unwrap()
        .codec
        .unwrap()
}

/// Two sets of registries hold the same entries with the same elements,
/// whatever their order and IDs (converted registries keep the sender's
/// order, so that its IDs keep their meaning).
pub fn assert_same_registries(
    actual: &[crate::codec::registries::Registry],
    expected: &[crate::codec::registries::Registry],
) {
    let names = |registries: &[crate::codec::registries::Registry]| {
        let mut names: Vec<String> = registries.iter().map(|r| r.name.clone()).collect();
        names.sort();
        names
    };
    assert_eq!(names(actual), names(expected));
    for expected in expected {
        let actual = actual.iter().find(|r| r.name == expected.name).unwrap();
        assert_eq!(
            actual.entries.len(),
            expected.entries.len(),
            "{}",
            expected.name
        );
        for entry in &expected.entries {
            assert_eq!(
                actual.entry(&entry.name).map(|e| &e.element),
                Some(&entry.element),
                "{} {}",
                expected.name,
                entry.name
            );
        }
    }
}
