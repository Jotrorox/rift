//! Minecraft 1.20 (protocol 763) and 1.20.2 (protocol 764).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 763;
pub const NEWER: i32 = 764;

/// A 1.20.2 client on a 1.20 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.20 client on a 1.20.2 backend.
pub const DOWN: Option<Make> = down::STEP;
