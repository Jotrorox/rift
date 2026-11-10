//! Minecraft 1.14.4 (protocol 498) and 1.15 (protocol 573).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 498;
pub const NEWER: i32 = 573;

/// A 1.15 client on a 1.14.4 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.14.4 client on a 1.15 backend.
pub const DOWN: Option<Make> = down::STEP;
