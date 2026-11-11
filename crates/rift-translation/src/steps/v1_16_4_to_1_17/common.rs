//! What the steps from 1.16.4 to 1.18.2 share: the `login` and `respawn`
//! handlers of 1.16.2 to 1.18.2, pre-1.20.5 item stacks as raw bytes, and
//! sending a packet under another name than the one it arrived as.
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared, registries::login},
    data,
    step::Cx,
    wire::{Reader, Rewriter},
};
use std::io;

/// Send `data` to the client as the packet `name` of `protocol`, the step's
/// client side.
pub fn to_client(cx: &mut Cx<'_>, protocol: i32, state: State, name: &str, data: Vec<u8>) {
    let id = data::version(protocol).expect_packet(state, Direction::Clientbound, name);
    cx.to_client(state, Packet::new(id, data));
}

/// Send `data` to the backend as the packet `name` of `protocol`, the step's
/// backend side.
pub fn to_backend(cx: &mut Cx<'_>, protocol: i32, state: State, name: &str, data: Vec<u8>) {
    let id = data::version(protocol).expect_packet(state, Direction::Serverbound, name);
    cx.to_backend(state, Packet::new(id, data));
}

/// Copy one item stack from `rw.r` to `rw.w`, converting it between the
/// step's protocols.
pub fn item(shared: &mut Shared, direction: Direction, rw: &mut Rewriter<'_>) -> io::Result<()> {
    crate::codec::item::transcode(shared, direction, rw)
}

/// The empty item stack before 1.20.5: a false presence flag.
pub const EMPTY_ITEM: &[u8] = &[0];

/// The raw bytes of one item stack as 1.13.2 to 1.20.4 send it: a presence
/// flag, then the item ID, count and named NBT tag.
pub fn raw_item<'a>(reader: &mut Reader<'a>) -> io::Result<&'a [u8]> {
    let start = reader.peek();
    if reader.bool()? {
        reader.varint()?;
        reader.i8()?;
        reader.nbt_bytes(true)?;
    }
    Ok(&start[..start.len() - reader.remaining()])
}

/// The simulation distance joins `login` in 1.18.
const SIMULATION_DISTANCE: i32 = 757;

/// The join game packet between two of 1.16.2 to 1.18.2: the registry
/// codec and the dimension type are the registries codec's to convert
/// (world height, biome shape and categories, infiniburn tags); a 1.18
/// client is told the server simulates as far as an older one sends.
pub fn login<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (from, to) = (shared.backend, shared.client);
    let mut rw = Rewriter::new(&packet.data);
    login::transcode_login(shared, Direction::Clientbound, &mut rw)?;
    // Seed, maximum players, view distance.
    rw.i64()?;
    rw.varint()?;
    let view_distance = rw.varint()?;
    match (from >= SIMULATION_DISTANCE, to >= SIMULATION_DISTANCE) {
        (true, true) => {
            rw.varint()?;
        }
        (true, false) => {
            rw.r.varint()?;
        }
        (false, true) => {
            rw.w.varint(view_distance);
        }
        (false, false) => {}
    }
    // Reduced debug info, respawn screen, debug and flat world.
    for _ in 0..4 {
        rw.bool()?;
    }
    cx.to_client(state, Packet::new(packet.id, rw.finish()?));
    Ok(())
}

/// The respawn packet between two of 1.16.2 to 1.18.2: only the dimension
/// type changes, the rest is checked and copied.
pub fn respawn<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let mut rw = Rewriter::new(&packet.data);
    login::transcode_respawn(step.shared(), Direction::Clientbound, &mut rw)?;
    // Seed, game mode, previous game mode, debug, flat, keep data.
    rw.i64()?;
    rw.u8()?;
    rw.i8()?;
    for _ in 0..3 {
        rw.bool()?;
    }
    cx.to_client(state, Packet::new(packet.id, rw.finish()?));
    Ok(())
}

/// Running a step on single packets and on recorded sessions, for the
/// tests of the steps from 1.16.4 to 1.18.2.
#[cfg(test)]
pub mod testing {
    use crate::{
        Direction, Packet, State,
        codec::testing::corpus,
        data,
        step::{Cx, Emitted, Step},
    };
    use std::{collections::BTreeMap, collections::VecDeque, io};

    /// A packet a step emitted, named in the protocol it is in.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Out {
        pub direction: Direction,
        pub state: State,
        pub name: String,
        pub data: Vec<u8>,
    }

    /// Feed the packet `name` (of the sender's protocol) to `step`, which
    /// sits between `client` and `backend`.
    pub fn send(
        step: &mut dyn Step,
        (client, backend): (i32, i32),
        direction: Direction,
        state: State,
        name: &str,
        body: &[u8],
    ) -> io::Result<Vec<Out>> {
        let source = match direction {
            Direction::Clientbound => backend,
            Direction::Serverbound => client,
        };
        let id = data::version(source).expect_packet(state, direction, name);
        send_id(step, (client, backend), direction, state, id, body)
    }

    pub fn send_id(
        step: &mut dyn Step,
        (client, backend): (i32, i32),
        direction: Direction,
        state: State,
        id: i32,
        body: &[u8],
    ) -> io::Result<Vec<Out>> {
        let mut emitted = VecDeque::new();
        let mut cx = Cx {
            emitted: &mut emitted,
            client_protocol: client,
            backend_protocol: backend,
        };
        let packet = Packet::new(id, body.to_vec());
        match direction {
            Direction::Clientbound => step.clientbound(&mut cx, state, packet)?,
            Direction::Serverbound => step.serverbound(&mut cx, state, packet)?,
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

    /// What replaying one direction of a recorded session did, per packet
    /// name of the sender's protocol.
    #[derive(Debug, Default)]
    pub struct Replay {
        pub passed: BTreeMap<String, usize>,
        pub failed: BTreeMap<String, String>,
    }

    /// Replay every `direction` packet the official `recorded` server's
    /// session holds through `step`. `None` without the corpus.
    pub fn replay(
        step: &mut dyn Step,
        sides: (i32, i32),
        recorded: i32,
        direction: Direction,
    ) -> Option<Replay> {
        let packets = corpus(recorded)?;
        let mut replay = Replay::default();
        for packet in packets
            .iter()
            .filter(|packet| packet.direction == direction)
        {
            let name = packet.name(recorded).to_owned();
            match send_id(
                step,
                sides,
                direction,
                packet.state,
                packet.id,
                &packet.data,
            ) {
                Ok(_) => *replay.passed.entry(name).or_default() += 1,
                Err(error) => {
                    replay
                        .failed
                        .entry(name)
                        .or_insert_with(|| error.to_string());
                }
            }
        }
        Some(replay)
    }

    /// The bodies of every `name` packet in `protocol`'s recorded session.
    pub fn recorded(protocol: i32, direction: Direction, name: &str) -> Vec<Vec<u8>> {
        let Some(packets) = corpus(protocol) else {
            return Vec::new();
        };
        packets
            .into_iter()
            .filter(|packet| packet.direction == direction && packet.name(protocol) == name)
            .map(|packet| packet.data)
            .collect()
    }

    pub fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }
}
