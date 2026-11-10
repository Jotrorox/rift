//! Minecraft 1.12 (protocol 335) and 1.12.1 (protocol 338).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 335;
pub const NEWER: i32 = 338;

/// A 1.12.1 client on a 1.12 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.12 client on a 1.12.1 backend.
pub const DOWN: Option<Make> = down::STEP;
