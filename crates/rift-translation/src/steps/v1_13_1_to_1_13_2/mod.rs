//! Minecraft 1.13.1 (protocol 401) and 1.13.2 (protocol 404).
//!
//! Packet IDs and registries are the same; the item stack changed from a
//! short item ID (-1 for an empty stack) to a presence flag and a VarInt
//! ID. The item codec converts the stacks of every packet it owns, and the
//! entity codec those in entity data and particles. The one other packet
//! that carries stacks is the villager trade list, which 1.13.x still sends
//! as the `minecraft:trader_list` plugin message.
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;

pub const OLDER: i32 = 401;
pub const NEWER: i32 = 404;

/// A 1.13.2 client on a 1.13.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.13.1 client on a 1.13.2 backend.
pub const DOWN: Option<Make> = down::STEP;
