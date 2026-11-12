//! Minecraft 1.21.5 (protocol 770) and 1.21.6 (protocol 771).
//!
//! What the step itself has to handle, learnt from both servers' traffic and
//! Mojang's code (sound sources, command parsers, entity data and the new
//! registries are the shared codecs'):
//!
//! - Difficulty is a VarInt instead of a byte, both ways.
//! - Sneaking: 1.21.6 servers read it from `player_input` alone, so the
//!   press and release actions left `player_command` and the others moved
//!   down by two. 1.21.5 clients already send `player_input` too.
//! - The game mode switcher asks with `change_game_mode` instead of sending
//!   a `/gamemode` command.
//! - Dialogs (`show_dialog`, `clear_dialog`, `custom_click_action`) and
//!   waypoints are new in 1.21.6.
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;
use crate::{
    nbt::{Compound, Nbt},
    wire::{Reader, Writer, invalid},
};
use std::io;

pub const OLDER: i32 = 770;
pub const NEWER: i32 = 771;

/// A 1.21.6 client on a 1.21.5 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.5 client on a 1.21.6 backend.
pub const DOWN: Option<Make> = down::STEP;

/// `player_command` actions 1.21.5 has before the ones both share.
const PRESS_SHIFT: i32 = 0;
const RELEASE_SHIFT: i32 = 1;
const SNEAK_ACTIONS: i32 = 2;
/// `player_command` actions 1.21.6 has: stop sleeping to start fall flying.
const SHARED_ACTIONS: i32 = 7;
/// The shift flag of `player_input`.
const SHIFT: u8 = 0x20;

/// `change_difficulty` (either direction): the difficulty is an unsigned byte
/// before 1.21.6 and a VarInt from then on; the clientbound packet then has
/// a locked flag.
fn difficulty(body: &[u8], towards_newer: bool) -> io::Result<Vec<u8>> {
    let mut reader = Reader::new(body);
    let difficulty = if towards_newer {
        i32::from(reader.u8()?)
    } else {
        reader.varint()?
    };
    if !(0..=3).contains(&difficulty) {
        return Err(invalid("unknown difficulty"));
    }
    let mut writer = Writer::new();
    if towards_newer {
        writer.varint(difficulty);
    } else {
        writer.u8(difficulty as u8);
    }
    writer.bytes(reader.rest());
    Ok(writer.buf)
}

/// A clientbound `change_difficulty`: the difficulty, then the locked flag.
fn clientbound_difficulty(body: &[u8], towards_newer: bool) -> io::Result<Vec<u8>> {
    let converted = difficulty(body, towards_newer)?;
    let mut reader = Reader::new(&converted);
    reader.u8()?; // 0 to 3 in either encoding.
    reader.bool()?;
    reader.finish()?;
    Ok(converted)
}

/// A serverbound `change_difficulty`: the difficulty alone.
fn serverbound_difficulty(body: &[u8], towards_newer: bool) -> io::Result<Vec<u8>> {
    let converted = difficulty(body, towards_newer)?;
    if converted.len() != 1 {
        return Err(invalid("trailing data after packet"));
    }
    Ok(converted)
}

/// A 1.21.6 `player_command` for a 1.21.5 server: the same action two
/// places up.
fn command_up_to_1_21_5(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut reader = Reader::new(body);
    let entity = reader.varint()?;
    let action = reader.varint()?;
    let data = reader.varint()?;
    reader.finish()?;
    if !(0..SHARED_ACTIONS).contains(&action) {
        return Err(invalid("unknown player command"));
    }
    let mut writer = Writer::new();
    writer
        .varint(entity)
        .varint(action + SNEAK_ACTIONS)
        .varint(data);
    Ok(writer.buf)
}

/// A 1.21.5 `player_command` for a 1.21.6 server, or `None` for the shift
/// actions, which the client's `player_input` already conveys.
fn command_down_to_1_21_6(body: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let mut reader = Reader::new(body);
    let entity = reader.varint()?;
    let action = reader.varint()?;
    let data = reader.varint()?;
    reader.finish()?;
    match action {
        PRESS_SHIFT | RELEASE_SHIFT => Ok(None),
        _ if (SNEAK_ACTIONS..SNEAK_ACTIONS + SHARED_ACTIONS).contains(&action) => {
            let mut writer = Writer::new();
            writer
                .varint(entity)
                .varint(action - SNEAK_ACTIONS)
                .varint(data);
            Ok(Some(writer.buf))
        }
        _ => Err(invalid("unknown player command")),
    }
}

/// `player_input`'s flags, checked: one byte of seven known bits.
fn input_flags(body: &[u8]) -> io::Result<u8> {
    let mut reader = Reader::new(body);
    let flags = reader.u8()?;
    reader.finish()?;
    if flags & 0x80 != 0 {
        return Err(invalid("unknown player input"));
    }
    Ok(flags)
}

/// The 1.21.5 `player_command` that presses or releases shift.
fn shift_command(entity: i32, pressed: bool) -> Vec<u8> {
    let mut writer = Writer::new();
    writer
        .varint(entity)
        .varint(if pressed { PRESS_SHIFT } else { RELEASE_SHIFT })
        .varint(0);
    writer.buf
}

/// The `/gamemode` command a 1.21.5 client's game mode switcher sends for
/// what a 1.21.6 client asks with `change_game_mode`.
fn game_mode_command(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut reader = Reader::new(body);
    let mode = match reader.varint()? {
        0 => "survival",
        1 => "creative",
        2 => "adventure",
        3 => "spectator",
        _ => return Err(invalid("unknown game mode")),
    };
    reader.finish()?;
    let mut writer = Writer::new();
    writer.string(&format!("gamemode {mode}"));
    Ok(writer.buf)
}

/// A play `show_dialog` as one chat message for a client without dialogs:
/// the title and the message bodies. `None` when there is nothing to show,
/// or the dialog is one of the server's registered ones (only an ID).
fn dialog_as_chat(body: &[u8]) -> io::Result<Option<Nbt>> {
    let mut reader = Reader::new(body);
    if reader.varint()? != 0 {
        return Ok(None);
    }
    let dialog = Nbt::read(&mut reader, false)?.ok_or_else(|| invalid("dialog without data"))?;
    reader.finish()?;
    let Nbt::Compound(dialog) = dialog else {
        return Err(invalid("dialog is not a compound"));
    };
    let mut lines = Vec::new();
    if let Some(mut title) = dialog.get("title").and_then(plain_text) {
        title.insert("bold", Nbt::Byte(1));
        lines.push(title);
    }
    let bodies = match dialog.get("body") {
        Some(Nbt::List(bodies)) => bodies.iter().collect(),
        Some(body) => vec![body],
        None => Vec::new(),
    };
    for body in bodies.into_iter().filter_map(Nbt::as_compound) {
        let message = match body.str("type").map(crate::data::strip) {
            Some("plain_message") => body.get("contents"),
            // An item's description: a message, or just its contents.
            Some("item") => body.get("description").map(|description| {
                description
                    .as_compound()
                    .and_then(|message| message.get("contents"))
                    .unwrap_or(description)
            }),
            _ => None,
        };
        lines.extend(message.and_then(plain_text));
    }
    if lines.is_empty() {
        return Ok(None);
    }
    let mut extra = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        if index > 0 {
            extra.push(Nbt::Compound(literal("\n")));
        }
        extra.push(Nbt::Compound(line));
    }
    let mut message = literal("");
    message.insert("extra", Nbt::List(extra));
    Ok(Some(Nbt::Compound(message)))
}

/// A text component of plain text.
fn literal(text: &str) -> Compound {
    let mut compound = Compound::new();
    compound.insert("text", text);
    compound
}

/// Format and style keys that mean the same to 1.21.5 and 1.21.6 clients.
const PLAIN_KEYS: &[&str] = &[
    "type",
    "text",
    "translate",
    "fallback",
    "keybind",
    "color",
    "font",
    "bold",
    "italic",
    "underlined",
    "strikethrough",
    "obfuscated",
    "shadow_color",
];

/// A text component reduced to what reads the same to a 1.21.5 client:
/// text, translations and keybinds with their styling and children, without
/// click and hover events (1.21.6 added dialog actions to them) or contents
/// only the server could resolve. Always a compound, so that a list of them
/// is a valid NBT list. `None` when nothing readable is left.
fn plain_text(text: &Nbt) -> Option<Compound> {
    plain_text_at(text, 0)
}

fn plain_text_at(text: &Nbt, depth: usize) -> Option<Compound> {
    if depth > crate::wire::MAX_DEPTH {
        return None;
    }
    let parts = |parts: &[Nbt]| -> Vec<Nbt> {
        parts
            .iter()
            .filter_map(|part| plain_text_at(part, depth + 1))
            .map(Nbt::Compound)
            .collect()
    };
    match text {
        Nbt::String(text) => Some(literal(text)),
        // Translation arguments may be plain values.
        Nbt::Byte(_) | Nbt::Short(_) | Nbt::Int(_) | Nbt::Long(_) => {
            Some(literal(&text.as_i64()?.to_string()))
        }
        Nbt::Float(_) | Nbt::Double(_) => Some(literal(&text.as_f64()?.to_string())),
        Nbt::List(list) => {
            let extra = parts(list);
            let mut plain = literal("");
            (!extra.is_empty()).then(|| {
                plain.insert("extra", Nbt::List(extra));
                plain
            })
        }
        Nbt::Compound(compound) => {
            if !["text", "translate", "keybind"]
                .iter()
                .any(|key| compound.contains(key))
            {
                return None;
            }
            if let Some(kind) = compound.str("type")
                && !matches!(kind, "text" | "translatable" | "keybind")
            {
                return None;
            }
            let mut plain: Compound = compound
                .iter()
                .filter(|(key, _)| PLAIN_KEYS.contains(key))
                .map(|(key, value)| (key.to_owned(), value.clone()))
                .collect();
            for key in ["with", "extra"] {
                if let Some(list) = compound.list(key) {
                    let list = parts(list);
                    if !list.is_empty() {
                        plain.insert(key, Nbt::List(list));
                    }
                }
            }
            Some(plain)
        }
        _ => None,
    }
}

/// A 1.21.5 `system_chat` showing `text` in the chat (not the action bar).
fn system_chat(text: &Nbt) -> Vec<u8> {
    let mut writer = Writer::new();
    text.write(&mut writer, false);
    writer.bool(false);
    writer.buf
}
