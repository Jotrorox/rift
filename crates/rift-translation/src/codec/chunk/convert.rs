//! Converting a [`Chunk`] read in one protocol into the terms of an
//! adjacent one: block state and biome IDs, container formats, biome
//! layouts, where light lives, heightmaps, block entity lists and the
//! world's height.
//!
//! What this does at each boundary (older -> newer, and back):
//!
//! - 1.12 / 1.13: state IDs through the flattening mapping, 13 / 14-bit
//!   global palettes, biome bytes / ints.
//! - 1.13 / 1.14: up, block counts and heightmaps are computed and light
//!   leaves the chunk (the packet handler sends it as a `light_update`):
//!   absent sections of a full column get dark block light and a fully lit
//!   sky. Down, the handler hands in the light it held back for the column;
//!   whatever is still missing is dark for blocks and fully lit for the sky.
//!   Heightmaps are dropped.
//! - 1.14 / 1.15: 256 column biomes become 4x4x4 cells (the column at each
//!   cell's centre, for every height) and back (the cell at sea level).
//! - 1.15 / 1.16: entries are repacked without spanning longs, heightmaps
//!   too.
//! - 1.16.5 / 1.17: a 1.17 world may be taller than 0 to 255: going down
//!   the column is cut to that range, going up it is placed in the 1.17
//!   world's range.
//! - 1.17 / 1.18: missing sections become empty ones and biome cells move
//!   into per-section containers, and back. The handler merges and splits
//!   the light.
//! - 1.21.5, 26.1, 26.3: layout only, plus the fluid count of 26.1.
use super::{
    container::{Container, Format, Kind, Palette},
    flags,
    light::{Light, LightArray},
    model::{Biomes, BlockEntity, Chunk, Heightmap, Section, World},
    storage::{self, Packing},
};
use crate::{
    data,
    mapping::Mapping,
    nbt::{Compound, Nbt},
    version,
    wire::invalid,
};
use std::io;

/// Reused buffers, so converting a chunk allocates little.
#[derive(Default)]
pub struct Scratch {
    pub values: Vec<i32>,
    pub raw: Vec<u32>,
}

/// Everything a conversion needs besides the chunk.
pub struct Plan<'a> {
    pub source: i32,
    pub target: i32,
    /// Block state IDs, sender's to receiver's; `None` keeps them.
    pub blocks: Option<&'a Mapping>,
    /// For states without a counterpart.
    pub block_fallback: i32,
    /// Biome IDs, sender's to receiver's; `None` keeps them.
    pub biomes: Option<&'a dyn Fn(i32) -> i32>,
    /// `block_entity_type` IDs (1.18 and newer on both sides).
    pub block_entity_types: Option<&'a Mapping>,
    /// The world as the receiver sees it.
    pub world: World,
}

impl Plan<'_> {
    fn map_block(&self, id: i32) -> i32 {
        match self.blocks {
            Some(mapping) => mapping.get(id).unwrap_or(self.block_fallback),
            None => id,
        }
    }

    fn map_biome(&self, id: i32) -> i32 {
        self.biomes.map_or(id, |map| map(id))
    }
}

/// The receiver's sections, `(min_section, count)`, for a column of the
/// sender's.
pub fn target_height(chunk: &Chunk, plan: &Plan<'_>) -> (i32, usize) {
    if plan.target < 755 {
        (0, 16)
    } else if plan.source < 755 {
        // A 1.16 column in a 1.17 world: as tall as the world, if known.
        (plan.world.min_section.unwrap_or(0), 16)
    } else {
        (
            plan.world.min_section.unwrap_or(chunk.min_section),
            chunk.sections.len(),
        )
    }
}

/// Convert `chunk` from `plan.source`'s terms into `plan.target`'s.
pub fn convert(chunk: &mut Chunk, plan: &Plan<'_>, scratch: &mut Scratch) -> io::Result<()> {
    let (source, target) = (plan.source, plan.target);
    let (min_section, count) = target_height(chunk, plan);
    if (min_section, count) != (chunk.min_section, chunk.sections.len()) {
        rebase(chunk, min_section, count, target, scratch)?;
    }
    convert_sections(chunk, plan, scratch)?;
    convert_biomes(chunk, plan, scratch)?;
    if target < 757 && source >= 757 {
        // Before 1.18 a section without blocks is left out.
        for section in chunk.sections.iter_mut() {
            if section
                .as_ref()
                .is_some_and(|section| section.block_count == 0)
            {
                *section = None;
            }
        }
    }
    convert_light(chunk, plan);
    convert_heightmaps(chunk, plan, scratch)?;
    convert_block_entities(chunk, plan)?;
    if target < 755 {
        chunk.ignore_old_data = if (735..751).contains(&source) {
            chunk.ignore_old_data
        } else {
            chunk.full
        };
    } else {
        chunk.full = true;
        chunk.ignore_old_data = false;
    }
    if let Some(sky) = plan.world.sky_light {
        chunk.sky_light = sky;
    }
    Ok(())
}

fn convert_sections(chunk: &mut Chunk, plan: &Plan<'_>, scratch: &mut Scratch) -> io::Result<()> {
    let (source, target) = (plan.source, plan.target);
    let format = Format::blocks(target);
    let map = |id: i32| plan.map_block(id);
    let map: Option<&dyn Fn(i32) -> i32> = plan.blocks.map(|_| &map as _);
    let (source_flags, target_flags) = (
        version::flattened(source).then(|| flags::of(source)),
        version::flattened(target).then(|| flags::of(target)),
    );
    if target >= 757 {
        // Every section is present from 1.18 on.
        for section in chunk
            .sections
            .iter_mut()
            .filter(|section| section.is_none())
        {
            *section = Some(Section {
                block_count: 0,
                fluid_count: 0,
                blocks: Container::single(Kind::Blocks, 0),
                biomes: None,
            });
        }
    }
    for section in chunk.sections.iter_mut().flatten() {
        // Counts survive unless a palette entry changes kind.
        let changes = |flag: u8| match (&section.blocks.palette, source_flags, target_flags) {
            (_, None, _) | (_, _, None) | (Palette::Direct, ..) => true,
            (Palette::Single(value), Some(from), Some(to)) => {
                flags::has(from, *value, flag) != flags::has(to, plan.map_block(*value), flag)
            }
            (Palette::Indirect(palette), Some(from), Some(to)) => palette.iter().any(|value| {
                flags::has(from, *value, flag) != flags::has(to, plan.map_block(*value), flag)
            }),
        };
        let recount =
            target >= 477 && (source < 477 || (plan.blocks.is_some() && changes(flags::AIR)));
        let refluid =
            target >= 775 && (source < 775 || (plan.blocks.is_some() && changes(flags::FLUID)));
        section
            .blocks
            .convert(map, &format, &mut scratch.values, &mut scratch.raw)?;
        if let Some(table) = target_flags {
            if recount {
                let air = section
                    .blocks
                    .count(|id| flags::has(table, id, flags::AIR), &mut scratch.raw)?;
                section.block_count = (4096 - air) as i16;
            }
            if refluid {
                section.fluid_count = section
                    .blocks
                    .count(|id| flags::has(table, id, flags::FLUID), &mut scratch.raw)?
                    as i16;
            }
        }
        if target < 775 {
            section.fluid_count = 0;
        }
        if target < 477 {
            section.block_count = 0;
        }
    }
    Ok(())
}

/// Keep at most `limit` distinct values, replacing the rarest with the
/// most common; for biome containers whose registry width is unknown.
fn limit_distinct(values: &mut [i32], limit: usize) {
    let mut counts: Vec<(i32, usize)> = Vec::new();
    for value in values.iter() {
        match counts.iter_mut().find(|(known, _)| known == value) {
            Some((_, count)) => *count += 1,
            None => counts.push((*value, 1)),
        }
    }
    if counts.len() <= limit {
        return;
    }
    counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    let keep: Vec<i32> = counts[..limit].iter().map(|(value, _)| *value).collect();
    for value in values.iter_mut() {
        if !keep.contains(value) {
            *value = keep[0];
        }
    }
}

fn biome_format(target: i32, world: &World, fallback: u8) -> Format {
    Format::biomes(target, world.biome_bits.unwrap_or(fallback))
}

fn convert_biomes(chunk: &mut Chunk, plan: &Plan<'_>, scratch: &mut Scratch) -> io::Result<()> {
    let target = plan.target;
    let biomes = std::mem::replace(&mut chunk.biomes, Biomes::Absent);
    chunk.biomes = match biomes {
        Biomes::Absent => {
            if target >= 757 {
                return Err(invalid("a 1.18 column needs biomes"));
            }
            Biomes::Absent
        }
        Biomes::Sections => {
            if target >= 757 {
                for section in chunk.sections.iter_mut().flatten() {
                    let biomes = section
                        .biomes
                        .as_mut()
                        .ok_or_else(|| invalid("section without biomes"))?;
                    let bits = if biomes.palette == Palette::Direct {
                        biomes.bits
                    } else {
                        0
                    };
                    let format = biome_format(target, &plan.world, bits);
                    let map = |id: i32| plan.map_biome(id);
                    let map: Option<&dyn Fn(i32) -> i32> = plan.biomes.map(|_| &map as _);
                    biomes.convert(map, &format, &mut scratch.values, &mut scratch.raw)?;
                }
                Biomes::Sections
            } else {
                let mut cells = Vec::with_capacity(chunk.sections.len() * 64);
                for section in &chunk.sections {
                    match section.as_ref().and_then(|section| section.biomes.as_ref()) {
                        Some(biomes) => {
                            biomes.values(&mut scratch.values, &mut scratch.raw)?;
                            cells.extend(scratch.values.iter().map(|id| plan.map_biome(*id)));
                        }
                        None => return Err(invalid("section without biomes")),
                    }
                }
                for section in chunk.sections.iter_mut().flatten() {
                    section.biomes = None;
                }
                cells_to(cells, target)
            }
        }
        Biomes::Cells(cells) => {
            let cells: Vec<i32> = cells.into_iter().map(|id| plan.map_biome(id)).collect();
            if target >= 757 {
                if cells.len() != chunk.sections.len() * 64 {
                    return Err(invalid("biome cells do not match the column's height"));
                }
                for (section, cells) in chunk.sections.iter_mut().zip(cells.chunks(64)) {
                    let section = section.as_mut().ok_or_else(|| invalid("missing section"))?;
                    let mut values = cells.to_vec();
                    let format = match plan.world.biome_bits {
                        Some(bits) => Format::biomes(target, bits),
                        None => {
                            limit_distinct(&mut values, 8);
                            Format::biomes(target, 3)
                        }
                    };
                    section.biomes =
                        Some(Container::from_values(&values, &format, &mut scratch.raw));
                }
                Biomes::Sections
            } else {
                cells_to(cells, target)
            }
        }
        Biomes::Columns(columns) => {
            let columns: Vec<i32> = columns.into_iter().map(|id| plan.map_biome(id)).collect();
            if target < 573 {
                Biomes::Columns(columns)
            } else {
                // Each 4x4 cell takes the column at its centre, at every height.
                let layer: Vec<i32> = (0..16)
                    .map(|cell| columns[((cell / 4) * 4 + 2) * 16 + (cell % 4) * 4 + 2])
                    .collect();
                let layers = if target >= 755 {
                    chunk.sections.len() * 4
                } else {
                    64
                };
                Biomes::Cells(layer.iter().copied().cycle().take(layers * 16).collect())
            }
        }
    };
    if target < 393
        && let Biomes::Columns(columns) = &mut chunk.biomes
    {
        for id in columns.iter_mut() {
            *id = (*id).clamp(0, 255);
        }
    }
    Ok(())
}

/// Biome cells (bottom up) in the layout of a pre-1.18 `target`.
fn cells_to(cells: Vec<i32>, target: i32) -> Biomes {
    if target >= 573 {
        return Biomes::Cells(cells);
    }
    // Each column takes the cell it is in at sea level (y 64).
    let layers = cells.len() / 16;
    let layer = 16.min(layers.saturating_sub(1));
    Biomes::Columns(
        (0..256)
            .map(|column| {
                let (x, z) = (column % 16, column / 16);
                cells
                    .get(layer * 16 + (z / 4) * 4 + x / 4)
                    .copied()
                    .unwrap_or(0)
            })
            .collect(),
    )
}

fn convert_light(chunk: &mut Chunk, plan: &Plan<'_>) {
    let (source, target) = (plan.source, plan.target);
    if source < 477 && target >= 477 {
        let mut light = chunk.light.take().unwrap_or_default();
        let count = chunk.sections.len() + 2;
        light.sky.resize(count, LightArray::Absent);
        light.block.resize(count, LightArray::Absent);
        if chunk.full {
            for index in 0..count {
                let present = index > 0
                    && index <= chunk.sections.len()
                    && chunk.sections[index - 1].is_some();
                if present {
                    continue;
                }
                light.block[index] = LightArray::Empty;
                if chunk.sky_light {
                    // Below the world it is dark; above the terrain the sky
                    // shines in.
                    light.sky[index] = if index == 0 {
                        LightArray::Empty
                    } else {
                        LightArray::filled(15)
                    };
                }
            }
        }
        if !chunk.sky_light {
            light
                .sky
                .iter_mut()
                .for_each(|array| *array = LightArray::Absent);
        }
        light.trust_edges = Light::has_trust_edges(target);
        chunk.light = Some(light);
    }
    if let Some(light) = &mut chunk.light {
        if Light::has_trust_edges(target) && !Light::has_trust_edges(source) {
            light.trust_edges = true;
        }
        if !Light::has_trust_edges(target) {
            light.trust_edges = false;
        }
    }
}

fn convert_heightmaps(chunk: &mut Chunk, plan: &Plan<'_>, scratch: &mut Scratch) -> io::Result<()> {
    let (source, target) = (plan.source, plan.target);
    if target < 477 {
        chunk.heightmaps.clear();
        return Ok(());
    }
    if source < 477 {
        chunk.heightmaps = compute_heightmaps(chunk, target, scratch)?;
        return Ok(());
    }
    let sections = chunk.sections.len();
    let (from_bits, from_packing) = (storage::ceil_log2(sections * 16 + 1), Packing::of(source));
    let (to_bits, to_packing) = (from_bits, Packing::of(target));
    if from_packing == to_packing {
        return Ok(());
    }
    for map in &mut chunk.heightmaps {
        repack_heightmap(
            map,
            from_bits,
            from_packing,
            to_bits,
            to_packing,
            0,
            sections * 16,
            scratch,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn repack_heightmap(
    map: &mut Heightmap,
    from_bits: u8,
    from_packing: Packing,
    to_bits: u8,
    to_packing: Packing,
    shift: i32,
    height: usize,
    scratch: &mut Scratch,
) -> io::Result<()> {
    let data: Vec<u64> = map.data.iter().map(|long| *long as u64).collect();
    scratch.raw.resize(256, 0);
    storage::unpack(&data, from_bits, from_packing, &mut scratch.raw)?;
    for value in scratch.raw.iter_mut() {
        *value = (*value as i32 + shift).clamp(0, height as i32) as u32;
    }
    let mut packed = Vec::new();
    storage::pack(&scratch.raw, to_bits, to_packing, &mut packed);
    map.data = packed.into_iter().map(|long| long as i64).collect();
    Ok(())
}

/// `WORLD_SURFACE` and `MOTION_BLOCKING` for a column that had none (1.13
/// going to 1.14): one above the highest non-air, and the highest solid or
/// fluid, block of each column.
fn compute_heightmaps(
    chunk: &Chunk,
    target: i32,
    scratch: &mut Scratch,
) -> io::Result<Vec<Heightmap>> {
    let table = flags::of(target);
    let mut surface = [0u32; 256];
    let mut motion = [0u32; 256];
    let (mut surface_left, mut motion_left) = (256, 256);
    for (index, section) in chunk.sections.iter().enumerate().rev() {
        let Some(section) = section else { continue };
        if surface_left == 0 && motion_left == 0 {
            break;
        }
        section
            .blocks
            .values(&mut scratch.values, &mut scratch.raw)?;
        for y in (0..16).rev() {
            for column in 0..256 {
                let id = scratch.values[y * 256 + column];
                let height = (index * 16 + y + 1) as u32;
                if surface[column] == 0 && !flags::has(table, id, flags::AIR) {
                    surface[column] = height;
                    surface_left -= 1;
                }
                if motion[column] == 0 && flags::has(table, id, flags::MOTION) {
                    motion[column] = height;
                    motion_left -= 1;
                }
            }
        }
    }
    let (bits, packing) = (
        storage::ceil_log2(chunk.sections.len() * 16 + 1),
        Packing::of(target),
    );
    let pack = |values: &[u32]| -> Vec<i64> {
        let mut packed = Vec::new();
        storage::pack(values, bits, packing, &mut packed);
        packed.into_iter().map(|long| long as i64).collect()
    };
    Ok(vec![
        Heightmap {
            kind: 4,
            data: pack(&motion),
        },
        Heightmap {
            kind: 1,
            data: pack(&surface),
        },
    ])
}

fn convert_block_entities(chunk: &mut Chunk, plan: &Plan<'_>) -> io::Result<()> {
    let (source, target) = (plan.source, plan.target);
    if source < 757 && target >= 757 {
        let registry = data::version(target).expect_registry("block_entity_type");
        let mut converted = Vec::with_capacity(chunk.block_entities.len());
        for entity in chunk.block_entities.drain(..) {
            let Some(Nbt::Compound(mut tag)) = entity.nbt else {
                continue;
            };
            let Some(kind) = tag.str("id").and_then(|id| registry.id(id)) else {
                continue; // A type the receiver does not know.
            };
            for key in ["id", "x", "y", "z"] {
                tag.remove(key);
            }
            converted.push(BlockEntity {
                kind,
                nbt: Some(Nbt::Compound(tag)),
                ..entity
            });
        }
        chunk.block_entities = converted;
    } else if source >= 757 && target < 757 {
        let registry = data::version(source).expect_registry("block_entity_type");
        let mut converted = Vec::with_capacity(chunk.block_entities.len());
        for entity in chunk.block_entities.drain(..) {
            let Some(name) = registry.name(entity.kind) else {
                continue;
            };
            converted.push(BlockEntity {
                kind: -1,
                nbt: Some(Nbt::Compound(with_metadata(
                    entity.nbt, name, entity.x, entity.y, entity.z,
                ))),
                ..entity
            });
        }
        chunk.block_entities = converted;
    } else if let Some(mapping) = plan.block_entity_types {
        chunk
            .block_entities
            .retain_mut(|entity| match mapping.get(entity.kind) {
                Some(kind) => {
                    entity.kind = kind;
                    true
                }
                None => false,
            });
    }
    Ok(())
}

/// A block entity's data with the `id` and position older layouts carry
/// in front.
pub fn with_metadata(nbt: Option<Nbt>, name: &str, x: i32, y: i32, z: i32) -> Compound {
    let mut tag = Compound::new();
    tag.insert("id", format!("minecraft:{name}"));
    tag.insert("x", x);
    tag.insert("y", y);
    tag.insert("z", z);
    if let Some(Nbt::Compound(data)) = nbt {
        for (key, value) in data.0 {
            if !matches!(key.as_str(), "id" | "x" | "y" | "z") {
                tag.0.push((key, value));
            }
        }
    }
    tag
}

/// Move the column to the sections `min_section .. min_section + count`:
/// what falls outside is dropped, what is new is left out (or empty).
fn rebase(
    chunk: &mut Chunk,
    min_section: i32,
    count: usize,
    target: i32,
    scratch: &mut Scratch,
) -> io::Result<()> {
    let old_min = chunk.min_section;
    let old_count = chunk.sections.len();
    let shift = old_min - min_section; // New index of old section 0.
    let moved = |index: usize| -> Option<usize> {
        let new = index as i32 + shift;
        (0..count as i32).contains(&new).then_some(new as usize)
    };
    let mut sections = vec![None; count];
    for (index, section) in chunk.sections.drain(..).enumerate() {
        if let Some(new) = moved(index) {
            sections[new] = section;
        }
    }
    chunk.sections = sections;
    if let Biomes::Cells(cells) = &mut chunk.biomes {
        let old = std::mem::take(cells);
        let layers = old.len() / 16;
        let per_section = layers / old_count.max(1);
        let mut new = Vec::with_capacity(count * per_section * 16);
        for section in 0..count as i32 {
            let source = (section - shift).clamp(0, old_count as i32 - 1) as usize;
            new.extend_from_slice(&old[source * per_section * 16..(source + 1) * per_section * 16]);
        }
        *cells = new;
    }
    if let Some(light) = &mut chunk.light {
        for arrays in [&mut light.sky, &mut light.block] {
            let old = std::mem::take(arrays);
            let mut new = vec![LightArray::Absent; count + 2];
            for (index, array) in old.into_iter().enumerate() {
                let index = index as i32 + shift;
                if (0..count as i32 + 2).contains(&index) {
                    new[index as usize] = array;
                }
            }
            *arrays = new;
        }
    }
    let source_bits = storage::ceil_log2(old_count * 16 + 1);
    let target_bits = storage::ceil_log2(count * 16 + 1);
    let packing = Packing::Padded; // Both sides of 1.16.5 / 1.17 pad.
    for map in &mut chunk.heightmaps {
        repack_heightmap(
            map,
            source_bits,
            packing,
            target_bits,
            packing,
            shift * 16,
            count * 16,
            scratch,
        )?;
    }
    let (bottom, top) = (min_section * 16, (min_section + count as i32) * 16);
    chunk
        .block_entities
        .retain(|entity| (bottom..top).contains(&entity.y));
    chunk.min_section = min_section;
    let _ = target;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiting_distinct_values_keeps_the_most_common() {
        let mut values: Vec<i32> = (0..64)
            .map(|index| if index < 40 { 1 } else { index })
            .collect();
        limit_distinct(&mut values, 8);
        let mut distinct = values.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), 8);
        assert_eq!(values.iter().filter(|value| **value == 1).count(), 40 + 17);
    }

    #[test]
    fn column_and_cell_biomes_convert_both_ways() {
        let columns: Vec<i32> = (0..256)
            .map(|column| column % 16 / 4 + column / 64 * 4)
            .collect();
        let layer: Vec<i32> = (0..16).collect();
        let cells: Vec<i32> = layer.iter().copied().cycle().take(1024).collect();
        let Biomes::Columns(back) = cells_to(cells, 498) else {
            panic!()
        };
        assert_eq!(back, columns);
    }
}
