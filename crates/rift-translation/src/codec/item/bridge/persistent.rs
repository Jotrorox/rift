//! Protocol 766's component patches in their persistent (codec) form, as
//! NBT: what a 1.20.5 `show_item` hover event carries (`components`), as
//! opposed to the network layouts the rest of the bridge works with.
//!
//! Only the components the bridge produces from 1.20.4 tags are covered,
//! with the field names of 1.20.6's codecs. A component this cannot encode
//! or decode is left out of the hover; the hover still shows the item.
use super::Bridge;
use crate::{
    codec::{
        item::{
            ItemStack,
            components::{Components, schema::Value},
            convert::{item_id, item_name},
        },
        text,
    },
    data,
    nbt::{Compound, Nbt},
};
use std::io;

/// Dye colours by their 1.20.5 index.
const DYES: &[&str] = &[
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

const OPERATIONS: &[&str] = &["add_value", "add_multiplied_base", "add_multiplied_total"];

/// A patch in persistent form: `minecraft:<type>` keys, `!minecraft:<type>`
/// for removed types.
pub(super) fn encode(cx: &mut Bridge, patch: &Components) -> Compound {
    let mut out = Compound::new();
    for component in &patch.added {
        if let Some(value) = encode_component(cx, component.kind, &component.value) {
            out.insert(format!("minecraft:{}", component.kind), value);
        }
    }
    for kind in &patch.removed {
        out.insert(format!("!minecraft:{kind}"), Compound::new());
    }
    out
}

/// The reverse of [`encode`]; what it cannot read is left out.
pub(super) fn decode(cx: &mut Bridge, components: &Compound) -> Components {
    let mut patch = Components::default();
    let layout = match crate::codec::item::components::layouts::Layout::of(super::NEW) {
        Ok(layout) => layout,
        Err(_) => return patch,
    };
    for (key, value) in components.iter() {
        let (removed, name) = match key.strip_prefix('!') {
            Some(name) => (true, name),
            None => (false, key),
        };
        let Some((id, _)) = layout.find(data::strip(name)) else {
            continue;
        };
        let (kind, _) = layout.get(id).expect("found by name");
        if removed {
            patch.removed.push(kind);
        } else if let Some(value) = decode_component(cx, kind, value) {
            patch.insert(kind, value);
        }
    }
    patch
}

fn show(compound: &mut Compound, show: &Value) {
    if show.as_bool() == Some(false) {
        compound.insert("show_in_tooltip", Nbt::Byte(0));
    }
}

fn shown(compound: &Compound) -> Value {
    Value::Bool(
        compound
            .get("show_in_tooltip")
            .and_then(Nbt::as_i64)
            .is_none_or(|show| show != 0),
    )
}

fn namespaced(name: &str) -> String {
    format!("minecraft:{}", data::strip(name))
}

fn text_json(value: &Value) -> Option<String> {
    let tag = value.as_nbt().ok()??;
    let component = text::from_nbt(super::NEW, &tag).ok()?;
    Some(text::to_json_string(super::NEW, &component))
}

fn json_text(json: &Nbt) -> Option<Value> {
    let component = text::from_json_str(super::NEW, json.as_str()?).ok()?;
    Some(Value::Nbt(
        text::to_nbt(super::NEW, &component).to_bytes(false),
    ))
}

fn uuid(value: &Value) -> Option<Nbt> {
    match value {
        Value::Uuid(uuid) => Some(Nbt::IntArray(text::uuid_ints(*uuid).to_vec())),
        _ => None,
    }
}

fn read_uuid(nbt: &Nbt) -> Option<Value> {
    match nbt {
        Nbt::IntArray(ints) if ints.len() == 4 => Some(Value::Uuid(text::uuid_from_ints(ints))),
        _ => None,
    }
}

fn encode_component(cx: &mut Bridge, kind: &str, value: &Value) -> Option<Nbt> {
    Some(match kind {
        "damage" | "repair_cost" | "custom_model_data" | "map_id" | "map_color" => {
            Nbt::Int(value.as_int()?)
        }
        "enchantment_glint_override" => Nbt::Byte(i8::from(value.as_bool()?)),
        "hide_additional_tooltip" | "hide_tooltip" | "fire_resistant" => {
            Nbt::Compound(Compound::new())
        }
        "unbreakable" => {
            let mut unbreakable = Compound::new();
            show(&mut unbreakable, value);
            Nbt::Compound(unbreakable)
        }
        "custom_name" | "item_name" => Nbt::String(text_json(value)?),
        "lore" => Nbt::List(
            value
                .items()
                .ok()?
                .iter()
                .map(|line| text_json(line).map(Nbt::String))
                .collect::<Option<_>>()?,
        ),
        "custom_data"
        | "entity_data"
        | "bucket_entity_data"
        | "block_entity_data"
        | "debug_stick_state"
        | "recipes"
        | "map_decorations"
        | "lock"
        | "container_loot"
        | "intangible_projectile" => value.as_nbt().ok()??,
        "enchantments" | "stored_enchantments" => {
            let fields = value.fields().ok()?;
            let mut levels = Compound::new();
            for entry in fields[0].items().ok()? {
                let entry = entry.fields().ok()?;
                let name = cx.new_name("enchantment", entry[0].as_int()?)?;
                levels.insert(namespaced(&name), Nbt::Int(entry[1].as_int()?));
            }
            let mut enchantments = Compound::new();
            enchantments.insert("levels", levels);
            show(&mut enchantments, &fields[1]);
            Nbt::Compound(enchantments)
        }
        "dyed_color" => {
            let fields = value.fields().ok()?;
            let mut color = Compound::new();
            color.insert("rgb", Nbt::Int(fields[0].as_int()?));
            show(&mut color, &fields[1]);
            Nbt::Compound(color)
        }
        "trim" => {
            let fields = value.fields().ok()?;
            let mut trim = Compound::new();
            trim.insert(
                "material",
                namespaced(&cx.new_name("trim_material", fields[0].as_int()?)?),
            );
            trim.insert(
                "pattern",
                namespaced(&cx.new_name("trim_pattern", fields[1].as_int()?)?),
            );
            show(&mut trim, &fields[2]);
            Nbt::Compound(trim)
        }
        "attribute_modifiers" => {
            let fields = value.fields().ok()?;
            let mut modifiers = Vec::new();
            for modifier in fields[0].items().ok()? {
                let modifier = modifier.fields().ok()?;
                let mut entry = Compound::new();
                entry.insert(
                    "type",
                    namespaced(&cx.new_name("attribute", modifier[0].as_int()?)?),
                );
                entry.insert("uuid", uuid(&modifier[1])?);
                entry.insert("name", modifier[2].as_str()?);
                let Value::Double(amount) = modifier[3] else {
                    return None;
                };
                entry.insert("amount", Nbt::Double(amount));
                entry.insert(
                    "operation",
                    *OPERATIONS.get(usize::try_from(modifier[4].as_int()?).ok()?)?,
                );
                if let Value::Enum(slot) = modifier[5]
                    && slot != "any"
                {
                    entry.insert("slot", slot);
                }
                modifiers.push(Nbt::Compound(entry));
            }
            let mut out = Compound::new();
            out.insert("modifiers", Nbt::List(modifiers));
            show(&mut out, &fields[1]);
            Nbt::Compound(out)
        }
        "potion_contents" => {
            let fields = value.fields().ok()?;
            let mut contents = Compound::new();
            if let Some(potion) = fields[0].option().ok()? {
                contents.insert(
                    "potion",
                    namespaced(&cx.new_name("potion", potion.as_int()?)?),
                );
            }
            if let Some(color) = fields[1].option().ok()? {
                contents.insert("custom_color", Nbt::Int(color.as_int()?));
            }
            let effects = fields[2]
                .items()
                .ok()?
                .iter()
                .map(|effect| {
                    let effect = effect.fields().ok()?;
                    let mut out = effect_details(&effect[1])?;
                    out.insert(
                        "id",
                        namespaced(&cx.new_name("mob_effect", effect[0].as_int()?)?),
                    );
                    Some(Nbt::Compound(out))
                })
                .collect::<Option<Vec<_>>>()?;
            if !effects.is_empty() {
                contents.insert("custom_effects", Nbt::List(effects));
            }
            Nbt::Compound(contents)
        }
        "instrument" => Nbt::String(namespaced(&cx.new_name("instrument", value.as_int()?)?)),
        "note_block_sound" => Nbt::String(value.as_str()?.to_owned()),
        "base_color" => {
            Nbt::String((*DYES.get(usize::try_from(value.as_int()?).ok()?)?).to_owned())
        }
        "banner_patterns" => Nbt::List(
            value
                .items()
                .ok()?
                .iter()
                .map(|layer| {
                    let layer = layer.fields().ok()?;
                    let mut out = Compound::new();
                    out.insert(
                        "pattern",
                        namespaced(&cx.new_name("banner_pattern", layer[0].as_int()?)?),
                    );
                    out.insert(
                        "color",
                        *DYES.get(usize::try_from(layer[1].as_int()?).ok()?)?,
                    );
                    Some(Nbt::Compound(out))
                })
                .collect::<Option<_>>()?,
        ),
        "pot_decorations" => Nbt::List(
            value
                .items()
                .ok()?
                .iter()
                .map(|id| {
                    Some(Nbt::String(namespaced(item_name(
                        super::NEW,
                        id.as_int()?,
                    )?)))
                })
                .collect::<Option<_>>()?,
        ),
        "block_state" => {
            let mut state = Compound::new();
            for property in value.items().ok()? {
                let property = property.fields().ok()?;
                state.insert(property[0].as_str()?, property[1].as_str()?);
            }
            Nbt::Compound(state)
        }
        "profile" => {
            let fields = value.fields().ok()?;
            let mut profile = Compound::new();
            if let Some(name) = fields[0].option().ok()? {
                profile.insert("name", name.as_str()?);
            }
            if let Some(id) = fields[1].option().ok()? {
                profile.insert("id", uuid(id)?);
            }
            let properties = fields[2]
                .items()
                .ok()?
                .iter()
                .map(|property| {
                    let property = property.fields().ok()?;
                    let mut out = Compound::new();
                    out.insert("name", property[0].as_str()?);
                    out.insert("value", property[1].as_str()?);
                    if let Some(signature) = property[2].option().ok()? {
                        out.insert("signature", signature.as_str()?);
                    }
                    Some(Nbt::Compound(out))
                })
                .collect::<Option<Vec<_>>>()?;
            if !properties.is_empty() {
                profile.insert("properties", Nbt::List(properties));
            }
            Nbt::Compound(profile)
        }
        "charged_projectiles" | "bundle_contents" => Nbt::List(
            value
                .items()
                .ok()?
                .iter()
                .map(|stack| match stack {
                    Value::Stack(Some(stack)) => encode_stack(cx, stack).map(Nbt::Compound),
                    _ => None,
                })
                .collect::<Option<_>>()?,
        ),
        "container" => {
            let mut slots = Vec::new();
            for (slot, stack) in value.items().ok()?.iter().enumerate() {
                let Value::Stack(Some(stack)) = stack else {
                    continue;
                };
                let mut entry = Compound::new();
                entry.insert("slot", Nbt::Int(slot as i32));
                entry.insert("item", encode_stack(cx, stack)?);
                slots.push(Nbt::Compound(entry));
            }
            Nbt::List(slots)
        }
        _ => return None,
    })
}

/// Effect details as 1.20.6's codec writes them: fields at their defaults
/// left out.
fn effect_details(details: &Value) -> Option<Compound> {
    let fields = details.fields().ok()?;
    let mut out = Compound::new();
    let amplifier = fields[0].as_int()?;
    if amplifier != 0 {
        out.insert("amplifier", Nbt::Byte(amplifier as i8));
    }
    let duration = fields[1].as_int()?;
    if duration != 0 {
        out.insert("duration", Nbt::Int(duration));
    }
    if fields[2].as_bool()? {
        out.insert("ambient", Nbt::Byte(1));
    }
    let particles = fields[3].as_bool()?;
    if !particles {
        out.insert("show_particles", Nbt::Byte(0));
    }
    let icon = fields[4].as_bool()?;
    if icon != particles {
        out.insert("show_icon", Nbt::Byte(i8::from(icon)));
    }
    if let Some(hidden) = fields[5].option().ok()? {
        out.insert("hidden_effect", effect_details(hidden)?);
    }
    Some(out)
}

fn read_effect_details(effect: &Compound) -> Value {
    let flag = |key: &str, default: bool| {
        effect
            .get(key)
            .and_then(Nbt::as_i64)
            .map_or(default, |value| value != 0)
    };
    let particles = flag("show_particles", true);
    Value::Struct(vec![
        Value::Int(i32::from(effect.int("amplifier").unwrap_or(0) as i8)),
        Value::Int(effect.int("duration").unwrap_or(0)),
        Value::Bool(flag("ambient", false)),
        Value::Bool(particles),
        Value::Bool(flag("show_icon", particles)),
        match effect.compound("hidden_effect") {
            Some(hidden) => Value::some(read_effect_details(hidden)),
            None => Value::none(),
        },
    ])
}

/// A 1.20.5 stack in persistent form: `{id, count, components}`.
fn encode_stack(cx: &mut Bridge, stack: &ItemStack) -> Option<Compound> {
    let mut out = Compound::new();
    out.insert("id", namespaced(item_name(super::NEW, stack.id)?));
    out.insert("count", Nbt::Int(stack.count));
    if let Some(patch) = stack.components.as_ref().filter(|patch| !patch.is_empty()) {
        out.insert("components", cx.nested(|cx| Ok(encode(cx, patch))).ok()?);
    }
    Some(out)
}

fn decode_stack(cx: &mut Bridge, stack: &Compound) -> Option<ItemStack> {
    let id = item_id(super::NEW, data::strip(stack.str("id")?))?;
    let components = match stack.compound("components") {
        Some(components) => cx.nested(|cx| Ok(decode(cx, components))).ok()?,
        None => Components::default(),
    };
    Some(ItemStack {
        id,
        count: stack.int("count").unwrap_or(1),
        damage: 0,
        tag: None,
        components: Some(components),
    })
}

fn decode_component(cx: &mut Bridge, kind: &str, nbt: &Nbt) -> Option<Value> {
    let compound = nbt.as_compound();
    Some(match kind {
        "damage" | "repair_cost" | "custom_model_data" | "map_id" | "map_color" => {
            Value::Int(nbt.as_i64()? as i32)
        }
        "enchantment_glint_override" => Value::Bool(nbt.as_i64()? != 0),
        "hide_additional_tooltip" | "hide_tooltip" | "fire_resistant" => Value::Unit,
        "unbreakable" => shown(compound?),
        "custom_name" | "item_name" => json_text(nbt)?,
        "lore" => Value::List(
            nbt.as_list()?
                .iter()
                .map(json_text)
                .collect::<Option<_>>()?,
        ),
        "custom_data"
        | "entity_data"
        | "bucket_entity_data"
        | "block_entity_data"
        | "debug_stick_state"
        | "recipes"
        | "map_decorations"
        | "lock"
        | "container_loot"
        | "intangible_projectile" => Value::nbt(nbt),
        "enchantments" | "stored_enchantments" => {
            // The full form, or just the levels.
            let (levels, show) = match compound?.compound("levels") {
                Some(levels) => (levels, shown(compound?)),
                None => (compound?, Value::Bool(true)),
            };
            let mut entries = Vec::new();
            for (name, level) in levels.iter() {
                let id = cx.new_id_by_name("enchantment", name)?;
                entries.push(Value::Struct(vec![
                    Value::Id(id),
                    Value::Int(level.as_i64()? as i32),
                ]));
            }
            Value::Struct(vec![Value::List(entries), show])
        }
        "dyed_color" => match nbt {
            Nbt::Compound(color) => {
                Value::Struct(vec![Value::Int(color.int("rgb")?), shown(color)])
            }
            other => Value::Struct(vec![Value::Int(other.as_i64()? as i32), Value::Bool(true)]),
        },
        "trim" => {
            let trim = compound?;
            Value::Struct(vec![
                Value::Id(cx.new_id_by_name("trim_material", trim.str("material")?)?),
                Value::Id(cx.new_id_by_name("trim_pattern", trim.str("pattern")?)?),
                shown(trim),
            ])
        }
        "attribute_modifiers" => {
            let (list, show) = match nbt {
                Nbt::Compound(full) => (full.list("modifiers")?, shown(full)),
                Nbt::List(list) => (list.as_slice(), Value::Bool(true)),
                _ => return None,
            };
            let mut modifiers = Vec::new();
            for modifier in list {
                let modifier = modifier.as_compound()?;
                let slot = modifier.str("slot").unwrap_or("any");
                let slot = super::tables::MODIFIER_SLOTS
                    .iter()
                    .chain(&["any", "hand", "armor"])
                    .find(|known| **known == slot)?;
                let operation = OPERATIONS.iter().position(|known| {
                    Some(*known) == modifier.str("operation").or(Some("add_value"))
                })?;
                modifiers.push(Value::Struct(vec![
                    Value::Id(cx.new_id_by_name("attribute", modifier.str("type")?)?),
                    read_uuid(modifier.get("uuid")?)?,
                    Value::String(modifier.str("name").unwrap_or("").to_owned()),
                    Value::Double(modifier.get("amount").and_then(Nbt::as_f64).unwrap_or(0.0)),
                    Value::Int(operation as i32),
                    Value::Enum(slot),
                ]));
            }
            Value::Struct(vec![Value::List(modifiers), show])
        }
        "potion_contents" => {
            // The full form, or just the potion.
            let (potion, color, effects) = match nbt {
                Nbt::String(potion) => (Some(potion.as_str()), None, &[][..]),
                Nbt::Compound(contents) => (
                    contents.str("potion"),
                    contents.int("custom_color"),
                    contents.list("custom_effects").unwrap_or(&[]),
                ),
                _ => return None,
            };
            let potion = match potion {
                Some(potion) => Value::some(Value::Id(cx.new_id_by_name("potion", potion)?)),
                None => Value::none(),
            };
            let effects = effects
                .iter()
                .map(|effect| {
                    let effect = effect.as_compound()?;
                    Some(Value::Struct(vec![
                        Value::Id(cx.new_id_by_name("mob_effect", effect.str("id")?)?),
                        read_effect_details(effect),
                    ]))
                })
                .collect::<Option<_>>()?;
            Value::Struct(vec![
                potion,
                color.map_or_else(Value::none, |color| Value::some(Value::Int(color))),
                Value::List(effects),
            ])
        }
        "instrument" => Value::Id(cx.new_id_by_name("instrument", nbt.as_str()?)?),
        "note_block_sound" => Value::String(nbt.as_str()?.to_owned()),
        "base_color" => {
            let color = nbt.as_str()?;
            Value::Int(DYES.iter().position(|dye| *dye == color)? as i32)
        }
        "banner_patterns" => Value::List(
            nbt.as_list()?
                .iter()
                .map(|layer| {
                    let layer = layer.as_compound()?;
                    let color = DYES
                        .iter()
                        .position(|dye| Some(*dye) == layer.str("color"))?;
                    Some(Value::Struct(vec![
                        Value::Id(cx.new_id_by_name("banner_pattern", layer.str("pattern")?)?),
                        Value::Int(color as i32),
                    ]))
                })
                .collect::<Option<_>>()?,
        ),
        "pot_decorations" => Value::List(
            nbt.as_list()?
                .iter()
                .map(|name| Some(Value::Id(item_id(super::NEW, data::strip(name.as_str()?))?)))
                .collect::<Option<_>>()?,
        ),
        "block_state" => Value::List(
            compound?
                .iter()
                .map(|(name, value)| {
                    Some(Value::Struct(vec![
                        Value::String(name.to_owned()),
                        Value::String(value.as_str()?.to_owned()),
                    ]))
                })
                .collect::<Option<_>>()?,
        ),
        "profile" => {
            let (name, id, properties) = match nbt {
                Nbt::String(name) => (Some(name.as_str()), None, &[][..]),
                Nbt::Compound(profile) => (
                    profile.str("name"),
                    profile.get("id"),
                    profile.list("properties").unwrap_or(&[]),
                ),
                _ => return None,
            };
            let properties = properties
                .iter()
                .map(|property| {
                    let property = property.as_compound()?;
                    Some(Value::Struct(vec![
                        Value::String(property.str("name")?.to_owned()),
                        Value::String(property.str("value")?.to_owned()),
                        property
                            .str("signature")
                            .map_or_else(Value::none, |signature| {
                                Value::some(Value::String(signature.to_owned()))
                            }),
                    ]))
                })
                .collect::<Option<_>>()?;
            Value::Struct(vec![
                name.map_or_else(Value::none, |name| {
                    Value::some(Value::String(name.to_owned()))
                }),
                match id {
                    Some(id) => Value::some(read_uuid(id)?),
                    None => Value::none(),
                },
                Value::List(properties),
            ])
        }
        "charged_projectiles" | "bundle_contents" => Value::List(
            nbt.as_list()?
                .iter()
                .map(|stack| {
                    Some(Value::Stack(Some(Box::new(decode_stack(
                        cx,
                        stack.as_compound()?,
                    )?))))
                })
                .collect::<Option<_>>()?,
        ),
        "container" => {
            let mut slots: Vec<Value> = Vec::new();
            for entry in nbt.as_list()? {
                let entry = entry.as_compound()?;
                let slot = usize::try_from(entry.int("slot")?)
                    .ok()
                    .filter(|slot| *slot < 256)?;
                let stack = decode_stack(cx, entry.compound("item")?)?;
                if slots.len() <= slot {
                    slots.resize(slot + 1, Value::Stack(None));
                }
                slots[slot] = Value::Stack(Some(Box::new(stack)));
            }
            Value::List(slots)
        }
        _ => return None,
    })
}

/// A `show_item` stack across the bridge: the 1.20.4 tag of `id` or the
/// 1.20.5 persistent components, converted for the receiver.
pub(super) fn hover(
    cx: &mut Bridge,
    upward: bool,
    id: &mut String,
    count: &mut i32,
    tag: &mut Option<Compound>,
    components: &mut Option<Compound>,
) -> io::Result<bool> {
    let (from, to) = if upward {
        (super::OLD, super::NEW)
    } else {
        (super::NEW, super::OLD)
    };
    let Some(item) = item_id(from, data::strip(id)) else {
        return Ok(false);
    };
    let stack = ItemStack {
        id: item,
        count: (*count).max(1),
        damage: 0,
        tag: if upward {
            tag.take().map(Nbt::Compound)
        } else {
            None
        },
        components: if upward {
            None
        } else {
            Some(
                components
                    .take()
                    .map(|c| decode(cx, &c))
                    .unwrap_or_default(),
            )
        },
    };
    let converted = if upward {
        cx.up(stack)?
    } else {
        cx.down(stack)?
    };
    let Some(converted) = converted else {
        return Ok(false);
    };
    let Some(name) = item_name(to, converted.id) else {
        return Ok(false);
    };
    *id = namespaced(name);
    *count = converted.count;
    if upward {
        *tag = None;
        *components = converted
            .components
            .map(|patch| encode(cx, &patch))
            .filter(|components| !components.is_empty());
    } else {
        *components = None;
        *tag = match converted.tag {
            Some(Nbt::Compound(tag)) => Some(tag),
            _ => None,
        };
    }
    Ok(true)
}
