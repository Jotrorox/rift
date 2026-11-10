//! Minecraft 1.21.7/1.21.8 (protocol 772) and 1.21.9/1.21.10 (protocol 773).
//!
//! The shared codecs convert what changed in shared structures (entity
//! velocity in `add_entity`, entity data serializers, particles, the
//! explosion packet, item components, registries). This step handles the
//! rest, learnt from both servers' packet classes:
//!
//! - `set_entity_motion` carried three shorts in 1/8000 blocks per tick and
//!   now carries a packed [`lp_vec3`] vector;
//! - `player_rotation` gained a "relative" flag after each angle;
//! - `set_default_spawn_position` became respawn data: the dimension, the
//!   position, the yaw and a new pitch;
//! - 1.21.9 servers may ask players to accept a code of conduct during
//!   configuration, which an older client cannot: the step accepts on its
//!   behalf;
//! - debug sample subscriptions (a sample type) became a set of debug
//!   subscriptions (registry IDs), and the new debug value packets have no
//!   older equivalent.
mod down;
pub(super) mod lp_vec3;
mod up;

use super::Make;
use crate::wire::{Reader, Writer, invalid};
use std::io;

pub const OLDER: i32 = 772;
pub const NEWER: i32 = 773;

/// A 1.21.9 client on a 1.21.7 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.7 client on a 1.21.9 backend.
pub const DOWN: Option<Make> = down::STEP;

/// Identifiers are strings of at most this many characters.
const IDENTIFIER: usize = 32767;

/// The dimension an older server's spawn position lies in when the step has
/// not seen a `login` yet.
const DEFAULT_DIMENSION: &str = "minecraft:overworld";

/// 1.21.8 clamps each motion component to this many blocks per tick before
/// scaling it into a short.
const MAX_SHORT_MOTION: f64 = 3.9;
const SHORT_MOTION_SCALE: f64 = 8000.0;

/// The only debug sample type 1.21.8 has (`RemoteDebugSampleType.TICK_TIME`),
/// and the 1.21.9 debug subscription that delivers the same samples.
const TICK_TIME_SAMPLE: i32 = 0;
const TICK_TIME_SUBSCRIPTION: &str = "dedicated_server_tick_time";

/// 1.21.8 servers stop sending samples ten seconds after a subscription and
/// 1.21.8 clients renew theirs every five; 1.21.9 subscriptions last until
/// changed. Clients end a tick 20 times a second, which keeps time.
const RENEW_AFTER_TICKS: u32 = 100;
const EXPIRE_AFTER_TICKS: u32 = 200;

/// 1.21.8 `set_entity_motion` (entity, three shorts) to 1.21.9 (entity,
/// packed vector).
fn motion_to_vector(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(body);
    let entity = r.varint()?;
    let mut motion = [0.0; 3];
    for component in &mut motion {
        *component = f64::from(r.i16()?) / SHORT_MOTION_SCALE;
    }
    r.finish()?;
    let mut w = Writer::with_capacity(body.len() + 4);
    w.varint(entity);
    lp_vec3::write(&mut w, motion);
    Ok(w.into_inner())
}

/// 1.21.9 `set_entity_motion` to 1.21.8, clamped as 1.21.8 servers clamp.
fn motion_to_shorts(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(body);
    let entity = r.varint()?;
    let motion = lp_vec3::read(&mut r)?;
    r.finish()?;
    let mut w = Writer::with_capacity(11);
    w.varint(entity);
    for component in motion {
        w.i16(short_motion(component));
    }
    Ok(w.into_inner())
}

fn short_motion(component: f64) -> i16 {
    // Java's d2i truncates towards zero; the clamp keeps it within a short.
    (component.clamp(-MAX_SHORT_MOTION, MAX_SHORT_MOTION) * SHORT_MOTION_SCALE) as i16
}

/// 1.21.8 `player_rotation` (yaw, pitch) to 1.21.9: both absolute.
fn rotation_to_flagged(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(body);
    let (yaw, pitch) = (r.f32()?, r.f32()?);
    r.finish()?;
    let mut w = Writer::with_capacity(10);
    w.f32(yaw).bool(false).f32(pitch).bool(false);
    Ok(w.into_inner())
}

/// A 1.21.9 `player_rotation`: each angle with whether it is relative to the
/// player's current one.
fn read_flagged_rotation(body: &[u8]) -> io::Result<((f32, bool), (f32, bool))> {
    let mut r = Reader::new(body);
    let yaw = (r.f32()?, r.bool()?);
    let pitch = (r.f32()?, r.bool()?);
    r.finish()?;
    Ok((yaw, pitch))
}

/// The dimension a `login` (`respawn` when `respawn`) puts the player in,
/// read in the layout both protocols share (1.21.5 to 1.21.10). Only the
/// part up to the dimension is read: the step does not own these packets.
fn spawn_dimension(body: &[u8], respawn: bool) -> io::Result<String> {
    let mut r = Reader::new(body);
    if !respawn {
        r.i32()?; // Entity ID.
        r.bool()?; // Hardcore.
        for _ in 0..r.count()? {
            r.string(IDENTIFIER)?; // Dimension names.
        }
        r.varint()?; // Max players.
        r.varint()?; // Chunk radius.
        r.varint()?; // Simulation distance.
        r.bool()?; // Reduced debug info.
        r.bool()?; // Show death screen.
        r.bool()?; // Limited crafting.
    }
    r.varint()?; // Dimension type.
    Ok(r.string(IDENTIFIER)?.to_owned())
}

/// 1.21.8 `set_default_spawn_position` (position, yaw) to 1.21.9 respawn
/// data (dimension, position, yaw, pitch).
fn spawn_to_respawn_data(body: &[u8], dimension: &str) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(body);
    let (position, yaw) = (r.i64()?, r.f32()?);
    r.finish()?;
    let mut w = Writer::with_capacity(body.len() + dimension.len() + 8);
    w.string(dimension).i64(position).f32(yaw).f32(0.0);
    Ok(w.into_inner())
}

/// 1.21.9 respawn data to 1.21.8 `set_default_spawn_position`. Older
/// clients only point compasses at the spawn of the world they are in.
fn respawn_data_to_spawn(body: &[u8]) -> io::Result<Vec<u8>> {
    let mut r = Reader::new(body);
    r.string(IDENTIFIER)?;
    let (position, yaw) = (r.i64()?, r.f32()?);
    r.f32()?; // Pitch.
    r.finish()?;
    let mut w = Writer::with_capacity(12);
    w.i64(position).f32(yaw);
    Ok(w.into_inner())
}

/// A 1.21.9 `debug_subscription_request` (registry IDs): whether it asks
/// for server tick times, the one subscription 1.21.8 can serve.
fn subscribes_to_tick_time(body: &[u8], tick_time: i32) -> io::Result<bool> {
    let mut r = Reader::new(body);
    let count = r.count()?;
    let mut found = false;
    for _ in 0..count {
        let id = r.varint()?;
        if id < 0 {
            return Err(invalid("invalid debug subscription"));
        }
        found |= id == tick_time;
    }
    r.finish()?;
    Ok(found)
}

/// Check that a 1.21.8 `debug_sample_subscription` names the one sample
/// type there is.
fn check_tick_time_sample(body: &[u8]) -> io::Result<()> {
    let mut r = Reader::new(body);
    let sample = r.varint()?;
    r.finish()?;
    if sample != TICK_TIME_SAMPLE {
        return Err(invalid("unknown debug sample type"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
