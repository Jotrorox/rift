//! Minecraft 1.12.2 (protocol 340) and 1.13 (protocol 393).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 340;
pub const NEWER: i32 = 393;

/// A 1.13 client on a 1.12.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.12.2 client on a 1.13 backend.
pub const DOWN: Option<Make> = down::STEP;
