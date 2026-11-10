//! How each client reads a text component. Hover and click events change
//! their spelling at 1.16 and again at 1.21.5, and older clients lack hex
//! colours, shadows, fonts and some content, which are downsampled.
use crate::protocol::ProtocolVersion;
use serde_json::{Map, Value as Json};

/// What a client can show of a text component. Components are written in
/// the newest form; [`for_client`] rewrites them for older clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile(Option<ProtocolVersion>);

impl Profile {
    /// A client whose version Rift does not know, such as one still in the
    /// handshake: components are sent as written.
    pub const UNKNOWN: Self = Self(None);

    pub fn of(version: ProtocolVersion) -> Self {
        Self(Some(version))
    }

    pub fn version(self) -> Option<ProtocolVersion> {
        self.0
    }

    fn at_least(self, protocol: i32) -> bool {
        self.0.is_none_or(|version| version.number() >= protocol)
    }

    /// `#rrggbb` colours and `font` (1.16).
    pub fn hex_colors(self) -> bool {
        self.at_least(735)
    }

    /// `shadow_color` (1.21.4).
    pub fn shadow_color(self) -> bool {
        self.at_least(769)
    }

    /// `keybind` content (1.12).
    pub fn keybinds(self) -> bool {
        self.at_least(335)
    }

    /// `object` content, such as sprites and player heads (1.21.9).
    pub fn objects(self) -> bool {
        self.at_least(773)
    }

    /// The `copy_to_clipboard` click action (1.15).
    pub fn copy_to_clipboard(self) -> bool {
        self.at_least(573)
    }

    fn dialect(self) -> Option<Dialect> {
        self.0.map(Dialect::of)
    }
}

impl From<Option<ProtocolVersion>> for Profile {
    fn from(version: Option<ProtocolVersion>) -> Self {
        Self(version)
    }
}

impl From<ProtocolVersion> for Profile {
    fn from(version: ProtocolVersion) -> Self {
        Self(Some(version))
    }
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

/// `component` as `profile` reads it, or `None` when nothing changes:
///
/// - hover and click events are spelled the client's way. Plugins may write
///   either form, or both for one event; the form the client does not read
///   is dropped, as is an event it cannot express;
/// - hex colours become the nearest named colour before 1.16, where `font`
///   is dropped too;
/// - `shadow_color` is dropped before 1.21.4;
/// - `keybind` content shows the key's name before 1.12, and `object`
///   content is dropped before 1.21.9;
/// - `copy_to_clipboard` clicks are dropped before 1.15.
pub(crate) fn for_client(component: &Json, profile: impl Into<Profile>) -> Option<Json> {
    let profile = profile.into();
    let dialect = profile.dialect()?;
    let mut converted = component.clone();
    if mentions_events(component, 0) {
        convert(&mut converted, dialect, 0);
    }
    downsample(&mut converted, profile, 0);
    (converted != *component).then_some(converted)
}

/// `component` in Rift's canonical spelling, the 1.21.5 one: an event
/// written only as `clickEvent` or `hoverEvent` becomes `click_event` or
/// `hover_event`. A component that carries both spellings of an event keeps
/// both, so each client still gets the one written for it. Returns `None`
/// when nothing changes.
pub(crate) fn canonical(component: &Json) -> Option<Json> {
    fn walk(value: &mut Json, depth: usize) {
        if depth > 128 {
            return;
        }
        match value {
            Json::Array(items) => items.iter_mut().for_each(|item| walk(item, depth + 1)),
            Json::Object(fields) => {
                for (legacy, typed, convert) in [
                    (
                        "clickEvent",
                        "click_event",
                        typed_click as fn(&Json) -> Option<Json>,
                    ),
                    ("hoverEvent", "hover_event", typed_hover),
                ] {
                    if fields.contains_key(typed) {
                        continue;
                    }
                    if let Some(event) = fields.get(legacy).and_then(convert) {
                        fields.remove(legacy);
                        fields.insert(typed.into(), event);
                    }
                }
                for key in ["extra", "with", "separator"] {
                    if let Some(child) = fields.get_mut(key) {
                        walk(child, depth + 1);
                    }
                }
                if let Some(Json::Object(event)) = fields.get_mut("hover_event") {
                    for key in ["value", "name"] {
                        if let Some(child) = event.get_mut(key) {
                            walk(child, depth + 1);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if !mentions_events(component, 0) {
        return None;
    }
    let mut converted = component.clone();
    walk(&mut converted, 0);
    (converted != *component).then_some(converted)
}

/// `object` content (1.21.9) anywhere in `value`, such as a dialog, replaced
/// with empty text for clients before 1.21.9.
pub(crate) fn strip_objects(value: &mut Json, profile: Profile, depth: usize) {
    if profile.objects() || depth > 128 {
        return;
    }
    match value {
        Json::Array(items) => items
            .iter_mut()
            .for_each(|item| strip_objects(item, profile, depth + 1)),
        Json::Object(fields) => {
            if fields.get("object").is_some_and(Json::is_string) {
                for key in ["object", "atlas", "sprite", "player", "hat"] {
                    fields.remove(key);
                }
                fields.insert("text".into(), "".into());
            }
            fields
                .values_mut()
                .for_each(|item| strip_objects(item, profile, depth + 1));
        }
        _ => {}
    }
}

/// Rewrite styles and content an old client cannot show. Only component
/// positions are visited, so event payloads keep their fields.
fn downsample(value: &mut Json, profile: Profile, depth: usize) {
    if depth > 128 {
        return;
    }
    match value {
        Json::Array(items) => items
            .iter_mut()
            .for_each(|item| downsample(item, profile, depth + 1)),
        Json::Object(fields) => {
            if !profile.hex_colors() {
                if let Some(rgb) = fields
                    .get("color")
                    .and_then(Json::as_str)
                    .and_then(super::color::hex)
                {
                    fields.insert("color".into(), super::color::nearest(rgb).into());
                }
                fields.remove("font");
            }
            if !profile.shadow_color() {
                fields.remove("shadow_color");
            }
            if !profile.keybinds()
                && let Some(key) = fields.remove("keybind")
            {
                fields.insert("text".into(), key);
            }
            if !profile.objects() && fields.remove("object").is_some() {
                for key in ["atlas", "sprite", "player", "hat"] {
                    fields.remove(key);
                }
                fields.insert("text".into(), "".into());
            }
            if !profile.copy_to_clipboard() {
                for key in ["click_event", "clickEvent"] {
                    let copies = fields
                        .get(key)
                        .and_then(|event| event.get("action"))
                        .and_then(Json::as_str)
                        == Some("copy_to_clipboard");
                    if copies {
                        fields.remove(key);
                    }
                }
            }
            for key in ["extra", "with", "separator"] {
                if let Some(child) = fields.get_mut(key) {
                    downsample(child, profile, depth + 1);
                }
            }
            for key in ["hover_event", "hoverEvent"] {
                let Some(Json::Object(event)) = fields.get_mut(key) else {
                    continue;
                };
                if event.get("action").and_then(Json::as_str) != Some("show_text") {
                    continue;
                }
                for text in ["value", "contents"] {
                    if let Some(child) = event.get_mut(text) {
                        downsample(child, profile, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
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

    #[test]
    fn old_clients_get_what_they_can_show() {
        let modern = json!({
            "text": "a", "color": "#ff8800", "font": "minecraft:uniform", "shadow_color": 1,
            "extra": [
                {"keybind": "key.jump"},
                {"object": "atlas", "sprite": "item/diamond"},
                {"text": "c", "click_event": {"action": "copy_to_clipboard", "value": "x"}},
            ],
            "hover_event": {"action": "show_text", "value": {"text": "h", "color": "#5555ff"}},
        });
        assert_eq!(
            for_client(&modern, version(47)).unwrap(),
            json!({
                "text": "a", "color": "gold",
                "extra": [{"text": "key.jump"}, {"text": ""}, {"text": "c"}],
                "hoverEvent": {"action": "show_text", "value": {"text": "h", "color": "blue"}},
            })
        );
        let v1_16 = for_client(&modern, version(735)).unwrap();
        assert_eq!(v1_16["color"], "#ff8800");
        assert_eq!(v1_16["font"], "minecraft:uniform");
        assert!(v1_16.get("shadow_color").is_none());
        assert_eq!(v1_16["extra"][1], json!({"text": ""}));
        assert_eq!(
            v1_16["extra"][2]["clickEvent"],
            json!({"action": "copy_to_clipboard", "value": "x"})
        );
        let v1_21_4 = for_client(&modern, version(769)).unwrap();
        assert_eq!(v1_21_4["shadow_color"], 1);
        assert_eq!(for_client(&modern, version(773)), None);
        // Event payloads are not components and keep their fields.
        let custom = json!({"text": "x", "click_event": {"action": "custom", "id": "a:b", "payload": {"color": "#ffffff"}}});
        assert_eq!(for_client(&custom, version(773)), None);
    }

    #[test]
    fn legacy_events_become_canonical_unless_both_are_given() {
        let legacy = json!({"text": "x",
            "clickEvent": {"action": "open_url", "value": "https://a"},
            "extra": [{"text": "y", "hoverEvent": {"action": "show_text", "contents": "tip"}}]});
        assert_eq!(
            canonical(&legacy).unwrap(),
            json!({"text": "x",
                "click_event": {"action": "open_url", "url": "https://a"},
                "extra": [{"text": "y", "hover_event": {"action": "show_text", "value": "tip"}}]})
        );
        let both = json!({"text": "x",
            "click_event": {"action": "open_url", "url": "https://new"},
            "clickEvent": {"action": "open_url", "value": "https://old"}});
        assert_eq!(canonical(&both), None);
        assert_eq!(canonical(&json!({"text": "plain"})), None);
    }

    #[test]
    fn dialogs_lose_objects_before_1_21_9() {
        let mut dialog =
            json!({"type": "minecraft:notice", "title": {"object": "atlas", "sprite": "a"}});
        strip_objects(&mut dialog, Profile::from(version(771)), 0);
        assert_eq!(dialog["title"], json!({"text": ""}));
        let mut kept = json!({"title": {"object": "atlas", "sprite": "a"}});
        strip_objects(&mut kept, Profile::from(version(773)), 0);
        assert_eq!(kept["title"]["sprite"], "a");
    }
}
