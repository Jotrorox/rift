//! Test helpers for the 1.19.3 to 1.20 steps: send packets through a step by
//! name, and pick packets and registry codecs out of the real-traffic corpus.
use super::play;
use crate::{
    Direction, Packet, State,
    codec::testing,
    nbt::{Compound, Nbt},
    step::{Cx, Routes},
};
use std::{collections::VecDeque, io};

/// A packet a step emitted, named in the protocol it is in.
#[derive(Debug)]
pub struct Out {
    pub direction: Direction,
    pub state: State,
    pub name: &'static str,
    pub data: Vec<u8>,
}

/// Send the packet `name` of the sender's protocol through `step`.
pub fn send<S>(
    routes: &'static Routes<S>,
    step: &mut S,
    direction: Direction,
    name: &str,
    body: &[u8],
) -> io::Result<Vec<Out>> {
    let state = State::Play;
    let (source, client, backend) = match direction {
        Direction::Clientbound => (routes.backend(), routes.client(), routes.backend()),
        Direction::Serverbound => (routes.client(), routes.client(), routes.backend()),
    };
    let id = source.expect_packet(state, direction, name);
    let mut emitted = VecDeque::new();
    let mut cx = Cx {
        emitted: &mut emitted,
        client_protocol: client.protocol,
        backend_protocol: backend.protocol,
    };
    routes.dispatch(
        step,
        &mut cx,
        direction,
        state,
        Packet::new(id, body.to_vec()),
    )?;
    Ok(emitted
        .into_iter()
        .map(|emitted| {
            let data = match emitted.direction {
                Direction::Clientbound => client,
                Direction::Serverbound => backend,
            };
            Out {
                direction: emitted.direction,
                state: emitted.state,
                name: data
                    .packet_name(emitted.state, emitted.direction, emitted.packet.id)
                    .expect("emitted packet has a name"),
                data: emitted.packet.data,
            }
        })
        .collect())
}

/// Like [`send`], expecting exactly one packet back: its name and body.
pub fn one<S>(
    routes: &'static Routes<S>,
    step: &mut S,
    direction: Direction,
    name: &str,
    body: &[u8],
) -> (&'static str, Vec<u8>) {
    let mut out = send(routes, step, direction, name, body).expect("packet translates");
    assert_eq!(out.len(), 1, "{name} becomes one packet: {out:?}");
    let out = out.remove(0);
    assert_eq!((out.direction, out.state), (direction, State::Play));
    (out.name, out.data)
}

pub fn hex(text: &str) -> Vec<u8> {
    let text: String = text.split_whitespace().collect();
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex"))
        .collect()
}

/// Every recorded play packet `name` in `direction` of the official
/// `protocol` server's session; `None` without the corpus (tests skip).
pub fn recorded(protocol: i32, direction: Direction, name: &str) -> Option<Vec<Vec<u8>>> {
    let Some(corpus) = testing::corpus(protocol) else {
        eprintln!("no corpus for protocol {protocol}: skipped");
        return None;
    };
    Some(
        corpus
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

/// The recorded join game of `protocol`.
pub fn recorded_login(protocol: i32) -> Option<Vec<u8>> {
    recorded(protocol, Direction::Clientbound, "login").map(|mut logins| logins.remove(0))
}

/// The registry codec of a join game packet of `protocol`.
pub fn codec_of(protocol: i32, body: &[u8]) -> Compound {
    let mut codec = None;
    play::rewrite_login(body, protocol, protocol, |found| {
        codec = Some(found.clone());
        Ok(())
    })
    .expect("join game parses");
    codec.expect("join game has a codec")
}

/// `value` with every compound's keys sorted, to compare tags whose entries
/// were added in another order.
pub fn canonical(value: &Nbt) -> Nbt {
    match value {
        Nbt::Compound(compound) => {
            let mut entries: Vec<(String, Nbt)> = compound
                .iter()
                .map(|(key, value)| (key.to_owned(), canonical(value)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Nbt::Compound(entries.into_iter().collect())
        }
        Nbt::List(values) => Nbt::List(values.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// The entries of `registry` by name.
pub fn entries<'a>(codec: &'a Compound, registry: &str) -> Vec<(&'a str, &'a Compound)> {
    codec
        .compound(&format!("minecraft:{registry}"))
        .and_then(|registry| registry.list("value"))
        .unwrap_or_default()
        .iter()
        .filter_map(Nbt::as_compound)
        .map(|entry| (entry.str("name").unwrap_or_default(), entry))
        .collect()
}

/// Truncations of `body`, and `body` with a byte added: none may panic or
/// translate.
pub fn mangle<S>(
    routes: &'static Routes<S>,
    make: impl Fn() -> S,
    direction: Direction,
    name: &str,
    body: &[u8],
) {
    // Every length of short packets; for long ones the ends and a sample.
    let lengths: Vec<usize> = if body.len() <= 256 {
        (0..body.len()).collect()
    } else {
        (0..64)
            .chain((64..body.len() - 64).step_by(body.len() / 128))
            .chain(body.len() - 64..body.len())
            .collect()
    };
    for length in lengths {
        let result = send(routes, &mut make(), direction, name, &body[..length]);
        assert!(
            result.is_err(),
            "{name} truncated to {length} bytes translated"
        );
    }
    let mut longer = body.to_vec();
    longer.push(0);
    assert!(
        send(routes, &mut make(), direction, name, &longer).is_err(),
        "{name} with a trailing byte translated"
    );
}
