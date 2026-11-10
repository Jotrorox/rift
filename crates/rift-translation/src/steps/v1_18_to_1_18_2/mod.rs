//! Minecraft 1.18 (protocol 757) and 1.18.2 (protocol 758).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 757;
pub const NEWER: i32 = 758;

/// A 1.18.2 client on a 1.18 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.18 client on a 1.18.2 backend.
pub const DOWN: Option<Make> = down::STEP;
