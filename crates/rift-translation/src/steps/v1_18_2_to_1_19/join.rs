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
    nbt::{Compound, Nbt},
    wire::{Position, Reader, Rewriter, Writer, invalid},
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
        (true, false) => read_death(from, &mut rw.r).map(drop),
        (false, true) => Ok(drop(rw.w.bool(false))),
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

/// The dimension type a `login` or `respawn` puts the player in.
#[derive(Clone, Debug, PartialEq)]
pub enum Dimension {
    /// 1.18.2: the dimension type element itself.
    Element(Compound),
    /// 1.19 and later: its key in the `minecraft:dimension_type` registry.
    Key(String),
}

/// Where the player last died: a dimension key and a block position.
pub type Death = (String, Position);

#[derive(Clone, Debug, PartialEq)]
pub struct Login {
    pub entity_id: i32,
    pub hardcore: bool,
    pub game_mode: u8,
    pub previous_game_mode: i8,
    pub worlds: Vec<String>,
    /// The registry codec: dimension types, biomes and (1.19+) chat types.
    pub codec: Compound,
    pub dimension: Dimension,
    pub world: String,
    pub seed: i64,
    pub max_players: i32,
    pub view_distance: i32,
    pub simulation_distance: i32,
    pub reduced_debug_info: bool,
    pub respawn_screen: bool,
    pub debug: bool,
    pub flat: bool,
    pub death: Option<Death>,
}

fn compound(reader: &mut Reader<'_>) -> io::Result<Compound> {
    match Nbt::read(reader, true)? {
        Some(Nbt::Compound(compound)) => Ok(compound),
        _ => Err(invalid("expected an NBT compound")),
    }
}

fn read_dimension(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Dimension> {
    Ok(if protocol >= 759 {
        Dimension::Key(reader.string(MAX_IDENTIFIER)?.to_owned())
    } else {
        Dimension::Element(compound(reader)?)
    })
}

fn write_dimension(protocol: i32, dimension: &Dimension, writer: &mut Writer) -> io::Result<()> {
    match (protocol >= 759, dimension) {
        (true, Dimension::Key(key)) => {
            writer.string(key);
        }
        (false, Dimension::Element(element)) => {
            Nbt::Compound(element.clone()).write(writer, true);
        }
        _ => return Err(invalid("dimension in the wrong form for the protocol")),
    }
    Ok(())
}

fn read_death(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Option<Death>> {
    if protocol < 759 || !reader.bool()? {
        return Ok(None);
    }
    let dimension = reader.string(MAX_IDENTIFIER)?.to_owned();
    Ok(Some((dimension, reader.position()?)))
}

fn write_death(protocol: i32, death: Option<&Death>, writer: &mut Writer) {
    if protocol < 759 {
        return;
    }
    writer.bool(death.is_some());
    if let Some((dimension, position)) = death {
        writer.string(dimension).position(*position);
    }
}

impl Login {
    pub fn read(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let entity_id = r.i32()?;
        let hardcore = r.bool()?;
        let game_mode = r.u8()?;
        let previous_game_mode = r.i8()?;
        let worlds = (0..r.count()?)
            .map(|_| Ok(r.string(MAX_IDENTIFIER)?.to_owned()))
            .collect::<io::Result<_>>()?;
        let codec = compound(&mut r)?;
        let login = Self {
            entity_id,
            hardcore,
            game_mode,
            previous_game_mode,
            worlds,
            codec,
            dimension: read_dimension(protocol, &mut r)?,
            world: r.string(MAX_IDENTIFIER)?.to_owned(),
            seed: r.i64()?,
            max_players: r.varint()?,
            view_distance: r.varint()?,
            simulation_distance: r.varint()?,
            reduced_debug_info: r.bool()?,
            respawn_screen: r.bool()?,
            debug: r.bool()?,
            flat: r.bool()?,
            death: read_death(protocol, &mut r)?,
        };
        r.finish()?;
        Ok(login)
    }

    pub fn write(&self, protocol: i32) -> io::Result<Vec<u8>> {
        let mut w = Writer::new();
        w.i32(self.entity_id)
            .bool(self.hardcore)
            .u8(self.game_mode)
            .i8(self.previous_game_mode)
            .varint(self.worlds.len() as i32);
        for world in &self.worlds {
            w.string(world);
        }
        Nbt::Compound(self.codec.clone()).write(&mut w, true);
        write_dimension(protocol, &self.dimension, &mut w)?;
        w.string(&self.world)
            .i64(self.seed)
            .varint(self.max_players)
            .varint(self.view_distance)
            .varint(self.simulation_distance)
            .bool(self.reduced_debug_info)
            .bool(self.respawn_screen)
            .bool(self.debug)
            .bool(self.flat);
        write_death(protocol, self.death.as_ref(), &mut w);
        Ok(w.buf)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Respawn {
    pub dimension: Dimension,
    pub world: String,
    pub seed: i64,
    pub game_mode: u8,
    pub previous_game_mode: i8,
    pub debug: bool,
    pub flat: bool,
    pub keep_attributes: bool,
    /// Before 1.19.3 clients always keep the player's entity data.
    pub keep_entity_data: bool,
    pub death: Option<Death>,
}

impl Respawn {
    pub fn read(protocol: i32, data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let dimension = read_dimension(protocol, &mut r)?;
        let world = r.string(MAX_IDENTIFIER)?.to_owned();
        let seed = r.i64()?;
        let game_mode = r.u8()?;
        let previous_game_mode = r.i8()?;
        let debug = r.bool()?;
        let flat = r.bool()?;
        let (keep_attributes, keep_entity_data) = if protocol >= 761 {
            let mask = r.u8()?;
            (mask & 1 != 0, mask & 2 != 0)
        } else {
            (r.bool()?, true)
        };
        let respawn = Self {
            dimension,
            world,
            seed,
            game_mode,
            previous_game_mode,
            debug,
            flat,
            keep_attributes,
            keep_entity_data,
            death: read_death(protocol, &mut r)?,
        };
        r.finish()?;
        Ok(respawn)
    }

    pub fn write(&self, protocol: i32) -> io::Result<Vec<u8>> {
        let mut w = Writer::new();
        write_dimension(protocol, &self.dimension, &mut w)?;
        w.string(&self.world)
            .i64(self.seed)
            .u8(self.game_mode)
            .i8(self.previous_game_mode)
            .bool(self.debug)
            .bool(self.flat);
        if protocol >= 761 {
            w.u8(u8::from(self.keep_attributes) | u8::from(self.keep_entity_data) << 1);
        } else {
            w.bool(self.keep_attributes);
        }
        write_death(protocol, self.death.as_ref(), &mut w);
        Ok(w.buf)
    }
}
