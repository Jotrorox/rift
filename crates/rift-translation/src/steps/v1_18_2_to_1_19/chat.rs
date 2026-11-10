//! Chat across the signing era (1.19 to 1.19.3): the chat type registries
//! `login` carries, decorating a message the way the receiving client would,
//! and timestamps for messages older clients send without one.
//!
//! Message signatures cannot survive any of the era's format changes, so the
//! steps deliver translated player chat as decorated system chat and send
//! backends unsigned messages; such backends need
//! `enforce-secure-profile=false`.
use crate::{
    codec::{
        Shared,
        registries::{self, Side},
    },
    nbt::{Compound, Nbt},
};
use serde_json::{Map, Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

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

impl ChatType {
    /// One `minecraft:chat_type` element. 1.19 nests each display's
    /// decoration under `decoration` and may show a type as an `overlay`;
    /// 1.19.1 and later put the decoration directly under `chat`.
    pub fn from_element(element: &Compound) -> Self {
        let display = element.compound("chat");
        let overlay = element.compound("overlay");
        let decoration = display
            .or(overlay)
            .and_then(|display| display.compound("decoration").or(Some(display)))
            .and_then(decoration);
        ChatType {
            chat: decoration,
            shows_in_chat: display.is_some() || overlay.is_some(),
            overlay: display.is_none() && overlay.is_some(),
        }
    }
}

impl ChatTypes {
    /// The chat types `side` of a step was last sent, as the registries
    /// codec recorded them (the side's vanilla types before any `login`).
    pub fn from_shared(shared: &Shared, side: Side) -> Self {
        let Some(synced) = registries::synced(shared, side, "chat_type") else {
            let protocol = registries::protocol(shared, side);
            let types = registries::defaults::get(protocol, "chat_type")
                .map(|vanilla| {
                    vanilla
                        .entries
                        .iter()
                        .filter_map(|entry| {
                            let element = entry.element.as_compound()?;
                            Some((entry.id, ChatType::from_element(element)))
                        })
                        .collect()
                })
                .unwrap_or_default();
            return Self { types };
        };
        let types = synced
            .iter()
            .filter_map(|(id, name)| {
                let element = registries::element(shared, side, "chat_type", name)?;
                Some((id, ChatType::from_element(element)))
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

/// The 1.19 chat type IDs of vanilla's `system` and `game_info` types.
pub const SYSTEM_1_19: i32 = 1;
pub const GAME_INFO_1_19: i32 = 2;

/// The network ID of the `system` or (`overlay`) `game_info` chat type on
/// `side`, which system chat to a 1.19 client names: the registries codec
/// gives a 1.19 client these vanilla types even where its backend lacks
/// them.
pub fn system_type(shared: &Shared, side: Side, overlay: bool) -> i32 {
    let (name, vanilla) = if overlay {
        ("game_info", GAME_INFO_1_19)
    } else {
        ("system", SYSTEM_1_19)
    };
    registries::id_of(shared, side, "chat_type", name).unwrap_or(vanilla)
}

/// Message timestamps for clients that send none: the wall clock in
/// milliseconds, never going backwards (backends reject chat out of order).
#[derive(Debug, Default)]
pub struct Clock {
    last: i64,
}

impl Clock {
    pub fn now(&mut self) -> i64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64);
        self.last = self.last.max(now);
        self.last
    }
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

    #[test]
    fn clock_never_goes_backwards() {
        let mut clock = Clock { last: i64::MAX - 1 };
        assert_eq!(clock.now(), i64::MAX - 1);
        let mut clock = Clock::default();
        let first = clock.now();
        assert!(first > 1_600_000_000_000 && clock.now() >= first);
    }
}
