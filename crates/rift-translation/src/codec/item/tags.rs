//! What changed in item tags between adjacent protocols after the
//! flattening and before data components (1.13 to 1.20.4):
//!
//! - 1.14: lore lines become JSON text components;
//! - 1.16: UUIDs become int arrays (`SkullOwner.Id`, attribute modifiers)
//!   and attribute names become namespaced snake case;
//! - 1.17: a filled map without a `map` number shows map 0 rather than
//!   nothing, so one is added (and taken away again);
//! - 1.20.2: `CustomPotionEffects` becomes `custom_potion_effects` with
//!   named effects and snake-case fields;
//! - every pair: JSON text the text codec converts (names, lore, book
//!   pages), and the item names of stacks held inside the tag (shulker box
//!   contents, bundles, crossbow projectiles).
use super::{
    convert::{item_id, item_name, map_item, readable, stash_key},
    legacy_text,
};
use crate::{
    Direction,
    codec::{Shared, text},
    data,
    nbt::{Compound, Nbt},
};
use std::io;

/// Pairs whose JSON text format changed in a way the text codec converts.
///
/// INTEGRATION: the text codec should say which pairs it changes JSON
/// text for (for example `text::json_changes(older, newer)`); until then
/// this lists the pairs known to change it (1.13 translation keys, 1.16
/// hex colours and hover contents, 1.20.3 component format).
fn json_text_changes(older: i32, newer: i32) -> bool {
    matches!((older, newer), (340, 393) | (578, 735) | (764, 765))
}

/// Returns whether the receiver cannot hold everything the tag said, so
/// the caller stashes the original.
pub(super) fn convert(
    shared: &mut Shared,
    direction: Direction,
    item: &str,
    tag: &mut Compound,
) -> io::Result<bool> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let (older, newer) = (source.min(target), source.max(target));
    let up = target > source;
    if json_text_changes(older, newer) {
        convert_text(shared, direction, tag)?;
    }
    match (older, newer) {
        (404, 477) => lore(tag, up),
        (578, 735) => {
            skull_owner(tag, up);
            attributes(tag, up);
        }
        (754, 755) => filled_map(tag, item, up, source),
        (763, 764) => potion_effects(tag, up),
        _ => {}
    }
    nested(shared, direction, tag)?;
    Ok(unknown_enchantments(tag, target))
}

/// Enchantments the receiver does not know (newer than it) would vanish
/// from the tooltip; list them in the lore instead, as `Name level`.
fn unknown_enchantments(tag: &mut Compound, target: i32) -> bool {
    let Some(registry) = data::version(target).registry("enchantment") else {
        return false;
    };
    let mut lines = Vec::new();
    for key in ["Enchantments", "StoredEnchantments"] {
        let Some(Nbt::List(list)) = tag.get_mut(key) else {
            continue;
        };
        list.retain(|entry| {
            let Some(entry) = entry.as_compound() else {
                return true;
            };
            let Some(id) = entry.str("id") else {
                return true;
            };
            if registry.id(id).is_some() {
                return true;
            }
            let level = entry.int("lvl").unwrap_or(1);
            lines.push(format!("{} {}", readable(id), roman(level)));
            false
        });
    }
    if lines.is_empty() {
        return false;
    }
    if !tag.contains("display") {
        tag.insert("display", Compound::new());
    }
    let Some(display) = tag.compound_mut("display") else {
        return true;
    };
    let mut lore = match display.remove("Lore") {
        Some(Nbt::List(lore)) => lore,
        _ => Vec::new(),
    };
    let json = target >= 477;
    for (index, line) in lines.into_iter().enumerate() {
        let line = if json {
            serde_json::json!({"text": line, "italic": false, "color": "gray"}).to_string()
        } else {
            format!("§7{line}")
        };
        lore.insert(index, Nbt::String(line));
    }
    display.insert("Lore", Nbt::List(lore));
    true
}

fn roman(level: i32) -> String {
    const NUMERALS: [&str; 10] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X"];
    usize::try_from(level - 1)
        .ok()
        .and_then(|index| NUMERALS.get(index))
        .map_or_else(|| level.to_string(), |numeral| (*numeral).to_owned())
}

fn convert_json(shared: &mut Shared, direction: Direction, value: &mut Nbt) -> io::Result<()> {
    if let Nbt::String(json) = value {
        *json = text::convert_json(shared, direction, json)?;
    }
    Ok(())
}

/// Names and lore (JSON on both sides from 1.14; names only, 1.13), and
/// written book pages.
fn convert_text(shared: &mut Shared, direction: Direction, tag: &mut Compound) -> io::Result<()> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let json_lore = source.min(target) >= 477;
    if let Some(display) = tag.compound_mut("display") {
        if let Some(name) = display.get_mut("Name") {
            convert_json(shared, direction, name)?;
        }
        if json_lore && let Some(Nbt::List(lore)) = display.get_mut("Lore") {
            for line in lore {
                convert_json(shared, direction, line)?;
            }
        }
    }
    if let Some(Nbt::List(pages)) = tag.get_mut("pages") {
        for page in pages {
            convert_json(shared, direction, page)?;
        }
    }
    if let Some(Nbt::Compound(pages)) = tag.get_mut("filtered_pages") {
        for (_, page) in pages.0.iter_mut() {
            convert_json(shared, direction, page)?;
        }
    }
    Ok(())
}

/// Lore lines: legacy text before 1.14, JSON after. The plain lines are
/// stashed on the way up so they come back exactly.
fn lore(tag: &mut Compound, up: bool) {
    let key = stash_key(404);
    let Some(display) = tag.compound_mut("display") else {
        return;
    };
    if !up && let Some(Nbt::List(saved)) = display.remove(&key) {
        display.insert("Lore", Nbt::List(saved));
        return;
    }
    let Some(Nbt::List(lines)) = display.get_mut("Lore") else {
        return;
    };
    let original = lines.clone();
    let mut lossy = false;
    for line in lines.iter_mut() {
        if let Nbt::String(text) = line {
            let converted = if up {
                legacy_text::to_json(text)
            } else {
                legacy_text::from_json(text)
            };
            let back = if up {
                legacy_text::from_json(&converted)
            } else {
                legacy_text::to_json(&converted)
            };
            lossy |= back != *text;
            *text = converted;
        }
    }
    if lossy && up {
        display.insert(key, Nbt::List(original));
    }
}

fn uuid_to_ints(most: i64, least: i64) -> Vec<i32> {
    vec![
        (most >> 32) as i32,
        most as i32,
        (least >> 32) as i32,
        least as i32,
    ]
}

fn ints_to_uuid(ints: &[i32]) -> Option<(i64, i64)> {
    let [a, b, c, d] = ints else {
        return None;
    };
    let most = (i64::from(*a) << 32) | i64::from(*b as u32);
    let least = (i64::from(*c) << 32) | i64::from(*d as u32);
    Some((most, least))
}

fn parse_uuid(text: &str) -> Option<(i64, i64)> {
    let hex: String = text.chars().filter(|char| *char != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let value = u128::from_str_radix(&hex, 16).ok()?;
    Some(((value >> 64) as i64, value as i64))
}

fn format_uuid(most: i64, least: i64) -> String {
    let value = (u128::from(most as u64) << 64) | u128::from(least as u64);
    let hex = format!("{value:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// A player head's owner ID: a string before 1.16, four ints after.
fn skull_owner(tag: &mut Compound, up: bool) {
    let Some(owner) = tag.compound_mut("SkullOwner") else {
        return;
    };
    match (owner.get("Id"), up) {
        (Some(Nbt::String(id)), true) => {
            if let Some((most, least)) = parse_uuid(id) {
                owner.insert("Id", Nbt::IntArray(uuid_to_ints(most, least)));
            }
        }
        (Some(Nbt::IntArray(ints)), false) => {
            if let Some((most, least)) = ints_to_uuid(ints) {
                owner.insert("Id", format_uuid(most, least));
            }
        }
        _ => {}
    }
}

/// "generic.maxHealth" to "minecraft:generic.max_health" and back.
fn attribute_name(name: &str, up: bool) -> String {
    if up {
        let mut snake = String::with_capacity(name.len() + 4);
        for char in name.chars() {
            if char.is_ascii_uppercase() {
                snake.push('_');
                snake.push(char.to_ascii_lowercase());
            } else {
                snake.push(char);
            }
        }
        format!("minecraft:{snake}")
    } else {
        let mut camel = String::with_capacity(name.len());
        let mut upper = false;
        for char in data::strip(name).chars() {
            if char == '_' {
                upper = true;
            } else if upper {
                camel.push(char.to_ascii_uppercase());
                upper = false;
            } else {
                camel.push(char);
            }
        }
        camel
    }
}

fn attributes(tag: &mut Compound, up: bool) {
    let Some(Nbt::List(modifiers)) = tag.get_mut("AttributeModifiers") else {
        return;
    };
    for modifier in modifiers.iter_mut().filter_map(Nbt::as_compound_mut) {
        if let Some(Nbt::String(name)) = modifier.get_mut("AttributeName") {
            *name = attribute_name(name, up);
        }
        if up {
            if let (Some(most), Some(least)) = (
                modifier.get("UUIDMost").and_then(Nbt::as_i64),
                modifier.get("UUIDLeast").and_then(Nbt::as_i64),
            ) {
                modifier.remove("UUIDMost");
                modifier.remove("UUIDLeast");
                modifier.insert("UUID", Nbt::IntArray(uuid_to_ints(most, least)));
            }
        } else if let Some(Nbt::IntArray(ints)) = modifier.get("UUID")
            && let Some((most, least)) = ints_to_uuid(ints)
        {
            modifier.remove("UUID");
            modifier.insert("UUIDMost", most);
            modifier.insert("UUIDLeast", least);
        }
    }
}

/// 1.17 clients show map 0 for a filled map without a number; older ones
/// showed an empty map. Add a number and remember that it was added.
fn filled_map(tag: &mut Compound, item: &str, up: bool, source: i32) {
    if item != "filled_map" {
        return;
    }
    if up {
        if !tag.contains("map") {
            tag.insert("map", 0i32);
            tag.insert(stash_key(source), 1i8);
        }
    } else if tag.remove(&stash_key(754)).is_some() {
        tag.remove("map");
    }
}

const EFFECT_FIELDS: [(&str, &str); 7] = [
    ("Amplifier", "amplifier"),
    ("Duration", "duration"),
    ("Ambient", "ambient"),
    ("ShowParticles", "show_particles"),
    ("ShowIcon", "show_icon"),
    ("HiddenEffect", "hidden_effect"),
    ("FactorCalculationData", "factor_calculation_data"),
];

fn rename_fields(effect: &mut Compound, up: bool) {
    for (old, new) in EFFECT_FIELDS {
        let (from, to) = if up { (old, new) } else { (new, old) };
        if let Some(value) = effect.remove(from) {
            effect.insert(to, value);
        }
    }
}

/// Custom potion effects: numeric IDs (the 1.20.1 registry ID, which
/// starts at one) and
/// capitalised fields before 1.20.2, named effects and snake case after.
fn potion_effects(tag: &mut Compound, up: bool) {
    let (from, to) = if up {
        ("CustomPotionEffects", "custom_potion_effects")
    } else {
        ("custom_potion_effects", "CustomPotionEffects")
    };
    let Some(Nbt::List(mut effects)) = tag.remove(from) else {
        return;
    };
    let effect_registry = data::version(763).registry("mob_effect");
    for effect in effects.iter_mut().filter_map(Nbt::as_compound_mut) {
        if up {
            if let Some(id) = effect.remove("Id").and_then(|id| id.as_i64())
                && let Some(name) = effect_registry.and_then(|r| r.name(id as i32))
            {
                effect.insert("id", format!("minecraft:{name}"));
            }
        } else if let Some(Nbt::String(name)) = effect.remove("id")
            && let Some(id) = effect_registry.and_then(|r| r.id(&name))
        {
            effect.insert("Id", id as i8);
        }
        rename_fields(effect, up);
    }
    tag.insert(to, Nbt::List(effects));
}

/// Stacks stored in the tag name their item; follow the item mapping.
fn nested(shared: &mut Shared, direction: Direction, tag: &mut Compound) -> io::Result<()> {
    let mut lists: Vec<&mut Vec<Nbt>> = Vec::new();
    let (entity, rest): (Option<&mut Nbt>, Vec<&mut Nbt>) = {
        let mut entity = None;
        let mut rest = Vec::new();
        for (key, value) in tag.0.iter_mut() {
            match key.as_str() {
                "BlockEntityTag" => entity = Some(value),
                "Items" | "ChargedProjectiles" => rest.push(value),
                _ => {}
            }
        }
        (entity, rest)
    };
    if let Some(Nbt::Compound(entity)) = entity
        && let Some(Nbt::List(items)) = entity.get_mut("Items")
    {
        lists.push(items);
    }
    for value in rest {
        if let Nbt::List(items) = value {
            lists.push(items);
        }
    }
    for items in lists {
        for item in items.iter_mut().filter_map(Nbt::as_compound_mut) {
            nested_stack(shared, direction, item)?;
        }
    }
    Ok(())
}

fn nested_stack(shared: &mut Shared, direction: Direction, item: &mut Compound) -> io::Result<()> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let Some(name) = item.str("id").map(|name| data::strip(name).to_owned()) else {
        return Ok(());
    };
    let Some(id) = item_id(source, &name) else {
        return Ok(());
    };
    let mapped = map_item(shared, direction, id)?;
    let target_name = item_name(target, mapped.id).unwrap_or("stone");
    if target_name != name {
        item.insert("id", format!("minecraft:{target_name}"));
    }
    if let Some(tag) = item.compound_mut("tag") {
        convert(shared, direction, target_name, tag)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enchantments_the_receiver_lacks_move_to_the_lore() {
        let mut soul_speed = Compound::new();
        soul_speed.insert("id", "minecraft:soul_speed");
        soul_speed.insert("lvl", 3i16);
        let mut sharpness = Compound::new();
        sharpness.insert("id", "minecraft:sharpness");
        sharpness.insert("lvl", 1i16);
        let mut tag = Compound::new();
        tag.insert(
            "Enchantments",
            vec![Nbt::Compound(soul_speed), Nbt::Compound(sharpness)],
        );
        assert!(unknown_enchantments(&mut tag, 578));
        assert_eq!(tag.list("Enchantments").unwrap().len(), 1);
        let lore = tag.compound("display").unwrap().list("Lore").unwrap();
        assert!(lore[0].as_str().unwrap().contains("Soul Speed III"));
        assert!(!unknown_enchantments(&mut tag, 578));
    }

    #[test]
    fn attribute_names_follow_the_1_16_rename() {
        assert_eq!(
            attribute_name("generic.maxHealth", true),
            "minecraft:generic.max_health"
        );
        assert_eq!(
            attribute_name("minecraft:generic.max_health", false),
            "generic.maxHealth"
        );
        assert_eq!(
            attribute_name("horse.jumpStrength", true),
            "minecraft:horse.jump_strength"
        );
    }

    #[test]
    fn uuids_round_trip_between_forms() {
        let text = "069a79f4-44e9-4726-a5be-fca90e38aaf5";
        let (most, least) = parse_uuid(text).unwrap();
        assert_eq!(format_uuid(most, least), text);
        let ints = uuid_to_ints(most, least);
        assert_eq!(ints_to_uuid(&ints), Some((most, least)));
        let mut tag = Compound::new();
        let mut owner = Compound::new();
        owner.insert("Id", text);
        tag.insert("SkullOwner", owner);
        skull_owner(&mut tag, true);
        assert!(matches!(
            tag.compound("SkullOwner").unwrap().get("Id"),
            Some(Nbt::IntArray(_))
        ));
        skull_owner(&mut tag, false);
        assert_eq!(tag.compound("SkullOwner").unwrap().str("Id"), Some(text));
    }

    #[test]
    fn lore_becomes_json_and_comes_back() {
        let mut tag = Compound::new();
        let mut display = Compound::new();
        display.insert("Lore", vec![Nbt::from("first"), Nbt::from("§cred")]);
        tag.insert("display", display);
        let before = tag.clone();
        lore(&mut tag, true);
        let lines = tag.compound("display").unwrap().list("Lore").unwrap();
        assert_eq!(lines[0].as_str(), Some(r#"{"text":"first"}"#));
        lore(&mut tag, false);
        assert_eq!(tag, before);
    }

    #[test]
    fn potion_effects_rename_ids_and_fields() {
        let mut effect = Compound::new();
        effect.insert("Id", 1i8);
        effect.insert("Duration", 100i32);
        let mut tag = Compound::new();
        tag.insert("CustomPotionEffects", vec![Nbt::Compound(effect)]);
        let before = tag.clone();
        potion_effects(&mut tag, true);
        let effects = tag.list("custom_potion_effects").unwrap();
        let effect = effects[0].as_compound().unwrap();
        assert_eq!(effect.str("id"), Some("minecraft:speed"));
        assert_eq!(effect.int("duration"), Some(100));
        potion_effects(&mut tag, false);
        assert_eq!(tag, before);
    }

    #[test]
    fn filled_maps_gain_and_lose_a_map_number() {
        let mut tag = Compound::new();
        filled_map(&mut tag, "filled_map", true, 754);
        assert_eq!(tag.int("map"), Some(0));
        filled_map(&mut tag, "filled_map", false, 755);
        assert!(tag.is_empty());
    }
}
