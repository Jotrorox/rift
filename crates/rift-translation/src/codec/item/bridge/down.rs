//! A 1.20.5 component patch as a 1.20.4 item tag: the inverse of `up`.
//! Components 1.20.4 cannot express are left out here; the bridge stashes
//! whatever the tag would not bring back (see `Bridge::lost`).
use super::{
    Bridge, OLD,
    tables::{self, BlockEntity},
};
use crate::{
    codec::{
        item::components::{Components, schema::Value},
        text,
    },
    nbt::{Compound, Nbt},
};
use std::io;

pub(super) fn tag(cx: &mut Bridge, item: &str, components: &Components) -> io::Result<Compound> {
    // Keys vanilla has no component for travel in `custom_data`; they are
    // the base the converted keys are merged into.
    let mut tag = match components
        .get("custom_data")
        .map(Value::as_nbt)
        .transpose()?
    {
        Some(Some(Nbt::Compound(data))) => data,
        Some(_) => return Err(super::invalid("custom_data is not a compound")),
        None => Compound::new(),
    };
    let mut out = Compound::new();
    let mut hide = 0;
    let mut block_entity = Compound::new();
    let mut display = Compound::new();
    for component in &components.added {
        let value = &component.value;
        match component.kind {
            "damage" => drop(out.insert("Damage", int(value)?)),
            "repair_cost" => drop(out.insert("RepairCost", int(value)?)),
            "custom_model_data" => drop(out.insert("CustomModelData", int(value)?)),
            "block_state" => drop(out.insert("BlockStateTag", block_state(value)?)),
            "entity_data" => {
                let mut entity = nbt(value)?;
                if let Nbt::Compound(data) = &mut entity {
                    cx.nested_stacks(data, false)?;
                }
                out.insert("EntityTag", entity);
            }
            "block_entity_data" => {
                if let Nbt::Compound(mut data) = nbt(value)? {
                    cx.nested_stacks(&mut data, false)?;
                    merge(&mut block_entity, data);
                }
            }
            "unbreakable" => {
                out.insert("Unbreakable", Nbt::Byte(1));
                if value.as_bool() == Some(false) {
                    hide |= tables::HIDE_UNBREAKABLE;
                }
            }
            "enchantments" => {
                let (levels, show) = enchantments(cx, value)?;
                if !levels.is_empty() {
                    out.insert("Enchantments", Nbt::List(levels));
                }
                if !show {
                    hide |= tables::HIDE_ENCHANTMENTS;
                }
            }
            "stored_enchantments" => {
                let (levels, show) = enchantments(cx, value)?;
                if !levels.is_empty() {
                    out.insert("StoredEnchantments", Nbt::List(levels));
                }
                if !show {
                    hide |= tables::HIDE_ADDITIONAL;
                }
            }
            "custom_name" => drop(display.insert("Name", cx.text_to_json(value)?)),
            "item_name" if components.get("custom_name").is_none() => {
                // The closest 1.20.4 has: a custom name, upright as item
                // names show.
                let mut name = cx.text_component(value)?;
                name.style.italic.get_or_insert(false);
                display.insert("Name", text::to_json_string(OLD, &name));
            }
            "lore" => {
                let lines = value
                    .items()?
                    .iter()
                    .map(|line| cx.text_to_json(line).map(Nbt::String))
                    .collect::<io::Result<Vec<_>>>()?;
                display.insert("Lore", Nbt::List(lines));
            }
            "dyed_color" => {
                let fields = value.fields()?;
                display.insert("color", int(&fields[0])?);
                if fields[1].as_bool() == Some(false) {
                    hide |= tables::HIDE_DYE;
                }
            }
            "map_color" if item == "filled_map" => drop(display.insert("MapColor", int(value)?)),
            "can_break" | "can_place_on" => {
                let (key, bit) = match component.kind {
                    "can_break" => ("CanDestroy", tables::HIDE_CAN_DESTROY),
                    _ => ("CanPlaceOn", tables::HIDE_CAN_PLACE),
                };
                let fields = value.fields()?;
                let mut strings = Vec::new();
                for predicate in fields[0].items()? {
                    block_predicate(cx, predicate, &mut strings)?;
                }
                out.insert(key, Nbt::List(strings));
                if fields[1].as_bool() == Some(false) {
                    hide |= bit;
                }
            }
            "attribute_modifiers" => {
                // See `up::attribute_modifiers`: hidden defaults are just the
                // bit, an empty list is the defaults where the prototype has
                // none and no modifiers where it has some.
                let fields = value.fields()?;
                let show = fields[1].as_bool() != Some(false);
                let list = fields[0].items()?;
                let defaults = cx.default_modifiers(item);
                if !(list.is_empty() && defaults.is_empty())
                    && !(!show && super::same(&fields[0], &Value::List(defaults)))
                {
                    let modifiers = list
                        .iter()
                        .filter_map(|modifier| attribute_modifier(cx, modifier).transpose())
                        .collect::<io::Result<Vec<_>>>()?;
                    out.insert("AttributeModifiers", Nbt::List(modifiers));
                }
                if !show {
                    hide |= tables::HIDE_MODIFIERS;
                }
            }
            "trim" => {
                let fields = value.fields()?;
                if let (Some(material), Some(pattern)) = (
                    holder_name(cx, "trim_material", &fields[0]),
                    holder_name(cx, "trim_pattern", &fields[1]),
                ) {
                    let mut trim = Compound::new();
                    trim.insert("material", material);
                    trim.insert("pattern", pattern);
                    out.insert("Trim", trim);
                }
                if fields[2].as_bool() == Some(false) {
                    hide |= tables::HIDE_UPGRADES;
                }
            }
            "hide_additional_tooltip" => hide |= tables::HIDE_ADDITIONAL,
            // 1.20.4 cannot hide the name; it hides everything else.
            "hide_tooltip" => hide |= 0xff,
            "enchantment_glint_override"
                if value.as_bool() == Some(true)
                    && components.get("enchantments").is_none()
                    && !tag.contains("Enchantments") =>
            {
                // An entry that names no enchantment: 1.20.4 shows the
                // glint and nothing in the tooltip.
                out.insert(
                    "Enchantments",
                    Nbt::List(vec![Nbt::Compound(Compound::new())]),
                );
            }
            "charged_projectiles" => {
                let stacks = stacks(cx, value)?;
                if item == "crossbow" {
                    out.insert("Charged", Nbt::Byte(i8::from(!stacks.is_empty())));
                }
                out.insert("ChargedProjectiles", Nbt::List(stacks));
            }
            "bundle_contents" => drop(out.insert("Items", Nbt::List(stacks(cx, value)?))),
            "map_id" => drop(out.insert("map", int(value)?)),
            "map_decorations" => drop(out.insert("Decorations", map_decorations(value)?)),
            "map_post_processing" => match value.as_int() {
                Some(0) => drop(out.insert("map_to_lock", Nbt::Byte(1))),
                _ => drop(out.insert("map_scale_direction", Nbt::Int(1))),
            },
            "potion_contents" => potion_contents(cx, value, &mut out)?,
            "writable_book_content" => {
                let mut pages = Vec::new();
                let mut filtered = Compound::new();
                for (index, page) in value.items()?.iter().enumerate() {
                    let fields = page.fields()?;
                    pages.push(Nbt::String(string(&fields[0])?));
                    if let Some(text) = fields[1].option()? {
                        filtered.insert(index.to_string(), string(text)?);
                    }
                }
                out.insert("pages", Nbt::List(pages));
                if !filtered.is_empty() {
                    out.insert("filtered_pages", filtered);
                }
            }
            "written_book_content" => written_book(cx, value, &mut out)?,
            "suspicious_stew_effects" => {
                let effects = value
                    .items()?
                    .iter()
                    .filter_map(|entry| {
                        let fields = entry.fields().ok()?;
                        let mut effect = Compound::new();
                        effect.insert("id", cx.old_name("mob_effect", fields[0].as_int()?)?);
                        effect.insert("duration", Nbt::Int(fields[1].as_int()?));
                        Some(Nbt::Compound(effect))
                    })
                    .collect();
                out.insert("effects", Nbt::List(effects));
            }
            "debug_stick_state" => drop(out.insert("DebugProperty", nbt(value)?)),
            "bucket_entity_data" => {
                if let Nbt::Compound(data) = nbt(value)? {
                    merge(&mut out, data);
                }
            }
            "instrument" => {
                if let Some(name) = holder_name(cx, "instrument", value) {
                    out.insert("instrument", name);
                }
            }
            "recipes" => drop(out.insert("Recipes", nbt(value)?)),
            "lodestone_tracker" => {
                let fields = value.fields()?;
                if let Some(target) = fields[0].option()? {
                    let target = target.fields()?;
                    let (x, y, z) = match &target[1] {
                        Value::Long(packed) => tables::unpack_position(*packed),
                        _ => (0, 0, 0),
                    };
                    let mut pos = Compound::new();
                    pos.insert("X", x);
                    pos.insert("Y", y);
                    pos.insert("Z", z);
                    out.insert("LodestonePos", pos);
                    out.insert("LodestoneDimension", string(&target[0])?);
                }
                out.insert(
                    "LodestoneTracked",
                    Nbt::Byte(i8::from(fields[1].as_bool() == Some(true))),
                );
            }
            "firework_explosion" => drop(out.insert("Explosion", explosion(value)?)),
            "fireworks" => {
                let fields = value.fields()?;
                let mut fireworks = Compound::new();
                fireworks.insert(
                    "Flight",
                    Nbt::Byte(int(&fields[0])?.as_i64().unwrap_or(0) as i8),
                );
                let explosions = fields[1]
                    .items()?
                    .iter()
                    .map(explosion)
                    .collect::<io::Result<Vec<_>>>()?;
                fireworks.insert("Explosions", Nbt::List(explosions));
                out.insert("Fireworks", fireworks);
            }
            "profile" => drop(out.insert("SkullOwner", profile(value)?)),
            "note_block_sound" => drop(block_entity.insert("note_block_sound", string(value)?)),
            "banner_patterns" => {
                let patterns = value
                    .items()?
                    .iter()
                    .filter_map(|layer| {
                        let fields = layer.fields().ok()?;
                        let name = holder_name(cx, "banner_pattern", &fields[0])?;
                        let mut pattern = Compound::new();
                        pattern.insert("Pattern", tables::pattern_code(&name)?);
                        pattern.insert("Color", Nbt::Int(fields[1].as_int()?));
                        Some(Nbt::Compound(pattern))
                    })
                    .collect();
                block_entity.insert("Patterns", Nbt::List(patterns));
            }
            "base_color" => drop(block_entity.insert("Base", int(value)?)),
            "pot_decorations" => {
                let sherds = value
                    .items()?
                    .iter()
                    .filter_map(|id| cx.old_name("item", id.as_int()?).map(Nbt::String))
                    .collect();
                block_entity.insert("sherds", Nbt::List(sherds));
            }
            "container" => {
                let pot = tables::block_entity_of(item)
                    .is_some_and(|id| tables::block_entity_kind(id) == BlockEntity::DecoratedPot);
                let mut items = Vec::new();
                for (slot, stack) in value.items()?.iter().enumerate() {
                    let Value::Stack(Some(stack)) = stack else {
                        continue;
                    };
                    let Some(mut nbt) = cx.nbt_stack_down(stack)? else {
                        continue;
                    };
                    if pot {
                        block_entity.insert("item", nbt);
                        break;
                    }
                    let mut slotted = Compound::new();
                    slotted.insert("Slot", Nbt::Byte(slot as u8 as i8));
                    slotted.0.append(&mut nbt.0);
                    items.push(Nbt::Compound(slotted));
                }
                if !pot {
                    block_entity.insert("Items", Nbt::List(items));
                }
            }
            "bees" => {
                let bees = value
                    .items()?
                    .iter()
                    .map(|bee| {
                        let fields = bee.fields()?;
                        let mut converted = Compound::new();
                        converted.insert("EntityData", nbt(&fields[0])?);
                        converted.insert("TicksInHive", int(&fields[1])?);
                        converted.insert("MinOccupationTicks", int(&fields[2])?);
                        Ok(Nbt::Compound(converted))
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                block_entity.insert("Bees", Nbt::List(bees));
            }
            "lock" => drop(block_entity.insert("Lock", nbt(value)?)),
            "container_loot" => {
                if let Nbt::Compound(loot) = nbt(value)? {
                    if let Some(table) = loot.get("loot_table") {
                        block_entity.insert("LootTable", table.clone());
                    }
                    if let Some(seed) = loot.get("seed").and_then(Nbt::as_i64) {
                        block_entity.insert("LootTableSeed", Nbt::Long(seed));
                    }
                }
            }
            // `custom_data` is the base; the rest has no 1.20.4 equivalent.
            _ => {}
        }
    }
    if components.get("damage").is_none()
        && !tag.contains("Damage")
        && tables::max_damage(item).is_some()
    {
        // 1.20.4 stacks that wear out always carry their damage.
        out.insert("Damage", Nbt::Int(0));
    }
    if !block_entity.is_empty() {
        // The ID only names the block entity in 1.20.5; 1.20.4 takes it
        // from the item.
        block_entity.remove("id");
        if !block_entity.is_empty() {
            out.insert("BlockEntityTag", block_entity);
        }
    }
    if !display.is_empty() {
        out.insert("display", display);
    }
    // Bits that hid nothing came in `custom_data`.
    if let Some(&Nbt::Int(rest)) = tag.get("HideFlags") {
        tag.remove("HideFlags");
        hide |= rest;
    }
    if hide != 0 {
        out.insert("HideFlags", Nbt::Int(hide));
    }
    merge(&mut tag, out);
    Ok(tag)
}

/// Merge `from` into `into`: compounds key by key, lists appended, other
/// values replaced.
fn merge(into: &mut Compound, from: Compound) {
    for (key, value) in from.0 {
        match (into.get_mut(&key), value) {
            (Some(Nbt::Compound(existing)), Nbt::Compound(value)) => merge(existing, value),
            (Some(Nbt::List(existing)), Nbt::List(mut value))
                if existing
                    .first()
                    .zip(value.first())
                    .is_none_or(|(a, b)| a.kind() == b.kind()) =>
            {
                existing.append(&mut value)
            }
            (_, value) => drop(into.insert(key, value)),
        }
    }
}

fn int(value: &Value) -> io::Result<Nbt> {
    value
        .as_int()
        .map(Nbt::Int)
        .ok_or_else(|| super::invalid("component value is not a number"))
}

fn string(value: &Value) -> io::Result<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| super::invalid("component value is not a string"))
}

fn nbt(value: &Value) -> io::Result<Nbt> {
    value
        .as_nbt()?
        .ok_or_else(|| super::invalid("missing NBT in a component"))
}

/// The 1.20.4 name of a holder's registry entry; `None` for an inline
/// value, which 1.20.4 cannot name.
fn holder_name(cx: &mut Bridge, registry: &'static str, value: &Value) -> Option<String> {
    match value {
        Value::Id(id) => cx.old_name(registry, *id),
        _ => None,
    }
}

fn block_state(value: &Value) -> io::Result<Nbt> {
    let mut state = Compound::new();
    for property in value.items()? {
        let fields = property.fields()?;
        state.insert(string(&fields[0])?, string(&fields[1])?);
    }
    Ok(Nbt::Compound(state))
}

fn enchantments(cx: &mut Bridge, value: &Value) -> io::Result<(Vec<Nbt>, bool)> {
    let fields = value.fields()?;
    let levels = fields[0]
        .items()?
        .iter()
        .filter_map(|entry| {
            let fields = entry.fields().ok()?;
            let mut enchantment = Compound::new();
            enchantment.insert("id", cx.old_name("enchantment", fields[0].as_int()?)?);
            enchantment.insert("lvl", Nbt::Short(fields[1].as_int()? as i16));
            Some(Nbt::Compound(enchantment))
        })
        .collect();
    Ok((levels, fields[1].as_bool() != Some(false)))
}

/// A 1.20.5 block predicate as 1.20.4 predicate strings: one per block.
fn block_predicate(cx: &mut Bridge, predicate: &Value, out: &mut Vec<Nbt>) -> io::Result<()> {
    let fields = predicate.fields()?;
    let mut suffix = String::new();
    if let Some(properties) = fields[1].option()? {
        let mut pairs = Vec::new();
        for property in properties.items()? {
            let fields = property.fields()?;
            // Ranges have no 1.20.4 form; the lower bound stands in.
            let value = match &fields[1] {
                Value::Either(true, exact) => string(exact)?,
                Value::Either(false, range) => match range.fields()?[0].option()? {
                    Some(min) => string(min)?,
                    None => continue,
                },
                _ => continue,
            };
            pairs.push(format!("{}={value}", string(&fields[0])?));
        }
        suffix.push('[');
        suffix.push_str(&pairs.join(","));
        suffix.push(']');
    }
    if let Some(nbt) = fields[2].option()? {
        suffix.push_str(&text::snbt::print(OLD, &self::nbt(nbt)?));
    }
    match fields[0].option()? {
        Some(Value::Tag(tag)) => out.push(Nbt::String(format!("#{tag}{suffix}"))),
        Some(Value::Ids(ids)) => {
            for id in ids {
                if let Some(name) = cx.old_name("block", *id) {
                    out.push(Nbt::String(format!("{name}{suffix}")));
                }
            }
        }
        // Any block: 1.20.4 has no predicate for that.
        _ => {}
    }
    Ok(())
}

fn attribute_modifier(cx: &mut Bridge, modifier: &Value) -> io::Result<Option<Nbt>> {
    let fields = modifier.fields()?;
    let Some(attribute) = fields[0]
        .as_int()
        .and_then(|id| cx.old_name("attribute", id))
    else {
        return Ok(None);
    };
    let Value::Uuid(uuid) = fields[1] else {
        return Err(super::invalid("attribute modifier without UUID"));
    };
    let mut converted = Compound::new();
    converted.insert("AttributeName", attribute);
    converted.insert("Name", string(&fields[2])?);
    converted.insert(
        "Amount",
        Nbt::Double(match fields[3] {
            Value::Double(amount) => amount,
            _ => 0.0,
        }),
    );
    converted.insert("Operation", int(&fields[4])?);
    converted.insert("UUID", Nbt::IntArray(text::uuid_ints(uuid).to_vec()));
    match &fields[5] {
        Value::Enum(slot) if tables::MODIFIER_SLOTS.contains(slot) => {
            converted.insert("Slot", *slot);
        }
        // `any` is a modifier without slot; `hand`, `armor` and `body`
        // have no 1.20.4 form and apply everywhere there.
        _ => {}
    }
    Ok(Some(Nbt::Compound(converted)))
}

fn stacks(cx: &mut Bridge, value: &Value) -> io::Result<Vec<Nbt>> {
    let mut stacks = Vec::new();
    for stack in value.items()? {
        if let Value::Stack(Some(stack)) = stack
            && let Some(nbt) = cx.nbt_stack_down(stack)?
        {
            stacks.push(Nbt::Compound(nbt));
        }
    }
    Ok(stacks)
}

fn map_decorations(value: &Value) -> io::Result<Nbt> {
    let Nbt::Compound(decorations) = nbt(value)? else {
        return Err(super::invalid("map decorations are not a compound"));
    };
    let mut list = Vec::new();
    for (id, decoration) in decorations.iter() {
        let Some(decoration) = decoration.as_compound() else {
            continue;
        };
        let kind = decoration.str("type").map_or("player", crate::data::strip);
        let kind = tables::MAP_DECORATIONS
            .iter()
            .position(|known| *known == kind)
            .unwrap_or(0);
        let mut converted = Compound::new();
        converted.insert("id", id);
        converted.insert("type", Nbt::Byte(kind as i8));
        converted.insert(
            "x",
            decoration.get("x").and_then(Nbt::as_f64).unwrap_or(0.0),
        );
        converted.insert(
            "z",
            decoration.get("z").and_then(Nbt::as_f64).unwrap_or(0.0),
        );
        converted.insert(
            "rot",
            decoration
                .get("rotation")
                .and_then(Nbt::as_f64)
                .unwrap_or(0.0),
        );
        list.push(Nbt::Compound(converted));
    }
    Ok(Nbt::List(list))
}

fn effect_details(details: &Value, effect: &mut Compound) -> io::Result<()> {
    let fields = details.fields()?;
    effect.insert(
        "amplifier",
        Nbt::Byte(fields[0].as_int().unwrap_or(0) as i8),
    );
    effect.insert("duration", int(&fields[1])?);
    effect.insert(
        "ambient",
        Nbt::Byte(i8::from(fields[2].as_bool() == Some(true))),
    );
    effect.insert(
        "show_particles",
        Nbt::Byte(i8::from(fields[3].as_bool() == Some(true))),
    );
    effect.insert(
        "show_icon",
        Nbt::Byte(i8::from(fields[4].as_bool() == Some(true))),
    );
    if let Some(hidden) = fields[5].option()? {
        let mut converted = Compound::new();
        if let Some(id) = effect.get("id") {
            converted.insert("id", id.clone());
        }
        effect_details(hidden, &mut converted)?;
        effect.insert("hidden_effect", converted);
    }
    Ok(())
}

fn potion_contents(cx: &mut Bridge, value: &Value, out: &mut Compound) -> io::Result<()> {
    let fields = value.fields()?;
    if let Some(potion) = fields[0].option()?
        && let Some(name) = potion.as_int().and_then(|id| cx.old_name("potion", id))
    {
        out.insert("Potion", name);
    }
    if let Some(color) = fields[1].option()? {
        out.insert("CustomPotionColor", int(color)?);
    }
    let mut effects = Vec::new();
    for entry in fields[2].items()? {
        let entry = entry.fields()?;
        let Some(name) = entry[0]
            .as_int()
            .and_then(|id| cx.old_name("mob_effect", id))
        else {
            continue;
        };
        let mut effect = Compound::new();
        effect.insert("id", name);
        effect_details(&entry[1], &mut effect)?;
        effects.push(Nbt::Compound(effect));
    }
    if !effects.is_empty() {
        out.insert("custom_potion_effects", Nbt::List(effects));
    }
    Ok(())
}

fn written_book(cx: &mut Bridge, value: &Value, out: &mut Compound) -> io::Result<()> {
    let fields = value.fields()?;
    let title = fields[0].fields()?;
    out.insert("title", string(&title[0])?);
    if let Some(filtered) = title[1].option()? {
        out.insert("filtered_title", string(filtered)?);
    }
    out.insert("author", string(&fields[1])?);
    if fields[2].as_int() != Some(0) {
        out.insert("generation", int(&fields[2])?);
    }
    let mut pages = Vec::new();
    let mut filtered = Compound::new();
    for (index, page) in fields[3].items()?.iter().enumerate() {
        let page = page.fields()?;
        pages.push(Nbt::String(cx.text_to_json(&page[0])?));
        if let Some(text) = page[1].option()? {
            filtered.insert(index.to_string(), cx.text_to_json(text)?);
        }
    }
    if !pages.is_empty() {
        out.insert("pages", Nbt::List(pages));
    }
    if !filtered.is_empty() {
        out.insert("filtered_pages", filtered);
    }
    if fields[4].as_bool() == Some(true) {
        out.insert("resolved", Nbt::Byte(1));
    }
    Ok(())
}

fn explosion(value: &Value) -> io::Result<Nbt> {
    let fields = value.fields()?;
    let colors = |value: &Value| -> io::Result<Nbt> {
        Ok(Nbt::IntArray(
            value.items()?.iter().filter_map(Value::as_int).collect(),
        ))
    };
    let mut explosion = Compound::new();
    explosion.insert("Type", Nbt::Byte(fields[0].as_int().unwrap_or(0) as i8));
    explosion.insert("Colors", colors(&fields[1])?);
    // As 1.20.4's recipes write them: what is empty or false is left out.
    if !fields[2].items()?.is_empty() {
        explosion.insert("FadeColors", colors(&fields[2])?);
    }
    if fields[3].as_bool() == Some(true) {
        explosion.insert("Trail", Nbt::Byte(1));
    }
    if fields[4].as_bool() == Some(true) {
        explosion.insert("Flicker", Nbt::Byte(1));
    }
    Ok(Nbt::Compound(explosion))
}

fn profile(value: &Value) -> io::Result<Nbt> {
    let fields = value.fields()?;
    let name = fields[0].option()?.map(string).transpose()?;
    let id = match fields[1].option()? {
        Some(Value::Uuid(id)) => Some(*id),
        _ => None,
    };
    let properties = fields[2].items()?;
    if let (Some(name), None, true) = (&name, id, properties.is_empty()) {
        return Ok(Nbt::String(name.clone()));
    }
    let mut owner = Compound::new();
    if let Some(id) = id {
        owner.insert("Id", Nbt::IntArray(text::uuid_ints(id).to_vec()));
    }
    if let Some(name) = name {
        owner.insert("Name", name);
    }
    if !properties.is_empty() {
        let mut all = Compound::new();
        for property in properties {
            let fields = property.fields()?;
            let mut entry = Compound::new();
            entry.insert("Value", string(&fields[1])?);
            if let Some(signature) = fields[2].option()? {
                entry.insert("Signature", string(signature)?);
            }
            let key = string(&fields[0])?;
            match all.get_mut(&key) {
                Some(Nbt::List(list)) => list.push(Nbt::Compound(entry)),
                _ => drop(all.insert(key, Nbt::List(vec![Nbt::Compound(entry)]))),
            }
        }
        owner.insert("Properties", all);
    }
    Ok(Nbt::Compound(owner))
}
