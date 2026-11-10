//! Minecraft 1.20.5 (protocol 766) and 1.21 (protocol 767).
//!
//! Built-in registries stay the same: 1.21's content shipped in 1.20.5
//! behind the `update_1_21` feature flag. What the pair itself changes:
//!
//! - `custom_report_details` and `server_links` (configuration and play) are
//!   new; a 1.20.5 client never sees them.
//! - `projectile_power` carries a scalar acceleration instead of a vector,
//!   `mount_screen_open` a column count instead of a slot count, and
//!   `use_item` the player's rotation.
//! - 1.21 clients always roll the end credits; 1.20.5 servers ask clients
//!   that have seen them to skip.
//! - Enchantments become a synchronised registry, so the enchanting table's
//!   hints in `container_set_data` use its network IDs.
//!
//! Item components, entity data, attributes, chat types, jukebox songs and
//! the new synchronised registries belong to the shared codecs.
mod down;
#[cfg(test)]
pub(super) mod testing;
#[cfg(test)]
mod tests;
mod up;

use super::Make;
use crate::{
    data,
    wire::{Reader, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub const OLDER: i32 = 766;
pub const NEWER: i32 = 767;

/// A 1.21 client on a 1.20.5 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.20.5 client on a 1.21 backend.
pub const DOWN: Option<Make> = down::STEP;

/// Feature flags a client of `protocol` knows (from each version's
/// `FeatureFlags`); a client only warns about others, but there is no point
/// in sending them.
pub(super) fn known_features(protocol: i32) -> &'static [&'static str] {
    match protocol {
        ..=765 => &["vanilla", "bundle", "trade_rebalance", "update_1_21"],
        766 => &["vanilla", "bundle", "trade_rebalance", "update_1_21"],
        767 => &["vanilla", "bundle", "trade_rebalance"],
        _ => &[
            "vanilla",
            "trade_rebalance",
            "redstone_experiments",
            "minecart_improvements",
            "winter_drop",
        ],
    }
}

/// Rewrite an `update_enabled_features` body for a client of `target`:
/// flags it does not know are dropped and `add` (content the sender's
/// version has by default that the client keeps behind a flag) appended.
pub(super) fn rewrite_features(body: &[u8], target: i32, add: &[&str]) -> io::Result<Vec<u8>> {
    let known = known_features(target);
    let mut reader = Reader::new(body);
    let count = reader.count()?;
    let mut flags = Vec::with_capacity(count + add.len());
    for _ in 0..count {
        let flag = reader.string(32767)?;
        let name = flag.strip_prefix("minecraft:");
        if name.is_none_or(|name| known.contains(&name)) {
            flags.push(flag.to_owned());
        }
    }
    reader.finish()?;
    for flag in add {
        let named = format!("minecraft:{flag}");
        if !flags.iter().any(|known| *known == named || known == flag) {
            flags.push(named);
        }
    }
    let mut writer = Writer::new();
    writer.varint(flags.len() as i32);
    for flag in &flags {
        writer.string(flag);
    }
    Ok(writer.into_inner())
}

/// The vanilla order of 1.21's synchronised enchantment registry: a
/// datapack registry is sent sorted by entry name.
fn vanilla_synced_enchantments() -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: Vec<String> = data::version(OLDER)
            .expect_registry("enchantment")
            .names()
            .map(|(_, name)| name.to_owned())
            .collect();
        names.sort();
        names
    })
}

/// The menu type ID of the enchanting table in both protocols.
fn enchantment_menu() -> i32 {
    data::version(OLDER)
        .expect_registry("menu")
        .id("enchantment")
        .expect("1.20.5 has an enchantment menu")
}

/// The enchanting table's three hint properties carry enchantment IDs.
fn is_enchantment_hint(property: i16) -> bool {
    (4..=6).contains(&property)
}

/// Rewrite a `container_set_data` body whose value is an enchantment hint:
/// window ID (unsigned byte), property, value. `-1` means no hint.
fn map_enchantment_hint(body: &[u8], map: impl FnOnce(i32) -> Option<i32>) -> io::Result<Vec<u8>> {
    let mut reader = Reader::new(body);
    let window = reader.u8()?;
    let property = reader.i16()?;
    let value = reader.i16()?;
    reader.finish()?;
    let value = if value < 0 {
        value
    } else {
        map(i32::from(value))
            .and_then(|id| i16::try_from(id).ok())
            .unwrap_or(-1)
    };
    let mut writer = Writer::with_capacity(5);
    writer.u8(window).i16(property).i16(value);
    Ok(writer.into_inner())
}

/// The menu type an `open_screen` body (window ID, menu type, title) opens.
fn screen_menu(body: &[u8]) -> io::Result<i32> {
    let mut reader = Reader::new(body);
    reader.varint()?;
    let menu = reader.varint()?;
    if menu < 0 {
        return Err(invalid("negative menu type"));
    }
    Ok(menu)
}

/// The rotation and on-ground flag of a 1.21 or older `move_player_*` body
/// (position, then rotation, as the packet has them, then on ground).
fn movement(body: &[u8], position: bool, rotation: bool) -> io::Result<(Option<(f32, f32)>, bool)> {
    let mut reader = Reader::new(body);
    if position {
        reader.take(24)?;
    }
    let rotation = if rotation {
        Some((reader.f32()?, reader.f32()?))
    } else {
        None
    };
    let on_ground = reader.bool()?;
    reader.finish()?;
    Ok((rotation, on_ground))
}
