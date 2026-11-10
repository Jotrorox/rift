//! The chunk packet of 1.12 and 1.13: column position, "full chunk" flag,
//! a bit mask of the sections sent, a length-prefixed data blob and the
//! block entities. The blob holds each section (bits per block, palette,
//! packed states that may span longs, block light, sky light where the
//! dimension has a sky) and, for full chunks, the biomes: 256 bytes in 1.12,
//! 256 ints in 1.13.
//!
//! Sections stay as received unless something changes them, so a column the
//! step leaves alone is written back byte for byte.
use crate::wire::{Reader, Writer, invalid};
use std::io;

pub const SECTION_BLOCKS: usize = 4096;
const LIGHT: usize = 2048;

/// The global palette's width, and the palette protocol differences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// 1.12: states are `id << 4 | meta` (13 bits) and the global palette
    /// still writes an empty palette length.
    Legacy,
    /// 1.13: flattened state IDs (14 bits), no palette length when global.
    Flattened,
}

impl Format {
    fn global_bits(self) -> u8 {
        match self {
            Self::Legacy => 13,
            Self::Flattened => 14,
        }
    }

    fn biome_bytes(self) -> usize {
        match self {
            Self::Legacy => 256,
            Self::Flattened => 1024,
        }
    }
}

/// One 16x16x16 section: its states, and its light as received.
#[derive(Clone, Debug)]
pub struct Section {
    /// Indexed `y << 8 | z << 4 | x`.
    pub states: Box<[u16; SECTION_BLOCKS]>,
    /// Block light, then sky light where the dimension has one.
    pub light: Vec<u8>,
    /// The section as received, while nothing changed it.
    raw: Option<Vec<u8>>,
}

impl Section {
    pub fn get(&self, index: usize) -> u16 {
        self.states[index]
    }

    pub fn set(&mut self, index: usize, state: u16) {
        if self.states[index] != state {
            self.states[index] = state;
            self.raw = None;
        }
    }

    fn read(reader: &mut Reader<'_>, format: Format, sky: bool) -> io::Result<Self> {
        let start = reader.peek();
        let declared = reader.u8()?;
        let global = declared > 8;
        let bits = if global {
            format.global_bits()
        } else {
            declared.max(4)
        };
        let mut palette = Vec::new();
        if !global || format == Format::Legacy {
            let length = reader.count()?;
            if global && length != 0 {
                return Err(invalid("global palette with entries"));
            }
            if length > 1 << bits {
                return Err(invalid("palette longer than its bits allow"));
            }
            palette.reserve(length);
            for _ in 0..length {
                let state = reader.varint()?;
                palette
                    .push(u16::try_from(state).map_err(|_| invalid("palette state out of range"))?);
            }
        }
        let longs = reader.count()?;
        if longs != SECTION_BLOCKS * usize::from(bits) / 64 {
            return Err(invalid("section data length does not match its bits"));
        }
        let mut data = Vec::with_capacity(longs);
        for _ in 0..longs {
            data.push(reader.i64()? as u64);
        }
        let mut states = Box::new([0u16; SECTION_BLOCKS]);
        let mask = (1u64 << bits) - 1;
        let bits = usize::from(bits);
        for (index, state) in states.iter_mut().enumerate() {
            let bit = index * bits;
            let (word, offset) = (bit / 64, bit % 64);
            let mut value = data[word] >> offset;
            if offset + bits > 64 {
                value |= data[word + 1] << (64 - offset);
            }
            let value = (value & mask) as usize;
            *state = if global {
                value as u16
            } else {
                *palette
                    .get(value)
                    .ok_or_else(|| invalid("palette index out of range"))?
            };
        }
        let light = reader.take(if sky { 2 * LIGHT } else { LIGHT })?.to_vec();
        let raw = start[..start.len() - reader.remaining()].to_vec();
        Ok(Self {
            states,
            light,
            raw: Some(raw),
        })
    }

    fn write(&self, writer: &mut Writer, format: Format) {
        if let Some(raw) = &self.raw {
            writer.bytes(raw);
            return;
        }
        let mut palette: Vec<u16> = Vec::new();
        for state in self.states.iter() {
            if !palette.contains(state) {
                palette.push(*state);
                if palette.len() > 256 {
                    break;
                }
            }
        }
        let needed = usize::BITS - (palette.len().max(1) - 1).leading_zeros();
        let global = needed > 8;
        let bits = if global {
            format.global_bits()
        } else {
            (needed as u8).max(4)
        };
        writer.u8(bits);
        if global {
            if format == Format::Legacy {
                writer.varint(0);
            }
        } else {
            writer.varint(palette.len() as i32);
            for state in &palette {
                writer.varint(i32::from(*state));
            }
        }
        let bits = usize::from(bits);
        let mut data = vec![0u64; SECTION_BLOCKS * bits / 64];
        for (index, state) in self.states.iter().enumerate() {
            let value = if global {
                u64::from(*state)
            } else {
                palette.iter().position(|known| known == state).unwrap_or(0) as u64
            };
            let bit = index * bits;
            let (word, offset) = (bit / 64, bit % 64);
            data[word] |= value << offset;
            if offset + bits > 64 {
                data[word + 1] |= value >> (64 - offset);
            }
        }
        writer.varint(data.len() as i32);
        for long in data {
            writer.i64(long as i64);
        }
        writer.bytes(&self.light);
    }
}

/// A decoded chunk packet.
#[derive(Clone, Debug)]
pub struct Column {
    pub x: i32,
    pub z: i32,
    pub full: bool,
    /// The sections sent, by section Y (0 to 15).
    pub sections: [Option<Section>; 16],
    /// Biomes as received (full chunks only).
    pub biomes: Option<Vec<u8>>,
    /// The block entity list as received: count and named NBT tags.
    pub block_entities: Vec<u8>,
}

impl Column {
    /// Parse a chunk packet body. `sky`: the dimension has sky light.
    pub fn read(body: &[u8], format: Format, sky: bool) -> io::Result<Self> {
        let mut reader = Reader::new(body);
        let x = reader.i32()?;
        let z = reader.i32()?;
        let full = reader.bool()?;
        let mask = reader.varint()?;
        if mask as u32 > 0xffff {
            return Err(invalid("chunk section mask beyond 16 sections"));
        }
        let mut blob = Reader::new(reader.byte_array()?);
        let mut sections: [Option<Section>; 16] = Default::default();
        for (y, section) in sections.iter_mut().enumerate() {
            if mask & (1 << y) != 0 {
                *section = Some(Section::read(&mut blob, format, sky)?);
            }
        }
        let biomes = if full {
            Some(blob.take(format.biome_bytes())?.to_vec())
        } else {
            None
        };
        blob.finish()?;
        let start = reader.peek();
        for _ in 0..reader.count()? {
            reader.nbt_bytes(true)?;
        }
        let block_entities = start.to_vec();
        Ok(Self {
            x,
            z,
            full,
            sections,
            biomes,
            block_entities,
        })
    }

    pub fn write(&self, format: Format) -> Vec<u8> {
        let mut blob = Writer::with_capacity(8192);
        let mut mask = 0;
        for (y, section) in self.sections.iter().enumerate() {
            if let Some(section) = section {
                mask |= 1 << y;
                section.write(&mut blob, format);
            }
        }
        if let Some(biomes) = &self.biomes {
            blob.bytes(biomes);
        }
        let mut writer = Writer::with_capacity(blob.len() + self.block_entities.len() + 16);
        writer
            .i32(self.x)
            .i32(self.z)
            .bool(self.full)
            .varint(mask)
            .byte_array(&blob.buf)
            .bytes(&self.block_entities);
        writer.buf
    }

    /// Whether any section changed since it was read.
    pub fn changed(&self) -> bool {
        self.sections
            .iter()
            .flatten()
            .any(|section| section.raw.is_none())
    }
}

/// The parts of a chunk packet the step follows without decoding sections.
pub struct Header<'a> {
    pub x: i32,
    pub z: i32,
    pub full: bool,
    pub mask: i32,
    /// The block entities' named NBT tags.
    pub block_entities: Vec<&'a [u8]>,
}

/// The header and block entity tags of a chunk packet (1.12 or 1.13).
pub fn header(body: &[u8]) -> io::Result<Header<'_>> {
    let mut reader = Reader::new(body);
    let x = reader.i32()?;
    let z = reader.i32()?;
    let full = reader.bool()?;
    let mask = reader.varint()?;
    reader.byte_array()?;
    let count = reader.count()?;
    let mut block_entities = Vec::with_capacity(count);
    for _ in 0..count {
        block_entities.push(reader.nbt_bytes(true)?);
    }
    reader.finish()?;
    Ok(Header {
        x,
        z,
        full,
        mask,
        block_entities,
    })
}
