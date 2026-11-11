//! Plugin channels between a 1.12.2 client and a 1.13 backend.
//!
//! 1.13 names channels with namespaced identifiers and turned the vanilla
//! `MC|*` channels the client sends into packets of their own; a 1.12
//! client accepts channel names of at most 20 characters. Channels the
//! client uses that are not valid identifiers are renamed the way plugins
//! renamed theirs (`WDL|INIT` becomes `wdl:init`) and renamed back when the
//! backend answers on them; other backend channels longer than 20
//! characters, and 1.13's own debug channels, cannot reach the client.
use super::Down;
use crate::{
    Direction, Packet, State, codec,
    step::Cx,
    wire::{Position, Reader, Rewriter, Writer, invalid},
};
use std::io;

/// Vanilla and proxy channels by their 1.13 identifier and 1.12 name.
const RENAMED: &[(&str, &str)] = &[
    ("minecraft:brand", "MC|Brand"),
    ("minecraft:book_open", "MC|BOpen"),
    ("minecraft:trader_list", "MC|TrList"),
    ("minecraft:register", "REGISTER"),
    ("minecraft:unregister", "UNREGISTER"),
    ("bungeecord:main", "BungeeCord"),
    ("worldedit:cui", "WECUI"),
];

/// Renamed client channels remembered; more are still renamed, but the
/// backend's answers on them stay on the 1.13 identifier.
const MAX_REMEMBERED: usize = 256;

/// The longest channel name a 1.12 client reads.
const MAX_LEGACY_CHANNEL: usize = 20;

/// `SoundCategory` names in 1.12, in the order of 1.13's IDs for them.
const SOUND_SOURCES: &[&str] = &[
    "master", "music", "record", "weather", "block", "hostile", "neutral", "player", "ambient",
    "voice",
];

/// Whether `name` is a valid 1.13 resource location.
fn valid_identifier(name: &str) -> bool {
    let (namespace, path) = name.split_once(':').unwrap_or(("minecraft", name));
    let allowed = |c: char, slash: bool| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.".contains(c) || (slash && c == '/')
    };
    !path.is_empty()
        && namespace.chars().all(|c| allowed(c, false))
        && path.chars().all(|c| allowed(c, true))
}

impl Down {
    /// The 1.12 name of the 1.13 channel `identifier`, if a 1.12 client can
    /// be told about it.
    fn legacy_channel(&self, identifier: &str) -> Option<String> {
        if let Some((_, name)) = RENAMED.iter().find(|(modern, _)| *modern == identifier) {
            return Some((*name).to_owned());
        }
        if let Some(name) = self.channels.get(identifier) {
            return Some(name.clone());
        }
        if identifier.starts_with("minecraft:")
            || identifier.encode_utf16().count() > MAX_LEGACY_CHANNEL
        {
            return None;
        }
        Some(identifier.to_owned())
    }

    /// The 1.13 identifier for the 1.12 channel `name`, remembering renames.
    fn modern_channel(&mut self, name: &str) -> Option<String> {
        if let Some((identifier, _)) = RENAMED.iter().find(|(_, legacy)| *legacy == name) {
            return Some((*identifier).to_owned());
        }
        if name.starts_with("MC|") {
            return None;
        }
        if valid_identifier(name) {
            return Some(name.to_owned());
        }
        let renamed = name.to_lowercase().replacen('|', ":", 1);
        if !valid_identifier(&renamed) {
            return None;
        }
        if self.channels.len() < MAX_REMEMBERED {
            self.channels.insert(renamed.clone(), name.to_owned());
        }
        Some(renamed)
    }

    /// Rename every channel of a `REGISTER` or `UNREGISTER` list with
    /// `rename`; channels it rejects are left out.
    fn rename_list(
        &mut self,
        data: &[u8],
        mut rename: impl FnMut(&mut Self, &str) -> Option<String>,
    ) -> io::Result<Vec<u8>> {
        let text = std::str::from_utf8(data).map_err(|_| invalid("channel list is not UTF-8"))?;
        let renamed: Vec<String> = text
            .split('\0')
            .filter(|name| !name.is_empty())
            .filter_map(|name| rename(self, name))
            .collect();
        Ok(renamed.join("\0").into_bytes())
    }

    pub(super) fn clientbound_payload(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let channel = reader.string(32767)?;
        let data = reader.rest();
        let Some(name) = self.legacy_channel(channel) else {
            return Ok(());
        };
        let data = match channel {
            "minecraft:register" | "minecraft:unregister" => {
                let list = self.rename_list(data, |this, name| this.legacy_channel(name))?;
                if list.is_empty() {
                    return Ok(());
                }
                list
            }
            "minecraft:trader_list" => self.trader_list(data)?,
            _ => data.to_vec(),
        };
        let mut out = Writer::with_capacity(data.len() + name.len() + 1);
        out.string(&name).bytes(&data);
        cx.to_client(state, Packet::new(packet.id, out.into_inner()));
        Ok(())
    }

    /// A villager's trades: the layout stayed, the item stacks changed.
    fn trader_list(&mut self, data: &[u8]) -> io::Result<Vec<u8>> {
        let mut rw = Rewriter::new(data);
        rw.i32()?;
        for _ in 0..rw.u8()? {
            codec::item::transcode(&mut self.shared, Direction::Clientbound, &mut rw)?;
            codec::item::transcode(&mut self.shared, Direction::Clientbound, &mut rw)?;
            if rw.bool()? {
                codec::item::transcode(&mut self.shared, Direction::Clientbound, &mut rw)?;
            }
            rw.bool()?;
            rw.i32()?;
            rw.i32()?;
        }
        rw.finish()
    }

    /// 1.13's stop sound packet is 1.12's `MC|StopSound` channel: the
    /// source's name and the sound's, either empty for all.
    pub(super) fn stop_sound(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let flags = reader.u8()?;
        let source = if flags & 1 != 0 {
            let source = reader.varint()?;
            *usize::try_from(source)
                .ok()
                .and_then(|source| SOUND_SOURCES.get(source))
                .ok_or_else(|| invalid("invalid sound source"))?
        } else {
            ""
        };
        let sound = if flags & 2 != 0 {
            let sound = reader.string(32767)?;
            self.sound_name(sound).unwrap_or_else(|| sound.to_owned())
        } else {
            String::new()
        };
        reader.finish()?;
        let mut out = Writer::new();
        out.string("MC|StopSound").string(source).string(&sound);
        cx.to_client(
            state,
            Packet::new(
                Self::client_id(State::Play, Direction::Clientbound, "custom_payload"),
                out.into_inner(),
            ),
        );
        Ok(())
    }

    pub(super) fn serverbound_payload(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let channel = reader.string(MAX_LEGACY_CHANNEL)?;
        let data = reader.rest();
        let packet = match channel {
            "MC|BEdit" | "MC|BSign" => Some(self.edit_book(data, channel == "MC|BSign")?),
            "MC|ItemName" => Some(rename_item(data)?),
            "MC|TrSel" => Some(select_trade(data)?),
            "MC|Beacon" => Some(self.set_beacon(data)?),
            "MC|AutoCmd" => set_command_block(data)?,
            "MC|AdvCmd" => set_command_minecart(data)?,
            "MC|Struct" => set_structure_block(data)?,
            "MC|PickItem" => Some(pick_item(data)?),
            _ => self.renamed_payload(packet.id, channel, data)?,
        };
        if let Some(packet) = packet {
            cx.to_backend(state, packet);
        }
        Ok(())
    }

    /// A channel that stays a channel, under its 1.13 identifier.
    fn renamed_payload(
        &mut self,
        id: i32,
        channel: &str,
        data: &[u8],
    ) -> io::Result<Option<Packet>> {
        let Some(identifier) = self.modern_channel(channel) else {
            return Ok(None);
        };
        let data = if matches!(channel, "REGISTER" | "UNREGISTER") {
            let list = self.rename_list(data, Self::modern_channel)?;
            if list.is_empty() {
                return Ok(None);
            }
            list
        } else {
            data.to_vec()
        };
        let mut out = Writer::with_capacity(data.len() + identifier.len() + 2);
        out.string(&identifier).bytes(&data);
        Ok(Some(Packet::new(id, out.into_inner())))
    }

    /// `MC|BEdit` and `MC|BSign`: the edited book, signed or not.
    fn edit_book(&mut self, data: &[u8], signing: bool) -> io::Result<Packet> {
        let mut rw = Rewriter::new(data);
        codec::item::transcode(&mut self.shared, Direction::Serverbound, &mut rw)?;
        rw.w.bool(signing);
        Ok(play_packet("edit_book", rw.finish()?))
    }

    /// `MC|Beacon`: the chosen effects as Ints; 1.13 sends VarInts.
    fn set_beacon(&mut self, data: &[u8]) -> io::Result<Packet> {
        let mut reader = Reader::new(data);
        let (primary, secondary) = (reader.i32()?, reader.i32()?);
        reader.finish()?;
        let effects = self.shared.mapping(Direction::Serverbound, "mob_effect");
        let mut out = Writer::new();
        // An ID without an effect (none chosen) keeps its value.
        out.varint(effects.get(primary).unwrap_or(primary))
            .varint(effects.get(secondary).unwrap_or(secondary));
        Ok(play_packet("set_beacon", out.into_inner()))
    }
}

/// A 1.13 serverbound play packet by name.
fn play_packet(name: &str, data: Vec<u8>) -> Packet {
    Packet::new(
        Down::backend_id(State::Play, Direction::Serverbound, name),
        data,
    )
}

/// `MC|ItemName`: the anvil's new name, or nothing to clear it.
fn rename_item(data: &[u8]) -> io::Result<Packet> {
    let mut reader = Reader::new(data);
    let name = if reader.is_empty() {
        ""
    } else {
        reader.string(32767)?
    };
    reader.finish()?;
    let mut out = Writer::new();
    out.string(name);
    Ok(play_packet("rename_item", out.into_inner()))
}

/// `MC|TrSel`: the selected trade as an Int.
fn select_trade(data: &[u8]) -> io::Result<Packet> {
    let mut reader = Reader::new(data);
    let slot = reader.i32()?;
    reader.finish()?;
    let mut out = Writer::new();
    out.varint(slot);
    Ok(play_packet("select_trade", out.into_inner()))
}

/// `MC|PickItem`: the inventory slot to pick from, as in 1.13.
fn pick_item(data: &[u8]) -> io::Result<Packet> {
    let mut reader = Reader::new(data);
    let slot = reader.varint()?;
    reader.finish()?;
    let mut out = Writer::new();
    out.varint(slot);
    Ok(play_packet("pick_item", out.into_inner()))
}

fn int_position(reader: &mut Reader<'_>) -> io::Result<Position> {
    Ok(Position {
        x: reader.i32()?,
        y: reader.i32()?,
        z: reader.i32()?,
    })
}

/// `MC|AutoCmd`, the command block screen: position as three Ints, command,
/// track output, mode by name, conditional, automatic. 1.13 packs the mode
/// as its ordinal and the three switches as flags.
fn set_command_block(data: &[u8]) -> io::Result<Option<Packet>> {
    let mut reader = Reader::new(data);
    let position = int_position(&mut reader)?;
    let command = reader.string(32767)?;
    let track_output = reader.bool()?;
    let mode = reader.string(16)?;
    let (conditional, automatic) = (reader.bool()?, reader.bool()?);
    reader.finish()?;
    // Like the 1.12 server, ignore a mode it does not know.
    let Some(mode) = ["SEQUENCE", "AUTO", "REDSTONE"]
        .iter()
        .position(|known| *known == mode)
    else {
        return Ok(None);
    };
    let flags = u8::from(track_output) | u8::from(conditional) << 1 | u8::from(automatic) << 2;
    let mut out = Writer::new();
    out.position_legacy(position)
        .string(command)
        .varint(mode as i32)
        .u8(flags);
    Ok(Some(play_packet("set_command_block", out.into_inner())))
}

/// `MC|AdvCmd`: a command block (type 0, by position) or command block
/// minecart (type 1, by entity ID), then the command and track output. The
/// 1.12 client sends type 0 only through `MC|AutoCmd`, and a 1.13 command
/// block update needs the mode and switches type 0 lacks; it is dropped.
fn set_command_minecart(data: &[u8]) -> io::Result<Option<Packet>> {
    let mut reader = Reader::new(data);
    match reader.u8()? {
        0 => {
            int_position(&mut reader)?;
            reader.string(32767)?;
            reader.bool()?;
            reader.finish()?;
            Ok(None)
        }
        1 => {
            let entity = reader.i32()?;
            let command = reader.string(32767)?;
            let track_output = reader.bool()?;
            reader.finish()?;
            let mut out = Writer::new();
            out.varint(entity).string(command).bool(track_output);
            Ok(Some(play_packet("set_command_minecart", out.into_inner())))
        }
        _ => Err(invalid("invalid command block type")),
    }
}

/// `MC|Struct`, the structure block screen. 1.12 numbers the action from
/// one and names the mode, mirror and rotation; 1.13 numbers the action from
/// zero, sends the others as ordinals, the offsets and size as bytes and the
/// three switches as flags.
fn set_structure_block(data: &[u8]) -> io::Result<Option<Packet>> {
    let mut reader = Reader::new(data);
    let position = int_position(&mut reader)?;
    let action = reader.i8()?;
    let mode = reader.string(32)?;
    let name = reader.string(64)?;
    let mut offset = [0i8; 3];
    for value in &mut offset {
        *value = reader.i32()?.clamp(-32, 32) as i8;
    }
    let mut size = [0i8; 3];
    for value in &mut size {
        *value = reader.i32()?.clamp(0, 32) as i8;
    }
    let mirror = reader.string(32)?;
    let rotation = reader.string(32)?;
    let metadata = reader.string(128)?;
    let switches = [reader.bool()?, reader.bool()?, reader.bool()?];
    let integrity = reader.f32()?;
    let seed = reader.varlong()?;
    reader.finish()?;
    let ordinal = |value: &str, names: &[&str]| names.iter().position(|known| *known == value);
    let (Some(mode), Some(mirror), Some(rotation)) = (
        ordinal(mode, &["SAVE", "LOAD", "CORNER", "DATA"]),
        ordinal(mirror, &["NONE", "LEFT_RIGHT", "FRONT_BACK"]),
        ordinal(
            rotation,
            &[
                "NONE",
                "CLOCKWISE_90",
                "CLOCKWISE_180",
                "COUNTERCLOCKWISE_90",
            ],
        ),
    ) else {
        // The 1.12 server rejects names it does not know as well.
        return Ok(None);
    };
    if !(1..=4).contains(&action) {
        return Ok(None);
    }
    let flags = switches
        .iter()
        .enumerate()
        .fold(0u8, |flags, (bit, on)| flags | u8::from(*on) << bit);
    let mut out = Writer::new();
    out.position_legacy(position)
        .varint(i32::from(action) - 1)
        .varint(mode as i32)
        .string(name);
    for value in offset.into_iter().chain(size) {
        out.i8(value);
    }
    out.varint(mirror as i32)
        .varint(rotation as i32)
        .string(metadata)
        .f32(integrity)
        .varlong(seed)
        .u8(flags);
    Ok(Some(play_packet("set_structure_block", out.into_inner())))
}
