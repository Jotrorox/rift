//! Minecraft 1.16 (protocol 735) and 1.16.1 (protocol 736).
//!
//! The two protocols have the same packets, layouts and registries (checked
//! by `identical_protocols` in `v1_14_4_to_1_15::support`).
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 735;
pub const NEWER: i32 = 736;

/// A 1.16.1 client on a 1.16 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.16 client on a 1.16.1 backend.
pub const DOWN: Option<Make> = down::STEP;
