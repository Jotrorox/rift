//! The player list as 1.18.2 to 1.19.3 lay it out: one `player_info`
//! packet with a single action until 1.19.2 (1.19 adds the chat profile key
//! to added players), then 1.19.3's `player_info_update`, which combines
//! several actions per packet, and `player_info_remove`.
//!
//! The text codec owns these packets; the era's steps install these
//! conversions as fallbacks that a codec default replaces.
// INTEGRATION: drop the fallbacks once the text codec converts `player_info`
// between 758/759 and splits it for 760/761.
use crate::wire::{Reader, Writer, invalid};
use std::io;

const MAX_COMPONENT: usize = 262_144;

/// 1.19.3's action bits, in wire order.
pub const ADD: u8 = 1 << 0;
pub const INITIALIZE_CHAT: u8 = 1 << 1;
pub const GAME_MODE: u8 = 1 << 2;
pub const LISTED: u8 = 1 << 3;
pub const LATENCY: u8 = 1 << 4;
pub const DISPLAY_NAME: u8 = 1 << 5;

/// One player's entry with every field any action can carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub uuid: u128,
    pub name: String,
    /// Profile properties as on the wire, without their count.
    pub properties: Vec<u8>,
    pub property_count: usize,
    pub game_mode: i32,
    pub latency: i32,
    pub display_name: Option<String>,
}

/// The single action of a `player_info` up to 1.19.2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Add,
    GameMode,
    Latency,
    DisplayName,
    Remove,
}

impl Action {
    fn from_id(id: i32) -> io::Result<Self> {
        Ok(match id {
            0 => Self::Add,
            1 => Self::GameMode,
            2 => Self::Latency,
            3 => Self::DisplayName,
            4 => Self::Remove,
            _ => return Err(invalid("unknown player info action")),
        })
    }

    fn id(self) -> i32 {
        self as i32
    }
}

fn properties(r: &mut Reader<'_>, entry: &mut Entry) -> io::Result<()> {
    entry.property_count = r.count()?;
    let start = r.peek();
    for _ in 0..entry.property_count {
        r.str()?;
        r.str()?;
        if r.bool()? {
            r.str()?;
        }
    }
    entry.properties = start[..start.len() - r.remaining()].to_vec();
    Ok(())
}

fn display_name(r: &mut Reader<'_>) -> io::Result<Option<String>> {
    Ok(if r.bool()? {
        Some(r.string(MAX_COMPONENT)?.to_owned())
    } else {
        None
    })
}

fn write_display_name(w: &mut Writer, name: Option<&str>) {
    w.bool(name.is_some());
    if let Some(name) = name {
        w.string(name);
    }
}

fn skip_profile_key(r: &mut Reader<'_>) -> io::Result<()> {
    r.i64()?;
    r.byte_array()?;
    r.byte_array()?;
    Ok(())
}

/// A `player_info` of 758 to 760.
pub fn read_legacy(protocol: i32, data: &[u8]) -> io::Result<(Action, Vec<Entry>)> {
    let mut r = Reader::new(data);
    let action = Action::from_id(r.varint()?)?;
    let entries = (0..r.count()?)
        .map(|_| {
            let mut entry = Entry {
                uuid: r.uuid()?,
                ..Entry::default()
            };
            match action {
                Action::Add => {
                    entry.name = r.string(16)?.to_owned();
                    properties(&mut r, &mut entry)?;
                    entry.game_mode = r.varint()?;
                    entry.latency = r.varint()?;
                    entry.display_name = display_name(&mut r)?;
                    if protocol >= 759 && r.bool()? {
                        skip_profile_key(&mut r)?;
                    }
                }
                Action::GameMode => entry.game_mode = r.varint()?,
                Action::Latency => entry.latency = r.varint()?,
                Action::DisplayName => entry.display_name = display_name(&mut r)?,
                Action::Remove => {}
            }
            Ok(entry)
        })
        .collect::<io::Result<_>>()?;
    r.finish()?;
    Ok((action, entries))
}

/// A `player_info` of 758 to 760, without chat profile keys.
pub fn write_legacy(protocol: i32, action: Action, entries: &[Entry]) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(action.id()).varint(entries.len() as i32);
    for entry in entries {
        w.uuid(entry.uuid);
        match action {
            Action::Add => {
                w.string(&entry.name)
                    .varint(entry.property_count as i32)
                    .bytes(&entry.properties)
                    .varint(entry.game_mode)
                    .varint(entry.latency);
                write_display_name(&mut w, entry.display_name.as_deref());
                if protocol >= 759 {
                    w.bool(false);
                }
            }
            Action::GameMode => {
                w.varint(entry.game_mode);
            }
            Action::Latency => {
                w.varint(entry.latency);
            }
            Action::DisplayName => write_display_name(&mut w, entry.display_name.as_deref()),
            Action::Remove => {}
        }
    }
    w.buf
}

/// A 1.19.3 `player_info_update`: its action bits and entries. Players
/// keep being listed whatever the update says: older clients list everyone.
pub fn read_update(data: &[u8]) -> io::Result<(u8, Vec<Entry>)> {
    let mut r = Reader::new(data);
    let actions = r.u8()?;
    if actions & !0x3f != 0 {
        return Err(invalid("unknown player info actions"));
    }
    let entries = (0..r.count()?)
        .map(|_| {
            let mut entry = Entry {
                uuid: r.uuid()?,
                ..Entry::default()
            };
            if actions & ADD != 0 {
                entry.name = r.string(16)?.to_owned();
                properties(&mut r, &mut entry)?;
            }
            if actions & INITIALIZE_CHAT != 0 && r.bool()? {
                r.uuid()?; // Chat session.
                skip_profile_key(&mut r)?;
            }
            if actions & GAME_MODE != 0 {
                entry.game_mode = r.varint()?;
            }
            if actions & LISTED != 0 {
                r.bool()?;
            }
            if actions & LATENCY != 0 {
                entry.latency = r.varint()?;
            }
            if actions & DISPLAY_NAME != 0 {
                entry.display_name = display_name(&mut r)?;
            }
            Ok(entry)
        })
        .collect::<io::Result<_>>()?;
    r.finish()?;
    Ok((actions, entries))
}

/// A 1.19.3 `player_info_update` without chat sessions; added players are
/// listed.
pub fn write_update(actions: u8, entries: &[Entry]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(actions).varint(entries.len() as i32);
    for entry in entries {
        w.uuid(entry.uuid);
        if actions & ADD != 0 {
            w.string(&entry.name)
                .varint(entry.property_count as i32)
                .bytes(&entry.properties);
        }
        if actions & INITIALIZE_CHAT != 0 {
            w.bool(false);
        }
        if actions & GAME_MODE != 0 {
            w.varint(entry.game_mode);
        }
        if actions & LISTED != 0 {
            w.bool(true);
        }
        if actions & LATENCY != 0 {
            w.varint(entry.latency);
        }
        if actions & DISPLAY_NAME != 0 {
            write_display_name(&mut w, entry.display_name.as_deref());
        }
    }
    w.buf
}

/// A 1.19.3 `player_info_remove`.
pub fn read_remove(data: &[u8]) -> io::Result<Vec<Entry>> {
    let mut r = Reader::new(data);
    let count = r.count()?;
    let entries = (0..count)
        .map(|_| {
            Ok(Entry {
                uuid: r.uuid()?,
                ..Entry::default()
            })
        })
        .collect::<io::Result<_>>()?;
    r.finish()?;
    Ok(entries)
}

pub fn write_remove(entries: &[Entry]) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(entries.len() as i32);
    for entry in entries {
        w.uuid(entry.uuid);
    }
    w.buf
}

/// The 1.19.3 action bits that carry what a legacy action carries.
pub fn update_actions(action: Action) -> u8 {
    match action {
        Action::Add => ADD | INITIALIZE_CHAT | GAME_MODE | LISTED | LATENCY | DISPLAY_NAME,
        Action::GameMode => GAME_MODE,
        Action::Latency => LATENCY,
        Action::DisplayName => DISPLAY_NAME,
        Action::Remove => 0,
    }
}

/// The legacy actions that carry what a 1.19.3 update carries: adding a
/// player carries every field at once, otherwise one packet per field.
pub fn legacy_actions(actions: u8) -> Vec<Action> {
    if actions & ADD != 0 {
        return vec![Action::Add];
    }
    [
        (GAME_MODE, Action::GameMode),
        (LATENCY, Action::Latency),
        (DISPLAY_NAME, Action::DisplayName),
    ]
    .into_iter()
    .filter(|(bit, _)| actions & bit != 0)
    .map(|(_, action)| action)
    .collect()
}
