//! Minecraft 1.21.4 (protocol 769) and 1.21.5 (protocol 770).
//!
//! What the step itself has to handle, learnt from both servers' traffic and
//! Mojang's code (the rest of this big release, from hashed item stacks to
//! entity variants and chunk heightmaps, is the shared codecs'):
//!
//! - Experience orbs: 1.21.5 spawns them with `add_entity` and carries their
//!   value in entity data (index 8) instead of `add_experience_orb`.
//! - Chat: `player_chat` starts with a global message index the 1.21.5
//!   client checks for gaps, and serverbound `chat` and
//!   `chat_command_signed` end with a checksum of the last seen messages,
//!   where 0 tells the server not to check. Signatures are unchanged, so
//!   signed chat keeps working in both directions.
//! - Shields: a 1.21.4 server has the client play the block and break sounds
//!   (entity events 29 and 30); a 1.21.5 server plays them itself.
//! - Test blocks and test instances are new in 1.21.5 and have no
//!   equivalent before.
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;
use crate::{
    Direction,
    codec::{Shared, text},
    data,
    nbt::Nbt,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

pub const OLDER: i32 = 769;
pub const NEWER: i32 = 770;

/// A 1.21.5 client on a 1.21.4 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.4 client on a 1.21.5 backend.
pub const DOWN: Option<Make> = down::STEP;

/// `ExperienceOrb.DATA_VALUE` in 1.21.5 and the `INT` serializer it uses.
const ORB_VALUE_INDEX: u8 = 8;
const INT_SERIALIZER: i32 = 1;
/// `LastSeenMessages.Update.IGNORE_CHECKSUM`.
const IGNORE_CHECKSUM: u8 = 0;
/// `LastSeenMessages`: signatures a message acknowledges at most, and the
/// fixed bit set of 20 acknowledgements on the wire.
const LAST_SEEN_MAX: usize = 20;
const ACKNOWLEDGED_BYTES: usize = 3;
const SIGNATURE_BYTES: usize = 256;

fn registry_id(protocol: i32, registry: &str, name: &str) -> i32 {
    data::version(protocol)
        .expect_registry(registry)
        .id(name)
        .unwrap_or_else(|| panic!("protocol {protocol} has no {registry} {name}"))
}

/// The UUID a spawned experience orb gets: 1.21.4 sends none, 1.21.5 wants
/// one. Derived from the entity ID so it is stable and unique per world.
fn orb_uuid(entity: i32) -> u128 {
    0x5269_6674_0000_4000_8000_0000_0000_0000 | u128::from(entity as u32)
}

/// A 1.21.4 `add_experience_orb` as the 1.21.5 `add_entity` and
/// `set_entity_data` its server sends for a new orb.
fn orb_to_entity(body: &[u8]) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut reader = Reader::new(body);
    let entity = reader.varint()?;
    let position = reader.take(3 * 8)?;
    let value = reader.i16()?;
    reader.finish()?;
    let mut spawn = Writer::new();
    spawn
        .varint(entity)
        .uuid(orb_uuid(entity))
        .varint(registry_id(NEWER, "entity_type", "experience_orb"))
        .bytes(position)
        .bytes(&[0, 0, 0]) // Pitch, yaw, head yaw.
        .varint(0) // The 1.21.5 server's data: the value is in entity data.
        .bytes(&[0; 6]); // No velocity.
    let mut entity_data = Writer::new();
    entity_data
        .varint(entity)
        .u8(ORB_VALUE_INDEX)
        .varint(INT_SERIALIZER)
        .varint(i32::from(value))
        .u8(0xff);
    Ok((spawn.buf, entity_data.buf))
}

/// The bodies of an `add_experience_orb` and, for a moving orb, its
/// `set_entity_motion`.
type Orb = (Vec<u8>, Option<Vec<u8>>);

/// A 1.21.5 `add_entity`'s fields when it spawns an experience orb: the
/// 1.21.4 `add_experience_orb`, and a `set_entity_motion` for a moving orb.
/// `None` for any other entity. 1.21.5 servers leave the orb's value to
/// entity data that a 1.21.4 client cannot take, so the orb gets the
/// smallest size unless an older side sent its value as spawn data.
fn entity_to_orb(body: &[u8]) -> io::Result<Option<Orb>> {
    let mut reader = Reader::new(body);
    let entity = reader.varint()?;
    reader.uuid()?;
    if reader.varint()? != registry_id(NEWER, "entity_type", "experience_orb") {
        return Ok(None);
    }
    let position = reader.take(3 * 8)?;
    reader.take(3)?; // Pitch, yaw, head yaw.
    let value = reader.varint()?;
    let velocity = reader.take(6)?;
    reader.finish()?;
    let mut orb = Writer::new();
    orb.varint(entity)
        .bytes(position)
        .i16(value.clamp(0, i32::from(i16::MAX)) as i16);
    let motion = velocity.iter().any(|byte| *byte != 0).then(|| {
        let mut motion = Writer::new();
        motion.varint(entity).bytes(velocity);
        motion.buf
    });
    Ok(Some((orb.buf, motion)))
}

/// Serverbound `chat` or `chat_command_signed` with the 1.21.5 checksum
/// byte appended (as "do not check") or removed. Both are parsed whole: the
/// checksum is the last byte only when the rest is what it should be.
fn chat_checksum(body: &[u8], command: bool, towards_newer: bool) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    if command {
        rw.str()?;
        rw.i64()?; // Timestamp.
        rw.i64()?; // Salt.
        let signatures = rw.count()?;
        if signatures > 8 {
            return Err(invalid("too many argument signatures"));
        }
        for _ in 0..signatures {
            rw.string(16)?;
            rw.copy(SIGNATURE_BYTES)?;
        }
    } else {
        rw.string(256)?;
        rw.i64()?; // Timestamp.
        rw.i64()?; // Salt.
        rw.optional(|rw| rw.copy(SIGNATURE_BYTES).map(drop))?;
    }
    rw.varint()?; // Offset.
    rw.copy(ACKNOWLEDGED_BYTES)?;
    if towards_newer {
        rw.w.u8(IGNORE_CHECKSUM);
    } else {
        rw.r.u8()?;
    }
    rw.finish()
}

/// Converts the text inside a packet: `component` copies one component from
/// the reader to the writer, `style` converts a style in place.
pub(super) struct Text<'t> {
    pub component: &'t mut dyn FnMut(&mut Rewriter<'_>) -> io::Result<()>,
    pub style: &'t mut dyn FnMut(&mut Nbt) -> io::Result<()>,
}

/// Run `body` with the text codec converting text for `direction`.
fn with_text<T>(
    shared: &mut Shared,
    direction: Direction,
    body: impl FnOnce(&mut Text<'_>) -> io::Result<T>,
) -> io::Result<T> {
    let shared = std::cell::RefCell::new(shared);
    // INTEGRATION: the text codec converts components and styles (stubs
    // that fail today).
    let mut component =
        |rw: &mut Rewriter<'_>| text::transcode(&mut shared.borrow_mut(), direction, rw);
    let mut style = |tag: &mut Nbt| text::convert_nbt(&mut shared.borrow_mut(), direction, tag);
    body(&mut Text {
        component: &mut component,
        style: &mut style,
    })
}

/// `player_chat` with the 1.21.5 global index (`Some`, for a 1.21.5 client)
/// or without it (`None`), and its text converted. Everything is parsed:
/// the text after the signed body needs converting, and a layout mismatch
/// must not slip through.
fn player_chat(body: &[u8], global_index: Option<i32>, text: &mut Text<'_>) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    match global_index {
        Some(index) => drop(rw.w.varint(index)),
        None => drop(rw.r.varint()?),
    }
    rw.uuid()?; // Sender.
    rw.varint()?; // The sender's message index.
    rw.optional(|rw| rw.copy(SIGNATURE_BYTES).map(drop))?;
    rw.string(256)?;
    rw.i64()?; // Timestamp.
    rw.i64()?; // Salt.
    let last_seen = rw.count()?;
    if last_seen > LAST_SEEN_MAX {
        return Err(invalid("too many last seen messages"));
    }
    for _ in 0..last_seen {
        // A cached signature's ID + 1, or 0 and the signature itself.
        if rw.varint()? == 0 {
            rw.copy(SIGNATURE_BYTES)?;
        }
    }
    rw.optional(|rw| (text.component)(rw))?; // Unsigned content.
    match rw.varint()? {
        0 | 1 => {}
        2 => {
            for _ in 0..rw.count()? {
                rw.i64()?;
            }
        }
        _ => return Err(invalid("unknown chat filter mask")),
    }
    // The chat type: a registry ID + 1, or 0 and the type itself.
    if rw.varint()? == 0 {
        for _ in 0..2 {
            chat_decoration(&mut rw, text)?;
        }
    }
    (text.component)(&mut rw)?; // Sender name.
    rw.optional(|rw| (text.component)(rw))?; // Target name.
    rw.finish()
}

/// `ChatTypeDecoration`: a translation key, its parameters and a style.
fn chat_decoration(rw: &mut Rewriter<'_>, text: &mut Text<'_>) -> io::Result<()> {
    rw.str()?;
    for _ in 0..rw.count()? {
        if !(0..=2).contains(&rw.varint()?) {
            return Err(invalid("unknown chat decoration parameter"));
        }
    }
    let mut style =
        Nbt::read(&mut rw.r, false)?.ok_or_else(|| invalid("chat decoration without a style"))?;
    (text.style)(&mut style)?;
    style.write(&mut rw.w, false);
    Ok(())
}

/// The shield sounds a 1.21.4 client plays on entity events 29 (an attack
/// blocked) and 30 (a shield disabled), with their volumes.
fn shield_sound(event: i8) -> Option<(&'static str, f32)> {
    match event {
        29 => Some(("item.shield.block", 1.0)),
        30 => Some(("item.shield.break", 0.8)),
        _ => None,
    }
}

/// A 1.21.5 `sound_entity` playing `sound` for `entity` from the players'
/// source, as `LivingEntity.handleEntityEvent` did on 1.21.4 clients.
fn entity_sound(entity: i32, sound: &str, volume: f32, random: u64) -> Vec<u8> {
    const PLAYERS: i32 = 7;
    let pitch = 0.8 + (random >> 40) as f32 / (1u64 << 24) as f32 * 0.4;
    let mut writer = Writer::new();
    writer
        .varint(registry_id(NEWER, "sound_event", sound) + 1)
        .varint(PLAYERS)
        .varint(entity)
        .f32(volume)
        .f32(pitch)
        .i64(random as i64);
    writer.buf
}

/// xorshift64*: the sound pitch and seed vanilla draws at random.
fn next_random(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}
