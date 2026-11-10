//! Chunks, light, block updates and block entities.
//!
//! [`model::Chunk`] holds a chunk column in any protocol's terms and reads
//! and writes every layout from 1.12 to 26.3; [`convert::convert`] brings a
//! column into an adjacent protocol's terms. The handlers here use both for
//! the packets this codec owns (see [`crate::codec`]) and are installed
//! only for the pairs whose layouts or IDs differ, so a pair with identical
//! chunks never parses one.
//!
//! Light is the one thing that cannot always be converted packet by packet:
//!
//! - A 1.13 column going to a 1.14 client is followed by its light as a
//!   `light_update` sent just before it.
//! - A 1.14 server sends a column's light before the column; for a 1.13
//!   client it is held (per column, merged with later updates) until the
//!   column comes and then goes inside it. Light updates for columns already
//!   sent are dropped: clients before 1.14 light block changes themselves.
//!   A column whose light never came gets dark block light and a lit sky.
//! - A 1.17 server's light for a column the 1.18 client does not have yet is
//!   held and merged into the column; later updates pass through. A 1.18
//!   column going to a 1.17 client is split into its light then the column.
//!
//! Partial columns (before 1.17) going to 1.17 clients become section
//! updates of every block plus block entity packets.
//!
//! Step authors: every handler is a plain generic function (`level_chunk`,
//! `light_update`, `block_update`...) that a step's own rule can call
//! before or after doing its own work, and [`packets::convert_chunk`]
//! converts a parsed column with the step's mappings.
pub mod block_entity;
pub mod blocks;
pub mod container;
pub mod convert;
pub mod explode;
mod flags;
pub mod flattening;
pub mod light;
pub mod model;
pub mod packets;
pub mod storage;
#[cfg(test)]
mod tests;

use super::HasShared;
use crate::{
    Direction, Packet, State as Phase, mapping,
    step::{Cx, RouteBuilder},
    wire::{Reader, Writer},
};
use container::{Container, Format, Kind};
use convert::Scratch;
use light::Light;
use model::World;
use std::{
    collections::{HashMap, HashSet},
    io,
};

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {
    /// What the backend's `login` and `respawn` said about the world.
    pub world: World,
    /// Light that arrived before its column, by column.
    held_light: HashMap<(i32, i32), Light>,
    /// Columns the client has (for steps that need to know).
    loaded: HashSet<(i32, i32)>,
    /// Across 1.12.2 / 1.13: the 1.13 states of beds, banners, skulls and
    /// flower pots by position, for block entity packets that arrive on
    /// their own.
    flattened: HashMap<(i32, i32, i32), i32>,
    scratch: Scratch,
}

/// Whether going from `source` to `target` crosses one of `boundaries`
/// (the first protocol with a change).
fn crosses(source: i32, target: i32, boundaries: &[i32]) -> bool {
    let (older, newer) = (source.min(target), source.max(target));
    boundaries
        .iter()
        .any(|boundary| older < *boundary && newer >= *boundary)
}

fn changed(source: i32, target: i32, registry: &str) -> bool {
    if mapping::try_mapping(source, target, registry).is_none() {
        // Either side lacks the registry: nothing to map, unless the
        // registry exists on one side only (the flattening).
        let has = |protocol: i32| {
            registry == "block_state" || crate::data::version(protocol).registry(registry).is_some()
        };
        return has(source) && has(target);
    }
    !packets::unchanged(source, target, registry)
}

/// Protocols whose chunk packet layout differs from the one before.
const CHUNK_LAYOUTS: &[i32] = &[393, 477, 573, 735, 751, 755, 757, 763, 764, 770, 775, 777];
/// Protocols whose light data layout differs from the one before (or where
/// light moves between packets).
const LIGHT_LAYOUTS: &[i32] = &[477, 735, 755, 757, 763, 777];

/// Whether chunks from `source` need converting for `target`: their
/// layout, block state, biome or block entity IDs or block entity contents
/// differ, or the receiver needs columns tracked.
fn install_needs_chunk(source: i32, target: i32) -> bool {
    crosses(source, target, CHUNK_LAYOUTS)
        || changed(source, target, "block_state")
        || packets::biome_ids(source, target).is_some()
        || (source >= 757 && target >= 757 && changed(source, target, "block_entity_type"))
        || block_entity::changes_content(source, target)
        || container::block_global_bits(source) != container::block_global_bits(target)
        || (source < 768 && target >= 768)
}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(mut builder: RouteBuilder<S>) -> RouteBuilder<S> {
    use Direction::Clientbound;
    use Phase::Play;
    let (source, target) = (builder.backend().protocol, builder.client().protocol);
    let blocks = changed(source, target, "block_state");
    let entity_types =
        source >= 757 && target >= 757 && changed(source, target, "block_entity_type");
    let contents = block_entity::changes_content(source, target);
    let positions = crosses(source, target, &[477]);

    if install_needs_chunk(source, target) {
        builder = builder
            .default_handler(Clientbound, Play, "level_chunk", packets::level_chunk::<S>)
            .default_handler(
                Clientbound,
                Play,
                "level_chunk_with_light",
                packets::level_chunk::<S>,
            );
    }
    if crosses(source, target, LIGHT_LAYOUTS) {
        builder = builder.default_handler(
            Clientbound,
            Play,
            "light_update",
            packets::light_update::<S>,
        );
    }
    // Steps that hold light or follow loaded columns, or need the world.
    let tracks = (source >= 477 && target < 477)
        || (source < 757 && target >= 757)
        || (source < 768 && target >= 768);
    if tracks || source <= 477 || (source == 755 && target == 754) {
        builder = builder
            .tap(Clientbound, Play, "login", packets::tap_world::<S>)
            .tap(Clientbound, Play, "respawn", packets::tap_world::<S>);
    }
    if tracks {
        builder = builder.tap(
            Clientbound,
            Play,
            "forget_level_chunk",
            packets::tap_forget::<S>,
        );
    }
    if crosses(source, target, &[764]) {
        builder = builder.default_handler(
            Clientbound,
            Play,
            "forget_level_chunk",
            packets::forget_level_chunk::<S>,
        );
    }
    if positions || blocks {
        builder =
            builder.default_handler(Clientbound, Play, "block_update", blocks::block_update::<S>);
    }
    if blocks || crosses(source, target, &[751, 755, 763]) {
        for name in ["section_blocks_update", "chunk_blocks_update"] {
            builder = builder.default_handler(
                Clientbound,
                Play,
                name,
                blocks::section_blocks_update::<S>,
            );
        }
    }
    if positions || changed(source, target, "block") {
        builder =
            builder.default_handler(Clientbound, Play, "block_event", blocks::block_event::<S>);
    }
    let records = source < 767 && target < 767 && changed(source, target, "item");
    if positions || blocks || records || crosses(source, target, &[401, 761, 767, 777]) {
        builder =
            builder.default_handler(Clientbound, Play, "level_event", blocks::level_event::<S>);
    }
    if positions || entity_types || contents || crosses(source, target, &[393, 757, 764]) {
        builder = builder.default_handler(
            Clientbound,
            Play,
            "block_entity_data",
            blocks::block_entity_data::<S>,
        );
    }
    let sounds = source >= 765 && changed(source, target, "sound_event");
    let particles = source >= 765 && changed(source, target, "particle_type");
    if crosses(source, target, &[755, 761, 765, 768, 773, 777]) || sounds || particles {
        builder = builder.default_handler(Clientbound, Play, "explode", explode::explode::<S>);
    }
    if crosses(source, target, &[770]) {
        builder = builder.default_handler(Clientbound, Play, "chunks_biomes", chunks_biomes::<S>);
    }
    builder
}

/// `chunks_biomes` (1.19.4 and newer): the biome containers of whole
/// columns. 1.21.5 drops the data length from containers (and pads the
/// buffer like chunk data, see [`model`]).
pub fn chunks_biomes<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: Phase,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let (source, target) = (shared.backend, shared.client);
    let world = shared.chunk.world;
    let mut scratch = std::mem::take(&mut shared.chunk.scratch);
    let mut reader = Reader::new(&packet.data);
    let mut writer = Writer::with_capacity(packet.data.len() + 16);
    let count = reader.count()?;
    writer.varint(count as i32);
    for _ in 0..count {
        writer.i64(reader.i64()?);
        let buffer = reader.byte_array()?;
        let mut inner = Reader::new(buffer);
        let mut out = Writer::with_capacity(buffer.len() + 16);
        let (mut padding, mut written_padding) = (0, 0);
        while !inner.is_empty() {
            if inner.remaining() == padding && inner.peek().iter().all(|byte| *byte == 0) {
                break;
            }
            let mut biomes = Container::read(
                &mut inner,
                source,
                Kind::Biomes,
                world.biome_bits.map(|bits| 1 << bits),
            )?;
            padding += usize::from(source == 770) * if biomes.data.len() < 128 { 1 } else { 2 };
            let bits = if biomes.palette == container::Palette::Direct {
                biomes.bits
            } else {
                0
            };
            let format = Format::biomes(target, world.biome_bits.unwrap_or(bits));
            // INTEGRATION: data-driven biome IDs pass unchanged until the
            // registries codec provides their mapping.
            biomes.convert(None, &format, &mut scratch.values, &mut scratch.raw)?;
            biomes.write(&mut out, target)?;
            if target == 770 {
                written_padding += if biomes.data.len() < 128 { 1 } else { 2 };
            }
        }
        out.buf.resize(out.buf.len() + written_padding, 0);
        writer.byte_array(&out.buf);
    }
    reader.finish()?;
    shared.chunk.scratch = scratch;
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    Ok(())
}
