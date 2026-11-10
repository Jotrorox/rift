//! Block states 1.13 has and a 1.12 server cannot send.
//!
//! 1.12 kept the colour of beds and banners, the type and rotation of
//! skulls, the plant in a flower pot and a note block's note in block
//! entities; 1.13 made them block states. The chunk codec flattens blocks
//! state by state; this module follows the block entities (in chunks,
//! `block_entity_data` and note block events) and completes the states in
//! what the client receives: in the codec's 1.13 chunks, in the block
//! updates the step converts itself, and with an extra block update when a
//! block entity arrives after its block.
//!
//! The step owns `block_update` and `chunk_blocks_update` for this pair
//! (their only conversion is the state mapping, and the step must see every
//! state it sends). Chunks, `block_entity_data` and `block_event` stay the
//! codecs': they are tapped as the backend sent them and, for chunks,
//! completed after conversion.
pub mod chunk;
mod special;

use super::{Up, packets};
use crate::{
    Direction, Packet, State,
    step::{Cx, RouteBuilder},
    wire::{Position, Reader, Writer, invalid},
};
use chunk::{Column, Format};
use special::Special;
use std::{collections::HashMap, io};

/// Section-relative block index, as chunk sections order blocks.
fn index(position: Position) -> usize {
    ((position.y & 15) << 8 | (position.z & 15) << 4 | (position.x & 15)) as usize
}

fn column_of(position: Position) -> (i32, i32) {
    (position.x >> 4, position.z >> 4)
}

/// A position as a map key.
type Key = (i32, i32, i32);

fn key(position: Position) -> Key {
    (position.x, position.y, position.z)
}

/// What the step knows about one block with block entity data.
#[derive(Clone, Copy, Debug, Default)]
struct Known {
    /// The 1.13 state the client was last sent, when the step saw it.
    state: Option<i32>,
    special: Option<Special>,
}

/// A chunk packet the backend sent, waiting for the codec's conversion.
struct PendingChunk {
    x: i32,
    z: i32,
    /// A hash of the 1.12 body: an unchanged body means no codec converted
    /// it, and there is nothing to complete.
    hash: u64,
}

#[derive(Default)]
pub struct World {
    /// Blocks with block entity data, by column.
    columns: HashMap<(i32, i32), HashMap<Key, Known>>,
    chunk: Option<PendingChunk>,
    /// Positions whose block entity data changed in the packet in flight.
    touched: Vec<Position>,
}

impl World {
    pub fn clear(&mut self) {
        self.columns.clear();
        self.chunk = None;
        self.touched.clear();
    }

    fn known(&mut self, position: Position) -> &mut Known {
        self.columns
            .entry(column_of(position))
            .or_default()
            .entry(key(position))
            .or_default()
    }
}

fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

pub fn install(builder: RouteBuilder<Up>) -> RouteBuilder<Up> {
    use Direction::Clientbound;
    builder
        .tap(Clientbound, State::Play, "level_chunk", level_chunk)
        .tap(
            Clientbound,
            State::Play,
            "block_entity_data",
            block_entity_data,
        )
        .tap(Clientbound, State::Play, "block_event", block_event)
        .tap(Clientbound, State::Play, "forget_level_chunk", forget)
        .clientbound(State::Play, "block_update", block_update)
        .clientbound(State::Play, "chunk_blocks_update", chunk_blocks_update)
}

/// A 1.12 chunk: its block entities replace what the step knew about the
/// sections it carries (all of the column for a full chunk).
fn level_chunk(step: &mut Up, _: State, packet: &Packet) -> io::Result<()> {
    let chunk::Header {
        x,
        z,
        full,
        mask,
        block_entities: tags,
    } = chunk::header(&packet.data)?;
    let world = &mut step.world;
    let column = world.columns.entry((x, z)).or_default();
    if full {
        column.clear();
    } else {
        column.retain(|(_, y, _), _| mask & (1 << (y >> 4)) == 0);
    }
    for tag in tags {
        if let Some((position, special)) = special::from_block_entity(tag)?
            && column_of(position) == (x, z)
        {
            column.insert(
                key(position),
                Known {
                    state: None,
                    special: Some(special),
                },
            );
        }
    }
    world.chunk = Some(PendingChunk {
        x,
        z,
        hash: hash(&packet.data),
    });
    Ok(())
}

/// 1.12: position, action, NBT. Beds, banners, skulls and flower pots
/// report their data this way after the block itself.
fn block_entity_data(step: &mut Up, _: State, packet: &Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let position = reader.position_legacy()?;
    reader.u8()?; // Action.
    let tag = reader.nbt_bytes(true)?;
    reader.finish()?;
    if let Some((_, special)) = special::from_block_entity(tag)? {
        step.world.known(position).special = Some(special);
        step.world.touched.push(position);
    }
    Ok(())
}

/// 1.12: position, action, parameter, block ID. Note blocks play with the
/// instrument as action and the note as parameter, which tells the step
/// both.
fn block_event(step: &mut Up, _: State, packet: &Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let position = reader.position_legacy()?;
    let action = reader.u8()?;
    let parameter = reader.u8()?;
    let block = reader.varint()?;
    reader.finish()?;
    if let Some(special) = special::from_block_event(block, action, parameter) {
        let known = step.world.known(position);
        known.special = Some(match known.special {
            Some(previous) => previous.merge(special),
            None => special,
        });
        step.world.touched.push(position);
    }
    Ok(())
}

fn forget(step: &mut Up, _: State, packet: &Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let x = reader.i32()?;
    let z = reader.i32()?;
    reader.finish()?;
    step.world.columns.remove(&(x, z));
    Ok(())
}

/// The 1.13 state for a block whose flattened state is `state`, completed
/// from what the step knows about that position; forgets block entity data
/// the block no longer matches.
fn complete_state(world: &mut World, position: Position, state: i32) -> i32 {
    let column = world.columns.entry(column_of(position)).or_default();
    if !special::has_block_entity(state) {
        column.remove(&key(position));
        return state;
    }
    let known = column.entry(key(position)).or_default();
    let completed = match known.special {
        Some(special) => special.apply(state).unwrap_or(state),
        None => state,
    };
    known.state = Some(completed);
    completed
}

/// 1.12 and 1.13: position, state.
fn block_update(step: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let position = reader.position_legacy()?;
    let block = reader.varint()?;
    reader.finish()?;
    let mapping = step.shared.mapping(Direction::Clientbound, "block_state");
    let flattened = mapping.get(block).unwrap_or(0);
    let completed = complete_state(&mut step.world, position, flattened);
    let mut writer = Writer::with_capacity(12);
    writer.position_legacy(position).varint(completed);
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    Ok(())
}

/// 1.12 and 1.13: column X and Z, then records of packed position
/// (`x << 4 | z` byte, Y byte) and state.
fn chunk_blocks_update(
    step: &mut Up,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let x = reader.i32()?;
    let z = reader.i32()?;
    let count = reader.count()?;
    let mapping = step.shared.mapping(Direction::Clientbound, "block_state");
    let mut writer = Writer::with_capacity(packet.data.len() + 16);
    writer.i32(x).i32(z).varint(count as i32);
    for _ in 0..count {
        let horizontal = reader.u8()?;
        let y = reader.u8()?;
        let block = reader.varint()?;
        let position = Position {
            x: x << 4 | i32::from(horizontal >> 4),
            y: i32::from(y),
            z: z << 4 | i32::from(horizontal & 15),
        };
        let flattened = mapping.get(block).unwrap_or(0);
        let completed = complete_state(&mut step.world, position, flattened);
        writer.u8(horizontal).u8(y).varint(completed);
    }
    reader.finish()?;
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    Ok(())
}

/// After the codecs ran on a backend packet: complete the 1.13 chunk they
/// produced, and send the corrected state of blocks whose block entity data
/// just arrived.
pub fn complete(step: &mut Up, cx: &mut Cx<'_>, start: usize) -> io::Result<()> {
    let ids = packets::ids();
    if let Some(pending) = step.world.chunk.take() {
        for emitted in cx.emitted.range_mut(start..) {
            if emitted.direction == Direction::Clientbound
                && emitted.state == State::Play
                && emitted.packet.id == ids.level_chunk
            {
                if hash(&emitted.packet.data) == pending.hash {
                    // INTEGRATION: no chunk codec converted it yet.
                    continue;
                }
                let sky = step.dimension == 0;
                let mut column = Column::read(&emitted.packet.data, Format::Flattened, sky)?;
                if (column.x, column.z) != (pending.x, pending.z) {
                    return Err(invalid("converted chunk is for another column"));
                }
                complete_column(&mut step.world, &mut column);
                if column.changed() {
                    emitted.packet.data = column.write(Format::Flattened);
                }
            }
        }
    }
    for position in std::mem::take(&mut step.world.touched) {
        let known = step.world.known(position);
        let (Some(state), Some(special)) = (known.state, known.special) else {
            continue;
        };
        let Some(completed) = special.apply(state) else {
            continue;
        };
        if completed != state {
            known.state = Some(completed);
            let mut writer = Writer::with_capacity(12);
            writer.position_legacy(position).varint(completed);
            cx.to_client(State::Play, Packet::new(ids.block_update, writer.buf));
        }
    }
    Ok(())
}

/// Apply what the step knows about the column's block entities to its
/// flattened sections.
fn complete_column(world: &mut World, column: &mut Column) {
    let Some(known) = world.columns.get_mut(&(column.x, column.z)) else {
        return;
    };
    known.retain(|&(x, y, z), known| {
        let position = Position { x, y, z };
        let Some(Some(section)) = usize::try_from(y >> 4)
            .ok()
            .and_then(|y| column.sections.get_mut(y))
        else {
            // Not in this packet: what the client has stays as it was.
            return (0..256).contains(&y);
        };
        let state = i32::from(section.get(index(position)));
        if !special::has_block_entity(state) {
            return false;
        }
        let completed = known
            .special
            .and_then(|special| special.apply(state))
            .unwrap_or(state);
        section.set(index(position), completed as u16);
        known.state = Some(completed);
        true
    });
}

/// The chunk tap on its own, for tests that convert chunks themselves.
#[cfg(test)]
pub fn tap_chunk(step: &mut Up, packet: &Packet) -> io::Result<()> {
    level_chunk(step, State::Play, packet)
}
