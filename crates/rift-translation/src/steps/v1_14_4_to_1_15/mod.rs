//! Minecraft 1.14.4 (protocol 498) and 1.15 (protocol 573).
//!
//! 1.15 adds the hashed world seed to `login` and `respawn`, the "enable
//! respawn screen" flag to `login` (switched later through `game_event` 11,
//! the `doImmediateRespawn` game rule), and moves the explosion sound from
//! the server to the client. Spawn packets without entity data, 3D biomes,
//! bees and particle positions as doubles are the shared codecs' part.
mod down;
pub(in crate::steps) mod support;
#[cfg(test)]
mod tests;
mod up;

use super::Make;

pub const OLDER: i32 = 498;
pub const NEWER: i32 = 573;

/// A 1.15 client on a 1.14.4 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.14.4 client on a 1.15 backend.
pub const DOWN: Option<Make> = down::STEP;

/// `game_event` reason: the `doImmediateRespawn` game rule changed.
const IMMEDIATE_RESPAWN: u8 = 11;
