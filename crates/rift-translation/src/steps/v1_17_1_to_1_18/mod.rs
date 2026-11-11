//! Minecraft 1.17.1 (protocol 756) and 1.18/1.18.1 (protocol 757).
//!
//! 1.18 sends every section of a chunk with paletted biomes, and the
//! chunk's light inside the chunk packet (the chunk codec's, which holds a
//! 1.17.1 server's light until its chunk comes and splits a 1.18 chunk into
//! its light and the chunk). It tells the client how far the server
//! simulates the world (`set_simulation_distance` and a join game field),
//! lets clients refuse to be listed in the server list, lifted the length
//! limits of scoreboard names and dropped the terrain shape of biomes from
//! the dimension codec. A 1.17.1 client can display the taller 1.18 world:
//! since 1.17 the dimension type states the world's extent.
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;

pub const OLDER: i32 = 756;
pub const NEWER: i32 = 757;

/// A 1.18 client on a 1.17.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.17.1 client on a 1.18 backend.
pub const DOWN: Option<Make> = down::STEP;
