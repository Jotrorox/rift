//! Chat packets as 1.18.2 to 1.19.3 lay them out, reduced to what survives
//! translation: the text, who sent it and how to show it. Signatures,
//! last-seen lists and message headers are read and dropped.
use super::chat::{literal, parse_component};
use crate::wire::{Reader, Writer, invalid};
use serde_json::Value;
use std::io;

/// Text components travel as JSON strings of at most this many characters.
const MAX_COMPONENT: usize = 262_144;
/// Chat messages and commands are at most 256 characters.
pub const MAX_MESSAGE: usize = 256;
/// 1.19.3 signatures have a fixed size.
const SIGNATURE: usize = 256;

fn optional<'a, T>(
    r: &mut Reader<'a>,
    value: impl FnOnce(&mut Reader<'a>) -> io::Result<T>,
) -> io::Result<Option<T>> {
    if r.bool()? {
        value(r).map(Some)
    } else {
        Ok(None)
    }
}

fn component(r: &mut Reader<'_>) -> io::Result<Value> {
    Ok(parse_component(r.string(MAX_COMPONENT)?))
}

fn write_component(w: &mut Writer, value: &Value) {
    w.string(&value.to_string());
}

fn write_optional_component(w: &mut Writer, value: Option<&Value>) {
    w.bool(value.is_some());
    if let Some(value) = value {
        write_component(w, value);
    }
}

/// 1.19.1–1.19.2: up to five `(sender, signature)` pairs.
fn skip_last_seen_1_19_1(r: &mut Reader<'_>) -> io::Result<()> {
    for _ in 0..r.count()? {
        r.uuid()?;
        r.byte_array()?;
    }
    Ok(())
}

/// 1.19.1–1.19.2: the last message the client saw but did not accept.
fn skip_last_rejected_1_19_1(r: &mut Reader<'_>) -> io::Result<()> {
    optional(r, |r| {
        r.uuid()?;
        r.byte_array()?;
        Ok(())
    })?;
    Ok(())
}

/// The 1.19.1+ filter mask: a type, and for partial filtering a bit set.
fn skip_filter_mask(r: &mut Reader<'_>) -> io::Result<()> {
    if r.varint()? == 2 {
        let longs = r.count()?;
        r.take(
            longs
                .checked_mul(8)
                .ok_or_else(|| invalid("invalid filter mask"))?,
        )?;
    }
    Ok(())
}

/// A serverbound chat message or command, unsigned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub text: String,
    /// `None` from 1.18.2, which sends no timestamp.
    pub timestamp: Option<i64>,
    pub salt: i64,
}

impl Sent {
    /// A serverbound `chat` in `protocol`'s layout.
    pub fn read_chat(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let text = r.string(MAX_MESSAGE)?.to_owned();
        let mut sent = Self {
            text,
            timestamp: None,
            salt: 0,
        };
        if protocol >= 759 {
            sent.timestamp = Some(r.i64()?);
            sent.salt = r.i64()?;
            if protocol >= 761 {
                optional(&mut r, |r| r.take(SIGNATURE))?;
                r.varint()?; // Messages acknowledged since the last update.
                r.take(3)?; // Acknowledged bit set.
            } else {
                r.byte_array()?; // Signature.
                r.bool()?; // Signed preview.
                if protocol >= 760 {
                    skip_last_seen_1_19_1(&mut r)?;
                    skip_last_rejected_1_19_1(&mut r)?;
                }
            }
        }
        r.finish()?;
        Ok(sent)
    }

    /// A serverbound `chat_command` (1.19 and later).
    pub fn read_command(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let text = r.string(MAX_MESSAGE)?.to_owned();
        let timestamp = Some(r.i64()?);
        let salt = r.i64()?;
        for _ in 0..r.count()? {
            r.string(16)?; // Argument name.
            if protocol >= 761 {
                r.take(SIGNATURE)?;
            } else {
                r.byte_array()?;
            }
        }
        if protocol >= 761 {
            r.varint()?;
            r.take(3)?;
        } else {
            r.bool()?; // Signed preview.
            if protocol >= 760 {
                skip_last_seen_1_19_1(&mut r)?;
                skip_last_rejected_1_19_1(&mut r)?;
            }
        }
        r.finish()?;
        Ok(Self {
            text,
            timestamp,
            salt,
        })
    }

    /// An unsigned serverbound `chat`. `acknowledged` is the 1.19.3 offset:
    /// how many signed messages arrived since the last acknowledgement.
    pub fn write_chat(&self, protocol: i32, timestamp: i64, acknowledged: i32) -> Vec<u8> {
        let mut w = Writer::new();
        w.string(&self.text);
        if protocol >= 759 {
            w.i64(timestamp).i64(self.salt);
            self.write_unsigned_tail(protocol, acknowledged, &mut w, true);
        }
        w.buf
    }

    /// An unsigned serverbound `chat_command` (1.19 and later).
    pub fn write_command(&self, protocol: i32, timestamp: i64, acknowledged: i32) -> Vec<u8> {
        let mut w = Writer::new();
        w.string(&self.text).i64(timestamp).i64(self.salt).varint(0);
        self.write_unsigned_tail(protocol, acknowledged, &mut w, false);
        w.buf
    }

    fn write_unsigned_tail(&self, protocol: i32, acknowledged: i32, w: &mut Writer, chat: bool) {
        if protocol >= 761 {
            if chat {
                w.bool(false); // No signature.
            }
            w.varint(acknowledged).bytes(&[0; 3]);
        } else {
            if chat {
                w.byte_array(&[]); // Empty signature.
            }
            w.bool(false); // Not a signed preview.
            if protocol >= 760 {
                w.varint(0).bool(false); // Nothing seen, nothing rejected.
            }
        }
    }
}

/// A message from a player (or the server speaking as one) to show in chat.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerChat {
    pub sender: u128,
    /// The content as the receiver shows it, before decoration.
    pub content: Value,
    pub chat_type: i32,
    pub sender_name: Value,
    pub target_name: Option<Value>,
    /// Whether the message carried a signature: 1.19.3 backends then wait
    /// for the client to acknowledge it.
    pub signed: bool,
}

impl PlayerChat {
    /// A clientbound `player_chat` of 1.19, 1.19.1–1.19.2 or 1.19.3.
    pub fn read(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let message = match protocol {
            759 => {
                let signed_content = component(&mut r)?;
                let unsigned = optional(&mut r, component)?;
                let chat_type = r.varint()?;
                let sender = r.uuid()?;
                let sender_name = component(&mut r)?;
                let target_name = optional(&mut r, component)?;
                r.i64()?; // Timestamp.
                r.i64()?; // Salt.
                let signature = r.byte_array()?;
                Self {
                    sender,
                    content: unsigned.unwrap_or(signed_content),
                    chat_type,
                    sender_name,
                    target_name,
                    signed: !signature.is_empty(),
                }
            }
            760 => {
                optional(&mut r, |r| r.byte_array())?; // Previous signature.
                let sender = r.uuid()?;
                let signature = r.byte_array()?;
                let plain = r.string(MAX_MESSAGE)?.to_owned();
                let formatted = optional(&mut r, component)?;
                r.i64()?;
                r.i64()?;
                skip_last_seen_1_19_1(&mut r)?;
                let unsigned = optional(&mut r, component)?;
                skip_filter_mask(&mut r)?;
                Self {
                    sender,
                    content: unsigned.or(formatted).unwrap_or_else(|| literal(&plain)),
                    chat_type: r.varint()?,
                    sender_name: component(&mut r)?,
                    target_name: optional(&mut r, component)?,
                    signed: !signature.is_empty(),
                }
            }
            761.. => {
                let sender = r.uuid()?;
                r.varint()?; // Index in the sender's message chain.
                let signature = optional(&mut r, |r| r.take(SIGNATURE))?;
                let plain = r.string(MAX_MESSAGE)?.to_owned();
                r.i64()?;
                r.i64()?;
                for _ in 0..r.count()? {
                    if r.varint()? == 0 {
                        r.take(SIGNATURE)?;
                    }
                }
                let unsigned = optional(&mut r, component)?;
                skip_filter_mask(&mut r)?;
                Self {
                    sender,
                    content: unsigned.unwrap_or_else(|| literal(&plain)),
                    chat_type: r.varint()?,
                    sender_name: component(&mut r)?,
                    target_name: optional(&mut r, component)?,
                    signed: signature.is_some(),
                }
            }
            _ => return Err(invalid("no player_chat before 1.19")),
        };
        r.finish()?;
        Ok(message)
    }
}

/// A clientbound 1.19.3 `disguised_chat`: player chat without a signature.
pub fn read_disguised_chat(data: &[u8]) -> io::Result<PlayerChat> {
    let mut r = Reader::new(data);
    let message = PlayerChat {
        sender: 0,
        content: component(&mut r)?,
        chat_type: r.varint()?,
        sender_name: component(&mut r)?,
        target_name: optional(&mut r, component)?,
        signed: false,
    };
    r.finish()?;
    Ok(message)
}

/// A clientbound 1.19.3 `disguised_chat`.
pub fn write_disguised_chat(message: &PlayerChat) -> Vec<u8> {
    let mut w = Writer::new();
    write_component(&mut w, &message.content);
    w.varint(message.chat_type);
    write_component(&mut w, &message.sender_name);
    write_optional_component(&mut w, message.target_name.as_ref());
    w.buf
}

/// Where a system message shows, in the form each version encodes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// 1.18.2: 0 chat, 1 system, 2 above the hotbar.
    Position(i8),
    /// 1.19: a chat type ID.
    ChatType(i32),
    /// 1.19.1 and later: above the hotbar or in chat.
    Overlay(bool),
}

/// A message for the chat (or the action bar) as 1.18.2's `chat` and later
/// versions' `system_chat` carry it, with its JSON kept as sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemChat {
    pub json: String,
    pub placement: Placement,
    /// 1.18.2 only: the player who sent it, or zero.
    pub sender: u128,
}

impl SystemChat {
    /// 1.18.2's `chat` or a later `system_chat`.
    pub fn read(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let json = r.string(MAX_COMPONENT)?.to_owned();
        let (placement, sender) = match protocol {
            ..759 => (Placement::Position(r.i8()?), r.uuid()?),
            759 => (Placement::ChatType(r.varint()?), 0),
            _ => (Placement::Overlay(r.bool()?), 0),
        };
        r.finish()?;
        Ok(Self {
            json,
            placement,
            sender,
        })
    }

    pub fn write(&self, protocol: i32) -> io::Result<Vec<u8>> {
        let mut w = Writer::new();
        w.string(&self.json);
        match (protocol, self.placement) {
            (..759, Placement::Position(position)) => {
                w.i8(position).uuid(self.sender);
            }
            (759, Placement::ChatType(chat_type)) => {
                w.varint(chat_type);
            }
            (760.., Placement::Overlay(overlay)) => {
                w.bool(overlay);
            }
            _ => return Err(invalid("system chat placement in the wrong form")),
        }
        Ok(w.buf)
    }
}

/// A clientbound `server_data` (1.19 and later), the parts that survive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerData {
    pub motd: Option<String>,
    pub icon: Option<String>,
}

impl ServerData {
    pub fn read(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let motd = optional(&mut r, |r| Ok(r.string(MAX_COMPONENT)?.to_owned()))?;
        let icon = optional(&mut r, |r| Ok(r.str()?.to_owned()))?;
        if protocol < 761 {
            r.bool()?; // Previews chat.
        }
        if protocol >= 760 {
            r.bool()?; // Enforces secure chat.
        }
        r.finish()?;
        Ok(Self { motd, icon })
    }

    /// Chat previews stay off and secure chat unenforced: translated chat is
    /// never signed.
    pub fn write(&self, protocol: i32) -> Vec<u8> {
        let mut w = Writer::new();
        for value in [&self.motd, &self.icon] {
            w.bool(value.is_some());
            if let Some(value) = value {
                w.string(value);
            }
        }
        if protocol < 761 {
            w.bool(false);
        }
        if protocol >= 760 {
            w.bool(false);
        }
        w.buf
    }
}
