//! Paletted containers: a section's 4096 block states and, from 1.18, its
//! 64 biome cells.
//!
//! Entries index a palette (indirect) or are global IDs themselves (direct);
//! from 1.18 a container whose entries are all the same is a single value
//! with no data. Conversions keep a container's shape where they can: a
//! registry mapping rewrites palette entries in place and leaves the packed
//! data alone, and only a direct palette, a new packing or a new global
//! width unpacks the entries.
use super::storage::{self, Packing};
use crate::{
    data,
    wire::{Reader, Writer, invalid},
};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Blocks,
    Biomes,
}

impl Kind {
    pub const fn entries(self) -> usize {
        match self {
            Kind::Blocks => 4096,
            Kind::Biomes => 64,
        }
    }

    /// The widest indirect palette; wider sends global IDs.
    const fn max_indirect(self) -> u8 {
        match self {
            Kind::Blocks => 8,
            Kind::Biomes => 3,
        }
    }

    /// The narrowest indirect palette the client reads as sent: narrower
    /// block palettes are widened to 4 bits.
    const fn min_indirect(self) -> u8 {
        match self {
            Kind::Blocks => 4,
            Kind::Biomes => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Palette {
    /// Every entry is this ID (1.18 and newer); there is no data.
    Single(i32),
    /// Entries index this list.
    Indirect(Vec<i32>),
    /// Entries are global IDs.
    Direct,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    pub kind: Kind,
    pub palette: Palette,
    /// Bits per packed entry; 0 for a single value.
    pub bits: u8,
    pub packing: Packing,
    pub data: Vec<u64>,
}

/// How a protocol stores one kind of container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub kind: Kind,
    pub packing: Packing,
    /// Whether single-value containers exist (1.18 and newer).
    pub single: bool,
    /// The width of direct entries.
    pub global_bits: u8,
}

/// Block state IDs need this many bits in `protocol`'s global palette.
pub fn block_global_bits(protocol: i32) -> u8 {
    if crate::version::flattened(protocol) {
        storage::ceil_log2(data::version(protocol).block_states.len() as usize)
    } else {
        // `id << 4 | metadata` over every registered state: always 13 bits.
        13
    }
}

impl Format {
    pub fn blocks(protocol: i32) -> Self {
        Self {
            kind: Kind::Blocks,
            packing: Packing::of(protocol),
            single: protocol >= 757,
            global_bits: block_global_bits(protocol),
        }
    }

    /// Biome containers (1.18 and newer); `global_bits` follows the size of
    /// the biome registry the server sent.
    pub fn biomes(protocol: i32, global_bits: u8) -> Self {
        Self {
            kind: Kind::Biomes,
            packing: Packing::of(protocol),
            single: true,
            global_bits,
        }
    }
}

impl Container {
    pub fn single(kind: Kind, value: i32) -> Self {
        Self {
            kind,
            palette: Palette::Single(value),
            bits: 0,
            packing: Packing::Padded,
            data: Vec::new(),
        }
    }

    /// A container holding `values` (global IDs, one per entry) in `format`,
    /// with the smallest palette that holds them.
    pub fn from_values(values: &[i32], format: &Format, scratch: &mut Vec<u32>) -> Self {
        debug_assert_eq!(values.len(), format.kind.entries());
        let kind = format.kind;
        let mut palette: Vec<i32> = Vec::new();
        scratch.clear();
        let mut direct = false;
        for value in values {
            let index = match palette.iter().position(|known| known == value) {
                Some(index) => index,
                None if palette.len() < 1 << kind.max_indirect() => {
                    palette.push(*value);
                    palette.len() - 1
                }
                None => {
                    direct = true;
                    break;
                }
            };
            scratch.push(index as u32);
        }
        if direct {
            let bits = format.global_bits;
            scratch.clear();
            scratch.extend(values.iter().map(|value| *value as u32));
            let mut data = Vec::new();
            storage::pack(scratch, bits, format.packing, &mut data);
            return Self {
                kind,
                palette: Palette::Direct,
                bits,
                packing: format.packing,
                data,
            };
        }
        if palette.len() == 1 && format.single {
            return Self::single(kind, palette[0]);
        }
        let bits = storage::bits_for(palette.len()).max(kind.min_indirect());
        let mut data = Vec::new();
        storage::pack(scratch, bits, format.packing, &mut data);
        Self {
            kind,
            palette: Palette::Indirect(palette),
            bits,
            packing: format.packing,
            data,
        }
    }

    /// Every entry as a global ID, into `out`.
    pub fn values(&self, out: &mut Vec<i32>, scratch: &mut Vec<u32>) -> io::Result<()> {
        let entries = self.kind.entries();
        out.clear();
        match &self.palette {
            Palette::Single(value) => out.resize(entries, *value),
            Palette::Indirect(palette) => {
                scratch.resize(entries, 0);
                storage::unpack(&self.data, self.bits, self.packing, scratch)?;
                for index in scratch.iter() {
                    out.push(
                        *palette
                            .get(*index as usize)
                            .ok_or_else(|| invalid("palette index out of range"))?,
                    );
                }
            }
            Palette::Direct => {
                scratch.resize(entries, 0);
                storage::unpack(&self.data, self.bits, self.packing, scratch)?;
                out.extend(scratch.iter().map(|value| *value as i32));
            }
        }
        Ok(())
    }

    /// The global ID at `index` (`(y * 16 + z) * 16 + x` for blocks).
    pub fn get(&self, index: usize, scratch: &mut Vec<u32>) -> io::Result<i32> {
        match &self.palette {
            Palette::Single(value) => Ok(*value),
            palette => {
                scratch.resize(self.kind.entries(), 0);
                storage::unpack(&self.data, self.bits, self.packing, scratch)?;
                let raw = scratch[index];
                match palette {
                    Palette::Indirect(palette) => palette
                        .get(raw as usize)
                        .copied()
                        .ok_or_else(|| invalid("palette index out of range")),
                    _ => Ok(raw as i32),
                }
            }
        }
    }

    /// How many entries satisfy `matches`; unpacks only when the answer is
    /// not decided by the palette alone.
    pub fn count(&self, matches: impl Fn(i32) -> bool, scratch: &mut Vec<u32>) -> io::Result<u32> {
        let entries = self.kind.entries();
        match &self.palette {
            Palette::Single(value) => Ok(if matches(*value) { entries as u32 } else { 0 }),
            Palette::Indirect(palette) => {
                let hits: Vec<bool> = palette.iter().map(|value| matches(*value)).collect();
                if hits.iter().all(|hit| !hit) {
                    return Ok(0);
                }
                scratch.resize(entries, 0);
                storage::unpack(&self.data, self.bits, self.packing, scratch)?;
                let mut count = 0;
                for index in scratch.iter() {
                    if *hits
                        .get(*index as usize)
                        .ok_or_else(|| invalid("palette index out of range"))?
                    {
                        count += 1;
                    }
                }
                Ok(count)
            }
            Palette::Direct => {
                scratch.resize(entries, 0);
                storage::unpack(&self.data, self.bits, self.packing, scratch)?;
                Ok(scratch
                    .iter()
                    .filter(|value| matches(**value as i32))
                    .count() as u32)
            }
        }
    }

    /// Rewrite every ID through `map` and bring the container into
    /// `format`. Palette entries are mapped in place; packed data is only
    /// touched for a direct palette or when the packing or global width
    /// changes.
    pub fn convert(
        &mut self,
        map: Option<&dyn Fn(i32) -> i32>,
        format: &Format,
        values: &mut Vec<i32>,
        scratch: &mut Vec<u32>,
    ) -> io::Result<()> {
        debug_assert_eq!(self.kind, format.kind);
        match &mut self.palette {
            Palette::Single(value) => {
                if let Some(map) = map {
                    *value = map(*value);
                }
                if !format.single {
                    let value = *value;
                    let bits = self.kind.min_indirect();
                    self.palette = Palette::Indirect(vec![value]);
                    self.bits = bits;
                    self.packing = format.packing;
                    self.data.clear();
                    self.data
                        .resize(format.packing.longs(bits, self.kind.entries()), 0);
                }
                Ok(())
            }
            Palette::Indirect(palette) => {
                if let Some(map) = map {
                    for value in palette.iter_mut() {
                        *value = map(*value);
                    }
                }
                if format.single && palette.len() == 1 {
                    *self = Self::single(self.kind, palette[0]);
                    return Ok(());
                }
                let bits = self.bits.max(self.kind.min_indirect());
                if bits != self.bits || self.packing != format.packing {
                    scratch.resize(self.kind.entries(), 0);
                    storage::unpack(&self.data, self.bits, self.packing, scratch)?;
                    storage::pack(scratch, bits, format.packing, &mut self.data);
                    self.bits = bits;
                    self.packing = format.packing;
                }
                Ok(())
            }
            Palette::Direct => {
                if map.is_none()
                    && self.bits == format.global_bits
                    && self.packing == format.packing
                {
                    return Ok(());
                }
                self.values(values, scratch)?;
                if let Some(map) = map {
                    for value in values.iter_mut() {
                        *value = map(*value);
                    }
                }
                *self = Self::from_values(values, format, scratch);
                Ok(())
            }
        }
    }

    /// Fail when an entry indexes past the palette or a direct entry does
    /// not fit `limit` (the number of IDs the protocol has), so corrupt data
    /// is caught before it is relabelled and passed on.
    fn validate(&self, limit: Option<usize>) -> io::Result<()> {
        let entries = self.kind.entries();
        match &self.palette {
            Palette::Single(_) => Ok(()),
            Palette::Indirect(palette) => {
                if palette.is_empty() || palette.len() > 1 << self.bits {
                    return Err(invalid("invalid palette length"));
                }
                if palette.len() < 1 << self.bits
                    && storage::max_entry(&self.data, self.bits, self.packing, entries)? as usize
                        >= palette.len()
                {
                    return Err(invalid("palette index out of range"));
                }
                Ok(())
            }
            Palette::Direct => {
                if let Some(limit) = limit
                    && (1usize << self.bits) > limit
                    && storage::max_entry(&self.data, self.bits, self.packing, entries)? as usize
                        >= limit
                {
                    return Err(invalid("global palette ID out of range"));
                }
                Ok(())
            }
        }
    }

    /// Read a container in `protocol`'s layout. `limit` bounds direct IDs.
    pub fn read(
        reader: &mut Reader<'_>,
        protocol: i32,
        kind: Kind,
        limit: Option<usize>,
    ) -> io::Result<Self> {
        let packing = Packing::of(protocol);
        let entries = kind.entries();
        let sent = reader.u8()?;
        let modern = protocol >= 757;
        let (palette, bits) = if modern && sent == 0 {
            (Palette::Single(reader.varint()?), 0)
        } else if sent <= kind.max_indirect() {
            let count = reader.count()?;
            let mut palette = Vec::with_capacity(count);
            for _ in 0..count {
                palette.push(reader.varint()?);
            }
            (Palette::Indirect(palette), sent.max(kind.min_indirect()))
        } else if sent > 32 {
            return Err(invalid("invalid bits per entry"));
        } else {
            if protocol < 393 {
                // 1.12 sends an empty palette before global IDs.
                for _ in 0..reader.count()? {
                    reader.varint()?;
                }
            }
            (Palette::Direct, sent)
        };
        let expected = packing.longs(bits, entries);
        let length = if protocol < 770 {
            let length = reader.varint()?;
            if usize::try_from(length).ok() != Some(expected) {
                return Err(invalid("paletted container has the wrong data length"));
            }
            expected
        } else {
            expected
        };
        if length.saturating_mul(8) > reader.remaining() {
            return Err(invalid("short packet"));
        }
        let mut data = Vec::with_capacity(length);
        for _ in 0..length {
            data.push(reader.i64()? as u64);
        }
        let container = Self {
            kind,
            palette,
            bits,
            packing,
            data,
        };
        container.validate(limit)?;
        Ok(container)
    }

    /// Write the container in `protocol`'s layout; it must already be in
    /// that protocol's format (see [`Container::convert`]).
    pub fn write(&self, writer: &mut Writer, protocol: i32) -> io::Result<()> {
        let modern = protocol >= 757;
        if self.packing != Packing::of(protocol) && self.bits != 0 {
            return Err(invalid("container is not packed for the target protocol"));
        }
        match &self.palette {
            Palette::Single(value) => {
                if !modern {
                    return Err(invalid("single-value container before 1.18"));
                }
                writer.u8(0).varint(*value);
            }
            Palette::Indirect(palette) => {
                writer.u8(self.bits).varint(palette.len() as i32);
                for value in palette {
                    writer.varint(*value);
                }
            }
            Palette::Direct => {
                writer.u8(self.bits);
                if protocol < 393 {
                    writer.varint(0);
                }
            }
        }
        if protocol < 770 {
            writer.varint(self.data.len() as i32);
        }
        writer.buf.reserve(self.data.len() * 8);
        for long in &self.data {
            writer.i64(*long as i64);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(protocol: i32) -> Format {
        Format {
            global_bits: 15,
            ..Format::blocks(protocol)
        }
    }

    fn round_trip(container: &Container, protocol: i32, limit: Option<usize>) -> Container {
        let mut writer = Writer::new();
        container.write(&mut writer, protocol).unwrap();
        let mut reader = Reader::new(&writer.buf);
        let back = Container::read(&mut reader, protocol, container.kind, limit).unwrap();
        reader.finish().unwrap();
        back
    }

    #[test]
    fn palettes_are_chosen_by_distinct_values() {
        let (mut values, mut scratch) = (Vec::new(), Vec::new());
        let single = Container::from_values(&[7; 4096], &format(757), &mut scratch);
        assert_eq!(single.palette, Palette::Single(7));
        let old = Container::from_values(&[7; 4096], &format(756), &mut scratch);
        assert_eq!(
            (old.bits, old.palette.clone()),
            (4, Palette::Indirect(vec![7]))
        );
        let mixed: Vec<i32> = (0..4096).map(|index| (index % 40) * 3).collect();
        let indirect = Container::from_values(&mixed, &format(757), &mut scratch);
        assert_eq!(indirect.bits, 6);
        let wide: Vec<i32> = (0..4096).map(|index| index * 7).collect();
        let direct = Container::from_values(&wide, &format(757), &mut scratch);
        assert_eq!((direct.palette.clone(), direct.bits), (Palette::Direct, 15));
        for (container, expected) in [(&indirect, &mixed), (&direct, &wide)] {
            for protocol in [757, 770] {
                let back = round_trip(container, protocol, Some(1 << 15));
                back.values(&mut values, &mut scratch).unwrap();
                assert_eq!(&values, expected);
            }
        }
    }

    #[test]
    fn mapping_rewrites_palettes_in_place_and_repacks_when_needed() {
        let (mut values, mut scratch) = (Vec::new(), Vec::new());
        let source: Vec<i32> = (0..4096).map(|index| index % 5).collect();
        let mut container = Container::from_values(&source, &format(578), &mut scratch);
        assert_eq!(container.packing, Packing::Spanning);
        let data = container.data.clone();
        let add = |value: i32| value + 100;
        container
            .convert(Some(&add), &format(578), &mut values, &mut scratch)
            .unwrap();
        assert_eq!(container.data, data, "same format: data untouched");
        // Crossing into 1.16 repacks without spanning.
        container
            .convert(None, &format(735), &mut values, &mut scratch)
            .unwrap();
        assert_eq!(container.packing, Packing::Padded);
        container.values(&mut values, &mut scratch).unwrap();
        assert!(
            values
                .iter()
                .zip(&source)
                .all(|(new, old)| *new == old + 100)
        );
        // A direct palette is unpacked, mapped and repacked compactly.
        let wide: Vec<i32> = (0..4096).collect();
        let mut direct = Container::from_values(&wide, &format(757), &mut scratch);
        let halve = |value: i32| value / 512;
        direct
            .convert(Some(&halve), &format(757), &mut values, &mut scratch)
            .unwrap();
        assert_eq!(direct.bits, 4);
        direct.values(&mut values, &mut scratch).unwrap();
        assert_eq!(values[4095], 7);
        // Single values widen to a one-entry palette before 1.18.
        let mut single = Container::single(Kind::Blocks, 9);
        single
            .convert(None, &format(756), &mut values, &mut scratch)
            .unwrap();
        assert_eq!(single.palette, Palette::Indirect(vec![9]));
        assert_eq!(round_trip(&single, 756, None), single);
    }

    #[test]
    fn biome_palettes_keep_narrow_widths() {
        let mut scratch = Vec::new();
        let values: Vec<i32> = (0..64).map(|index| index % 2).collect();
        let format = Format::biomes(757, 6);
        let container = Container::from_values(&values, &format, &mut scratch);
        assert_eq!((container.bits, container.data.len()), (1, 1));
        assert_eq!(round_trip(&container, 757, Some(64)), container);
        let counted = container.count(|value| value == 1, &mut scratch).unwrap();
        assert_eq!(counted, 32);
    }

    #[test]
    fn legacy_direct_palettes_send_an_empty_palette() {
        let mut scratch = Vec::new();
        let wide: Vec<i32> = (0..4096).map(|index| index * 2).collect();
        let format = Format {
            global_bits: 13,
            ..Format::blocks(340)
        };
        let container = Container::from_values(&wide, &format, &mut scratch);
        let mut writer = Writer::new();
        container.write(&mut writer, 340).unwrap();
        assert_eq!(&writer.buf[..4], &[13, 0, 0xc0, 0x06]); // Bits, empty palette, 832 longs.
        assert_eq!(round_trip(&container, 340, None), container);
    }

    #[test]
    fn malformed_containers_fail() {
        let mut scratch = Vec::new();
        let source: Vec<i32> = (0..4096).map(|index| index % 3).collect();
        let container = Container::from_values(&source, &format(757), &mut scratch);
        let mut writer = Writer::new();
        container.write(&mut writer, 757).unwrap();
        let bytes = writer.buf;
        // Truncated anywhere.
        for cut in [0, 1, 3, 10, bytes.len() - 1] {
            assert!(
                Container::read(&mut Reader::new(&bytes[..cut]), 757, Kind::Blocks, None).is_err()
            );
        }
        // A palette of two entries with an index of 2 in the data.
        let mut bad = bytes.clone();
        bad[1] = 2;
        bad.remove(4);
        assert!(Container::read(&mut Reader::new(&bad), 757, Kind::Blocks, None).is_err());
        // Wrong data length.
        let mut bad = bytes.clone();
        bad[5] = 0x7f;
        assert!(Container::read(&mut Reader::new(&bad), 757, Kind::Blocks, None).is_err());
        // A huge palette length does not allocate.
        let huge = [4, 0xff, 0xff, 0xff, 0xff, 0x07];
        assert!(Container::read(&mut Reader::new(&huge), 757, Kind::Blocks, None).is_err());
        // Direct IDs past the registry.
        let wide: Vec<i32> = (0..4096).map(|index| index * 7).collect();
        let direct = Container::from_values(&wide, &format(757), &mut scratch);
        let mut writer = Writer::new();
        direct.write(&mut writer, 757).unwrap();
        assert!(
            Container::read(&mut Reader::new(&writer.buf), 757, Kind::Blocks, Some(1000)).is_err()
        );
    }
}
