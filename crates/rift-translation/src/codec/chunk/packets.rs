//! Handlers for the chunk and light packets, and the taps that follow the
//! world a connection is in.
use super::{
    State,
    convert::{self, Plan},
    light::{Light, LightArray, LightUpdate},
    model::{Chunk, World},
};
use crate::{
    Direction, Packet, State as Phase,
    codec::{HasShared, Shared},
    data,
    mapping::{self, Mapping},
    nbt::Nbt,
    step::Cx,
    version,
    wire::{Position, Reader, Writer, invalid},
};
use std::{
    collections::HashMap,
    io,
    sync::{Mutex, OnceLock},
};

/// At most this many columns' light is held back at once; a server that
/// sends light for columns it never loads cannot grow the table further.
const HELD_LIGHT: usize = 4096;

/// Whether a mapping changes no ID, computed once per mapping.
pub fn is_identity(mapping: &'static Mapping) -> bool {
    static SEEN: OnceLock<Mutex<HashMap<usize, bool>>> = OnceLock::new();
    let key = mapping as *const Mapping as usize;
    *SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(key)
        .or_insert_with(|| mapping.is_identity())
}

/// The IDs of `registry` from `from` to the adjacent `to`: `Ok(None)` when
/// nothing changes, an error when there is no mapping at all.
pub fn ids(from: i32, to: i32, registry: &str) -> io::Result<Option<&'static Mapping>> {
    match mapping::try_mapping(from, to, registry) {
        // INTEGRATION: the pre-flattening block state and item mappings
        // (`crate::legacy`) are still being written; until then the
        // 1.12.2 / 1.13 pair has none and its chunks fail.
        None => Err(invalid(&format!(
            "no {registry} mapping from {} to {}",
            version::release(from),
            version::release(to)
        ))),
        Some(mapping) if is_identity(mapping) => Ok(None),
        Some(mapping) => Ok(Some(mapping)),
    }
}

/// Whether `registry` maps between the protocols without changing an ID;
/// a missing mapping counts as a change.
pub fn unchanged(from: i32, to: i32, registry: &str) -> bool {
    matches!(ids(from, to, registry), Ok(None))
}

/// The state receivers use where a block has no counterpart.
pub fn block_fallback(protocol: i32) -> i32 {
    if version::flattened(protocol) {
        data::version(protocol)
            .block_states
            .parse("stone")
            .unwrap_or(1)
    } else {
        1 << 4
    }
}

/// The biome ID mapping for built-in biome registries (before 1.16.2).
pub fn biome_ids(from: i32, to: i32) -> Option<&'static Mapping> {
    if from >= 751 || to >= 751 {
        // INTEGRATION: from 1.16.2 biome IDs index the registry the server
        // sends; the registries codec will provide that mapping. Until then
        // IDs pass unchanged across 1.16.1 / 1.16.2 and later pairs.
        return None;
    }
    mapping::try_mapping(from, to, "biome").filter(|mapping| !is_identity(mapping))
}

fn plains(protocol: i32) -> i32 {
    data::version(protocol)
        .registry("biome")
        .and_then(|registry| registry.id("plains"))
        .unwrap_or(1)
}

/// The world as the receiver of clientbound packets sees it.
fn target_world(state: &State, source: i32, target: i32) -> World {
    World {
        sky_light: state.world.sky_light,
        // INTEGRATION: a 1.17+ client's world height comes from the
        // dimension the step sends it; a 1.16.5 backend's world is 0 to 255.
        min_section: if source >= 755 && target >= 755 {
            state.world.min_section
        } else {
            None
        },
        biome_bits: state.world.biome_bits,
    }
}

/// Convert a chunk read from the backend into the client's terms, with
/// the shared state's mappings.
pub fn convert_chunk(shared: &mut Shared, chunk: &mut Chunk) -> io::Result<()> {
    let (source, target) = (shared.backend, shared.client);
    let blocks = ids(source, target, "block_state")?;
    let biomes = biome_ids(source, target);
    let fallback_biome = plains(target);
    let biome_map = biomes.map(|mapping| move |id: i32| mapping.get(id).unwrap_or(fallback_biome));
    let block_entity_types = if source >= 757 && target >= 757 {
        ids(source, target, "block_entity_type")?
    } else {
        None
    };
    let plan = Plan {
        source,
        target,
        blocks,
        block_fallback: block_fallback(target),
        biomes: biome_map.as_ref().map(|map| map as &dyn Fn(i32) -> i32),
        block_entity_types,
        world: target_world(&shared.chunk, source, target),
    };
    let mut scratch = std::mem::take(&mut shared.chunk.scratch);
    let result = convert::convert(chunk, &plan, &mut scratch);
    shared.chunk.scratch = scratch;
    result?;
    super::block_entity::convert_chunk(shared, Direction::Clientbound, chunk)
}

fn packet_id(protocol: i32, name: &str) -> io::Result<i32> {
    data::version(protocol)
        .packet_id(Phase::Play, Direction::Clientbound, name)
        .ok_or_else(|| {
            invalid(&format!(
                "{} has no {name} packet",
                version::release(protocol)
            ))
        })
}

fn chunk_packet_name(protocol: i32) -> &'static str {
    if protocol >= 757 {
        "level_chunk_with_light"
    } else {
        "level_chunk"
    }
}

/// `level_chunk` and `level_chunk_with_light`.
pub fn level_chunk<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (source, target) = (shared.backend, shared.client);
    let mut chunk = Chunk::read(&packet.data, source, &shared.chunk.world)?;
    let position = (chunk.x, chunk.z);
    if !chunk.full && target >= 755 {
        // 1.17 has no partial columns: send their sections block by block.
        convert_chunk(shared, &mut chunk)?;
        return super::blocks::sections_as_updates(shared, cx, state, &chunk);
    }
    if (477..757).contains(&source) && target < 477 {
        // The light this column needs arrived earlier on its own.
        chunk.light = shared.chunk.held_light.remove(&position);
    }
    if source < 757 && target >= 757 {
        chunk.light = Some(
            shared
                .chunk
                .held_light
                .remove(&position)
                .unwrap_or_default(),
        );
        shared.chunk.loaded.insert(position);
    }
    if (source < 393) != (target < 393) {
        track_column(shared, &mut chunk, source >= 393, true)?;
    }
    convert_chunk(shared, &mut chunk)?;
    if source < 393 && target >= 393 {
        track_column(shared, &mut chunk, true, false)?;
    }
    if (477..757).contains(&target)
        && let Some(light) = chunk.light.take()
    {
        let update = LightUpdate {
            x: chunk.x,
            z: chunk.z,
            light,
        };
        cx.to_client(
            state,
            Packet::new(packet_id(target, "light_update")?, update.write(target)?),
        );
    }
    if source < 768 && target >= 768 && !shared.chunk.loaded.insert(position) {
        // 1.21.2 clients draw a replaced column wrongly unless it is
        // forgotten first; older servers resend columns in place.
        let mut writer = Writer::new();
        writer.i32(chunk.z).i32(chunk.x);
        cx.to_client(
            state,
            Packet::new(packet_id(target, "forget_level_chunk")?, writer.buf),
        );
    }
    let body = chunk.write(target)?;
    cx.to_client(
        state,
        Packet::new(packet_id(target, chunk_packet_name(target))?, body),
    );
    Ok(())
}

/// Move light sections when a 1.17 world is cut to 1.16's 0 to 255.
fn rebase_light(light: &mut Light, shift: i32, count: usize) {
    for arrays in [&mut light.sky, &mut light.block] {
        let old = std::mem::take(arrays);
        let mut new = Vec::with_capacity(count);
        for (index, array) in old.into_iter().enumerate() {
            let index = index as i32 + shift;
            if (0..count as i32).contains(&index) {
                if new.len() <= index as usize {
                    new.resize(index as usize + 1, LightArray::Absent);
                }
                new[index as usize] = array;
            }
        }
        *arrays = new;
    }
}

/// Follow the flattened blocks of a column (1.12.2 / 1.13): before
/// conversion when the column is in 1.13 terms (`modern`), after when it
/// came from 1.12. `forget` first drops what was known for the column.
fn track_column(
    shared: &mut Shared,
    chunk: &mut Chunk,
    modern: bool,
    forget: bool,
) -> io::Result<()> {
    if forget {
        let (x, z) = (chunk.x, chunk.z);
        shared
            .chunk
            .flattened
            .retain(|(bx, _, bz), _| (bx >> 4, bz >> 4) != (x, z));
    }
    if !modern {
        return Ok(());
    }
    let states = &data::version(393).block_states;
    let mut scratch = std::mem::take(&mut shared.chunk.scratch);
    let found = super::flattening::find(chunk, states, &mut scratch);
    shared.chunk.scratch = scratch;
    for (x, y, z, state) in found? {
        super::blocks::track(shared, Position { x, y, z }, state);
    }
    Ok(())
}

/// `light_update`.
pub fn light_update<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (source, target) = (shared.backend, shared.client);
    let mut update = LightUpdate::read(&packet.data, source)?;
    let position = (update.x, update.z);
    if source >= 755 && target < 755 {
        let min = shared.chunk.world.min_section.unwrap_or(0);
        rebase_light(&mut update.light, min, 18);
    }
    let hold =
        target < 477 || (source < 757 && target >= 757 && !shared.chunk.loaded.contains(&position));
    if hold {
        // Pre-1.14 clients light blocks themselves and take light only
        // with a column; 1.18 clients take it with the column too. Keep it
        // until the column comes.
        if shared.chunk.held_light.len() >= HELD_LIGHT
            && !shared.chunk.held_light.contains_key(&position)
        {
            shared.chunk.held_light.clear();
        }
        match shared.chunk.held_light.get_mut(&position) {
            Some(held) => held.merge(update.light),
            None => {
                shared.chunk.held_light.insert(position, update.light);
            }
        }
        return Ok(());
    }
    if Light::has_trust_edges(target) && !Light::has_trust_edges(source) {
        update.light.trust_edges = true;
    }
    cx.to_client(state, Packet::new(packet.id, update.write(target)?));
    Ok(())
}

/// `forget_level_chunk`: x then z before 1.20.2, z then x after.
pub fn forget_level_chunk<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (source, target) = (shared.backend, shared.client);
    let mut reader = Reader::new(&packet.data);
    let (a, b) = (reader.i32()?, reader.i32()?);
    reader.finish()?;
    let (x, z) = if source >= 764 { (b, a) } else { (a, b) };
    let mut writer = Writer::new();
    if target >= 764 {
        writer.i32(z).i32(x);
    } else {
        writer.i32(x).i32(z);
    }
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    Ok(())
}

/// Forget what is held for a column the client drops.
pub fn tap_forget<S: HasShared>(step: &mut S, _: Phase, packet: &Packet) -> io::Result<()> {
    let shared = step.shared();
    let mut reader = Reader::new(&packet.data);
    let (a, b) = (reader.i32()?, reader.i32()?);
    let position = if shared.backend >= 764 {
        (b, a)
    } else {
        (a, b)
    };
    shared.chunk.held_light.remove(&position);
    shared.chunk.loaded.remove(&position);
    Ok(())
}

/// Follow the dimension `login` and `respawn` put the player in: whether it
/// has sky light (before 1.16, where the dimension is a number) and its
/// height (1.17 and 1.18, where the dimension type is inline NBT).
pub fn tap_world<S: HasShared>(step: &mut S, _: Phase, packet: &Packet) -> io::Result<()> {
    let shared = step.shared();
    let protocol = shared.backend;
    let data = data::version(protocol);
    let name = data
        .packet_name(Phase::Play, Direction::Clientbound, packet.id)
        .unwrap_or("");
    let login = name == "login";
    shared.chunk.held_light.clear();
    shared.chunk.loaded.clear();
    shared.chunk.flattened.clear();
    let mut reader = Reader::new(&packet.data);
    if protocol < 735 {
        if login {
            reader.i32()?; // Entity ID.
            reader.u8()?; // Game mode.
        }
        let dimension = reader.i32()?;
        shared.chunk.world.sky_light = Some(dimension == 0);
        return Ok(());
    }
    if !(755..=758).contains(&protocol) {
        return Ok(());
    }
    if login {
        reader.i32()?;
        reader.bool()?;
        reader.u8()?;
        reader.i8()?;
        for _ in 0..reader.count()? {
            reader.str()?;
        }
        reader.nbt_bytes(true)?; // The registry codec.
    }
    let Some(Nbt::Compound(dimension)) = Nbt::read(&mut reader, true)? else {
        return Err(invalid("dimension type is not a compound"));
    };
    if let Some(min_y) = dimension.int("min_y") {
        shared.chunk.world.min_section = Some(min_y.div_euclid(16));
    }
    shared.chunk.world.sky_light = dimension.int("has_skylight").map(|sky| sky != 0);
    Ok(())
}

/// A block position in `protocol`'s packing.
pub fn read_position(reader: &mut Reader<'_>, protocol: i32) -> io::Result<Position> {
    if version::modern_positions(protocol) {
        reader.position()
    } else {
        reader.position_legacy()
    }
}

pub fn write_position(writer: &mut Writer, protocol: i32, position: Position) {
    if version::modern_positions(protocol) {
        writer.position(position);
    } else {
        writer.position_legacy(position);
    }
}
