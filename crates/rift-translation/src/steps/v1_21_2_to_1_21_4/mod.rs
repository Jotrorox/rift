//! Minecraft 1.21.2 (protocol 768) and 1.21.4 (protocol 769).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 768;
pub const NEWER: i32 = 769;

/// A 1.21.4 client on a 1.21.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.2 client on a 1.21.4 backend.
pub const DOWN: Option<Make> = down::STEP;
