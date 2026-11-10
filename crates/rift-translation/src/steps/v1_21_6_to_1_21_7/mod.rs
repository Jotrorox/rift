//! Minecraft 1.21.6 (protocol 771) and 1.21.7 (protocol 772).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 771;
pub const NEWER: i32 = 772;

/// A 1.21.7 client on a 1.21.6 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.6 client on a 1.21.7 backend.
pub const DOWN: Option<Make> = down::STEP;
