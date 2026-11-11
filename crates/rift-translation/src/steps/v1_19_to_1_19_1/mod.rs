//! Minecraft 1.19 (protocol 759) and 1.19.1/1.19.2 (protocol 760).
//!
//! 1.19.1 reworked message signing: player chat carries a signed header
//! chain and last-seen lists, `system_chat` says overlay or not instead of
//! naming a chat type, and the chat type registry in `login` changed its
//! layout and entries. Signatures of one version cannot be checked by the
//! other, so both steps deliver player chat as decorated system chat (see
//! [`super::v1_18_2_to_1_19::chat`]), send backends unsigned messages, and
//! give each client the vanilla chat type registry of its own version.
//! `hello` gained the player's UUID; profile keys are dropped either way.
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 759;
pub const NEWER: i32 = 760;

/// A 1.19.1 client on a 1.19 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.19 client on a 1.19.1 backend.
pub const DOWN: Option<Make> = down::STEP;

#[cfg(test)]
mod tests;
