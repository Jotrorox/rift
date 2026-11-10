//! Minecraft 1.19.1 (protocol 760) and 1.19.3 (protocol 761).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 760;
pub const NEWER: i32 = 761;

/// A 1.19.3 client on a 1.19.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.19.1 client on a 1.19.3 backend.
pub const DOWN: Option<Make> = down::STEP;
