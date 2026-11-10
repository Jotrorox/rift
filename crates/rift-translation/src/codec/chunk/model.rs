//! One chunk column in any protocol's terms, and its packet layouts:
//! `level_chunk` before 1.18 and `level_chunk_with_light` from 1.18.
//!
//! The layouts, oldest first:
//!
//! - 1.12: x, z (int), full, VarInt section mask, a data buffer of sections
//!   (palette and packed `id << 4 | metadata` entries spanning longs, block
//!   light, sky light where the dimension has it) and 256 biome bytes on
//!   full chunks, then named NBT block entities that carry `id`, `x`, `y`,
//!   `z`.
//! - 1.13: flattened state IDs, no palette before global IDs, biomes as 256
//!   ints.
//! - 1.14: a block count per section, heightmaps as NBT before the buffer,
//!   light moved to `light_update`.
//! - 1.15: biomes move out of the buffer as 1024 ints (4x4x4 cells).
//! - 1.16: entries no longer span longs; an `ignore old data` flag (1.16
//!   and 1.16.1 only).
//! - 1.16.2: biomes are a VarInt array of data-driven IDs.
//! - 1.17: the section mask is a long array, chunks are always full, the
//!   world's height comes from its dimension.
//! - 1.18: every section is present with a paletted biome container, block
//!   entities are compact (packed position, type ID, NBT without `id`
//!   and position) and the light data follows.
//! - 1.19.4 to 1.20: the light data loses its trust edges flag (1.20).
//! - 1.20.2: NBT roots lose their names.
//! - 1.21.5: heightmaps are a list of typed long arrays; containers no
//!   longer send the length of their data.
//! - 26.1: each section also counts its fluid blocks.
//! - 26.3: light masks become byte arrays.
use super::{
    container::{Container, Kind},
    light::{ARRAY, FULL, Light, LightArray},
    storage::Packing,
};
use crate::{
    data,
    nbt::{Compound, Nbt},
    version,
    wire::{Reader, Writer, invalid},
};
use std::io;

/// Heightmap types by their network ID (`Heightmap.Types` order).
pub const HEIGHTMAPS: [&str; 6] = [
    "WORLD_SURFACE_WG",
    "WORLD_SURFACE",
    "OCEAN_FLOOR_WG",
    "OCEAN_FLOOR",
    "MOTION_BLOCKING",
    "MOTION_BLOCKING_NO_LEAVES",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heightmap {
    /// Index into [`HEIGHTMAPS`].
    pub kind: i32,
    /// 256 heights relative to the world's bottom, packed with
    /// `ceil(log2(height + 1))` bits each.
    pub data: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlockEntity {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// The `block_entity_type` ID from 1.18; -1 before, where the NBT names
    /// its type.
    pub kind: i32,
    /// Before 1.18 the full tag including `id`, `x`, `y`, `z`; from 1.18
    /// only the data, or `None` for an empty tag.
    pub nbt: Option<Nbt>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Biomes {
    /// A partial chunk (before 1.17) carries none.
    Absent,
    /// One per block column, `z * 16 + x`: bytes in 1.12, ints in 1.13 and
    /// 1.14.
    Columns(Vec<i32>),
    /// 4x4x4 block cells, `(y * 4 + z) * 4 + x`, bottom up: 1.15 to 1.17.
    Cells(Vec<i32>),
    /// In each section's biome container (1.18 and newer).
    Sections,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// Non-air blocks (1.14 and newer).
    pub block_count: i16,
    /// Blocks holding a fluid (26.1 and newer).
    pub fluid_count: i16,
    pub blocks: Container,
    /// 1.18 and newer.
    pub biomes: Option<Container>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub x: i32,
    pub z: i32,
    /// A whole column rather than an update of some sections (before 1.17).
    pub full: bool,
    /// 1.16 and 1.16.1 only.
    pub ignore_old_data: bool,
    /// The section index of `sections[0]` (world bottom / 16).
    pub min_section: i32,
    /// One per section bottom up; `None` is a section the packet leaves out
    /// (only before 1.18).
    pub sections: Vec<Option<Section>>,
    /// Whether the dimension has sky light (decides the pre-1.14 layout).
    pub sky_light: bool,
    pub biomes: Biomes,
    pub heightmaps: Vec<Heightmap>,
    pub block_entities: Vec<BlockEntity>,
    /// Inside the chunk before 1.14 and from 1.18; `None` otherwise.
    pub light: Option<Light>,
}

/// What a chunk layout leaves to the world it is in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct World {
    /// Whether the dimension has sky light; `None` lets pre-1.14 readers
    /// work it out from the data.
    pub sky_light: Option<bool>,
    /// The lowest section, when known (1.17 and newer).
    pub min_section: Option<i32>,
    /// The bits of the biome registry's global palette (1.18 and newer).
    pub biome_bits: Option<u8>,
}

fn named(protocol: i32) -> bool {
    !version::nameless_nbt(protocol)
}

impl Chunk {
    /// The number of sections the column spans.
    pub fn height_sections(&self) -> usize {
        self.sections.len()
    }

    /// Read a chunk packet body in `protocol`'s layout.
    pub fn read(data: &[u8], protocol: i32, world: &World) -> io::Result<Self> {
        if protocol >= 757 {
            return read_modern(data, protocol, world);
        }
        if protocol < 477 {
            // The data buffer only says whether sections carry sky light
            // by its length; try what the world says first.
            let first = world.sky_light.unwrap_or(true);
            return match read_legacy(data, protocol, first) {
                Ok(chunk) => Ok(chunk),
                Err(error) if world.sky_light.is_none() => {
                    read_legacy(data, protocol, !first).map_err(|_| error)
                }
                Err(error) => Err(error),
            };
        }
        read_middle(data, protocol, world)
    }

    /// Write the chunk in `protocol`'s layout; it must already be in that
    /// protocol's terms (see the conversions).
    pub fn write(&self, protocol: i32) -> io::Result<Vec<u8>> {
        let mut writer = Writer::with_capacity(16 * 1024);
        if protocol >= 757 {
            write_modern(self, protocol, &mut writer)?;
        } else if protocol < 477 {
            write_legacy(self, protocol, &mut writer)?;
        } else {
            write_middle(self, protocol, &mut writer)?;
        }
        Ok(writer.buf)
    }
}

fn block_limit(protocol: i32) -> Option<usize> {
    version::flattened(protocol).then(|| data::version(protocol).block_states.len() as usize)
}

fn read_section_mask(reader: &mut Reader<'_>, protocol: i32) -> io::Result<Vec<u64>> {
    if protocol >= 755 {
        let count = reader.count()?;
        if count.saturating_mul(8) > reader.remaining() {
            return Err(invalid("short packet"));
        }
        (0..count).map(|_| Ok(reader.i64()? as u64)).collect()
    } else {
        Ok(vec![u64::from(reader.varint()? as u32)])
    }
}

fn mask_bit(mask: &[u64], index: usize) -> bool {
    mask.get(index / 64)
        .is_some_and(|word| word >> (index % 64) & 1 == 1)
}

fn mask_len(mask: &[u64]) -> usize {
    mask.iter()
        .enumerate()
        .rev()
        .find(|(_, word)| **word != 0)
        .map_or(0, |(index, word)| {
            index * 64 + 64 - word.leading_zeros() as usize
        })
}

fn write_section_mask(
    writer: &mut Writer,
    protocol: i32,
    sections: &[Option<Section>],
) -> io::Result<()> {
    let mut words = vec![0u64; sections.len().div_ceil(64)];
    for (index, section) in sections.iter().enumerate() {
        if section.is_some() {
            words[index / 64] |= 1 << (index % 64);
        }
    }
    if protocol >= 755 {
        while words.last() == Some(&0) {
            words.pop();
        }
        writer.varint(words.len() as i32);
        for word in words {
            writer.i64(word as i64);
        }
    } else {
        if sections.len() > 16 {
            return Err(invalid("more than 16 sections before 1.17"));
        }
        writer.varint(words.first().copied().unwrap_or(0) as i32);
    }
    Ok(())
}

fn read_nbt_list(reader: &mut Reader<'_>, protocol: i32) -> io::Result<Vec<BlockEntity>> {
    let count = reader.count()?;
    let mut entities = Vec::with_capacity(count);
    for _ in 0..count {
        let nbt =
            Nbt::read(reader, named(protocol))?.ok_or_else(|| invalid("empty block entity tag"))?;
        let tag = nbt
            .as_compound()
            .ok_or_else(|| invalid("block entity tag is not a compound"))?;
        let coordinate = |key| {
            tag.int(key)
                .ok_or_else(|| invalid("block entity tag has no position"))
        };
        entities.push(BlockEntity {
            x: coordinate("x")?,
            y: coordinate("y")?,
            z: coordinate("z")?,
            kind: -1,
            nbt: Some(nbt),
        });
    }
    Ok(entities)
}

fn write_nbt_list(writer: &mut Writer, protocol: i32, entities: &[BlockEntity]) -> io::Result<()> {
    writer.varint(entities.len() as i32);
    for entity in entities {
        let nbt = entity
            .nbt
            .as_ref()
            .filter(|nbt| nbt.as_compound().is_some())
            .ok_or_else(|| invalid("block entity before 1.18 needs a tag"))?;
        nbt.write(writer, named(protocol));
    }
    Ok(())
}

fn read_heightmaps(reader: &mut Reader<'_>, protocol: i32) -> io::Result<Vec<Heightmap>> {
    if protocol >= 770 {
        let count = reader.count()?;
        let mut maps = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = reader.varint()?;
            if usize::try_from(kind).map_or(true, |kind| kind >= HEIGHTMAPS.len()) {
                return Err(invalid("unknown heightmap type"));
            }
            let length = reader.count()?;
            if length.saturating_mul(8) > reader.remaining() {
                return Err(invalid("short packet"));
            }
            let data = (0..length)
                .map(|_| reader.i64())
                .collect::<io::Result<_>>()?;
            maps.push(Heightmap { kind, data });
        }
        return Ok(maps);
    }
    let Some(Nbt::Compound(tag)) = Nbt::read(reader, named(protocol))? else {
        return Err(invalid("heightmaps are not a compound"));
    };
    tag.0
        .into_iter()
        .map(|(name, value)| {
            let kind = HEIGHTMAPS
                .iter()
                .position(|known| *known == name)
                .ok_or_else(|| invalid("unknown heightmap type"))?;
            match value {
                Nbt::LongArray(data) => Ok(Heightmap {
                    kind: kind as i32,
                    data,
                }),
                _ => Err(invalid("heightmap is not a long array")),
            }
        })
        .collect()
}

fn write_heightmaps(writer: &mut Writer, protocol: i32, maps: &[Heightmap]) {
    if protocol >= 770 {
        writer.varint(maps.len() as i32);
        for map in maps {
            writer.varint(map.kind).varint(map.data.len() as i32);
            for long in &map.data {
                writer.i64(*long);
            }
        }
        return;
    }
    let tag: Compound = maps
        .iter()
        .map(|map| {
            (
                HEIGHTMAPS[map.kind as usize].to_owned(),
                Nbt::LongArray(map.data.clone()),
            )
        })
        .collect();
    Nbt::Compound(tag).write(writer, named(protocol));
}

/// 1.12 and 1.13.
fn read_legacy(data: &[u8], protocol: i32, sky_light: bool) -> io::Result<Chunk> {
    let mut reader = Reader::new(data);
    let x = reader.i32()?;
    let z = reader.i32()?;
    let full = reader.bool()?;
    let mask = read_section_mask(&mut reader, protocol)?;
    if mask_len(&mask) > 16 {
        return Err(invalid("section mask beyond 16 sections"));
    }
    let buffer = reader.byte_array()?;
    let mut inner = Reader::new(buffer);
    let limit = block_limit(protocol);
    let mut sections = Vec::with_capacity(16);
    let mut light = Light {
        trust_edges: false,
        sky: vec![LightArray::Absent; 18],
        block: vec![LightArray::Absent; 18],
    };
    for index in 0..16 {
        if !mask_bit(&mask, index) {
            sections.push(None);
            continue;
        }
        let blocks = Container::read(&mut inner, protocol, Kind::Blocks, limit)?;
        light.block[index + 1] = LightArray::Data(inner.take(ARRAY)?.into());
        if sky_light {
            light.sky[index + 1] = LightArray::Data(inner.take(ARRAY)?.into());
        }
        sections.push(Some(Section {
            block_count: 0,
            fluid_count: 0,
            blocks,
            biomes: None,
        }));
    }
    let biomes = if full {
        let biomes = if protocol >= 393 {
            (0..256).map(|_| inner.i32()).collect::<io::Result<_>>()?
        } else {
            inner.take(256)?.iter().map(|id| i32::from(*id)).collect()
        };
        Biomes::Columns(biomes)
    } else {
        Biomes::Absent
    };
    inner.finish()?;
    let block_entities = read_nbt_list(&mut reader, protocol)?;
    reader.finish()?;
    Ok(Chunk {
        x,
        z,
        full,
        ignore_old_data: false,
        min_section: 0,
        sections,
        sky_light,
        biomes,
        heightmaps: Vec::new(),
        block_entities,
        light: Some(light),
    })
}

fn write_legacy(chunk: &Chunk, protocol: i32, writer: &mut Writer) -> io::Result<()> {
    writer.i32(chunk.x).i32(chunk.z).bool(chunk.full);
    write_section_mask(writer, protocol, &chunk.sections)?;
    let mut buffer = Writer::with_capacity(chunk.sections.len() * 6 * 1024 + 1024);
    let empty = Light::default();
    let light = chunk.light.as_ref().unwrap_or(&empty);
    fn array(arrays: &[LightArray], index: usize) -> &LightArray {
        arrays.get(index).unwrap_or(&LightArray::Absent)
    }
    for (index, section) in chunk.sections.iter().enumerate() {
        let Some(section) = section else { continue };
        section.blocks.write(&mut buffer, protocol)?;
        // Without data, blocks are dark and the sky is fully lit.
        buffer.bytes(array(&light.block, index + 1).bytes_or(&super::light::ZEROS));
        if chunk.sky_light {
            buffer.bytes(array(&light.sky, index + 1).bytes_or(&FULL));
        }
    }
    if chunk.full {
        let Biomes::Columns(biomes) = &chunk.biomes else {
            return Err(invalid("full chunk before 1.15 needs column biomes"));
        };
        if biomes.len() != 256 {
            return Err(invalid("column biomes need 256 entries"));
        }
        for biome in biomes {
            if protocol >= 393 {
                buffer.i32(*biome);
            } else {
                buffer.u8(*biome as u8);
            }
        }
    }
    writer.byte_array(&buffer.buf);
    write_nbt_list(writer, protocol, &chunk.block_entities)
}

/// 1.14 to 1.17.
fn read_middle(data: &[u8], protocol: i32, world: &World) -> io::Result<Chunk> {
    let mut reader = Reader::new(data);
    let x = reader.i32()?;
    let z = reader.i32()?;
    let full = protocol >= 755 || reader.bool()?;
    let ignore_old_data = (735..751).contains(&protocol) && reader.bool()?;
    let mask = read_section_mask(&mut reader, protocol)?;
    let heightmaps = read_heightmaps(&mut reader, protocol)?;
    let mut biomes = Biomes::Absent;
    if full && protocol >= 573 {
        let cells = if protocol >= 751 {
            let count = reader.count()?;
            (0..count)
                .map(|_| reader.varint())
                .collect::<io::Result<Vec<_>>>()?
        } else {
            (0..1024)
                .map(|_| reader.i32())
                .collect::<io::Result<Vec<_>>>()?
        };
        biomes = Biomes::Cells(cells);
    }
    // Before 1.17 a column is 16 sections; from 1.17 its 4x4x4 biome cells
    // tell its height (64 per section).
    let count = match &biomes {
        Biomes::Cells(cells) if protocol >= 755 => {
            if cells.is_empty() || cells.len() % 64 != 0 || cells.len() > 64 * 4096 {
                return Err(invalid("biomes do not cover whole sections"));
            }
            cells.len() / 64
        }
        Biomes::Cells(cells) if cells.len() != 1024 => {
            return Err(invalid("a 1.15 to 1.16.5 column has 1024 biome cells"));
        }
        _ => 16,
    };
    if mask_len(&mask) > count {
        return Err(invalid("section mask beyond the column's height"));
    }
    let buffer = reader.byte_array()?;
    let mut inner = Reader::new(buffer);
    let limit = block_limit(protocol);
    let mut sections = Vec::with_capacity(count);
    for index in 0..count {
        if !mask_bit(&mask, index) {
            sections.push(None);
            continue;
        }
        let block_count = inner.i16()?;
        let blocks = Container::read(&mut inner, protocol, Kind::Blocks, limit)?;
        sections.push(Some(Section {
            block_count,
            fluid_count: 0,
            blocks,
            biomes: None,
        }));
    }
    if full && protocol < 573 {
        biomes = Biomes::Columns((0..256).map(|_| inner.i32()).collect::<io::Result<_>>()?);
    }
    inner.finish()?;
    let block_entities = read_nbt_list(&mut reader, protocol)?;
    reader.finish()?;
    Ok(Chunk {
        x,
        z,
        full,
        ignore_old_data,
        min_section: if protocol >= 755 {
            world.min_section.unwrap_or(0)
        } else {
            0
        },
        sections,
        sky_light: world.sky_light.unwrap_or(true),
        biomes,
        heightmaps,
        block_entities,
        light: None,
    })
}

fn write_middle(chunk: &Chunk, protocol: i32, writer: &mut Writer) -> io::Result<()> {
    writer.i32(chunk.x).i32(chunk.z);
    if protocol < 755 {
        writer.bool(chunk.full);
    } else if !chunk.full {
        return Err(invalid("partial chunks end with 1.16.5"));
    }
    if (735..751).contains(&protocol) {
        writer.bool(chunk.ignore_old_data);
    }
    write_section_mask(writer, protocol, &chunk.sections)?;
    write_heightmaps(writer, protocol, &chunk.heightmaps);
    if chunk.full && protocol >= 573 {
        let Biomes::Cells(cells) = &chunk.biomes else {
            return Err(invalid("full chunk from 1.15 needs biome cells"));
        };
        if protocol >= 751 {
            writer.varint(cells.len() as i32);
            for cell in cells {
                writer.varint(*cell);
            }
        } else {
            if cells.len() != 1024 {
                return Err(invalid("a 1.15 to 1.16.1 column has 1024 biome cells"));
            }
            for cell in cells {
                writer.i32(*cell);
            }
        }
    }
    let mut buffer = Writer::with_capacity(chunk.sections.len() * 4 * 1024 + 1024);
    for section in chunk.sections.iter().flatten() {
        buffer.i16(section.block_count);
        section.blocks.write(&mut buffer, protocol)?;
    }
    if chunk.full && protocol < 573 {
        let Biomes::Columns(biomes) = &chunk.biomes else {
            return Err(invalid("full chunk before 1.15 needs column biomes"));
        };
        if biomes.len() != 256 {
            return Err(invalid("column biomes need 256 entries"));
        }
        for biome in biomes {
            buffer.i32(*biome);
        }
    }
    writer.byte_array(&buffer.buf);
    write_nbt_list(writer, protocol, &chunk.block_entities)
}

/// The bits of a direct biome container, as sent by the server whose
/// registry they index; 1.18 sends it before each container.
fn biome_limit(world: &World) -> Option<usize> {
    world.biome_bits.map(|bits| 1usize << bits)
}

/// 1.18 and newer.
fn read_modern(data: &[u8], protocol: i32, world: &World) -> io::Result<Chunk> {
    let mut reader = Reader::new(data);
    let x = reader.i32()?;
    let z = reader.i32()?;
    let heightmaps = read_heightmaps(&mut reader, protocol)?;
    let buffer = reader.byte_array()?;
    let mut inner = Reader::new(buffer);
    let limit = block_limit(protocol);
    let mut sections = Vec::new();
    // Some servers size the buffer larger than what they write and leave
    // zeros at the end (see `padding`); a section of zeros is also a valid
    // empty section, so stop where the rest is exactly that padding.
    let mut padding = 0;
    while !inner.is_empty() {
        if inner.remaining() == padding && inner.peek().iter().all(|byte| *byte == 0) {
            inner.take(padding)?;
            break;
        }
        if sections.len() >= 4096 {
            return Err(invalid("too many sections"));
        }
        let block_count = inner.i16()?;
        let fluid_count = if protocol >= 775 { inner.i16()? } else { 0 };
        let blocks = Container::read(&mut inner, protocol, Kind::Blocks, limit)?;
        let biomes = Container::read(&mut inner, protocol, Kind::Biomes, biome_limit(world))?;
        padding += overcount(protocol, &blocks) + overcount(protocol, &biomes);
        sections.push(Some(Section {
            block_count,
            fluid_count,
            blocks,
            biomes: Some(biomes),
        }));
    }
    let count = reader.count()?;
    let mut block_entities = Vec::with_capacity(count);
    for _ in 0..count {
        let packed = reader.u8()?;
        let y = i32::from(reader.i16()?);
        let kind = reader.varint()?;
        let nbt = Nbt::read(&mut reader, named(protocol))?;
        block_entities.push(BlockEntity {
            x: x.wrapping_mul(16) + i32::from(packed >> 4),
            y,
            z: z.wrapping_mul(16) + i32::from(packed & 15),
            kind,
            nbt,
        });
    }
    let light = Light::read(&mut reader, protocol)?;
    reader.finish()?;
    let min_section = world.min_section.unwrap_or(match sections.len() {
        // The vanilla overworld; other heights are datapack worlds.
        24 => -4,
        _ => 0,
    });
    Ok(Chunk {
        x,
        z,
        full: true,
        ignore_old_data: false,
        min_section,
        sections,
        sky_light: world.sky_light.unwrap_or(true),
        biomes: Biomes::Sections,
        heightmaps,
        block_entities,
        light: Some(light),
    })
}

fn varint_len(value: usize) -> usize {
    match value {
        0..0x80 => 1,
        0x80..0x4000 => 2,
        0x4000..0x20_0000 => 3,
        0x20_0000..0x1000_0000 => 4,
        _ => 5,
    }
}

/// The bytes vanilla reserves for a container beyond what it writes. 1.18
/// to 1.20.1 size the data length VarInt by the entry count rather than the
/// number of longs (a single value sends 0 longs but is sized for 4096);
/// 1.21.5 still counts the length VarInt it no longer sends. The unused
/// tail of the buffer stays zero and clients ignore it.
fn overcount(protocol: i32, container: &Container) -> usize {
    match protocol {
        757..764 => varint_len(container.kind.entries()) - varint_len(container.data.len()),
        770 => varint_len(container.data.len()),
        _ => 0,
    }
}

fn write_modern(chunk: &Chunk, protocol: i32, writer: &mut Writer) -> io::Result<()> {
    writer.i32(chunk.x).i32(chunk.z);
    write_heightmaps(writer, protocol, &chunk.heightmaps);
    let mut buffer = Writer::with_capacity(chunk.sections.len() * 2 * 1024 + 256);
    let mut padding = 0;
    for section in &chunk.sections {
        let section = section
            .as_ref()
            .ok_or_else(|| invalid("1.18 chunks send every section"))?;
        buffer.i16(section.block_count);
        if protocol >= 775 {
            buffer.i16(section.fluid_count);
        }
        section.blocks.write(&mut buffer, protocol)?;
        let biomes = section
            .biomes
            .as_ref()
            .ok_or_else(|| invalid("1.18 sections carry biomes"))?;
        biomes.write(&mut buffer, protocol)?;
        padding += overcount(protocol, &section.blocks) + overcount(protocol, biomes);
    }
    buffer.buf.resize(buffer.buf.len() + padding, 0);
    writer.byte_array(&buffer.buf);
    writer.varint(chunk.block_entities.len() as i32);
    for entity in &chunk.block_entities {
        if entity.x >> 4 != chunk.x || entity.z >> 4 != chunk.z {
            return Err(invalid("block entity outside its chunk"));
        }
        let y = i16::try_from(entity.y).map_err(|_| invalid("block entity height out of range"))?;
        writer
            .u8(((entity.x & 15) << 4 | (entity.z & 15)) as u8)
            .i16(y)
            .varint(entity.kind);
        Nbt::write_optional(entity.nbt.as_ref(), writer, named(protocol));
    }
    match &chunk.light {
        Some(light) => light.write(writer, protocol),
        None => Light::default().write(writer, protocol),
    }
}

/// The packing heightmaps use in `protocol`, with the bits for a column of
/// `sections` sections.
pub fn heightmap_format(protocol: i32, sections: usize) -> (u8, Packing) {
    (
        super::storage::ceil_log2(sections * 16 + 1),
        Packing::of(protocol),
    )
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::codec::chunk::{
        container::Format,
        convert::{self, Plan, Scratch},
    };

    /// A column of `protocol` with a mix of palette kinds: an indirect
    /// section, a direct one, an empty one and (before 1.18) a gap.
    pub fn sample(protocol: i32) -> Chunk {
        let mut scratch = Vec::new();
        let format = Format::blocks(protocol);
        let limit: i32 = if version::flattened(protocol) {
            data::version(protocol).block_states.len()
        } else {
            4096
        };
        let count = if protocol >= 755 { 24 } else { 16 };
        let min_section = if protocol >= 755 { -4 } else { 0 };
        let mut sections = Vec::new();
        for index in 0..count {
            let values: Vec<i32> = match index % 4 {
                0 => (0..4096).map(|entry| (entry % 7) * 16).collect(),
                1 => (0..4096).map(|entry| (entry * 5) % limit).collect(),
                2 if protocol < 757 => {
                    sections.push(None);
                    continue;
                }
                _ => vec![0; 4096],
            };
            let blocks = Container::from_values(&values, &format, &mut scratch);
            let biomes = (protocol >= 757).then(|| {
                let cells: Vec<i32> = (0..64).map(|cell| cell % (1 + index as i32 % 3)).collect();
                Container::from_values(&cells, &Format::biomes(protocol, 6), &mut scratch)
            });
            let block_count = values.iter().filter(|value| **value != 0).count() as i16;
            sections.push(Some(Section {
                block_count: if protocol >= 477 { block_count } else { 0 },
                fluid_count: 0,
                blocks,
                biomes,
            }));
        }
        let biomes = match protocol {
            ..573 => Biomes::Columns((0..256).map(|column| column % 40).collect()),
            573..757 => Biomes::Cells((0..count * 64).map(|cell| (cell % 23) as i32).collect()),
            _ => Biomes::Sections,
        };
        let (bits, packing) = heightmap_format(protocol, count);
        let heights: Vec<u32> = (0..256).map(|column| column as u32 % 300).collect();
        let mut packed = Vec::new();
        super::super::storage::pack(&heights, bits, packing, &mut packed);
        let heightmaps = if protocol >= 477 {
            vec![Heightmap {
                kind: 4,
                data: packed.into_iter().map(|long| long as i64).collect(),
            }]
        } else {
            Vec::new()
        };
        let mut tag = Compound::new();
        tag.insert("Text1", "{\"text\":\"hi\"}");
        let entity = if protocol >= 757 {
            BlockEntity {
                x: -31,
                y: 60,
                z: 17,
                kind: data::version(protocol)
                    .expect_registry("block_entity_type")
                    .id("sign")
                    .unwrap(),
                nbt: Some(Nbt::Compound(tag)),
            }
        } else {
            BlockEntity {
                x: -31,
                y: 60,
                z: 17,
                kind: -1,
                nbt: Some(Nbt::Compound(convert::with_metadata(
                    Some(Nbt::Compound(tag)),
                    "sign",
                    -31,
                    60,
                    17,
                ))),
            }
        };
        let light = (!(477..757).contains(&protocol)).then(|| {
            let mut light = Light::default();
            for (index, section) in std::iter::once(&None)
                .chain(&sections)
                .chain(std::iter::once(&None))
                .enumerate()
            {
                let present = section.is_some();
                light.block.push(if present || protocol >= 757 {
                    LightArray::Data((0..ARRAY).map(|byte| (byte + index) as u8).collect())
                } else {
                    LightArray::Absent
                });
                light.sky.push(if present || protocol >= 757 {
                    LightArray::filled(15)
                } else {
                    LightArray::Absent
                });
            }
            light.trust_edges = Light::has_trust_edges(protocol);
            light
        });
        Chunk {
            x: -2,
            z: 1,
            full: true,
            ignore_old_data: (735..751).contains(&protocol),
            min_section,
            sections,
            sky_light: true,
            biomes,
            heightmaps,
            block_entities: vec![entity],
            light,
        }
    }

    fn world(protocol: i32) -> World {
        World {
            sky_light: Some(true),
            min_section: (protocol >= 755).then_some(-4),
            biome_bits: None,
        }
    }

    #[test]
    fn synthetic_columns_round_trip_in_every_layout() {
        for &(protocol, release) in crate::version::PROTOCOLS {
            let chunk = sample(protocol);
            let bytes = chunk
                .write(protocol)
                .unwrap_or_else(|error| panic!("{release}: {error}"));
            let back = Chunk::read(&bytes, protocol, &world(protocol))
                .unwrap_or_else(|error| panic!("{release}: {error}"));
            assert_eq!(back, chunk, "{release}");
            // Inferring sky light works too before 1.14.
            if protocol < 477 {
                let inferred = Chunk::read(&bytes, protocol, &World::default()).unwrap();
                assert!(inferred.sky_light);
                let mut dark = chunk.clone();
                dark.sky_light = false;
                let bytes = dark.write(protocol).unwrap();
                assert!(
                    !Chunk::read(&bytes, protocol, &World::default())
                        .unwrap()
                        .sky_light
                );
            }
        }
    }

    #[test]
    fn synthetic_columns_convert_through_the_whole_chain() {
        // Up from 1.13 to 26.3 and back down, checking each step parses.
        let protocols: Vec<i32> = crate::version::PROTOCOLS
            .iter()
            .map(|(protocol, _)| *protocol)
            .filter(|protocol| *protocol >= 393)
            .collect();
        let mut scratch = Scratch::default();
        for direction in [false, true] {
            let order: Vec<i32> = if direction {
                protocols.iter().rev().copied().collect()
            } else {
                protocols.clone()
            };
            let mut chunk = sample(order[0]);
            for pair in order.windows(2) {
                let (source, target) = (pair[0], pair[1]);
                let blocks = crate::mapping::try_mapping(source, target, "block_state");
                let plan = Plan {
                    source,
                    target,
                    blocks,
                    block_fallback: 1,
                    biomes: None,
                    block_entity_types: crate::mapping::try_mapping(
                        source,
                        target,
                        "block_entity_type",
                    )
                    .filter(|_| source >= 757 && target >= 757),
                    world: World {
                        sky_light: Some(true),
                        min_section: None,
                        biome_bits: None,
                    },
                };
                convert::convert(&mut chunk, &plan, &mut scratch)
                    .unwrap_or_else(|error| panic!("{source} -> {target}: {error}"));
                if (477..757).contains(&target) {
                    chunk.light = None; // The handler sends it on its own.
                }
                let bytes = chunk
                    .write(target)
                    .unwrap_or_else(|error| panic!("{source} -> {target}: {error}"));
                let world = World {
                    sky_light: Some(true),
                    min_section: Some(chunk.min_section),
                    biome_bits: None,
                };
                let back = Chunk::read(&bytes, target, &world)
                    .unwrap_or_else(|error| panic!("{source} -> {target}: {error}"));
                assert_eq!(back.write(target).unwrap(), bytes, "{source} -> {target}");
                assert_eq!(back.block_entities.len(), 1, "{source} -> {target}");
                chunk = back;
            }
        }
    }

    #[test]
    fn malformed_columns_fail_without_panicking() {
        for protocol in [340, 404, 498, 578, 736, 754, 756, 757, 763, 770, 777] {
            let bytes = sample(protocol).write(protocol).unwrap();
            for cut in (0..bytes.len()).step_by(97).chain([bytes.len() - 1]) {
                assert!(
                    Chunk::read(&bytes[..cut], protocol, &world(protocol)).is_err(),
                    "{protocol} cut at {cut}"
                );
            }
            let mut extended = bytes.clone();
            extended.push(0);
            assert!(
                Chunk::read(&extended, protocol, &world(protocol)).is_err(),
                "{protocol} trailing byte"
            );
            // Flip bytes everywhere: errors are fine, panics are not.
            for index in (0..bytes.len()).step_by(13) {
                let mut corrupt = bytes.clone();
                corrupt[index] ^= 0xa5;
                let _ = Chunk::read(&corrupt, protocol, &world(protocol));
            }
        }
        // A section count VarInt claiming a huge buffer.
        let huge = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0xff, 0xff, 0xff, 0xff, 0x07];
        assert!(Chunk::read(&huge, 340, &World::default()).is_err());
    }
}
