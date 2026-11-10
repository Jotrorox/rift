//! Block entity data a 1.12.2 client needs that 1.13 keeps in block states.
//!
//! 1.13 made a bed's colour, a banner's base colour, a skull's type and
//! rotation and a flower pot's plant part of the block state; a 1.12.2
//! client reads them from the block entity, and draws beds red, banners
//! black, skulls not at all and pots empty without them. A 1.13 server
//! sends the block entity of beds, banners and skulls without these fields
//! and has none for flower pots.
//!
//! INTEGRATION: the chunk codec, converting chunks, block updates and block
//! entities from 1.13 to 1.12.2, should merge [`legacy_block_entity`]'s
//! fields into the block entity at each such block (creating the block
//! entity for flower pots, with `x`, `y`, `z` and `id`). A re-export from
//! `steps/mod.rs` makes it reachable from `codec::chunk`.
#![cfg_attr(not(test), allow(dead_code))]
use crate::{
    data,
    nbt::{Compound, Nbt},
};

/// Dye colours in 1.13's naming and 1.12's metadata order.
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

/// 1.13 skull blocks by 1.12 skull type.
const SKULLS: [(&str, &str); 6] = [
    ("skeleton_skull", "skeleton_wall_skull"),
    ("wither_skeleton_skull", "wither_skeleton_wall_skull"),
    ("zombie_head", "zombie_wall_head"),
    ("player_head", "player_wall_head"),
    ("creeper_head", "creeper_wall_head"),
    ("dragon_head", "dragon_wall_head"),
];

/// 1.13 potted plants as the 1.12 item and damage in the pot.
const POTTED: &[(&str, &str, i32)] = &[
    ("potted_poppy", "red_flower", 0),
    ("potted_blue_orchid", "red_flower", 1),
    ("potted_allium", "red_flower", 2),
    ("potted_azure_bluet", "red_flower", 3),
    ("potted_red_tulip", "red_flower", 4),
    ("potted_orange_tulip", "red_flower", 5),
    ("potted_white_tulip", "red_flower", 6),
    ("potted_pink_tulip", "red_flower", 7),
    ("potted_oxeye_daisy", "red_flower", 8),
    ("potted_dandelion", "yellow_flower", 0),
    ("potted_oak_sapling", "sapling", 0),
    ("potted_spruce_sapling", "sapling", 1),
    ("potted_birch_sapling", "sapling", 2),
    ("potted_jungle_sapling", "sapling", 3),
    ("potted_acacia_sapling", "sapling", 4),
    ("potted_dark_oak_sapling", "sapling", 5),
    ("potted_red_mushroom", "red_mushroom", 0),
    ("potted_brown_mushroom", "brown_mushroom", 0),
    ("potted_fern", "tallgrass", 2),
    ("potted_dead_bush", "deadbush", 0),
    ("potted_cactus", "cactus", 0),
];

/// The block entity a 1.12.2 client needs for a block.
#[derive(Clone, Debug, PartialEq)]
pub struct LegacyBlockEntity {
    /// The 1.12.2 block entity ID.
    pub id: &'static str,
    /// The fields to set, without `id` and the position.
    pub fields: Compound,
}

/// What a 1.12.2 client needs in the block entity of the 1.13 block state
/// `state`; `None` for blocks whose state implies nothing for it.
pub fn legacy_block_entity(state: i32) -> Option<LegacyBlockEntity> {
    let (block, offset) = data::version(super::NEWER).block_states.locate(state)?;
    let name = &*block.name;
    let values = block.values(offset);
    let value = |property: &str| {
        values
            .iter()
            .find(|(name, _)| *name == property)
            .map(|(_, value)| *value)
    };
    let colour = |suffix: &str| {
        let colour = name.strip_suffix(suffix)?;
        COLOURS.iter().position(|known| *known == colour)
    };
    let mut fields = Compound::new();
    let id = if let Some(colour) = colour("_bed") {
        fields.insert("color", Nbt::Int(colour as i32));
        "minecraft:bed"
    } else if let Some(colour) = colour("_banner").or_else(|| colour("_wall_banner")) {
        // 1.12 stores the dye's damage value, the reverse of the wool order.
        fields.insert("Base", Nbt::Int(15 - colour as i32));
        "minecraft:banner"
    } else if let Some(kind) = SKULLS
        .iter()
        .position(|(floor, wall)| name == *floor || name == *wall)
    {
        let rotation = value("rotation").and_then(|rotation| rotation.parse::<i8>().ok());
        fields.insert("SkullType", Nbt::Byte(kind as i8));
        // A wall skull's facing is in its 1.12 metadata.
        fields.insert("Rot", Nbt::Byte(rotation.unwrap_or(0)));
        "minecraft:skull"
    } else if name == "flower_pot" {
        fields.insert("Item", Nbt::String(String::new()));
        fields.insert("Data", Nbt::Int(0));
        "minecraft:flower_pot"
    } else if let Some((_, item, data)) = POTTED.iter().find(|(potted, _, _)| *potted == name) {
        fields.insert("Item", Nbt::String(format!("minecraft:{item}")));
        fields.insert("Data", Nbt::Int(*data));
        "minecraft:flower_pot"
    } else {
        return None;
    };
    Some(LegacyBlockEntity { id, fields })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> i32 {
        data::version(super::super::NEWER)
            .block_states
            .parse(name)
            .unwrap_or_else(|| panic!("{name}"))
    }

    fn fields(entries: &[(&str, Nbt)]) -> Compound {
        let mut compound = Compound::new();
        for (key, value) in entries {
            compound.insert(*key, value.clone());
        }
        compound
    }

    #[test]
    fn colours_types_and_plants_move_back_into_block_entities() {
        assert_eq!(
            legacy_block_entity(state("lime_bed[part=head]")),
            Some(LegacyBlockEntity {
                id: "minecraft:bed",
                fields: fields(&[("color", Nbt::Int(5))]),
            })
        );
        assert_eq!(
            legacy_block_entity(state("white_wall_banner"))
                .unwrap()
                .fields,
            fields(&[("Base", Nbt::Int(15))])
        );
        assert_eq!(
            legacy_block_entity(state("black_banner[rotation=3]"))
                .unwrap()
                .fields,
            fields(&[("Base", Nbt::Int(0))])
        );
        assert_eq!(
            legacy_block_entity(state("creeper_head[rotation=7]")),
            Some(LegacyBlockEntity {
                id: "minecraft:skull",
                fields: fields(&[("SkullType", Nbt::Byte(4)), ("Rot", Nbt::Byte(7))]),
            })
        );
        assert_eq!(
            legacy_block_entity(state("wither_skeleton_wall_skull[facing=east]"))
                .unwrap()
                .fields,
            fields(&[("SkullType", Nbt::Byte(1)), ("Rot", Nbt::Byte(0))])
        );
        assert_eq!(
            legacy_block_entity(state("potted_fern")).unwrap().fields,
            fields(&[
                ("Item", Nbt::String("minecraft:tallgrass".into())),
                ("Data", Nbt::Int(2))
            ])
        );
        assert_eq!(
            legacy_block_entity(state("flower_pot")).unwrap().fields,
            fields(&[("Item", Nbt::String(String::new())), ("Data", Nbt::Int(0))])
        );
        assert_eq!(legacy_block_entity(state("stone")), None);
        assert_eq!(legacy_block_entity(-1), None);
    }

    #[test]
    fn every_bed_banner_skull_and_pot_is_covered() {
        let states = &data::version(super::super::NEWER).block_states;
        for block in &states.blocks {
            let name = &*block.name;
            let expected = name.ends_with("_bed")
                || name.ends_with("_banner")
                || name.ends_with("_skull")
                || name.ends_with("_head") && name != "piston_head"
                || name.starts_with("potted_")
                || name == "flower_pot";
            assert_eq!(
                legacy_block_entity(block.default_state()).is_some(),
                expected,
                "{name}"
            );
        }
    }
}
