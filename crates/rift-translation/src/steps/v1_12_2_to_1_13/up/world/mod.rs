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
//! It also computes the states a 1.12 client derives from neighbouring
//! blocks (see [`connect`]), keeping the blocks of every loaded column in a
//! compact [`store`]; when a block changes, its neighbours are connected
//! again and the client gets updates for those that changed, as a 1.13
//! server would send them.
//!
//! The step owns `block_update` and `chunk_blocks_update` for this pair
//! (their only conversion is the state mapping, and the step must see every
//! state it sends). Chunks, `block_entity_data` and `block_event` stay the
//! codecs': they are tapped as the backend sent them and, for chunks,
//! completed after conversion.
pub mod chunk;
mod connect;
mod special;
mod store;

use super::{Up, packets};
use crate::{
    Direction, Packet, State,
    step::{Cx, RouteBuilder},
    wire::{Position, Reader, Writer, invalid},
};
use chunk::{Column, Format};
use special::Special;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io,
};
use store::{Packed, Store};

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
    /// The blocks of loaded columns, for connections.
    store: Store,
    /// Blocks an explosion in flight removed.
    exploded: Vec<Key>,
}

impl World {
    pub fn clear(&mut self) {
        self.columns.clear();
        self.chunk = None;
        self.touched.clear();
        self.store.clear();
        self.exploded.clear();
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
        .tap(Clientbound, State::Play, "explode", explode)
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
    step.world.store.unload(x, z);
    Ok(())
}

/// 1.12 and 1.13: centre, radius, the removed blocks as byte offsets from
/// the centre's integer part (truncated, as vanilla casts), the push.
fn explode(step: &mut Up, _: State, packet: &Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let (x, y, z) = (reader.f32()?, reader.f32()?, reader.f32()?);
    reader.f32()?; // Radius.
    let count = usize::try_from(reader.i32()?).map_err(|_| invalid("negative block count"))?;
    if count > reader.remaining() / 3 {
        return Err(invalid("explosion lists more blocks than it carries"));
    }
    let (x, y, z) = (x as i32, y as i32, z as i32);
    for _ in 0..count {
        let (dx, dy, dz) = (reader.i8()?, reader.i8()?, reader.i8()?);
        step.world
            .exploded
            .push((x + i32::from(dx), y + i32::from(dy), z + i32::from(dz)));
    }
    reader.take(12)?; // Player push.
    reader.finish()
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
    crate::codec::chunk::blocks::track(&mut step.shared, position, completed);
    let mut placed = [(key(position), completed as u16)];
    let changed = place(&mut step.world.store, &mut placed);
    let mut writer = Writer::with_capacity(12);
    writer
        .position_legacy(position)
        .varint(i32::from(placed[0].1));
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    send_changes(cx, changed);
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
    let mut placed = Vec::with_capacity(count);
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
        crate::codec::chunk::blocks::track(&mut step.shared, position, completed);
        placed.push((key(position), completed as u16));
    }
    reader.finish()?;
    let changed = place(&mut step.world.store, &mut placed);
    let mut writer = Writer::with_capacity(packet.data.len() + 16);
    writer.i32(x).i32(z).varint(placed.len() as i32);
    for ((bx, by, bz), block) in placed {
        writer
            .u8(((bx & 15) << 4 | (bz & 15)) as u8)
            .u8(by as u8)
            .varint(i32::from(block));
    }
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    send_changes(cx, changed);
    Ok(())
}

/// After the codecs ran on a backend packet: complete the 1.13 chunk they
/// produced, and send the corrected state of blocks whose block entity data
/// just arrived.
pub fn complete(step: &mut Up, cx: &mut Cx<'_>, start: usize) -> io::Result<()> {
    let ids = packets::ids();
    let mut changed = HashMap::new();
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
                changed.extend(connect_column(&mut step.world.store, &mut column));
                if column.changed() {
                    emitted.packet.data = column.write(Format::Flattened);
                }
            }
        }
    }
    let exploded = std::mem::take(&mut step.world.exploded);
    if !exploded.is_empty() {
        let store = &mut step.world.store;
        for &(x, y, z) in &exploded {
            store.set(x, y, z, 0);
        }
        settle(
            store,
            exploded.iter().flat_map(|key| around(*key)),
            &mut changed,
        );
    }
    send_changes(cx, changed);
    for position in std::mem::take(&mut step.world.touched) {
        let known = step.world.known(position);
        let (Some(state), Some(special)) = (known.state, known.special) else {
            continue;
        };
        let completed = special.apply(state).unwrap_or(state);
        known.state = Some(completed);
        let mut writer = Writer::with_capacity(12);
        writer.position_legacy(position).varint(completed);
        // The chunk codec corrects beds, banners, skulls and pots from the
        // same block entity; its update carries what the step knows.
        let codec = cx.emitted.range_mut(start..).find(|emitted| {
            emitted.direction == Direction::Clientbound
                && emitted.packet.id == ids.block_update
                && Reader::new(&emitted.packet.data)
                    .position_legacy()
                    .is_ok_and(|at| at == position)
        });
        match codec {
            Some(emitted) => emitted.packet.data = writer.buf,
            None if completed != state => {
                cx.to_client(State::Play, Packet::new(ids.block_update, writer.buf));
            }
            None => {}
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

impl connect::Blocks for Store {
    fn state(&self, x: i32, y: i32, z: i32) -> Option<u16> {
        self.get(x, y, z)
    }
}

/// The 26 blocks around a position.
fn around((x, y, z): Key) -> impl Iterator<Item = Key> {
    (-1..=1).flat_map(move |dx| {
        (-1..=1).flat_map(move |dy| {
            (-1..=1)
                .filter(move |dz| (dx, dy, *dz) != (0, 0, 0))
                .map(move |dz| (x + dx, y + dy, z + dz))
        })
    })
}

/// How many neighbour changes one packet may cascade into; vanilla's rules
/// settle long before.
const CASCADE: usize = 4096;

/// Connect the blocks at `seeds` again, and around every block that
/// changed, until nothing changes; changed blocks go into `changed`.
fn settle(
    store: &mut Store,
    seeds: impl IntoIterator<Item = Key>,
    changed: &mut HashMap<Key, u16>,
) {
    let mut queue: VecDeque<Key> = seeds.into_iter().collect();
    let mut budget = CASCADE;
    while let Some((x, y, z)) = queue.pop_front() {
        let Some(state) = store.get(x, y, z) else {
            continue;
        };
        if !connect::connects(state) {
            continue;
        }
        let connected = connect::connect(store, x, y, z, state, true);
        if connected != state && budget > 0 {
            budget -= 1;
            store.set(x, y, z, connected);
            changed.insert((x, y, z), connected);
            queue.extend(around((x, y, z)));
        }
    }
}

/// Blocks the backend set (flattened and completed): store them, connect
/// them and their neighbours. `blocks` ends with the states to send for
/// them; returns the other blocks that changed.
fn place(store: &mut Store, blocks: &mut [(Key, u16)]) -> HashMap<Key, u16> {
    for &((x, y, z), state) in blocks.iter() {
        store.set(x, y, z, connect::info(state).stored);
    }
    for ((x, y, z), state) in blocks.iter_mut() {
        if connect::connects(*state) && store.get(*x, *y, *z).is_some() {
            *state = connect::connect(store, *x, *y, *z, *state, false);
            store.set(*x, *y, *z, *state);
        }
    }
    let mut changed = HashMap::new();
    settle(
        store,
        blocks.iter().flat_map(|(key, _)| around(*key)),
        &mut changed,
    );
    for (key, state) in blocks.iter_mut() {
        if let Some(connected) = changed.remove(key) {
            *state = connected;
        }
    }
    changed
}

/// Send changed blocks, as one block update or one multi-block update per
/// column.
fn send_changes(cx: &mut Cx<'_>, changed: HashMap<Key, u16>) {
    let ids = packets::ids();
    let mut columns: BTreeMap<(i32, i32), Vec<(Key, u16)>> = BTreeMap::new();
    for (key, state) in changed {
        columns
            .entry((key.0 >> 4, key.2 >> 4))
            .or_default()
            .push((key, state));
    }
    for ((x, z), mut blocks) in columns {
        blocks.sort_unstable();
        let mut writer = Writer::new();
        if let [((bx, by, bz), state)] = blocks[..] {
            writer
                .position_legacy(Position {
                    x: bx,
                    y: by,
                    z: bz,
                })
                .varint(i32::from(state));
            cx.to_client(State::Play, Packet::new(ids.block_update, writer.buf));
            continue;
        }
        writer.i32(x).i32(z).varint(blocks.len() as i32);
        for ((bx, by, bz), state) in blocks {
            writer
                .u8(((bx & 15) << 4 | (bz & 15)) as u8)
                .u8(by as u8)
                .varint(i32::from(state));
        }
        cx.to_client(
            State::Play,
            Packet::new(ids.chunk_blocks_update, writer.buf),
        );
    }
}

/// Store a converted column and connect its blocks: those whose state
/// depends on others' first (stairs shapes decide stairs' faces), then
/// fences, panes and walls. Blocks of loaded neighbour columns along its
/// edges are connected again; returns those that changed.
fn connect_column(store: &mut Store, column: &mut Column) -> HashMap<Key, u16> {
    let (cx, cz) = (column.x, column.z);
    let sections = store.load(cx, cz);
    if column.full {
        *sections = Default::default();
    }
    let mut connecting = Vec::new();
    for (sy, section) in column.sections.iter().enumerate() {
        let Some(section) = section else { continue };
        let mut stored = Box::new([0u16; chunk::SECTION_BLOCKS]);
        for (index, slot) in stored.iter_mut().enumerate() {
            let state = section.get(index);
            *slot = connect::info(state).stored;
            if connect::connects(state) {
                connecting.push((sy, index, state));
            }
        }
        sections[sy] = stored
            .iter()
            .any(|state| *state != 0)
            .then(|| Packed::from_states(&stored));
    }
    let position = |sy: usize, index: usize| {
        (
            cx << 4 | (index & 15) as i32,
            (sy << 4 | index >> 8) as i32,
            cz << 4 | (index >> 4 & 15) as i32,
        )
    };
    for faces in [false, true] {
        for &(sy, index, state) in &connecting {
            if connect::face_dependent(state) != faces {
                continue;
            }
            let (x, y, z) = position(sy, index);
            let current = store.get(x, y, z).unwrap_or(state);
            let connected = connect::connect(store, x, y, z, current, false);
            if connected != current {
                store.set(x, y, z, connected);
            }
            if let Some(section) = &mut column.sections[sy] {
                section.set(index, connected);
            }
        }
    }
    let mut seeds = Vec::new();
    for (dx, dz) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
        if !store.has_column(cx + dx, cz + dz) {
            continue;
        }
        for t in 0..16 {
            let (x, z) = match (dx, dz) {
                (-1, _) => ((cx << 4) - 1, cz << 4 | t),
                (1, _) => ((cx + 1) << 4, cz << 4 | t),
                (_, -1) => (cx << 4 | t, (cz << 4) - 1),
                _ => (cx << 4 | t, (cz + 1) << 4),
            };
            seeds.extend((0..256).map(|y| (x, y, z)));
        }
    }
    let mut changed = HashMap::new();
    settle(store, seeds, &mut changed);
    changed.retain(|&(x, y, z), state| {
        if (x >> 4, z >> 4) != (cx, cz) {
            return true;
        }
        if let Some(Some(section)) = column.sections.get_mut((y >> 4) as usize) {
            section.set(((y & 15) << 8 | (z & 15) << 4 | (x & 15)) as usize, *state);
            false
        } else {
            true
        }
    });
    changed
}

/// The chunk tap on its own, for tests that convert chunks themselves.
#[cfg(test)]
pub fn tap_chunk(step: &mut Up, packet: &Packet) -> io::Result<()> {
    level_chunk(step, State::Play, packet)
}

#[cfg(test)]
impl World {
    pub fn store_bytes(&self) -> usize {
        self.store.bytes()
    }
}
