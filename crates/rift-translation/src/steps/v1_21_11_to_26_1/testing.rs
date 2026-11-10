//! Drive the 26.x steps by packet name in tests and look at what comes out.
use crate::{
    Direction, Packet, State, data,
    step::{Cx, Step},
    steps::Make,
};
use std::{collections::VecDeque, io};

/// One step instance between `client` and `backend`.
pub(in crate::steps) struct Harness {
    step: Box<dyn Step>,
    client: i32,
    backend: i32,
}

/// A packet the step sent, named in the protocol it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::steps) struct Sent {
    pub direction: Direction,
    pub state: State,
    pub name: String,
    pub data: Vec<u8>,
}

impl Harness {
    pub fn new(make: Option<Make>, client: i32, backend: i32) -> Self {
        Self {
            step: make.expect("the step exists")(),
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
        body: &[u8],
    ) -> io::Result<Vec<Sent>> {
        let source = match direction {
            Direction::Clientbound => self.backend,
            Direction::Serverbound => self.client,
        };
        let id = data::version(source).expect_packet(state, direction, name);
        self.send_id(direction, state, id, body)
    }

    pub fn send_id(
        &mut self,
        direction: Direction,
        state: State,
        id: i32,
        body: &[u8],
    ) -> io::Result<Vec<Sent>> {
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
            .map(|emitted| {
                let protocol = match emitted.direction {
                    Direction::Clientbound => self.client,
                    Direction::Serverbound => self.backend,
                };
                Sent {
                    direction: emitted.direction,
                    state: emitted.state,
                    name: data::version(protocol)
                        .packet_name(emitted.state, emitted.direction, emitted.packet.id)
                        .unwrap_or("?")
                        .to_owned(),
                    data: emitted.packet.data,
                }
            })
            .collect())
    }

    pub fn clientbound(&mut self, name: &str, body: &[u8]) -> io::Result<Vec<Sent>> {
        self.send(Direction::Clientbound, State::Play, name, body)
    }

    pub fn serverbound(&mut self, name: &str, body: &[u8]) -> io::Result<Vec<Sent>> {
        self.send(Direction::Serverbound, State::Play, name, body)
    }

    /// The one packet a play packet turns into, by name and body.
    pub fn one(&mut self, direction: Direction, name: &str, body: &[u8]) -> (String, Vec<u8>) {
        let sent = self
            .send(direction, State::Play, name, body)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(sent.len(), 1, "{name} became {sent:?}");
        let Sent { name, data, .. } = sent.into_iter().next().unwrap();
        (name, data)
    }
}

/// A recorded packet's name and body and what the step made of it.
pub(in crate::steps) type Converted = (String, Vec<u8>, Vec<Sent>);

pub(in crate::steps) fn hex(text: &str) -> Vec<u8> {
    let text: String = text.split_whitespace().collect();
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

/// Feed every packet of the `sender` corpus travelling in `direction`
/// through a fresh step (in recording order, phases as recorded) and
/// return what came out of each packet named in `names`, with its input.
/// `None` when the corpus has not been captured on this machine.
pub(in crate::steps) fn corpus_through(
    make: Option<Make>,
    client: i32,
    backend: i32,
    direction: Direction,
    names: &[&str],
) -> Option<Vec<Converted>> {
    let sender = match direction {
        Direction::Clientbound => backend,
        Direction::Serverbound => client,
    };
    let recorded = crate::codec::testing::corpus(sender)?;
    let mut harness = Harness::new(make, client, backend);
    let mut out = Vec::new();
    for packet in recorded
        .iter()
        .filter(|packet| packet.direction == direction)
    {
        let name = packet.name(sender);
        let sent = harness
            .send_id(direction, packet.state, packet.id, &packet.data)
            .unwrap_or_else(|error| panic!("{sender} {name}: {error}"));
        if names.contains(&name) {
            out.push((name.to_owned(), packet.data.clone(), sent));
        }
    }
    Some(out)
}
