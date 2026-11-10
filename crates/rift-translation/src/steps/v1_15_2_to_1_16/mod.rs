//! Minecraft 1.15.2 (protocol 578) and 1.16 (protocol 735).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 578;
pub const NEWER: i32 = 735;

/// A 1.16 client on a 1.15.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.15.2 client on a 1.16 backend.
pub const DOWN: Option<Make> = down::STEP;
