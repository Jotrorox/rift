//! Minecraft 1.19.3 (protocol 761) and 1.19.4 (protocol 762).
//!
//! What the pair changes, and who converts it:
//!
//! - Join game: 1.19.4 synchronises damage types, trim patterns and trim
//!   materials, biomes say whether they have precipitation instead of naming
//!   it, and biome sounds become registry references; the registries codec
//!   converts all of it.
//! - Hurting: 1.19.3 announces a hurt entity with entity events 2, 33, 36,
//!   37, 44 and 57 (generic, thorns, drowning, burning, berry bush,
//!   freezing); 1.19.4 replaces them with `damage_event`, naming a damage
//!   type, and adds `hurt_animation` for the hurt player's tilt direction.
//! - `player_position` loses its "dismount first" flag: 1.19.4 servers
//!   dismount through `set_passengers` instead.
//! - New in 1.19.4 and dropped for 1.19.3 clients: `bundle_delimiter` (the
//!   bundled packets still arrive, in order) and `chunks_biomes` (biome
//!   changes then show after the chunk is sent again).
//! - Left to the shared codecs: entity data serializers and display
//!   entities, `server_data`, the `time` command argument, infinite effect
//!   durations, recipes, the new smithing menu, tags and registry IDs.
//!
//! Respawn already carries the "data to keep" flags in 1.19.3 and keeps its
//! layout, and both clients ignore `teleport_entity` for the player they
//! control, so neither needs converting; serverbound packets do not change.
//! The signed chat format is the same on both sides: signatures pass intact.
mod down;
#[cfg(test)]
pub(super) mod harness;
pub(super) mod play;
mod up;

use super::Make;

pub const OLDER: i32 = 761;
pub const NEWER: i32 = 762;

/// A 1.19.4 client on a 1.19.3 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.19.3 client on a 1.19.4 backend.
pub const DOWN: Option<Make> = down::STEP;

/// The hurt entity events of 1.19.3 and the damage type each stands for.
const HURT_EVENTS: [(i8, &str); 6] = [
    (2, "generic"),
    (33, "thorns"),
    (36, "drown"),
    (37, "on_fire"),
    (44, "sweet_berry_bush"),
    (57, "freeze"),
];

/// Damage types a 1.19.3 server announced as burning (event 37).
const FIRE: [&str; 6] = [
    "in_fire",
    "on_fire",
    "lava",
    "hot_floor",
    "fireball",
    "unattributed_fireball",
];

/// The 1.19.3 entity event for a hurt by damage type `name`.
fn hurt_event(name: &str) -> i8 {
    if FIRE.contains(&name) {
        return 37;
    }
    HURT_EVENTS
        .iter()
        .find(|(_, kind)| *kind == name)
        .map_or(2, |(event, _)| *event)
}

/// The "take damage" action of the animate packet, which 1.19.4 replaced
/// with `hurt_animation`.
const ANIMATE_HURT: u8 = 1;

#[cfg(test)]
mod tests;
