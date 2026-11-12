//! Minecraft 1.21.9/1.21.10 (protocol 773) and 1.21.11 (protocol 774).
//!
//! Both protocols have the same packets. The shared codecs convert what
//! changed inside shared structures (new data components and entity data
//! serializers, registries, the commands tree). What is left for the step,
//! from both servers' code and recorded sessions:
//!
//! - world border times: 1.21.11 counts the lerp time in ticks instead of
//!   milliseconds and the warning time in ticks instead of seconds (its
//!   server's default warning of 300 is 1.21.10's 15);
//! - `player_action` gained the spear's `STAB`, which 1.21.10 servers
//!   reject.
mod down;
mod up;

use super::Make;
use crate::wire::{Reader, Rewriter, invalid};
use std::io;

pub const OLDER: i32 = 773;
pub const NEWER: i32 = 774;

/// A 1.21.11 client on a 1.21.9 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.9 client on a 1.21.11 backend.
pub const DOWN: Option<Make> = down::STEP;

const MILLIS_PER_TICK: i64 = 50;
const TICKS_PER_SECOND: i32 = 20;

/// `ServerboundPlayerActionPacket.Action.STAB`, new in 1.21.11.
const STAB: i32 = 7;

/// How border times convert, from the sender's units to the receiver's.
#[derive(Clone, Copy)]
enum Border {
    /// 1.21.10 to 1.21.11: milliseconds and seconds to ticks.
    ToTicks,
    /// 1.21.11 to 1.21.10: ticks to milliseconds and seconds.
    FromTicks,
}

impl Border {
    fn lerp_time(self, time: i64) -> io::Result<i64> {
        if time < 0 {
            return Err(invalid("negative border lerp time"));
        }
        Ok(match self {
            Self::ToTicks => (time + MILLIS_PER_TICK / 2) / MILLIS_PER_TICK,
            Self::FromTicks => time.saturating_mul(MILLIS_PER_TICK),
        })
    }

    fn warning_time(self, time: i32) -> i32 {
        match self {
            Self::ToTicks => time.saturating_mul(TICKS_PER_SECOND),
            Self::FromTicks => {
                (time.saturating_add(TICKS_PER_SECOND / 2)).div_euclid(TICKS_PER_SECOND)
            }
        }
    }

    /// `initialize_border`: center, old and new size, lerp time, absolute
    /// maximum size, warning blocks, warning time.
    fn initialize(self, body: &[u8]) -> io::Result<Vec<u8>> {
        let mut rw = Rewriter::new(body);
        rw.copy(32)?; // Center x and z, old and new size.
        let lerp = rw.r.varlong()?;
        rw.w.varlong(self.lerp_time(lerp)?);
        rw.varint()?; // Absolute maximum size.
        rw.varint()?; // Warning blocks.
        let warning = rw.r.varint()?;
        rw.w.varint(self.warning_time(warning));
        rw.finish()
    }

    /// `set_border_lerp_size`: old size, new size, lerp time.
    fn lerp_size(self, body: &[u8]) -> io::Result<Vec<u8>> {
        let mut rw = Rewriter::new(body);
        rw.copy(16)?;
        let lerp = rw.r.varlong()?;
        rw.w.varlong(self.lerp_time(lerp)?);
        rw.finish()
    }

    /// `set_border_warning_delay`: the warning time alone.
    fn warning_delay(self, body: &[u8]) -> io::Result<Vec<u8>> {
        let mut rw = Rewriter::new(body);
        let warning = rw.r.varint()?;
        rw.w.varint(self.warning_time(warning));
        rw.finish()
    }
}

/// Whether a 1.21.11 `player_action` (action, position, face, sequence)
/// is a spear stab, which 1.21.10 has no action for.
fn is_stab(body: &[u8]) -> io::Result<bool> {
    let mut r = Reader::new(body);
    let action = r.varint()?;
    r.i64()?; // Position.
    r.u8()?; // Face.
    r.varint()?; // Sequence.
    r.finish()?;
    if !(0..=STAB).contains(&action) {
        return Err(invalid("unknown player action"));
    }
    Ok(action == STAB)
}

#[cfg(test)]
mod tests;
