//! Minecraft 1.19.3 (protocol 761) and 1.19.4 (protocol 762).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 761;
pub const NEWER: i32 = 762;

/// A 1.19.4 client on a 1.19.3 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.19.3 client on a 1.19.4 backend.
pub const DOWN: Option<Make> = down::STEP;
