//! Minecraft 1.19.1/1.19.2 (protocol 760) and 1.19.3 (protocol 761).
//!
//! 1.19.3 redid message signing once more: chat sessions replace profile
//! keys in `hello` and the encryption response, player chat is indexed and
//! acknowledged by count, and chat previews and message headers are gone;
//! `disguised_chat` carries unsigned player chat. Both steps deliver chat
//! unsigned (see [`super::v1_18_2_to_1_19::chat`]); going down, the step
//! acknowledges signed messages for the client so the backend's pending
//! count never overflows. `player_info` became `player_info_update` and
//! `player_info_remove`, `custom_sound` folded into `sound` as an inline
//! sound event, `respawn` keeps entity data by a flag, and biome effects in
//! `login` may name sounds as `{sound_id, range}` compounds.
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 760;
pub const NEWER: i32 = 761;

/// A 1.19.3 client on a 1.19.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.19.1 client on a 1.19.3 backend.
pub const DOWN: Option<Make> = down::STEP;

#[cfg(test)]
mod tests;
