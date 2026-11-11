//! Minecraft 1.13 (protocol 393) and 1.13.1 (protocol 401).
//!
//! Packet IDs are the same. What changed, besides what the shared codecs
//! convert (five dead coral plants inserted into the block and item
//! registries, see `data/rules/393-401.json`; arrows' shooter UUID in entity
//! data; two new command argument types):
//!
//! - Tab completion: a 1.13.1 client sends a command with its leading slash
//!   and a 1.13.1 server skips one if present, so suggestion ranges count
//!   it; a 1.13 client sends the command without it and a 1.13 server parses
//!   the text as it is (`command_suggestion`, `command_suggestions`; only a
//!   1.13.1 client on a 1.13 backend needs help, see `up`).
//! - Boss bars: 1.13 sends "create fog" in the "play boss music" bit 0x02
//!   and its clients read both from that bit; 1.13.1 gives fog bit 0x04
//!   (`boss_event`).
//! - Book editing: 1.13.1 names the hand holding the book; 1.13 always
//!   edits the main hand (`edit_book`).
//! - Dispenser smoke (`level_event` 2000): 1.13 encodes the dispenser's
//!   facing as a cell of the 3x3 grid around it (`dx + 1 + (dz + 1) * 3`),
//!   1.13.1 as the facing's 3D data value (down, up, north, south, west,
//!   east). The same packet carries a record's item ID and a broken block's
//!   state ID, which the inserted entries renumber.
//! - Villager trades still travel in the `minecraft:trader_list` plugin
//!   channel, whose item stacks need the item codec (`custom_payload`).
//!
//! The facts come from Mojang's 1.13 and 1.13.1 server code and the two
//! versions' recorded sessions.
mod down;
mod packets;
#[cfg(test)]
mod tests;
mod up;

use super::Make;

pub(super) use packets::custom_payload;

pub const OLDER: i32 = 393;
pub const NEWER: i32 = 401;

/// A 1.13.1 client on a 1.13 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.13 client on a 1.13.1 backend.
pub const DOWN: Option<Make> = down::STEP;
