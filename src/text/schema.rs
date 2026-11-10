//! Semantic checks for JSON text components. The NBT check only proves that
//! a component can be encoded; this one proves that a client will read it,
//! so `"color": "orange"` or a `run_command` without a command is refused
//! here instead of disconnecting the player.
//!
//! Keys Rift knows are checked; unknown keys pass through, so new vanilla
//! fields keep working without a Rift release.
use super::color;
use serde_json::{Map, Value as Json};

/// Matches the depth the NBT encoder and serde_json accept.
const MAX_DEPTH: usize = 128;

const CONTENT: [&str; 7] = [
    "text",
    "translate",
    "keybind",
    "score",
    "selector",
    "nbt",
    "object",
];

const DECORATIONS: [&str; 5] = [
    "bold",
    "italic",
    "underlined",
    "strikethrough",
    "obfuscated",
];

/// Check `component`; the error names the offending key, such as
/// `extra[1].color must be one of the 16 colour names or #rrggbb`.
pub fn check(component: &Json) -> Result<(), String> {
    self::component(component, "", 0)
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// How an error names `path`; the root component is "component".
fn name(path: &str) -> &str {
    if path.is_empty() { "component" } else { path }
}

fn component(value: &Json, path: &str, depth: usize) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err(format!("{} nests too deeply", name(path)));
    }
    match value {
        Json::String(_) => Ok(()),
        Json::Array(items) => list(items, path, depth),
        Json::Object(fields) => object(fields, path, depth),
        _ => Err(format!(
            "{} must be a text component (a string, list or object)",
            name(path)
        )),
    }
}

fn list(items: &[Json], path: &str, depth: usize) -> Result<(), String> {
    if items.is_empty() {
        return Err(format!("{} must not be an empty list", name(path)));
    }
    items.iter().enumerate().try_for_each(|(index, item)| {
        component(item, &format!("{}[{index}]", name(path)), depth + 1)
    })
}

fn object(fields: &Map<String, Json>, path: &str, depth: usize) -> Result<(), String> {
    let field = |key: &str| join(path, key);
    let present: Vec<&str> = CONTENT
        .iter()
        .copied()
        .filter(|key| fields.contains_key(*key))
        .collect();
    match present.as_slice() {
        [content] => self::content(fields, content, path, depth)?,
        [] => {
            return Err(format!(
                "{} needs one of {}",
                name(path),
                CONTENT.join(", ")
            ));
        }
        [first, second, ..] => {
            return Err(format!(
                "{} has both {first} and {second}; use one",
                name(path)
            ));
        }
    }
    if let Some(value) = fields.get("color") {
        let valid = value
            .as_str()
            .is_some_and(|text| color::parse(text).is_some());
        if !valid {
            return Err(format!(
                "{} must be one of the 16 colour names or #rrggbb, got {value}",
                field("color")
            ));
        }
    }
    for key in DECORATIONS {
        if fields.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(format!("{} must be true or false", field(key)));
        }
    }
    if let Some(value) = fields.get("shadow_color") {
        shadow_color(value).map_err(|error| format!("{} {error}", field("shadow_color")))?;
    }
    if let Some(value) = fields.get("font")
        && !value.as_str().is_some_and(crate::dialog::identifier)
    {
        return Err(format!(
            "{} must be an identifier such as minecraft:uniform, got {value}",
            field("font")
        ));
    }
    string(fields, "insertion", path)?;
    if let Some(value) = fields.get("extra") {
        components(value, &field("extra"), depth)?;
    }
    for key in ["click_event", "clickEvent"] {
        if let Some(event) = fields.get(key) {
            click(event, key == "click_event", &field(key))?;
        }
    }
    for key in ["hover_event", "hoverEvent"] {
        if let Some(event) = fields.get(key) {
            hover(event, key == "hover_event", &field(key), depth)?;
        }
    }
    Ok(())
}

/// `key`, when present, must be a string.
fn string(fields: &Map<String, Json>, key: &str, path: &str) -> Result<(), String> {
    match fields.get(key) {
        Some(value) if !value.is_string() => Err(format!("{} must be a string", join(path, key))),
        _ => Ok(()),
    }
}

/// `key` must be a string.
fn required<'a>(fields: &'a Map<String, Json>, key: &str, path: &str) -> Result<&'a str, String> {
    fields
        .get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("{} must be a string", join(path, key)))
}

/// A non-empty list of components, as `extra` takes.
fn components(value: &Json, path: &str, depth: usize) -> Result<(), String> {
    match value {
        Json::Array(items) => list(items, path, depth),
        _ => Err(format!("{path} must be a list of text components")),
    }
}

fn content(fields: &Map<String, Json>, key: &str, path: &str, depth: usize) -> Result<(), String> {
    match key {
        "text" | "keybind" => {
            required(fields, key, path)?;
        }
        "translate" => {
            required(fields, key, path)?;
            string(fields, "fallback", path)?;
            if let Some(value) = fields.get("with") {
                arguments(value, &join(path, "with"), depth)?;
            }
        }
        "selector" => {
            required(fields, key, path)?;
            separator(fields, path, depth)?;
        }
        "score" => {
            let score = fields
                .get(key)
                .and_then(Json::as_object)
                .ok_or_else(|| format!("{} must be an object", join(path, key)))?;
            let score_path = join(path, key);
            required(score, "name", &score_path)?;
            required(score, "objective", &score_path)?;
        }
        "nbt" => {
            required(fields, key, path)?;
            let sources: Vec<&str> = ["block", "entity", "storage"]
                .into_iter()
                .filter(|source| fields.contains_key(*source))
                .collect();
            let [source] = sources.as_slice() else {
                return Err(format!(
                    "{} needs exactly one of block, entity, storage",
                    join(path, key)
                ));
            };
            required(fields, source, path)?;
            if fields
                .get("interpret")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err(format!("{} must be true or false", join(path, "interpret")));
            }
            separator(fields, path, depth)?;
        }
        // An object's fields depend on its kind (atlas sprite, player head).
        _ => {}
    }
    Ok(())
}

fn separator(fields: &Map<String, Json>, path: &str, depth: usize) -> Result<(), String> {
    match fields.get("separator") {
        Some(value) => component(value, &join(path, "separator"), depth + 1),
        None => Ok(()),
    }
}

/// `with`: like `extra`, but vanilla also takes plain numbers and booleans.
fn arguments(value: &Json, path: &str, depth: usize) -> Result<(), String> {
    let Json::Array(items) = value else {
        return Err(format!("{path} must be a list of text components"));
    };
    if items.is_empty() {
        return Err(format!("{path} must not be an empty list"));
    }
    items
        .iter()
        .enumerate()
        .try_for_each(|(index, item)| match item {
            Json::Number(_) | Json::Bool(_) => Ok(()),
            item => component(item, &format!("{path}[{index}]"), depth + 1),
        })
}

/// An ARGB integer, or a list of four colour parts each in 0..=1.
fn shadow_color(value: &Json) -> Result<(), &'static str> {
    const MESSAGE: &str = "must be an ARGB integer or a list of four numbers in 0..=1";
    match value {
        Json::Number(number) => {
            let argb = number.as_i64().ok_or(MESSAGE)?;
            (i64::from(i32::MIN)..=i64::from(u32::MAX))
                .contains(&argb)
                .then_some(())
                .ok_or(MESSAGE)
        }
        Json::Array(parts) => {
            let valid = parts.len() == 4
                && parts.iter().all(|part| {
                    part.as_f64()
                        .is_some_and(|part| (0.0..=1.0).contains(&part))
                });
            valid.then_some(()).ok_or(MESSAGE)
        }
        _ => Err(MESSAGE),
    }
}

/// The URL of an `open_url` event: `http` or `https`, as the client allows.
fn web_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !rest.is_empty()
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

const TYPED_CLICKS: [&str; 7] = [
    "open_url",
    "run_command",
    "suggest_command",
    "change_page",
    "copy_to_clipboard",
    "show_dialog",
    "custom",
];

/// Typed events (1.21.5+) hold their value in a field named after the
/// action; legacy events hold it in `value` as a string. `show_dialog` and
/// `custom` exist only in the typed form.
fn click(event: &Json, typed: bool, path: &str) -> Result<(), String> {
    let fields = event
        .as_object()
        .ok_or_else(|| format!("{path} must be an object"))?;
    let action = required(fields, "action", path)?;
    let known: &[&str] = if typed {
        &TYPED_CLICKS
    } else {
        &TYPED_CLICKS[..5]
    };
    if !known.contains(&action) {
        return Err(format!(
            "{}: unknown action {action:?}; expected one of {}",
            join(path, "action"),
            known.join(", ")
        ));
    }
    let key = if typed {
        match action {
            "open_url" => "url",
            "run_command" | "suggest_command" => "command",
            "change_page" => "page",
            "show_dialog" => "dialog",
            "custom" => "id",
            _ => "value",
        }
    } else {
        "value"
    };
    let at = join(path, key);
    let value = fields
        .get(key)
        .ok_or_else(|| format!("{at} is required for {action}"))?;
    match action {
        "change_page" => {
            let page = match value {
                Json::Number(page) if typed => page.as_i64(),
                Json::String(page) if !typed => page.trim().parse().ok(),
                _ => None,
            };
            if !page.is_some_and(|page| page >= 1) {
                let form = if typed {
                    "integer"
                } else {
                    "string holding an integer"
                };
                return Err(format!("{at} must be a positive {form}"));
            }
        }
        "show_dialog" => {
            let valid = value.is_object() || value.as_str().is_some_and(crate::dialog::identifier);
            if !valid {
                return Err(format!("{at} must be a dialog object or an identifier"));
            }
        }
        "custom" => {
            if !value.as_str().is_some_and(crate::dialog::identifier) {
                return Err(format!("{at} must be an identifier"));
            }
        }
        _ => {
            let text = value
                .as_str()
                .ok_or_else(|| format!("{at} must be a string"))?;
            match action {
                "open_url" if !web_url(text) => {
                    return Err(format!("{at} must be an http or https URL"));
                }
                "run_command" | "suggest_command" if !crate::dialog::chat_string(text) => {
                    return Err(format!("{at} must not contain control characters or '§'"));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The action is known, hover text is itself a valid component, and an item
/// or entity names what it shows.
fn hover(event: &Json, typed: bool, path: &str, depth: usize) -> Result<(), String> {
    let fields = event
        .as_object()
        .ok_or_else(|| format!("{path} must be an object"))?;
    let action = required(fields, "action", path)?;
    // Typed events keep text in `value`; legacy ones in `contents` (1.16+)
    // or `value` (before 1.16, where items and entities are SNBT strings).
    let payload = if typed {
        fields.get("value")
    } else {
        fields.get("contents").or_else(|| fields.get("value"))
    };
    match action {
        "show_text" => {
            let key = if fields.contains_key("contents") && !typed {
                "contents"
            } else {
                "value"
            };
            let text = payload.ok_or_else(|| format!("{} is required", join(path, key)))?;
            component(text, &join(path, key), depth + 1)
        }
        "show_item" | "show_entity" => {
            let id_key = if action == "show_item" || typed {
                "id"
            } else {
                "type"
            };
            let named = if typed {
                fields.get(id_key).and_then(Json::as_str)
            } else {
                match payload {
                    Some(Json::String(text)) => Some(text.as_str()),
                    Some(Json::Object(contents)) => contents.get(id_key).and_then(Json::as_str),
                    _ => None,
                }
            };
            if named.is_none_or(str::is_empty) {
                let at = if typed {
                    join(path, id_key)
                } else {
                    join(path, &format!("contents.{id_key}"))
                };
                return Err(format!("{at} must name the {} shown", &action[5..]));
            }
            Ok(())
        }
        _ => Err(format!(
            "{}: unknown action {action:?}; expected one of show_text, show_item, show_entity",
            join(path, "action")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn error(component: Json) -> String {
        check(&component).unwrap_err()
    }

    #[test]
    fn well_formed_components_pass() {
        for valid in [
            json!("plain"),
            json!(["a", {"text": "b"}]),
            json!({
                "text": "Shop", "color": "#FFAA00", "bold": true, "italic": false,
                "font": "minecraft:uniform", "insertion": "x",
                "shadow_color": -16777216,
                "extra": [{"translate": "chat.type.text", "with": ["a", 3, true, {"text": "b"}]}],
                "click_event": {"action": "open_url", "url": "https://example.com"},
                "hover_event": {"action": "show_text", "value": {"text": "Open", "color": "gray"}},
            }),
            json!({"text": "x", "shadow_color": 4278190080u32}),
            json!({"text": "x", "shadow_color": [1, 0.5, 0, 1]}),
            json!({"text": "x", "font": "uniform"}),
            json!({"keybind": "key.jump"}),
            json!({"selector": "@p", "separator": ", "}),
            json!({"score": {"name": "@s", "objective": "kills"}}),
            json!({"nbt": "Inventory", "entity": "@s", "interpret": true}),
            json!({"object": "atlas", "sprite": "item/diamond"}),
            json!({"text": "x", "future_vanilla_key": {"anything": 1}}),
            json!({"text": "x", "clickEvent": {"action": "change_page", "value": "2"}}),
            json!({"text": "x", "click_event": {"action": "change_page", "page": 2}}),
            json!({"text": "x", "click_event": {"action": "show_dialog", "dialog": "ns:menu"}}),
            json!({"text": "x", "click_event": {"action": "custom", "id": "shop:buy", "payload": {"a": 1}}}),
            json!({"text": "x", "clickEvent": {"action": "copy_to_clipboard", "value": "code"}}),
            json!({"text": "x", "hoverEvent": {"action": "show_text", "contents": "tip"}}),
            json!({"text": "x", "hoverEvent": {"action": "show_text", "value": "tip"}}),
            json!({"text": "x", "hoverEvent": {"action": "show_item", "contents": "minecraft:stone"}}),
            json!({"text": "x", "hoverEvent": {"action": "show_item", "contents": {"id": "minecraft:stone", "count": 2}}}),
            json!({"text": "x", "hoverEvent": {"action": "show_entity", "contents": {"type": "minecraft:pig", "id": "0-0-0-0-1"}}}),
            json!({"text": "x", "hover_event": {"action": "show_item", "id": "minecraft:stone"}}),
            json!({"text": "x", "hover_event": {"action": "show_entity", "id": "minecraft:pig", "uuid": "0-0-0-0-1"}}),
            json!({"text": "x", "click_event": {"action": "open_url", "url": "HTTP://example.com"}}),
        ] {
            assert_eq!(check(&valid), Ok(()), "{valid}");
        }
    }

    #[test]
    fn errors_name_the_key_that_is_wrong() {
        assert_eq!(
            error(json!({"text": "x", "color": "orange"})),
            "color must be one of the 16 colour names or #rrggbb, got \"orange\""
        );
        assert_eq!(
            error(json!({"text": "x", "extra": ["a", {"text": "b", "bold": "yes"}]})),
            "extra[1].bold must be true or false"
        );
        assert_eq!(
            error(json!(["a", {"text": "b", "color": "#fff"}])),
            "component[1].color must be one of the 16 colour names or #rrggbb, got \"#fff\""
        );
        assert_eq!(
            error(json!({"color": "red"})),
            "component needs one of text, translate, keybind, score, selector, nbt, object"
        );
        assert_eq!(
            error(json!({"text": "a", "translate": "b"})),
            "component has both text and translate; use one"
        );
        assert_eq!(
            error(
                json!({"text": "x", "click_event": {"action": "open_url", "url": "javascript:alert(1)"}})
            ),
            "click_event.url must be an http or https URL"
        );
        assert_eq!(
            error(
                json!({"text": "x", "clickEvent": {"action": "open_url", "value": "file:///etc/passwd"}})
            ),
            "clickEvent.value must be an http or https URL"
        );
        assert_eq!(
            error(json!({"text": "x", "click_event": {"action": "run_command"}})),
            "click_event.command is required for run_command"
        );
        assert_eq!(
            error(json!({"text": "x", "click_event": {"action": "open_file", "path": "/"}})),
            "click_event.action: unknown action \"open_file\"; expected one of open_url, \
             run_command, suggest_command, change_page, copy_to_clipboard, show_dialog, custom"
        );
        assert_eq!(
            error(json!({"text": "x", "clickEvent": {"action": "show_dialog", "value": "a"}})),
            "clickEvent.action: unknown action \"show_dialog\"; expected one of open_url, \
             run_command, suggest_command, change_page, copy_to_clipboard"
        );
        assert_eq!(
            error(json!({"text": "x", "click_event": {"action": "change_page", "page": 0}})),
            "click_event.page must be a positive integer"
        );
        assert_eq!(
            error(json!({"text": "x", "clickEvent": {"action": "change_page", "value": "two"}})),
            "clickEvent.value must be a positive string holding an integer"
        );
        assert_eq!(
            error(
                json!({"text": "x", "click_event": {"action": "run_command", "command": "/a\nb"}})
            ),
            "click_event.command must not contain control characters or '§'"
        );
        assert_eq!(
            error(
                json!({"text": "x", "hover_event": {"action": "show_text", "value": {"text": "t", "italic": 1}}})
            ),
            "hover_event.value.italic must be true or false"
        );
        assert_eq!(
            error(json!({"text": "x", "hover_event": {"action": "show_item"}})),
            "hover_event.id must name the item shown"
        );
        assert_eq!(
            error(json!({"text": "x", "hoverEvent": {"action": "show_achievement", "value": "a"}})),
            "hoverEvent.action: unknown action \"show_achievement\"; expected one of show_text, \
             show_item, show_entity"
        );
        assert_eq!(
            error(json!({"text": "x", "extra": []})),
            "extra must not be an empty list"
        );
        assert_eq!(
            error(json!({"text": "x", "extra": {"text": "y"}})),
            "extra must be a list of text components"
        );
        assert_eq!(
            error(json!({"translate": "x", "with": []})),
            "with must not be an empty list"
        );
        assert_eq!(error(json!({"text": 3})), "text must be a string");
        assert_eq!(
            error(json!({"text": "x", "insertion": 1})),
            "insertion must be a string"
        );
        assert_eq!(
            error(json!({"text": "x", "font": "Minecraft:Uniform"})),
            "font must be an identifier such as minecraft:uniform, got \"Minecraft:Uniform\""
        );
        assert_eq!(
            error(json!({"text": "x", "shadow_color": [1, 2, 0, 1]})),
            "shadow_color must be an ARGB integer or a list of four numbers in 0..=1"
        );
        assert_eq!(
            error(json!({"text": "x", "shadow_color": 1.5})),
            "shadow_color must be an ARGB integer or a list of four numbers in 0..=1"
        );
        assert_eq!(
            error(json!({"score": {"name": "@s"}})),
            "score.objective must be a string"
        );
        assert_eq!(
            error(json!({"nbt": "a", "block": "~ ~ ~", "entity": "@s"})),
            "nbt needs exactly one of block, entity, storage"
        );
    }
}
