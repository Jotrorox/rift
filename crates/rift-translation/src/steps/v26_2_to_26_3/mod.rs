//! Minecraft 26.2 (protocol 776) and 26.3 (protocol 777).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 776;
pub const NEWER: i32 = 777;

/// A 26.3 client on a 26.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 26.2 client on a 26.3 backend.
pub const DOWN: Option<Make> = down::STEP;
