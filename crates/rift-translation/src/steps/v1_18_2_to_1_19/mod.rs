//! Minecraft 1.18.2 (protocol 758) and 1.19 (protocol 759).
//!
//! The chat-signing era starts here, and this pair also holds what its three
//! steps (up to 1.19.3) share: chat type registries and decoration
//! ([`chat`]), chat packet layouts ([`messages`]), `login`/`respawn`
//! ([`join`]) and the login phase ([`login`]).
pub(super) mod chat;
pub(super) mod join;
pub(super) mod login;
pub(super) mod messages;
pub(super) mod players;
#[cfg(test)]
pub(super) mod testing;

mod down;
mod up;

use super::Make;
use crate::{Direction, State, data};

pub const OLDER: i32 = 758;
pub const NEWER: i32 = 759;

/// A 1.19 client on a 1.18.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.18.2 client on a 1.19 backend.
pub const DOWN: Option<Make> = down::STEP;

/// The ID of a packet `protocol` must have, for packets a step emits under
/// another name than the one it received.
pub(super) fn id(protocol: i32, state: State, direction: Direction, name: &str) -> i32 {
    data::version(protocol).expect_packet(state, direction, name)
}
