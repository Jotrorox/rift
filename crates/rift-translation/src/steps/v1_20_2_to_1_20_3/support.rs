//! Test helpers for the steps from 1.20.2 to 1.20.5: one step instance fed
//! packets by name or by recorded ID, and hex for inline test vectors.
use crate::{
    Direction, Packet, State,
    codec::testing::Out,
    data,
    step::{Cx, Emitted, Step},
    steps::Make,
};
use std::{collections::VecDeque, io};

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

    fn source(&self, direction: Direction) -> i32 {
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
}

pub fn hex(text: &str) -> Vec<u8> {
    let text: String = text.split_whitespace().collect();
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("test hex"))
        .collect()
}

/// The names and bodies of what a step emitted, for compact assertions.
pub fn names(out: &[Out]) -> Vec<&str> {
    out.iter().map(|out| out.name.as_str()).collect()
}

/// Every prefix of `body` but the whole: a strict handler must reject each
/// without panicking.
pub fn truncations(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    (0..body.len()).map(|length| &body[..length])
}
