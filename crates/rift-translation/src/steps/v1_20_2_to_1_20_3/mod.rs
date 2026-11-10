//! Minecraft 1.20.2 (protocol 764) and 1.20.3 (protocol 765).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 764;
pub const NEWER: i32 = 765;

/// A 1.20.3 client on a 1.20.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.20.2 client on a 1.20.3 backend.
pub const DOWN: Option<Make> = down::STEP;
