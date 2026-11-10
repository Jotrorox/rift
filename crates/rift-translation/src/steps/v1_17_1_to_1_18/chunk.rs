//! 1.18 sends a chunk's light inside the chunk packet; 1.17.1 sent it in a
//! `light_update` just before the chunk. These steps make that sequence
//! right: going down a 1.18 chunk becomes a light update followed by the
//! chunk, going up light updates for chunks the client has not got yet wait
//! for their chunk and travel inside it.
//!
//! The light data itself is laid out identically in both: a trust-edges
//! flag, four section masks as long arrays, then the sky and block light
//! arrays. The column between the coordinates and the light (sections,
//! biomes, heightmaps, block entities) changed completely and is the chunk
//! codec's to convert.
use crate::{
    Direction,
    codec::Shared,
    wire::{Reader, Writer, invalid},
};
use std::{
    collections::{HashSet, VecDeque},
    io,
};

/// Bytes in one section's light array.
const LIGHT_ARRAY: usize = 2048;

/// Check the light data at the front of `r` and return its bytes.
pub fn light<'a>(r: &mut Reader<'a>) -> io::Result<&'a [u8]> {
    let start = r.peek();
    r.bool()?;
    for _ in 0..4 {
        let longs = r.varint()?;
        let longs = usize::try_from(longs)
            .ok()
            .filter(|longs| longs.saturating_mul(8) <= r.remaining())
            .ok_or_else(|| invalid("invalid light mask"))?;
        r.take(longs * 8)?;
    }
    for _ in 0..2 {
        for _ in 0..r.count()? {
            if r.byte_array()?.len() != LIGHT_ARRAY {
                return Err(invalid("light array of the wrong size"));
            }
        }
    }
    Ok(&start[..start.len() - r.remaining()])
}

/// Light data saying nothing about any section: the client keeps what it
/// has, and computes sky light from above where it has nothing.
pub fn no_light() -> Vec<u8> {
    let mut w = Writer::new();
    w.bool(true);
    for _ in 0..4 {
        w.varint(0);
    }
    w.varint(0).varint(0);
    w.into_inner()
}

/// The column of a 1.18 chunk packet: heightmaps, the section data and the
/// compact block entities. Checked structurally; returns its bytes.
pub fn column_1_18<'a>(r: &mut Reader<'a>) -> io::Result<&'a [u8]> {
    let start = r.peek();
    r.nbt_bytes(true)?;
    r.byte_array()?;
    for _ in 0..r.count()? {
        r.u8()?;
        r.i16()?;
        r.varint()?;
        r.nbt_bytes(true)?;
    }
    Ok(&start[..start.len() - r.remaining()])
}

/// The column of a 1.17.1 chunk packet: section mask, heightmaps, biomes,
/// the present sections' data and the block entities. Checked
/// structurally; returns its bytes.
pub fn column_1_17<'a>(r: &mut Reader<'a>) -> io::Result<&'a [u8]> {
    let start = r.peek();
    let longs = r.count()?;
    r.take(
        longs
            .checked_mul(8)
            .ok_or_else(|| invalid("invalid section mask"))?,
    )?;
    r.nbt_bytes(true)?;
    for _ in 0..r.count()? {
        r.varint()?;
    }
    r.byte_array()?;
    for _ in 0..r.count()? {
        r.nbt_bytes(true)?;
    }
    Ok(&start[..start.len() - r.remaining()])
}

/// Convert the column of a chunk packet (everything between the chunk
/// coordinates and, from 1.18, the light) from the sender's layout to the
/// receiver's: 1.17.1's section mask, heightmaps, biome array, present
/// sections and block entities with their position and ID in the NBT, or
/// 1.18's heightmaps, every section with paletted blocks and biomes, and
/// compact block entities.
// INTEGRATION: the chunk codec's column model (read in one protocol, write
// in the other, with block state, biome and block entity type IDs mapped).
// It needs the world's section count, which the registries codec knows from
// the dimension type of `login`/`respawn`.
pub fn column(
    shared: &mut Shared,
    direction: Direction,
    x: i32,
    z: i32,
    column: &[u8],
) -> io::Result<Vec<u8>> {
    let _ = (shared, direction, x, z, column);
    #[cfg(test)]
    if crate::steps::v1_16_4_to_1_17::common::testing::stand_ins() {
        return Ok(column.to_vec());
    }
    Err(invalid("chunk columns are not implemented"))
}

/// Light updates a 1.17.1 backend sent for chunks it has not sent yet, and
/// the chunks the client has, for a 1.18 client.
#[derive(Default)]
pub struct Pending {
    /// Light data by chunk, oldest first.
    light: VecDeque<((i32, i32), Vec<u8>)>,
    loaded: HashSet<(i32, i32)>,
}

/// Light waiting for a chunk at most: vanilla sends the chunk right after.
const MAX_PENDING: usize = 64;
/// Chunks followed at most, far more than a 32-chunk view distance needs.
const MAX_LOADED: usize = 1 << 14;

impl Pending {
    pub fn is_loaded(&self, chunk: (i32, i32)) -> bool {
        self.loaded.contains(&chunk)
    }

    /// Keep the light of a chunk not yet sent; newer light replaces older.
    pub fn hold(&mut self, chunk: (i32, i32), light: Vec<u8>) {
        self.light.retain(|(held, _)| *held != chunk);
        if self.light.len() >= MAX_PENDING {
            self.light.pop_front();
        }
        self.light.push_back((chunk, light));
    }

    /// The chunk is being sent: its held light, if any.
    pub fn load(&mut self, chunk: (i32, i32)) -> Option<Vec<u8>> {
        if self.loaded.len() >= MAX_LOADED {
            self.loaded.clear();
        }
        self.loaded.insert(chunk);
        let index = self.light.iter().position(|(held, _)| *held == chunk)?;
        self.light.remove(index).map(|(_, light)| light)
    }

    pub fn forget(&mut self, chunk: (i32, i32)) {
        self.loaded.remove(&chunk);
        self.light.retain(|(held, _)| *held != chunk);
    }

    pub fn clear(&mut self) {
        self.light.clear();
        self.loaded.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn light_with_arrays(arrays: usize) -> Vec<u8> {
        let mut w = Writer::new();
        w.bool(true)
            .varint(1)
            .i64(0b110)
            .varint(0)
            .varint(0)
            .varint(0);
        w.varint(arrays as i32);
        for _ in 0..arrays {
            w.byte_array(&[0xff; LIGHT_ARRAY]);
        }
        w.varint(0);
        w.into_inner()
    }

    #[test]
    fn light_is_checked() {
        let data = light_with_arrays(2);
        let mut r = Reader::new(&data);
        assert_eq!(light(&mut r).unwrap(), &data[..]);
        assert!(r.is_empty());
        let empty = no_light();
        let mut r = Reader::new(&empty);
        light(&mut r).unwrap();
        r.finish().unwrap();
        let mut short = light_with_arrays(1);
        short.truncate(short.len() - 10);
        assert!(light(&mut Reader::new(&short)).is_err());
        assert!(light(&mut Reader::new(&[1, 0x7f])).is_err());
        let mut odd = Writer::new();
        odd.bool(false)
            .varint(0)
            .varint(0)
            .varint(0)
            .varint(0)
            .varint(1)
            .byte_array(&[0; 3])
            .varint(0);
        assert!(light(&mut Reader::new(&odd.buf)).is_err());
    }

    #[test]
    fn held_light_waits_for_its_chunk() {
        let mut pending = Pending::default();
        pending.hold((1, 2), vec![1]);
        pending.hold((1, 2), vec![2]);
        assert!(!pending.is_loaded((1, 2)));
        assert_eq!(pending.load((1, 2)), Some(vec![2]));
        assert!(pending.is_loaded((1, 2)));
        assert_eq!(pending.load((1, 2)), None);
        pending.forget((1, 2));
        assert!(!pending.is_loaded((1, 2)));
        for x in 0..100 {
            pending.hold((x, 0), vec![]);
        }
        assert_eq!(pending.load((0, 0)), None, "the oldest was dropped");
        assert_eq!(pending.load((99, 0)), Some(vec![]));
        pending.clear();
        assert!(!pending.is_loaded((99, 0)));
    }
}
