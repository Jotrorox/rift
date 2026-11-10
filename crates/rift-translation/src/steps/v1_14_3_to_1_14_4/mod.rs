//! Minecraft 1.14.3 (protocol 490) and 1.14.4 (protocol 498).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 490;
pub const NEWER: i32 = 498;

/// A 1.14.4 client on a 1.14.3 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.14.3 client on a 1.14.4 backend.
pub const DOWN: Option<Make> = down::STEP;
