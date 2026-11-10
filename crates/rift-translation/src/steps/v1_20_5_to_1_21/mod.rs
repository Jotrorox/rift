//! Minecraft 1.20.5 (protocol 766) and 1.21 (protocol 767).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 766;
pub const NEWER: i32 = 767;

/// A 1.21 client on a 1.20.5 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.20.5 client on a 1.21 backend.
pub const DOWN: Option<Make> = down::STEP;
