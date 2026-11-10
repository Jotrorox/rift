//! Minecraft 26.1 (protocol 775) and 26.2 (protocol 776).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 775;
pub const NEWER: i32 = 776;

/// A 26.2 client on a 26.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 26.1 client on a 26.2 backend.
pub const DOWN: Option<Make> = down::STEP;
