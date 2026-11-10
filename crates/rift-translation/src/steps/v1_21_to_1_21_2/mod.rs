//! Minecraft 1.21 (protocol 767) and 1.21.2/1.21.3 (protocol 768).
//!
//! What the pair itself changes, beyond what the shared codecs convert:
//!
//! - Login Success loses its strict error handling flag; client information
//!   gains a particle setting; `login` and `respawn` gain the sea level.
//! - Teleports carry a velocity and 32 relative flags; entity teleports split
//!   into `entity_position_sync` (periodic) and `teleport_entity` (a real
//!   teleport, relative only for a player's own vehicle); the server can turn
//!   the player with `player_rotation`; experimental minecarts move along
//!   tracks.
//! - Window IDs become VarInts; the cursor and the player's inventory get
//!   their own packets instead of `container_set_slot` windows -1 and -2.
//! - The recipe book is rebuilt: the server no longer sends recipes, only
//!   displays of the ones the player knows, numbered per connection, plus
//!   the stonecutter's recipes and the item sets furnaces and smithing tables
//!   accept. The steps keep both numberings (see [`recipes`]).
//! - Movement packets carry a horizontal collision flag, input is a set of
//!   keys sent whether or not the player rides, and clients mark the end of
//!   each tick.
//!
//! Item stacks inside packets this pair owns go through [`stacks`]: a stack
//! without data components only needs its item ID mapped; anything else is
//! handed to the item codec.
mod down;
mod recipes;
mod stacks;
#[cfg(test)]
mod tests;
mod up;

use super::Make;
use crate::{
    Direction, State, data,
    wire::{Rewriter, invalid},
};
use std::io;

pub const OLDER: i32 = 767;
pub const NEWER: i32 = 768;

/// A 1.21.2 client on a 1.21 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21 client on a 1.21.2 backend.
pub const DOWN: Option<Make> = down::STEP;

/// `Relative` bits of a 1.21.2 teleport; 1.21 has the first five.
const RELATIVE_X: i32 = 1 << 0;
const RELATIVE_Y: i32 = 1 << 1;
const RELATIVE_Z: i32 = 1 << 2;
const RELATIVE_Y_ROT: i32 = 1 << 3;
const RELATIVE_X_ROT: i32 = 1 << 4;
const RELATIVE_DELTA_X: i32 = 1 << 5;
const RELATIVE_DELTA_Y: i32 = 1 << 6;
const RELATIVE_DELTA_Z: i32 = 1 << 7;
const RELATIVE_POSITION: i32 = RELATIVE_X | RELATIVE_Y | RELATIVE_Z;
const RELATIVE_DELTA: i32 = RELATIVE_DELTA_X | RELATIVE_DELTA_Y | RELATIVE_DELTA_Z;

/// The sea level every 1.21 world reports (`Level.getSeaLevel`); 1.21.2
/// sends the dimension's own.
const SEA_LEVEL: i32 = 63;

/// An angle packed into a byte the way both versions do it.
fn pack_degrees(degrees: f32) -> i8 {
    (degrees * 256.0 / 360.0).floor() as i64 as i8
}

fn unpack_degrees(packed: i8) -> f32 {
    f32::from(packed) * 360.0 / 256.0
}

/// The ID of a packet that must exist in `protocol`.
fn packet_id(protocol: i32, state: State, direction: Direction, name: &str) -> i32 {
    data::version(protocol).expect_packet(state, direction, name)
}

/// The ID of a built-in registry entry that must exist in `protocol`.
fn registry_id(protocol: i32, registry: &str, name: &str) -> i32 {
    data::version(protocol)
        .expect_registry(registry)
        .id(name)
        .unwrap_or_else(|| panic!("protocol {protocol} has no {registry} {name}"))
}

/// Copy a window ID from a 1.21 unsigned byte to a 1.21.2 VarInt.
fn window_to_varint(rw: &mut Rewriter<'_>, signed: bool) -> io::Result<i32> {
    let window = if signed {
        i32::from(rw.r.i8()?)
    } else {
        i32::from(rw.r.u8()?)
    };
    rw.w.varint(window);
    Ok(window)
}

/// Copy a window ID from a 1.21.2 VarInt to a 1.21 byte. 1.21.2 servers
/// number windows 1 to 100, so a value that does not fit is malformed.
fn window_to_byte(rw: &mut Rewriter<'_>, signed: bool) -> io::Result<i32> {
    let window = rw.r.varint()?;
    if signed {
        let byte = i8::try_from(window).map_err(|_| invalid("window ID out of range"))?;
        rw.w.i8(byte);
    } else {
        let byte = u8::try_from(window).map_err(|_| invalid("window ID out of range"))?;
        rw.w.u8(byte);
    }
    Ok(window)
}

/// Copy the `CommonPlayerSpawnInfo` of `login` and `respawn` from `from`'s
/// layout to `to`'s: 1.21.2 appends the sea level.
fn spawn_info(rw: &mut Rewriter<'_>, from: i32, to: i32) -> io::Result<()> {
    // INTEGRATION: the dimension type is a network ID of the synchronised
    // registry; map it through `shared.registries` once the registries codec
    // exposes its bookkeeping. Vanilla sends the same four types in the same
    // order on both versions.
    rw.varint()?;
    rw.string(32767)?; // Dimension name.
    rw.i64()?; // Hashed seed.
    rw.u8()?; // Game mode.
    rw.i8()?; // Previous game mode.
    rw.bool()?; // Debug.
    rw.bool()?; // Flat.
    rw.optional(|rw| {
        rw.string(32767)?;
        rw.i64()?;
        Ok(())
    })?;
    rw.varint()?; // Portal cooldown.
    let sea_level = if from >= NEWER {
        rw.r.varint()?
    } else {
        SEA_LEVEL
    };
    if to >= NEWER {
        rw.w.varint(sea_level);
    }
    Ok(())
}

/// Convert a `login` body; returns the player's entity ID.
fn login(body: &[u8], from: i32, to: i32) -> io::Result<(i32, Vec<u8>)> {
    let mut rw = Rewriter::new(body);
    let entity = rw.i32()?;
    rw.bool()?; // Hardcore.
    rw.list(|rw| rw.string(32767).map(drop))?; // Dimension names.
    rw.varint()?; // Max players.
    rw.varint()?; // View distance.
    rw.varint()?; // Simulation distance.
    rw.bool()?; // Reduced debug info.
    rw.bool()?; // Respawn screen.
    rw.bool()?; // Limited crafting.
    spawn_info(&mut rw, from, to)?;
    rw.bool()?; // Enforces secure chat.
    Ok((entity, rw.finish()?))
}

fn respawn(body: &[u8], from: i32, to: i32) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    spawn_info(&mut rw, from, to)?;
    rw.u8()?; // Data to keep.
    rw.finish()
}

/// Convert a `login_finished` body: 1.21 ends it with the strict error
/// handling flag. 1.21.2 clients always handle errors strictly, so that is
/// what an older client is told.
fn login_finished(body: &[u8], from: i32, to: i32) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    rw.uuid()?;
    rw.string(16)?;
    rw.list(|rw| {
        rw.string(64)?;
        rw.string(32767)?;
        rw.optional(|rw| rw.string(1024).map(drop))
    })?;
    if from < NEWER {
        rw.r.bool()?;
    }
    if to < NEWER {
        rw.w.bool(true);
    }
    rw.finish()
}

/// Convert a `client_information` body: 1.21.2 appends the particle setting
/// ("all" for an older client).
fn client_information(body: &[u8], from: i32, to: i32) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    rw.string(16)?; // Language.
    rw.i8()?; // View distance.
    rw.varint()?; // Chat visibility.
    rw.bool()?; // Chat colours.
    rw.u8()?; // Skin parts.
    rw.varint()?; // Main hand.
    rw.bool()?; // Text filtering.
    rw.bool()?; // Allow listing.
    let particles = if from >= NEWER { rw.r.varint()? } else { 0 };
    if to >= NEWER {
        rw.w.varint(particles);
    }
    rw.finish()
}

/// Convert a `set_time` body. 1.21 marks a frozen day time by negating it
/// (and sends -1 for a frozen 0); 1.21.2 has a flag.
fn set_time(body: &[u8], from: i32, to: i32) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    rw.i64()?; // Game time.
    let (time, ticking) = if from >= NEWER {
        (rw.r.i64()?, rw.r.bool()?)
    } else {
        match rw.r.i64()? {
            time if time < 0 => (time.checked_neg().unwrap_or(i64::MAX), false),
            time => (time, true),
        }
    };
    if to >= NEWER {
        rw.w.i64(time).bool(ticking);
    } else if ticking {
        rw.w.i64(time.max(0));
    } else {
        rw.w.i64(if time == 0 { -1 } else { -time.abs() });
    }
    rw.finish()
}

/// Convert a `use_item_on` body: 1.21.2 says whether the player clicked the
/// world border, just before the sequence number.
fn use_item_on(body: &[u8], from: i32, to: i32) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    rw.varint()?; // Hand.
    rw.i64()?; // Position.
    rw.varint()?; // Face.
    rw.f32()?;
    rw.f32()?;
    rw.f32()?;
    rw.bool()?; // Inside.
    let border = from >= NEWER && rw.r.bool()?;
    if to >= NEWER {
        rw.w.bool(border);
    }
    rw.varint()?; // Sequence.
    rw.finish()
}
