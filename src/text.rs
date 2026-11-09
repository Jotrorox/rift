//! Player-facing text that Rift sends on someone else's behalf: chat
//! messages, action bars, kicks and the reasons of denied logins and
//! transfers. Each is plain text or a JSON text component, so plugins can
//! add colours, line breaks, hover text and click events.
//!
//! The JSON forms are explicit everywhere: a Lua table, a JSON object or
//! list in a bus reply, or a WIT `*-json` function or action. Rift never
//! guesses JSON from a plain string, so a plugin that relays what a player
//! typed cannot be tricked into sending click events.
use crate::protocol::{self, ProtocolVersion};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};
use std::{fmt, io};

/// Plain text keeps the contract's long-standing limit.
pub const MAX_PLAIN_BYTES: usize = 1024;
/// A component's compact JSON form, the same bound as tab lists and boss bars.
pub const MAX_COMPONENT_BYTES: usize = 32 * 1024;

/// Plain text or a JSON text component. Serialized untagged: a JSON string
/// is plain text, an object or a list is a component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Text {
    Plain(String),
    Component(Json),
}

impl From<&str> for Text {
    fn from(text: &str) -> Self {
        Self::Plain(text.to_owned())
    }
}

impl From<String> for Text {
    fn from(text: String) -> Self {
        Self::Plain(text)
    }
}

impl fmt::Display for Text {
    /// The words without formatting, for logs and error messages.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(text) => formatter.write_str(text),
            Self::Component(component) => {
                let mut plain = String::new();
                flatten(component, &mut plain, 0);
                formatter.write_str(&plain)
            }
        }
    }
}

impl Text {
    /// Plain text of 1..=1024 bytes.
    pub fn plain(text: impl Into<String>, label: &str) -> Result<Self, String> {
        let text = text.into();
        if text.is_empty() || text.len() > MAX_PLAIN_BYTES {
            return Err(format!("{label} must contain 1..=1024 bytes"));
        }
        Ok(Self::Plain(text))
    }

    /// A JSON text component: a non-empty string, list or object of at
    /// most 32 KiB that also has a network NBT form.
    pub fn component(component: Json, label: &str) -> Result<Self, String> {
        let empty = match &component {
            Json::String(text) => text.is_empty(),
            Json::Array(items) => items.is_empty(),
            Json::Object(fields) => fields.is_empty(),
            _ => {
                return Err(format!(
                    "{label} must be a JSON text component (a string, list or object)"
                ));
            }
        };
        if empty {
            return Err(format!("{label} must not be empty"));
        }
        if component.to_string().len() > MAX_COMPONENT_BYTES {
            return Err(format!("{label} exceeds 32 KiB"));
        }
        // The NBT form drops nulls but clients reading JSON reject them.
        if has_null(&component, 0) {
            return Err(format!("{label} must not contain null"));
        }
        protocol::validate_component(&component).map_err(|error| format!("{label}: {error}"))?;
        Ok(Self::Component(component))
    }

    /// The text of a WIT `*-json` argument: a JSON text component, like a
    /// tab list header. Text that does not start with `{`, `[` or `"` is
    /// shown as written.
    pub fn json(text: &str, label: &str) -> Result<Self, String> {
        if text.len() > MAX_COMPONENT_BYTES {
            return Err(format!("{label} exceeds 32 KiB"));
        }
        if !text.starts_with(['{', '[', '"']) {
            return Self::component(Json::String(text.to_owned()), label);
        }
        let component = serde_json::from_str(text)
            .map_err(|error| format!("{label}: invalid JSON: {error}"))?;
        Self::component(component, label)
    }

    /// A string (or a number) is plain text; a table is a chat component,
    /// as in `rift.server_list` and `players.tab_list`.
    pub fn lua(value: mlua::Value, label: &str) -> Result<Self, String> {
        match value {
            mlua::Value::String(text) => Self::plain(
                text.to_str()
                    .map_err(|_| format!("{label} must be valid UTF-8"))?
                    .to_owned(),
                label,
            ),
            // Lua's own string coercion, which string arguments always had.
            mlua::Value::Integer(number) => Self::plain(number.to_string(), label),
            mlua::Value::Number(number) => Self::plain(number.to_string(), label),
            value @ mlua::Value::Table(_) => {
                Self::component(crate::server_list::chat_component(value, label)?, label)
            }
            value => Err(format!(
                "{label} must be a string or a chat component table, got {}",
                value.type_name()
            )),
        }
    }

    /// Check a value that arrived already built, such as a bus reply.
    pub fn validate(&self, label: &str) -> Result<(), String> {
        match self {
            Self::Plain(text) => Self::plain(text.as_str(), label).map(drop),
            Self::Component(component) => Self::component(component.clone(), label).map(drop),
        }
    }

    /// `prefix` followed by this text, keeping the text's formatting.
    pub fn prefixed(&self, prefix: &str) -> Self {
        match self {
            Self::Plain(text) => Self::Plain(format!("{prefix}{text}")),
            Self::Component(component) => {
                Self::Component(serde_json::json!({"text": prefix, "extra": [component]}))
            }
        }
    }
}

fn has_null(value: &Json, depth: usize) -> bool {
    depth <= 128
        && match value {
            Json::Null => true,
            Json::Array(items) => items.iter().any(|item| has_null(item, depth + 1)),
            Json::Object(fields) => fields.values().any(|item| has_null(item, depth + 1)),
            _ => false,
        }
}

/// Append the visible words of a component; formatting and events are
/// dropped and a translation shows its fallback or its key.
fn flatten(component: &Json, out: &mut String, depth: usize) {
    if depth > 64 {
        return;
    }
    match component {
        Json::String(text) => out.push_str(text),
        Json::Array(items) => items.iter().for_each(|item| flatten(item, out, depth + 1)),
        Json::Object(fields) => {
            let text = ["text", "fallback", "translate", "keybind", "selector"]
                .iter()
                .find_map(|key| fields.get(*key).and_then(Json::as_str));
            if let Some(text) = text {
                out.push_str(text);
            }
            if let Some(Json::Array(extra)) = fields.get("extra") {
                extra.iter().for_each(|item| flatten(item, out, depth + 1));
            }
        }
        Json::Number(number) => out.push_str(&number.to_string()),
        Json::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Json::Null => {}
    }
}

/// The payload of a `PermissionDenied` error whose reason a decision hook
/// supplied, so the player sees it with its formatting.
#[derive(Debug)]
pub struct Denied(pub Text);

impl fmt::Display for Denied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for Denied {}

/// A `PermissionDenied` error carrying `reason`.
pub fn denied(reason: Text) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, Denied(reason))
}

/// What to show the player for `error`: a hook's reason as it was given,
/// otherwise the error's message.
pub fn reason(error: &io::Error) -> Text {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Denied>())
        .map_or_else(|| Text::Plain(error.to_string()), |denied| denied.0.clone())
}

/// How a client spells hover and click events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    /// Before 1.16: `hoverEvent.value`.
    Value,
    /// 1.16 to 1.21.4: `hoverEvent.contents` and `clickEvent.value`.
    Contents,
    /// 1.21.5 and later: `hover_event` and `click_event` with typed fields.
    Typed,
}

impl Dialect {
    fn of(version: ProtocolVersion) -> Self {
        match version.number() {
            ..735 => Self::Value,
            735..770 => Self::Contents,
            _ => Self::Typed,
        }
    }
}

/// `component` with its hover and click events spelled the way `version`
/// reads them. Plugins may write either form, or both for one event; the
/// form the client does not read is dropped, as is an event it cannot
/// express. Returns `None` when nothing changes.
pub(crate) fn for_client(component: &Json, version: Option<ProtocolVersion>) -> Option<Json> {
    let dialect = Dialect::of(version?);
    if !mentions_events(component, 0) {
        return None;
    }
    let mut converted = component.clone();
    convert(&mut converted, dialect, 0);
    (converted != *component).then_some(converted)
}

const EVENT_KEYS: [&str; 4] = ["clickEvent", "click_event", "hoverEvent", "hover_event"];

fn mentions_events(value: &Json, depth: usize) -> bool {
    depth <= 128
        && match value {
            Json::Array(items) => items.iter().any(|item| mentions_events(item, depth + 1)),
            Json::Object(fields) => {
                EVENT_KEYS.iter().any(|key| fields.contains_key(*key))
                    || fields.values().any(|item| mentions_events(item, depth + 1))
            }
            _ => false,
        }
}

fn convert(value: &mut Json, dialect: Dialect, depth: usize) {
    if depth > 128 {
        return;
    }
    match value {
        Json::Array(items) => items
            .iter_mut()
            .for_each(|item| convert(item, dialect, depth + 1)),
        Json::Object(fields) => {
            convert_click(fields, dialect);
            convert_hover(fields, dialect);
            fields
                .values_mut()
                .for_each(|item| convert(item, dialect, depth + 1));
        }
        _ => {}
    }
}

/// `clickEvent: {action, value}` before 1.21.5; afterwards `click_event`
/// with `url`, `command`, `page` or `value` depending on the action.
fn convert_click(fields: &mut Map<String, Json>, dialect: Dialect) {
    let typed = fields.remove("click_event");
    let legacy = fields.remove("clickEvent");
    let converted = match dialect {
        Dialect::Typed => typed.or_else(|| legacy.as_ref().and_then(typed_click)),
        _ => legacy.or_else(|| typed.as_ref().and_then(legacy_click)),
    };
    if let Some(event) = converted {
        let key = if dialect == Dialect::Typed {
            "click_event"
        } else {
            "clickEvent"
        };
        fields.insert(key.to_owned(), event);
    }
}

fn click_field(action: &str) -> Option<&'static str> {
    Some(match action {
        "open_url" => "url",
        "run_command" | "suggest_command" => "command",
        "change_page" => "page",
        "copy_to_clipboard" => "value",
        _ => return None,
    })
}

fn typed_click(event: &Json) -> Option<Json> {
    let action = event.get("action")?.as_str()?;
    let field = click_field(action)?;
    let value = event.get("value")?;
    let value = if field == "page" {
        match value {
            Json::String(page) => Json::from(page.trim().parse::<i32>().ok()?),
            Json::Number(_) => value.clone(),
            _ => return None,
        }
    } else {
        value.clone()
    };
    Some(serde_json::json!({"action": action, field: value}))
}

fn legacy_click(event: &Json) -> Option<Json> {
    let action = event.get("action")?.as_str()?;
    let value = match event.get(click_field(action)?)? {
        Json::Number(page) => Json::String(page.to_string()),
        value => value.clone(),
    };
    Some(serde_json::json!({"action": action, "value": value}))
}

/// `show_text` keeps its text in `value` (before 1.16), `contents` (1.16 to
/// 1.21.4) or `value` again (1.21.5 and later). Items and entities move
/// their fields out of `contents` from 1.21.5, where an entity's type is
/// `id` and its UUID `uuid`.
fn convert_hover(fields: &mut Map<String, Json>, dialect: Dialect) {
    let typed = fields.remove("hover_event");
    let legacy = fields.remove("hoverEvent");
    let converted = match dialect {
        Dialect::Typed => typed.or_else(|| legacy.as_ref().and_then(typed_hover)),
        Dialect::Contents => legacy.or_else(|| typed.as_ref().and_then(contents_hover)),
        Dialect::Value => legacy
            .and_then(|event| value_hover(&event).or(Some(event)))
            .or_else(|| {
                typed
                    .as_ref()
                    .and_then(contents_hover)
                    .and_then(|event| value_hover(&event))
            }),
    };
    if let Some(event) = converted {
        let key = if dialect == Dialect::Typed {
            "hover_event"
        } else {
            "hoverEvent"
        };
        fields.insert(key.to_owned(), event);
    }
}

fn typed_hover(event: &Json) -> Option<Json> {
    let action = event.get("action")?.as_str()?;
    let contents = event.get("contents").or_else(|| event.get("value"))?;
    match action {
        "show_text" => Some(serde_json::json!({"action": action, "value": contents})),
        "show_item" => {
            let mut typed = match contents {
                Json::String(id) => Map::from_iter([("id".to_owned(), Json::String(id.clone()))]),
                Json::Object(item) => item.clone(),
                _ => return None,
            };
            typed.insert("action".into(), action.into());
            Some(Json::Object(typed))
        }
        "show_entity" => {
            let entity = contents.as_object()?;
            let mut typed = Map::from_iter([("action".to_owned(), Json::from(action))]);
            for (from, to) in [("type", "id"), ("id", "uuid"), ("name", "name")] {
                if let Some(value) = entity.get(from) {
                    typed.insert(to.to_owned(), value.clone());
                }
            }
            Some(Json::Object(typed))
        }
        _ => None,
    }
}

fn contents_hover(event: &Json) -> Option<Json> {
    let action = event.get("action")?.as_str()?;
    let fields = event.as_object()?;
    let contents = match action {
        "show_text" => fields.get("value")?.clone(),
        "show_item" => Json::Object(
            fields
                .iter()
                .filter(|(key, _)| *key != "action")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        "show_entity" => {
            let mut entity = Map::new();
            for (from, to) in [("id", "type"), ("uuid", "id"), ("name", "name")] {
                if let Some(value) = fields.get(from) {
                    entity.insert(to.to_owned(), value.clone());
                }
            }
            Json::Object(entity)
        }
        _ => return None,
    };
    Some(serde_json::json!({"action": action, "contents": contents}))
}

/// Before 1.16 only `show_text` survives the move from `contents`; item
/// and entity tooltips there are SNBT strings that Rift does not build.
fn value_hover(event: &Json) -> Option<Json> {
    if event.get("value").is_some() {
        return Some(event.clone());
    }
    match event.get("action")?.as_str()? {
        "show_text" => Some(serde_json::json!({
            "action": "show_text",
            "value": event.get("contents")?,
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn version(number: i32) -> Option<ProtocolVersion> {
        Some(ProtocolVersion::new(number).unwrap())
    }

    #[test]
    fn serde_reads_strings_as_plain_text_and_objects_or_lists_as_components() {
        assert_eq!(
            serde_json::from_value::<Text>(json!("hi")).unwrap(),
            Text::Plain("hi".into())
        );
        let component = json!({"text": "hi", "color": "red"});
        assert_eq!(
            serde_json::from_value::<Text>(component.clone()).unwrap(),
            Text::Component(component.clone())
        );
        assert_eq!(
            serde_json::to_value(Text::Component(component.clone())).unwrap(),
            component
        );
        assert_eq!(serde_json::to_value(Text::from("hi")).unwrap(), json!("hi"));
    }

    #[test]
    fn limits_apply_to_each_form() {
        assert!(Text::plain("", "message").is_err());
        assert!(Text::plain("x".repeat(1024), "message").is_ok());
        assert_eq!(
            Text::plain("x".repeat(1025), "message").unwrap_err(),
            "message must contain 1..=1024 bytes"
        );
        let long = json!({"text": "x".repeat(4000)});
        assert!(Text::component(long, "message").is_ok());
        assert_eq!(
            Text::component(json!({"text": "x".repeat(32 * 1024)}), "message").unwrap_err(),
            "message exceeds 32 KiB"
        );
        for empty in [json!(""), json!([]), json!({})] {
            assert_eq!(
                Text::component(empty, "kick reason").unwrap_err(),
                "kick reason must not be empty"
            );
        }
        assert!(Text::component(json!(3), "message").is_err());
        assert_eq!(
            Text::component(json!({"text": "a", "extra": [{"text": null}]}), "message")
                .unwrap_err(),
            "message must not contain null"
        );
        assert!(Text::Component(json!({})).validate("deny reason").is_err());
        assert!(Text::Plain("ok".into()).validate("deny reason").is_ok());
    }

    #[test]
    fn json_arguments_parse_components_and_keep_other_text() {
        assert_eq!(
            Text::json(r#"{"text":"Accept","color":"green"}"#, "message").unwrap(),
            Text::Component(json!({"text": "Accept", "color": "green"}))
        );
        assert_eq!(
            Text::json("[Rift] hello", "message").unwrap_err(),
            "message: invalid JSON: expected value at line 1 column 2"
        );
        assert_eq!(
            Text::json("hello", "message").unwrap(),
            Text::Component(json!("hello"))
        );
        assert!(Text::json("", "message").is_err());
    }

    #[test]
    fn display_flattens_components_for_logs() {
        let text = Text::Component(json!([
            {"text": "Banned", "color": "red", "extra": ["\n", {"translate": "x", "fallback": "Reason"}]},
            ": spam",
        ]));
        assert_eq!(text.to_string(), "Banned\nReason: spam");
        assert_eq!(
            text.prefixed("Unable to switch servers: ").to_string(),
            "Unable to switch servers: Banned\nReason: spam"
        );
        assert_eq!(Text::from("x").prefixed("a: "), Text::from("a: x"));
    }

    #[test]
    fn denials_keep_their_component_through_io_errors() {
        let component = Text::Component(json!({"text": "No", "color": "red"}));
        let error = denied(component.clone());
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "No");
        assert_eq!(reason(&error), component);
        assert_eq!(
            reason(&io::Error::other("plain")),
            Text::Plain("plain".into())
        );
    }

    #[test]
    fn events_follow_the_client_dialect() {
        let typed = json!({
            "text": "[Accept]",
            "click_event": {"action": "run_command", "command": "/party accept"},
            "hover_event": {"action": "show_text", "value": {"text": "Join", "color": "green"}},
            "extra": [{"text": "page", "click_event": {"action": "change_page", "page": 2}}],
        });
        let contents = json!({
            "text": "[Accept]",
            "clickEvent": {"action": "run_command", "value": "/party accept"},
            "hoverEvent": {"action": "show_text", "contents": {"text": "Join", "color": "green"}},
            "extra": [{"text": "page", "clickEvent": {"action": "change_page", "value": "2"}}],
        });
        assert_eq!(for_client(&typed, version(770)), None);
        assert_eq!(for_client(&typed, version(769)).unwrap(), contents);
        assert_eq!(for_client(&contents, version(770)).unwrap(), typed);
        assert_eq!(for_client(&contents, version(769)), None);
        let value = for_client(&typed, version(47)).unwrap();
        assert_eq!(
            value["hoverEvent"],
            json!({"action": "show_text", "value": {"text": "Join", "color": "green"}})
        );
        assert_eq!(value["clickEvent"], contents["clickEvent"]);
        assert_eq!(for_client(&typed, None), None);
        assert_eq!(for_client(&json!({"text": "plain"}), version(769)), None);
    }

    #[test]
    fn both_spellings_keep_the_one_the_client_reads() {
        let both = json!({
            "text": "x",
            "click_event": {"action": "open_url", "url": "https://new"},
            "clickEvent": {"action": "open_url", "value": "https://old"},
        });
        assert_eq!(
            for_client(&both, version(771)).unwrap(),
            json!({"text": "x", "click_event": {"action": "open_url", "url": "https://new"}})
        );
        assert_eq!(
            for_client(&both, version(767)).unwrap(),
            json!({"text": "x", "clickEvent": {"action": "open_url", "value": "https://old"}})
        );
    }

    #[test]
    fn items_and_entities_move_in_and_out_of_contents() {
        let typed = json!({"text": "x", "hover_event": {
            "action": "show_entity", "id": "minecraft:pig", "uuid": "0-0-0-0-1", "name": "Bob",
        }});
        let contents = json!({"text": "x", "hoverEvent": {"action": "show_entity", "contents": {
            "type": "minecraft:pig", "id": "0-0-0-0-1", "name": "Bob",
        }}});
        assert_eq!(for_client(&typed, version(769)).unwrap(), contents);
        assert_eq!(for_client(&contents, version(770)).unwrap(), typed);
        let item = json!({"text": "x", "hoverEvent": {"action": "show_item", "contents": "minecraft:stone"}});
        assert_eq!(
            for_client(&item, version(770)).unwrap(),
            json!({"text": "x", "hover_event": {"action": "show_item", "id": "minecraft:stone"}})
        );
    }

    #[test]
    fn events_a_client_cannot_express_are_dropped() {
        let dialog = json!({"text": "x", "click_event": {"action": "show_dialog", "dialog": "a"}});
        assert_eq!(
            for_client(&dialog, version(769)).unwrap(),
            json!({"text": "x"})
        );
        let page = json!({"text": "x", "clickEvent": {"action": "change_page", "value": "two"}});
        assert_eq!(
            for_client(&page, version(770)).unwrap(),
            json!({"text": "x"})
        );
        let item =
            json!({"text": "x", "hover_event": {"action": "show_item", "id": "minecraft:stone"}});
        assert_eq!(
            for_client(&item, version(47)).unwrap(),
            json!({"text": "x"})
        );
    }
}
