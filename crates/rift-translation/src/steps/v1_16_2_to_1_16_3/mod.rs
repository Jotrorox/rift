//! Minecraft 1.16.2 (protocol 751) and 1.16.3 (protocol 753).
//!
//! The two protocols have the same packets, layouts and registries (checked
//! by `identical_protocols` in `v1_14_4_to_1_15::support`).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 751;
pub const NEWER: i32 = 753;

/// A 1.16.3 client on a 1.16.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.16.2 client on a 1.16.3 backend.
pub const DOWN: Option<Make> = down::STEP;
