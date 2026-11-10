//! Minecraft 1.15.2 (protocol 578) and 1.16 (protocol 735), the Nether
//! Update.
//!
//! What changes for this step's own packets:
//!
//! - Login Success carries the player's UUID as 16 bytes, not a string.
//! - `login` and `respawn` name dimensions by identifier, add the world name,
//!   the previous game mode and the debug and flat flags in place of the
//!   level type; `login` adds the world list and the dimension codec, and
//!   `respawn` whether to keep the player's attributes.
//! - Lightning loses its own packet (`add_global_entity`) and becomes an
//!   `add_entity` of `lightning_bolt`.
//! - `set_equipped_item` (one slot) becomes `set_equipment` (a list).
//! - Serverbound `interact` gains the sneaking flag, `player_abilities`
//!   loses the speeds the client never controlled, and the jigsaw editor
//!   gains names, a joint type and a "generate" button.
//! - Enchantment IDs in the enchanting table's container data and the
//!   colours of maps follow the new registries.
//!
//! Spawn data, chunks, text colours, recipes, tags, sounds and the rest are
//! the shared codecs' part.
mod dimension;
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;
use crate::{
    Direction, Packet,
    codec::Shared,
    data,
    wire::{Reader, Writer, invalid},
};
use std::io;

/// The dimension codec of a vanilla 1.16 server, for the next step's tests.
#[cfg(test)]
pub(in crate::steps) use dimension::VANILLA_CODEC as VANILLA_CODEC_1_16;

pub const OLDER: i32 = 578;
pub const NEWER: i32 = 735;

/// A 1.16 client on a 1.15.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.15.2 client on a 1.16 backend.
pub const DOWN: Option<Make> = down::STEP;

/// A UUID as 1.15 and older write it in Login Success: lowercase hex with
/// hyphens.
pub(in crate::steps) fn format_uuid(uuid: u128) -> String {
    let hex = format!("{uuid:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Parse a UUID string, with or without hyphens.
pub(in crate::steps) fn parse_uuid(text: &str) -> io::Result<u128> {
    let digits: String = match text.len() {
        36 if [8, 13, 18, 23]
            .iter()
            .all(|&at| text.as_bytes()[at] == b'-') =>
        {
            text.split('-').collect()
        }
        32 => text.to_owned(),
        _ => return Err(invalid("invalid UUID")),
    };
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("invalid UUID"));
    }
    u128::from_str_radix(&digits, 16).map_err(|_| invalid("invalid UUID"))
}

/// 1.16 names attributes `generic.max_health` where 1.15 says
/// `generic.maxHealth`; every vanilla attribute follows that rule.
///
/// INTEGRATION: the entity codec converts `update_attributes` between the
/// two spellings; this step only needs it to replay the player's attributes
/// after a respawn that keeps them.
fn legacy_attribute(name: &str) -> String {
    let name = data::strip(name);
    let mut legacy = String::with_capacity(name.len());
    let mut upper = false;
    for character in name.chars() {
        if character == '_' {
            upper = true;
        } else if upper {
            legacy.extend(character.to_uppercase());
            upper = false;
        } else {
            legacy.push(character);
        }
    }
    legacy
}

/// The enchanting table's window, followed through `open_screen` so that
/// its container data (properties 4 to 6 are enchantment IDs) can be mapped
/// without touching the same properties of other menus.
#[derive(Default)]
struct EnchantmentMenu {
    window: Option<u8>,
}

impl EnchantmentMenu {
    /// A backend `open_screen` in `protocol`'s layout.
    fn observe(&mut self, protocol: i32, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let window = reader.varint()?;
        let menu = reader.varint()?;
        let enchantment = data::version(protocol)
            .expect_registry("menu")
            .id("enchantment");
        self.window = (Some(menu) == enchantment)
            .then(|| u8::try_from(window).ok())
            .flatten();
        Ok(())
    }

    /// `container_set_data`: window, property, value.
    fn rewrite(
        &self,
        shared: &mut Shared,
        direction: Direction,
        body: &[u8],
    ) -> io::Result<Vec<u8>> {
        let mut reader = Reader::new(body);
        let window = reader.u8()?;
        let property = reader.i16()?;
        let mut value = reader.i16()?;
        reader.finish()?;
        if Some(window) == self.window && (4..=6).contains(&property) && value >= 0 {
            let mapping = shared.mapping(direction, "enchantment");
            value = mapping
                .get(i32::from(value))
                .and_then(|value| i16::try_from(value).ok())
                .unwrap_or(-1);
        }
        let mut writer = Writer::with_capacity(5);
        writer.u8(window).i16(property).i16(value);
        Ok(writer.buf)
    }
}

/// 1.16 map colours 52 to 58 (crimson and warped wood, nylium and wart)
/// as the 1.15 colour nearest in RGB: 1.15 clients have no colour for them
/// and fail to draw the map.
const NETHER_MAP_COLORS: [u8; 7] = [28, 38, 48, 23, 23, 48, 23];
/// The highest base colour 1.15 knows.
const LAST_LEGACY_MAP_COLOR: u8 = 51;
/// `MaterialColor.STONE`, for colours newer than 1.16.
const STONE_MAP_COLOR: u8 = 11;

fn legacy_map_color(color: u8) -> u8 {
    let (base, shade) = (color >> 2, color & 3);
    match base {
        0..=LAST_LEGACY_MAP_COLOR => color,
        52..=58 => NETHER_MAP_COLORS[usize::from(base - 52)] << 2 | shade,
        _ => STONE_MAP_COLOR << 2 | shade,
    }
}
