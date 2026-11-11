//! Minecraft 1.20.2 (protocol 764) and 1.20.3/1.20.4 (protocol 765).
//!
//! What 1.20.3 changed that is not a shared codec's business:
//!
//! - Resource packs got UUIDs. A 1.20.3 server can push several packs and
//!   remove them again (`resource_pack_pop`); a 1.20.2 client holds one pack
//!   that each push replaces, and answers without naming the pack. The
//!   serverbound status gained four results (downloaded, invalid URL, failed
//!   reload, discarded).
//! - Removing a score became its own packet (`reset_score`); `set_score`
//!   lost its action and gained an optional display name and number format.
//! - The server tells the client when the level's chunks start arriving
//!   (game event 13); a 1.20.3 client keeps the "Loading terrain" screen up
//!   until it does. A 1.20.2 client closed that screen when the spawn
//!   position arrived instead.
//! - The tick rate is synchronised (`ticking_state`, `ticking_step`), the
//!   crafter's slots can be toggled (`container_slot_state_changed`) and
//!   jigsaw blocks gained selection and placement priorities.
//!
//! Text became NBT in every play and configuration packet; that, and the new
//! blocks, items and entities, belong to the shared codecs and the pair's
//! rules (`data/rules/764-765.json`).
mod down;
mod up;

#[cfg(test)]
pub(super) mod support;
#[cfg(test)]
mod tests;

use super::Make;
use crate::{
    Direction,
    codec::{self, Shared},
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

pub const OLDER: i32 = 764;
pub const NEWER: i32 = 765;

/// A 1.20.3 client on a 1.20.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.20.2 client on a 1.20.3 backend.
pub const DOWN: Option<Make> = down::STEP;

/// The longest resource pack URL vanilla accepts.
const MAX_URL: usize = 32767;
/// A resource pack hash: 40 hexadecimal digits, or empty.
const MAX_HASH: usize = 40;

/// The game event that starts the level loading on a 1.20.3 client.
const LEVEL_CHUNKS_LOAD_START: u8 = 13;

/// A stable pack ID for a URL, for a newer client that needs one per pack
/// while the older server only ever has the one it last pushed. Two FNV-1a
/// hashes of the URL, shaped as a name-based (version 3) UUID.
fn pack_uuid(url: &str) -> u128 {
    let hash = |seed: u64| {
        url.bytes().fold(seed, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    let value =
        u128::from(hash(0xcbf2_9ce4_8422_2325)) << 64 | u128::from(hash(0x6c62_272e_07bb_0142));
    value & !(0xf000 << 64 | 0xc000 << 48) | 0x3000 << 64 | 0x8000 << 48
}

/// A 1.20.3 resource pack status as a 1.20.2 server knows it, or `None`
/// for a status it has no equivalent of and does not wait for.
fn status_for_older(status: i32) -> io::Result<Option<i32>> {
    match status {
        // Loaded, declined, failed download, accepted.
        0..=3 => Ok(Some(status)),
        // Downloaded: the client goes on to load it and reports that.
        4 => Ok(None),
        // Invalid URL, failed reload, discarded: the pack did not load.
        5..=7 => Ok(Some(2)),
        _ => Err(invalid("unknown resource pack status")),
    }
}

fn status_for_newer(status: i32) -> io::Result<i32> {
    match status {
        0..=3 => Ok(status),
        _ => Err(invalid("unknown resource pack status")),
    }
}

/// Copy the rest of a `resource_pack_push` after its UUID (1.20.3) or from
/// its start (1.20.2): URL, hash, whether it is required, and the optional
/// prompt, a JSON text before 1.20.3 and NBT from then on.
fn push_rest(shared: &mut Shared, direction: Direction, rw: &mut Rewriter<'_>) -> io::Result<()> {
    rw.string(MAX_URL)?;
    rw.string(MAX_HASH)?;
    rw.bool()?;
    if rw.bool()? {
        // INTEGRATION: converts the prompt once the text codec is implemented.
        codec::text::transcode(shared, direction, rw)?;
    }
    Ok(())
}

/// A 1.20.2 `set_score`: the owner, the action (0 sets, 1 removes) and the
/// objective, then the value when setting.
enum OlderScore<'a> {
    Set {
        owner: &'a str,
        objective: &'a str,
        value: i32,
    },
    /// An empty objective removes the owner's scores in every objective.
    Reset { owner: &'a str, objective: &'a str },
}

impl<'a> OlderScore<'a> {
    fn read(data: &'a [u8]) -> io::Result<Self> {
        let mut reader = Reader::new(data);
        let owner = reader.str()?;
        let action = reader.varint()?;
        let objective = reader.str()?;
        let score = match action {
            0 => Self::Set {
                owner,
                objective,
                value: reader.varint()?,
            },
            1 => Self::Reset { owner, objective },
            _ => return Err(invalid("unknown set_score action")),
        };
        reader.finish()?;
        Ok(score)
    }

    fn write(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        match self {
            Self::Set {
                owner,
                objective,
                value,
            } => writer
                .string(owner)
                .varint(0)
                .string(objective)
                .varint(*value),
            Self::Reset { owner, objective } => writer.string(owner).varint(1).string(objective),
        };
        writer.buf
    }
}

/// Skip a 1.20.3 score's optional display name and number format: `0`
/// blank, `1` styled (a style compound), `2` fixed (a text component).
fn skip_score_format(reader: &mut Reader<'_>) -> io::Result<()> {
    if reader.bool()? {
        reader.nbt_bytes(false)?;
    }
    if reader.bool()? {
        match reader.varint()? {
            0 => {}
            1 | 2 => drop(reader.nbt_bytes(false)?),
            _ => return Err(invalid("unknown number format")),
        }
    }
    Ok(())
}
