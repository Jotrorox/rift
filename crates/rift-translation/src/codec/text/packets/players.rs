//! The player list: `player_info` (one action per packet, to 1.19.2) and
//! `player_info_update`/`player_info_remove` (action bitsets, 1.19.3+),
//! converted into each other for the 1.19.2/1.19.3 pair.
use super::{Cv, crosses, text_changes};
use crate::{
    codec::text::Component,
    wire::{Reader, Writer, invalid},
};
use std::io;

/// 1.19 added profile key data to added players.
const PROFILE_KEYS: i32 = 759;
/// 1.19.3 introduced the bitset form.
const BITSET: i32 = 761;
/// 1.21.2 added the list order action, 1.21.4 the hat action.
const LIST_ORDER: i32 = 768;
const HAT: i32 = 769;

pub fn player_info_needed(from: i32, to: i32) -> bool {
    text_changes(from, to) || crosses(from, to, PROFILE_KEYS) || crosses(from, to, BITSET)
}

pub fn player_info_update_needed(from: i32, to: i32) -> bool {
    text_changes(from, to)
        || [BITSET, LIST_ORDER, HAT]
            .iter()
            .any(|boundary| crosses(from, to, *boundary))
}

pub fn player_info_remove_needed(from: i32, to: i32) -> bool {
    crosses(from, to, BITSET)
}

#[derive(Clone, Debug)]
struct Property {
    name: String,
    value: String,
    signature: Option<String>,
}

/// One player's entry with whatever the packet carried for it.
#[derive(Clone, Debug, Default)]
struct Entry {
    uuid: u128,
    profile: Option<(String, Vec<Property>)>,
    /// 1.19.3+: the chat session, raw (absent or present).
    session: Option<Option<Vec<u8>>>,
    /// 1.19–1.19.2: the profile key data of added players, raw.
    key: Option<Vec<u8>>,
    game_mode: Option<i32>,
    listed: Option<bool>,
    latency: Option<i32>,
    display_name: Option<Option<Component>>,
    list_order: Option<i32>,
    hat: Option<bool>,
}

fn read_profile(r: &mut Reader<'_>) -> io::Result<(String, Vec<Property>)> {
    let name = r.string(16)?.to_owned();
    let count = r.count()?;
    let mut properties = Vec::with_capacity(count);
    for _ in 0..count {
        let name = r.string(64)?.to_owned();
        let value = r.str()?.to_owned();
        let signature = if r.bool()? {
            Some(r.string(1024)?.to_owned())
        } else {
            None
        };
        properties.push(Property {
            name,
            value,
            signature,
        });
    }
    Ok((name, properties))
}

fn write_profile(w: &mut Writer, (name, properties): &(String, Vec<Property>)) {
    w.string(name).varint(properties.len() as i32);
    for property in properties {
        w.string(&property.name)
            .string(&property.value)
            .bool(property.signature.is_some());
        if let Some(signature) = &property.signature {
            w.string(signature);
        }
    }
}

/// Copy the raw bytes `read` consumes.
fn raw(
    r: &mut Reader<'_>,
    read: impl FnOnce(&mut Reader<'_>) -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    let start = r.peek();
    read(r)?;
    Ok(start[..start.len() - r.remaining()].to_vec())
}

/// The one-action form: 0 add, 1 game mode, 2 latency, 3 display name,
/// 4 remove.
pub fn player_info(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut r = Reader::new(data);
    let action = r.varint()?;
    let count = r.count()?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let mut entry = Entry {
            uuid: r.uuid()?,
            ..Entry::default()
        };
        match action {
            0 => {
                entry.profile = Some(read_profile(&mut r)?);
                entry.game_mode = Some(r.varint()?);
                entry.latency = Some(r.varint()?);
                entry.display_name = Some(cv.read_optional_component(&mut r)?);
                entry.listed = Some(true);
                if cv.from >= PROFILE_KEYS && r.bool()? {
                    entry.key = Some(raw(&mut r, |r| {
                        r.i64()?;
                        r.byte_array()?;
                        r.byte_array().map(drop)
                    })?);
                }
            }
            1 => entry.game_mode = Some(r.varint()?),
            2 => entry.latency = Some(r.varint()?),
            3 => entry.display_name = Some(cv.read_optional_component(&mut r)?),
            4 => {}
            _ => return Err(invalid("unknown player info action")),
        }
        entries.push(entry);
    }
    r.finish()?;
    if cv.to >= BITSET {
        if action == 4 {
            emit_remove(cv, &entries);
        } else {
            emit_update(cv, &entries, None);
        }
    } else {
        emit_legacy(cv, action, &entries);
    }
    Ok(())
}

pub fn player_info_remove(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut r = Reader::new(data);
    let count = r.count()?;
    let entries = (0..count)
        .map(|_| {
            Ok(Entry {
                uuid: r.uuid()?,
                ..Entry::default()
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    r.finish()?;
    if cv.to >= BITSET {
        emit_remove(cv, &entries);
    } else {
        emit_legacy(cv, 4, &entries);
    }
    Ok(())
}

/// The actions of the bitset form in bit order, with the protocol each
/// appeared in.
const ACTIONS: [i32; 8] = [
    BITSET, BITSET, BITSET, BITSET, BITSET, BITSET, LIST_ORDER, HAT,
];

pub fn player_info_update(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut r = Reader::new(data);
    let actions = r.u8()?;
    if (0..8).any(|bit| actions & (1 << bit) != 0 && cv.from < ACTIONS[bit]) {
        return Err(invalid("unknown player info actions"));
    }
    let has = |bit: u8| actions & (1 << bit) != 0;
    let count = r.count()?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let mut entry = Entry {
            uuid: r.uuid()?,
            ..Entry::default()
        };
        if has(0) {
            entry.profile = Some(read_profile(&mut r)?);
        }
        if has(1) {
            entry.session = Some(if r.bool()? {
                Some(raw(&mut r, |r| {
                    r.uuid()?;
                    r.i64()?;
                    r.byte_array()?;
                    r.byte_array().map(drop)
                })?)
            } else {
                None
            });
        }
        if has(2) {
            entry.game_mode = Some(r.varint()?);
        }
        if has(3) {
            entry.listed = Some(r.bool()?);
        }
        if has(4) {
            entry.latency = Some(r.varint()?);
        }
        if has(5) {
            entry.display_name = Some(cv.read_optional_component(&mut r)?);
        }
        if has(6) {
            entry.list_order = Some(r.varint()?);
        }
        if has(7) {
            entry.hat = Some(r.bool()?);
        }
        entries.push(entry);
    }
    r.finish()?;
    if cv.to >= BITSET {
        // An update whose every action the receiver lacks carries nothing.
        let kept = (0..8).any(|bit| has(bit) && cv.to >= ACTIONS[usize::from(bit)]);
        if kept || actions == 0 {
            emit_update(cv, &entries, Some(actions));
        }
        return Ok(());
    }
    // One packet per action for the older client; an added player carries
    // its game mode, latency and display name along.
    for entry in &entries {
        if entry.profile.is_some() {
            emit_legacy(cv, 0, std::slice::from_ref(entry));
            continue;
        }
        if entry.game_mode.is_some() {
            emit_legacy(cv, 1, std::slice::from_ref(entry));
        }
        if entry.latency.is_some() {
            emit_legacy(cv, 2, std::slice::from_ref(entry));
        }
        if entry.display_name.is_some() {
            emit_legacy(cv, 3, std::slice::from_ref(entry));
        }
    }
    Ok(())
}

fn emit_remove(cv: &mut Cv<'_>, entries: &[Entry]) {
    let mut w = Writer::new();
    w.varint(entries.len() as i32);
    for entry in entries {
        w.uuid(entry.uuid);
    }
    cv.emit("player_info_remove", w.into_inner());
}

/// `actions` is the sender's bitset; without one (the one-action form) the
/// union of what the entries carry. Either is limited to the receiver's
/// actions.
fn emit_update(cv: &mut Cv<'_>, entries: &[Entry], actions: Option<u8>) {
    let present = |entry: &Entry| {
        [
            entry.profile.is_some(),
            entry.session.is_some(),
            entry.game_mode.is_some(),
            entry.listed.is_some(),
            entry.latency.is_some(),
            entry.display_name.is_some(),
            entry.list_order.is_some(),
            entry.hat.is_some(),
        ]
    };
    let mut actions = actions.unwrap_or_else(|| {
        entries.iter().fold(0, |actions, entry| {
            present(entry)
                .into_iter()
                .enumerate()
                .fold(actions, |actions, (bit, present)| {
                    actions | (u8::from(present) << bit)
                })
        })
    });
    for (bit, since) in ACTIONS.iter().enumerate() {
        if cv.to < *since {
            actions &= !(1 << bit);
        }
    }
    let mut w = Writer::new();
    w.u8(actions).varint(entries.len() as i32);
    let has = |bit: u8| actions & (1 << bit) != 0;
    for entry in entries {
        w.uuid(entry.uuid);
        if has(0) {
            write_profile(
                &mut w,
                entry
                    .profile
                    .as_ref()
                    .unwrap_or(&(String::new(), Vec::new())),
            );
        }
        if has(1) {
            match entry.session.as_ref().and_then(Option::as_ref) {
                Some(session) => drop(w.bool(true).bytes(session)),
                None => drop(w.bool(false)),
            }
        }
        if has(2) {
            w.varint(entry.game_mode.unwrap_or(0));
        }
        if has(3) {
            w.bool(entry.listed.unwrap_or(true));
        }
        if has(4) {
            w.varint(entry.latency.unwrap_or(0));
        }
        if has(5) {
            cv.write_optional_component(
                &mut w,
                entry.display_name.as_ref().and_then(Option::as_ref),
            );
        }
        if has(6) {
            w.varint(entry.list_order.unwrap_or(0));
        }
        if has(7) {
            w.bool(entry.hat.unwrap_or(true));
        }
    }
    cv.emit("player_info_update", w.into_inner());
}

fn emit_legacy(cv: &mut Cv<'_>, action: i32, entries: &[Entry]) {
    let mut w = Writer::new();
    w.varint(action).varint(entries.len() as i32);
    for entry in entries {
        w.uuid(entry.uuid);
        match action {
            0 => {
                write_profile(
                    &mut w,
                    entry
                        .profile
                        .as_ref()
                        .unwrap_or(&(String::new(), Vec::new())),
                );
                w.varint(entry.game_mode.unwrap_or(0));
                w.varint(entry.latency.unwrap_or(0));
                cv.write_optional_component(
                    &mut w,
                    entry.display_name.as_ref().and_then(Option::as_ref),
                );
                if cv.to >= PROFILE_KEYS {
                    match &entry.key {
                        Some(key) => drop(w.bool(true).bytes(key)),
                        None => drop(w.bool(false)),
                    }
                }
            }
            1 => drop(w.varint(entry.game_mode.unwrap_or(0))),
            2 => drop(w.varint(entry.latency.unwrap_or(0))),
            3 => cv.write_optional_component(
                &mut w,
                entry.display_name.as_ref().and_then(Option::as_ref),
            ),
            _ => {}
        }
    }
    cv.emit("player_info", w.into_inner());
}
