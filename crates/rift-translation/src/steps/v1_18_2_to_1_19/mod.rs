//! Minecraft 1.18.2 (protocol 758) and 1.19 (protocol 759).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 758;
pub const NEWER: i32 = 759;

/// A 1.19 client on a 1.18.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.18.2 client on a 1.19 backend.
pub const DOWN: Option<Make> = down::STEP;
