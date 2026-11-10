//! Block entity data 1.13 moved into block states, and how it completes a
//! flattened state.
use crate::{
    data::{self, Block},
    nbt::{Compound, Nbt},
    wire::Position,
};
use std::{io, sync::OnceLock};

/// Dye colours in 1.13 name order and 1.12 colour order (beds' `color`,
/// 15 minus banners' `Base`).
const COLOURS: [&str; 16] = [
    "white",
    "orange",
    "magenta",
    "light_blue",
    "yellow",
    "lime",
    "pink",
    "gray",
    "light_gray",
    "cyan",
    "purple",
    "blue",
    "brown",
    "green",
    "red",
    "black",
];

/// By 1.12 `SkullType`: floor block, wall block.
const SKULLS: [(&str, &str); 6] = [
    ("skeleton_skull", "skeleton_wall_skull"),
    ("wither_skeleton_skull", "wither_skeleton_wall_skull"),
    ("zombie_head", "zombie_wall_head"),
    ("player_head", "player_wall_head"),
    ("creeper_head", "creeper_wall_head"),
    ("dragon_head", "dragon_wall_head"),
];

/// 1.12 note block instruments by number, as 1.13 names them.
const INSTRUMENTS: [&str; 10] = [
    "harp",
    "basedrum",
    "snare",
    "hat",
    "bass",
    "flute",
    "bell",
    "guitar",
    "chime",
    "xylophone",
];

/// A flower pot's 1.12 plant (item name and data value) and its potted
/// 1.13 block.
const POTTED: &[(&str, i32, &str)] = &[
    ("sapling", 0, "potted_oak_sapling"),
    ("sapling", 1, "potted_spruce_sapling"),
    ("sapling", 2, "potted_birch_sapling"),
    ("sapling", 3, "potted_jungle_sapling"),
    ("sapling", 4, "potted_acacia_sapling"),
    ("sapling", 5, "potted_dark_oak_sapling"),
    ("red_flower", 0, "potted_poppy"),
    ("red_flower", 1, "potted_blue_orchid"),
    ("red_flower", 2, "potted_allium"),
    ("red_flower", 3, "potted_azure_bluet"),
    ("red_flower", 4, "potted_red_tulip"),
    ("red_flower", 5, "potted_orange_tulip"),
    ("red_flower", 6, "potted_white_tulip"),
    ("red_flower", 7, "potted_pink_tulip"),
    ("red_flower", 8, "potted_oxeye_daisy"),
    ("yellow_flower", 0, "potted_dandelion"),
    ("red_mushroom", 0, "potted_red_mushroom"),
    ("brown_mushroom", 0, "potted_brown_mushroom"),
    ("cactus", 0, "potted_cactus"),
    ("deadbush", 0, "potted_dead_bush"),
    ("tallgrass", 2, "potted_fern"),
];

/// What a block entity says about its block's 1.13 state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Special {
    /// 1.13 colour index.
    Bed(u8),
    Banner(u8),
    /// `SkullType` and floor rotation.
    Skull(u8, u8),
    /// The potted block (`flower_pot` when empty).
    FlowerPot(&'static str),
    Note {
        note: Option<u8>,
        instrument: Option<u8>,
        powered: Option<bool>,
    },
}

/// The kinds of 1.13 blocks whose state depends on block entity data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    None,
    Bed,
    Banner,
    WallBanner,
    Skull,
    WallSkull,
    FlowerPot,
    NoteBlock,
}

fn kind_of(name: &str) -> Kind {
    if name == "note_block" {
        Kind::NoteBlock
    } else if name == "flower_pot" || name.starts_with("potted_") {
        Kind::FlowerPot
    } else if name.ends_with("_wall_banner") {
        Kind::WallBanner
    } else if name.ends_with("_banner") {
        Kind::Banner
    } else if name.ends_with("_bed") {
        Kind::Bed
    } else if SKULLS.iter().any(|(_, wall)| *wall == name) {
        Kind::WallSkull
    } else if SKULLS.iter().any(|(floor, _)| *floor == name) {
        Kind::Skull
    } else {
        Kind::None
    }
}

fn states() -> &'static data::BlockStates {
    &data::version(super::super::super::NEWER).block_states
}

/// The kind of every 1.13 block, by block index.
fn kinds() -> &'static [Kind] {
    static KINDS: OnceLock<Vec<Kind>> = OnceLock::new();
    KINDS.get_or_init(|| {
        states()
            .blocks
            .iter()
            .map(|block| kind_of(&block.name))
            .collect()
    })
}

fn kind(state: i32) -> Kind {
    let states = states();
    let Some((block, _)) = states.locate(state) else {
        return Kind::None;
    };
    let index = states
        .blocks
        .partition_point(|known| known.first_state < block.first_state);
    kinds().get(index).copied().unwrap_or(Kind::None)
}

/// Whether a 1.13 state is one whose block kept data in a 1.12 block entity.
pub fn has_block_entity(state: i32) -> bool {
    kind(state) != Kind::None
}

/// The state of `target` with the property values of `state` it shares,
/// then `overrides`.
fn restyle(state: i32, target: &str, overrides: &[(&str, &str)]) -> Option<i32> {
    let states = states();
    let (block, offset) = states.locate(state)?;
    let target: &Block = states.block(target)?;
    let mut values: Vec<(&str, &str)> = block
        .values(offset)
        .into_iter()
        .filter(|(name, _)| !overrides.iter().any(|(overridden, _)| overridden == name))
        .collect();
    values.extend_from_slice(overrides);
    // Values the target lacks fall back to its default: offset() looks up
    // only the target's own properties.
    let values: Vec<(&str, &str)> = values
        .into_iter()
        .filter(|(name, value)| {
            target
                .properties
                .iter()
                .any(|(known, values)| &**known == *name && values.iter().any(|v| &**v == *value))
        })
        .collect();
    Some(target.first_state + target.offset(&values)?)
}

const ROTATIONS: [&str; 16] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15",
];
const NOTES: [&str; 25] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
    "17", "18", "19", "20", "21", "22", "23", "24",
];

impl Special {
    /// The completed state for a flattened `state`, or `None` when this data
    /// is not for that kind of block.
    pub fn apply(self, state: i32) -> Option<i32> {
        let kind = kind(state);
        match (self, kind) {
            (Self::Bed(colour), Kind::Bed) => {
                restyle(state, &format!("{}_bed", COLOURS[usize::from(colour)]), &[])
            }
            (Self::Banner(colour), Kind::Banner) => restyle(
                state,
                &format!("{}_banner", COLOURS[usize::from(colour)]),
                &[],
            ),
            (Self::Banner(colour), Kind::WallBanner) => restyle(
                state,
                &format!("{}_wall_banner", COLOURS[usize::from(colour)]),
                &[],
            ),
            (Self::Skull(skull, rotation), Kind::Skull) => restyle(
                state,
                SKULLS[usize::from(skull)].0,
                &[("rotation", ROTATIONS[usize::from(rotation)])],
            ),
            (Self::Skull(skull, _), Kind::WallSkull) => {
                restyle(state, SKULLS[usize::from(skull)].1, &[])
            }
            (Self::FlowerPot(potted), Kind::FlowerPot) => restyle(state, potted, &[]),
            (
                Self::Note {
                    note,
                    instrument,
                    powered,
                },
                Kind::NoteBlock,
            ) => {
                let mut overrides = Vec::with_capacity(3);
                if let Some(note) = note {
                    overrides.push(("note", NOTES[usize::from(note)]));
                }
                if let Some(instrument) = instrument {
                    overrides.push(("instrument", INSTRUMENTS[usize::from(instrument)]));
                }
                if let Some(powered) = powered {
                    overrides.push(("powered", if powered { "true" } else { "false" }));
                }
                restyle(state, "note_block", &overrides)
            }
            _ => None,
        }
    }

    /// Newer note block data over older: what a note block event leaves out
    /// stays as the block entity said.
    pub fn merge(self, newer: Self) -> Self {
        match (self, newer) {
            (
                Self::Note {
                    note,
                    instrument,
                    powered,
                },
                Self::Note {
                    note: new_note,
                    instrument: new_instrument,
                    powered: new_powered,
                },
            ) => Self::Note {
                note: new_note.or(note),
                instrument: new_instrument.or(instrument),
                powered: new_powered.or(powered),
            },
            _ => newer,
        }
    }
}

/// A number tag of any width, as 1.12 reads it leniently.
fn number(tag: &Compound, key: &str) -> Option<i64> {
    tag.get(key)?.as_i64()
}

/// The position and data of a 1.12 block entity tag (named root), when
/// it is one whose data 1.13 keeps in the block state.
pub fn from_block_entity(bytes: &[u8]) -> io::Result<Option<(Position, Special)>> {
    let Some(Nbt::Compound(tag)) = Nbt::from_bytes(bytes, true)? else {
        return Ok(None);
    };
    let (Some(x), Some(y), Some(z)) = (number(&tag, "x"), number(&tag, "y"), number(&tag, "z"))
    else {
        return Ok(None);
    };
    let position = Position {
        x: x as i32,
        y: y as i32,
        z: z as i32,
    };
    let id = tag.str("id").map(data::strip).unwrap_or("");
    let special = match id {
        "bed" => {
            let colour = number(&tag, "color").unwrap_or(14);
            Special::Bed((colour & 15) as u8)
        }
        "banner" => {
            // Dye damage: 0 is black, 15 white.
            let base = number(&tag, "Base").unwrap_or(0);
            Special::Banner(15 - (base & 15) as u8)
        }
        "skull" => {
            let skull = number(&tag, "SkullType").unwrap_or(0);
            let skull = if (0..6).contains(&skull) { skull } else { 0 };
            let rotation = number(&tag, "Rot").unwrap_or(0) & 15;
            Special::Skull(skull as u8, rotation as u8)
        }
        "flower_pot" => Special::FlowerPot(potted(&tag)),
        "noteblock" => Special::Note {
            note: Some((number(&tag, "note").unwrap_or(0).clamp(0, 24)) as u8),
            instrument: None,
            powered: Some(number(&tag, "powered").unwrap_or(0) != 0),
        },
        _ => return Ok(None),
    };
    Ok(Some((position, special)))
}

/// The potted block for a 1.12 flower pot tag: `Item` names the plant (a
/// numeric ID in old worlds), `Data` its variant.
fn potted(tag: &Compound) -> &'static str {
    let item = match tag.get("Item") {
        Some(Nbt::String(name)) => data::strip(name).to_owned(),
        Some(other) => other
            .as_i64()
            .and_then(|id| {
                data::version(super::super::super::OLDER)
                    .registry("item")?
                    .name(i32::try_from(id).ok()?)
                    .map(str::to_owned)
            })
            .unwrap_or_default(),
        None => String::new(),
    };
    let variant = number(tag, "Data").unwrap_or(0);
    POTTED
        .iter()
        .find(|(plant, data, _)| *plant == item && i64::from(*data) == variant)
        .map_or("flower_pot", |(_, _, potted)| potted)
}

/// Note block data from a 1.12 block event: the note block (1.12 block 25)
/// plays with its instrument as action and its note as parameter.
pub fn from_block_event(block: i32, action: u8, parameter: u8) -> Option<Special> {
    let note_block = data::version(super::super::super::OLDER)
        .registry("block")?
        .id("noteblock")?;
    (block == note_block && usize::from(action) < INSTRUMENTS.len() && parameter <= 24).then_some(
        Special::Note {
            note: Some(parameter),
            instrument: Some(action),
            powered: None,
        },
    )
}
