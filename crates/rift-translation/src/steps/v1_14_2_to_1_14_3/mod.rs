//! Minecraft 1.14.2 (protocol 485) and 1.14.3 (protocol 490).
//!
//! The two protocols share packet IDs. `merchant_offers` gained the
//! can-restock flag and 1.14.3 added the `crafting_special_repairitem`
//! recipe type; both are the item codec's. Everything else passes through.
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 485;
pub const NEWER: i32 = 490;

/// A 1.14.3 client on a 1.14.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.14.2 client on a 1.14.3 backend.
pub const DOWN: Option<Make> = down::STEP;
