//! Minecraft 1.17 (protocol 755) and 1.17.1 (protocol 756).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 755;
pub const NEWER: i32 = 756;

/// A 1.17.1 client on a 1.17 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.17 client on a 1.17.1 backend.
pub const DOWN: Option<Make> = down::STEP;
