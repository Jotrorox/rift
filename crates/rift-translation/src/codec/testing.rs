//! A step made of nothing but the shared codecs' defaults, for codec tests:
//! feed it a packet by name and look at what comes out.
use super::{HasShared, Shared};
use crate::{
    Direction, Packet, State, data,
    step::{Cx, Emitted, RouteBuilder, Routes},
};
use std::{
    collections::{HashMap, VecDeque},
    io,
    sync::{Mutex, OnceLock},
};

pub struct Defaults {
    shared: Shared,
    routes: &'static Routes<Defaults>,
}

impl HasShared for Defaults {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// A packet a handler emitted, with its name in the protocol it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Out {
    pub direction: Direction,
    pub state: State,
    pub name: String,
    pub data: Vec<u8>,
}

impl Defaults {
    /// The default routes of the step between the adjacent protocols
    /// `client` and `backend`.
    pub fn new(client: i32, backend: i32) -> Self {
        type Built = HashMap<(i32, i32), &'static Routes<Defaults>>;
        static ROUTES: OnceLock<Mutex<Built>> = OnceLock::new();
        let routes = *ROUTES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .entry((client, backend))
            .or_insert_with(|| {
                Box::leak(Box::new(
                    RouteBuilder::new(client, backend).defaults().build(),
                ))
            });
        Self {
            shared: Shared::new(client, backend),
            routes,
        }
    }

    /// Send the packet `name` (of the sender's protocol) through the step.
    pub fn send(
        &mut self,
        direction: Direction,
        state: State,
        name: &str,
        body: &[u8],
    ) -> io::Result<Vec<Out>> {
        let source = data::version(self.shared.source(direction));
        let id = source.expect_packet(state, direction, name);
        self.send_id(direction, state, id, body)
    }

    /// Send a packet by its ID in the sender's protocol, as corpus lines have it.
    pub fn send_id(
        &mut self,
        direction: Direction,
        state: State,
        id: i32,
        body: &[u8],
    ) -> io::Result<Vec<Out>> {
        let (client, backend) = (self.shared.client, self.shared.backend);
        let mut emitted = VecDeque::new();
        let mut cx = Cx {
            emitted: &mut emitted,
            client_protocol: client,
            backend_protocol: backend,
        };
        let routes = self.routes;
        routes.dispatch(
            self,
            &mut cx,
            direction,
            state,
            Packet::new(id, body.to_vec()),
        )?;
        Ok(emitted
            .into_iter()
            .map(
                |Emitted {
                     direction,
                     state,
                     packet,
                 }| {
                    let protocol = match direction {
                        Direction::Clientbound => client,
                        Direction::Serverbound => backend,
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

    /// The backend packet `name` on its way to the client.
    pub fn clientbound(&mut self, state: State, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
        self.send(Direction::Clientbound, state, name, body)
    }

    /// The client packet `name` on its way to the backend.
    pub fn serverbound(&mut self, state: State, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
        self.send(Direction::Serverbound, state, name, body)
    }
}

/// One recorded packet of the real-traffic corpus.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub direction: Direction,
    pub state: State,
    pub id: i32,
    pub data: Vec<u8>,
}

impl Recorded {
    /// The packet's name in `protocol`, the one it was recorded in.
    pub fn name(&self, protocol: i32) -> &'static str {
        data::version(protocol)
            .packet_name(self.state, self.direction, self.id)
            .unwrap_or("?")
    }
}

/// The session `tests/translation_corpus.py` recorded from the official
/// `protocol` server, or `None` when it has not been captured on this
/// machine (tests then skip). Looked up in `$RIFT_TRANSLATION_CORPUS` or
/// `~/.cache/rift-via/corpus`.
pub fn corpus(protocol: i32) -> Option<Vec<Recorded>> {
    let directory = std::env::var_os("RIFT_TRANSLATION_CORPUS")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join(".cache/rift-via/corpus"))
        })?;
    corpus_in(&directory, protocol)
}

/// A session recorded in the corpus format in another directory, such as
/// a codec's private corpus; `None` when it is absent.
pub fn corpus_in(directory: &std::path::Path, protocol: i32) -> Option<Vec<Recorded>> {
    let text = std::fs::read_to_string(directory.join(format!("{protocol}.jsonl"))).ok()?;
    let mut packets = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line).expect("corpus line is JSON");
        let hex = value["hex"].as_str().expect("corpus line has hex");
        let data = (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).expect("corpus hex"))
            .collect();
        packets.push(Recorded {
            direction: match value["d"].as_str() {
                Some("c") => Direction::Clientbound,
                _ => Direction::Serverbound,
            },
            state: match value["s"].as_str() {
                Some("login") => State::Login,
                Some("configuration") => State::Configuration,
                _ => State::Play,
            },
            id: value["id"].as_i64().expect("corpus line has an id") as i32,
            data,
        });
    }
    Some(packets)
}
