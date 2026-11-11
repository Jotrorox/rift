//! Blocks and items across the 1.13 flattening.
//!
//! 1.12 numbers a block state `id << 4 | metadata` and an item by ID plus a
//! damage value; 1.13 names both. `data/legacy.json` (generated from
//! minecraft-data's `common/legacy.json`, see `data/SOURCES.md`) holds:
//!
//! * every 1.12 state and item variant minecraft-data knows, with its 1.13
//!   state (`name[property=value,...]`, unnamed properties at their default)
//!   or item;
//! * for every 1.13 state and item, the best 1.12 one: an exact source;
//!   else, for a state, the source of the same block matching the most
//!   property values (stairs shapes, fence connections and waterlogging are
//!   computed by the 1.12 client or did not exist); else the source of a
//!   variant 1.12 kept in block entity or NBT data, or did not tell apart.
//!   So a white bed becomes a (red) 1.12 bed, a zombie head a skeleton skull,
//!   every spawn egg the pig spawn egg and a spruce button an oak button: the
//!   step restores the colour, skull type or entity from block entity or NBT
//!   data. Still water and lava win over flowing. Genuinely new 1.13 content
//!   (coral, kelp, conduits, ...) has no 1.12 counterpart; the 340-393 rules
//!   name stand-ins for it.
//!
//! [`crate::mapping::mapping`] between 340 and 393 goes through this module
//! for `block_state`, `block` and `item` and then applies the 340-393 rules,
//! whose 1.12 names are `name:metadata` for states and `name` or
//! `name:damage` for items. Item mappings carry no damage: steps convert
//! items with [`Legacy::flatten_item`] and [`Legacy::unflatten_item`].
use crate::{
    data::{self, BlockStates, Registry},
    mapping::{self, Kind, Mapping, RegistryRules},
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    sync::OnceLock,
};

/// The last protocol before the flattening (1.12.2).
pub const LEGACY: i32 = 340;
/// The first flattened protocol (1.13).
pub const FLATTENED: i32 = 393;

#[derive(Deserialize)]
struct RawLegacy {
    blocks: BTreeMap<String, String>,
    block_reverse: Vec<Option<String>>,
    items: BTreeMap<String, String>,
    item_reverse: BTreeMap<String, String>,
    spawn_eggs: BTreeMap<String, String>,
}

/// A 1.12 item as 1.13 sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlatItem {
    /// The 1.13 item ID.
    pub item: i32,
    /// The damage value was the item's durability, which 1.13 keeps in the
    /// `Damage` tag; otherwise it selected the variant and is used up.
    pub durability: bool,
}

/// The flattening tables, with IDs resolved against 1.12.2 and 1.13.
#[derive(Debug)]
pub struct Legacy {
    /// 1.13 state by 1.12 state.
    blocks: HashMap<i32, i32>,
    /// The lowest 1.12 state minecraft-data lists for each block ID.
    first_states: HashMap<i32, i32>,
    /// 1.12 state by 1.13 state, -1 for none.
    block_reverse: Vec<i32>,
    /// 1.13 item by 1.12 item and damage.
    items: HashMap<(i32, i16), i32>,
    /// How many variants each 1.12 item has, and its lowest damage value.
    variants: HashMap<i32, (usize, i16)>,
    /// 1.12 item and damage by 1.13 item.
    item_reverse: HashMap<i32, (i32, i16)>,
    spawn_eggs: HashMap<String, i32>,
    spawn_egg_entities: HashMap<i32, String>,
}

/// `id:number` split; `None` when malformed.
fn pair(text: &str) -> Option<(i32, i32)> {
    let (id, number) = text.split_once(':')?;
    Some((id.parse().ok()?, number.parse().ok()?))
}

/// The flattening tables, loaded on first use.
pub fn legacy() -> &'static Legacy {
    static LEGACY_DATA: OnceLock<Legacy> = OnceLock::new();
    LEGACY_DATA.get_or_init(|| {
        let text = data::text("legacy").expect("data/legacy.json exists");
        let raw: RawLegacy = serde_json::from_str(&text).expect("data/legacy.json is valid");
        let flat = data::version(FLATTENED);
        let (states, items) = (&flat.block_states, flat.expect_registry("item"));
        let state = |text: &str| {
            states
                .parse(text)
                .unwrap_or_else(|| panic!("legacy.json names unknown 1.13 state {text}"))
        };
        let item = |name: &str| {
            items
                .id(name)
                .unwrap_or_else(|| panic!("legacy.json names unknown 1.13 item {name}"))
        };
        let blocks: HashMap<i32, i32> = raw
            .blocks
            .iter()
            .map(|(key, target)| {
                let (id, meta) = pair(key).expect("legacy block keys are id:meta");
                (id << 4 | meta, state(target))
            })
            .collect();
        let mut first_states: HashMap<i32, i32> = HashMap::new();
        for state in blocks.keys() {
            let first = first_states.entry(state >> 4).or_insert(*state);
            *first = (*first).min(*state);
        }
        let block_reverse = raw
            .block_reverse
            .iter()
            .map(|source| {
                source.as_deref().map_or(-1, |source| {
                    let (id, meta) = pair(source).expect("legacy block sources are id:meta");
                    id << 4 | meta
                })
            })
            .collect();
        let mut variants = HashMap::new();
        let items_by_damage = raw
            .items
            .iter()
            .map(|(key, target)| {
                let (id, damage) = pair(key).expect("legacy item keys are id:damage");
                let entry = variants.entry(id).or_insert((0, damage as i16));
                entry.0 += 1;
                entry.1 = entry.1.min(damage as i16);
                ((id, damage as i16), item(target))
            })
            .collect();
        let item_reverse = raw
            .item_reverse
            .iter()
            .map(|(name, source)| {
                let (id, damage) = pair(source).expect("legacy item sources are id:damage");
                (item(name), (id, damage as i16))
            })
            .collect();
        let spawn_eggs: HashMap<String, i32> = raw
            .spawn_eggs
            .iter()
            .map(|(entity, egg)| (entity.clone(), item(egg)))
            .collect();
        let spawn_egg_entities = spawn_eggs
            .iter()
            .map(|(entity, egg)| (*egg, entity.clone()))
            .collect();
        Legacy {
            blocks,
            first_states,
            block_reverse,
            items: items_by_damage,
            variants,
            item_reverse,
            spawn_eggs,
            spawn_egg_entities,
        }
    })
}

impl Legacy {
    /// The 1.13 state of 1.12 state `id << 4 | meta` as minecraft-data
    /// knows it; `None` for metadata the block does not use.
    pub fn flatten_block_exact(&self, state: i32) -> Option<i32> {
        self.blocks.get(&state).copied()
    }

    /// The 1.13 state of a 1.12 state, falling back to the block's lowest
    /// listed metadata for metadata it does not use.
    pub fn flatten_block(&self, state: i32) -> Option<i32> {
        self.flatten_block_exact(state)
            .or_else(|| self.flatten_block_exact(*self.first_states.get(&(state >> 4))?))
    }

    /// The best 1.12 state for a 1.13 state; see the module docs.
    pub fn unflatten_block(&self, state: i32) -> Option<i32> {
        let found = *self.block_reverse.get(usize::try_from(state).ok()?)?;
        (found >= 0).then_some(found)
    }

    /// The 1.13 item of a 1.12 item and damage value. An unknown damage
    /// value of an item with one variant is its durability; of an item with
    /// variants it falls back to the lowest listed damage value.
    pub fn flatten_item(&self, id: i32, damage: i16) -> Option<FlatItem> {
        if let Some(item) = self.items.get(&(id, damage)) {
            return Some(FlatItem {
                item: *item,
                durability: false,
            });
        }
        let (count, lowest) = *self.variants.get(&id)?;
        Some(FlatItem {
            item: *self.items.get(&(id, lowest))?,
            durability: count == 1,
        })
    }

    /// The 1.13 item minecraft-data lists for exactly this 1.12 item and
    /// damage value, without the fallbacks of [`Self::flatten_item`].
    pub fn flatten_item_exact(&self, id: i32, damage: i16) -> Option<i32> {
        self.items.get(&(id, damage)).copied()
    }

    /// The best 1.12 item and damage value for a 1.13 item; see the module
    /// docs. Durability goes back into the damage value: the step adds it.
    pub fn unflatten_item(&self, item: i32) -> Option<(i32, i16)> {
        self.item_reverse.get(&item).copied()
    }

    /// The 1.13 spawn egg for a 1.12 spawn egg's entity (its 1.12 entity
    /// name, such as `evocation_illager`).
    pub fn spawn_egg(&self, entity: &str) -> Option<i32> {
        self.spawn_eggs.get(data::strip(entity)).copied()
    }

    /// The 1.12 entity name a 1.13 spawn egg spawns, for the NBT of the
    /// 1.12 spawn egg it becomes.
    pub fn spawn_egg_entity(&self, item: i32) -> Option<&str> {
        self.spawn_egg_entities.get(&item).map(String::as_str)
    }
}

/// A 1.12 state named `name:meta` (or `name`, metadata 0) in `protocol`.
pub fn parse_block(protocol: i32, name: &str) -> Option<i32> {
    let blocks = data::try_version(protocol)?.registry("block")?;
    let (name, meta) = split_number(name)?;
    (0..16)
        .contains(&meta)
        .then_some(())
        .and(blocks.id(name).map(|id| id << 4 | meta))
}

/// A 1.12 item named `name:damage` (or `name`, damage 0) in `protocol`.
pub fn parse_item(protocol: i32, name: &str) -> Option<(i32, i16)> {
    let items = data::try_version(protocol)?.registry("item")?;
    let (name, damage) = split_number(name)?;
    Some((items.id(name)?, i16::try_from(damage).ok()?))
}

fn split_number(name: &str) -> Option<(&str, i32)> {
    let name = data::strip(name);
    match name.rsplit_once(':') {
        Some((name, number)) => Some((name, number.parse().ok()?)),
        None => Some((name, 0)),
    }
}

fn rules(registry: &str) -> &'static RegistryRules {
    static EMPTY: OnceLock<RegistryRules> = OnceLock::new();
    mapping::rules(LEGACY, FLATTENED)
        .get(registry)
        .unwrap_or_else(|| EMPTY.get_or_init(RegistryRules::default))
}

fn push(table: &mut Vec<i32>, kinds: &mut Vec<Kind>, found: Option<(i32, Kind)>) {
    let (mapped, kind) = found.unwrap_or((-1, Kind::Unmapped));
    table.push(mapped);
    kinds.push(if mapped < 0 { Kind::Unmapped } else { kind });
}

/// `block_state` between 340 and 393, through the tables and the rules.
pub(crate) fn block_state_mapping(from: i32, to: i32) -> Option<Mapping> {
    let legacy = legacy();
    let rules = rules("block_state");
    let flat = &data::try_version(FLATTENED)?.block_states;
    let (mut table, mut kinds) = (Vec::new(), Vec::new());
    if (from, to) == (LEGACY, FLATTENED) {
        let blocks = data::try_version(LEGACY)?.registry("block")?;
        let up: HashMap<i32, i32> = rules
            .up
            .iter()
            .filter_map(|(key, value)| Some((parse_block(LEGACY, key)?, flat.parse(value)?)))
            .collect();
        let default = rules
            .up_default
            .as_deref()
            .and_then(|name| flat.parse(name));
        for state in 0..(blocks.len() as i32) << 4 {
            if blocks.name(state >> 4).is_none() {
                table.push(-1);
                kinds.push(Kind::Gap);
                continue;
            }
            // A whole-block stand-in is keyed by metadata 0.
            let found = legacy
                .flatten_block_exact(state)
                .map(|mapped| (mapped, Kind::Identical))
                .or_else(|| up.get(&state).map(|mapped| (*mapped, Kind::StandIn)))
                .or_else(|| {
                    legacy
                        .flatten_block(state)
                        .map(|mapped| (mapped, Kind::Closest))
                })
                .or_else(|| {
                    up.get(&(state & !15))
                        .map(|mapped| (*mapped, Kind::StandIn))
                })
                .or_else(|| default.map(|mapped| (mapped, Kind::StandIn)));
            push(&mut table, &mut kinds, found);
        }
        return Some(Mapping::from_parts(table, kinds));
    }
    if (from, to) != (FLATTENED, LEGACY) {
        return None;
    }
    let down = down_rules(flat, rules);
    let default = rules
        .down_default
        .as_deref()
        .and_then(|name| parse_block(LEGACY, name));
    for state in 0..flat.len() {
        let found = legacy
            .unflatten_block(state)
            .map(|mapped| {
                let kind = if legacy.flatten_block_exact(mapped) == Some(state) {
                    Kind::Identical
                } else {
                    Kind::Closest
                };
                (mapped, kind)
            })
            .or_else(|| down_rule(flat, &down, state).map(|mapped| (mapped, Kind::StandIn)))
            .or_else(|| default.map(|mapped| (mapped, Kind::StandIn)));
        push(&mut table, &mut kinds, found);
    }
    Some(Mapping::from_parts(table, kinds))
}

type DownRule<'a> = (Vec<(&'a str, &'a str)>, i32, &'a str);

/// The `down` block-state rules of 340-393 by 1.13 block: the properties
/// each names and its 1.12 state.
fn down_rules<'a>(
    flat: &BlockStates,
    rules: &'a RegistryRules,
) -> HashMap<&'a str, Vec<DownRule<'a>>> {
    let mut by_block: HashMap<&str, Vec<DownRule<'_>>> = HashMap::new();
    for (key, value) in &rules.down {
        let (Some((name, properties)), Some(target)) =
            (mapping::parse_pattern(key), parse_block(LEGACY, value))
        else {
            continue;
        };
        if flat.block(name).is_some() {
            by_block
                .entry(name)
                .or_default()
                .push((properties, target, key));
        }
    }
    by_block
}

/// The most specific `down` rule matching a 1.13 state, as in
/// [`crate::mapping`].
fn down_rule(
    flat: &BlockStates,
    rules: &HashMap<&str, Vec<DownRule<'_>>>,
    state: i32,
) -> Option<i32> {
    let (block, offset) = flat.locate(state)?;
    let values = block.values(offset);
    rules
        .get(&*block.name)?
        .iter()
        .filter(|(properties, _, _)| properties.iter().all(|pair| values.contains(pair)))
        .min_by(|a, b| b.0.len().cmp(&a.0.len()).then(a.2.cmp(b.2)))
        .map(|(_, target, _)| *target)
}

/// `item` between 340 and 393 by item ID alone (damage 0 upwards, the
/// damage value dropped downwards).
pub(crate) fn item_mapping(from: i32, to: i32) -> Option<Mapping> {
    let legacy = legacy();
    let rules = rules("item");
    let old = data::try_version(LEGACY)?.registry("item")?;
    let new = data::try_version(FLATTENED)?.registry("item")?;
    let (mut table, mut kinds) = (Vec::new(), Vec::new());
    if (from, to) == (LEGACY, FLATTENED) {
        let up: HashMap<(i32, i16), i32> = rules
            .up
            .iter()
            .filter_map(|(key, value)| Some((parse_item(LEGACY, key)?, new.id(value)?)))
            .collect();
        let default = rules.up_default.as_deref().and_then(|name| new.id(name));
        for id in 0..old.len() as i32 {
            if old.name(id).is_none() && id != 0 {
                table.push(-1);
                kinds.push(Kind::Gap);
                continue;
            }
            let found = legacy
                .flatten_item(id, 0)
                .map(|item| (item.item, Kind::Identical))
                .or_else(|| up.get(&(id, 0)).map(|item| (*item, Kind::StandIn)))
                .or_else(|| default.map(|item| (item, Kind::StandIn)));
            push(&mut table, &mut kinds, found);
        }
        return Some(Mapping::from_parts(table, kinds));
    }
    if (from, to) != (FLATTENED, LEGACY) {
        return None;
    }
    let default = rules
        .down_default
        .as_deref()
        .and_then(|name| parse_item(LEGACY, name));
    for id in 0..new.len() as i32 {
        let found = legacy
            .unflatten_item(id)
            .map(|(item, _)| (item, Kind::Identical))
            .or_else(|| {
                let name = new.name(id)?;
                let (item, _) = parse_item(LEGACY, rules.down.get(name)?)?;
                Some((item, Kind::StandIn))
            })
            .or_else(|| default.map(|(item, _)| (item, Kind::StandIn)));
        push(&mut table, &mut kinds, found);
    }
    Some(Mapping::from_parts(table, kinds))
}

/// The `block` registry between 340 and 393, through block states: a 1.12
/// block is its metadata 0 state's 1.13 block, a 1.13 block its default
/// state's 1.12 block.
pub(crate) fn block_mapping(from: i32, to: i32) -> Option<Mapping> {
    let states = mapping::try_mapping(from, to, "block_state")?;
    let old = data::try_version(LEGACY)?.registry("block")?;
    let flat = data::try_version(FLATTENED)?;
    let new: &Registry = flat.registry("block")?;
    let (mut table, mut kinds) = (Vec::new(), Vec::new());
    if (from, to) == (LEGACY, FLATTENED) {
        for id in 0..old.len() as i32 {
            if old.name(id).is_none() {
                table.push(-1);
                kinds.push(Kind::Gap);
                continue;
            }
            // The block's lowest listed state: torches start at metadata 1.
            let source = legacy().first_states.get(&id).copied().unwrap_or(id << 4);
            let found = states.get(source).and_then(|state| {
                let (block, _) = flat.block_states.locate(state)?;
                Some((new.id(&block.name)?, states.kind(source)?))
            });
            push(&mut table, &mut kinds, found);
        }
    } else {
        for name in (0..new.len() as i32).map(|id| new.name(id)) {
            let found = name
                .and_then(|name| flat.block_states.block(name))
                .and_then(|block| {
                    let state = block.default_state();
                    Some((states.get(state)? >> 4, states.kind(state)?))
                });
            push(&mut table, &mut kinds, found);
        }
    }
    Some(Mapping::from_parts(table, kinds))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat() -> &'static BlockStates {
        &data::version(FLATTENED).block_states
    }

    fn up(id: i32, meta: i32) -> String {
        flat()
            .describe(legacy().flatten_block(id << 4 | meta).unwrap())
            .unwrap()
    }

    fn down(state: &str) -> (i32, i32) {
        let found = legacy()
            .unflatten_block(flat().parse(state).unwrap())
            .unwrap();
        (found >> 4, found & 15)
    }

    fn item(name: &str) -> i32 {
        data::version(FLATTENED)
            .expect_registry("item")
            .id(name)
            .unwrap()
    }

    fn item_name(id: i32) -> &'static str {
        data::version(FLATTENED)
            .expect_registry("item")
            .name(id)
            .unwrap()
    }

    #[test]
    fn blocks_flatten_and_come_back() {
        assert_eq!(up(1, 0), "stone");
        assert_eq!(up(1, 1), "granite");
        assert_eq!(up(35, 14), "red_wool");
        assert_eq!(up(17, 6), "birch_log[axis=x]");
        assert_eq!(down("stone"), (1, 0));
        assert_eq!(down("polished_andesite"), (1, 6));
        assert_eq!(down("black_wool"), (35, 15));
        assert_eq!(down("birch_log[axis=z]"), (17, 10));
        assert_eq!(down("dark_oak_log[axis=y]"), (162, 1));
        // Metadata a block does not use falls back to its lowest listed one.
        assert_eq!(legacy().flatten_block_exact(1 << 4 | 15), None);
        assert_eq!(up(1, 15), "stone");
        assert_eq!(legacy().flatten_block_exact(50 << 4), None);
        assert_eq!(up(50, 0), up(50, 1));
        // Every state minecraft-data lists comes back to itself or to a
        // source with the same 1.13 state.
        for (state, flat_state) in &legacy().blocks {
            let back = legacy().unflatten_block(*flat_state).unwrap();
            assert_eq!(
                legacy().flatten_block_exact(back),
                Some(*flat_state),
                "{state}"
            );
        }
    }

    #[test]
    fn ambiguous_reverses_choose_sensibly() {
        // Still water and lava rather than flowing.
        assert_eq!(down("water[level=0]"), (9, 0));
        assert_eq!(down("lava[level=0]"), (11, 0));
        // 1.12 keeps bed colour, skull type and banner colour in block
        // entities: the shape survives, the step restores the rest.
        let bed = down("white_bed[facing=north,occupied=false,part=head]");
        assert_eq!(bed.0, 26);
        assert_eq!(
            up(bed.0, bed.1),
            "red_bed[facing=north,occupied=false,part=head]"
        );
        assert_eq!(down("zombie_wall_head[facing=east]"), (144, 5));
        assert_eq!(down("creeper_head[rotation=0]").0, 144);
        assert_eq!(down("blue_wall_banner[facing=south]"), (177, 3));
        // Doors keep their half; 1.12 stores hinge and facing on one half
        // each.
        let lower = down("oak_door[facing=west,half=lower,hinge=left,open=true,powered=false]");
        assert_eq!(lower, (64, 6));
        let upper = down("oak_door[facing=west,half=upper,hinge=left,open=false,powered=false]");
        assert_eq!(upper, (64, 8));
        // Waterlogging and fence connections did not exist in 1.12.
        assert_eq!(
            down("oak_fence[east=true,north=false,south=true,waterlogged=true,west=false]"),
            (85, 0)
        );
        assert_eq!(
            down("spruce_button[face=wall,facing=north,powered=false]").0,
            143
        );
        assert_eq!(down("stripped_birch_log[axis=y]"), (17, 2));
        assert_eq!(down("cave_air"), (0, 0));
        // New in 1.13: nothing, until the 340-393 rules name a stand-in.
        assert_eq!(
            legacy().unflatten_block(flat().parse("tube_coral").unwrap()),
            None
        );
    }

    #[test]
    fn items_flatten_with_their_damage() {
        let legacy = legacy();
        let flat_item = |id, damage| item_name(legacy.flatten_item(id, damage).unwrap().item);
        assert_eq!(flat_item(1, 3), "diorite");
        assert_eq!(flat_item(35, 1), "orange_wool");
        assert_eq!(flat_item(351, 4), "lapis_lazuli");
        assert_eq!(flat_item(397, 4), "creeper_head");
        assert_eq!(flat_item(373, 0), "potion");
        // Tools: the damage value is durability.
        assert_eq!(
            legacy.flatten_item(256, 37),
            Some(FlatItem {
                item: item("iron_shovel"),
                durability: true
            })
        );
        assert!(!legacy.flatten_item(35, 1).unwrap().durability);
        assert_eq!(legacy.unflatten_item(item("orange_wool")), Some((35, 1)));
        assert_eq!(legacy.unflatten_item(item("bone_meal")), Some((351, 15)));
        assert_eq!(
            legacy.unflatten_item(item("wither_skeleton_skull")),
            Some((397, 1))
        );
        assert_eq!(legacy.unflatten_item(item("splash_potion")), Some((438, 0)));
        assert_eq!(legacy.unflatten_item(item("trident")), None);
    }

    #[test]
    fn spawn_eggs_name_their_entity() {
        let legacy = legacy();
        assert_eq!(
            legacy.spawn_egg("evocation_illager"),
            Some(item("evoker_spawn_egg"))
        );
        assert_eq!(
            legacy.spawn_egg("minecraft:cow"),
            Some(item("cow_spawn_egg"))
        );
        assert_eq!(
            legacy.spawn_egg_entity(item("vindicator_spawn_egg")),
            Some("vindication_illager")
        );
        // Every spawn egg is item 383; the entity is NBT.
        assert_eq!(legacy.unflatten_item(item("cow_spawn_egg")), Some((383, 0)));
        assert_eq!(legacy.spawn_egg("drowned"), None);
    }

    #[test]
    fn registry_mappings_cross_the_flattening() {
        let states = mapping::mapping(LEGACY, FLATTENED, "block_state");
        assert_eq!(
            flat().describe(states.get(35 << 4 | 14).unwrap()).unwrap(),
            "red_wool"
        );
        let back = mapping::mapping(FLATTENED, LEGACY, "block_state");
        assert_eq!(
            back.get(flat().parse("red_wool").unwrap()),
            Some(35 << 4 | 14)
        );
        let items = mapping::mapping(LEGACY, FLATTENED, "item");
        let old_items = data::version(LEGACY).expect_registry("item");
        assert_eq!(
            item_name(items.get(old_items.id("diamond_sword").unwrap()).unwrap()),
            "diamond_sword"
        );
        let items_back = mapping::mapping(FLATTENED, LEGACY, "item");
        assert_eq!(items_back.get(item("lime_wool")), old_items.id("wool"));
        let blocks = mapping::mapping(LEGACY, FLATTENED, "block");
        let old_blocks = data::version(LEGACY).expect_registry("block");
        let new_blocks = data::version(FLATTENED).expect_registry("block");
        assert_eq!(
            new_blocks.name(blocks.get(old_blocks.id("wool").unwrap()).unwrap()),
            Some("white_wool")
        );
        let blocks_back = mapping::mapping(FLATTENED, LEGACY, "block");
        assert_eq!(
            blocks_back.get(new_blocks.id("lime_wool").unwrap()),
            old_blocks.id("wool")
        );
    }

    #[test]
    fn legacy_names_parse() {
        assert_eq!(parse_block(LEGACY, "wool:14"), Some(35 << 4 | 14));
        assert_eq!(parse_block(LEGACY, "minecraft:stone"), Some(1 << 4));
        assert_eq!(parse_block(LEGACY, "wool:16"), None);
        assert_eq!(
            parse_item(LEGACY, "dye:4").map(|(_, damage)| damage),
            Some(4)
        );
        assert_eq!(parse_item(LEGACY, "no_such_item"), None);
    }
}
