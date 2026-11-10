//! Minecraft 1.16.1 (protocol 736) and 1.16.2 (protocol 751).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 736;
pub const NEWER: i32 = 751;

/// A 1.16.2 client on a 1.16.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.16.1 client on a 1.16.2 backend.
pub const DOWN: Option<Make> = down::STEP;
