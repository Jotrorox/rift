//! Plugin channels: free-form names of at most 20 characters in 1.12,
//! namespaced identifiers in 1.13. Vanilla's own `MC|...` channels got
//! `minecraft:` names, and two of them became packets (`MC|StopSound`
//! clientbound; the serverbound ones are in [`super::serverbound`]).
//!
//! A legacy name that is not a valid identifier is escaped into the
//! `rift:legacy/` namespace (any byte outside `[a-z0-9./-]` as `_` plus two
//! hex digits), which decodes back without state; names the backend used are
//! also remembered so that what the client echoes goes back as it came.
use super::{Up, packets};
use crate::{
    Direction, Packet, State,
    codec::item,
    data,
    step::{Cx, RouteBuilder},
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{collections::HashMap, io};

/// The longest channel name a 1.12 server accepts.
const LEGACY_MAX: usize = 20;
const ESCAPED: &str = "rift:legacy/";
/// Remembered names; a backend that invents more gets the stateless form.
const REMEMBERED: usize = 256;

/// Vanilla channels, legacy name first.
const VANILLA: &[(&str, &str)] = &[
    ("MC|Brand", "minecraft:brand"),
    ("MC|BOpen", "minecraft:book_open"),
    ("MC|TrList", "minecraft:trader_list"),
    ("MC|DebugPath", "minecraft:debug/paths"),
    (
        "MC|DebugNeighborsUpdate",
        "minecraft:debug/neighbors_update",
    ),
    ("REGISTER", "minecraft:register"),
    ("UNREGISTER", "minecraft:unregister"),
    ("BungeeCord", "bungeecord:main"),
];

/// 1.12's sound categories in their network order, as 1.13 numbers them.
const SOUND_SOURCES: &[&str] = &[
    "master", "music", "record", "weather", "block", "hostile", "neutral", "player", "ambient",
    "voice",
];

/// Legacy names by the identifier the client knows them as.
#[derive(Default)]
pub struct Names {
    legacy: HashMap<String, String>,
}

impl Names {
    /// The 1.13 identifier of a legacy channel name.
    pub fn upgrade(&mut self, legacy: &str) -> String {
        let modern = upgrade(legacy);
        if modern != legacy && !self.legacy.contains_key(&modern) && self.legacy.len() < REMEMBERED
        {
            self.legacy.insert(modern.clone(), legacy.to_owned());
        }
        modern
    }

    /// The legacy name of a 1.13 channel identifier, or `None` when a 1.12
    /// server could not read it.
    pub fn downgrade(&self, modern: &str) -> Option<String> {
        let modern = namespaced(modern);
        let legacy = match self.legacy.get(&modern) {
            Some(legacy) => legacy.clone(),
            None => downgrade(&modern)?,
        };
        (legacy.encode_utf16().count() <= LEGACY_MAX).then_some(legacy)
    }
}

fn valid_namespace(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.'))
}

fn valid_path(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/'))
}

/// Whether `text` is an identifier 1.13 accepts, with or without namespace.
fn valid_identifier(text: &str) -> bool {
    match text.split_once(':') {
        Some((namespace, path)) => valid_namespace(namespace) && valid_path(path),
        None => valid_path(text),
    }
}

fn namespaced(text: &str) -> String {
    if text.contains(':') {
        text.to_owned()
    } else {
        format!("minecraft:{text}")
    }
}

/// The 1.13 identifier for a legacy channel name, without remembering it.
pub fn upgrade(legacy: &str) -> String {
    if let Some((_, modern)) = VANILLA.iter().find(|(old, _)| *old == legacy) {
        return (*modern).to_owned();
    }
    if valid_identifier(legacy) && !legacy.starts_with(ESCAPED) {
        return namespaced(legacy);
    }
    let mut escaped = String::from(ESCAPED);
    for byte in legacy.bytes() {
        if matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'/' | b'-') {
            escaped.push(byte as char);
        } else {
            escaped.push_str(&format!("_{byte:02x}"));
        }
    }
    escaped
}

/// The legacy name for a namespaced 1.13 identifier, without remembered
/// names; `None` for an escape that does not decode.
pub fn downgrade(modern: &str) -> Option<String> {
    if let Some((legacy, _)) = VANILLA.iter().find(|(_, new)| *new == modern) {
        return Some((*legacy).to_owned());
    }
    let Some(escaped) = modern.strip_prefix(ESCAPED) else {
        return Some(modern.to_owned());
    };
    let mut bytes = Vec::with_capacity(escaped.len());
    let mut rest = escaped.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        if byte == b'_' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

pub fn install(builder: RouteBuilder<Up>) -> RouteBuilder<Up> {
    builder
        .clientbound(State::Play, "custom_payload", clientbound)
        .serverbound(State::Play, "custom_payload", serverbound)
}

/// A NUL-separated channel list (`REGISTER`/`UNREGISTER`), each name
/// converted by `convert`; names it rejects are left out.
fn channel_list(payload: &[u8], mut convert: impl FnMut(&str) -> Option<String>) -> Vec<u8> {
    let text = String::from_utf8_lossy(payload);
    let names: Vec<String> = text
        .split('\0')
        .filter(|name| !name.is_empty())
        .filter_map(&mut convert)
        .collect();
    names.join("\0").into_bytes()
}

fn clientbound(step: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let channel = reader.string(LEGACY_MAX)?;
    let payload = reader.rest();
    let mut writer = Writer::with_capacity(packet.data.len() + 16);
    match channel {
        "MC|StopSound" => return stop_sound(step, cx, state, payload),
        "MC|TrList" => {
            writer.string("minecraft:trader_list");
            trades(step, payload, &mut writer)?;
        }
        "REGISTER" | "UNREGISTER" => {
            let list = channel_list(payload, |name| Some(step.channels.upgrade(name)));
            if list.is_empty() {
                return Ok(());
            }
            writer.string(&upgrade(channel)).bytes(&list);
        }
        _ => {
            let modern = step.channels.upgrade(channel);
            writer.string(&modern).bytes(payload);
        }
    }
    cx.to_client(state, Packet::new(packet.id, writer.buf));
    Ok(())
}

/// `MC|TrList`: window ID, then the offers, whose item stacks change format.
fn trades(step: &mut Up, payload: &[u8], writer: &mut Writer) -> io::Result<()> {
    let mut rw = Rewriter::new(payload);
    rw.i32()?; // Window ID.
    let count = rw.u8()?;
    for _ in 0..count {
        // INTEGRATION: the item codec converts 1.12 stacks to 1.13 ones; a
        // stub that fails until it lands.
        item::transcode(&mut step.shared, Direction::Clientbound, &mut rw)?; // First input.
        item::transcode(&mut step.shared, Direction::Clientbound, &mut rw)?; // Result.
        if rw.bool()? {
            item::transcode(&mut step.shared, Direction::Clientbound, &mut rw)?;
        }
        rw.bool()?; // Disabled.
        rw.i32()?; // Uses.
        rw.i32()?; // Maximum uses.
    }
    writer.bytes(&rw.finish()?);
    Ok(())
}

/// `MC|StopSound` (category name and sound name, either empty for "all")
/// became the `stop_sound` packet with a flags byte.
fn stop_sound(step: &mut Up, cx: &mut Cx<'_>, state: State, payload: &[u8]) -> io::Result<()> {
    let mut reader = Reader::new(payload);
    let source = reader.str()?;
    let sound = reader.str()?;
    reader.finish()?;
    let mut flags = 0u8;
    let mut body = Writer::new();
    body.u8(0);
    if !source.is_empty() {
        // Vanilla 1.12 resolves an unknown category to `master` as well.
        let index = SOUND_SOURCES
            .iter()
            .position(|known| *known == source)
            .unwrap_or(0);
        flags |= 1;
        body.varint(index as i32);
    }
    if !sound.is_empty() {
        flags |= 2;
        body.string(&sound_name(step, sound));
    }
    body.buf[0] = flags;
    cx.to_client(state, Packet::new(packets::ids().stop_sound, body.buf));
    Ok(())
}

/// A sound's 1.13 name: vanilla sounds were renamed wholesale; others
/// (resource pack sounds) keep their name.
fn sound_name(step: &mut Up, sound: &str) -> String {
    let (old, new) = (
        data::version(super::super::OLDER).expect_registry("sound_event"),
        data::version(super::super::NEWER).expect_registry("sound_event"),
    );
    let mapping = step.shared.mapping(Direction::Clientbound, "sound_event");
    old.id(sound)
        .and_then(|id| mapping.get(id))
        .and_then(|id| new.name(id))
        .map_or_else(|| sound.to_owned(), |name| format!("minecraft:{name}"))
}

fn serverbound(step: &mut Up, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
    let mut reader = Reader::new(&packet.data);
    let channel = namespaced(reader.str()?);
    if !valid_identifier(&channel) {
        return Err(invalid("plugin channel is not a valid identifier"));
    }
    let payload = reader.rest();
    let Some(legacy) = step.channels.downgrade(&channel) else {
        // A 1.12 server would disconnect the player over a longer name, and
        // has never heard of the channel anyway.
        return Ok(());
    };
    let mut writer = Writer::with_capacity(packet.data.len());
    writer.string(&legacy);
    if matches!(legacy.as_str(), "REGISTER" | "UNREGISTER") {
        let channels = &step.channels;
        let list = channel_list(payload, |name| channels.downgrade(name));
        writer.bytes(&list);
    } else {
        writer.bytes(payload);
    }
    cx.to_backend(
        state,
        Packet::new(packets::ids().custom_payload, writer.buf),
    );
    Ok(())
}
