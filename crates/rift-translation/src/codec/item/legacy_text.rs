//! Legacy formatted text (section-sign codes) and JSON text components, for
//! item names before 1.13 and lore lines before 1.14, which were plain
//! strings.
use serde_json::{Map, Value, json};

const COLORS: [(char, &str); 16] = [
    ('0', "black"),
    ('1', "dark_blue"),
    ('2', "dark_green"),
    ('3', "dark_aqua"),
    ('4', "dark_red"),
    ('5', "dark_purple"),
    ('6', "gold"),
    ('7', "gray"),
    ('8', "dark_gray"),
    ('9', "blue"),
    ('a', "green"),
    ('b', "aqua"),
    ('c', "red"),
    ('d', "light_purple"),
    ('e', "yellow"),
    ('f', "white"),
];

const FORMATS: [(char, &str); 5] = [
    ('k', "obfuscated"),
    ('l', "bold"),
    ('m', "strikethrough"),
    ('n', "underlined"),
    ('o', "italic"),
];

#[derive(Clone, Default, PartialEq)]
struct Style {
    color: Option<&'static str>,
    /// Indexed like [`FORMATS`].
    formats: [bool; 5],
}

impl Style {
    fn is_plain(&self) -> bool {
        self.color.is_none() && !self.formats.contains(&true)
    }

    fn apply(&self, object: &mut Map<String, Value>) {
        if let Some(color) = self.color {
            object.insert("color".into(), color.into());
        }
        for (index, (_, name)) in FORMATS.iter().enumerate() {
            if self.formats[index] {
                object.insert((*name).into(), true.into());
            }
        }
    }

    fn codes(&self) -> String {
        let mut codes = String::new();
        if let Some(color) = self.color {
            let code = COLORS
                .iter()
                .find(|(_, name)| *name == color)
                .map(|(code, _)| *code);
            if let Some(code) = code {
                codes.push('§');
                codes.push(code);
            }
        }
        for (index, (code, _)) in FORMATS.iter().enumerate() {
            if self.formats[index] {
                codes.push('§');
                codes.push(*code);
            }
        }
        codes
    }
}

/// Legacy text as a JSON component, `{"text": ...}` when it is unformatted.
pub fn to_json(text: &str) -> String {
    let mut runs: Vec<(Style, String)> = Vec::new();
    let mut style = Style::default();
    let mut current = String::new();
    let mut chars = text.chars();
    while let Some(char) = chars.next() {
        if char != '§' {
            current.push(char);
            continue;
        }
        let Some(code) = chars.next().map(|code| code.to_ascii_lowercase()) else {
            break;
        };
        let mut next = style.clone();
        if let Some((_, color)) = COLORS.iter().find(|(known, _)| *known == code) {
            // A colour resets the formats, as the legacy renderer does.
            next = Style {
                color: Some(color),
                formats: [false; 5],
            };
        } else if let Some(index) = FORMATS.iter().position(|(known, _)| *known == code) {
            next.formats[index] = true;
        } else if code == 'r' {
            next = Style::default();
        } else {
            continue;
        }
        if next != style {
            if !current.is_empty() {
                runs.push((style, std::mem::take(&mut current)));
            }
            style = next;
        }
    }
    if !current.is_empty() {
        runs.push((style, current));
    }
    match runs.as_slice() {
        [] => json!({"text": ""}).to_string(),
        [(style, text)] if style.is_plain() => json!({ "text": text }).to_string(),
        _ => {
            let extra: Vec<Value> = runs
                .into_iter()
                .map(|(style, text)| {
                    let mut object = Map::new();
                    object.insert("text".into(), text.into());
                    style.apply(&mut object);
                    Value::Object(object)
                })
                .collect();
            json!({"text": "", "extra": extra}).to_string()
        }
    }
}

/// A JSON component as legacy text. Text that is not JSON is taken as
/// plain, as the legacy clients would show it.
pub fn from_json(json: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return json.to_owned();
    };
    let mut out = String::new();
    let mut last = Style::default();
    flatten(&value, &Style::default(), &mut out, &mut last, 0);
    out
}

fn flatten(value: &Value, parent: &Style, out: &mut String, last: &mut Style, depth: usize) {
    if depth > 32 {
        return;
    }
    match value {
        Value::String(text) => emit(text, parent, out, last),
        Value::Array(values) => {
            // The first element is the parent of the rest.
            if let Some((first, rest)) = values.split_first() {
                let style = style_of(first, parent);
                flatten(first, parent, out, last, depth + 1);
                for value in rest {
                    flatten(value, &style, out, last, depth + 1);
                }
            }
        }
        Value::Object(object) => {
            let style = style_of(value, parent);
            let text = if let Some(text) = object.get("text").and_then(Value::as_str) {
                text.to_owned()
            } else if let Some(key) = object.get("translate").and_then(Value::as_str) {
                object
                    .get("fallback")
                    .and_then(Value::as_str)
                    .unwrap_or(key)
                    .to_owned()
            } else if let Some(key) = object.get("keybind").and_then(Value::as_str) {
                key.to_owned()
            } else if let Some(selector) = object.get("selector").and_then(Value::as_str) {
                selector.to_owned()
            } else {
                String::new()
            };
            emit(&text, &style, out, last);
            if let Some(Value::Array(extra)) = object.get("extra") {
                for child in extra {
                    flatten(child, &style, out, last, depth + 1);
                }
            }
        }
        Value::Number(number) => emit(&number.to_string(), parent, out, last),
        Value::Bool(value) => emit(&value.to_string(), parent, out, last),
        Value::Null => {}
    }
}

fn style_of(value: &Value, parent: &Style) -> Style {
    let mut style = parent.clone();
    let Value::Object(object) = value else {
        return style;
    };
    if let Some(color) = object.get("color").and_then(Value::as_str) {
        style.color = COLORS
            .iter()
            .find(|(_, name)| *name == color)
            .map(|(_, name)| *name);
    }
    for (index, (_, name)) in FORMATS.iter().enumerate() {
        if let Some(value) = object.get(*name).and_then(Value::as_bool) {
            style.formats[index] = value;
        }
    }
    style
}

fn emit(text: &str, style: &Style, out: &mut String, last: &mut Style) {
    if text.is_empty() {
        return;
    }
    if style != last {
        // Formats only switch off with a reset.
        let dropped = last
            .formats
            .iter()
            .zip(style.formats)
            .any(|(was, is)| *was && !is);
        if dropped || (style.color.is_none() && last.color.is_some()) {
            out.push_str("§r");
        }
        out.push_str(&style.codes());
        *last = style.clone();
    }
    out.push_str(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_round_trips() {
        assert_eq!(to_json("Blade"), r#"{"text":"Blade"}"#);
        assert_eq!(from_json(r#"{"text":"Blade"}"#), "Blade");
        assert_eq!(from_json(r#""Blade""#), "Blade");
        assert_eq!(from_json("not json {"), "not json {");
        assert_eq!(to_json(""), r#"{"text":""}"#);
    }

    #[test]
    fn formatting_codes_become_styles_and_back() {
        let json = to_json("§cRed §lBold§r plain");
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["extra"][0], json!({"text": "Red ", "color": "red"}));
        assert_eq!(
            value["extra"][1],
            json!({"text": "Bold", "color": "red", "bold": true})
        );
        assert_eq!(value["extra"][2], json!({"text": " plain"}));
        assert_eq!(from_json(&json), "§cRed §c§lBold§r plain");
        assert_eq!(
            from_json(r#"{"text":"a","color":"gold","extra":[{"text":"b","italic":true}]}"#),
            "§6a§6§ob"
        );
        assert_eq!(from_json(r#"{"translate":"item.x","fallback":"X"}"#), "X");
    }
}
