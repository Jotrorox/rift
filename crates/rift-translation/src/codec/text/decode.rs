//! Reading text components as each protocol's client reads them: Gson's
//! `Component.Serializer` before 1.20.3 (JSON) and the `ComponentSerialization`
//! codec from 1.20.3 on (NBT on the wire, JSON in login and in old item tags).
//!
//! Reading is at least as lenient as vanilla: what a version's client
//! accepts is accepted, and invalid optional parts (an unknown colour, an
//! unknown click action, an unparsable item tag) are dropped the way the
//! client drops them. Content vanilla cannot read at all fails.
use super::{component::*, json::Json, parse_uuid, snbt, version as v};
use crate::{
    nbt::{Compound, Nbt},
    wire::{MAX_DEPTH, invalid},
};
use std::io;

/// A value of either tree a component can be read from.
#[derive(Clone, Copy)]
pub(super) enum Node<'a> {
    Json(&'a Json),
    Nbt(&'a Nbt),
}

impl<'a> Node<'a> {
    /// A string; numbers and booleans count, as Gson's `getAsString` takes
    /// any primitive.
    fn string(self) -> Option<String> {
        match self {
            Node::Json(json) => json.primitive_string(),
            Node::Nbt(Nbt::String(text)) => Some(text.clone()),
            Node::Nbt(Nbt::List(_) | Nbt::Compound(_)) => None,
            Node::Nbt(other) if other.kind() <= 6 => Some(primitive_text(other)),
            Node::Nbt(_) => None,
        }
    }

    fn strict_string(self) -> Option<&'a str> {
        match self {
            Node::Json(json) => json.as_str(),
            Node::Nbt(nbt) => nbt.as_str(),
        }
    }

    /// A boolean flag: JSON booleans, numbers (non-zero) and `"true"`.
    fn flag(self) -> Option<bool> {
        match self {
            Node::Json(Json::Bool(value)) => Some(*value),
            Node::Json(json @ Json::Number(_)) => json.as_f64().map(|value| value != 0.0),
            Node::Json(Json::String(text)) => Some(text.eq_ignore_ascii_case("true")),
            Node::Nbt(Nbt::String(text)) => Some(text.eq_ignore_ascii_case("true")),
            Node::Nbt(nbt) => nbt.as_i64().map(|value| value as i8 != 0),
            Node::Json(_) => None,
        }
    }

    fn int(self) -> Option<i64> {
        match self {
            Node::Json(json) => json.as_i64(),
            Node::Nbt(Nbt::String(text)) => text.trim().parse().ok(),
            Node::Nbt(nbt) => nbt.as_i64(),
        }
    }

    fn get(self, key: &str) -> Option<Node<'a>> {
        match self {
            Node::Json(json) => json
                .get(key)
                .filter(|value| **value != Json::Null)
                .map(Node::Json),
            Node::Nbt(Nbt::Compound(compound)) => compound.get(key).map(Node::Nbt),
            Node::Nbt(_) => None,
        }
    }

    fn is_map(self) -> bool {
        matches!(
            self,
            Node::Json(Json::Object(_)) | Node::Nbt(Nbt::Compound(_))
        )
    }

    /// List elements; NBT compounds that only wrap a value under `""` (a
    /// list of mixed types) are unwrapped, as vanilla's list reader does.
    fn list(self) -> Option<Vec<Node<'a>>> {
        match self {
            Node::Json(Json::Array(values)) => Some(values.iter().map(Node::Json).collect()),
            Node::Nbt(Nbt::List(values)) => Some(
                values
                    .iter()
                    .map(|value| match value {
                        Nbt::Compound(compound) if compound.len() == 1 => {
                            Node::Nbt(compound.get("").unwrap_or(value))
                        }
                        _ => Node::Nbt(value),
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    /// The value as NBT, for parts passed through opaquely (dialogs, click
    /// payloads, profiles, item components).
    pub(super) fn to_nbt(self) -> Option<Nbt> {
        match self {
            Node::Nbt(nbt) => Some(nbt.clone()),
            Node::Json(json) => json_to_nbt(json),
        }
    }
}

/// Convert a JSON value to NBT as vanilla's `JsonOps` to `NbtOps`
/// conversion does: booleans become bytes, mixed lists are wrapped, and
/// numbers keep the narrowest of int, long or double.
pub fn json_to_nbt(json: &Json) -> Option<Nbt> {
    Some(match json {
        Json::Null => return None,
        Json::Bool(value) => Nbt::Byte(i8::from(*value)),
        Json::Number(text) => {
            if let Ok(value) = text.parse::<i32>() {
                Nbt::Int(value)
            } else if let Ok(value) = text.parse::<i64>() {
                Nbt::Long(value)
            } else {
                Nbt::Double(text.parse().ok()?)
            }
        }
        Json::String(text) => Nbt::String(text.clone()),
        Json::Array(values) => {
            super::encode::homogenize(values.iter().filter_map(json_to_nbt).collect())
        }
        Json::Object(members) => Nbt::Compound(Compound(
            members
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), json_to_nbt(value)?)))
                .collect(),
        )),
    })
}

/// Read one component in `protocol`'s format.
pub fn decode(protocol: i32, node: Node<'_>) -> io::Result<Component> {
    component(protocol, node, 0)
}

fn component(protocol: i32, node: Node<'_>, depth: usize) -> io::Result<Component> {
    // Each level becomes up to two NBT levels (compound and list); vanilla
    // reads at most 512.
    if depth > MAX_DEPTH * 2 {
        return Err(invalid("text component nesting limit exceeded"));
    }
    if let Some(list) = node.list() {
        // A list is its first element with the others appended; vanilla's
        // old reader turns an empty list into no component at all.
        let mut elements = list.into_iter();
        let Some(first) = elements.next() else {
            return Ok(Component::text(""));
        };
        let mut first = component(protocol, first, depth + 1)?;
        for element in elements {
            first.extra.push(component(protocol, element, depth + 1)?);
        }
        return Ok(first);
    }
    if !node.is_map() {
        return node
            .string()
            .map(Component::text)
            .ok_or_else(|| invalid("text component is not a string, list or object"));
    }
    let content = content(protocol, node, depth)?;
    let mut extra = Vec::new();
    if let Some(children) = node.get("extra") {
        let children = children
            .list()
            .ok_or_else(|| invalid("text component extra is not a list"))?;
        for child in children {
            extra.push(component(protocol, child, depth + 1)?);
        }
    }
    Ok(Component {
        content,
        style: style(protocol, node, depth)?,
        extra,
    })
}

fn optional_component(
    protocol: i32,
    node: Option<Node<'_>>,
    depth: usize,
) -> io::Result<Option<Box<Component>>> {
    node.map(|node| component(protocol, node, depth + 1).map(Box::new))
        .transpose()
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Translate,
    Keybind,
    Score,
    Selector,
    Nbt,
    Object,
}

fn content(protocol: i32, node: Node<'_>, depth: usize) -> io::Result<Content> {
    // From 1.20.3 a `type` field selects the content; without one the
    // first content whose fields are present wins, in each era's order.
    if protocol >= v::NBT_ENCODING
        && let Some(kind) = node.get("type")
    {
        let kind = match kind.strict_string() {
            Some("text") => Kind::Text,
            Some("translatable") => Kind::Translate,
            Some("keybind") => Kind::Keybind,
            Some("score") => Kind::Score,
            Some("selector") => Kind::Selector,
            Some("nbt") => Kind::Nbt,
            Some("object") if protocol >= v::OBJECT_CONTENT => Kind::Object,
            _ => return Err(invalid("unknown text component type")),
        };
        return typed_content(protocol, node, kind, depth)?
            .ok_or_else(|| invalid("text component lacks the fields of its type"));
    }
    let order: &[Kind] = if protocol >= v::NBT_ENCODING {
        &[
            Kind::Text,
            Kind::Translate,
            Kind::Keybind,
            Kind::Score,
            Kind::Selector,
            Kind::Nbt,
            Kind::Object,
        ]
    } else {
        &[
            Kind::Text,
            Kind::Translate,
            Kind::Score,
            Kind::Selector,
            Kind::Keybind,
            Kind::Nbt,
        ]
    };
    for kind in order {
        if let Some(content) = typed_content(protocol, node, *kind, depth)? {
            return Ok(content);
        }
    }
    Err(invalid("text component has no content"))
}

/// The content of `kind` when its fields are present (`None` otherwise).
fn typed_content(
    protocol: i32,
    node: Node<'_>,
    kind: Kind,
    depth: usize,
) -> io::Result<Option<Content>> {
    let string = |key: &str| node.get(key).and_then(Node::string);
    Ok(match kind {
        Kind::Text => node
            .get("text")
            .map(|text| Content::Text(text.string().unwrap_or_default())),
        Kind::Translate => match string("translate") {
            None => None,
            Some(key) => {
                let mut args = Vec::new();
                // NBT writes a list of numbers of one type as an array tag.
                let numbers: Option<Vec<Nbt>> = match node.get("with") {
                    Some(Node::Nbt(Nbt::ByteArray(values))) => {
                        Some(values.iter().map(|v| Nbt::Byte(*v as i8)).collect())
                    }
                    Some(Node::Nbt(Nbt::IntArray(values))) => {
                        Some(values.iter().copied().map(Nbt::Int).collect())
                    }
                    Some(Node::Nbt(Nbt::LongArray(values))) => {
                        Some(values.iter().copied().map(Nbt::Long).collect())
                    }
                    _ => None,
                };
                if let Some(numbers) = numbers {
                    for number in &numbers {
                        args.push(argument(protocol, Node::Nbt(number), depth)?);
                    }
                } else if let Some(with) = node.get("with") {
                    for arg in with
                        .list()
                        .ok_or_else(|| invalid("translation arguments are not a list"))?
                    {
                        args.push(argument(protocol, arg, depth)?);
                    }
                }
                Some(Content::Translate {
                    key,
                    fallback: string("fallback"),
                    args,
                })
            }
        },
        Kind::Keybind => string("keybind").map(Content::Keybind),
        Kind::Score => match node.get("score") {
            Some(score) if score.is_map() => {
                let name = score.get("name").and_then(Node::string);
                let objective = score.get("objective").and_then(Node::string);
                match (name, objective) {
                    (Some(name), Some(objective)) => Some(Content::Score {
                        name,
                        objective,
                        value: score.get("value").and_then(Node::string),
                    }),
                    _ => return Err(invalid("score component lacks a name or objective")),
                }
            }
            _ => None,
        },
        Kind::Selector => match string("selector") {
            None => None,
            Some(pattern) => Some(Content::Selector {
                pattern,
                separator: separator(protocol, node, depth)?,
            }),
        },
        Kind::Nbt => match string("nbt") {
            None => None,
            Some(path) => {
                let source = nbt_source(protocol, node)?
                    .ok_or_else(|| invalid("nbt component lacks a source"))?;
                Some(Content::Nbt {
                    path,
                    interpret: node.get("interpret").and_then(Node::flag).unwrap_or(false),
                    plain: node.get("plain").and_then(Node::flag).unwrap_or(false),
                    separator: separator(protocol, node, depth)?,
                    source,
                })
            }
        },
        Kind::Object => {
            if protocol < v::OBJECT_CONTENT {
                None
            } else {
                match object_info(node)? {
                    None => None,
                    Some(object) => Some(Content::Object {
                        object,
                        fallback: optional_component(protocol, node.get("fallback"), depth)?,
                    }),
                }
            }
        }
    })
}

fn separator(protocol: i32, node: Node<'_>, depth: usize) -> io::Result<Option<Box<Component>>> {
    optional_component(protocol, node.get("separator"), depth)
}

fn nbt_source(protocol: i32, node: Node<'_>) -> io::Result<Option<NbtSource>> {
    let field = |key: &str| node.get(key).and_then(Node::string);
    let block = || field("block").map(NbtSource::Block);
    let entity = || field("entity").map(NbtSource::Entity);
    let storage = || field("storage").map(NbtSource::Storage);
    if protocol >= v::NBT_ENCODING {
        if let Some(source) = node.get("source") {
            return Ok(match source.strict_string() {
                Some("block") => block(),
                Some("entity") => entity(),
                Some("storage") => storage(),
                _ => return Err(invalid("unknown nbt component source")),
            });
        }
        return Ok(entity().or_else(block).or_else(storage));
    }
    Ok(block().or_else(entity).or_else(storage))
}

fn object_info(node: Node<'_>) -> io::Result<Option<ObjectInfo>> {
    let atlas = || {
        node.get("sprite")
            .and_then(Node::string)
            .map(|sprite| ObjectInfo::Atlas {
                atlas: node.get("atlas").and_then(Node::string),
                sprite,
            })
    };
    let player = || {
        node.get("player")
            .and_then(Node::to_nbt)
            .map(|player| ObjectInfo::Player {
                player,
                hat: node.get("hat").and_then(Node::flag).unwrap_or(true),
            })
    };
    if let Some(kind) = node.get("object") {
        return Ok(match kind.strict_string() {
            Some("atlas") => atlas(),
            Some("player") => player(),
            _ => return Err(invalid("unknown object component type")),
        });
    }
    Ok(atlas().or_else(player))
}

fn argument(protocol: i32, node: Node<'_>, depth: usize) -> io::Result<Arg> {
    if protocol >= v::NBT_ENCODING {
        match node {
            Node::Nbt(nbt) if nbt.kind() <= 6 || nbt.kind() == 8 => {
                return Ok(Arg::Primitive(nbt.clone()));
            }
            Node::Json(Json::Bool(value)) => {
                return Ok(Arg::Primitive(Nbt::Byte(i8::from(*value))));
            }
            Node::Json(json @ Json::Number(_)) => {
                if let Some(value) = json_to_nbt(json) {
                    return Ok(Arg::Primitive(value));
                }
            }
            Node::Json(Json::String(text)) => return Ok(Arg::Primitive(Nbt::String(text.clone()))),
            _ => {}
        }
    }
    component(protocol, node, depth + 1).map(|component| Arg::Component(Box::new(component)))
}

fn style(protocol: i32, node: Node<'_>, depth: usize) -> io::Result<Style> {
    let flag = |key: &str| node.get(key).and_then(Node::flag);
    let color = node
        .get("color")
        .and_then(Node::strict_string)
        .and_then(|name| Color::parse(name, true));
    let shadow_color = node
        .get("shadow_color")
        .and_then(|value| match value.list() {
            Some(channels) if channels.len() == 4 => {
                let channel = |index: usize| {
                    let value = match channels[index] {
                        Node::Json(json) => json.as_f64(),
                        Node::Nbt(nbt) => nbt.as_f64(),
                    }
                    .unwrap_or(0.0);
                    ((value.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xff
                };
                Some((channel(3) << 24 | channel(0) << 16 | channel(1) << 8 | channel(2)) as i32)
            }
            Some(_) => None,
            None => value.int().map(|value| value as i32),
        });
    let (click_key, hover_key) = if protocol >= v::SNAKE_CASE_EVENTS {
        (["click_event", "clickEvent"], ["hover_event", "hoverEvent"])
    } else {
        (["clickEvent", "click_event"], ["hoverEvent", "hover_event"])
    };
    let click = click_key
        .iter()
        .find_map(|key| node.get(key))
        .filter(|node| node.is_map())
        .and_then(click_event);
    let hover = match hover_key.iter().find_map(|key| node.get(key)) {
        Some(hover) if hover.is_map() => hover_event(protocol, hover, depth)?,
        _ => None,
    };
    Ok(Style {
        color,
        shadow_color,
        bold: flag("bold"),
        italic: flag("italic"),
        underlined: flag("underlined"),
        strikethrough: flag("strikethrough"),
        obfuscated: flag("obfuscated"),
        click,
        hover,
        insertion: node.get("insertion").and_then(Node::string),
        font: node.get("font").and_then(Node::string),
    })
}

fn click_event(node: Node<'_>) -> Option<ClickEvent> {
    let action = node.get("action")?.strict_string()?;
    let field = |key: &str| node.get(key).and_then(Node::string);
    // The old `value` field and the 1.21.5 per-action fields.
    let text = |key: &str| field(key).or_else(|| field("value"));
    Some(match action {
        "open_url" => ClickEvent::OpenUrl(text("url")?),
        "open_file" => ClickEvent::OpenFile(text("path")?),
        "run_command" => ClickEvent::RunCommand(text("command")?),
        "suggest_command" => ClickEvent::SuggestCommand(text("command")?),
        "change_page" => ClickEvent::ChangePage(text("page")?),
        "copy_to_clipboard" => ClickEvent::CopyToClipboard(field("value")?),
        "show_dialog" => ClickEvent::ShowDialog(node.get("dialog")?.to_nbt()?),
        "custom" => ClickEvent::Custom {
            id: field("id")?,
            payload: node.get("payload").and_then(Node::to_nbt),
        },
        _ => return None,
    })
}

fn hover_event(protocol: i32, node: Node<'_>, depth: usize) -> io::Result<Option<HoverEvent>> {
    let Some(action) = node.get("action").and_then(Node::strict_string) else {
        return Ok(None);
    };
    if protocol >= v::SNAKE_CASE_EVENTS {
        // 1.21.5: fields inline; `value` is the show_text component.
        return Ok(match action {
            "show_text" => match node.get("value") {
                Some(value) => Some(HoverEvent::ShowText(Box::new(component(
                    protocol,
                    value,
                    depth + 1,
                )?))),
                None => None,
            },
            "show_item" => item_contents(protocol, node).map(HoverEvent::ShowItem),
            "show_entity" => {
                let kind = node.get("id").and_then(Node::string);
                let id = node.get("uuid").and_then(uuid_node);
                match (kind, id) {
                    (Some(kind), Some(id)) => Some(HoverEvent::ShowEntity(EntityInfo {
                        kind,
                        id: EntityId::Uuid(id),
                        name: optional_component(protocol, node.get("name"), depth)?,
                    })),
                    _ => None,
                }
            }
            _ => None,
        });
    }
    if protocol >= v::HOVER_CONTENTS
        && let Some(contents) = node.get("contents")
    {
        return Ok(match action {
            "show_text" => Some(HoverEvent::ShowText(Box::new(component(
                protocol,
                contents,
                depth + 1,
            )?))),
            "show_item" => match contents.strict_string() {
                Some(id) => Some(HoverEvent::ShowItem(ItemInfo {
                    id: id.to_owned(),
                    count: 1,
                    damage: None,
                    tag: None,
                    components: None,
                })),
                None if contents.is_map() => {
                    item_contents(protocol, contents).map(HoverEvent::ShowItem)
                }
                None => None,
            },
            "show_entity" if contents.is_map() => {
                let kind = contents.get("type").and_then(Node::string);
                let id = contents.get("id").and_then(uuid_node);
                match (kind, id) {
                    (Some(kind), Some(id)) => Some(HoverEvent::ShowEntity(EntityInfo {
                        kind,
                        id: EntityId::Uuid(id),
                        name: match contents.get("name") {
                            // A broken name drops only the name.
                            Some(name) => component(protocol, name, depth + 1).ok().map(Box::new),
                            None => None,
                        },
                    })),
                    _ => None,
                }
            }
            _ => None,
        });
    }
    // The legacy form: `value` is a component, whose text is SNBT for items
    // and entities.
    let Some(value) = node.get("value") else {
        return Ok(None);
    };
    let value = component(protocol, value, depth + 1)?;
    Ok(match action {
        "show_text" => Some(HoverEvent::ShowText(Box::new(value))),
        "show_item" => legacy_item(protocol, &value.plain_text()),
        "show_entity" => legacy_entity(protocol, &value.plain_text(), depth),
        _ => None,
    })
}

fn uuid_node(node: Node<'_>) -> Option<u128> {
    match node {
        Node::Nbt(Nbt::IntArray(ints)) if ints.len() == 4 => Some(super::uuid_from_ints(ints)),
        Node::Json(Json::Array(values)) if values.len() == 4 => {
            let ints: Option<Vec<i32>> = values
                .iter()
                .map(|value| value.as_i64().map(|value| value as i32))
                .collect();
            Some(super::uuid_from_ints(&ints?))
        }
        _ => parse_uuid(node.strict_string()?),
    }
}

/// `show_item` contents as an object: `id`, `count` and either the SNBT
/// `tag` (before 1.20.5) or `components` (from 1.20.5).
fn item_contents(protocol: i32, node: Node<'_>) -> Option<ItemInfo> {
    let id = node.get("id").and_then(Node::string)?;
    let count = node
        .get("count")
        .and_then(Node::int)
        .map_or(1, |count| count as i32);
    let tag = if protocol < v::ITEM_COMPONENTS {
        // An unparsable tag drops the tag, as vanilla's lenient field does.
        node.get("tag").and_then(|tag| match tag {
            Node::Nbt(Nbt::Compound(compound)) => Some(compound.clone()),
            _ => snbt::parse_compound(protocol, &tag.string()?).ok(),
        })
    } else {
        None
    };
    let components = if protocol >= v::ITEM_COMPONENTS {
        node.get("components")
            .and_then(Node::to_nbt)
            .and_then(|nbt| match nbt {
                Nbt::Compound(compound) => Some(compound),
                _ => None,
            })
    } else {
        None
    };
    Some(ItemInfo {
        id,
        count,
        damage: None,
        tag,
        components,
    })
}

/// A legacy `show_item` value: the item stack as SNBT.
fn legacy_item(protocol: i32, text: &str) -> Option<HoverEvent> {
    let stack = snbt::parse_compound(protocol, text).ok()?;
    let id = stack.str("id")?.to_owned();
    let count = stack.get("Count").and_then(Nbt::as_i64).unwrap_or(1) as i32;
    let damage = if protocol < v::FLATTENING {
        stack
            .get("Damage")
            .and_then(Nbt::as_i64)
            .map(|value| value as i16)
    } else {
        None
    };
    let tag = stack.compound("tag").cloned();
    Some(HoverEvent::ShowItem(ItemInfo {
        id,
        count,
        damage,
        tag,
        components: None,
    }))
}

/// A legacy `show_entity` value: `{name:"<json>",type:"..",id:".."}`.
fn legacy_entity(protocol: i32, text: &str, depth: usize) -> Option<HoverEvent> {
    let tag = snbt::parse_compound(protocol, text).ok()?;
    let kind = tag.str("type").unwrap_or("minecraft:pig").to_owned();
    let id = tag.str("id").unwrap_or_default();
    // The name is legacy text before 1.13 and JSON from then on.
    let name = tag
        .str("name")
        .and_then(|name| {
            if protocol < v::FLATTENING {
                return Some(super::legacy::from_legacy(name));
            }
            let json = Json::parse(name).ok()?;
            component(protocol, Node::Json(&json), depth + 1).ok()
        })
        .map(Box::new);
    Some(HoverEvent::ShowEntity(EntityInfo {
        kind,
        id: parse_uuid(id).map_or_else(|| EntityId::Text(id.to_owned()), EntityId::Uuid),
        name,
    }))
}
