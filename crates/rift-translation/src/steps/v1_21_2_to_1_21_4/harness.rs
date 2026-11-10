//! Drive a step by packet name in tests, and look at what comes out by name.
//! Shared by the steps from 1.21.2 to 1.21.6.
use crate::{
    Direction, Packet, State,
    codec::testing::{Recorded, corpus},
    data,
    step::{Cx, Emitted, Step},
};
use std::{collections::VecDeque, io};

/// A packet a step emitted, named in the protocol it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Out {
    pub direction: Direction,
    pub state: State,
    pub name: String,
    pub data: Vec<u8>,
}

/// One step of one connection between `client` and `backend`.
pub struct Harness {
    pub step: Box<dyn Step>,
    pub client: i32,
    pub backend: i32,
}

impl Harness {
    pub fn new(step: Box<dyn Step>, client: i32, backend: i32) -> Self {
        Self {
            step,
            client,
            backend,
        }
    }

    /// The protocol packets travelling in `direction` arrive in.
    pub fn source(&self, direction: Direction) -> i32 {
        match direction {
            Direction::Clientbound => self.backend,
            Direction::Serverbound => self.client,
        }
    }

    /// Send the packet `name` of the sender's protocol through the step.
    pub fn send(
        &mut self,
        direction: Direction,
        state: State,
        name: &str,
        body: &[u8],
    ) -> io::Result<Vec<Out>> {
        let id = data::version(self.source(direction)).expect_packet(state, direction, name);
        self.send_id(direction, state, id, body)
    }

    pub fn send_id(
        &mut self,
        direction: Direction,
        state: State,
        id: i32,
        body: &[u8],
    ) -> io::Result<Vec<Out>> {
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

    pub fn clientbound(&mut self, state: State, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
        self.send(Direction::Clientbound, state, name, body)
    }

    pub fn serverbound(&mut self, state: State, name: &str, body: &[u8]) -> io::Result<Vec<Out>> {
        self.send(Direction::Serverbound, state, name, body)
    }

    /// Send one recorded packet, which must be of the sender's protocol.
    pub fn replay(&mut self, recorded: &Recorded) -> io::Result<Vec<Out>> {
        self.send_id(
            recorded.direction,
            recorded.state,
            recorded.id,
            &recorded.data,
        )
    }
}

pub fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex"))
        .collect()
}

/// The recorded packets of `protocol` named `name`, or `None` (and a note)
/// when the corpus is not on this machine.
pub fn recorded(
    protocol: i32,
    direction: Direction,
    state: State,
    name: &str,
) -> Option<Vec<Recorded>> {
    let Some(packets) = corpus(protocol) else {
        eprintln!("no corpus for protocol {protocol}: skipped");
        return None;
    };
    Some(
        packets
            .into_iter()
            .filter(|packet| {
                packet.direction == direction
                    && packet.state == state
                    && packet.name(protocol) == name
            })
            .collect(),
    )
}

/// One emitted packet named `name`, which must be the only one.
pub fn single<'a>(out: &'a [Out], name: &str) -> &'a [u8] {
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0].name, name, "{out:?}");
    &out[0].data
}
