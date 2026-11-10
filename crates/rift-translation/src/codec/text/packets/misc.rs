//! Disconnects, titles, boss bars, the tab list header, combat death
//! messages, resource packs, server data, maps and command suggestions.
use super::{Cv, crosses, text_changes};
use crate::{
    Direction,
    codec::text::{Component, version as v},
    data,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

pub fn disconnect(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    cv.component(&mut rw)?;
    cv.emit("disconnect", rw.finish()?);
    Ok(())
}

/// 1.17 split the title packet into one packet per action.
const TITLES_SPLIT: i32 = 755;

pub fn titles_needed(from: i32, to: i32) -> bool {
    text_changes(from, to) || crosses(from, to, TITLES_SPLIT)
}

pub fn titles_split(from: i32, to: i32) -> bool {
    crosses(from, to, TITLES_SPLIT)
}

/// The combined title packet: an action, then its fields. 0 title, 1
/// subtitle, 2 action bar, 3 times, 4 clear, 5 reset.
pub fn set_titles(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    let action = rw.r.varint()?;
    if cv.to < TITLES_SPLIT {
        rw.w.varint(action);
    }
    let name = match action {
        0..=2 => {
            cv.component(&mut rw)?;
            ["set_title_text", "set_subtitle_text", "set_action_bar_text"][action as usize]
        }
        3 => {
            rw.copy(12)?;
            "set_titles_animation"
        }
        4 | 5 => {
            if cv.to >= TITLES_SPLIT {
                rw.w.bool(action == 5);
            }
            "clear_titles"
        }
        _ => return Err(invalid("unknown title action")),
    };
    let body = rw.finish()?;
    cv.emit(
        if cv.to < TITLES_SPLIT {
            "set_titles"
        } else {
            name
        },
        body,
    );
    Ok(())
}

/// One of the split title packets, as itself or as the combined packet.
fn title_part(cv: &mut Cv<'_>, data: &[u8], name: &'static str, action: i32) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    if cv.to < TITLES_SPLIT {
        rw.w.varint(action);
    }
    cv.component(&mut rw)?;
    let body = rw.finish()?;
    cv.emit(
        if cv.to < TITLES_SPLIT {
            "set_titles"
        } else {
            name
        },
        body,
    );
    Ok(())
}

pub fn set_title_text(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    title_part(cv, data, "set_title_text", 0)
}

pub fn set_subtitle_text(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    title_part(cv, data, "set_subtitle_text", 1)
}

pub fn set_action_bar_text(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    title_part(cv, data, "set_action_bar_text", 2)
}

pub fn set_titles_animation(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut reader = Reader::new(data);
    reader.take(12)?;
    reader.finish()?;
    if cv.to >= TITLES_SPLIT {
        cv.emit("set_titles_animation", data.to_vec());
        return Ok(());
    }
    let mut writer = Writer::new();
    writer.varint(3).bytes(data);
    cv.emit("set_titles", writer.into_inner());
    Ok(())
}

pub fn clear_titles(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut reader = Reader::new(data);
    let reset = reader.bool()?;
    reader.finish()?;
    if cv.to >= TITLES_SPLIT {
        cv.emit("clear_titles", data.to_vec());
        return Ok(());
    }
    let mut writer = Writer::new();
    writer.varint(if reset { 5 } else { 4 });
    cv.emit("set_titles", writer.into_inner());
    Ok(())
}

pub fn boss_event(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    rw.uuid()?;
    match rw.varint()? {
        0 => {
            cv.component(&mut rw)?;
            rw.f32()?;
            rw.varint()?;
            rw.varint()?;
            rw.u8()?;
        }
        1 => {}
        2 => drop(rw.f32()?),
        3 => cv.component(&mut rw)?,
        4 => {
            rw.varint()?;
            rw.varint()?;
        }
        5 => drop(rw.u8()?),
        _ => return Err(invalid("unknown boss event action")),
    }
    cv.emit("boss_event", rw.finish()?);
    Ok(())
}

pub fn tab_list(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    cv.component(&mut rw)?;
    cv.component(&mut rw)?;
    cv.emit("tab_list", rw.finish()?);
    Ok(())
}

/// 1.17 split the combat packet; 1.20 dropped the killer from the death
/// message packet.
const COMBAT_SPLIT: i32 = 755;
const KILL_WITHOUT_KILLER: i32 = 763;

pub fn combat_needed(from: i32, to: i32) -> bool {
    text_changes(from, to) || crosses(from, to, COMBAT_SPLIT)
}

pub fn combat_split(from: i32, to: i32) -> bool {
    crosses(from, to, COMBAT_SPLIT)
}

pub fn combat_kill_needed(from: i32, to: i32) -> bool {
    combat_needed(from, to) || crosses(from, to, KILL_WITHOUT_KILLER)
}

/// The combined combat packet: 0 enter, 1 end (duration, entity), 2
/// entity died (player, killer, message).
pub fn player_combat(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    let event = rw.r.varint()?;
    let split = cv.to >= COMBAT_SPLIT;
    if !split {
        rw.w.varint(event);
    }
    let name = match event {
        0 => "player_combat_enter",
        1 => {
            rw.varint()?;
            rw.i32()?;
            "player_combat_end"
        }
        2 => {
            rw.varint()?;
            rw.i32()?;
            cv.component(&mut rw)?;
            "player_combat_kill"
        }
        _ => return Err(invalid("unknown combat event")),
    };
    let body = rw.finish()?;
    cv.emit(if split { name } else { "player_combat" }, body);
    Ok(())
}

pub fn player_combat_kill(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    if cv.to < COMBAT_SPLIT {
        rw.w.varint(2);
    }
    rw.varint()?;
    let killer = if cv.from < KILL_WITHOUT_KILLER {
        rw.r.i32()?
    } else {
        -1
    };
    if cv.to < KILL_WITHOUT_KILLER {
        rw.w.i32(killer);
    }
    cv.component(&mut rw)?;
    let body = rw.finish()?;
    cv.emit(
        if cv.to < COMBAT_SPLIT {
            "player_combat"
        } else {
            "player_combat_kill"
        },
        body,
    );
    Ok(())
}

pub fn player_combat_enter(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    Reader::new(data).finish()?;
    if cv.to >= COMBAT_SPLIT {
        cv.emit("player_combat_enter", Vec::new());
        return Ok(());
    }
    let mut writer = Writer::new();
    writer.varint(0);
    cv.emit("player_combat", writer.into_inner());
    Ok(())
}

pub fn player_combat_end(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    let split = cv.to >= COMBAT_SPLIT;
    if !split {
        rw.w.varint(1);
    }
    rw.varint()?;
    rw.i32()?;
    cv.emit(
        if split {
            "player_combat_end"
        } else {
            "player_combat"
        },
        rw.finish()?,
    );
    Ok(())
}

/// 1.17 added the forced flag and prompt; 1.20.3 added pack UUIDs, several
/// packs at once and removal.
const PACK_PROMPT: i32 = 755;
const PACK_IDS: i32 = 765;

pub fn resource_pack_push_needed(from: i32, to: i32) -> bool {
    text_changes(from, to) || crosses(from, to, PACK_PROMPT) || crosses(from, to, PACK_IDS)
}

pub fn resource_pack_split(from: i32, to: i32) -> bool {
    crosses(from, to, PACK_IDS)
}

pub fn resource_pack_push(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    let id = if cv.from >= PACK_IDS {
        Some(rw.r.uuid()?)
    } else {
        None
    };
    let url = rw.r.str()?;
    let id = id.unwrap_or_else(|| name_uuid(url.as_bytes()));
    if cv.to >= PACK_IDS {
        rw.w.uuid(id);
    } else if cv.from >= PACK_IDS {
        // The older client answers without an ID: remember whose turn it is.
        cv.text().resource_packs.push_back(id);
    }
    rw.w.string(url);
    rw.string(40)?;
    let (forced, prompt) = if cv.from >= PACK_PROMPT {
        (rw.r.bool()?, cv.read_optional_component(&mut rw.r)?)
    } else {
        (false, None)
    };
    if cv.to >= PACK_PROMPT {
        rw.w.bool(forced);
        cv.write_optional_component(&mut rw.w, prompt.as_ref());
    }
    cv.emit("resource_pack_push", rw.finish()?);
    Ok(())
}

/// Removing packs (1.20.3) has no counterpart for older clients, which keep
/// what they loaded; the packet is dropped and the IDs forgotten.
pub fn resource_pack_pop(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut reader = Reader::new(data);
    let id = if reader.bool()? {
        Some(reader.uuid()?)
    } else {
        None
    };
    reader.finish()?;
    if cv.to >= PACK_IDS {
        cv.emit("resource_pack_pop", data.to_vec());
        return Ok(());
    }
    let packs = &mut cv.text().resource_packs;
    match id {
        Some(id) => packs.retain(|known| *known != id),
        None => packs.clear(),
    }
    Ok(())
}

/// The client's answer. Results: 0 loaded, 1 declined, 2 failed download,
/// 3 accepted; from 1.20.3 also 4 downloaded, 5 invalid URL, 6 failed
/// reload, 7 discarded.
pub fn resource_pack(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    debug_assert_eq!(cv.direction, Direction::Serverbound);
    let mut reader = Reader::new(data);
    let id = if cv.from >= PACK_IDS {
        Some(reader.uuid()?)
    } else {
        None
    };
    let result = reader.varint()?;
    reader.finish()?;
    let mut writer = Writer::new();
    if cv.to >= PACK_IDS {
        let id = match id {
            Some(id) => id,
            None => {
                let packs = &mut cv.text().resource_packs;
                let pending = match result {
                    // A final answer ends the oldest pending pack.
                    0..=2 => packs.pop_front(),
                    _ => packs.front().copied(),
                };
                // An answer to no pack the backend sent is dropped.
                let Some(pending) = pending else {
                    return Ok(());
                };
                pending
            }
        };
        writer.uuid(id);
        writer.varint(result);
    } else {
        let result = match result {
            0..=3 => result,
            5 | 6 => 2,
            7 => 1,
            // "Downloaded" has no older counterpart.
            _ => return Ok(()),
        };
        writer.varint(result);
    }
    cv.emit("resource_pack", writer.into_inner());
    Ok(())
}

/// Server data appeared in 1.19 and changed in each of the next releases:
/// 1.19.1 added the secure chat flag, 1.19.3 dropped chat previews, 1.19.4
/// made the MOTD required and sends the icon as PNG bytes, 1.20.5 moved the
/// secure chat flag into the login packet.
const SECURE_CHAT: i32 = 760;
const NO_PREVIEWS: i32 = 761;
const ICON_BYTES: i32 = 762;
const SECURE_CHAT_MOVED: i32 = 766;

pub fn server_data_needed(from: i32, to: i32) -> bool {
    text_changes(from, to)
        || [SECURE_CHAT, NO_PREVIEWS, ICON_BYTES, SECURE_CHAT_MOVED]
            .iter()
            .any(|boundary| crosses(from, to, *boundary))
}

const ICON_PREFIX: &str = "data:image/png;base64,";

pub fn server_data(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut reader = Reader::new(data);
    let (motd, icon) = if cv.from >= ICON_BYTES {
        let motd = Some(cv.read_component(&mut reader)?);
        let icon = if reader.bool()? {
            Some(reader.byte_array()?.to_vec())
        } else {
            None
        };
        (motd, icon)
    } else {
        let motd = cv.read_optional_component(&mut reader)?;
        let icon = if reader.bool()? {
            let text = reader.str()?;
            text.strip_prefix(ICON_PREFIX).and_then(base64_decode)
        } else {
            None
        };
        (motd, icon)
    };
    if cv.from < NO_PREVIEWS {
        reader.bool()?;
    }
    let secure_chat = if (SECURE_CHAT..SECURE_CHAT_MOVED).contains(&cv.from) {
        reader.bool()?
    } else {
        false
    };
    reader.finish()?;
    let mut writer = Writer::new();
    if cv.to >= ICON_BYTES {
        cv.write_component(&mut writer, &motd.unwrap_or_else(|| Component::text("")));
        writer.bool(icon.is_some());
        if let Some(icon) = &icon {
            writer.byte_array(icon);
        }
    } else {
        cv.write_optional_component(&mut writer, motd.as_ref());
        writer.bool(icon.is_some());
        if let Some(icon) = &icon {
            writer.string(&format!("{ICON_PREFIX}{}", base64_encode(icon)));
        }
    }
    if cv.to < NO_PREVIEWS {
        writer.bool(false);
    }
    if (SECURE_CHAT..SECURE_CHAT_MOVED).contains(&cv.to) {
        writer.bool(secure_chat);
    }
    cv.emit("server_data", writer.into_inner());
    Ok(())
}

/// Map packet layouts: 1.13 typed decorations with names, 1.14 the locked
/// flag, 1.17 optional decorations without the tracking flag, 1.20.5
/// decoration types from a registry.
const MAP_ICON_NAMES: i32 = 393;
const MAP_LOCKED: i32 = 477;
const MAP_OPTIONAL_ICONS: i32 = 755;

pub fn map_needed(from: i32, to: i32) -> bool {
    text_changes(from, to)
        || [
            MAP_ICON_NAMES,
            MAP_LOCKED,
            MAP_OPTIONAL_ICONS,
            v::ITEM_COMPONENTS,
        ]
        .iter()
        .any(|boundary| crosses(from, to, *boundary))
        || (from >= v::ITEM_COMPONENTS
            && to >= v::ITEM_COMPONENTS
            && decoration_names(from) != decoration_names(to))
}

/// One map decoration in the receiver's terms.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Icon {
    kind: String,
    x: i8,
    z: i8,
    direction: u8,
    name: Option<Component>,
}

/// The decoration types before 1.20.5, in ID order (1.12 has the first ten).
const LEGACY_DECORATIONS: &[&str] = &[
    "player",
    "frame",
    "red_marker",
    "blue_marker",
    "target_x",
    "target_point",
    "player_off_map",
    "player_off_limits",
    "mansion",
    "monument",
    "banner_white",
    "banner_orange",
    "banner_magenta",
    "banner_light_blue",
    "banner_yellow",
    "banner_lime",
    "banner_pink",
    "banner_gray",
    "banner_light_gray",
    "banner_cyan",
    "banner_purple",
    "banner_blue",
    "banner_brown",
    "banner_green",
    "banner_red",
    "banner_black",
    "red_x",
];

fn decoration_names(protocol: i32) -> Vec<String> {
    if protocol >= v::ITEM_COMPONENTS
        && let Some(registry) = data::version(protocol).registry("map_decoration_type")
    {
        return registry.names().map(|(_, name)| name.to_owned()).collect();
    }
    let count = if protocol < MAP_ICON_NAMES {
        10
    } else {
        LEGACY_DECORATIONS.len()
    };
    LEGACY_DECORATIONS[..count]
        .iter()
        .map(|name| (*name).to_owned())
        .collect()
}

pub fn map_item_data(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut r = Reader::new(data);
    let map = r.varint()?;
    let scale = r.i8()?;
    let tracking = if cv.from < MAP_OPTIONAL_ICONS {
        r.bool()?
    } else {
        true
    };
    let locked = if cv.from >= MAP_LOCKED {
        r.bool()?
    } else {
        false
    };
    let present = cv.from < MAP_OPTIONAL_ICONS || r.bool()?;
    let source_names = decoration_names(cv.from);
    let mut icons = None;
    if present {
        let count = r.count()?;
        let mut list = Vec::with_capacity(count);
        for _ in 0..count {
            let (kind, x, z, direction, name) = if cv.from < MAP_ICON_NAMES {
                let packed = r.u8()?;
                let (x, z) = (r.i8()?, r.i8()?);
                (i32::from(packed >> 4), x, z, packed & 15, None)
            } else {
                let kind = r.varint()?;
                let (x, z, direction) = (r.i8()?, r.i8()?, r.u8()?);
                (kind, x, z, direction, cv.read_optional_component(&mut r)?)
            };
            let kind = usize::try_from(kind)
                .ok()
                .and_then(|kind| source_names.get(kind))
                .ok_or_else(|| invalid("unknown map decoration type"))?;
            list.push(Icon {
                kind: kind.clone(),
                x,
                z,
                direction,
                name,
            });
        }
        icons = Some(list);
    }
    let rest = r.rest();
    let target_names = decoration_names(cv.to);
    if let Some(list) = &mut icons {
        // Decorations the receiver lacks are left out.
        list.retain(|icon| target_names.contains(&icon.kind));
    }
    let mut w = Writer::new();
    w.varint(map).i8(scale);
    if cv.to < MAP_OPTIONAL_ICONS {
        w.bool(tracking);
    }
    if cv.to >= MAP_LOCKED {
        w.bool(locked);
    }
    let icons = if cv.to < MAP_OPTIONAL_ICONS {
        // Older clients replace decorations on every update: resend the last.
        let cache = &mut cv.text().map_icons;
        Some(match icons {
            Some(list) => {
                cache.insert(map, list.clone());
                list
            }
            None => cache.get(&map).cloned().unwrap_or_default(),
        })
    } else {
        w.bool(icons.is_some());
        icons
    };
    if let Some(list) = icons {
        w.varint(list.len() as i32);
        for icon in &list {
            let kind = target_names
                .iter()
                .position(|name| *name == icon.kind)
                .unwrap_or_default() as i32;
            if cv.to < MAP_ICON_NAMES {
                w.u8(((kind as u8) << 4) | (icon.direction & 15));
                w.i8(icon.x).i8(icon.z);
            } else {
                w.varint(kind).i8(icon.x).i8(icon.z).u8(icon.direction);
                cv.write_optional_component(&mut w, icon.name.as_ref());
            }
        }
    }
    // The colour patch is the same in every version.
    let mut patch = Reader::new(rest);
    let columns = patch.u8()?;
    if columns > 0 {
        patch.take(3)?;
        patch.byte_array()?;
    }
    patch.finish()?;
    w.bytes(rest);
    cv.emit("map_item_data", w.into_inner());
    Ok(())
}

pub fn suggestions_needed(from: i32, to: i32) -> bool {
    from >= v::FLATTENING && to >= v::FLATTENING && text_changes(from, to)
}

pub fn command_suggestions(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    rw.varint()?;
    rw.varint()?;
    rw.varint()?;
    for _ in 0..rw.count()? {
        rw.str()?;
        cv.optional_component(&mut rw)?;
    }
    cv.emit("command_suggestions", rw.finish()?);
    Ok(())
}

/// Java's `UUID.nameUUIDFromBytes`: an MD5 name-based (version 3) UUID, as
/// vanilla derives resource pack IDs from URLs.
pub fn name_uuid(name: &[u8]) -> u128 {
    let mut digest = md5(name);
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    u128::from_be_bytes(digest)
}

fn md5(input: &[u8]) -> [u8; 16] {
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let constants: Vec<u32> = (0..64)
        .map(|index: i32| ((f64::from(index + 1)).sin().abs() * 4_294_967_296.0) as u32)
        .collect();
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((input.len() as u64).wrapping_mul(8)).to_le_bytes());
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    for chunk in message.chunks(64) {
        let words: Vec<u32> = chunk
            .chunks(4)
            .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
            .collect();
        let [mut a, mut b, mut c, mut d] = state;
        for index in 0..64 {
            let (f, g) = match index / 16 {
                0 => ((b & c) | (!b & d), index),
                1 => ((d & b) | (!d & c), (5 * index + 1) % 16),
                2 => (b ^ c ^ d, (3 * index + 5) % 16),
                _ => (c ^ (b | !d), (7 * index) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(constants[index])
                .wrapping_add(words[g])
                .rotate_left(SHIFTS[index]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(rotated);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut out = [0u8; 16];
    for (index, word) in state.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| {
            value | u32::from(*byte) << (16 - 8 * index)
        });
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(BASE64[(value >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decode standard base64 (as Java's decoder, also accepting line breaks);
/// `None` for invalid input, which drops the icon.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut value, mut bits) = (0u32, 0);
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        if byte == b'\r' || byte == b'\n' {
            continue;
        }
        let digit = BASE64.iter().position(|known| *known == byte)? as u32;
        value = (value << 6) | digit;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((value >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_match_java() {
        // UUID.nameUUIDFromBytes("test".getBytes()) in Java.
        assert_eq!(
            crate::codec::text::format_uuid(name_uuid(b"test")),
            "098f6bcd-4621-3373-8ade-4e832627b4f6"
        );
        assert_eq!(base64_encode(b"hello!?"), "aGVsbG8hPw==");
        assert_eq!(base64_decode("aGVsbG8hPw==").unwrap(), b"hello!?");
        assert_eq!(base64_decode("a*"), None);
    }
}
