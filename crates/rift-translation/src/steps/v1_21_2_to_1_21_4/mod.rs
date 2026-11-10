//! Minecraft 1.21.2/1.21.3 (protocol 768) and 1.21.4 (protocol 769).
//!
//! What changed for the step, learnt from both servers' traffic and
//! Mojang's code: the held slot is a VarInt instead of a byte, vehicle moves
//! carry an on-ground flag, the winter drop feature flag is gone (its content
//! became vanilla), and picking blocks moved to the server: the client names
//! the block or entity (`pick_item_from_block`/`_entity`) instead of the
//! inventory slot (`pick_item`), and tells the server once it has loaded the
//! world (`player_loaded`), which keeps the player invulnerable until then.
//! Items, particles, the player list and registries are the shared codecs'.
mod down;
#[cfg(test)]
pub(super) mod harness;
#[cfg(test)]
mod tests;
mod up;

use super::Make;
use crate::wire::{Reader, Writer, invalid};
use std::io;

pub const OLDER: i32 = 768;
pub const NEWER: i32 = 769;

/// A 1.21.4 client on a 1.21.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.2 client on a 1.21.4 backend.
pub const DOWN: Option<Make> = down::STEP;

/// The feature flag 1.21.2 kept the pale garden behind; 1.21.4 has no such
/// flag and ignores it with a warning.
const WINTER_DROP: &str = "minecraft:winter_drop";

/// `set_held_slot`: a byte before 1.21.4, a VarInt from then on. The slot is
/// a hotbar index; anything a byte cannot carry is not one.
fn held_slot(body: &[u8], towards_newer: bool) -> io::Result<Vec<u8>> {
    let mut reader = Reader::new(body);
    let slot = if towards_newer {
        i32::from(reader.i8()?)
    } else {
        reader.varint()?
    };
    reader.finish()?;
    let mut writer = Writer::new();
    if towards_newer {
        writer.varint(slot);
    } else {
        let slot = i8::try_from(slot).map_err(|_| invalid("held slot out of range"))?;
        writer.i8(slot);
    }
    Ok(writer.buf)
}

/// `update_enabled_features` without the winter drop flag for 1.21.4, and
/// with it for 1.21.2, whose client otherwise treats the pale garden blocks
/// and items a 1.21.4 server sends as disabled.
fn enabled_features(body: &[u8], towards_newer: bool) -> io::Result<Vec<u8>> {
    let mut reader = Reader::new(body);
    let mut features = Vec::new();
    for _ in 0..reader.count()? {
        features.push(reader.str()?);
    }
    reader.finish()?;
    features.retain(|feature| *feature != WINTER_DROP);
    if !towards_newer {
        features.push(WINTER_DROP);
    }
    let mut writer = Writer::new();
    writer.varint(features.len() as i32);
    for feature in features {
        writer.string(feature);
    }
    Ok(writer.buf)
}

/// Serverbound `move_vehicle`: 1.21.4 appends whether the vehicle is on the
/// ground. Older clients do not say; the server uses the flag for fall
/// damage and jumping, so a vehicle is taken to be on the ground, as before
/// 1.21.4 when the server did not ask.
fn move_vehicle(body: &[u8], towards_newer: bool) -> io::Result<Vec<u8>> {
    let mut reader = Reader::new(body);
    let position = reader.take(3 * 8 + 2 * 4)?;
    let mut writer = Writer::new();
    writer.bytes(position);
    if towards_newer {
        writer.bool(true);
    } else {
        reader.bool()?;
    }
    reader.finish()?;
    Ok(writer.buf)
}
