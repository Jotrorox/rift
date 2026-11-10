//! Minecraft 1.14.1 (protocol 480) and 1.14.2 (protocol 485).
//!
//! The two protocols share packet IDs, layouts and every registry that
//! travels on the wire: everything passes through.
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 480;
pub const NEWER: i32 = 485;

/// A 1.14.2 client on a 1.14.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.14.1 client on a 1.14.2 backend.
pub const DOWN: Option<Make> = down::STEP;
