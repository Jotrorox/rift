//! Minecraft 1.19.4 (protocol 762) and 1.20/1.20.1 (protocol 763).
//!
//! What the pair changes, and who converts it:
//!
//! - Join game and respawn end with a portal cooldown from 1.20 (a newer
//!   client gets none). The registries codec gives 1.20 clients the
//!   `generic_kill` and `outside_border` damage types, and 1.19.4 clients
//!   the 1.20 trim patterns with stand-ins for templates they lack.
//! - Signs get a back side: `open_sign_editor` says which side to edit and
//!   `sign_update` which side was edited. A 1.19.4 client edits the side
//!   the server asked for; a 1.20 client never edits the back of a sign on
//!   a 1.19.4 server, which has none.
//! - `player_combat_end` loses its unused killer ID.
//! - 1.20 makes the `update_1_20` experiment (cherry trees, sniffers,
//!   camels, trims...) part of the game: a 1.19.4 client is told it is
//!   enabled, so those blocks and items work, and a 1.20 client is not told
//!   about a flag it no longer knows.
//! - Left to the shared codecs: `player_combat_kill` (loses its killer ID),
//!   two-sided sign block entities, the "trust edges" flag of light data,
//!   the "suppress light updates" flag of `section_blocks_update`,
//!   advancements' telemetry flag, smithing recipes, entity data and
//!   registry IDs.
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 762;
pub const NEWER: i32 = 763;

/// A 1.20 client on a 1.19.4 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.19.4 client on a 1.20 backend.
pub const DOWN: Option<Make> = down::STEP;

/// The experiment 1.20 made part of the game.
const UPDATE_1_20: &str = "minecraft:update_1_20";

#[cfg(test)]
mod tests;
