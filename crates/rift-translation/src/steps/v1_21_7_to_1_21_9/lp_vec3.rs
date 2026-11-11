//! The packed low-precision vector 1.21.9 introduced for velocities
//! (`LpVec3`): three 15-bit components in units of a shared integer scale,
//! six bytes in all, with a VarInt continuation for scales from 4 on and a
//! single zero byte for a vector too small to tell from zero. The layout
//! follows the 1.21.10 server's codec.
use crate::wire::{Reader, Writer};
use std::io;

const DATA_MASK: u64 = 0x7fff;
const MAX_QUANTIZED: f64 = 32766.0;
const SCALE_MASK: u64 = 3;
const CONTINUATION: u64 = 4;
const OFFSETS: [u32; 3] = [3, 18, 33];
/// Components are clamped to this magnitude before packing.
const ABS_MAX: f64 = 1.7179869183E10;
/// Below this magnitude every component encodes as zero.
const ABS_MIN: f64 = 3.051944088384301E-5;

pub fn read(r: &mut Reader<'_>) -> io::Result<[f64; 3]> {
    let lowest = u64::from(r.u8()?);
    if lowest == 0 {
        return Ok([0.0; 3]);
    }
    let middle = u64::from(r.u8()?);
    let highest = u64::from(r.i32()? as u32);
    let packed = highest << 16 | middle << 8 | lowest;
    let mut scale = lowest & SCALE_MASK;
    if lowest & CONTINUATION != 0 {
        scale |= u64::from(r.varint()? as u32) << 2;
    }
    let scale = scale as f64;
    Ok(OFFSETS.map(|offset| unpack(packed >> offset) * scale))
}

pub fn write(w: &mut Writer, vector: [f64; 3]) {
    let vector = vector.map(|component| {
        if component.is_nan() {
            0.0
        } else {
            component.clamp(-ABS_MAX, ABS_MAX)
        }
    });
    let largest = vector
        .iter()
        .fold(0.0f64, |largest, c| largest.max(c.abs()));
    if largest < ABS_MIN {
        w.u8(0);
        return;
    }
    let scale = ceil(largest);
    let partial = scale & SCALE_MASK != scale;
    let mut packed = if partial {
        scale & SCALE_MASK | CONTINUATION
    } else {
        scale
    };
    for (component, offset) in vector.into_iter().zip(OFFSETS) {
        packed |= pack(component / scale as f64) << offset;
    }
    w.u8(packed as u8)
        .u8((packed >> 8) as u8)
        .i32((packed >> 16) as i32);
    if partial {
        w.varint((scale >> 2) as i32);
    }
}

/// `Mth.ceilLong` for the positive magnitudes `write` passes.
fn ceil(value: f64) -> u64 {
    let truncated = value as u64;
    if value > truncated as f64 {
        truncated + 1
    } else {
        truncated
    }
}

fn pack(value: f64) -> u64 {
    // Non-negative here, where Java's Math.round and Rust's round agree.
    ((value * 0.5 + 0.5) * MAX_QUANTIZED).round() as u64
}

fn unpack(value: u64) -> f64 {
    ((value & DATA_MASK) as f64).min(MAX_QUANTIZED) * 2.0 / MAX_QUANTIZED - 1.0
}
