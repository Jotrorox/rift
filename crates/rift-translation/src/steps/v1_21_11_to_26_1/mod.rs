//! Minecraft 1.21.11 (protocol 774) and 26.1 (protocol 775).
//!
//! What changed, from the official servers' packet classes:
//!
//! - `set_time` carries a game time and a map of world clocks (registry ID,
//!   total ticks, partial tick, rate) instead of one day time and a flag.
//! - Serverbound `interact` lost its action: 26.1 sends the interaction with
//!   its hit location (`LpVec3`) in one packet, attacks as `attack` and a
//!   spectator's click on an entity as `spectator_action`.
//! - 26.1 adds `game_rule_values`, `low_disk_space_warning`,
//!   `set_game_rule` and the `client_command` action that requests game
//!   rule values.
//!
//! Everything else that changed is in the shared codecs: entity data
//! serializers and indices, data components, recipes, registries and tags.
mod down;
pub(in crate::steps) mod join;
mod up;

#[cfg(test)]
mod tests;

use super::Make;

pub const OLDER: i32 = 774;
pub const NEWER: i32 = 775;

/// A 26.1 client on a 1.21.11 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.11 client on a 26.1 backend.
pub const DOWN: Option<Make> = down::STEP;

/// 1.21.11 `interact` actions.
const INTERACT: i32 = 0;
const ATTACK: i32 = 1;
const INTERACT_AT: i32 = 2;

/// The `client_command` action 26.1 added: request the game rule values.
const REQUEST_GAME_RULE_VALUES: i32 = 2;

/// The `world_clock` registry, as the 26.1 server sends it.
const WORLD_CLOCK_REGISTRY: &str = "minecraft:world_clock";
const OVERWORLD_CLOCK: &str = "minecraft:overworld";
