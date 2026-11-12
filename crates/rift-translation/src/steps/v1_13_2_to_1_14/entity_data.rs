//! The two entity data fields sleeping moved into in 1.14: the sleeping
//! position of every living entity (index 12) and the pose of every entity
//! (index 6). Everything else in entity data is the entity codec's.
use super::layout::read_position;
use crate::{
    data,
    wire::{Position, Reader, Writer, invalid},
};
use std::io;

/// Living entities' optional sleeping position in 1.14.
const SLEEPING_INDEX: u8 = 12;
const POSE_INDEX: u8 = 6;
/// Data serializer IDs of 1.14.
const OPTIONAL_POSITION: i32 = 10;
const POSE: i32 = 18;
const END: u8 = 0xff;

/// Poses 1.14 shows other players in.
pub(super) const STANDING: i32 = 0;
pub(super) const SLEEPING: i32 = 2;

/// Read an entity data list (1.13 to 1.14.4 layouts) to its end and return
/// the sleeping position it sets: `Some(None)` when it clears it, `None`
/// when it does not touch it. Only meaningful for living entities in 1.14.
pub(super) fn sleeping(protocol: i32, r: &mut Reader<'_>) -> io::Result<Option<Option<Position>>> {
    let mut found = None;
    loop {
        let index = r.u8()?;
        if index == END {
            return Ok(found);
        }
        let kind = r.varint()?;
        if index == SLEEPING_INDEX && kind == OPTIONAL_POSITION && protocol >= 477 {
            found = Some(if r.bool()? {
                Some(read_position(r, protocol)?)
            } else {
                None
            });
        } else {
            skip(protocol, kind, r)?;
        }
    }
}

/// Skip one value of data serializer `kind`.
fn skip(protocol: i32, kind: i32, r: &mut Reader<'_>) -> io::Result<()> {
    match kind {
        0 => drop(r.u8()?),
        1 | 11 | 13 => drop(r.varint()?),
        2 => drop(r.f32()?),
        // Strings and JSON text components.
        3 | 4 => drop(r.string(262_144)?),
        5 => {
            if r.bool()? {
                r.string(262_144)?;
            }
        }
        6 => skip_stack(r)?,
        7 => drop(r.bool()?),
        8 => drop(r.take(12)?),
        9 => drop(r.i64()?),
        10 => {
            if r.bool()? {
                r.i64()?;
            }
        }
        12 => {
            if r.bool()? {
                r.uuid()?;
            }
        }
        14 => drop(r.nbt_bytes(true)?),
        15 => skip_particle(protocol, r)?,
        16 if protocol >= 477 => {
            r.varint()?;
            r.varint()?;
            r.varint()?;
        }
        17 | 18 if protocol >= 477 => drop(r.varint()?),
        _ => return Err(invalid("unknown entity data type")),
    }
    Ok(())
}

/// An item stack of 1.13.2 to 1.20.4: present flag, item, count, tag.
fn skip_stack(r: &mut Reader<'_>) -> io::Result<()> {
    if r.bool()? {
        r.varint()?;
        r.i8()?;
        r.nbt_bytes(true)?;
    }
    Ok(())
}

fn skip_particle(protocol: i32, r: &mut Reader<'_>) -> io::Result<()> {
    let id = r.varint()?;
    let name = data::version(protocol)
        .expect_registry("particle_type")
        .name(id)
        .ok_or_else(|| invalid("unknown particle"))?;
    match name {
        "block" | "falling_dust" => drop(r.varint()?),
        "dust" => drop(r.take(16)?),
        "item" => skip_stack(r)?,
        _ => {}
    }
    Ok(())
}

/// A 1.14 `set_entity_data` body setting the sleeping position and, when
/// given, the pose.
pub(super) fn sleep_data(entity: i32, position: Option<Position>, pose: Option<i32>) -> Vec<u8> {
    let mut w = Writer::with_capacity(24);
    w.varint(entity);
    if let Some(pose) = pose {
        w.u8(POSE_INDEX).varint(POSE).varint(pose);
    }
    w.u8(SLEEPING_INDEX).varint(OPTIONAL_POSITION);
    match position {
        Some(position) => w.bool(true).position(position),
        None => w.bool(false),
    };
    w.u8(END);
    w.buf
}
