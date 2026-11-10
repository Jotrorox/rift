//! Chat across the signing era (1.19 to 1.19.3): the chat type registries
//! `login` carries, decorating a message the way the receiving client would,
//! and timestamps for messages older clients send without one.
//!
//! Message signatures cannot survive any of the era's format changes, so the
//! steps deliver translated player chat as decorated system chat and send
//! backends unsigned messages; such backends need
//! `enforce-secure-profile=false`.
use crate::nbt::{Compound, Nbt};
use serde_json::{Map, Value, json};

/// How one chat type displays a message in chat: a translation of its
/// parameters with a style.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoration {
    pub translation_key: String,
    pub parameters: Vec<String>,
    /// Style keys as JSON text component fields (`color`, `bold`, ...).
    pub style: Map<String, Value>,
}

/// One entry of a chat type registry, reduced to what display needs.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatType {
    /// `None` when the type shows nothing in chat (narration only) or shows
    /// the content undecorated.
    pub chat: Option<Decoration>,
    /// Whether the type shows in chat at all, decorated or not.
    pub shows_in_chat: bool,
    /// 1.19 only: the type shows above the hotbar instead of in chat.
    pub overlay: bool,
}

/// The chat types a backend announced in `login`, by network ID.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChatTypes {
    types: Vec<(i32, ChatType)>,
}

impl ChatTypes {
    /// Read the `minecraft:chat_type` registry of a `login` registry codec.
    /// 1.19 nests each display's decoration under `decoration` and may show
    /// a type as an `overlay`; 1.19.1 and later put the decoration directly
    /// under `chat`. Entries that do not parse are skipped.
    pub fn from_codec(codec: &Compound) -> Self {
        let entries = registry_entries(codec, "minecraft:chat_type").unwrap_or_default();
        let types = entries
            .iter()
            .filter_map(|entry| {
                let entry = entry.as_compound()?;
                let id = entry.int("id")?;
                let element = entry.compound("element")?;
                let display = element.compound("chat");
                let overlay = element.compound("overlay");
                let decoration = display
                    .or(overlay)
                    .and_then(|display| display.compound("decoration").or(Some(display)))
                    .and_then(decoration);
                Some((
                    id,
                    ChatType {
                        chat: decoration,
                        shows_in_chat: display.is_some() || overlay.is_some(),
                        overlay: display.is_none() && overlay.is_some(),
                    },
                ))
            })
            .collect();
        Self { types }
    }

    pub fn get(&self, id: i32) -> Option<&ChatType> {
        self.types
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, chat_type)| chat_type)
    }

    /// The text component a client shows for a message of chat type `id`,
    /// or `None` when the type shows nothing in chat. A type the backend
    /// never announced falls back to `<sender> content`, so a message is
    /// never lost to a registry the step could not read.
    pub fn display(
        &self,
        id: i32,
        sender: &Value,
        target: Option<&Value>,
        content: &Value,
    ) -> Option<Value> {
        match self.get(id) {
            Some(ChatType {
                chat: Some(decoration),
                ..
            }) => Some(decoration.apply(sender, target, content)),
            Some(ChatType {
                shows_in_chat: true,
                ..
            }) => Some(content.clone()),
            Some(_) => None,
            None => Some(
                Decoration {
                    translation_key: "chat.type.text".into(),
                    parameters: vec!["sender".into(), "content".into()],
                    style: Map::new(),
                }
                .apply(sender, target, content),
            ),
        }
    }

    /// Whether type `id` shows above the hotbar (1.19's `game_info`).
    pub fn is_overlay(&self, id: i32) -> bool {
        self.get(id).is_some_and(|chat_type| chat_type.overlay)
    }
}

fn decoration(tag: &Compound) -> Option<Decoration> {
    let translation_key = tag.str("translation_key")?.to_owned();
    let parameters = tag
        .list("parameters")
        .unwrap_or_default()
        .iter()
        .filter_map(|parameter| Some(parameter.as_str()?.to_owned()))
        .collect();
    let mut style = Map::new();
    if let Some(tag) = tag.compound("style") {
        for (key, value) in tag.iter() {
            match (key, value) {
                ("color" | "insertion" | "font", Nbt::String(text)) => {
                    style.insert(key.to_owned(), Value::String(text.clone()));
                }
                ("bold" | "italic" | "underlined" | "strikethrough" | "obfuscated", value) => {
                    if let Some(flag) = value.as_i64() {
                        style.insert(key.to_owned(), Value::Bool(flag != 0));
                    }
                }
                _ => {}
            }
        }
    }
    Some(Decoration {
        translation_key,
        parameters,
        style,
    })
}

impl Decoration {
    /// A translatable component with the parameters filled in, as the
    /// client builds it from the chat type.
    pub fn apply(&self, sender: &Value, target: Option<&Value>, content: &Value) -> Value {
        let empty = json!({"text": ""});
        let with: Vec<Value> = self
            .parameters
            .iter()
            .map(|parameter| match parameter.as_str() {
                "sender" => sender.clone(),
                "content" => content.clone(),
                // 1.19 calls the target `team_name`; later versions `target`.
                _ => target.unwrap_or(&empty).clone(),
            })
            .collect();
        let mut component = Map::new();
        component.insert(
            "translate".into(),
            Value::String(self.translation_key.clone()),
        );
        component.insert("with".into(), Value::Array(with));
        for (key, value) in &self.style {
            component.insert(key.clone(), value.clone());
        }
        Value::Object(component)
    }
}

/// The entries (`{name, id, element}` compounds) of one registry in a
/// `login` registry codec.
pub fn registry_entries<'a>(codec: &'a Compound, registry: &str) -> Option<&'a [Nbt]> {
    codec.compound(registry)?.list("value")
}

/// Mutable entries of one registry in a `login` registry codec.
pub fn registry_entries_mut<'a>(
    codec: &'a mut Compound,
    registry: &str,
) -> Option<&'a mut Vec<Nbt>> {
    codec
        .compound_mut(registry)?
        .get_mut("value")?
        .as_list_mut()
}

/// Parse a JSON text component as a client would; text that is not JSON is
/// shown literally.
pub fn parse_component(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|_| json!({"text": text}))
}

/// A plain string as a text component.
pub fn literal(text: &str) -> Value {
    json!({"text": text})
}

/// The registry entry of one chat type in the layout `protocol` uses.
fn entry(name: &str, id: i32, element: Compound) -> Nbt {
    Nbt::Compound(Compound(vec![
        ("name".into(), Nbt::String(format!("minecraft:{name}"))),
        ("id".into(), Nbt::Int(id)),
        ("element".into(), Nbt::Compound(element)),
    ]))
}

fn decoration_tag(key: &str, parameters: &[&str], style: Option<Compound>) -> Compound {
    let mut tag = Compound::new();
    tag.insert("translation_key", key);
    if let Some(style) = style {
        tag.insert("style", style);
    }
    tag.insert(
        "parameters",
        Nbt::List(parameters.iter().map(|name| Nbt::from(*name)).collect()),
    );
    tag
}

fn gray_italic() -> Compound {
    Compound(vec![
        ("color".into(), Nbt::from("gray")),
        ("italic".into(), Nbt::Byte(1)),
    ])
}

/// The chat type registry a vanilla 1.19 server sends, for 1.19 clients
/// whose backend speaks another format. IDs: `system` is 1, `game_info` 2.
pub fn vanilla_1_19() -> Compound {
    let display = |key: &str, parameters: &[&str], style: Option<Compound>| {
        let mut style = style;
        Compound(vec![(
            "decoration".into(),
            Nbt::Compound(decoration_tag(
                key,
                parameters,
                Some(style.take().unwrap_or_default()),
            )),
        )])
    };
    let narration = |priority: &str, decoration: Option<Compound>| {
        let mut tag = Compound::new();
        tag.insert("priority", priority);
        if let Some(decoration) = decoration {
            tag.insert("decoration", decoration);
        }
        tag
    };
    let narrate = || {
        Some(decoration_tag(
            "chat.type.text.narrate",
            &["sender", "content"],
            Some(Compound::new()),
        ))
    };
    let chat_entry = |key: &str, parameters: &[&str], style: Option<Compound>| {
        Compound(vec![
            (
                "chat".into(),
                Nbt::Compound(display(key, parameters, style)),
            ),
            (
                "narration".into(),
                Nbt::Compound(narration("chat", narrate())),
            ),
        ])
    };
    let mut emote = chat_entry("chat.type.emote", &["sender", "content"], None);
    emote.insert(
        "narration",
        narration(
            "chat",
            Some(decoration_tag(
                "chat.type.emote",
                &["sender", "content"],
                Some(Compound::new()),
            )),
        ),
    );
    let entries = vec![
        entry(
            "chat",
            0,
            chat_entry("chat.type.text", &["sender", "content"], None),
        ),
        entry(
            "system",
            1,
            Compound(vec![
                ("chat".into(), Nbt::Compound(Compound::new())),
                ("narration".into(), Nbt::Compound(narration("system", None))),
            ]),
        ),
        entry(
            "game_info",
            2,
            Compound(vec![("overlay".into(), Nbt::Compound(Compound::new()))]),
        ),
        entry(
            "say_command",
            3,
            chat_entry("chat.type.announcement", &["sender", "content"], None),
        ),
        entry(
            "msg_command",
            4,
            chat_entry(
                "commands.message.display.incoming",
                &["sender", "content"],
                Some(gray_italic()),
            ),
        ),
        entry(
            "team_msg_command",
            5,
            chat_entry(
                "chat.type.team.text",
                &["team_name", "sender", "content"],
                None,
            ),
        ),
        entry("emote_command", 6, emote),
        entry(
            "tellraw_command",
            7,
            Compound(vec![
                ("chat".into(), Nbt::Compound(Compound::new())),
                ("narration".into(), Nbt::Compound(narration("chat", None))),
            ]),
        ),
    ];
    registry("minecraft:chat_type", entries)
}

/// The 1.19 chat type IDs of [`vanilla_1_19`] that steps write.
pub const SYSTEM_1_19: i32 = 1;
pub const GAME_INFO_1_19: i32 = 2;

/// The chat type registry a vanilla 1.19.1/1.19.2 server sends.
pub fn vanilla_1_19_1() -> Compound {
    let element = |key: &str, parameters: &[&str], style: Option<Compound>, narration: &str| {
        Compound(vec![
            (
                "chat".into(),
                Nbt::Compound(decoration_tag(key, parameters, style)),
            ),
            (
                "narration".into(),
                Nbt::Compound(decoration_tag(narration, &["sender", "content"], None)),
            ),
        ])
    };
    let narrate = "chat.type.text.narrate";
    let entries = vec![
        entry(
            "chat",
            0,
            element("chat.type.text", &["sender", "content"], None, narrate),
        ),
        entry(
            "say_command",
            1,
            element(
                "chat.type.announcement",
                &["sender", "content"],
                None,
                narrate,
            ),
        ),
        entry(
            "msg_command_incoming",
            2,
            element(
                "commands.message.display.incoming",
                &["sender", "content"],
                Some(gray_italic()),
                narrate,
            ),
        ),
        entry(
            "msg_command_outgoing",
            3,
            element(
                "commands.message.display.outgoing",
                &["target", "content"],
                Some(gray_italic()),
                narrate,
            ),
        ),
        entry(
            "team_msg_command_incoming",
            4,
            element(
                "chat.type.team.text",
                &["target", "sender", "content"],
                None,
                narrate,
            ),
        ),
        entry(
            "team_msg_command_outgoing",
            5,
            element(
                "chat.type.team.sent",
                &["target", "sender", "content"],
                None,
                narrate,
            ),
        ),
        entry(
            "emote_command",
            6,
            element(
                "chat.type.emote",
                &["sender", "content"],
                None,
                "chat.type.emote",
            ),
        ),
    ];
    registry("minecraft:chat_type", entries)
}

fn registry(kind: &str, entries: Vec<Nbt>) -> Compound {
    Compound(vec![
        ("type".into(), Nbt::from(kind)),
        ("value".into(), Nbt::List(entries)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decorations_follow_their_parameters_and_style() {
        let mut codec = Compound::new();
        codec.insert("minecraft:chat_type", vanilla_1_19());
        let types = ChatTypes::from_codec(&codec);
        let sender = literal("Alex");
        let content = literal("hi");
        assert_eq!(
            types.display(3, &sender, None, &content),
            Some(json!({"translate": "chat.type.announcement", "with": [sender, content]}))
        );
        assert_eq!(
            types.display(4, &sender, None, &content),
            Some(json!({"translate": "commands.message.display.incoming",
                        "with": [sender, content], "color": "gray", "italic": true}))
        );
        // System messages show undecorated, game info only above the hotbar.
        assert_eq!(
            types.display(1, &sender, None, &content),
            Some(content.clone())
        );
        assert!(types.is_overlay(2) && !types.is_overlay(1));
        // An unknown type still shows the message.
        assert_eq!(
            types.display(99, &sender, None, &content),
            Some(json!({"translate": "chat.type.text", "with": [sender, content]}))
        );

        let mut codec = Compound::new();
        codec.insert("minecraft:chat_type", vanilla_1_19_1());
        let types = ChatTypes::from_codec(&codec);
        let target = literal("Steve");
        assert_eq!(
            types.display(3, &sender, Some(&target), &content),
            Some(json!({"translate": "commands.message.display.outgoing",
                        "with": [target, content], "color": "gray", "italic": true}))
        );
        assert!(!types.is_overlay(0));
    }
}
