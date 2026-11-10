//! Minecraft 1.21.4 (protocol 769) and 1.21.5 (protocol 770).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 769;
pub const NEWER: i32 = 770;

/// A 1.21.5 client on a 1.21.4 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.4 client on a 1.21.5 backend.
pub const DOWN: Option<Make> = down::STEP;
