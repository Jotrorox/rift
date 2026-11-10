//! Test helpers for the steps from 1.12 to 1.13.2: a translator between two
//! protocols that is already in the play phase, fed packets by name.
use crate::{Direction, Output, Packet, State, Translator, data};
use std::io;

/// A packet the translator sent on, with its name in the receiver's protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Out {
    pub direction: Direction,
    pub state: State,
    pub name: String,
    pub data: Vec<u8>,
}

pub struct Session {
    translator: Translator,
}

impl Session {
    /// A `client` player on a `backend` server, past login. Neither protocol
    /// has a configuration phase, so login success moves both to play.
    pub fn play(client: i32, backend: i32) -> Self {
        let mut translator = Translator::new(client, backend).unwrap().unwrap();
        let id = data::version(backend).expect_packet(
            State::Login,
            Direction::Clientbound,
            "login_finished",
        );
        translator
            .clientbound(Packet::empty(id), &mut Output::default())
            .unwrap();
        assert_eq!(translator.backend_phase(), State::Play);
        assert_eq!(translator.client_phase(), State::Play);
        Self { translator }
    }

    /// The backend's play packet `name` (in its protocol).
    pub fn clientbound(&mut self, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
        let backend = self.translator.backend_protocol();
        let id = data::version(backend).expect_packet(State::Play, Direction::Clientbound, name);
        self.clientbound_id(id, body)
    }

    /// The client's play packet `name` (in its protocol).
    pub fn serverbound(&mut self, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
        let client = self.translator.client_protocol();
        let id = data::version(client).expect_packet(State::Play, Direction::Serverbound, name);
        self.serverbound_id(id, body)
    }

    pub fn clientbound_id(&mut self, id: i32, body: &[u8]) -> io::Result<Vec<Out>> {
        let mut out = Output::default();
        self.translator
            .clientbound(Packet::new(id, body.to_vec()), &mut out)?;
        Ok(self.named(out))
    }

    pub fn serverbound_id(&mut self, id: i32, body: &[u8]) -> io::Result<Vec<Out>> {
        let mut out = Output::default();
        self.translator
            .serverbound(Packet::new(id, body.to_vec()), &mut out)?;
        Ok(self.named(out))
    }

    fn named(&self, out: Output) -> Vec<Out> {
        let sides = [
            (
                Direction::Clientbound,
                self.translator.client_protocol(),
                out.client,
            ),
            (
                Direction::Serverbound,
                self.translator.backend_protocol(),
                out.backend,
            ),
        ];
        sides
            .into_iter()
            .flat_map(|(direction, protocol, sent)| {
                sent.into_iter().map(move |sent| Out {
                    direction,
                    state: sent.state,
                    name: data::version(protocol)
                        .packet_name(sent.state, direction, sent.packet.id)
                        .unwrap_or("?")
                        .to_owned(),
                    data: sent.packet.data,
                })
            })
            .collect()
    }
}

/// A packet `name` sent on to the client with `body`.
pub fn to_client(name: &str, body: &[u8]) -> Out {
    Out {
        direction: Direction::Clientbound,
        state: State::Play,
        name: name.to_owned(),
        data: body.to_vec(),
    }
}

/// A packet `name` sent on to the backend with `body`.
pub fn to_backend(name: &str, body: &[u8]) -> Out {
    Out {
        direction: Direction::Serverbound,
        state: State::Play,
        name: name.to_owned(),
        data: body.to_vec(),
    }
}

/// The bodies of every play packet `name` going in `direction` that the
/// official `protocol` server's recorded session holds, or `None` when the
/// corpus is not on this machine (the caller then skips, with a note).
pub fn recorded(protocol: i32, direction: Direction, name: &str) -> Option<Vec<Vec<u8>>> {
    let Some(session) = crate::codec::testing::corpus(protocol) else {
        eprintln!("no corpus for protocol {protocol}; skipping");
        return None;
    };
    Some(
        session
            .into_iter()
            .filter(|packet| {
                packet.direction == direction
                    && packet.state == State::Play
                    && packet.name(protocol) == name
            })
            .map(|packet| packet.data)
            .collect(),
    )
}

pub fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}
