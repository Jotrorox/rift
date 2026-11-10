//! Minecraft 1.13.2 (protocol 404) and 1.14 (protocol 477).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 404;
pub const NEWER: i32 = 477;

/// A 1.14 client on a 1.13.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.13.2 client on a 1.14 backend.
pub const DOWN: Option<Make> = down::STEP;
