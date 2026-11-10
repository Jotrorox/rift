//! Minecraft 1.16.3 (protocol 753) and 1.16.4 (protocol 754).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 753;
pub const NEWER: i32 = 754;

/// A 1.16.4 client on a 1.16.3 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.16.3 client on a 1.16.4 backend.
pub const DOWN: Option<Make> = down::STEP;
