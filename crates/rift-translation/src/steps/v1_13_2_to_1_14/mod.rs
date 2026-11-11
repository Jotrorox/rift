//! Minecraft 1.13.2 (protocol 404) and 1.14 (protocol 477).
//!
//! What the pair changes beyond the shared codecs:
//!
//! - Block positions pack as x, z, y instead of x, y, z in every packet
//!   that carries one ([`layout`]).
//! - `login` lost the difficulty and gained the server's view distance;
//!   `respawn` lost the difficulty, which travels in `change_difficulty`
//!   (now with a lock flag) alone.
//! - The 1.14 client keeps a chunk cache around a centre the server moves
//!   with `set_chunk_cache_center` and ignores chunks outside it; a 1.13
//!   server never sends one, so going up the step follows the player and
//!   sends it. Going down the 1.13 client has neither packet.
//! - Sleeping moved from the `player_sleep` packet into entity data (the
//!   sleeping position and, for other players, the pose).
//! - The trade list and the open-book signal moved from the
//!   `minecraft:trader_list` and `minecraft:book_open` plugin channels into
//!   `merchant_offers` and `open_book`; the horse inventory moved from
//!   `open_screen` with the `EntityHorse` type into `mount_screen_open`.
//! - Entity sounds (`sound_entity`, played by tridents) are new.
//! - `use_item_on` puts the hand first and gained the inside-block flag;
//!   the recipe book settings gained the blast furnace and smoker books;
//!   the 1.14 client can send difficulty and jigsaw packets a 1.13 server
//!   has no use for.
//!
//! Facts about both protocols were checked against the official servers'
//! recorded traffic and the minecraft-data schemas; the implementation was
//! written independently for this crate.
mod down;
mod entity_data;
mod layout;
mod trades;
mod up;

#[cfg(test)]
mod tests;

use super::Make;
use crate::{
    Direction, Packet, State,
    step::{Cx, Routes},
    wire::invalid,
};
use std::io;

pub const OLDER: i32 = 404;
pub const NEWER: i32 = 477;

/// A 1.14 client on a 1.13.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.13.2 client on a 1.14 backend.
pub const DOWN: Option<Make> = down::STEP;

/// Hand a packet the step looked at to the shared codecs' default route for
/// it, as if the step had no rule of its own. `defaults` holds nothing but
/// the codecs' routes for the step's pair; `name` is the packet's name in the
/// sender's protocol. Codec taps on the packet run once more: they observe
/// state and repeat harmlessly.
pub(super) fn delegate<S>(
    defaults: &Routes<S>,
    step: &mut S,
    cx: &mut Cx<'_>,
    direction: Direction,
    state: State,
    name: &str,
    mut packet: Packet,
) -> io::Result<()> {
    let source = match direction {
        Direction::Clientbound => defaults.backend(),
        Direction::Serverbound => defaults.client(),
    };
    packet.id = source
        .packet_id(state, direction, name)
        .ok_or_else(|| invalid("delegated packet is not in the sender's protocol"))?;
    defaults.dispatch(step, cx, direction, state, packet)
}

/// Send `packet` on in `direction`.
pub(super) fn send(cx: &mut Cx<'_>, direction: Direction, state: State, packet: Packet) {
    match direction {
        Direction::Clientbound => cx.to_client(state, packet),
        Direction::Serverbound => cx.to_backend(state, packet),
    }
}

/// The chunk coordinate of a block coordinate.
pub(super) fn chunk(coordinate: f64) -> i32 {
    // Saturating float-to-int casts keep NaN and infinities in range.
    (coordinate.floor() as i32) >> 4
}
