//! Minecraft 1.18/1.18.1 (protocol 757) and 1.18.2 (protocol 758).
//!
//! 1.18.2 names the blocks that burn forever in a dimension by a tag
//! reference (the registries codec converts the dimension types of `login`
//! and `respawn`), sends mob effect IDs as VarInts (the entity codec's) and
//! registry tags of every registry (the misc codec's). Everything else is
//! unchanged.
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;

pub const OLDER: i32 = 757;
pub const NEWER: i32 = 758;

/// A 1.18.2 client on a 1.18 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.18 client on a 1.18.2 backend.
pub const DOWN: Option<Make> = down::STEP;
