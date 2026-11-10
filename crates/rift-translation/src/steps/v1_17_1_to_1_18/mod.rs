//! Minecraft 1.17.1 (protocol 756) and 1.18 (protocol 757).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 756;
pub const NEWER: i32 = 757;

/// A 1.18 client on a 1.17.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.17.1 client on a 1.18 backend.
pub const DOWN: Option<Make> = down::STEP;
