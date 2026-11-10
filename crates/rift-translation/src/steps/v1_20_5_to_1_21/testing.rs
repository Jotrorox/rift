//! Drive one step by packet name in tests and look at what comes out.
use crate::{
    Direction, Packet, State, data,
    step::{Cx, Emitted, Step},
    steps::Make,
};
use std::{collections::VecDeque, io};

/// A packet the step emitted, named in the protocol it is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Out {
    pub direction: Direction,
    pub state: State,
    pub name: String,
    pub data: Vec<u8>,
}

pub struct Harness {
    pub step: Box<dyn Step>,
    pub client: i32,
    pub backend: i32,
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
    ) -> io::Result<Vec<Out>> {
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

    /// The one packet a clientbound packet became.
    pub fn one_clientbound(&mut self, state: State, name: &str, body: &[u8]) -> Out {
        let mut out = self.clientbound(state, name, body).unwrap();
        assert_eq!(out.len(), 1, "{name} became {out:?}");
        out.pop().unwrap()
    }

    /// The one packet a serverbound packet became.
    pub fn one_serverbound(&mut self, state: State, name: &str, body: &[u8]) -> Out {
        let mut out = self.serverbound(state, name, body).unwrap();
        assert_eq!(out.len(), 1, "{name} became {out:?}");
        out.pop().unwrap()
    }
}

pub fn hex(text: &str) -> Vec<u8> {
    let text: String = text.split_whitespace().collect();
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

/// The recorded session of `protocol`, or `None` (the test then skips).
pub fn corpus(protocol: i32) -> Option<Vec<crate::codec::testing::Recorded>> {
    let recorded = crate::codec::testing::corpus(protocol);
    if recorded.is_none() {
        eprintln!("no corpus for protocol {protocol}; skipping");
    }
    recorded
}
