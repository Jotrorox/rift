//! Bounded skipping of named NBT used by pre-configuration dimension registries.
//! Values are copied verbatim; Rift does not interpret or rewrite registries.
use super::{invalid, packets::take};
use std::io;

fn string(bytes: &mut &[u8]) -> io::Result<()> {
    let length = u16::from_be_bytes(take(bytes, 2)?.try_into().unwrap());
    take(bytes, usize::from(length))?;
    Ok(())
}

fn payload(bytes: &mut &[u8], tag: u8, depth: usize) -> io::Result<()> {
    if depth > 64 {
        return Err(invalid("NBT nesting limit exceeded"));
    }
    match tag {
        1..=6 => {
            take(bytes, [1, 2, 4, 8, 4, 8][usize::from(tag - 1)])?;
        }
        7 | 11 | 12 => {
            let count = i32::from_be_bytes(take(bytes, 4)?.try_into().unwrap());
            let width = match tag {
                7 => 1,
                11 => 4,
                _ => 8,
            };
            let length = usize::try_from(count)
                .ok()
                .and_then(|n| n.checked_mul(width))
                .ok_or_else(|| invalid("invalid NBT array length"))?;
            take(bytes, length)?;
        }
        8 => string(bytes)?,
        9 => {
            let kind = take(bytes, 1)?[0];
            let count = i32::from_be_bytes(take(bytes, 4)?.try_into().unwrap());
            if count < 0 || count as usize > bytes.len() || (count > 0 && kind == 0) {
                return Err(invalid("invalid NBT list"));
            }
            for _ in 0..count {
                payload(bytes, kind, depth + 1)?;
            }
        }
        10 => loop {
            let kind = take(bytes, 1)?[0];
            if kind == 0 {
                break;
            }
            string(bytes)?;
            payload(bytes, kind, depth + 1)?;
        },
        _ => return Err(invalid("invalid NBT tag")),
    }
    Ok(())
}

pub(super) fn named<'a>(bytes: &mut &'a [u8]) -> io::Result<&'a [u8]> {
    let start = *bytes;
    let kind = take(bytes, 1)?[0];
    if kind != 10 {
        return Err(invalid("expected NBT compound"));
    }
    string(bytes)?;
    payload(bytes, kind, 0)?;
    Ok(&start[..start.len() - bytes.len()])
}
