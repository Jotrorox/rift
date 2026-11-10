//! Minecraft 26.1 (protocol 775) and 26.2 (protocol 776).
//!
//! What changed, from the official servers' packet classes: `login_finished`
//! gained the server's session UUID (used for the client's telemetry), join
//! game the server's online-mode flag (a 26.2 client prepares its chat
//! signing key only on online-mode servers), and `spectator_action` took an
//! optional entity ID so a spectator's click on nothing reaches the server.
//! `set_player_team` (colour moved and became optional) and
//! `update_attributes` (a list limit) belong to the shared codecs. Beds lost
//! their block entity (see `beds`).
mod beds;
mod down;
mod up;

#[cfg(test)]
mod tests;

use super::Make;
use crate::wire::{Reader, Writer};
use std::io;

pub const OLDER: i32 = 775;
pub const NEWER: i32 = 776;

/// A 26.2 client on a 26.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 26.1 client on a 26.2 backend.
pub const DOWN: Option<Make> = down::STEP;

/// Copy a game profile (UUID, name and properties with optional
/// signatures), checking the limits vanilla reads it with.
fn copy_profile(r: &mut Reader<'_>, w: &mut Writer) -> io::Result<()> {
    w.uuid(r.uuid()?);
    w.string(r.string(16)?);
    let count = r.count()?;
    if count > 16 {
        return Err(crate::wire::invalid("too many profile properties"));
    }
    w.varint(count as i32);
    for _ in 0..count {
        w.string(r.string(64)?);
        w.string(r.string(32767)?);
        let signed = r.bool()?;
        w.bool(signed);
        if signed {
            w.string(r.string(1024)?);
        }
    }
    Ok(())
}
