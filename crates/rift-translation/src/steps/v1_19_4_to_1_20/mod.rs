//! Minecraft 1.19.4 (protocol 762) and 1.20 (protocol 763).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 762;
pub const NEWER: i32 = 763;

/// A 1.20 client on a 1.19.4 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.19.4 client on a 1.20 backend.
pub const DOWN: Option<Make> = down::STEP;
