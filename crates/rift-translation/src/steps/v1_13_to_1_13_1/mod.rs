//! Minecraft 1.13 (protocol 393) and 1.13.1 (protocol 401).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 393;
pub const NEWER: i32 = 401;

/// A 1.13.1 client on a 1.13 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.13 client on a 1.13.1 backend.
pub const DOWN: Option<Make> = down::STEP;
