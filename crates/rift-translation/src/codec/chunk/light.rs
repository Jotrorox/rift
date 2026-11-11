//! Sky and block light: nibble arrays per section, inside the chunk packet
//! before 1.14, in `light_update` from 1.14, and in both from 1.18.
//!
//! Index 0 of each list is the section below the world's lowest one, so a
//! column of `n` sections has `n + 2` light sections (18 before 1.17).
//! Masks select the sections a packet carries: a data bit sends an array, an
//! empty bit says the section is dark, and a section with neither keeps
//! whatever the client has.
use crate::wire::{Reader, Writer, invalid};
use std::io;

/// Bytes in one section's nibble array.
pub const ARRAY: usize = 2048;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LightArray {
    /// Not sent: the client keeps what it has.
    #[default]
    Absent,
    /// Sent as all zero (an "empty" mask bit).
    Empty,
    Data(Box<[u8]>),
}

impl LightArray {
    /// A full array of one nibble value.
    pub fn filled(level: u8) -> Self {
        LightArray::Data(vec![level * 17; ARRAY].into_boxed_slice())
    }

    /// The bytes the pre-1.14 layout sends: absent arrays become `default`.
    pub fn bytes_or<'a>(&'a self, default: &'a [u8]) -> &'a [u8] {
        match self {
            LightArray::Data(bytes) => bytes,
            LightArray::Empty => &ZEROS,
            LightArray::Absent => default,
        }
    }
}

pub static ZEROS: [u8; ARRAY] = [0; ARRAY];
pub static FULL: [u8; ARRAY] = [0xff; ARRAY];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Light {
    /// 1.16 to 1.19.4: whether the client may skip edge relighting.
    pub trust_edges: bool,
    pub sky: Vec<LightArray>,
    pub block: Vec<LightArray>,
}

/// Masks are varints before 1.17, long arrays (`BitSet.toLongArray`) until
/// 26.3 and byte arrays (`BitSet.toByteArray`) from 26.3.
fn read_mask(reader: &mut Reader<'_>, protocol: i32) -> io::Result<Vec<u64>> {
    if protocol < 755 {
        return Ok(vec![u64::from(reader.varint()? as u32)]);
    }
    if protocol >= 777 {
        let bytes = reader.byte_array()?;
        let mut words = vec![0u64; bytes.len().div_ceil(8)];
        for (index, byte) in bytes.iter().enumerate() {
            words[index / 8] |= u64::from(*byte) << (8 * (index % 8));
        }
        return Ok(words);
    }
    let count = reader.count()?;
    if count.saturating_mul(8) > reader.remaining() {
        return Err(invalid("short packet"));
    }
    (0..count).map(|_| Ok(reader.i64()? as u64)).collect()
}

fn write_mask(writer: &mut Writer, protocol: i32, words: &[u64]) -> io::Result<()> {
    let used = words.len() - words.iter().rev().take_while(|word| **word == 0).count();
    let words = &words[..used];
    if protocol < 755 {
        if words.len() > 1
            || words
                .first()
                .is_some_and(|word| *word > u64::from(u32::MAX >> 1))
        {
            return Err(invalid("light mask does not fit a VarInt"));
        }
        writer.varint(words.first().copied().unwrap_or(0) as i32);
    } else if protocol >= 777 {
        let mut bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        while bytes.last() == Some(&0) {
            bytes.pop();
        }
        writer.byte_array(&bytes);
    } else {
        writer.varint(words.len() as i32);
        for word in words {
            writer.i64(*word as i64);
        }
    }
    Ok(())
}

fn bit(words: &[u64], index: usize) -> bool {
    words
        .get(index / 64)
        .is_some_and(|word| word >> (index % 64) & 1 == 1)
}

fn set(words: &mut Vec<u64>, index: usize) {
    if words.len() <= index / 64 {
        words.resize(index / 64 + 1, 0);
    }
    words[index / 64] |= 1 << (index % 64);
}

fn bits(words: &[u64]) -> usize {
    words.len() * 64
        - words.iter().rev().take_while(|word| **word == 0).count() * 64
        - words
            .iter()
            .rev()
            .find(|word| **word != 0)
            .map_or(0, |word| word.leading_zeros() as usize)
}

impl Light {
    /// Whether this protocol's light data carries the trust edges flag.
    pub fn has_trust_edges(protocol: i32) -> bool {
        (735..763).contains(&protocol)
    }

    /// Read light data (masks and arrays, after the coordinates of a
    /// `light_update`, or at the end of a 1.18 chunk) in `protocol`'s layout.
    pub fn read(reader: &mut Reader<'_>, protocol: i32) -> io::Result<Self> {
        let trust_edges = Self::has_trust_edges(protocol) && reader.bool()?;
        let sky_mask = read_mask(reader, protocol)?;
        let block_mask = read_mask(reader, protocol)?;
        let empty_sky = read_mask(reader, protocol)?;
        let empty_block = read_mask(reader, protocol)?;
        let mut arrays = |data: &[u64], empty: &[u64]| -> io::Result<Vec<LightArray>> {
            let length = bits(data).max(bits(empty));
            if length > 4096 {
                return Err(invalid("light mask too long"));
            }
            let expected = (0..length).filter(|index| bit(data, *index)).count();
            if protocol >= 755 && reader.varint()? as usize != expected {
                return Err(invalid("light array count does not match its mask"));
            }
            let mut out = Vec::with_capacity(length);
            for index in 0..length {
                out.push(match (bit(data, index), bit(empty, index)) {
                    (true, true) => return Err(invalid("light section is both sent and empty")),
                    (true, false) => {
                        let bytes = reader.byte_array()?;
                        if bytes.len() != ARRAY {
                            return Err(invalid("light array is not 2048 bytes"));
                        }
                        LightArray::Data(bytes.into())
                    }
                    (false, true) => LightArray::Empty,
                    (false, false) => LightArray::Absent,
                });
            }
            Ok(out)
        };
        let sky = arrays(&sky_mask, &empty_sky)?;
        let block = arrays(&block_mask, &empty_block)?;
        Ok(Self {
            trust_edges,
            sky,
            block,
        })
    }

    pub fn write(&self, writer: &mut Writer, protocol: i32) -> io::Result<()> {
        if Self::has_trust_edges(protocol) {
            writer.bool(self.trust_edges);
        }
        let masks = |arrays: &[LightArray]| {
            let (mut data, mut empty) = (Vec::new(), Vec::new());
            for (index, array) in arrays.iter().enumerate() {
                match array {
                    LightArray::Data(_) => set(&mut data, index),
                    LightArray::Empty => set(&mut empty, index),
                    LightArray::Absent => {}
                }
            }
            (data, empty)
        };
        let (sky, empty_sky) = masks(&self.sky);
        let (block, empty_block) = masks(&self.block);
        for mask in [&sky, &block, &empty_sky, &empty_block] {
            write_mask(writer, protocol, mask)?;
        }
        for arrays in [&self.sky, &self.block] {
            if protocol >= 755 {
                let count = arrays
                    .iter()
                    .filter(|array| matches!(array, LightArray::Data(_)))
                    .count();
                writer.varint(count as i32);
            }
            for array in arrays.iter() {
                if let LightArray::Data(bytes) = array {
                    writer.byte_array(bytes);
                }
            }
        }
        Ok(())
    }

    /// Apply a later update for the same column: sections it carries
    /// replace these, the rest stay.
    pub fn merge(&mut self, newer: Light) {
        self.trust_edges = newer.trust_edges;
        for (mine, theirs) in [(&mut self.sky, newer.sky), (&mut self.block, newer.block)] {
            if mine.len() < theirs.len() {
                mine.resize(theirs.len(), LightArray::Absent);
            }
            for (index, array) in theirs.into_iter().enumerate() {
                if array != LightArray::Absent {
                    mine[index] = array;
                }
            }
        }
    }

    /// Whether the update carries nothing.
    pub fn is_empty(&self) -> bool {
        self.sky
            .iter()
            .chain(&self.block)
            .all(|array| *array == LightArray::Absent)
    }
}

/// A `light_update` packet: column coordinates and light data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LightUpdate {
    pub x: i32,
    pub z: i32,
    pub light: Light,
}

impl LightUpdate {
    pub fn read(data: &[u8], protocol: i32) -> io::Result<Self> {
        let mut reader = Reader::new(data);
        let x = reader.varint()?;
        let z = reader.varint()?;
        let light = Light::read(&mut reader, protocol)?;
        reader.finish()?;
        Ok(Self { x, z, light })
    }

    pub fn write(&self, protocol: i32) -> io::Result<Vec<u8>> {
        let mut writer = Writer::with_capacity(64 + self.light.sky.len() * 2 * (ARRAY + 3));
        writer.varint(self.x).varint(self.z);
        self.light.write(&mut writer, protocol)?;
        Ok(writer.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Light {
        let mut sky = vec![LightArray::Absent; 26];
        sky[0] = LightArray::filled(15);
        sky[3] = LightArray::Empty;
        sky[25] = LightArray::Data((0..ARRAY).map(|index| index as u8).collect());
        let block = vec![LightArray::Empty, LightArray::Absent, LightArray::filled(3)];
        Light {
            trust_edges: true,
            sky,
            block,
        }
    }

    #[test]
    fn light_round_trips_in_every_mask_layout() {
        for protocol in [757, 762, 763, 777] {
            let mut light = sample();
            light.trust_edges = Light::has_trust_edges(protocol);
            let update = LightUpdate {
                x: -3,
                z: 70000,
                light,
            };
            let bytes = update.write(protocol).unwrap();
            assert_eq!(
                LightUpdate::read(&bytes, protocol).unwrap(),
                update,
                "{protocol}"
            );
        }
        // Up to 18 sections fit a VarInt mask before 1.17.
        let mut light = sample();
        light.sky.truncate(4); // Trailing absent sections are not sent.
        let update = LightUpdate { x: 1, z: 2, light };
        let bytes = update.write(735).unwrap();
        assert_eq!(LightUpdate::read(&bytes, 735).unwrap(), update);
        let bytes = update.write(477).unwrap();
        let mut expected = update.clone();
        expected.light.trust_edges = false;
        assert_eq!(LightUpdate::read(&bytes, 477).unwrap(), expected);
    }

    #[test]
    fn byte_masks_trim_trailing_zero_bytes() {
        let mut writer = Writer::new();
        write_mask(&mut writer, 777, &[0x0102, 0]).unwrap();
        assert_eq!(writer.buf, [2, 2, 1]);
        let mut writer = Writer::new();
        write_mask(&mut writer, 760, &[5, 0]).unwrap();
        assert_eq!(writer.buf, [1, 0, 0, 0, 0, 0, 0, 0, 5]);
    }

    #[test]
    fn merging_replaces_only_sent_sections() {
        let mut light = sample();
        let newer = Light {
            sky: vec![LightArray::Absent, LightArray::Empty],
            ..Light::default()
        };
        light.merge(newer);
        assert_eq!(light.sky[0], LightArray::filled(15));
        assert_eq!(light.sky[1], LightArray::Empty);
        assert!(!light.trust_edges);
    }

    #[test]
    fn malformed_light_fails() {
        let update = LightUpdate {
            x: 0,
            z: 0,
            light: sample(),
        };
        let bytes = update.write(760).unwrap();
        for cut in [1, 5, 20, bytes.len() - 1] {
            assert!(LightUpdate::read(&bytes[..cut], 760).is_err());
        }
        // A section both sent and empty.
        let mut writer = Writer::new();
        writer.varint(0).varint(0).bool(false);
        for _ in 0..4 {
            writer.varint(1).i64(1);
        }
        assert!(LightUpdate::read(&writer.buf, 760).is_err());
        // An array count that disagrees with the mask.
        let mut writer = Writer::new();
        writer.varint(0).varint(0).bool(false);
        writer.varint(1).i64(1);
        for _ in 0..3 {
            writer.varint(0);
        }
        writer.varint(2);
        assert!(LightUpdate::read(&writer.buf, 760).is_err());
    }
}
