//! Minecraft 1.16.4 (protocol 754) and 1.17 (protocol 755).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 754;
pub const NEWER: i32 = 755;

/// A 1.17 client on a 1.16.4 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.16.4 client on a 1.17 backend.
pub const DOWN: Option<Make> = down::STEP;
