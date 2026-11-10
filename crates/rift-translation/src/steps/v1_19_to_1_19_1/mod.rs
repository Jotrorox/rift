//! Minecraft 1.19 (protocol 759) and 1.19.1 (protocol 760).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 759;
pub const NEWER: i32 = 760;

/// A 1.19.1 client on a 1.19 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.19 client on a 1.19.1 backend.
pub const DOWN: Option<Make> = down::STEP;
