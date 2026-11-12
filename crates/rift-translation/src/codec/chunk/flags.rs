//! What chunk conversions need to know about each block state of a
//! flattened protocol: whether it is air, holds a fluid, or stops motion.
//! Built once per protocol from its block states.
use crate::data;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

pub const AIR: u8 = 1;
pub const FLUID: u8 = 2;
/// Counted by the `MOTION_BLOCKING` heightmap: solid or fluid.
pub const MOTION: u8 = 4;

/// Blocks a player walks through, by name or name suffix. Vanilla decides
/// by material; Rift only computes heightmaps for 1.13 chunks going to
/// 1.14, where the client uses them for rain, so an approximation will do.
const PASSABLE: &[&str] = &[
    "grass",
    "fern",
    "tall_grass",
    "large_fern",
    "dead_bush",
    "torch",
    "redstone_wire",
    "rail",
    "lever",
    "tripwire",
    "tripwire_hook",
    "button",
    "pressure_plate",
    "sign",
    "sapling",
    "dandelion",
    "poppy",
    "blue_orchid",
    "allium",
    "azure_bluet",
    "tulip",
    "oxeye_daisy",
    "sunflower",
    "lilac",
    "rose_bush",
    "peony",
    "mushroom",
    "wheat",
    "carrots",
    "potatoes",
    "beetroots",
    "sugar_cane",
    "nether_wart",
    "vine",
    "banner",
    "structure_void",
    "fire",
    "nether_portal",
    "end_portal",
    "cobweb",
];

const ALWAYS_WET: &[&str] = &[
    "water",
    "lava",
    "bubble_column",
    "kelp",
    "kelp_plant",
    "seagrass",
    "tall_seagrass",
];

fn passable(name: &str) -> bool {
    PASSABLE
        .iter()
        .any(|suffix| name == *suffix || name.ends_with(&format!("_{suffix}")))
}

/// Flags of every block state of `protocol` (1.13 or newer), by state ID.
pub fn of(protocol: i32) -> &'static [u8] {
    static TABLES: OnceLock<Mutex<HashMap<i32, &'static [u8]>>> = OnceLock::new();
    let mut tables = TABLES.get_or_init(Default::default).lock().unwrap();
    tables.entry(protocol).or_insert_with(|| {
        let states = &data::version(protocol).block_states;
        let mut flags = vec![0u8; states.len() as usize];
        for block in &states.blocks {
            let name = &*block.name;
            let air = matches!(name, "air" | "cave_air" | "void_air");
            let wet = ALWAYS_WET.contains(&name);
            let waterlogged = block
                .properties
                .iter()
                .position(|(property, _)| &**property == "waterlogged");
            for offset in 0..block.state_count() {
                let mut flag = 0;
                if air {
                    flag |= AIR;
                }
                let fluid =
                    wet || waterlogged.is_some_and(|index| block.values(offset)[index].1 == "true");
                if fluid {
                    flag |= FLUID | MOTION;
                }
                if !air && !passable(name) {
                    flag |= MOTION;
                }
                flags[(block.first_state + offset) as usize] = flag;
            }
        }
        Box::leak(flags.into_boxed_slice())
    })
}

/// Whether state `id` has `flag` in `table`; unknown IDs have none.
pub fn has(table: &[u8], id: i32, flag: u8) -> bool {
    usize::try_from(id)
        .ok()
        .and_then(|id| table.get(id))
        .is_some_and(|flags| flags & flag != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_follow_names_and_properties() {
        let states = &data::version(777).block_states;
        let table = of(777);
        let id = |state: &str| states.parse(state).unwrap();
        assert!(has(table, id("air"), AIR) && has(table, id("cave_air"), AIR));
        assert!(!has(table, id("stone"), AIR) && has(table, id("stone"), MOTION));
        assert!(has(table, id("water"), FLUID));
        assert!(has(table, id("oak_stairs[waterlogged=true]"), FLUID));
        assert!(!has(table, id("oak_stairs[waterlogged=false]"), FLUID));
        assert!(!has(table, id("poppy"), MOTION) && !has(table, id("oak_sign"), MOTION));
        assert!(has(table, id("grass_block"), MOTION));
        assert!(!has(table, -1, AIR) && !has(table, i32::MAX, AIR));
    }
}
