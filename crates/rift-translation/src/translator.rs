//! The chain of steps between a client's and a backend's protocol.
use crate::{
    Direction, Packet, State,
    data::{self, VersionData},
    step::{Cx, Emitted, Step},
    steps, version,
};
use std::{collections::VecDeque, io};

/// A translated packet and the protocol phase it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub state: State,
    pub packet: Packet,
}

/// Packets a translation call produced, in sending order.
#[derive(Debug, Default)]
pub struct Output {
    /// For the client, in the client's protocol.
    pub client: Vec<Sent>,
    /// For the backend, in the backend's protocol.
    pub backend: Vec<Sent>,
}

impl Output {
    pub fn is_empty(&self) -> bool {
        self.client.is_empty() && self.backend.is_empty()
    }
}

/// The protocol phases of one end of the chain, followed through the control
/// packets that change them.
#[derive(Clone, Copy, Debug)]
struct Edge {
    data: &'static VersionData,
    clientbound: State,
    serverbound: State,
}

impl Edge {
    fn new(protocol: i32) -> Self {
        Self {
            data: data::version(protocol),
            clientbound: State::Login,
            serverbound: State::Login,
        }
    }

    fn observe(&mut self, direction: Direction, state: State, id: i32) {
        let Some(name) = self.data.packet_name(state, direction, id) else {
            return;
        };
        use Direction::*;
        use State::*;
        match (direction, state, name) {
            (Clientbound, Login, "login_finished" | "game_profile") => {
                if version::has_configuration(self.data.protocol) {
                    self.clientbound = Configuration;
                } else {
                    self.clientbound = Play;
                    self.serverbound = Play;
                }
            }
            (Serverbound, Login, "login_acknowledged") => self.serverbound = Configuration,
            (Clientbound, Configuration, "finish_configuration") => self.clientbound = Play,
            (Serverbound, Configuration, "finish_configuration") => self.serverbound = Play,
            (Clientbound, Play, "start_configuration") => self.clientbound = Configuration,
            (Serverbound, Play, "configuration_acknowledged") => self.serverbound = Configuration,
            _ => {}
        }
    }
}

/// Whether a client on `client` can play on a backend on `backend`.
pub fn supported(client: i32, backend: i32) -> bool {
    client == backend || steps::chain(client, backend).is_some()
}

/// Translates one client/backend connection pair. Created when the backend
/// connection is, in the login phase on both sides: the caller sends the
/// handshake itself, naming the backend's protocol.
pub struct Translator {
    client_protocol: i32,
    backend_protocol: i32,
    /// Nearest the client first.
    steps: Vec<Box<dyn Step>>,
    client: Edge,
    backend: Edge,
}

struct Item {
    /// The step to run next: -1 leaves towards the client, `steps.len()`
    /// towards the backend.
    position: isize,
    emitted: Emitted,
}

impl Translator {
    /// `None` when both sides already speak the same protocol.
    pub fn new(client: i32, backend: i32) -> io::Result<Option<Self>> {
        if client == backend {
            return Ok(None);
        }
        let steps = steps::chain(client, backend).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Rift cannot translate between Minecraft {} and {}.",
                    version::release(client),
                    version::release(backend)
                ),
            )
        })?;
        Ok(Some(Self {
            client_protocol: client,
            backend_protocol: backend,
            steps,
            client: Edge::new(client),
            backend: Edge::new(backend),
        }))
    }

    pub fn client_protocol(&self) -> i32 {
        self.client_protocol
    }

    pub fn backend_protocol(&self) -> i32 {
        self.backend_protocol
    }

    /// The phase the backend's next packet belongs to, in its protocol.
    pub fn backend_phase(&self) -> State {
        self.backend.clientbound
    }

    /// The phase the client's next packet belongs to, in its protocol.
    pub fn client_phase(&self) -> State {
        self.client.serverbound
    }

    /// Translate a packet received from the backend.
    pub fn clientbound(&mut self, packet: Packet, out: &mut Output) -> io::Result<()> {
        let state = self.backend.clientbound;
        self.backend
            .observe(Direction::Clientbound, state, packet.id);
        self.run(
            Item {
                position: self.steps.len() as isize - 1,
                emitted: Emitted {
                    direction: Direction::Clientbound,
                    state,
                    packet,
                },
            },
            out,
        )
    }

    /// Translate a packet received from the client.
    pub fn serverbound(&mut self, packet: Packet, out: &mut Output) -> io::Result<()> {
        let state = self.client.serverbound;
        self.client
            .observe(Direction::Serverbound, state, packet.id);
        self.run(
            Item {
                position: 0,
                emitted: Emitted {
                    direction: Direction::Serverbound,
                    state,
                    packet,
                },
            },
            out,
        )
    }

    /// Run one packet through the chain. Steps emit in order and the queue is
    /// first in, first out, so packets keep their order at every step and in
    /// both outputs.
    fn run(&mut self, first: Item, out: &mut Output) -> io::Result<()> {
        let mut queue = VecDeque::from([first]);
        let mut emitted = VecDeque::new();
        while let Some(Item {
            position,
            emitted: item,
        }) = queue.pop_front()
        {
            let Emitted {
                direction,
                state,
                packet,
            } = item;
            if position < 0 {
                self.client.observe(direction, state, packet.id);
                out.client.push(Sent { state, packet });
                continue;
            }
            if position as usize >= self.steps.len() {
                self.backend.observe(direction, state, packet.id);
                out.backend.push(Sent { state, packet });
                continue;
            }
            let mut cx = Cx {
                emitted: &mut emitted,
                client_protocol: self.client_protocol,
                backend_protocol: self.backend_protocol,
            };
            let step = &mut self.steps[position as usize];
            match direction {
                Direction::Clientbound => step.clientbound(&mut cx, state, packet)?,
                Direction::Serverbound => step.serverbound(&mut cx, state, packet)?,
            }
            queue.extend(emitted.drain(..).map(|emitted| Item {
                position: match emitted.direction {
                    Direction::Clientbound => position - 1,
                    Direction::Serverbound => position + 1,
                },
                emitted,
            }));
        }
        Ok(())
    }
}
