//! `login` (join game) and `respawn` as 1.18.2 to 1.19.3 lay them out.
//!
//! 1.18.2 names the player's dimension type by its full element; 1.19 by its
//! key in the registry codec and adds the last death location. 1.19.3 turns
//! `respawn`'s keep-attributes flag into a mask that can also keep entity
//! data. Between those, the layouts are equal.
use crate::{
    nbt::{Compound, Nbt},
    wire::{Position, Reader, Writer, invalid},
};
use std::io;

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

/// Whether two compounds hold the same entries, in any order.
pub fn same_compound(a: &Compound, b: &Compound) -> bool {
    a.len() == b.len()
        && a.iter().all(|(key, value)| {
            b.get(key).is_some_and(|other| match (value, other) {
                (Nbt::Compound(value), Nbt::Compound(other)) => same_compound(value, other),
                _ => value == other,
            })
        })
}
