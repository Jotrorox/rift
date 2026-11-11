//! Layouts of the play packets the 1.19.3 to 1.20 steps rewrite whole: the
//! player position packet that loses its dismount flag in 1.19.4, and (for
//! tests) join game parsed field by field.
use crate::wire::{Reader, Rewriter, Writer};
#[cfg(test)]
use crate::{
    nbt::{Compound, Nbt},
    wire::invalid,
};
use std::io;

#[cfg(test)]
/// Protocols from 1.20 on end join game and respawn with a portal cooldown.
fn has_portal_cooldown(protocol: i32) -> bool {
    protocol >= 763
}

#[cfg(test)]
/// A resource location such as a dimension name.
fn identifier(rw: &mut Rewriter<'_>) -> io::Result<()> {
    rw.string(32767).map(drop)
}

#[cfg(test)]
/// The optional death location both packets end with (before 1.20): a
/// dimension and a block position.
fn death_location(rw: &mut Rewriter<'_>) -> io::Result<()> {
    rw.optional(|rw| {
        identifier(rw)?;
        rw.i64().map(drop)
    })
}

#[cfg(test)]
/// Copy the portal cooldown from `source` to `target`, dropping it or
/// adding a zero (no cooldown) where only one side has it.
fn portal_cooldown(rw: &mut Rewriter<'_>, source: i32, target: i32) -> io::Result<()> {
    let cooldown = if has_portal_cooldown(source) {
        rw.r.varint()?
    } else {
        0
    };
    if has_portal_cooldown(target) {
        rw.w.varint(cooldown);
    }
    Ok(())
}

#[cfg(test)]
/// Rewrite a join game packet of `source` (1.19.3 to 1.20) for `target`,
/// handing its registry codec to `codec` to change. Every field is parsed.
pub fn rewrite_login(
    body: &[u8],
    source: i32,
    target: i32,
    codec: impl FnOnce(&mut Compound) -> io::Result<()>,
) -> io::Result<Vec<u8>> {
    let mut rw = Rewriter::new(body);
    rw.i32()?; // player
    rw.bool()?; // hardcore
    rw.u8()?; // game mode
    rw.i8()?; // previous game mode
    rw.list(identifier)?; // dimensions
    let mut registries = match Nbt::read(&mut rw.r, true)? {
        Some(Nbt::Compound(registries)) => registries,
        _ => return Err(invalid("join game without a registry codec")),
    };
    codec(&mut registries)?;
    Nbt::Compound(registries).write(&mut rw.w, true);
    identifier(&mut rw)?; // dimension type
    identifier(&mut rw)?; // dimension
    rw.i64()?; // hashed seed
    rw.varint()?; // max players
    rw.varint()?; // view distance
    rw.varint()?; // simulation distance
    rw.bool()?; // reduced debug info
    rw.bool()?; // respawn screen
    rw.bool()?; // debug world
    rw.bool()?; // flat world
    death_location(&mut rw)?;
    portal_cooldown(&mut rw, source, target)?;
    rw.finish()
}

/// A player position packet: position, rotation, relative flags and
/// teleport ID, then (before 1.19.4) whether to dismount first.
pub struct PlayerPosition {
    /// Everything up to and including the teleport ID, unchanged.
    pub head: Vec<u8>,
    pub dismount: bool,
}

pub fn read_player_position(body: &[u8], protocol: i32) -> io::Result<PlayerPosition> {
    let mut rw = Rewriter::new(body);
    rw.f64()?;
    rw.f64()?;
    rw.f64()?;
    rw.f32()?;
    rw.f32()?;
    rw.i8()?; // relative flags
    rw.varint()?; // teleport ID
    let dismount = if protocol < 762 { rw.r.bool()? } else { false };
    Ok(PlayerPosition {
        head: rw.finish()?,
        dismount,
    })
}

pub fn write_player_position(position: &PlayerPosition, protocol: i32) -> Vec<u8> {
    let mut writer = Writer::with_capacity(position.head.len() + 1);
    writer.bytes(&position.head);
    if protocol < 762 {
        writer.bool(position.dismount);
    }
    writer.into_inner()
}

/// A vehicle and its passengers, from a set passengers packet.
pub fn read_passengers(body: &[u8]) -> io::Result<(i32, Vec<i32>)> {
    let mut reader = Reader::new(body);
    let vehicle = reader.varint()?;
    let count = reader.count()?;
    let passengers = (0..count)
        .map(|_| reader.varint())
        .collect::<io::Result<_>>()?;
    reader.finish()?;
    Ok((vehicle, passengers))
}

pub fn write_passengers(vehicle: i32, passengers: &[i32]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(vehicle).varint(passengers.len() as i32);
    for passenger in passengers {
        writer.varint(*passenger);
    }
    writer.into_inner()
}

/// The entity IDs of a remove entities packet.
pub fn read_removed(body: &[u8]) -> io::Result<Vec<i32>> {
    let mut reader = Reader::new(body);
    let count = reader.count()?;
    let ids = (0..count)
        .map(|_| reader.varint())
        .collect::<io::Result<_>>()?;
    reader.finish()?;
    Ok(ids)
}

/// The feature flags of an update enabled features packet.
pub fn read_features(body: &[u8]) -> io::Result<Vec<&str>> {
    let mut reader = Reader::new(body);
    let count = reader.count()?;
    let features = (0..count)
        .map(|_| reader.string(32767))
        .collect::<io::Result<_>>()?;
    reader.finish()?;
    Ok(features)
}

pub fn write_features(features: &[&str]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint(features.len() as i32);
    for feature in features {
        writer.string(feature);
    }
    writer.into_inner()
}
