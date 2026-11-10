//! The 1.13 flattening of items (1.12.2, protocol 340, and 1.13, 393): a
//! numeric ID and data value become one item per variant, and the tag
//! changes with it:
//!
//! - damageable items keep their damage in `Damage`, filled maps their map
//!   in `map`;
//! - spawn eggs become one item per mob (`EntityTag.id` before);
//! - banners take their colour from the item, and banner and shield colours
//!   in `BlockEntityTag` count from white instead of black;
//! - `ench` (numeric IDs) becomes `Enchantments` (names), and stored
//!   enchantments name theirs;
//! - `display.Name` becomes a JSON text component;
//! - `CanPlaceOn` and `CanDestroy` name the flattened blocks;
//! - items in a `BlockEntityTag` (shulker boxes) change the same way.
//!
//! What one side cannot express is stashed under `rift:<protocol>` with the
//! original tag, and restored when the stack comes back.
use super::{
    ItemStack,
    convert::{item_id, item_name, name_stand_in, stash_key, tidy},
    legacy, legacy_text,
};
use crate::{
    Direction,
    codec::Shared,
    data,
    nbt::{Compound, Nbt},
    wire::invalid,
};
use std::io;

const LEGACY: i32 = 340;
const FLAT: i32 = 393;

const SPAWN_EGG: i16 = 383;
const BANNER: i16 = 425;
const SHIELD: i16 = 442;
const FILLED_MAP: i16 = 358;
const SHRUB: i16 = 31;

/// 1.12 items that use their data value as damage.
fn damageable(id: i16) -> bool {
    matches!(id, 256..=259 | 261 | 267..=279 | 283..=286 | 290..=294 | 298..=317 | 346 | 359 | 398 | 442 | 443)
}

fn stash(tag: &Compound, key: &str) -> Option<Compound> {
    tag.compound(key).cloned()
}

/// A 1.12 stack for a 1.13 receiver.
pub(super) fn up(
    shared: &mut Shared,
    direction: Direction,
    item: &mut ItemStack,
) -> io::Result<()> {
    let _ = (shared, direction);
    let id = i16::try_from(item.id).map_err(|_| invalid("1.12 item ID out of range"))?;
    let damage = item.damage;
    item.damage = 0;
    let mut tag = match item.tag.take() {
        Some(Nbt::Compound(tag)) => tag,
        Some(_) => return Err(invalid("item tag is not a compound")),
        None => Compound::new(),
    };
    if let Some(original) = tag.remove(&stash_key(FLAT)) {
        // A 1.13 stack this client got as a stand-in comes back as it was.
        let original = original.as_compound().cloned().unwrap_or_default();
        let name = original.str("id").unwrap_or("stone");
        item.id = item_id(FLAT, name).ok_or_else(|| invalid("unknown stashed item"))?;
        item.tag = original.get("tag").cloned();
        tidy(item);
        return Ok(());
    }
    let original = (id, damage, tag.clone());
    let mut lossy = false;
    let name: String = if id == SPAWN_EGG {
        spawn_egg_up(&mut tag).unwrap_or_else(|| "pig_spawn_egg".to_owned())
    } else if id == BANNER {
        let color = tag
            .compound("BlockEntityTag")
            .and_then(|entity| entity.int("Base"))
            .map_or(damage, |base| base as i16);
        legacy::name(BANNER, color)
            .unwrap_or("white_banner")
            .to_owned()
    } else if damageable(id) {
        tag.insert("Damage", i32::from(damage));
        legacy::name(id, 0).unwrap_or("stone").to_owned()
    } else if id == FILLED_MAP {
        tag.insert("map", i32::from(damage));
        "filled_map".to_owned()
    } else if let Some(name) = legacy::name(id, damage) {
        name.to_owned()
    } else if id == SHRUB && damage == 0 {
        lossy = true;
        "dead_bush".to_owned()
    } else if let Some(name) = legacy::name(id, 0) {
        lossy = true;
        name.to_owned()
    } else {
        lossy = true;
        "stone".to_owned()
    };
    if id == BANNER || id == SHIELD {
        invert_banner_colors(&mut tag);
    }
    if let Some(display) = tag.compound_mut("display")
        && let Some(Nbt::String(name)) = display.get_mut("Name")
    {
        let json = legacy_text::to_json(name);
        lossy |= legacy_text::from_json(&json) != *name;
        *name = json;
    }
    lossy |= enchantments_up(&mut tag);
    for key in ["CanPlaceOn", "CanDestroy"] {
        lossy |= blocks_up(&mut tag, key);
    }
    if let Some(entity) = tag.compound_mut("BlockEntityTag")
        && let Some(Nbt::List(items)) = entity.get_mut("Items")
    {
        for nested in items.iter_mut().filter_map(Nbt::as_compound_mut) {
            nested_up(nested);
        }
    }
    item.id = item_id(FLAT, &name).ok_or_else(|| invalid("flattened item missing from 1.13"))?;
    if lossy {
        let (id, damage, tag_before) = original;
        let mut stash = Compound::new();
        stash.insert("id", i32::from(id));
        stash.insert("damage", damage);
        stash.insert("tag", tag_before);
        tag.insert(stash_key(LEGACY), stash);
    }
    item.tag = Some(Nbt::Compound(tag));
    tidy(item);
    Ok(())
}

/// A 1.13 stack for a 1.12 receiver.
pub(super) fn down(
    shared: &mut Shared,
    direction: Direction,
    item: &mut ItemStack,
) -> io::Result<()> {
    let _ = (shared, direction);
    let mut tag = match item.tag.take() {
        Some(Nbt::Compound(tag)) => tag,
        Some(_) => return Err(invalid("item tag is not a compound")),
        None => Compound::new(),
    };
    if let Some(original) = stash(&tag, &stash_key(LEGACY)) {
        // A 1.12 stack that 1.13 could not hold exactly comes back as it was.
        item.id = original.int("id").unwrap_or(1);
        item.damage = original.int("damage").unwrap_or(0) as i16;
        item.tag = original.get("tag").cloned();
        tidy(item);
        return Ok(());
    }
    let name =
        item_name(FLAT, item.id).ok_or_else(|| invalid(&format!("unknown item ID {}", item.id)))?;
    let original = tag.clone();
    let mut lossy = false;
    let (id, mut damage) = if let Some(mob) = name.strip_suffix("_spawn_egg") {
        spawn_egg_down(&mut tag, mob);
        (SPAWN_EGG, 0)
    } else if let Some(legacy) = legacy::legacy(name) {
        legacy
    } else {
        lossy = true;
        (1, 0)
    };
    if damageable(id) {
        damage = tag
            .remove("Damage")
            .and_then(|damage| damage.as_i64())
            .unwrap_or(0) as i16;
    }
    if id == FILLED_MAP {
        damage = tag.remove("map").and_then(|map| map.as_i64()).unwrap_or(0) as i16;
    }
    if id == BANNER || id == SHIELD {
        invert_banner_colors(&mut tag);
    }
    if let Some(display) = tag.compound_mut("display")
        && let Some(Nbt::String(text)) = display.get_mut("Name")
    {
        let legacy = legacy_text::from_json(text);
        lossy |= legacy_text::to_json(&legacy) != *text;
        *text = legacy;
    }
    lossy |= enchantments_down(&mut tag);
    for key in ["CanPlaceOn", "CanDestroy"] {
        lossy |= blocks_down(&mut tag, key);
    }
    if let Some(entity) = tag.compound_mut("BlockEntityTag")
        && let Some(Nbt::List(items)) = entity.get_mut("Items")
    {
        for nested in items.iter_mut().filter_map(Nbt::as_compound_mut) {
            nested_down(nested);
        }
    }
    item.id = i32::from(id);
    item.damage = damage;
    if lossy {
        if legacy::legacy(name).is_none() && !name.ends_with("_spawn_egg") {
            name_stand_in(&mut tag, LEGACY, name);
        }
        let mut stash = Compound::new();
        stash.insert("id", format!("minecraft:{name}"));
        stash.insert("tag", original);
        tag.insert(stash_key(FLAT), stash);
    }
    item.tag = Some(Nbt::Compound(tag));
    tidy(item);
    Ok(())
}

/// The mob's spawn egg, from `EntityTag.id`, which the egg item replaces.
fn spawn_egg_up(tag: &mut Compound) -> Option<String> {
    let entity = tag.compound_mut("EntityTag")?;
    let legacy = data::strip(entity.str("id")?).to_owned();
    let mob = map_entity(LEGACY, FLAT, &legacy)?;
    let egg = format!("{mob}_spawn_egg");
    item_id(FLAT, &egg)?;
    entity.remove("id");
    if entity.is_empty() {
        tag.remove("EntityTag");
    }
    Some(egg)
}

fn spawn_egg_down(tag: &mut Compound, mob: &str) {
    let Some(legacy) = map_entity(FLAT, LEGACY, mob) else {
        return;
    };
    if !tag.contains("EntityTag") {
        tag.insert("EntityTag", Compound::new());
    }
    if let Some(entity) = tag.compound_mut("EntityTag")
        && !entity.contains("id")
    {
        entity.insert("id", format!("minecraft:{legacy}"));
    }
}

/// An entity type's name across the flattening, by the entity mapping
/// (which knows the 1.13 renames).
fn map_entity(from: i32, to: i32, name: &str) -> Option<String> {
    let source = data::version(from).registry("entity_type")?;
    let id = source.id(name)?;
    let mapped = crate::mapping::try_mapping(from, to, "entity_type")?.get(id)?;
    Some(
        data::version(to)
            .registry("entity_type")?
            .name(mapped)?
            .to_owned(),
    )
}

/// Banner and shield colours count from black (dye order) in 1.12 and from
/// white (wool order) in 1.13; the conversion is its own inverse.
fn invert_banner_colors(tag: &mut Compound) {
    let Some(entity) = tag.compound_mut("BlockEntityTag") else {
        return;
    };
    if let Some(base) = entity.int("Base") {
        entity.insert("Base", 15 - base);
    }
    if let Some(Nbt::List(patterns)) = entity.get_mut("Patterns") {
        for pattern in patterns.iter_mut().filter_map(Nbt::as_compound_mut) {
            if let Some(color) = pattern.int("Color") {
                pattern.insert("Color", 15 - color);
            }
        }
    }
}

fn enchantment_name(protocol: i32, id: i32) -> Option<&'static str> {
    data::version(protocol).registry("enchantment")?.name(id)
}

fn enchantment_id(protocol: i32, name: &str) -> Option<i32> {
    data::version(protocol).registry("enchantment")?.id(name)
}

/// Rename an enchantment across the flattening through the registry
/// mapping; `None` when the receiver lacks it.
fn map_enchantment(from: i32, to: i32, id: i32) -> Option<i32> {
    crate::mapping::try_mapping(from, to, "enchantment")?.get(id)
}

/// `ench` (and `StoredEnchantments`) with numeric IDs to named IDs.
/// Returns whether an enchantment was dropped.
fn enchantments_up(tag: &mut Compound) -> bool {
    let mut lossy = false;
    let mut convert = |list: Vec<Nbt>| -> Vec<Nbt> {
        list.into_iter()
            .filter_map(|entry| {
                let entry = entry.as_compound()?;
                let mapped = entry
                    .int("id")
                    .and_then(|id| map_enchantment(LEGACY, FLAT, id))
                    .and_then(|id| enchantment_name(FLAT, id));
                let Some(name) = mapped else {
                    lossy = true;
                    return None;
                };
                let mut converted = Compound::new();
                converted.insert("id", format!("minecraft:{name}"));
                converted.insert("lvl", entry.int("lvl").unwrap_or(0) as i16);
                Some(Nbt::Compound(converted))
            })
            .collect()
    };
    if let Some(Nbt::List(list)) = tag.remove("ench") {
        tag.insert("Enchantments", convert(list));
    }
    if let Some(Nbt::List(list)) = tag.get_mut("StoredEnchantments") {
        *list = convert(std::mem::take(list));
    }
    lossy
}

fn enchantments_down(tag: &mut Compound) -> bool {
    let mut lossy = false;
    let mut convert = |list: Vec<Nbt>| -> Vec<Nbt> {
        list.into_iter()
            .filter_map(|entry| {
                let entry = entry.as_compound()?;
                let mapped = entry
                    .str("id")
                    .and_then(|name| enchantment_id(FLAT, name))
                    .and_then(|id| map_enchantment(FLAT, LEGACY, id));
                let Some(id) = mapped else {
                    lossy = true;
                    return None;
                };
                let mut converted = Compound::new();
                converted.insert("id", id as i16);
                converted.insert("lvl", entry.int("lvl").unwrap_or(0) as i16);
                Some(Nbt::Compound(converted))
            })
            .collect()
    };
    if let Some(Nbt::List(list)) = tag.remove("Enchantments") {
        tag.insert("ench", convert(list));
    }
    if let Some(Nbt::List(list)) = tag.get_mut("StoredEnchantments") {
        *list = convert(std::mem::take(list));
    }
    lossy
}

/// The 1.13 blocks a 1.12 block name stands for: every variant its item
/// had, or the same name when 1.13 kept it.
fn blocks_for_legacy(name: &str) -> Vec<String> {
    let name = data::strip(name);
    let legacy_block = data::version(LEGACY)
        .registry("block")
        .and_then(|blocks| blocks.id(name))
        .or_else(|| name.parse().ok());
    let flat_blocks = data::version(FLAT).registry("block");
    let mut names = Vec::new();
    if let Some(id) = legacy_block.and_then(|id| i16::try_from(id).ok()) {
        for damage in 0..16 {
            if let Some(variant) = legacy::name(id, damage)
                && flat_blocks.and_then(|blocks| blocks.id(variant)).is_some()
                && !names.iter().any(|known: &String| known == variant)
            {
                names.push(variant.to_owned());
            }
        }
    }
    if names.is_empty() && flat_blocks.and_then(|blocks| blocks.id(name)).is_some() {
        names.push(name.to_owned());
    }
    names
}

/// The 1.12 block a 1.13 block name was part of.
fn legacy_block(name: &str) -> Option<String> {
    let name = data::strip(name);
    let blocks = data::version(LEGACY).registry("block")?;
    if let Some((id, _)) = legacy::legacy(name)
        && let Some(block) = blocks.name(i32::from(id))
    {
        return Some(block.to_owned());
    }
    blocks.id(name).map(|_| name.to_owned())
}

/// Returns whether the list changed in a way the reverse cannot undo.
fn blocks_up(tag: &mut Compound, key: &str) -> bool {
    let Some(Nbt::List(list)) = tag.get_mut(key) else {
        return false;
    };
    let mut lossy = false;
    let mut converted: Vec<Nbt> = Vec::new();
    for entry in list.iter() {
        let Some(name) = entry.as_str() else {
            lossy = true;
            continue;
        };
        let names = blocks_for_legacy(name);
        lossy |= names.len() != 1;
        for name in names {
            let value = Nbt::String(format!("minecraft:{name}"));
            if !converted.contains(&value) {
                converted.push(value);
            }
        }
    }
    *list = converted;
    lossy
}

fn blocks_down(tag: &mut Compound, key: &str) -> bool {
    let Some(Nbt::List(list)) = tag.get_mut(key) else {
        return false;
    };
    let mut lossy = false;
    let mut converted: Vec<Nbt> = Vec::new();
    for entry in list.iter() {
        let Some(block) = entry.as_str().and_then(legacy_block) else {
            lossy = true;
            continue;
        };
        let value = Nbt::String(format!("minecraft:{block}"));
        if converted.contains(&value) {
            lossy = true;
        } else {
            converted.push(value);
        }
    }
    *list = converted;
    lossy
}

/// An item inside a block entity: `{id: "minecraft:wool", Damage: 14s}` to
/// `{id: "minecraft:red_wool"}`.
fn nested_up(item: &mut Compound) {
    let Some(name) = item.str("id").map(|name| data::strip(name).to_owned()) else {
        return;
    };
    let damage = item
        .remove("Damage")
        .and_then(|damage| damage.as_i64())
        .unwrap_or(0) as i16;
    let Some(id) = data::version(LEGACY)
        .registry("item")
        .and_then(|items| items.id(&name))
        .and_then(|id| i16::try_from(id).ok())
    else {
        return;
    };
    let flat = if damageable(id) {
        if damage != 0 {
            insert_nested_tag(item, "Damage", Nbt::Int(i32::from(damage)));
        }
        legacy::name(id, 0)
    } else {
        legacy::name(id, damage).or_else(|| legacy::name(id, 0))
    };
    item.insert("id", format!("minecraft:{}", flat.unwrap_or("stone")));
}

fn nested_down(item: &mut Compound) {
    let Some(name) = item.str("id").map(|name| data::strip(name).to_owned()) else {
        return;
    };
    let (id, mut damage) = legacy::legacy(&name).unwrap_or((1, 0));
    if damageable(id)
        && let Some(tag) = item.compound_mut("tag")
    {
        damage = tag
            .remove("Damage")
            .and_then(|damage| damage.as_i64())
            .unwrap_or(0) as i16;
    }
    let legacy_name = data::version(LEGACY)
        .registry("item")
        .and_then(|items| items.name(i32::from(id)))
        .unwrap_or("stone");
    item.insert("id", format!("minecraft:{legacy_name}"));
    item.insert("Damage", damage);
}

fn insert_nested_tag(item: &mut Compound, key: &str, value: Nbt) {
    if !item.contains("tag") {
        item.insert("tag", Compound::new());
    }
    if let Some(tag) = item.compound_mut("tag") {
        tag.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_variants_and_their_reverse() {
        assert_eq!(legacy::name(35, 14), Some("red_wool"));
        assert_eq!(legacy::legacy("red_wool"), Some((35, 14)));
        assert_eq!(legacy::legacy("stone"), Some((1, 0)));
        assert!(damageable(276));
        assert!(!damageable(1));
    }

    #[test]
    fn block_lists_expand_and_collapse() {
        assert_eq!(blocks_for_legacy("minecraft:wool").len(), 16);
        assert_eq!(blocks_for_legacy("stone").len(), 7);
        assert_eq!(legacy_block("minecraft:red_wool").as_deref(), Some("wool"));
        assert_eq!(legacy_block("stone").as_deref(), Some("stone"));
    }

    #[test]
    fn spawn_eggs_follow_entity_renames() {
        assert_eq!(
            map_entity(LEGACY, FLAT, "zombie_pigman").as_deref(),
            Some("zombie_pigman")
        );
        assert_eq!(
            map_entity(LEGACY, FLAT, "evocation_illager").as_deref(),
            Some("evoker")
        );
        assert_eq!(
            map_entity(FLAT, LEGACY, "evoker").as_deref(),
            Some("evocation_illager")
        );
    }
}
