//! Minecraft 1.14 (protocol 477) and 1.14.1 (protocol 480).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 477;
pub const NEWER: i32 = 480;

/// A 1.14.1 client on a 1.14 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.14 client on a 1.14.1 backend.
pub const DOWN: Option<Make> = down::STEP;
