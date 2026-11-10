//! Minecraft 1.13.1 (protocol 401) and 1.13.2 (protocol 404).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 401;
pub const NEWER: i32 = 404;

/// A 1.13.2 client on a 1.13.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.13.1 client on a 1.13.2 backend.
pub const DOWN: Option<Make> = down::STEP;
