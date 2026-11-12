//! Blocks whose look moved between a block entity and the block state at
//! the 1.13 flattening, converted inside a column where both are at hand.
//!
//! - Beds: a 1.12 bed's colour is its block entity's `color`; 1.13 has one
//!   block per colour.
//! - Banners: 1.12 keeps the base colour (`Base`, with dye colours counted
//!   backwards, as pattern colours are) in the block entity; 1.13 has one
//!   block per colour.
//! - Skulls: 1.12 keeps the kind (`SkullType`) and the standing rotation
//!   (`Rot`) in the block entity; 1.13 has a block per kind, standing with
//!   a rotation or on a wall with a facing.
//! - Flower pots: 1.12 keeps the plant (`Item`, `Data`) in the block entity;
//!   1.13 has a block per potted plant and no block entity.
//!
//! Going up the column's states have already been mapped (the flattening
//! mapping gives each block its default colour or kind); the block entities
//! then pick the right block. Going down the block entities 1.12 needs are
//! written from the 1.13 blocks before the states are mapped.
//!
//! Not converted: a 1.12 note block's pitch is not in the chunk packet, so
//! 1.13 note blocks get their default note; and block entity packets that
//! arrive outside a column (a bed placed later) cannot change the block,
//! which the step tracks if it needs to (see the step docs).
use super::{
    container::Palette,
    convert::Scratch,
    model::{BlockEntity, Chunk},
};
use crate::{
    data::{self, BlockStates},
    nbt::{Compound, Nbt},
};
use std::io;

const DYES: [&str; 16] = [
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

/// 1.12 `SkullType` by index, as 1.13 standing and wall block names.
const SKULLS: [(&str, &str); 6] = [
    ("skeleton_skull", "skeleton_wall_skull"),
    ("wither_skeleton_skull", "wither_skeleton_wall_skull"),
    ("zombie_head", "zombie_wall_head"),
    ("player_head", "player_wall_head"),
    ("creeper_head", "creeper_wall_head"),
    ("dragon_head", "dragon_wall_head"),
];

/// 1.12 flower pot contents (item, data) as 1.13 potted blocks.
const POTS: &[(&str, i32, &str)] = &[
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
    ("sapling", 0, "potted_oak_sapling"),
    ("sapling", 1, "potted_spruce_sapling"),
    ("sapling", 2, "potted_birch_sapling"),
    ("sapling", 3, "potted_jungle_sapling"),
    ("sapling", 4, "potted_acacia_sapling"),
    ("sapling", 5, "potted_dark_oak_sapling"),
    ("red_mushroom", 0, "potted_red_mushroom"),
    ("brown_mushroom", 0, "potted_brown_mushroom"),
    ("cactus", 0, "potted_cactus"),
    ("deadbush", 0, "potted_dead_bush"),
    ("tallgrass", 2, "potted_fern"),
];

/// The same state with another block name, keeping the properties both
/// blocks have.
fn rename(states: &BlockStates, state: i32, name: &str) -> Option<i32> {
    let (block, offset) = states.locate(state)?;
    let values = block.values(offset);
    let target = states.block(name)?;
    let wanted: Vec<(&str, &str)> = values
        .into_iter()
        .filter(|(property, _)| {
            target
                .properties
                .iter()
                .any(|(known, _)| &**known == *property)
        })
        .collect();
    Some(target.first_state + target.offset(&wanted)?)
}

fn index(chunk: &Chunk, x: i32, y: i32, z: i32) -> Option<(usize, usize)> {
    let section = usize::try_from(y.div_euclid(16) - chunk.min_section).ok()?;
    let entry = ((y.rem_euclid(16) * 16 + z.rem_euclid(16)) * 16 + x.rem_euclid(16)) as usize;
    (section < chunk.sections.len()).then_some((section, entry))
}

/// Set one block, unpacking only the section it is in.
fn set(
    chunk: &mut Chunk,
    target: i32,
    (section, entry): (usize, usize),
    state: i32,
    scratch: &mut Scratch,
) -> io::Result<()> {
    let Some(section) = chunk.sections[section].as_mut() else {
        return Ok(());
    };
    section
        .blocks
        .values(&mut scratch.values, &mut scratch.raw)?;
    scratch.values[entry] = state;
    let format = super::container::Format::blocks(target);
    section.blocks =
        super::container::Container::from_values(&scratch.values, &format, &mut scratch.raw);
    Ok(())
}

fn get(
    chunk: &Chunk,
    (section, entry): (usize, usize),
    scratch: &mut Scratch,
) -> io::Result<Option<i32>> {
    match &chunk.sections[section] {
        Some(section) => section.blocks.get(entry, &mut scratch.raw).map(Some),
        None => Ok(None),
    }
}

/// Whether 1.13 state `id` is one whose look 1.12 keeps in a block entity.
pub fn relevant(states: &BlockStates, id: i32) -> bool {
    states.locate(id).is_some_and(|(block, _)| {
        let name = &*block.name;
        name.ends_with("_bed")
            || name.ends_with("_banner")
            || name.ends_with("_skull")
            || name.ends_with("_head")
            || name == "flower_pot"
            || name.starts_with("potted_")
    })
}

/// What a 1.12 block entity (`id` without namespace, its tag) makes of the
/// 1.13 block `state` it sits on: the state it should be, and whether 1.13
/// keeps the block entity. Removes the fields 1.13 no longer reads.
pub fn fix_up(
    states: &BlockStates,
    state: i32,
    id: &str,
    tag: &mut Compound,
) -> (Option<i32>, bool) {
    let name = states
        .locate(state)
        .map(|(block, _)| block.name.to_string())
        .unwrap_or_default();
    match id {
        "bed" => {
            let color = DYES
                .get(tag.int("color").unwrap_or(14) as usize)
                .copied()
                .unwrap_or("red");
            tag.remove("color");
            let new = name
                .ends_with("_bed")
                .then(|| rename(states, state, &format!("{color}_bed")))
                .flatten();
            (new, true)
        }
        "banner" => {
            let base = tag
                .remove("Base")
                .and_then(|base| base.as_i64())
                .unwrap_or(0);
            let color = DYES[(15 - base.clamp(0, 15)) as usize];
            invert_patterns(tag);
            let new = if name.ends_with("_wall_banner") {
                rename(states, state, &format!("{color}_wall_banner"))
            } else if name.ends_with("_banner") {
                rename(states, state, &format!("{color}_banner"))
            } else {
                None
            };
            (new, true)
        }
        "skull" => {
            let kind = tag
                .remove("SkullType")
                .and_then(|kind| kind.as_i64())
                .unwrap_or(0);
            let rotation = tag.remove("Rot").and_then(|rot| rot.as_i64()).unwrap_or(0);
            let (standing, wall) = SKULLS[kind.clamp(0, 5) as usize];
            let new = if name.contains("_wall_") {
                rename(states, state, wall)
            } else if name.ends_with("_skull") || name.ends_with("_head") {
                states.block(standing).and_then(|block| {
                    let rotation = (rotation & 15).to_string();
                    Some(block.first_state + block.offset(&[("rotation", rotation.as_str())])?)
                })
            } else {
                None
            };
            (new, true)
        }
        "flower_pot" => {
            let item = data::strip(tag.str("Item").unwrap_or("")).to_owned();
            let data = tag.int("Data").unwrap_or(0);
            let new = POTS
                .iter()
                .find(|(known, value, _)| *known == item && *value == data)
                .and_then(|(.., potted)| states.block(potted))
                .map(|block| block.default_state());
            (new, false)
        }
        "noteblock" => (None, false),
        _ => (None, true),
    }
}

fn invert_patterns(tag: &mut Compound) {
    if let Some(Nbt::List(patterns)) = tag.get_mut("Patterns") {
        for pattern in patterns.iter_mut() {
            if let Some(pattern) = pattern.as_compound_mut()
                && let Some(color) = pattern.int("Color")
            {
                pattern.insert("Color", 15 - color.clamp(0, 15));
            }
        }
    }
}

/// The block entity 1.12 needs for 1.13 block `state`: its ID and the
/// fields that carry what the 1.13 block name says.
pub fn fields_down(states: &BlockStates, state: i32) -> Option<(&'static str, Compound)> {
    let (block, offset) = states.locate(state)?;
    let name = &*block.name;
    let dye = |name: &str| {
        DYES.iter()
            .position(|dye| name.starts_with(&format!("{dye}_")))
            .map(|index| index as i32)
    };
    let mut fields = Compound::new();
    if name.ends_with("_bed") {
        fields.insert("color", dye(name).unwrap_or(14));
        Some(("bed", fields))
    } else if name.ends_with("_banner") {
        fields.insert("Base", 15 - dye(name).unwrap_or(0));
        Some(("banner", fields))
    } else if name.ends_with("_skull") || name.ends_with("_head") {
        let kind = SKULLS
            .iter()
            .position(|(standing, wall)| *standing == name || *wall == name)?;
        let rotation = block
            .values(offset)
            .iter()
            .find(|(property, _)| *property == "rotation")
            .and_then(|(_, value)| value.parse::<i8>().ok())
            .unwrap_or(0);
        fields.insert("SkullType", kind as i8);
        fields.insert("Rot", rotation);
        Some(("skull", fields))
    } else if name.starts_with("potted_") || name == "flower_pot" {
        let (item, data) = POTS
            .iter()
            .find(|(.., potted)| *potted == name)
            .map_or(("air", 0), |(item, data, _)| (*item, *data));
        fields.insert("Item", format!("minecraft:{item}"));
        fields.insert("Data", data);
        Some(("flower_pot", fields))
    } else {
        None
    }
}

/// After a 1.12 column's states were mapped to `target` (1.13): give beds,
/// banners, skulls and flower pots the block their block entity describes,
/// and drop the block entities 1.13 does not have.
pub fn up(chunk: &mut Chunk, target: i32, scratch: &mut Scratch) -> io::Result<()> {
    let states = &data::version(target).block_states;
    let entities = std::mem::take(&mut chunk.block_entities);
    let mut kept = Vec::with_capacity(entities.len());
    for mut entity in entities {
        let Some(Nbt::Compound(tag)) = &mut entity.nbt else {
            kept.push(entity);
            continue;
        };
        let id = data::strip(tag.str("id").unwrap_or("")).to_owned();
        let Some(at) = index(chunk, entity.x, entity.y, entity.z) else {
            continue;
        };
        let Some(state) = get(chunk, at, scratch)? else {
            continue;
        };
        let (new, keep) = fix_up(states, state, &id, tag);
        if let Some(new) = new {
            set(chunk, target, at, new, scratch)?;
        }
        if keep {
            kept.push(entity);
        }
    }
    chunk.block_entities = kept;
    Ok(())
}

/// Every block of a column `relevant` says 1.12 keeps a block entity for:
/// position and state.
pub fn find(
    chunk: &Chunk,
    states: &BlockStates,
    scratch: &mut Scratch,
) -> io::Result<Vec<(i32, i32, i32, i32)>> {
    let mut found = Vec::new();
    for (index, section) in chunk.sections.iter().enumerate() {
        let Some(section) = section else { continue };
        let any = match &section.blocks.palette {
            Palette::Single(value) => relevant(states, *value),
            Palette::Indirect(palette) => palette.iter().any(|value| relevant(states, *value)),
            Palette::Direct => true,
        };
        if !any {
            continue;
        }
        section
            .blocks
            .values(&mut scratch.values, &mut scratch.raw)?;
        for (entry, state) in scratch.values.iter().enumerate() {
            if relevant(states, *state) {
                let y = (chunk.min_section + index as i32) * 16 + (entry >> 8) as i32;
                let x = chunk.x.wrapping_mul(16) + (entry & 15) as i32;
                let z = chunk.z.wrapping_mul(16) + (entry >> 4 & 15) as i32;
                found.push((x, y, z, *state));
            }
        }
    }
    Ok(found)
}

fn tag_for<'a>(chunk: &'a mut Chunk, x: i32, y: i32, z: i32, id: &str) -> &'a mut Compound {
    let found = chunk
        .block_entities
        .iter()
        .position(|entity| (entity.x, entity.y, entity.z) == (x, y, z));
    let index = found.unwrap_or_else(|| {
        let mut tag = Compound::new();
        tag.insert("id", format!("minecraft:{id}"));
        tag.insert("x", x);
        tag.insert("y", y);
        tag.insert("z", z);
        chunk.block_entities.push(BlockEntity {
            x,
            y,
            z,
            kind: -1,
            nbt: Some(Nbt::Compound(tag)),
        });
        chunk.block_entities.len() - 1
    });
    let entity = &mut chunk.block_entities[index];
    if !matches!(entity.nbt, Some(Nbt::Compound(_))) {
        entity.nbt = Some(Nbt::Compound(Compound::new()));
    }
    match &mut entity.nbt {
        Some(Nbt::Compound(tag)) => tag,
        _ => unreachable!("just made a compound"),
    }
}

/// Before a 1.13 (`source`) column's states are mapped to 1.12: write the
/// block entities that carry the colour, kind or plant 1.12 needs.
pub fn down(chunk: &mut Chunk, source: i32, scratch: &mut Scratch) -> io::Result<()> {
    let states = &data::version(source).block_states;
    for (x, y, z, state) in find(chunk, states, scratch)? {
        let Some((id, fields)) = fields_down(states, state) else {
            continue;
        };
        let tag = tag_for(chunk, x, y, z, id);
        if id == "banner" {
            invert_patterns(tag);
        }
        for (key, value) in fields.0 {
            tag.insert(key, value);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::chunk::{
        container::{Container, Format},
        model::{Biomes, Section},
    };

    fn column(states: &[(usize, i32)]) -> Chunk {
        let mut values = vec![0; 4096];
        for (entry, state) in states {
            values[*entry] = *state;
        }
        let mut scratch = Vec::new();
        Chunk {
            x: 1,
            z: 0,
            full: true,
            ignore_old_data: false,
            min_section: 0,
            sections: vec![Some(Section {
                block_count: 0,
                fluid_count: 0,
                blocks: Container::from_values(&values, &Format::blocks(393), &mut scratch),
                biomes: None,
            })],
            sky_light: true,
            biomes: Biomes::Absent,
            heightmaps: Vec::new(),
            block_entities: Vec::new(),
            light: None,
        }
    }

    #[test]
    fn colours_kinds_and_plants_move_between_states_and_block_entities() {
        let states = &data::version(393).block_states;
        let id = |state: &str| states.parse(state).unwrap();
        // Entry (y 0, z 0, x 3), (x 4), (x 5), (x 6) of column x 1.
        let mut chunk = column(&[
            (3, id("lime_bed[facing=east,occupied=false,part=head]")),
            (4, id("blue_wall_banner[facing=north]")),
            (5, id("creeper_head[rotation=7]")),
            (6, id("potted_fern")),
        ]);
        let mut scratch = Scratch::default();
        down(&mut chunk, 393, &mut scratch).unwrap();
        let tag = |chunk: &Chunk, x: i32| {
            chunk
                .block_entities
                .iter()
                .find(|entity| entity.x == 16 + x)
                .and_then(|entity| entity.nbt.as_ref())
                .and_then(Nbt::as_compound)
                .cloned()
                .unwrap()
        };
        assert_eq!(tag(&chunk, 3).int("color"), Some(5));
        assert_eq!(tag(&chunk, 4).int("Base"), Some(15 - 11));
        assert_eq!(
            (tag(&chunk, 5).int("SkullType"), tag(&chunk, 5).int("Rot")),
            (Some(4), Some(7))
        );
        assert_eq!(tag(&chunk, 6).str("Item"), Some("minecraft:tallgrass"));

        // Going up from what a flattening mapping would produce: default
        // colours and kinds, which the block entities then correct.
        let mut up_chunk = column(&[
            (3, id("red_bed[facing=east,occupied=false,part=head]")),
            (4, id("white_wall_banner[facing=north]")),
            (5, id("skeleton_skull[rotation=0]")),
            (6, id("flower_pot")),
        ]);
        up_chunk.block_entities = chunk.block_entities.clone();
        up(&mut up_chunk, 393, &mut scratch).unwrap();
        let block = |chunk: &Chunk, entry: usize| {
            let state = chunk.sections[0]
                .as_ref()
                .unwrap()
                .blocks
                .get(entry, &mut Vec::new())
                .unwrap();
            states.describe(state).unwrap()
        };
        assert_eq!(
            block(&up_chunk, 3),
            "lime_bed[facing=east,occupied=false,part=head]"
        );
        assert_eq!(block(&up_chunk, 4), "blue_wall_banner[facing=north]");
        assert_eq!(block(&up_chunk, 5), "creeper_head[rotation=7]");
        assert_eq!(block(&up_chunk, 6), "potted_fern");
        assert_eq!(
            up_chunk.block_entities.len(),
            3,
            "the flower pot's block entity is gone"
        );
    }
}
