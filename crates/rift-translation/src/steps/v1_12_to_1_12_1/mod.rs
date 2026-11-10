//! Minecraft 1.12 (protocol 335) and 1.12.1 (protocol 338).
//!
//! The two protocols differ only in recipe book placement, which 1.12.1
//! moved from the client to the server. A 1.12 client works out which items
//! to move into the crafting grid itself and sends the moves
//! (`crafting_recipe_placement`); a 1.12.1 client only names the recipe
//! (`place_recipe`) and the server fills the grid or, when the player lacks
//! ingredients, shows the recipe as a ghost (`place_ghost_recipe`). The
//! inserted packets shift the IDs of everything after them, which the routes
//! take care of; every other packet, recipe IDs included (both versions load
//! the same 433 recipes), is unchanged.
//!
//! Neither request can be expressed in the other version's terms without
//! the player's inventory and the recipes' ingredients, so a placement only
//! ever shows a ghost recipe (see `up`) or does nothing (see `down`).
mod down;
/// Shared by the tests of every step from 1.12 to 1.13.2.
#[cfg(test)]
pub(super) mod testing;
#[cfg(test)]
mod tests;
mod up;

use super::Make;

pub const OLDER: i32 = 335;
pub const NEWER: i32 = 338;

/// A 1.12.1 client on a 1.12 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.12 client on a 1.12.1 backend.
pub const DOWN: Option<Make> = down::STEP;
