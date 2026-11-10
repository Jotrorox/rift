//! Minecraft 1.21.11 (protocol 774) and 26.1 (protocol 775).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 774;
pub const NEWER: i32 = 775;

/// A 26.1 client on a 1.21.11 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.11 client on a 26.1 backend.
pub const DOWN: Option<Make> = down::STEP;
