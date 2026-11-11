//! Minecraft 1.15 (protocol 573) and 1.15.1 (protocol 575).
//!
//! The two protocols have the same packets, layouts and registries (checked
//! by `identical_protocols` in `v1_14_4_to_1_15::support`).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 573;
pub const NEWER: i32 = 575;

/// A 1.15.1 client on a 1.15 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.15 client on a 1.15.1 backend.
pub const DOWN: Option<Make> = down::STEP;
