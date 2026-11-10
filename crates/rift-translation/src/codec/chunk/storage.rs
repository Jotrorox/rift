//! Fixed-width entries packed into longs, as block states, biomes and
//! heightmaps travel.
//!
//! Before 1.16 entries are packed back to back and one may straddle two longs
//! ([`Packing::Spanning`]); from 1.16 on a long holds `64 / bits` entries and
//! the leftover high bits stay zero ([`Packing::Padded`]).
use crate::wire::invalid;
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packing {
    Spanning,
    Padded,
}

impl Packing {
    /// How `protocol` packs entries.
    pub fn of(protocol: i32) -> Self {
        if protocol >= 735 {
            Packing::Padded
        } else {
            Packing::Spanning
        }
    }

    /// The number of longs `count` entries of `bits` bits take.
    pub fn longs(self, bits: u8, count: usize) -> usize {
        let bits = usize::from(bits);
        if bits == 0 {
            return 0;
        }
        match self {
            Packing::Spanning => (count * bits).div_ceil(64),
            Packing::Padded => count.div_ceil(64 / bits),
        }
    }
}

fn mask(bits: u8) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1 << bits) - 1
    }
}

/// Unpack `out.len()` entries of `bits` bits from `data`, which must hold
/// [`Packing::longs`] longs.
pub fn unpack(data: &[u64], bits: u8, packing: Packing, out: &mut [u32]) -> io::Result<()> {
    if bits == 0 || bits > 32 {
        return Err(invalid("invalid bits per entry"));
    }
    if data.len() != packing.longs(bits, out.len()) {
        return Err(invalid("packed data has the wrong length"));
    }
    let mask = mask(bits);
    let width = usize::from(bits);
    match packing {
        Packing::Padded => {
            let per_long = 64 / width;
            for (chunk, long) in out.chunks_mut(per_long).zip(data) {
                let mut long = *long;
                for value in chunk {
                    *value = (long & mask) as u32;
                    long >>= width;
                }
            }
        }
        Packing::Spanning => {
            for (index, value) in out.iter_mut().enumerate() {
                let bit = index * width;
                let (start, offset) = (bit / 64, bit % 64);
                let mut entry = data[start] >> offset;
                if offset + width > 64 {
                    entry |= data[start + 1] << (64 - offset);
                }
                *value = (entry & mask) as u32;
            }
        }
    }
    Ok(())
}

/// Pack `values` with `bits` bits each into `out` (replacing its contents).
/// Values must fit in `bits`.
pub fn pack(values: &[u32], bits: u8, packing: Packing, out: &mut Vec<u64>) {
    out.clear();
    if bits == 0 {
        return;
    }
    debug_assert!(values.iter().all(|value| u64::from(*value) <= mask(bits)));
    let width = usize::from(bits);
    out.resize(packing.longs(bits, values.len()), 0);
    match packing {
        Packing::Padded => {
            let per_long = 64 / width;
            for (chunk, long) in values.chunks(per_long).zip(out.iter_mut()) {
                let mut packed = 0u64;
                for (slot, value) in chunk.iter().enumerate() {
                    packed |= u64::from(*value) << (slot * width);
                }
                *long = packed;
            }
        }
        Packing::Spanning => {
            for (index, value) in values.iter().enumerate() {
                let value = u64::from(*value);
                let bit = index * width;
                let (start, offset) = (bit / 64, bit % 64);
                out[start] |= value << offset;
                if offset + width > 64 {
                    out[start + 1] |= value >> (64 - offset);
                }
            }
        }
    }
}

/// The largest entry in packed `data`, without unpacking into a buffer.
pub fn max_entry(data: &[u64], bits: u8, packing: Packing, count: usize) -> io::Result<u32> {
    if bits == 0 || bits > 32 {
        return Err(invalid("invalid bits per entry"));
    }
    if data.len() != packing.longs(bits, count) {
        return Err(invalid("packed data has the wrong length"));
    }
    let mask = mask(bits);
    let width = usize::from(bits);
    let mut max = 0u64;
    match packing {
        Packing::Padded => {
            let per_long = 64 / width;
            let mut left = count;
            for long in data {
                let mut long = *long;
                for _ in 0..per_long.min(left) {
                    max = max.max(long & mask);
                    long >>= width;
                }
                left = left.saturating_sub(per_long);
            }
        }
        Packing::Spanning => {
            for index in 0..count {
                let bit = index * width;
                let (start, offset) = (bit / 64, bit % 64);
                let mut entry = data[start] >> offset;
                if offset + width > 64 {
                    entry |= data[start + 1] << (64 - offset);
                }
                max = max.max(entry & mask);
            }
        }
    }
    Ok(max as u32)
}

/// The fewest bits that hold every value below `count` (at least one).
pub fn bits_for(count: usize) -> u8 {
    if count <= 1 {
        1
    } else {
        (usize::BITS - (count - 1).leading_zeros()) as u8
    }
}

/// `ceil(log2(count))`, as the game sizes its global palettes; 0 for 1.
pub fn ceil_log2(count: usize) -> u8 {
    if count <= 1 {
        0
    } else {
        (usize::BITS - (count - 1).leading_zeros()) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_packings_round_trip_every_width() {
        for packing in [Packing::Spanning, Packing::Padded] {
            for bits in 1..=20u8 {
                let values: Vec<u32> = (0..4096u32)
                    .map(|index| index.wrapping_mul(2_654_435_761) & ((1 << bits) - 1))
                    .collect();
                let mut packed = Vec::new();
                pack(&values, bits, packing, &mut packed);
                assert_eq!(packed.len(), packing.longs(bits, values.len()));
                let mut back = vec![0; values.len()];
                unpack(&packed, bits, packing, &mut back).unwrap();
                assert_eq!(back, values, "{packing:?} {bits}");
                assert_eq!(
                    max_entry(&packed, bits, packing, values.len()).unwrap(),
                    *values.iter().max().unwrap()
                );
            }
        }
    }

    #[test]
    fn spanning_entries_cross_longs_and_padded_ones_do_not() {
        // 5-bit entries: the 13th starts at bit 60 and spans two longs.
        let mut values = vec![0u32; 64];
        values[12] = 0b11111;
        let mut packed = Vec::new();
        pack(&values, 5, Packing::Spanning, &mut packed);
        assert_eq!(packed.len(), 5);
        assert_eq!(packed[0], 0b1111 << 60);
        assert_eq!(packed[1], 1);
        pack(&values, 5, Packing::Padded, &mut packed);
        assert_eq!(packed.len(), 6); // 12 entries per long.
        assert_eq!(packed[1], 0b11111);
        // Known vanilla sizes: a 4096-entry section and a 256-entry heightmap.
        assert_eq!(Packing::Spanning.longs(14, 4096), 896);
        assert_eq!(Packing::Padded.longs(15, 4096), 1024);
        assert_eq!(Packing::Spanning.longs(9, 256), 36);
        assert_eq!(Packing::Padded.longs(9, 256), 37);
    }

    #[test]
    fn wrong_lengths_and_widths_fail() {
        let mut out = vec![0; 4096];
        assert!(unpack(&[0; 10], 4, Packing::Padded, &mut out).is_err());
        assert!(unpack(&[], 0, Packing::Padded, &mut out).is_err());
        assert!(max_entry(&[0; 3], 4, Packing::Spanning, 4096).is_err());
    }

    #[test]
    fn widths() {
        assert_eq!(bits_for(1), 1);
        assert_eq!(bits_for(2), 1);
        assert_eq!(bits_for(3), 2);
        assert_eq!(bits_for(16), 4);
        assert_eq!(bits_for(17), 5);
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(8598), 14);
        assert_eq!(ceil_log2(17112), 15);
        assert_eq!(ceil_log2(64), 6);
    }
}
