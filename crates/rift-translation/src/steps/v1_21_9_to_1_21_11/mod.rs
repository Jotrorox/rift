//! Minecraft 1.21.9 (protocol 773) and 1.21.11 (protocol 774).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 773;
pub const NEWER: i32 = 774;

/// A 1.21.11 client on a 1.21.9 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.9 client on a 1.21.11 backend.
pub const DOWN: Option<Make> = down::STEP;
