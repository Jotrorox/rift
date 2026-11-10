//! Minecraft 1.20.3/1.20.4 (protocol 765) and 1.20.5/1.20.6 (protocol 766).
//!
//! Item stacks carry data components from 1.20.5 on and registries travel
//! one per `registry_data` packet; both belong to the shared codecs (the
//! item codec's tag/component bridge and the registries codec). What this
//! module handles:
//!
//! - Login: the encryption request says whether the client must
//!   authenticate, and the login success whether the client should treat
//!   every packet error as fatal. Cookies (`cookie_request`, `store_cookie`,
//!   `cookie_response`) exist in login, configuration and play, and servers
//!   can `transfer` a client to another address.
//! - Configuration: the server offers the data packs it shares with the
//!   client (`select_known_packs`) and only sends registry entries the
//!   client lacks; `reset_chat` clears the chat.
//! - `login` and `respawn` name the dimension type by its network ID instead
//!   of its name, and `login` took over `enforces_secure_chat` from
//!   `server_data`.
//! - Commands without signed arguments are sent as `chat_command` with
//!   nothing but the command; signed ones moved to `chat_command_signed`.
//! - Reach became the player attributes `block_interaction_range` and
//!   `entity_interaction_range`; creative players get their longer reach
//!   from attribute modifiers the server adds.
//! - `container_button_click` sends VarInts instead of bytes, horse
//!   inventories lost their armor slot (it is an equipment slot now), and
//!   the server may send `debug_sample` and `projectile_power`.
//!
//! Secure chat keeps working across the pair: both protocols sign messages
//! the same way, so signatures pass through untouched.
mod chat;
mod down;
mod up;

#[cfg(test)]
mod tests;

use super::Make;
use crate::{
    Direction,
    codec::{self, Shared},
    nbt::Nbt,
    wire::{Reader, Rewriter, invalid},
};
use std::io;

pub const OLDER: i32 = 765;
pub const NEWER: i32 = 766;

/// A 1.20.5 client on a 1.20.3 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.20.3 client on a 1.20.5 backend.
pub const DOWN: Option<Make> = down::STEP;

/// The game event that changes the player's game mode.
const CHANGE_GAME_MODE: u8 = 3;
const CREATIVE: u8 = 1;

/// The vanilla dimension types of 1.20.5 in network ID order, for a client
/// or backend whose registry data has not been seen.
const VANILLA_DIMENSION_TYPES: [&str; 4] = [
    "minecraft:overworld",
    "minecraft:overworld_caves",
    "minecraft:the_end",
    "minecraft:the_nether",
];

/// Copy one NBT text component between 1.20.3 and 1.20.5. Their text
/// formats differ only in how a `show_item` hover event holds its item (a
/// tag before, components after), so a component without one is copied as
/// it is.
fn text(shared: &mut Shared, direction: Direction, rw: &mut Rewriter<'_>) -> io::Result<()> {
    let bytes = rw.r.peek();
    let length = Reader::new(bytes).nbt_bytes(false)?.len();
    if !bytes[..length]
        .windows(9)
        .any(|window| window == b"show_item")
    {
        rw.copy(length)?;
        return Ok(());
    }
    // INTEGRATION: converts item hover events once the text codec is implemented.
    codec::text::transcode(shared, direction, rw)
}

/// The fields of `login` before the dimension type, the same in both
/// protocols: entity ID, hardcore, the world list, maximum players, view
/// and simulation distance, reduced debug info, respawn screen and limited
/// crafting. Returns the player's entity ID.
fn login_head(rw: &mut Rewriter<'_>) -> io::Result<i32> {
    let entity = rw.i32()?;
    rw.bool()?;
    rw.list(|rw| rw.str().map(drop))?;
    rw.varint()?;
    rw.varint()?;
    rw.varint()?;
    rw.bool()?;
    rw.bool()?;
    rw.bool()?;
    Ok(entity)
}

/// The spawn fields after the dimension type, shared by `login` and
/// `respawn` and the same in both protocols: world name, hashed seed, game
/// mode, previous game mode, debug and flat flags, the optional death
/// location and the portal cooldown. Returns the game mode.
fn spawn_tail(rw: &mut Rewriter<'_>) -> io::Result<u8> {
    rw.str()?;
    rw.i64()?;
    let game_mode = rw.u8()?;
    rw.i8()?;
    rw.bool()?;
    rw.bool()?;
    rw.optional(|rw| {
        rw.str()?;
        rw.i64().map(drop)
    })?;
    rw.varint()?;
    Ok(game_mode)
}

/// The dimension type names of a 1.20.3 registry codec, in network ID order.
fn codec_dimension_types(data: &[u8]) -> io::Result<Vec<String>> {
    let codec = Nbt::from_bytes(data, false)?;
    let entries = codec
        .as_ref()
        .and_then(Nbt::as_compound)
        .and_then(|codec| codec.compound("minecraft:dimension_type"))
        .and_then(|registry| registry.list("value"))
        .ok_or_else(|| invalid("registry codec without dimension types"))?;
    let mut named = entries
        .iter()
        .map(|entry| {
            let entry = entry.as_compound()?;
            Some((entry.int("id")?, entry.str("name")?.to_owned()))
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| invalid("invalid dimension type entry"))?;
    named.sort_by_key(|(id, _)| *id);
    Ok(named.into_iter().map(|(_, name)| name).collect())
}

/// The entry names of a 1.20.5 `registry_data` packet when it is the
/// dimension type registry.
fn packet_dimension_types(data: &[u8]) -> io::Result<Option<Vec<String>>> {
    let mut reader = Reader::new(data);
    if crate::data::strip(reader.str()?) != "dimension_type" {
        return Ok(None);
    }
    let mut names = Vec::new();
    for _ in 0..reader.count()? {
        names.push(reader.str()?.to_owned());
        if reader.bool()? {
            reader.nbt_bytes(false)?;
        }
    }
    reader.finish()?;
    Ok(Some(names))
}

/// The network ID of the dimension type `name` among `known` (what the
/// side was sent), or among vanilla's when nothing was.
fn dimension_id(known: &[String], name: &str) -> io::Result<i32> {
    let name = crate::data::strip(name);
    let position = if known.is_empty() {
        VANILLA_DIMENSION_TYPES
            .iter()
            .position(|known| crate::data::strip(known) == name)
    } else {
        known
            .iter()
            .position(|known| crate::data::strip(known) == name)
    };
    position
        .map(|id| id as i32)
        .ok_or_else(|| invalid("unknown dimension type"))
}

fn dimension_name(known: &[String], id: i32) -> io::Result<String> {
    let id = usize::try_from(id).map_err(|_| invalid("negative dimension type"))?;
    if known.is_empty() {
        VANILLA_DIMENSION_TYPES
            .get(id)
            .map(|name| (*name).to_owned())
    } else {
        known.get(id).cloned()
    }
    .ok_or_else(|| invalid("unknown dimension type ID"))
}
