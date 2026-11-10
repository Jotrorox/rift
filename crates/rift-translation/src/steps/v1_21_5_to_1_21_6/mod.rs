//! Minecraft 1.21.5 (protocol 770) and 1.21.6 (protocol 771).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 770;
pub const NEWER: i32 = 771;

/// A 1.21.6 client on a 1.21.5 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.5 client on a 1.21.6 backend.
pub const DOWN: Option<Make> = down::STEP;
