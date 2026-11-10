//! Minecraft 1.21.7 (protocol 772) and 1.21.9 (protocol 773).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 772;
pub const NEWER: i32 = 773;

/// A 1.21.9 client on a 1.21.7 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.7 client on a 1.21.9 backend.
pub const DOWN: Option<Make> = down::STEP;
