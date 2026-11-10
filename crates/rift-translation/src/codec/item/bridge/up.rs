//! A 1.20.4 item tag as a 1.20.5 component patch, in protocol 766's
//! network layouts. The rules follow vanilla's `ItemStackComponentizationFix`
//! (and `ItemStackCustomNameToOverrideComponentFix`): what a rule cannot
//! read stays in the tag, and what is left of the tag becomes
//! `custom_data`.
use super::{
    Bridge, OLD,
    tables::{self, BlockEntity},
};
use crate::{
    Direction,
    codec::{
        item::components::{Components, schema::Value},
        text,
    },
    data,
    nbt::{Compound, Nbt},
};
use std::io;

pub(super) fn components(cx: &mut Bridge, item: &str, mut tag: Compound) -> io::Result<Components> {
    let mut out = Components::default();
    let hide = match tag.remove("HideFlags") {
        Some(flags) => flags.as_i64().map_or(0, |flags| flags as i32),
        None => 0,
    };
    let hidden = |bit: i32| hide & bit != 0;
    // Bits with nothing to hide stay in `custom_data`, so the tag comes back
    // as it was.
    let mut shown = tables::HIDE_ENCHANTMENTS | tables::HIDE_ADDITIONAL;
    if let Some(damage) = take_int(&mut tag, "Damage")
        && damage != 0
    {
        out.insert("damage", Value::Int(damage));
    }
    if let Some(cost) = take_int(&mut tag, "RepairCost")
        && cost != 0
    {
        out.insert("repair_cost", Value::Int(cost));
    }
    if let Some(data) = take_int(&mut tag, "CustomModelData") {
        out.insert("custom_model_data", Value::Int(data));
    }
    if let Some(Nbt::Compound(state)) = tag.remove("BlockStateTag") {
        out.insert("block_state", block_state(&state));
    }
    if let Some(mut entity) = take_compound(&mut tag, "EntityTag") {
        // A 1.20.5 server cannot save entity data without an ID; the client
        // does not need one, and the stack converts back unchanged without.
        if cx.upward == Direction::Serverbound
            && !entity.contains("id")
            && let Some(id) = tables::entity_of(item)
        {
            entity.insert("id", format!("minecraft:{id}"));
        }
        cx.nested_stacks(&mut entity, true)?;
        out.insert("entity_data", Value::nbt(&Nbt::Compound(entity)));
    }
    if let Some(block_entity) = take_compound(&mut tag, "BlockEntityTag") {
        let mut rest = block_entity_tag(cx, item, block_entity, &mut out)?;
        cx.nested_stacks(&mut rest, true)?;
        if !rest.is_empty() {
            out.insert("block_entity_data", Value::nbt(&Nbt::Compound(rest)));
        }
    }
    if let Some(unbreakable) = tag.remove("Unbreakable")
        && unbreakable.as_i64().is_some_and(|value| value != 0)
    {
        out.insert(
            "unbreakable",
            Value::Bool(!hidden(tables::HIDE_UNBREAKABLE)),
        );
        shown |= tables::HIDE_UNBREAKABLE;
    }
    enchantments(
        cx,
        &mut tag,
        "Enchantments",
        "enchantments",
        hidden(tables::HIDE_ENCHANTMENTS),
        &mut out,
    );
    if item == "enchanted_book" {
        enchantments(
            cx,
            &mut tag,
            "StoredEnchantments",
            "stored_enchantments",
            hidden(tables::HIDE_ADDITIONAL),
            &mut out,
        );
    }
    // Vanilla's fixer gives a hidden dye without colour the default
    // leather colour; that colour would come back as a tag `color`, so the
    // bit stays in `custom_data` instead (it hides nothing either way).
    if let Some(mut display) = take_compound(&mut tag, "display") {
        if self::display(cx, item, &mut display, hide, &mut out)? {
            shown |= tables::HIDE_DYE;
        }
        if !display.is_empty() {
            tag.insert("display", display);
        }
    }
    if predicates(
        cx,
        &mut tag,
        "CanDestroy",
        "can_break",
        hidden(tables::HIDE_CAN_DESTROY),
        &mut out,
    ) {
        shown |= tables::HIDE_CAN_DESTROY;
    }
    if predicates(
        cx,
        &mut tag,
        "CanPlaceOn",
        "can_place_on",
        hidden(tables::HIDE_CAN_PLACE),
        &mut out,
    ) {
        shown |= tables::HIDE_CAN_PLACE;
    }
    if attribute_modifiers(cx, item, &mut tag, hidden(tables::HIDE_MODIFIERS), &mut out) {
        shown |= tables::HIDE_MODIFIERS;
    }
    if let Some(trim) = take_compound(&mut tag, "Trim") {
        match self::trim(cx, &trim, !hidden(tables::HIDE_UPGRADES)) {
            Some(value) => {
                out.insert("trim", value);
                shown |= tables::HIDE_UPGRADES;
            }
            None => {
                tag.insert("Trim", trim);
            }
        }
    }
    if hidden(tables::HIDE_ADDITIONAL) {
        out.insert("hide_additional_tooltip", Value::Unit);
    }
    match item {
        "crossbow" => {
            tag.remove("Charged");
            stacks(
                cx,
                &mut tag,
                "ChargedProjectiles",
                "charged_projectiles",
                &mut out,
            )?;
        }
        "bundle" => stacks(cx, &mut tag, "Items", "bundle_contents", &mut out)?,
        "filled_map" => map(&mut tag, &mut out),
        "writable_book" => writable_book(&mut tag, &mut out),
        "written_book" => written_book(cx, &mut tag, &mut out),
        "suspicious_stew" => suspicious_stew(cx, &mut tag, &mut out),
        "debug_stick" => {
            if let Some(state) = tag.remove("DebugProperty") {
                out.insert("debug_stick_state", Value::nbt(&state));
            }
        }
        "goat_horn" => {
            if let Some(name) = tag.str("instrument").map(str::to_owned)
                && let Some(id) = cx.new_id("instrument", &name)
            {
                tag.remove("instrument");
                out.insert("instrument", Value::Id(id));
            }
        }
        "knowledge_book" => {
            if let Some(recipes) = tag.remove("Recipes") {
                out.insert("recipes", Value::nbt(&recipes));
            }
        }
        "compass" => lodestone(&mut tag, &mut out),
        "firework_rocket" => {
            if let Some(mut fireworks) = take_compound(&mut tag, "Fireworks") {
                let explosions = match fireworks.remove("Explosions") {
                    Some(Nbt::List(explosions)) => explosions
                        .iter()
                        .filter_map(Nbt::as_compound)
                        .map(explosion)
                        .collect(),
                    _ => Vec::new(),
                };
                let flight = fireworks
                    .remove("Flight")
                    .and_then(|f| f.as_i64())
                    .unwrap_or(0);
                out.insert(
                    "fireworks",
                    Value::Struct(vec![
                        Value::Int(i32::from(flight as i8)),
                        Value::List(explosions),
                    ]),
                );
                if !fireworks.is_empty() {
                    tag.insert("Fireworks", fireworks);
                }
            }
        }
        "firework_star" => {
            if let Some(mut star) = take_compound(&mut tag, "Explosion") {
                out.insert("firework_explosion", explosion(&star));
                for key in ["Type", "Colors", "FadeColors", "Trail", "Flicker"] {
                    star.remove(key);
                }
                if !star.is_empty() {
                    tag.insert("Explosion", star);
                }
            }
        }
        "player_head" => {
            if let Some(owner) = tag.remove("SkullOwner") {
                out.insert("profile", profile(&owner));
            }
        }
        _ => {}
    }
    if tables::POTION_HOLDERS.contains(&item) {
        potion_contents(cx, &mut tag, &mut out);
    }
    if tables::BUCKETS.contains(&item) {
        let mut data = Compound::new();
        for key in tables::BUCKETED_MOB_TAGS {
            if let Some(value) = tag.remove(key) {
                data.insert(*key, value);
            }
        }
        if !data.is_empty() {
            out.insert("bucket_entity_data", Value::nbt(&Nbt::Compound(data)));
        }
    }
    if hide & !shown != 0 {
        tag.insert("HideFlags", Nbt::Int(hide & !shown));
    }
    if !tag.is_empty() {
        out.insert("custom_data", Value::nbt(&Nbt::Compound(tag)));
    }
    Ok(out)
}

/// A number, removed from the tag; anything else stays.
fn take_int(tag: &mut Compound, key: &str) -> Option<i32> {
    let value = tag.get(key)?.as_i64()?;
    tag.remove(key);
    Some(value as i32)
}

/// A compound, removed from the tag; anything else stays.
fn take_compound(tag: &mut Compound, key: &str) -> Option<Compound> {
    match tag.remove(key)? {
        Nbt::Compound(compound) => Some(compound),
        other => {
            tag.insert(key, other);
            None
        }
    }
}

/// The text of a string, number or boolean property value, as vanilla's
/// fixer writes it.
fn property_value(name: &str, value: &Nbt) -> Option<String> {
    Some(match value {
        Nbt::String(value) => value.clone(),
        Nbt::Byte(value) if tables::BOOLEAN_PROPERTIES.contains(&name) => (*value != 0).to_string(),
        Nbt::Byte(value) => value.to_string(),
        Nbt::Short(value) => value.to_string(),
        Nbt::Int(value) => value.to_string(),
        Nbt::Long(value) => value.to_string(),
        Nbt::Float(value) => format!("{value:?}"),
        Nbt::Double(value) => format!("{value:?}"),
        _ => return None,
    })
}

fn block_state(state: &Compound) -> Value {
    Value::List(
        state
            .iter()
            .filter_map(|(name, value)| {
                Some(Value::Struct(vec![
                    Value::String(name.to_owned()),
                    Value::String(property_value(name, value)?),
                ]))
            })
            .collect(),
    )
}

/// Take the components a `BlockEntityTag` holds out of it; returns the
/// rest, with the block entity's ID when anything is left.
fn block_entity_tag(
    cx: &mut Bridge,
    item: &str,
    mut tag: Compound,
    out: &mut Components,
) -> io::Result<Compound> {
    let id = match tag.remove("id") {
        Some(Nbt::String(id)) => Some(format!("minecraft:{}", data::strip(&id))),
        Some(other) => {
            tag.insert("id", other);
            None
        }
        None => tables::block_entity_of(item).map(|name| format!("minecraft:{name}")),
    };
    if let Some(lock) = tag.remove("Lock") {
        out.insert("lock", Value::nbt(&lock));
    }
    if let Some(table) = tag.remove("LootTable") {
        let mut loot = Compound::new();
        loot.insert("loot_table", table);
        let seed = tag
            .remove("LootTableSeed")
            .and_then(|seed| seed.as_i64())
            .unwrap_or(0);
        if seed != 0 {
            loot.insert("seed", seed);
        }
        out.insert("container_loot", Value::nbt(&Nbt::Compound(loot)));
    }
    match id.as_deref().map(tables::block_entity_kind) {
        Some(BlockEntity::Skull) => {
            if let Some(Nbt::String(sound)) = tag.remove("note_block_sound") {
                out.insert("note_block_sound", Value::String(sound));
            }
        }
        Some(BlockEntity::DecoratedPot) => {
            if let Some(Nbt::List(sherds)) = tag.get("sherds") {
                let ids: Option<Vec<Value>> = sherds
                    .iter()
                    .take(4)
                    .map(|sherd| Some(Value::Id(cx.new_id("item", sherd.as_str()?)?)))
                    .collect();
                if let Some(ids) = ids {
                    tag.remove("sherds");
                    out.insert("pot_decorations", Value::List(ids));
                }
            }
            if let Some(Nbt::Compound(stack)) = tag.get("item") {
                let stack = cx.nbt_stack_up(stack)?;
                tag.remove("item");
                if let Some(stack) = stack {
                    out.insert(
                        "container",
                        Value::List(vec![Value::Stack(Some(Box::new(stack)))]),
                    );
                }
            }
        }
        Some(BlockEntity::Banner) => {
            if let Some(Nbt::List(patterns)) = tag.get("Patterns")
                && let Some(patterns) = banner_patterns(cx, patterns)
            {
                tag.remove("Patterns");
                out.insert("banner_patterns", patterns);
            }
            if let Some(base) = tag.get("Base").and_then(Nbt::as_i64) {
                tag.remove("Base");
                out.insert("base_color", Value::Int(base as i32 & 15));
            }
        }
        Some(BlockEntity::Container) => {
            if let Some(Nbt::List(items)) = tag.remove("Items") {
                let mut slots: Vec<Value> = Vec::new();
                for item in items.iter().filter_map(Nbt::as_compound) {
                    let slot = item.int("Slot").unwrap_or(0) as u8 as usize;
                    let Some(stack) = cx.nbt_stack_up(item)? else {
                        continue;
                    };
                    if slots.len() <= slot {
                        slots.resize(slot + 1, Value::Stack(None));
                    }
                    slots[slot] = Value::Stack(Some(Box::new(stack)));
                }
                if !slots.is_empty() {
                    out.insert("container", Value::List(slots));
                }
            }
        }
        Some(BlockEntity::Beehive) => {
            if let Some(Nbt::List(bees)) = tag.remove("Bees") {
                let bees = bees
                    .iter()
                    .filter_map(Nbt::as_compound)
                    .map(|bee| {
                        let data = match bee.get("EntityData") {
                            Some(Nbt::Compound(data)) => data.clone(),
                            _ => Compound::new(),
                        };
                        Value::Struct(vec![
                            Value::nbt(&Nbt::Compound(data)),
                            Value::Int(bee.int("TicksInHive").unwrap_or(0)),
                            Value::Int(bee.int("MinOccupationTicks").unwrap_or(0)),
                        ])
                    })
                    .collect();
                out.insert("bees", Value::List(bees));
            }
        }
        Some(BlockEntity::Other) | None => {}
    }
    if !tag.is_empty()
        && let Some(id) = id
    {
        tag.insert("id", id);
    }
    Ok(tag)
}

fn banner_patterns(cx: &mut Bridge, patterns: &[Nbt]) -> Option<Value> {
    let mut layers = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let pattern = pattern.as_compound()?;
        let name = tables::pattern_name(pattern.str("Pattern")?)?;
        let id = cx.new_id("banner_pattern", name)?;
        let color = pattern.int("Color").unwrap_or(0);
        layers.push(Value::Struct(vec![
            Value::Id(id),
            Value::Int(if (0..16).contains(&color) { color } else { 0 }),
        ]));
    }
    Some(Value::List(layers))
}

fn enchantments(
    cx: &mut Bridge,
    tag: &mut Compound,
    key: &str,
    kind: &'static str,
    hidden: bool,
    out: &mut Components,
) {
    let list = match tag.remove(key) {
        Some(Nbt::List(list)) => Some(list),
        Some(other) => {
            tag.insert(key, other);
            None
        }
        None => None,
    };
    // Entries 1.20.5 cannot hold (unknown enchantments, levels outside
    // 0..=255, earlier repeats, which the last one overrides) stay in the
    // tag, so they come back. Entries without ID or level mean nothing but
    // the glint, as vanilla's fixer has it.
    let mut levels: Vec<(i32, i32, &Nbt)> = Vec::new();
    let mut rest = Vec::new();
    for entry in list.iter().flatten() {
        let Some((name, level)) = entry
            .as_compound()
            .and_then(|entry| Some((entry.str("id")?, entry.get("lvl").and_then(Nbt::as_i64)?)))
        else {
            continue;
        };
        let level = i32::try_from(level)
            .ok()
            .filter(|level| (0..=255).contains(level));
        let Some((id, level)) = cx.new_id("enchantment", name).zip(level) else {
            rest.push(entry.clone());
            continue;
        };
        if let Some(index) = levels.iter().position(|(known, _, _)| *known == id) {
            rest.push(levels.remove(index).2.clone());
        }
        levels.push((id, level, entry));
    }
    let levels: Vec<(i32, i32)> = levels
        .into_iter()
        .map(|(id, level, _)| (id, level))
        .collect();
    if !rest.is_empty() {
        tag.insert(key, Nbt::List(rest));
    }
    if !levels.is_empty() || hidden {
        out.insert(
            kind,
            Value::Struct(vec![
                Value::List(
                    levels
                        .iter()
                        .map(|(id, level)| Value::Struct(vec![Value::Id(*id), Value::Int(*level)]))
                        .collect(),
                ),
                Value::Bool(!hidden),
            ]),
        );
    }
    // Entries none of which name an enchantment still made 1.20.4 stacks
    // glint.
    if kind == "enchantments" && levels.is_empty() && list.is_some_and(|list| !list.is_empty()) {
        out.insert("enchantment_glint_override", Value::Bool(true));
    }
}

/// Names vanilla moved from `custom_name` to `item_name` with 1.20.5.
fn names_the_item(item: &str, name: &text::Component) -> bool {
    let text::component::Content::Translate { key, .. } = &name.content else {
        return false;
    };
    match item {
        "white_banner" => key == "block.minecraft.ominous_banner",
        "filled_map" => [
            "filled_map.buried_treasure",
            "filled_map.explorer_jungle",
            "filled_map.explorer_swamp",
            "filled_map.mansion",
            "filled_map.monument",
            "filled_map.trial_chambers",
            "filled_map.village_desert",
            "filled_map.village_plains",
            "filled_map.village_savanna",
            "filled_map.village_snowy",
            "filled_map.village_taiga",
        ]
        .contains(&key.as_str()),
        _ => false,
    }
}

/// Returns whether the display held a dye colour.
fn display(
    cx: &mut Bridge,
    item: &str,
    display: &mut Compound,
    hide: i32,
    out: &mut Components,
) -> io::Result<bool> {
    if let Some(Nbt::String(name)) = display.get("Name")
        && let Ok(component) = text::from_json_str(OLD, name)
    {
        let kind = if names_the_item(item, &component) {
            "item_name"
        } else {
            "custom_name"
        };
        let value = cx.component_to_text(component);
        display.remove("Name");
        out.insert(kind, Value::Nbt(value));
    }
    if let Some(Nbt::List(lore)) = display.get("Lore") {
        let lines: Option<Vec<Value>> = (lore.len() <= 256)
            .then(|| {
                lore.iter()
                    .map(|line| Some(Value::Nbt(cx.json_to_text(line.as_str()?).ok()?)))
                    .collect()
            })
            .flatten();
        if let Some(lines) = lines {
            display.remove("Lore");
            out.insert("lore", Value::List(lines));
        }
    }
    let color = match display.get("color").and_then(Nbt::as_i64) {
        Some(color) => {
            display.remove("color");
            Some(color as i32)
        }
        None => None,
    };
    if let Some(color) = color {
        out.insert(
            "dyed_color",
            Value::Struct(vec![
                Value::Int(color),
                Value::Bool(hide & tables::HIDE_DYE == 0),
            ]),
        );
    }
    if let Some(Nbt::String(key)) = display.get("LocName") {
        let name = cx.component_to_text(text::Component::translate(key.clone(), Vec::new()));
        display.remove("LocName");
        out.insert("item_name", Value::Nbt(name));
    }
    if item == "filled_map"
        && let Some(color) = display.get("MapColor").and_then(Nbt::as_i64)
    {
        display.remove("MapColor");
        out.insert("map_color", Value::Int(color as i32));
    }
    Ok(color.is_some())
}

/// `CanDestroy` and `CanPlaceOn`: block predicate strings. Returns whether
/// they were converted.
fn predicates(
    cx: &mut Bridge,
    tag: &mut Compound,
    key: &str,
    kind: &'static str,
    hidden: bool,
    out: &mut Components,
) -> bool {
    let Some(Nbt::List(list)) = tag.get(key) else {
        return false;
    };
    let predicates: Option<Vec<Value>> = list
        .iter()
        .map(|predicate| block_predicate(cx, predicate.as_str()?))
        .collect();
    let Some(predicates) = predicates else {
        return false;
    };
    tag.remove(key);
    out.insert(
        kind,
        Value::Struct(vec![Value::List(predicates), Value::Bool(!hidden)]),
    );
    true
}

/// One `block[state]{nbt}` predicate string of 1.20.4 as a 1.20.5 block
/// predicate.
fn block_predicate(cx: &mut Bridge, predicate: &str) -> Option<Value> {
    let state_at = predicate.find('[');
    let nbt_at = predicate.find('{');
    let end = [state_at, nbt_at]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(predicate.len());
    let block = predicate[..end].trim();
    let blocks = match block.strip_prefix('#') {
        Some(tag) => Value::Tag(namespaced(tag)),
        None => Value::Ids(vec![cx.new_id("block", block)?]),
    };
    let mut properties = Vec::new();
    if let Some(start) = state_at {
        let close = predicate[start..].find(']')? + start;
        for pair in predicate[start + 1..close].split(',') {
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            properties.push(Value::Struct(vec![
                Value::String(name.trim().to_owned()),
                Value::Either(true, Box::new(Value::String(value.trim().to_owned()))),
            ]));
        }
    }
    let nbt = match nbt_at {
        Some(start) => {
            let close = predicate.rfind('}')?;
            let parsed = text::snbt::parse(OLD, &predicate[start..=close]).ok()?;
            (parsed.kind() == 10).then(|| Value::some(Value::nbt(&parsed)))?
        }
        None => Value::none(),
    };
    Some(Value::Struct(vec![
        Value::some(blocks),
        if state_at.is_some() {
            Value::some(Value::List(properties))
        } else {
            Value::none()
        },
        nbt,
    ]))
}

fn namespaced(name: &str) -> String {
    if name.contains(':') {
        name.to_owned()
    } else {
        format!("minecraft:{name}")
    }
}

/// `AttributeModifiers` replace the item's defaults in both versions, but
/// an empty 1.20.5 list stands for the defaults of armour, whose prototype
/// sets none, and for no modifiers on weapons and tools, whose prototype
/// sets theirs. Hidden defaults are the prototype's, hidden. Returns
/// whether a component was set; what is not converted stays in the tag.
fn attribute_modifiers(
    cx: &mut Bridge,
    item: &str,
    tag: &mut Compound,
    hidden: bool,
    out: &mut Components,
) -> bool {
    let modifiers = match tag.get("AttributeModifiers") {
        Some(Nbt::List(list)) => {
            let converted: Option<Vec<Value>> = list
                .iter()
                .map(|modifier| attribute_modifier(cx, modifier.as_compound()?))
                .collect();
            match converted {
                Some(converted) => Some(converted),
                None => return false,
            }
        }
        Some(_) => return false,
        None => None,
    };
    let defaults = cx.default_modifiers(item);
    let modifiers = match modifiers {
        Some(modifiers) if !modifiers.is_empty() || !defaults.is_empty() || hidden => modifiers,
        // No modifiers on armour: 1.20.5 cannot say that, the defaults stay
        // and the list travels in `custom_data`.
        Some(_) => return false,
        None if hidden => defaults,
        None => return false,
    };
    tag.remove("AttributeModifiers");
    out.insert(
        "attribute_modifiers",
        Value::Struct(vec![Value::List(modifiers), Value::Bool(!hidden)]),
    );
    true
}

fn attribute_modifier(cx: &mut Bridge, modifier: &Compound) -> Option<Value> {
    let attribute = cx.new_id("attribute", modifier.str("AttributeName")?)?;
    let uuid = match modifier.get("UUID") {
        Some(Nbt::IntArray(ints)) if ints.len() == 4 => text::uuid_from_ints(ints),
        _ => return None,
    };
    let slot = match modifier.str("Slot") {
        Some(slot) => *tables::MODIFIER_SLOTS
            .iter()
            .find(|known| **known == slot)?,
        None => "any",
    };
    let operation = match modifier.int("Operation").unwrap_or(0) {
        operation @ 0..=2 => operation,
        _ => 0,
    };
    Some(Value::Struct(vec![
        Value::Id(attribute),
        Value::Uuid(uuid),
        Value::String(modifier.str("Name").unwrap_or("").to_owned()),
        Value::Double(modifier.get("Amount").and_then(Nbt::as_f64).unwrap_or(0.0)),
        Value::Int(operation),
        Value::Enum(slot),
    ]))
}

fn trim(cx: &mut Bridge, trim: &Compound, show: bool) -> Option<Value> {
    let material = cx.new_id("trim_material", trim.str("material")?)?;
    let pattern = cx.new_id("trim_pattern", trim.str("pattern")?)?;
    Some(Value::Struct(vec![
        Value::Id(material),
        Value::Id(pattern),
        Value::Bool(show),
    ]))
}

/// Nested stacks in 1.20.4 NBT under `key`, as a list of 1.20.5 stacks.
fn stacks(
    cx: &mut Bridge,
    tag: &mut Compound,
    key: &str,
    kind: &'static str,
    out: &mut Components,
) -> io::Result<()> {
    let Some(Nbt::List(items)) = tag.get(key) else {
        return Ok(());
    };
    let mut stacks = Vec::with_capacity(items.len());
    for item in items.clone().iter().filter_map(Nbt::as_compound) {
        if let Some(stack) = cx.nbt_stack_up(item)? {
            stacks.push(Value::Stack(Some(Box::new(stack))));
        }
    }
    tag.remove(key);
    if !stacks.is_empty() {
        out.insert(kind, Value::List(stacks));
    }
    Ok(())
}

fn map(tag: &mut Compound, out: &mut Components) {
    if let Some(id) = take_int(tag, "map") {
        out.insert("map_id", Value::Int(id));
    }
    if let Some(Nbt::List(decorations)) = tag.remove("Decorations") {
        let mut converted = Compound::new();
        for decoration in decorations.iter().filter_map(Nbt::as_compound) {
            let id = decoration.str("id").unwrap_or("").to_owned();
            if converted.contains(&id) {
                continue;
            }
            let kind = decoration.int("type").unwrap_or(0);
            let kind = tables::MAP_DECORATIONS
                .get(kind as usize)
                .unwrap_or(&tables::MAP_DECORATIONS[0]);
            let mut entry = Compound::new();
            entry.insert("type", format!("minecraft:{kind}"));
            entry.insert(
                "x",
                decoration.get("x").and_then(Nbt::as_f64).unwrap_or(0.0),
            );
            entry.insert(
                "z",
                decoration.get("z").and_then(Nbt::as_f64).unwrap_or(0.0),
            );
            entry.insert(
                "rotation",
                decoration.get("rot").and_then(Nbt::as_f64).unwrap_or(0.0) as f32,
            );
            converted.insert(id, entry);
        }
        if !converted.is_empty() {
            out.insert("map_decorations", Value::nbt(&Nbt::Compound(converted)));
        }
    }
    // What crafting a map into a locked or larger one leaves on the result.
    if tag
        .get("map_to_lock")
        .and_then(Nbt::as_i64)
        .is_some_and(|lock| lock != 0)
    {
        tag.remove("map_to_lock");
        out.insert("map_post_processing", Value::Int(0));
    } else if take_int(tag, "map_scale_direction").is_some() {
        out.insert("map_post_processing", Value::Int(1));
    }
}

fn effect(cx: &mut Bridge, effect: &Compound) -> Option<Value> {
    let id = cx.new_id("mob_effect", effect.str("id")?)?;
    Some(Value::Struct(vec![Value::Id(id), effect_details(effect)]))
}

fn effect_details(effect: &Compound) -> Value {
    let flag = |key: &str, default: bool| {
        effect
            .get(key)
            .and_then(Nbt::as_i64)
            .map_or(default, |value| value != 0)
    };
    let hidden = match effect.get("hidden_effect") {
        Some(Nbt::Compound(hidden)) => Value::some(effect_details(hidden)),
        _ => Value::none(),
    };
    let ambient = flag("ambient", false);
    Value::Struct(vec![
        Value::Int(i32::from(effect.int("amplifier").unwrap_or(0) as i8)),
        Value::Int(effect.int("duration").unwrap_or(0)),
        Value::Bool(ambient),
        Value::Bool(flag("show_particles", true)),
        Value::Bool(flag("show_icon", flag("show_particles", true))),
        hidden,
    ])
}

fn potion_contents(cx: &mut Bridge, tag: &mut Compound, out: &mut Components) {
    let mut potion = Value::none();
    if let Some(Nbt::String(name)) = tag.get("Potion") {
        if data::strip(name) == "empty" {
            tag.remove("Potion");
        } else if let Some(id) = cx.new_id("potion", &name.clone()) {
            tag.remove("Potion");
            potion = Value::some(Value::Id(id));
        }
    }
    let color = match take_int(tag, "CustomPotionColor") {
        Some(color) => Value::some(Value::Int(color)),
        None => Value::none(),
    };
    let mut effects = Vec::new();
    let mut any = false;
    if let Some(Nbt::List(list)) = tag.get("custom_potion_effects") {
        let converted: Option<Vec<Value>> = list
            .clone()
            .iter()
            .map(|entry| effect(cx, entry.as_compound()?))
            .collect();
        if let Some(converted) = converted {
            tag.remove("custom_potion_effects");
            effects = converted;
            any = true;
        }
    }
    if potion != Value::none() || color != Value::none() || any {
        out.insert(
            "potion_contents",
            Value::Struct(vec![potion, color, Value::List(effects)]),
        );
    }
}

/// Book pages and their filtered versions (`filtered_pages`, by page
/// index).
fn pages(tag: &mut Compound) -> Option<Vec<(String, Option<String>)>> {
    let Some(Nbt::List(pages)) = tag.get("pages") else {
        return None;
    };
    let pages: Vec<String> = pages
        .iter()
        .map(|page| page.as_str().unwrap_or("").to_owned())
        .collect();
    tag.remove("pages");
    let filtered = match tag.remove("filtered_pages") {
        Some(Nbt::Compound(filtered)) => filtered,
        _ => Compound::new(),
    };
    (!pages.is_empty()).then(|| {
        pages
            .into_iter()
            .enumerate()
            .map(|(index, page)| {
                let filtered = filtered.str(&index.to_string()).map(str::to_owned);
                (page, filtered)
            })
            .collect()
    })
}

fn writable_book(tag: &mut Compound, out: &mut Components) {
    if let Some(Nbt::List(list)) = tag.get("pages")
        && list.iter().any(|page| {
            page.as_str()
                .is_some_and(|page| page.chars().count() > 1024)
        })
    {
        return;
    }
    if let Some(pages) = pages(tag) {
        let optional = |text: Option<String>| match text {
            Some(text) => Value::some(Value::String(text)),
            None => Value::none(),
        };
        out.insert(
            "writable_book_content",
            Value::List(
                pages
                    .into_iter()
                    .map(|(raw, filtered)| {
                        Value::Struct(vec![Value::String(raw), optional(filtered)])
                    })
                    .collect(),
            ),
        );
    }
}

/// A page of a written book: JSON text, or (unresolved books) plain text.
fn page_text(cx: &mut Bridge, page: &str) -> Vec<u8> {
    match text::from_json_str(OLD, page) {
        Ok(component) => cx.component_to_text(component),
        Err(_) => cx.component_to_text(text::Component::text(page)),
    }
}

fn written_book(cx: &mut Bridge, tag: &mut Compound, out: &mut Components) {
    let pages = pages(tag).unwrap_or_default();
    let title = match tag.remove("title") {
        Some(Nbt::String(title)) => title,
        _ => String::new(),
    };
    let filtered_title = match tag.remove("filtered_title") {
        Some(Nbt::String(title)) => Value::some(Value::String(title)),
        _ => Value::none(),
    };
    let author = match tag.remove("author") {
        Some(Nbt::String(author)) => author,
        _ => String::new(),
    };
    let generation = take_int(tag, "generation").unwrap_or(0).clamp(0, 3);
    let resolved = tag
        .remove("resolved")
        .and_then(|resolved| resolved.as_i64())
        .is_some_and(|resolved| resolved != 0);
    let pages = pages
        .into_iter()
        .map(|(raw, filtered)| {
            let raw = Value::Nbt(page_text(cx, &raw));
            let filtered = match filtered {
                Some(filtered) => Value::some(Value::Nbt(page_text(cx, &filtered))),
                None => Value::none(),
            };
            Value::Struct(vec![raw, filtered])
        })
        .collect();
    out.insert(
        "written_book_content",
        Value::Struct(vec![
            Value::Struct(vec![
                Value::String(title.chars().take(32).collect()),
                filtered_title,
            ]),
            Value::String(author),
            Value::Int(generation),
            Value::List(pages),
            Value::Bool(resolved),
        ]),
    );
}

fn suspicious_stew(cx: &mut Bridge, tag: &mut Compound, out: &mut Components) {
    let Some(Nbt::List(list)) = tag.get("effects") else {
        return;
    };
    let effects: Option<Vec<Value>> = list
        .clone()
        .iter()
        .map(|entry| {
            let entry = entry.as_compound()?;
            let id = cx.new_id("mob_effect", entry.str("id")?)?;
            Some(Value::Struct(vec![
                Value::Id(id),
                Value::Int(entry.int("duration").unwrap_or(160)),
            ]))
        })
        .collect();
    if let Some(effects) = effects {
        tag.remove("effects");
        out.insert("suspicious_stew_effects", Value::List(effects));
    }
}

fn lodestone(tag: &mut Compound, out: &mut Components) {
    let pos = take_compound(tag, "LodestonePos");
    let dimension = match tag.remove("LodestoneDimension") {
        Some(Nbt::String(dimension)) => Some(dimension),
        _ => None,
    };
    if pos.is_none() && dimension.is_none() {
        return;
    }
    let tracked = tag
        .remove("LodestoneTracked")
        .and_then(|tracked| tracked.as_i64())
        .is_none_or(|tracked| tracked != 0);
    let target = match (pos, dimension) {
        (Some(pos), Some(dimension)) => {
            let coordinate = |key| pos.int(key).unwrap_or(0);
            Value::some(Value::Struct(vec![
                Value::String(dimension),
                Value::Long(tables::pack_position(
                    coordinate("X"),
                    coordinate("Y"),
                    coordinate("Z"),
                )),
            ]))
        }
        _ => Value::none(),
    };
    out.insert(
        "lodestone_tracker",
        Value::Struct(vec![target, Value::Bool(tracked)]),
    );
}

fn explosion(explosion: &Compound) -> Value {
    let colors = |key: &str| match explosion.get(key) {
        Some(Nbt::IntArray(colors)) => colors.iter().map(|color| Value::Int(*color)).collect(),
        Some(Nbt::List(colors)) => colors
            .iter()
            .filter_map(Nbt::as_i64)
            .map(|color| Value::Int(color as i32))
            .collect(),
        _ => Vec::new(),
    };
    let flag = |key: &str| {
        explosion
            .get(key)
            .and_then(Nbt::as_i64)
            .is_some_and(|v| v != 0)
    };
    let shape = match explosion.int("Type").unwrap_or(0) {
        shape @ 0..=4 => shape,
        _ => 0,
    };
    Value::Struct(vec![
        Value::Int(shape),
        Value::List(colors("Colors")),
        Value::List(colors("FadeColors")),
        Value::Bool(flag("Trail")),
        Value::Bool(flag("Flicker")),
    ])
}

/// Vanilla's check for names a profile may carry.
fn valid_player_name(name: &str) -> bool {
    name.chars().count() <= 16 && name.chars().all(|c| c > ' ' && c < '\u{7f}')
}

fn profile(owner: &Nbt) -> Value {
    let (name, id, properties) = match owner {
        Nbt::String(name) => (Some(name.clone()), None, Vec::new()),
        Nbt::Compound(owner) => {
            let id = match owner.get("Id") {
                Some(Nbt::IntArray(ints)) if ints.len() == 4 => Some(text::uuid_from_ints(ints)),
                _ => None,
            };
            let mut properties = Vec::new();
            if let Some(Nbt::Compound(all)) = owner.get("Properties") {
                for (property, values) in all.iter() {
                    for value in values
                        .as_list()
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(Nbt::as_compound)
                    {
                        let signature = match value.str("Signature") {
                            Some(signature) => Value::some(Value::String(signature.to_owned())),
                            None => Value::none(),
                        };
                        properties.push(Value::Struct(vec![
                            Value::String(property.to_owned()),
                            Value::String(value.str("Value").unwrap_or("").to_owned()),
                            signature,
                        ]));
                    }
                }
            }
            (owner.str("Name").map(str::to_owned), id, properties)
        }
        _ => (None, None, Vec::new()),
    };
    let name = name.filter(|name| valid_player_name(name));
    Value::Struct(vec![
        name.map_or_else(Value::none, |name| Value::some(Value::String(name))),
        id.map_or_else(Value::none, |id| Value::some(Value::Uuid(id))),
        Value::List(properties),
    ])
}
