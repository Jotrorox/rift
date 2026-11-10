//! Block entity types and the tag contents clients read, in chunks and in
//! `block_entity_data`.
//!
//! Before 1.18 a block entity packet names its kind by an action number and
//! its tag carries `id` and the position; from 1.18 the packet carries a
//! `block_entity_type` ID and the tag only the data. Tag contents that
//! changed between adjacent protocols and that clients render:
//!
//! - 1.16: skull owner UUIDs become int arrays.
//! - 1.18: spawner `SpawnData` wraps the entity (`{entity: {id: ...}}`).
//! - 1.20: signs hold `front_text` / `back_text` instead of `Text1..4`.
//! - 1.20.5: skulls hold a `profile` instead of `Owner`, banner patterns are
//!   named (`{pattern: "minecraft:creeper", color: "lime"}`).
//! - 1.21.5: sign lines and custom names are NBT text instead of JSON.
//!
//! Spawned entity IDs follow the entity type renames of each pair.
//!
//! INTEGRATION: text inside signs and names is converted between JSON and
//! NBT structurally here; changes to the component format itself (event
//! names in 1.21.5, for example) belong to the text codec, whose
//! `convert_json` / `convert_nbt` should be applied once it exists.
use super::{
    convert::with_metadata,
    model::{BlockEntity, Chunk},
};
use crate::{
    Direction,
    codec::Shared,
    data, mapping,
    nbt::{Compound, Nbt},
    wire::{Position, invalid},
};
use serde_json::Value;
use std::io;

/// Pre-1.18 `block_entity_data` actions by block entity type.
const ACTIONS: &[(&str, i32, i32, i32)] = &[
    // Name, action, first protocol, last protocol.
    ("mob_spawner", 1, 0, 756),
    ("command_block", 2, 0, 756),
    ("beacon", 3, 0, 756),
    ("skull", 4, 0, 756),
    ("flower_pot", 5, 0, 340),
    ("conduit", 5, 393, 756),
    ("banner", 6, 0, 756),
    ("structure_block", 7, 0, 756),
    ("end_gateway", 8, 0, 756),
    ("sign", 9, 0, 756),
    ("shulker_box", 10, 0, 756),
    ("bed", 11, 0, 756),
    ("jigsaw", 12, 477, 756),
    ("campfire", 13, 477, 756),
    ("beehive", 14, 573, 756),
];

/// The action a pre-1.18 `protocol` updates block entity `name` with.
pub fn action(protocol: i32, name: &str) -> Option<i32> {
    ACTIONS
        .iter()
        .find(|(known, _, first, last)| *known == name && (*first..=*last).contains(&protocol))
        .map(|(_, action, ..)| *action)
}

fn action_name(protocol: i32, action: i32) -> Option<&'static str> {
    ACTIONS
        .iter()
        .find(|(_, known, first, last)| *known == action && (*first..=*last).contains(&protocol))
        .map(|(name, ..)| *name)
}

/// Convert one `block_entity_data` packet's type and tag; `None` drops the
/// packet (the receiver has no such block entity).
pub fn convert_packet(
    shared: &mut Shared,
    direction: Direction,
    position: Position,
    kind: i32,
    nbt: Option<Nbt>,
) -> io::Result<Option<(i32, Option<Nbt>)>> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let name: String = if source >= 757 {
        match data::version(source)
            .registry("block_entity_type")
            .and_then(|registry| registry.name(kind))
        {
            Some(name) => name.to_owned(),
            None => return Ok(None),
        }
    } else {
        let named = nbt
            .as_ref()
            .and_then(Nbt::as_compound)
            .and_then(|tag| tag.str("id"))
            .map(|id| data::strip(id).to_owned());
        match named.or_else(|| action_name(source, kind).map(str::to_owned)) {
            Some(name) => name,
            None => return Ok(None),
        }
    };
    let name = rename_type(source, target, &name);
    let kind = if target >= 757 {
        match data::version(target)
            .registry("block_entity_type")
            .and_then(|registry| registry.id(&name))
        {
            Some(kind) => kind,
            None => return Ok(None),
        }
    } else {
        match action(target, &name) {
            Some(action) => action,
            None => return Ok(None),
        }
    };
    let nbt = match nbt {
        Some(Nbt::Compound(mut tag)) => {
            convert_content(shared, direction, &name, &mut tag)?;
            Some(Nbt::Compound(if target >= 757 {
                for key in ["id", "x", "y", "z"] {
                    tag.remove(key);
                }
                tag
            } else if source >= 757 {
                with_metadata(
                    Some(Nbt::Compound(tag)),
                    &name,
                    position.x,
                    position.y,
                    position.z,
                )
            } else {
                tag
            }))
        }
        Some(_) => return Err(invalid("block entity tag is not a compound")),
        // 1.18 sends an empty tag (an ownerless skull's) as none; older
        // clients load whatever tag comes and fail on a missing one.
        None if source >= 757 && target < 757 => Some(Nbt::Compound(with_metadata(
            None, &name, position.x, position.y, position.z,
        ))),
        None => None,
    };
    Ok(Some((kind, nbt)))
}

/// Block entity types that were renamed between adjacent protocols.
fn rename_type(source: i32, target: i32, name: &str) -> String {
    let _ = (source, target);
    name.to_owned()
}

/// Convert the tags of a converted chunk's block entities.
pub fn convert_chunk(
    shared: &mut Shared,
    direction: Direction,
    chunk: &mut Chunk,
) -> io::Result<()> {
    let target = shared.target(direction);
    if !needs_content(shared.source(direction), target) {
        return Ok(());
    }
    let registry =
        (target >= 757).then(|| data::version(target).expect_registry("block_entity_type"));
    for BlockEntity { kind, nbt, .. } in &mut chunk.block_entities {
        let Some(Nbt::Compound(tag)) = nbt else {
            continue;
        };
        let name = match registry {
            Some(registry) => registry.name(*kind).map(str::to_owned),
            None => tag.str("id").map(|id| data::strip(id).to_owned()),
        };
        if let Some(name) = name {
            convert_content(shared, direction, &name, tag)?;
        }
    }
    Ok(())
}

/// Protocols where a block entity tag's contents change.
const CONTENT_BOUNDARIES: &[i32] = &[735, 757, 763, 766, 770];

fn needs_content(source: i32, target: i32) -> bool {
    let (older, newer) = (source.min(target), source.max(target));
    CONTENT_BOUNDARIES
        .iter()
        .any(|boundary| older < *boundary && newer >= *boundary)
        || mapping::try_mapping(source, target, "entity_type")
            .is_some_and(|mapping| !super::packets::is_identity(mapping))
}

/// Whether the pair converts any block entity tag (for deciding which
/// handlers to install).
pub fn changes_content(source: i32, target: i32) -> bool {
    needs_content(source, target)
}

fn convert_content(
    shared: &mut Shared,
    direction: Direction,
    name: &str,
    tag: &mut Compound,
) -> io::Result<()> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let crossed = |boundary: i32| (source < boundary) != (target < boundary);
    let up = target > source;
    match name {
        "sign" | "hanging_sign" => {
            if crossed(763) {
                if up {
                    signs_up(tag);
                } else {
                    signs_down(tag);
                }
            }
            if crossed(770) {
                for side in ["front_text", "back_text"] {
                    if let Some(Nbt::List(messages)) = tag
                        .compound_mut(side)
                        .and_then(|text| text.get_mut("messages"))
                    {
                        for message in messages.iter_mut() {
                            *message = text(message, up)?;
                        }
                    }
                }
            }
        }
        "skull" => {
            if crossed(735)
                && let Some(Nbt::Compound(owner)) = tag.get_mut("Owner")
            {
                uuid_format(owner, "Id", up);
            }
            if crossed(766) {
                if up {
                    if let Some(Nbt::Compound(owner)) = tag.remove("Owner") {
                        tag.insert("profile", owner_to_profile(owner));
                    }
                } else if let Some(profile) = tag.remove("profile")
                    && let Some(owner) = profile_to_owner(profile)
                {
                    tag.insert("Owner", owner);
                }
            }
        }
        "banner" => {
            if crossed(766) {
                if up {
                    if let Some(Nbt::List(patterns)) = tag.remove("Patterns") {
                        tag.insert("patterns", patterns_up(patterns));
                    }
                } else if let Some(Nbt::List(patterns)) = tag.remove("patterns") {
                    tag.insert("Patterns", patterns_down(patterns));
                }
            }
        }
        "mob_spawner" | "trial_spawner" => spawner(shared, direction, tag),
        _ => {}
    }
    if crossed(770)
        && let Some(name) = tag.get_mut("CustomName")
    {
        *name = text(name, up)?;
    }
    Ok(())
}

/// A JSON text component held in a string, as NBT text (going up to 1.21.5)
/// or back.
fn text(value: &Nbt, up: bool) -> io::Result<Nbt> {
    if up {
        let Nbt::String(json) = value else {
            return Ok(value.clone());
        };
        let parsed: Value = serde_json::from_str(json).unwrap_or(Value::String(json.clone()));
        Ok(json_to_nbt(&parsed))
    } else {
        Ok(Nbt::String(nbt_to_json(value).to_string()))
    }
}

/// JSON as NBT the way text components are written: lists of mixed values
/// become lists of compounds (`{"text": ...}` for plain strings).
pub fn json_to_nbt(value: &Value) -> Nbt {
    match value {
        Value::Null => Nbt::String(String::new()),
        Value::Bool(value) => Nbt::Byte(i8::from(*value)),
        Value::Number(number) => match number.as_i64() {
            Some(value) if i32::try_from(value).is_ok() => Nbt::Int(value as i32),
            Some(value) => Nbt::Long(value),
            None => Nbt::Double(number.as_f64().unwrap_or(0.0)),
        },
        Value::String(value) => Nbt::String(value.clone()),
        Value::Array(values) => {
            let items: Vec<Nbt> = values.iter().map(json_to_nbt).collect();
            let kind = items.first().map(Nbt::kind);
            if items.iter().all(|item| Some(item.kind()) == kind) {
                Nbt::List(items)
            } else {
                Nbt::List(
                    items
                        .into_iter()
                        .map(|item| match item {
                            Nbt::Compound(_) => item,
                            Nbt::String(text) => Nbt::Compound(
                                [("text".to_owned(), Nbt::String(text))]
                                    .into_iter()
                                    .collect(),
                            ),
                            other => {
                                let text = nbt_to_json(&other).to_string();
                                Nbt::Compound(
                                    [("text".to_owned(), Nbt::String(text))]
                                        .into_iter()
                                        .collect(),
                                )
                            }
                        })
                        .collect(),
                )
            }
        }
        Value::Object(map) => Nbt::Compound(
            map.iter()
                .map(|(key, value)| (key.clone(), json_to_nbt(value)))
                .collect(),
        ),
    }
}

/// NBT text as its JSON form.
pub fn nbt_to_json(value: &Nbt) -> Value {
    match value {
        Nbt::Byte(value) => {
            if *value == 0 || *value == 1 {
                Value::Bool(*value == 1)
            } else {
                Value::from(*value)
            }
        }
        Nbt::Short(value) => Value::from(*value),
        Nbt::Int(value) => Value::from(*value),
        Nbt::Long(value) => Value::from(*value),
        Nbt::Float(value) => Value::from(*value),
        Nbt::Double(value) => Value::from(*value),
        Nbt::String(value) => Value::String(value.clone()),
        Nbt::List(values) => Value::Array(values.iter().map(nbt_to_json).collect()),
        Nbt::Compound(tag) => Value::Object(
            tag.iter()
                .map(|(key, value)| (key.to_owned(), nbt_to_json(value)))
                .collect(),
        ),
        Nbt::ByteArray(values) => Value::Array(
            values
                .iter()
                .map(|value| Value::from(*value as i8))
                .collect(),
        ),
        Nbt::IntArray(values) => {
            Value::Array(values.iter().map(|value| Value::from(*value)).collect())
        }
        Nbt::LongArray(values) => {
            Value::Array(values.iter().map(|value| Value::from(*value)).collect())
        }
    }
}

const EMPTY_LINE: &str = "\"\"";

fn side(messages: Vec<Nbt>, color: Option<Nbt>, glowing: Option<Nbt>) -> Compound {
    let mut text = Compound::new();
    text.insert("messages", messages);
    text.insert("color", color.unwrap_or_else(|| "black".into()));
    text.insert("has_glowing_text", glowing.unwrap_or(Nbt::Byte(0)));
    text
}

fn signs_up(tag: &mut Compound) {
    let messages: Vec<Nbt> = (1..=4)
        .map(|line| {
            tag.remove(&format!("Text{line}"))
                .unwrap_or_else(|| EMPTY_LINE.into())
        })
        .collect();
    let color = tag.remove("Color");
    let glowing = tag.remove("GlowingText");
    tag.insert("front_text", side(messages, color, glowing));
    let empty = (0..4).map(|_| Nbt::from(EMPTY_LINE)).collect();
    tag.insert("back_text", side(empty, None, None));
    tag.insert("is_waxed", Nbt::Byte(0));
}

fn signs_down(tag: &mut Compound) {
    let front = tag.remove("front_text");
    tag.remove("back_text");
    tag.remove("is_waxed");
    let Some(Nbt::Compound(mut front)) = front else {
        return;
    };
    if let Some(Nbt::List(messages)) = front.remove("messages") {
        for (line, message) in messages.into_iter().take(4).enumerate() {
            tag.insert(format!("Text{}", line + 1), message);
        }
    }
    if let Some(color) = front.remove("color") {
        tag.insert("Color", color);
    }
    if let Some(glowing) = front.remove("has_glowing_text") {
        tag.insert("GlowingText", glowing);
    }
}

/// A UUID as `"8-4-4-4-12"` hex (before 1.16) or four ints (1.16 on).
fn uuid_format(tag: &mut Compound, key: &str, to_ints: bool) {
    match (tag.get(key), to_ints) {
        (Some(Nbt::String(text)), true) => {
            if let Ok(value) = u128::from_str_radix(&text.replace('-', ""), 16) {
                tag.insert(key, uuid_ints(value));
            }
        }
        (Some(Nbt::IntArray(ints)), false) if ints.len() == 4 => {
            let value = ints
                .iter()
                .fold(0u128, |value, int| value << 32 | u128::from(*int as u32));
            let hex = format!("{value:032x}");
            tag.insert(
                key,
                format!(
                    "{}-{}-{}-{}-{}",
                    &hex[..8],
                    &hex[8..12],
                    &hex[12..16],
                    &hex[16..20],
                    &hex[20..]
                ),
            );
        }
        _ => {}
    }
}

fn uuid_ints(value: u128) -> Nbt {
    Nbt::IntArray(
        (0..4)
            .map(|index| (value >> (96 - 32 * index)) as u32 as i32)
            .collect(),
    )
}

fn owner_to_profile(owner: Compound) -> Compound {
    let mut profile = Compound::new();
    if let Some(name) = owner.get("Name") {
        profile.insert("name", name.clone());
    }
    if let Some(id) = owner.get("Id") {
        profile.insert("id", id.clone());
    }
    let mut properties = Vec::new();
    if let Some(Nbt::Compound(map)) = owner.get("Properties") {
        for (name, values) in map.iter() {
            for value in values.as_list().unwrap_or(&[]) {
                let Some(value) = value.as_compound() else {
                    continue;
                };
                let mut property = Compound::new();
                property.insert("name", name);
                if let Some(text) = value.get("Value") {
                    property.insert("value", text.clone());
                }
                if let Some(signature) = value.get("Signature") {
                    property.insert("signature", signature.clone());
                }
                properties.push(Nbt::Compound(property));
            }
        }
    }
    if !properties.is_empty() {
        profile.insert("properties", properties);
    }
    profile
}

fn profile_to_owner(profile: Nbt) -> Option<Compound> {
    let profile = match profile {
        Nbt::Compound(profile) => profile,
        // A bare name.
        Nbt::String(name) => {
            let mut owner = Compound::new();
            owner.insert("Name", name);
            return Some(owner);
        }
        _ => return None,
    };
    let mut owner = Compound::new();
    if let Some(id) = profile.get("id") {
        owner.insert("Id", id.clone());
    }
    if let Some(name) = profile.get("name") {
        owner.insert("Name", name.clone());
    }
    let mut properties = Compound::new();
    for property in profile.list("properties").unwrap_or(&[]) {
        let Some(property) = property.as_compound() else {
            continue;
        };
        let Some(name) = property.str("name") else {
            continue;
        };
        let mut value = Compound::new();
        if let Some(text) = property.get("value") {
            value.insert("Value", text.clone());
        }
        if let Some(signature) = property.get("signature") {
            value.insert("Signature", signature.clone());
        }
        match properties.get_mut(name) {
            Some(Nbt::List(values)) => values.push(Nbt::Compound(value)),
            _ => {
                properties.insert(name, vec![Nbt::Compound(value)]);
            }
        }
    }
    if !properties.is_empty() {
        owner.insert("Properties", properties);
    }
    Some(owner)
}

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

/// Banner pattern names and the short codes tags used before 1.20.5.
const PATTERNS: &[(&str, &str)] = &[
    ("base", "b"),
    ("square_bottom_left", "bl"),
    ("square_bottom_right", "br"),
    ("square_top_left", "tl"),
    ("square_top_right", "tr"),
    ("stripe_bottom", "bs"),
    ("stripe_top", "ts"),
    ("stripe_left", "ls"),
    ("stripe_right", "rs"),
    ("stripe_center", "cs"),
    ("stripe_middle", "ms"),
    ("stripe_downright", "drs"),
    ("stripe_downleft", "dls"),
    ("small_stripes", "ss"),
    ("cross", "cr"),
    ("straight_cross", "sc"),
    ("triangle_bottom", "bt"),
    ("triangle_top", "tt"),
    ("triangles_bottom", "bts"),
    ("triangles_top", "tts"),
    ("diagonal_left", "ld"),
    ("diagonal_up_right", "rd"),
    ("diagonal_up_left", "lud"),
    ("diagonal_right", "rud"),
    ("circle", "mc"),
    ("rhombus", "mr"),
    ("half_vertical", "vh"),
    ("half_horizontal", "hh"),
    ("half_vertical_right", "vhr"),
    ("half_horizontal_bottom", "hhb"),
    ("border", "bo"),
    ("curly_border", "cbo"),
    ("gradient", "gra"),
    ("gradient_up", "gru"),
    ("bricks", "bri"),
    ("globe", "glb"),
    ("creeper", "cre"),
    ("skull", "sku"),
    ("flower", "flo"),
    ("mojang", "moj"),
    ("piglin", "pig"),
];

fn patterns_up(patterns: Vec<Nbt>) -> Vec<Nbt> {
    patterns
        .into_iter()
        .filter_map(|pattern| {
            let pattern = pattern.as_compound()?;
            let name = PATTERNS
                .iter()
                .find(|(_, code)| Some(*code) == pattern.str("Pattern"))?
                .0;
            let color = DYES.get(usize::try_from(pattern.int("Color")?).ok()?)?;
            let mut new = Compound::new();
            new.insert("color", *color);
            new.insert("pattern", format!("minecraft:{name}"));
            Some(Nbt::Compound(new))
        })
        .collect()
}

fn patterns_down(patterns: Vec<Nbt>) -> Vec<Nbt> {
    patterns
        .into_iter()
        .filter_map(|pattern| {
            let pattern = pattern.as_compound()?;
            let name = data::strip(pattern.str("pattern")?);
            let code = PATTERNS.iter().find(|(known, _)| *known == name)?.1;
            let color = DYES
                .iter()
                .position(|dye| Some(*dye) == pattern.str("color"))?;
            let mut old = Compound::new();
            old.insert("Color", color as i32);
            old.insert("Pattern", code);
            Some(Nbt::Compound(old))
        })
        .collect()
}

/// An entity type's name in the receiver's registry.
fn rename_entity(source: i32, target: i32, id: &str) -> Option<String> {
    let Some(mapping) = mapping::try_mapping(source, target, "entity_type") else {
        return Some(id.to_owned());
    };
    let from = data::version(source).registry("entity_type")?.id(id)?;
    let to = mapping.get(from)?;
    Some(format!(
        "minecraft:{}",
        data::version(target).registry("entity_type")?.name(to)?
    ))
}

fn spawner(shared: &mut Shared, direction: Direction, tag: &mut Compound) {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let crossed = (source < 757) != (target < 757);
    let rename = |entity: &mut Compound| {
        if let Some(id) = entity.str("id").map(str::to_owned) {
            match rename_entity(source, target, &id) {
                Some(new) => {
                    entity.insert("id", new);
                }
                None => {
                    entity.insert("id", "minecraft:pig");
                }
            }
        }
    };
    if let Some(Nbt::Compound(data)) = tag.get_mut("SpawnData") {
        if crossed && target >= 757 {
            let entity = std::mem::take(data);
            data.insert("entity", entity);
        } else if crossed && let Some(Nbt::Compound(entity)) = data.remove("entity") {
            *data = entity;
        }
        let entity = if target >= 757 {
            data.compound_mut("entity")
        } else {
            Some(data)
        };
        if let Some(entity) = entity {
            rename(entity);
        }
    }
    // Clients only show `SpawnData`; potentials are the server's business.
    if crossed {
        tag.remove("SpawnPotentials");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_text_moves_between_lines_and_sides() {
        let mut tag = Compound::new();
        for line in 1..=4 {
            tag.insert(format!("Text{line}"), format!("{{\"text\":\"{line}\"}}"));
        }
        tag.insert("Color", "red");
        let original = tag.clone();
        signs_up(&mut tag);
        assert!(tag.contains("front_text") && !tag.contains("Text1"));
        signs_down(&mut tag);
        let mut expected = original;
        expected.insert("GlowingText", Nbt::Byte(0));
        for (key, value) in expected.iter() {
            assert_eq!(tag.get(key), Some(value), "{key}");
        }
    }

    #[test]
    fn json_text_and_nbt_text_convert_both_ways() {
        let json = r#"{"text":"hi","bold":true,"extra":["a",{"text":"b"}]}"#;
        let nbt = text(&Nbt::String(json.into()), true).unwrap();
        let tag = nbt.as_compound().unwrap();
        assert_eq!(tag.str("text"), Some("hi"));
        assert_eq!(tag.list("extra").unwrap().len(), 2);
        let back = text(&nbt, false).unwrap();
        let value: Value = serde_json::from_str(back.as_str().unwrap()).unwrap();
        assert_eq!(value["text"], "hi");
        assert_eq!(value["bold"], true);
        assert_eq!(value["extra"][0]["text"], "a");
    }

    #[test]
    fn skull_owners_and_banner_patterns_convert_both_ways() {
        let mut owner = Compound::new();
        owner.insert("Id", "069a79f4-44e9-4726-a5be-fca90e38aaf5");
        owner.insert("Name", "Notch");
        let mut ints = owner.clone();
        uuid_format(&mut ints, "Id", true);
        assert!(matches!(ints.get("Id"), Some(Nbt::IntArray(values)) if values.len() == 4));
        let mut back = ints.clone();
        uuid_format(&mut back, "Id", false);
        assert_eq!(back, owner);
        let profile = owner_to_profile(ints.clone());
        assert_eq!(profile.str("name"), Some("Notch"));
        assert_eq!(profile_to_owner(Nbt::Compound(profile)).unwrap(), ints);

        let mut pattern = Compound::new();
        pattern.insert("Color", 5);
        pattern.insert("Pattern", "cre");
        let up = patterns_up(vec![Nbt::Compound(pattern.clone())]);
        let tag = up[0].as_compound().unwrap();
        assert_eq!(
            (tag.str("pattern"), tag.str("color")),
            (Some("minecraft:creeper"), Some("lime"))
        );
        assert_eq!(patterns_down(up), vec![Nbt::Compound(pattern)]);
    }

    #[test]
    fn actions_follow_the_protocol() {
        assert_eq!(action(340, "flower_pot"), Some(5));
        assert_eq!(action(393, "flower_pot"), None);
        assert_eq!(action(393, "conduit"), Some(5));
        assert_eq!(action(498, "beehive"), None);
        assert_eq!(action(573, "beehive"), Some(14));
        assert_eq!(action(757, "sign"), None);
        assert_eq!(action_name(340, 5), Some("flower_pot"));
    }

    #[test]
    fn an_empty_1_18_tag_reaches_older_clients_as_id_and_position() {
        let mut shared = Shared::new(756, 757);
        let skull = data::version(757)
            .expect_registry("block_entity_type")
            .id("skull")
            .unwrap();
        let position = Position { x: 4, y: -60, z: 3 };
        let (kind, nbt) =
            convert_packet(&mut shared, Direction::Clientbound, position, skull, None)
                .unwrap()
                .unwrap();
        assert_eq!(Some(kind), action(756, "skull"));
        let tag = nbt.unwrap();
        let tag = tag.as_compound().unwrap();
        assert_eq!(tag.str("id"), Some("minecraft:skull"));
        assert_eq!(
            (tag.int("x"), tag.int("y"), tag.int("z")),
            (Some(4), Some(-60), Some(3))
        );
        // Between 1.18 versions the empty tag stays empty.
        let mut shared = Shared::new(757, 758);
        let (_, nbt) = convert_packet(&mut shared, Direction::Clientbound, position, skull, None)
            .unwrap()
            .unwrap();
        assert_eq!(nbt, None);
    }
}
