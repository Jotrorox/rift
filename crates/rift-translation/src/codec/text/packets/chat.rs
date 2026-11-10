//! Chat packets: the old `chat` packet (to 1.18.2), `system_chat` (1.19+),
//! and `player_chat`/`disguised_chat` for the pairs from 1.19.3 on. The
//! signed-chat redesign between 1.18.2 and 1.19.3 (the `chat` to
//! `system_chat`/`player_chat` split, message headers, chat previews) is
//! left to those steps; [`chat_type`] and the component helpers of
//! [`super::Cv`] are what they can reuse.
use super::{Cv, crosses, text_changes};
use crate::{
    nbt::Nbt,
    wire::{Rewriter, invalid},
};
use std::io;

/// 1.16 added the sender's UUID to the chat packet.
const CHAT_SENDER: i32 = 735;
/// 1.19 (759) sends the chat type, 1.19.1 (760) an overlay flag.
const SYSTEM_OVERLAY: i32 = 760;
/// 1.19.3 restructured player chat into what later versions refine.
const PLAYER_CHAT: i32 = 761;
/// 1.21 sends chat types as registry holders (inline definitions allowed).
const CHAT_TYPE_HOLDER: i32 = 767;
/// 1.21.5 added the global message index.
const GLOBAL_INDEX: i32 = 770;

/// The built-in 1.19 chat type IDs of system messages and the action bar.
const SYSTEM: i32 = 1;
const GAME_INFO: i32 = 2;

pub fn chat_needed(from: i32, to: i32) -> bool {
    text_changes(from, to) || crosses(from, to, CHAT_SENDER)
}

pub fn chat(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    cv.component(&mut rw)?;
    rw.i8()?;
    let sender = if cv.from >= CHAT_SENDER {
        rw.r.uuid()?
    } else {
        0
    };
    if cv.to >= CHAT_SENDER {
        rw.w.uuid(sender);
    }
    cv.emit("chat", rw.finish()?);
    Ok(())
}

pub fn system_chat_needed(from: i32, to: i32) -> bool {
    text_changes(from, to) || crosses(from, to, SYSTEM_OVERLAY)
}

pub fn system_chat(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    cv.component(&mut rw)?;
    if cv.from < SYSTEM_OVERLAY && cv.to < SYSTEM_OVERLAY {
        rw.varint()?;
        cv.emit("system_chat", rw.finish()?);
        return Ok(());
    }
    let overlay = if cv.from >= SYSTEM_OVERLAY {
        rw.r.bool()?
    } else {
        rw.r.varint()? == GAME_INFO
    };
    if cv.to >= SYSTEM_OVERLAY {
        rw.w.bool(overlay);
    } else {
        rw.w.varint(if overlay { GAME_INFO } else { SYSTEM });
    }
    cv.emit("system_chat", rw.finish()?);
    Ok(())
}

pub fn disguised_chat_needed(from: i32, to: i32) -> bool {
    text_changes(from, to) || crosses(from, to, CHAT_TYPE_HOLDER)
}

pub fn disguised_chat(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    cv.component(&mut rw)?;
    bound(cv, &mut rw)?;
    cv.emit("disguised_chat", rw.finish()?);
    Ok(())
}

pub fn player_chat_needed(from: i32, to: i32) -> bool {
    from >= PLAYER_CHAT
        && to >= PLAYER_CHAT
        && (text_changes(from, to)
            || crosses(from, to, CHAT_TYPE_HOLDER)
            || crosses(from, to, GLOBAL_INDEX))
}

pub fn player_chat(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut rw = Rewriter::new(data);
    let index = if cv.from >= GLOBAL_INDEX {
        Some(rw.r.varint()?)
    } else {
        None
    };
    if cv.to >= GLOBAL_INDEX {
        let state = cv.text();
        let index = index.unwrap_or_else(|| {
            let next = state.chat_index;
            state.chat_index = next.wrapping_add(1);
            next
        });
        rw.w.varint(index);
    }
    rw.uuid()?;
    rw.varint()?;
    if rw.bool()? {
        rw.copy(256)?;
    }
    // The signed body: plain text, timestamp, salt, last seen signatures.
    rw.string(256)?;
    rw.i64()?;
    rw.i64()?;
    let seen = rw.count()?;
    if seen > 20 {
        return Err(invalid("too many last seen messages"));
    }
    for _ in 0..seen {
        if rw.varint()? == 0 {
            rw.copy(256)?;
        }
    }
    cv.optional_component(&mut rw)?;
    if rw.varint()? == 2 {
        let words = rw.count()?;
        rw.copy(
            words
                .checked_mul(8)
                .ok_or_else(|| invalid("bad filter mask"))?,
        )?;
    }
    bound(cv, &mut rw)?;
    cv.emit("player_chat", rw.finish()?);
    Ok(())
}

/// A bound chat type: the type, the sender's name, the optional target.
fn bound(cv: &mut Cv<'_>, rw: &mut Rewriter<'_>) -> io::Result<()> {
    chat_type(cv, rw)?;
    cv.component(rw)?;
    cv.optional_component(rw)
}

/// Copy a chat type reference: a registry ID before 1.21, a holder (ID + 1,
/// or 0 and an inline definition) from then on. An inline type has no ID
/// an older client knows; it becomes the first chat type (`chat`).
// INTEGRATION: chat types are a synchronised registry. If the registries
// codec renumbers `chat_type` entries between the two sides of a pair, map
// the ID here through its per-connection state (for example a
// `registries::State::map(direction, "chat_type", id)`); today IDs pass
// through unchanged.
pub fn chat_type(cv: &mut Cv<'_>, rw: &mut Rewriter<'_>) -> io::Result<()> {
    let (id, inline) = if cv.from >= CHAT_TYPE_HOLDER {
        match rw.r.varint()? {
            0 => (None, Some(read_inline(cv, rw)?)),
            holder => (Some(holder - 1), None),
        }
    } else {
        (Some(rw.r.varint()?), None)
    };
    if cv.to >= CHAT_TYPE_HOLDER {
        match (id, inline) {
            (Some(id), _) => drop(rw.w.varint(id + 1)),
            (None, Some(inline)) => {
                rw.w.varint(0);
                write_inline(cv, rw, &inline);
            }
            (None, None) => unreachable!("a chat type is an ID or inline"),
        }
    } else {
        rw.w.varint(id.unwrap_or(0));
    }
    Ok(())
}

/// One chat type decoration: translation key, parameters, style.
struct Decoration {
    key: String,
    parameters: Vec<i32>,
    style: super::super::component::Style,
}

fn read_inline(cv: &mut Cv<'_>, rw: &mut Rewriter<'_>) -> io::Result<[Decoration; 2]> {
    let mut decoration = || -> io::Result<Decoration> {
        let key = rw.r.str()?.to_owned();
        let count = rw.r.count()?;
        let parameters = (0..count)
            .map(|_| rw.r.varint())
            .collect::<io::Result<_>>()?;
        let tag = Nbt::read(&mut rw.r, false)?.ok_or_else(|| invalid("missing chat style"))?;
        Ok(Decoration {
            key,
            parameters,
            style: super::super::style_from_nbt(cv.from, &tag)?,
        })
    };
    Ok([decoration()?, decoration()?])
}

fn write_inline(cv: &Cv<'_>, rw: &mut Rewriter<'_>, decorations: &[Decoration; 2]) {
    for decoration in decorations {
        rw.w.string(&decoration.key);
        rw.w.varint(decoration.parameters.len() as i32);
        for parameter in &decoration.parameters {
            rw.w.varint(*parameter);
        }
        super::super::style_to_nbt(cv.to, &decoration.style).write(&mut rw.w, false);
    }
}
