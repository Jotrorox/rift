//! `login` (join game) and `respawn` as 1.18.2 to 1.20.1 lay them out.
//!
//! 1.18.2 names the player's dimension type by its full element; 1.19 by its
//! key in the registry codec and adds the last death location. 1.19.3 turns
//! `respawn`'s keep-attributes flag into a mask that can also keep entity
//! data, and 1.20 ends both packets with a portal cooldown. The registry
//! codec and the dimension at their start are the registries codec's to
//! convert ([`registries::login`]); [`convert_login`] and
//! [`convert_respawn`] do the rest for every step from 1.18.2 to 1.20.1.
use crate::{
    Direction,
    codec::{Shared, registries},
    wire::Rewriter,
};
use std::io;

/// 1.19 adds the last death location to both packets.
const DEATH_LOCATION: i32 = 759;
/// 1.19.3 keeps entity data on request instead of always.
const KEEP_MASK: i32 = 761;
/// 1.20 ends both packets with the portal cooldown.
const PORTAL_COOLDOWN: i32 = 763;

/// Copy the optional death location, dropping it for a receiver before
/// 1.19 and adding none for one from 1.19 on.
fn death_location(rw: &mut Rewriter<'_>, from: i32, to: i32) -> io::Result<()> {
    match (from >= DEATH_LOCATION, to >= DEATH_LOCATION) {
        (true, true) => rw.optional(|rw| {
            rw.string(MAX_IDENTIFIER)?;
            rw.i64().map(drop)
        }),
        (true, false) => {
            if rw.r.bool()? {
                rw.r.string(MAX_IDENTIFIER)?;
                rw.r.i64()?;
            }
            Ok(())
        }
        (false, true) => {
            rw.w.bool(false);
            Ok(())
        }
        (false, false) => Ok(()),
    }
}

/// Copy the portal cooldown, or add none (0) for a receiver from 1.20.
fn portal_cooldown(rw: &mut Rewriter<'_>, from: i32, to: i32) -> io::Result<()> {
    let cooldown = if from >= PORTAL_COOLDOWN {
        rw.r.varint()?
    } else {
        0
    };
    if to >= PORTAL_COOLDOWN {
        rw.w.varint(cooldown);
    }
    Ok(())
}

/// Convert a `login` between two adjacent protocols of 1.18.2 to 1.20.1.
pub fn convert_login(
    shared: &mut Shared,
    direction: Direction,
    data: &[u8],
) -> io::Result<Vec<u8>> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    let mut rw = Rewriter::new(data);
    registries::login::transcode_login(shared, direction, &mut rw)?;
    rw.i64()?; // Hashed seed.
    rw.varint()?; // Maximum players.
    rw.varint()?; // View distance.
    rw.varint()?; // Simulation distance.
    for _ in 0..4 {
        rw.bool()?; // Reduced debug info, respawn screen, debug and flat world.
    }
    death_location(&mut rw, from, to)?;
    portal_cooldown(&mut rw, from, to)?;
    rw.finish()
}

/// Convert a `respawn` between two adjacent protocols of 1.18.2 to 1.20.1.
/// Clients before 1.19.3 always keep the player's entity data, so a newer
/// one is asked to as well.
pub fn convert_respawn(
    shared: &mut Shared,
    direction: Direction,
    data: &[u8],
) -> io::Result<Vec<u8>> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    let mut rw = Rewriter::new(data);
    registries::login::transcode_respawn(shared, direction, &mut rw)?;
    rw.i64()?; // Hashed seed.
    rw.u8()?; // Game mode.
    rw.i8()?; // Previous game mode.
    rw.bool()?; // Debug world.
    rw.bool()?; // Flat world.
    let keep_attributes = if from >= KEEP_MASK {
        let mask = rw.r.u8()?;
        if to >= KEEP_MASK {
            rw.w.u8(mask);
        }
        mask & 1 != 0
    } else {
        rw.r.bool()?
    };
    if to < KEEP_MASK {
        rw.w.bool(keep_attributes);
    } else if from < KEEP_MASK {
        rw.w.u8(u8::from(keep_attributes) | 2);
    }
    death_location(&mut rw, from, to)?;
    portal_cooldown(&mut rw, from, to)?;
    rw.finish()
}

const MAX_IDENTIFIER: usize = 32767;
