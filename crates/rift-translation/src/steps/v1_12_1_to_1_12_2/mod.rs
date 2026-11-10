//! Minecraft 1.12.1 (protocol 338) and 1.12.2 (protocol 340).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 338;
pub const NEWER: i32 = 340;

/// A 1.12.2 client on a 1.12.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.12.1 client on a 1.12.2 backend.
pub const DOWN: Option<Make> = down::STEP;
