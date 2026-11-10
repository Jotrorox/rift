//! What changed between adjacent protocols beyond what [`convert_value`]
//! carries over by itself: components split, merged, renamed or
//! restructured, and fields added or removed. Rules are keyed by the older
//! protocol of the pair and the direction (`up` to the newer one); each
//! produces the receiver's components and marks or stashes what it loses.
//!
//! Facts come from the official servers' stream codecs and data fixers of
//! each version (for example 1.21.5's `TooltipDisplayComponentFix` and 26.3's
//! `SwingAnimationComponentSplitFix`); the code is Rift's own.
use super::{
    Component, Components,
    convert::{Conv, Fail, Out, Res, convert_stack, convert_value},
    layouts::{self, Layout},
    schema::{Node, Value},
};
use crate::{
    codec::item::ItemStack,
    data,
    nbt::{Compound, Nbt},
    wire::invalid,
};
use std::io;

/// Patch-level rules, before components are converted one by one.
pub fn before(
    cx: &mut Conv<'_>,
    item: i32,
    input: &mut Components,
    out: &mut Out,
) -> io::Result<()> {
    let _ = item;
    match (cx.older(), cx.up()) {
        (767, false) => food_merge(cx, input, out),
        (769, true) => tooltip_up(cx, input, out),
        (769, false) => tooltip_down(cx, input, out),
        (776, false) => swing_merge(cx, input, out),
        _ => Ok(()),
    }
}

/// Rules for one component; `true` when the rule handled it (pushed the
/// receiver's components to `out`, or the component to `out.lost`).
pub fn component(
    cx: &mut Conv<'_>,
    item: i32,
    component: &Component,
    out: &mut Out,
) -> io::Result<bool> {
    match run(cx, item, component, out) {
        Ok(handled) => Ok(handled),
        Err(Fail::Lost) => {
            out.lost.push(component.clone());
            Ok(true)
        }
        Err(Fail::Error(error)) => Err(error),
    }
}

/// Rules for a type the sender's patch removes; `true` when handled.
pub fn removed(cx: &mut Conv<'_>, kind: &'static str, out: &mut Out) -> bool {
    let renamed: &[&str] = match (cx.older(), cx.up(), kind) {
        (767, true, "fire_resistant") => &["damage_resistant"],
        (767, false, "damage_resistant") => &["fire_resistant"],
        (776, true, "swing_animation") => &["attack_animation", "interact_animation"],
        (776, false, "attack_animation") => &["swing_animation"],
        _ => return false,
    };
    for name in renamed {
        if let Some(kind) = kind_name(cx.target, name)
            && !out.components.removed.contains(&kind)
        {
            out.components.removed.push(kind);
        }
    }
    true
}

fn run(cx: &mut Conv<'_>, item: i32, component: &Component, out: &mut Out) -> Res<bool> {
    let value = &component.value;
    let up = cx.up();
    match (cx.older(), component.kind) {
        (766, "attribute_modifiers") => {
            let converted = if up {
                attribute_ids_up(cx, value)?
            } else {
                attribute_ids_down(cx, value)?
            };
            out.push("attribute_modifiers", converted);
        }
        (766, "food") => {
            let converted = same_kind(cx, "food", value, &[("using_converts_to", Value::none())])?;
            out.push("food", converted);
        }
        (767, "food") if up => food_split(cx, item, value, out)?,
        (767, "fire_resistant") if up => out.push(
            "damage_resistant",
            Value::String("minecraft:is_fire".into()),
        ),
        (767, "damage_resistant") if !up => match value.as_str() {
            Some("minecraft:is_fire" | "is_fire") => out.push("fire_resistant", Value::Unit),
            _ => return Err(Fail::Lost),
        },
        (767, "potion_contents") => {
            let converted = same_kind(
                cx,
                "potion_contents",
                value,
                &[("custom_name", Value::none())],
            )?;
            out.push("potion_contents", converted);
        }
        (767, "instrument") => {
            let converted = instrument_767(cx, value)?;
            out.push("instrument", converted);
        }
        (767..=769, "trim") => {
            let converted = trim(cx, "trim", value)?;
            out.push("trim", converted);
        }
        // The lock changed from a string to an item predicate; only the
        // server reads it.
        (767, "lock") => return Err(Fail::Lost),
        (768, "custom_model_data") => {
            let converted = if up {
                Value::Struct(vec![
                    Value::List(vec![Value::Float(int(value)? as f32)]),
                    Value::List(Vec::new()),
                    Value::List(Vec::new()),
                    Value::List(Vec::new()),
                ])
            } else {
                custom_model_data_down(cx, value)?
            };
            out.push("custom_model_data", converted);
        }
        (
            769,
            kind @ ("unbreakable"
            | "enchantments"
            | "stored_enchantments"
            | "dyed_color"
            | "jukebox_playable"
            | "attribute_modifiers"
            | "can_place_on"
            | "can_break"),
        ) => {
            let converted = if up {
                strip_flag(cx, kind, value)?
            } else {
                add_flag(cx, kind, value)?
            };
            out.push(kind_name(cx.target, kind).ok_or(Fail::Lost)?, converted);
        }
        (769, "tool") => {
            let converted = same_kind(
                cx,
                "tool",
                value,
                &[("can_destroy_blocks_in_creative", Value::Bool(true))],
            )?;
            out.push("tool", converted);
        }
        (769, "equippable") => {
            if !up && field(cx.layouts("equippable").0, value, "slot")? == &Value::Enum("saddle") {
                return Err(Fail::Lost);
            }
            let converted = same_kind(
                cx,
                "equippable",
                value,
                &[("equip_on_interact", Value::Bool(false))],
            )?;
            out.push("equippable", converted);
        }
        (770, "attribute_modifiers") => {
            let (source, target) = layouts_of(cx, "attribute_modifiers")?;
            let (source, target) = (element(source)?, element(target)?);
            let mut entries = Vec::new();
            for entry in value.items()? {
                let default = Value::Variant("default", Box::new(Value::Unit));
                entries.push(by_name(
                    cx,
                    source,
                    target,
                    entry,
                    &[],
                    &[("display", default)],
                )?);
            }
            out.push("attribute_modifiers", Value::List(entries));
        }
        (770, "equippable") => {
            let shears = if up {
                cx.sound("item.shears.snip")
            } else {
                source_sound(cx, "item.shears.snip")
            };
            let converted = same_kind(
                cx,
                "equippable",
                value,
                &[
                    ("can_be_sheared", Value::Bool(false)),
                    ("shearing_sound", shears),
                ],
            )?;
            out.push("equippable", converted);
        }
        (772, "entity_data") => {
            let converted = typed_data(cx, "entity_type", value)?;
            out.push("entity_data", converted);
        }
        (772, "block_entity_data") => {
            let converted = typed_data(cx, "block_entity_type", value)?;
            out.push("block_entity_data", converted);
        }
        (772, "bees") => {
            let mut bees = Vec::new();
            for bee in value.items()? {
                let fields = bee.fields()?;
                match typed_data(cx, "entity_type", &fields[0]) {
                    Ok(data) => bees.push(Value::Struct(vec![
                        data,
                        fields[1].clone(),
                        fields[2].clone(),
                    ])),
                    Err(Fail::Lost) => cx.lossy = true,
                    Err(error) => return Err(error),
                }
            }
            out.push("bees", Value::List(bees));
        }
        (772, "profile") => {
            let converted = profile(cx, value)?;
            out.push("profile", converted);
        }
        (776, "swing_animation") if up => {
            for kind in ["attack_animation", "interact_animation"] {
                out.push(kind, value.clone());
            }
        }
        (776, kind @ ("trim" | "provides_trim_material")) => {
            let converted = trim(cx, kind, value)?;
            out.push(kind, converted);
        }
        (776, "instrument") => {
            let converted = instrument_777(cx, value)?;
            out.push("instrument", converted);
        }
        (776, "consumable") => {
            let (source, target) = layouts_of(cx, "consumable")?;
            let (s, t) = (struct_fields(source)?, struct_fields(target)?);
            let mut fields = value.fields()?.to_vec();
            for (index, field) in fields.iter_mut().enumerate() {
                *field = if s[index].0 == "on_consume_effects" {
                    consume_effects_777(cx, field)?
                } else {
                    convert_value(cx, &s[index].1, &t[index].1, field)?
                };
            }
            out.push("consumable", Value::Struct(fields));
        }
        (776, "death_protection") => {
            let converted = consume_effects_777(cx, value)?;
            out.push("death_protection", converted);
        }
        (776, "pot_decorations") => {
            let converted = pot_decorations(cx, value)?;
            out.push("pot_decorations", converted);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// The static name of component type `name` in `protocol`.
fn kind_name(protocol: i32, name: &str) -> Option<&'static str> {
    let layout = Layout::of(protocol).ok()?;
    Some(layout.get(layout.id(name)?)?.0)
}

fn layouts_of(cx: &Conv<'_>, kind: &str) -> Res<(&'static Node, &'static Node)> {
    match cx.layouts(kind) {
        (Some(source), Some(target)) => Ok((source, target)),
        _ => Err(Fail::Lost),
    }
}

fn deref(node: &'static Node) -> &'static Node {
    match node {
        Node::Ref(inner) => deref(inner),
        node => node,
    }
}

fn struct_fields(node: &'static Node) -> Res<&'static [(&'static str, Node)]> {
    match deref(node) {
        Node::Struct(fields) => Ok(fields),
        _ => Err(invalid("layout is not a struct").into()),
    }
}

fn element(node: &'static Node) -> Res<&'static Node> {
    match deref(node) {
        Node::List(element, _) | Node::Optional(element) => Ok(element),
        _ => Err(invalid("layout is not a list").into()),
    }
}

/// The layout of a struct's field `name`.
fn sub(node: &'static Node, name: &str) -> Res<&'static Node> {
    struct_fields(node)?
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, node)| node)
        .ok_or_else(|| invalid(&format!("layout has no field {name}")).into())
}

/// The value of a struct's field `name`, by its layout.
fn field<'v>(node: Option<&'static Node>, value: &'v Value, name: &str) -> Res<&'v Value> {
    let fields = struct_fields(node.ok_or(Fail::Lost)?)?;
    let index = fields
        .iter()
        .position(|(known, _)| *known == name)
        .ok_or_else(|| invalid(&format!("layout has no field {name}")))?;
    value
        .fields()?
        .get(index)
        .ok_or_else(|| invalid("struct value is too short").into())
}

fn int(value: &Value) -> Res<i32> {
    value
        .as_int()
        .ok_or_else(|| invalid("value is not an int").into())
}

/// Convert a struct field by field, matched by name (`renames` maps a
/// source name to the target's). Target fields the source lacks take their
/// value from `defaults`; source fields the target lacks are dropped, which
/// is lossy unless they hold the value `defaults` gives for them.
fn by_name(
    cx: &mut Conv<'_>,
    source: &'static Node,
    target: &'static Node,
    value: &Value,
    renames: &[(&str, &str)],
    defaults: &[(&str, Value)],
) -> Res<Value> {
    let (s, t) = (struct_fields(source)?, struct_fields(target)?);
    let values = value.fields()?;
    if values.len() != s.len() {
        return Err(invalid("struct value does not match its layout").into());
    }
    let source_name = |name: &str| -> String {
        renames
            .iter()
            .find(|(_, to)| *to == name)
            .map_or(name, |(from, _)| from)
            .to_owned()
    };
    let mut used = vec![false; s.len()];
    let mut fields = Vec::with_capacity(t.len());
    for (name, node) in t {
        let wanted = source_name(name);
        match s.iter().position(|(known, _)| *known == wanted) {
            Some(index) => {
                used[index] = true;
                fields.push(convert_value(cx, &s[index].1, node, &values[index])?);
            }
            None => fields.push(
                defaults
                    .iter()
                    .find(|(known, _)| known == name)
                    .map(|(_, value)| value.clone())
                    .ok_or_else(|| invalid(&format!("no default for field {name}")))?,
            ),
        }
    }
    for (index, (name, _)) in s.iter().enumerate() {
        if !used[index]
            && defaults
                .iter()
                .find(|(known, _)| known == name)
                .is_none_or(|(_, default)| *default != values[index])
        {
            cx.lossy = true;
        }
    }
    Ok(Value::Struct(fields))
}

/// [`by_name`] for a component that keeps its name.
fn same_kind(
    cx: &mut Conv<'_>,
    kind: &str,
    value: &Value,
    defaults: &[(&str, Value)],
) -> Res<Value> {
    let (source, target) = layouts_of(cx, kind)?;
    by_name(cx, source, target, value, &[], defaults)
}

/// The sender's ID of its built-in sound `name`, as a holder value.
fn source_sound(cx: &Conv<'_>, name: &str) -> Value {
    data::version(cx.source)
        .registry("sound_event")
        .and_then(|registry| registry.id(name))
        .map_or(Value::Unit, Value::Id)
}

/// `[8, 0, 0]`: an empty string tag, the plainest text component.
fn empty_text() -> Value {
    Value::Nbt(vec![8, 0, 0])
}

/// Vanilla's modifier UUIDs before 1.21 and the identifiers that replaced
/// them.
const MODIFIER_IDS: &[(u128, &str)] = &[
    (
        0xCB3F55D3_645C_4F38_A497_9C13A33DB5CF,
        "minecraft:base_attack_damage",
    ),
    (
        0xFA233E1C_4180_4865_B01B_BCCE9785ACA3,
        "minecraft:base_attack_speed",
    ),
    (
        0x845DB27C_C624_495F_8C9F_6020A9A58B6B,
        "minecraft:armor.boots",
    ),
    (
        0xD8499B04_0E66_4726_AB29_64469D734E0D,
        "minecraft:armor.leggings",
    ),
    (
        0x9F3D476D_C118_4544_8365_64846904B48E,
        "minecraft:armor.chestplate",
    ),
    (
        0x2AD3F246_FEE1_4E67_B886_69FD380BB150,
        "minecraft:armor.helmet",
    ),
    (
        0xC1C72771_8B8E_BA4A_ACE0_81A93C8928B2,
        "minecraft:armor.body",
    ),
];

fn format_uuid(uuid: u128) -> String {
    let hex = format!("{uuid:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn parse_uuid(text: &str) -> Option<u128> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    (hex.len() == 32 && text.len() == 36)
        .then(|| u128::from_str_radix(&hex, 16).ok())
        .flatten()
}

/// A stable name-based UUID (version 3 layout, FNV-1a based) for modifier
/// identifiers that are neither vanilla's nor written from a UUID.
fn name_uuid(name: &str) -> u128 {
    let hash = |seed: u64| {
        name.bytes().fold(seed, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
        })
    };
    let raw =
        (u128::from(hash(0xcbf2_9ce4_8422_2325)) << 64) | u128::from(hash(0x6c62_272e_07bb_0142));
    (raw & !(0xf << 76) | (3 << 76)) & !(0x3 << 62) | (0x2 << 62)
}

/// 1.20.5 to 1.21: a modifier's UUID and name became one identifier.
fn attribute_ids_up(cx: &mut Conv<'_>, value: &Value) -> Res<Value> {
    let fields = value.fields()?;
    let mut entries = Vec::new();
    for entry in fields[0].items()? {
        let e = entry.fields()?;
        let Some(attribute) = cx.map_id("attribute", int(&e[0])?) else {
            cx.lossy = true;
            continue;
        };
        let Value::Uuid(uuid) = e[1] else {
            return Err(invalid("modifier ID is not a UUID").into());
        };
        let id = MODIFIER_IDS
            .iter()
            .find(|(known, _)| *known == uuid)
            .map_or_else(
                || format!("minecraft:{}", format_uuid(uuid)),
                |(_, id)| (*id).to_owned(),
            );
        if e[2].as_str() != Some(id.as_str()) {
            cx.lossy = true; // The name is gone.
        }
        entries.push(Value::Struct(vec![
            Value::Id(attribute),
            Value::String(id),
            e[3].clone(),
            e[4].clone(),
            e[5].clone(),
        ]));
    }
    Ok(Value::Struct(vec![Value::List(entries), fields[1].clone()]))
}

fn attribute_ids_down(cx: &mut Conv<'_>, value: &Value) -> Res<Value> {
    let fields = value.fields()?;
    let mut entries = Vec::new();
    for entry in fields[0].items()? {
        let e = entry.fields()?;
        let Some(attribute) = cx.map_id("attribute", int(&e[0])?) else {
            cx.lossy = true;
            continue;
        };
        let id = e[1]
            .as_str()
            .ok_or_else(|| invalid("modifier ID is not a string"))?;
        let uuid = match MODIFIER_IDS.iter().find(|(_, known)| *known == id) {
            Some((uuid, _)) => *uuid,
            None => match parse_uuid(data::strip(id)) {
                Some(uuid) if id.starts_with("minecraft:") => uuid,
                _ => {
                    cx.lossy = true;
                    name_uuid(id)
                }
            },
        };
        entries.push(Value::Struct(vec![
            Value::Id(attribute),
            Value::Uuid(uuid),
            Value::String(id.to_owned()),
            e[2].clone(),
            e[3].clone(),
            e[4].clone(),
        ]));
    }
    Ok(Value::Struct(vec![Value::List(entries), fields[1].clone()]))
}

/// Items whose food was drunk rather than eaten before 1.21.2 made that a
/// property of the `consumable` component.
const DRUNK: &[&str] = &["potion", "honey_bottle", "milk_bucket", "ominous_bottle"];

/// 1.21.2 split `food` into `food`, `consumable` and `use_remainder`.
fn food_split(cx: &mut Conv<'_>, item: i32, value: &Value, out: &mut Out) -> Res<()> {
    let f = value.fields()?;
    out.push("food", Value::Struct(f[..3].to_vec()));
    let drunk = data::version(cx.target)
        .registry("item")
        .and_then(|registry| registry.name(item))
        .is_some_and(|name| DRUNK.contains(&name));
    let mut effects = Vec::new();
    for possible in f[5].items()? {
        let p = possible.fields()?;
        let effect = match convert_value(cx, &layouts::EFFECT, &layouts::EFFECT, &p[0]) {
            Ok(effect) => effect,
            Err(Fail::Lost) => {
                cx.lossy = true;
                continue;
            }
            Err(error) => return Err(error),
        };
        effects.push(Value::Variant(
            "apply_effects",
            Box::new(Value::Struct(vec![Value::List(vec![effect]), p[1].clone()])),
        ));
    }
    let (animation, sound) = if drunk {
        ("drink", "entity.generic.drink")
    } else {
        ("eat", "entity.generic.eat")
    };
    out.push(
        "consumable",
        Value::Struct(vec![
            f[3].clone(),
            Value::Enum(animation),
            cx.sound(sound),
            Value::Bool(true),
            Value::List(effects),
        ]),
    );
    if let Some(Value::Stack(stack)) = f[4].option()? {
        match convert_stack(cx, stack.as_deref())? {
            Some(stack) => out.push("use_remainder", Value::Stack(Some(Box::new(stack)))),
            None => cx.lossy = true,
        }
    }
    Ok(())
}

/// The reverse of [`food_split`], for the whole patch: `consumable` and
/// `use_remainder` fold back into `food` when it is there.
fn food_merge(cx: &mut Conv<'_>, input: &mut Components, out: &mut Out) -> io::Result<()> {
    let Some(food) = input.take("food") else {
        return Ok(());
    };
    let consumable = input.take("consumable");
    if consumable.is_none() && !input.removes("consumable") {
        // Going back up, `food` always brings a `consumable` (as vanilla's
        // own data fixer does); this patch relied on the item's default.
        out.absent.push("consumable");
    }
    let remainder = input.take("use_remainder");
    cx.lossy = false;
    let merged = (|| -> Res<Value> {
        let f = food.fields()?;
        let mut seconds = Value::Float(1.6);
        let mut effects = Vec::new();
        if let Some(consumable) = &consumable {
            let c = consumable.fields()?;
            seconds = c[0].clone();
            let default_sound = match c[1] {
                Value::Enum("eat") => source_sound(cx, "entity.generic.eat"),
                Value::Enum("drink") => source_sound(cx, "entity.generic.drink"),
                _ => Value::Unit,
            };
            if c[2] != default_sound || c[3] != Value::Bool(true) {
                cx.lossy = true;
            }
            for effect in c[4].items()? {
                let Value::Variant("apply_effects", apply) = effect else {
                    cx.lossy = true;
                    continue;
                };
                let a = apply.fields()?;
                for instance in a[0].items()? {
                    match convert_value(cx, &layouts::EFFECT, &layouts::EFFECT, instance) {
                        Ok(instance) => effects.push(Value::Struct(vec![instance, a[1].clone()])),
                        Err(Fail::Lost) => cx.lossy = true,
                        Err(error) => return Err(error),
                    }
                }
            }
        }
        let using = match &remainder {
            Some(Value::Stack(stack)) => match convert_stack(cx, stack.as_deref())? {
                Some(stack) => Value::some(Value::Stack(Some(Box::new(stack)))),
                None => {
                    cx.lossy = true;
                    Value::none()
                }
            },
            _ => Value::none(),
        };
        let mut fields = f.to_vec();
        fields.extend([seconds, using, Value::List(effects)]);
        Ok(Value::Struct(fields))
    })();
    match merged {
        Ok(merged) => {
            out.push("food", merged);
            if cx.lossy {
                let consumable = consumable.into_iter().map(|value| ("consumable", value));
                out.lost
                    .extend(consumable.map(|(kind, value)| Component { kind, value }));
            }
        }
        Err(Fail::Lost) => out.lost.push(Component {
            kind: "food",
            value: food,
        }),
        Err(Fail::Error(error)) => return Err(error),
    }
    cx.lossy = false;
    Ok(())
}

/// The instrument holder before 1.21.2 counted its use duration in ticks
/// and had no description.
fn instrument_767(cx: &mut Conv<'_>, value: &Value) -> Res<Value> {
    let Value::Direct(inner) = value else {
        return Ok(Value::Id(
            cx.map_id("instrument", int(value)?).ok_or(Fail::Lost)?,
        ));
    };
    let i = inner.fields()?;
    let sound = convert_value(cx, &layouts::SOUND, &layouts::SOUND, &i[0])?;
    Ok(Value::Direct(Box::new(if cx.up() {
        Value::Struct(vec![
            sound,
            Value::Float(int(&i[1])? as f32 / 20.0),
            i[2].clone(),
            empty_text(),
        ])
    } else {
        let Value::Float(seconds) = i[1] else {
            return Err(invalid("instrument duration is not a float").into());
        };
        if i[3] != empty_text() || (seconds * 20.0).fract() != 0.0 {
            cx.lossy = true;
        }
        Value::Struct(vec![
            sound,
            Value::Int((seconds * 20.0).round() as i32),
            i[2].clone(),
        ])
    })))
}

/// 26.3 added how much durability an instrument use costs.
fn instrument_777(cx: &mut Conv<'_>, value: &Value) -> Res<Value> {
    let Value::Direct(inner) = value else {
        return Ok(Value::Id(
            cx.map_id("instrument", int(value)?).ok_or(Fail::Lost)?,
        ));
    };
    let (source, target) = layouts_of(cx, "instrument")?;
    let (Node::Holder(_, source), Node::Holder(_, target)) = (deref(source), deref(target)) else {
        return Err(invalid("instrument layout is not a holder").into());
    };
    let converted = by_name(
        cx,
        source,
        target,
        inner,
        &[],
        &[("durability_damage", Value::Int(0))],
    )?;
    Ok(Value::Direct(Box::new(converted)))
}

/// `trim` (and 26.3's `provides_trim_material`): the inline trim material
/// and pattern changed nearly every release, and the tooltip flag went in
/// 1.21.5. Registry references only need their IDs mapped.
fn trim(cx: &mut Conv<'_>, kind: &str, value: &Value) -> Res<Value> {
    let (source, target) = layouts_of(cx, kind)?;
    if let Node::Holder(..) = deref(source) {
        // `provides_trim_material` is the material holder alone.
        return trim_material(cx, source, target, value);
    }
    let (source_material, target_material) = (sub(source, "material")?, sub(target, "material")?);
    let fields = value.fields()?;
    let material = trim_material(cx, source_material, target_material, &fields[0])?;
    let pattern = trim_pattern(
        cx,
        sub(source, "pattern")?,
        sub(target, "pattern")?,
        &fields[1],
    )?;
    let mut converted = vec![material, pattern];
    let source_flag = struct_fields(source)?.len() == 3;
    let target_flag = struct_fields(target)?.len() == 3;
    match (source_flag, target_flag) {
        (true, true) => converted.push(fields[2].clone()),
        (false, true) => converted.push(Value::Bool(!cx.hidden.contains(&"trim"))),
        _ => {}
    }
    Ok(Value::Struct(converted))
}

fn trim_material(
    cx: &mut Conv<'_>,
    source: &'static Node,
    target: &'static Node,
    value: &Value,
) -> Res<Value> {
    let Value::Direct(inner) = value else {
        return Ok(Value::Id(
            cx.map_id("trim_material", int(value)?).ok_or(Fail::Lost)?,
        ));
    };
    let (Node::Holder(_, source), Node::Holder(_, target)) = (deref(source), deref(target)) else {
        return Err(invalid("trim material layout is not a holder").into());
    };
    let s = struct_fields(source)?;
    let t = struct_fields(target)?;
    let values = inner.fields()?;
    let get = |name: &str| {
        s.iter()
            .position(|(known, _)| *known == name)
            .map(|index| &values[index])
    };
    let description = convert_value(
        cx,
        &Node::Text,
        &Node::Text,
        get("description").ok_or(Fail::Lost)?,
    )?;
    let base = get("asset_name")
        .or_else(|| get("base"))
        .cloned()
        .or_else(|| {
            get("palette")
                .and_then(Value::as_str)
                .map(|palette| Value::String(data::strip(palette).to_owned()))
        })
        .ok_or(Fail::Lost)?;
    let overrides = get("override_armor_materials")
        .or_else(|| get("override_armor_assets"))
        .or_else(|| get("overrides"));
    let mut fields = Vec::with_capacity(t.len());
    for (name, _) in t {
        fields.push(match *name {
            "asset_name" | "base" => base.clone(),
            "palette" => {
                cx.lossy = true;
                Value::String(format!("minecraft:{}", base.as_str().unwrap_or("")))
            }
            "ingredient" => match get("ingredient") {
                Some(Value::Id(id)) => Value::Id(cx.map_id("item", *id).ok_or(Fail::Lost)?),
                _ => {
                    cx.lossy = true;
                    Value::Id(cx.target_id("item", "air").unwrap_or(0))
                }
            },
            "item_model_index" => get("item_model_index").cloned().unwrap_or_else(|| {
                cx.lossy = true;
                Value::Float(0.0)
            }),
            "override_armor_materials" | "override_armor_assets" | "overrides" => {
                let materials = *name == "override_armor_materials"
                    && matches!(sub(target, name).and_then(element), Ok(element) if matches!(sub(element, "material"), Ok(Node::Id(_))));
                let mut entries = Vec::new();
                for entry in overrides.map(Value::items).transpose()?.unwrap_or_default() {
                    let e = entry.fields()?;
                    let key = match &e[0] {
                        Value::Id(id) => data::version(cx.source)
                            .registry("armor_material")
                            .and_then(|registry| registry.name(*id))
                            .map(|name| format!("minecraft:{name}")),
                        Value::String(name) => Some(name.clone()),
                        _ => None,
                    };
                    let key = match (key, materials) {
                        (Some(key), true) => cx.target_id("armor_material", &key).map(Value::Id),
                        (Some(key), false) => Some(Value::String(key)),
                        (None, _) => None,
                    };
                    match key {
                        Some(key) => entries.push(Value::Struct(vec![key, e[1].clone()])),
                        None => cx.lossy = true,
                    }
                }
                if overrides.is_none() {
                    cx.lossy = true;
                }
                Value::List(entries)
            }
            "description" => description.clone(),
            _ => return Err(invalid(&format!("unknown trim material field {name}")).into()),
        });
    }
    for (name, _) in s {
        if matches!(*name, "ingredient" | "item_model_index")
            && !t.iter().any(|(known, _)| known == name)
        {
            cx.lossy = true;
        }
        if *name == "overrides"
            && t.iter().all(|(known, _)| *known != "overrides")
            && t.iter().all(|(known, _)| !known.starts_with("override"))
            && overrides.is_some_and(|value| value.items().is_ok_and(|items| !items.is_empty()))
        {
            cx.lossy = true;
        }
    }
    Ok(Value::Direct(Box::new(Value::Struct(fields))))
}

fn trim_pattern(
    cx: &mut Conv<'_>,
    source: &'static Node,
    target: &'static Node,
    value: &Value,
) -> Res<Value> {
    let Value::Direct(inner) = value else {
        return Ok(Value::Id(
            cx.map_id("trim_pattern", int(value)?).ok_or(Fail::Lost)?,
        ));
    };
    let (Node::Holder(_, source), Node::Holder(_, target)) = (deref(source), deref(target)) else {
        return Err(invalid("trim pattern layout is not a holder").into());
    };
    let air = Value::Id(cx.target_id("item", "air").unwrap_or(0));
    let converted = by_name(cx, source, target, inner, &[], &[("template_item", air)])?;
    Ok(Value::Direct(Box::new(converted)))
}

fn custom_model_data_down(cx: &mut Conv<'_>, value: &Value) -> Res<Value> {
    let fields = value.fields()?;
    let floats = fields[0].items()?;
    let Some(Value::Float(first)) = floats.first() else {
        return Err(Fail::Lost);
    };
    if floats.len() != 1
        || first.fract() != 0.0
        || fields[1..]
            .iter()
            .any(|list| list.items().is_ok_and(|items| !items.is_empty()))
    {
        cx.lossy = true;
    }
    Ok(Value::Int(*first as i32))
}

/// Components that showed their own tooltip flag before 1.21.5.
const FLAGGED: &[&str] = &[
    "unbreakable",
    "enchantments",
    "stored_enchantments",
    "attribute_modifiers",
    "dyed_color",
    "trim",
    "can_place_on",
    "can_break",
    "jukebox_playable",
];

/// What `hide_additional_tooltip` hid, as 1.21.5's data fixer lists it.
const ADDITIONAL: &[&str] = &[
    "banner_patterns",
    "bees",
    "block_entity_data",
    "block_state",
    "bundle_contents",
    "charged_projectiles",
    "container",
    "container_loot",
    "firework_explosion",
    "fireworks",
    "instrument",
    "map_id",
    "painting/variant",
    "pot_decorations",
    "potion_contents",
    "tropical_fish/pattern",
    "written_book_content",
];

/// Whether a flagged 1.21.4 component's tooltip shows.
fn shown(kind: &str, value: &Value) -> bool {
    match (kind, value) {
        ("unbreakable", Value::Bool(show)) => *show,
        (_, Value::Struct(fields)) => fields.last() != Some(&Value::Bool(false)),
        _ => true,
    }
}

/// 1.21.5 replaced `hide_tooltip`, `hide_additional_tooltip` and the
/// per-component flags with `tooltip_display`.
fn tooltip_up(cx: &mut Conv<'_>, input: &mut Components, out: &mut Out) -> io::Result<()> {
    let mut hidden: Vec<&str> = FLAGGED
        .iter()
        .copied()
        .filter(|kind| input.get(kind).is_some_and(|value| !shown(kind, value)))
        .collect();
    let hide = input.take("hide_tooltip").is_some();
    if input.take("hide_additional_tooltip").is_some() {
        let before = hidden.len();
        hidden.extend(
            ADDITIONAL
                .iter()
                .copied()
                .filter(|kind| input.get(kind).is_some()),
        );
        if hidden.len() == before {
            // It hides nothing this stack has, so no list can say it.
            out.lost.push(Component {
                kind: "hide_additional_tooltip",
                value: Value::Unit,
            });
        }
    }
    if hide || !hidden.is_empty() {
        let kinds = hidden
            .iter()
            .filter_map(|kind| kind_name(cx.target, kind))
            .map(Value::Kind)
            .collect();
        out.push(
            "tooltip_display",
            Value::Struct(vec![Value::Bool(hide), Value::List(kinds)]),
        );
    }
    Ok(())
}

fn tooltip_down(cx: &mut Conv<'_>, input: &mut Components, out: &mut Out) -> io::Result<()> {
    cx.hidden.clear();
    let Some(display) = input.take("tooltip_display") else {
        return Ok(());
    };
    let fields = display.fields()?;
    let hide = fields[0] == Value::Bool(true);
    for kind in fields[1].items()? {
        if let Value::Kind(kind) = kind {
            cx.hidden.push(kind);
        }
    }
    if hide {
        out.push("hide_tooltip", Value::Unit);
    }
    let additional = cx.hidden.iter().any(|kind| ADDITIONAL.contains(kind));
    if additional {
        out.push("hide_additional_tooltip", Value::Unit);
    }
    // Lossless only when converting back gives the same hidden set.
    let mut back: Vec<&str> = FLAGGED
        .iter()
        .copied()
        .filter(|kind| cx.hidden.contains(kind) && input.get(kind).is_some())
        .collect();
    if additional {
        back.extend(
            ADDITIONAL
                .iter()
                .copied()
                .filter(|kind| input.get(kind).is_some()),
        );
    }
    let same = back.len() == cx.hidden.len() && back.iter().all(|kind| cx.hidden.contains(kind));
    if !same {
        out.lost.push(Component {
            kind: "tooltip_display",
            value: display,
        });
    }
    Ok(())
}

/// Drop a 1.21.4 component's tooltip flag (handled by [`tooltip_up`]).
fn strip_flag(cx: &mut Conv<'_>, kind: &str, value: &Value) -> Res<Value> {
    let (source, target) = layouts_of(cx, kind)?;
    match kind {
        "unbreakable" => Ok(Value::Unit),
        "can_place_on" | "can_break" => {
            let (s, t) = (element(sub(source, "predicates")?)?, element(target)?);
            let empty = Value::Struct(vec![Value::List(Vec::new()), Value::List(Vec::new())]);
            let mut predicates = Vec::new();
            for predicate in value.fields()?[0].items()? {
                predicates.push(by_name(
                    cx,
                    s,
                    t,
                    predicate,
                    &[],
                    &[("components", empty.clone())],
                )?);
            }
            Ok(Value::List(predicates))
        }
        _ => {
            let inner_source = &struct_fields(source)?[0].1;
            convert_value(cx, inner_source, target, &value.fields()?[0])
        }
    }
}

/// Give a 1.21.5 component its 1.21.4 tooltip flag from `tooltip_display`.
fn add_flag(cx: &mut Conv<'_>, kind: &str, value: &Value) -> Res<Value> {
    let (source, target) = layouts_of(cx, kind)?;
    let show = Value::Bool(!cx.hidden.contains(&kind));
    match kind {
        "unbreakable" => Ok(show),
        "can_place_on" | "can_break" => {
            let (s, t) = (element(source)?, element(sub(target, "predicates")?)?);
            let empty = Value::Struct(vec![Value::List(Vec::new()), Value::List(Vec::new())]);
            let mut predicates = Vec::new();
            for predicate in value.items()? {
                predicates.push(by_name(
                    cx,
                    s,
                    t,
                    predicate,
                    &[],
                    &[("components", empty.clone())],
                )?);
            }
            Ok(Value::Struct(vec![Value::List(predicates), show]))
        }
        "attribute_modifiers" => {
            let (s, t) = (element(source)?, element(sub(target, "modifiers")?)?);
            let mut entries = Vec::new();
            for entry in value.items()? {
                if field(Some(s), entry, "slot")? == &Value::Enum("saddle") {
                    cx.lossy = true;
                    continue;
                }
                entries.push(convert_value(cx, s, t, entry)?);
            }
            Ok(Value::Struct(vec![Value::List(entries), show]))
        }
        _ => {
            let inner_target = &struct_fields(target)?[0].1;
            Ok(Value::Struct(vec![
                convert_value(cx, source, inner_target, value)?,
                show,
            ]))
        }
    }
}

/// 1.21.9 moved the entity or block entity type out of the data's `id` tag.
fn typed_data(cx: &mut Conv<'_>, registry: &'static str, value: &Value) -> Res<Value> {
    if cx.up() {
        let Some(Nbt::Compound(mut data)) = value.as_nbt()? else {
            return Err(invalid("entity data is not a compound").into());
        };
        // Going back puts `id` first; anywhere else, keep the original.
        if data.iter().next().is_some_and(|(key, _)| key != "id") {
            cx.lossy = true;
        }
        let Some(Nbt::String(name)) = data.remove("id") else {
            return Err(Fail::Lost);
        };
        let id = cx.target_id(registry, &name).ok_or(Fail::Lost)?;
        Ok(Value::Struct(vec![
            Value::Id(id),
            Value::nbt(&Nbt::Compound(data)),
        ]))
    } else {
        let fields = value.fields()?;
        let name = data::version(cx.source)
            .registry(registry)
            .and_then(|known| known.name(fields[0].as_int()?).map(str::to_owned))
            .ok_or(Fail::Lost)?;
        let Some(Nbt::Compound(data)) = fields[1].as_nbt()? else {
            return Err(invalid("entity data is not a compound").into());
        };
        let mut with_id = Compound::new();
        with_id.insert("id", Nbt::String(format!("minecraft:{name}")));
        with_id
            .0
            .extend(data.0.into_iter().filter(|(key, _)| key != "id"));
        Ok(Value::nbt(&Nbt::Compound(with_id)))
    }
}

/// 1.21.9's profile is a complete game profile or a partial one, plus a
/// skin override.
fn profile(cx: &mut Conv<'_>, value: &Value) -> Res<Value> {
    let fields = value.fields()?;
    if cx.up() {
        let skin = Value::Struct(vec![
            Value::none(),
            Value::none(),
            Value::none(),
            Value::none(),
        ]);
        let profile = match (fields[0].option()?, fields[1].option()?) {
            (Some(name), Some(id)) => Value::Either(
                true,
                Box::new(Value::Struct(vec![
                    id.clone(),
                    name.clone(),
                    fields[2].clone(),
                ])),
            ),
            _ => Value::Either(false, Box::new(value.clone())),
        };
        Ok(Value::Struct(vec![profile, skin]))
    } else {
        let Value::Either(complete, profile) = &fields[0] else {
            return Err(invalid("profile is not an either").into());
        };
        if fields[1]
            .fields()?
            .iter()
            .any(|patch| *patch != Value::none())
        {
            cx.lossy = true;
        }
        if *complete {
            let p = profile.fields()?;
            Ok(Value::Struct(vec![
                Value::some(p[1].clone()),
                Value::some(p[0].clone()),
                p[2].clone(),
            ]))
        } else {
            Ok((**profile).clone())
        }
    }
}

/// 26.3 split `swing_animation` into `attack_animation` and
/// `interact_animation`; going back keeps the attack one.
fn swing_merge(cx: &mut Conv<'_>, input: &mut Components, out: &mut Out) -> io::Result<()> {
    let _ = cx;
    if input.removes("attack_animation") && input.removes("interact_animation") {
        // Both come from removing `swing_animation`.
        input.removed.retain(|kind| *kind != "interact_animation");
    } else if input.removes("attack_animation") && input.get("interact_animation").is_none() {
        // Going back up removes both.
        out.absent.push("interact_animation");
    }
    let attack = input.take("attack_animation");
    let interact = input.take("interact_animation");
    match (attack, interact) {
        (Some(attack), interact) => {
            match interact {
                Some(interact) if interact != attack => out.lost.push(Component {
                    kind: "interact_animation",
                    value: interact,
                }),
                Some(_) => {}
                // Going back up gives both.
                None => out.absent.push("interact_animation"),
            }
            out.push("swing_animation", attack);
        }
        (None, Some(interact)) => out.lost.push(Component {
            kind: "interact_animation",
            value: interact,
        }),
        (None, None) => {}
    }
    Ok(())
}

/// 26.3 can point a random teleport's particles; the effect list otherwise
/// converts node by node.
pub(super) fn consume_effects_777(cx: &mut Conv<'_>, value: &Value) -> Res<Value> {
    let (old, new) = (&layouts::CONSUME_EFFECT_768, &layouts::CONSUME_EFFECT_777);
    let (source, target) = if cx.up() { (old, new) } else { (new, old) };
    let mut effects = Vec::new();
    for effect in value.items()? {
        let converted = match effect {
            Value::Variant("teleport_randomly", inner) => Value::Variant(
                "teleport_randomly",
                Box::new(if cx.up() {
                    Value::Struct(vec![(**inner).clone(), Value::Bool(false)])
                } else {
                    let fields = inner.fields()?;
                    if fields[1] != Value::Bool(false) {
                        cx.lossy = true;
                    }
                    fields[0].clone()
                }),
            ),
            effect => match convert_value(cx, source, target, effect) {
                Ok(effect) => effect,
                Err(Fail::Lost) => {
                    cx.lossy = true;
                    continue;
                }
                Err(error) => return Err(error),
            },
        };
        effects.push(converted);
    }
    Ok(Value::List(effects))
}

/// 26.3 turned the four sherd items (bricks where there is none) into
/// optional stack templates.
fn pot_decorations(cx: &mut Conv<'_>, value: &Value) -> Res<Value> {
    if cx.up() {
        let brick = data::version(cx.source)
            .registry("item")
            .and_then(|registry| registry.id("brick"));
        let items = value.items()?;
        if items.len() != 4 {
            // Going back always lists four, bricks where there is none.
            cx.lossy = true;
        }
        let mut sides = Vec::with_capacity(4);
        for index in 0..4 {
            sides.push(match items.get(index) {
                Some(Value::Id(id)) if Some(*id) != brick => match cx.map_id("item", *id) {
                    Some(id) => Value::some(Value::Stack(Some(Box::new(ItemStack {
                        id,
                        count: 1,
                        damage: 0,
                        tag: None,
                        components: Some(Components::default()),
                    })))),
                    None => {
                        cx.lossy = true;
                        Value::none()
                    }
                },
                _ => Value::none(),
            });
        }
        Ok(Value::Struct(sides))
    } else {
        let brick = cx.target_id("item", "brick").ok_or(Fail::Lost)?;
        let mut items = Vec::with_capacity(4);
        for side in value.fields()? {
            items.push(match side.option()? {
                Some(Value::Stack(Some(stack))) => {
                    if stack.count != 1 || stack.components.as_ref().is_some_and(|c| !c.is_empty())
                    {
                        cx.lossy = true;
                    }
                    match cx.map_id("item", stack.id) {
                        Some(id) => Value::Id(id),
                        None => {
                            cx.lossy = true;
                            Value::Id(brick)
                        }
                    }
                }
                _ => Value::Id(brick),
            });
        }
        Ok(Value::List(items))
    }
}
