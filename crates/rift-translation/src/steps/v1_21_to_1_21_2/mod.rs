//! Minecraft 1.21 (protocol 767) and 1.21.2 (protocol 768).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 767;
pub const NEWER: i32 = 768;

/// A 1.21.2 client on a 1.21 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21 client on a 1.21.2 backend.
pub const DOWN: Option<Make> = down::STEP;
