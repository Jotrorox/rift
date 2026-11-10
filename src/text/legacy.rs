//! Legacy formatting codes: `&c` or `§c` colours, `&l` decorations, `&r`
//! resets and the hex forms `&#rrggbb` and `&x&r&r&g&g&b&b`. Parsed into
//! components for operators migrating from other proxies, and written as `§`
//! codes for surfaces that only take strings, such as server-list hover lines.
use super::{color, markup::Out, markup::Style};
use serde_json::Value as Json;

/// `k` to `o`: obfuscated, bold, strikethrough, underlined, italic, as
/// indexes into [`super::markup::DECORATIONS`].
fn decoration(code: char) -> Option<usize> {
    Some(match code.to_ascii_lowercase() {
        'l' => 0,
        'o' => 1,
        'n' => 2,
        'm' => 3,
        'k' => 4,
        _ => return None,
    })
}

const DECORATION_CODES: [char; 5] = ['l', 'o', 'n', 'm', 'k'];

/// Parse legacy text with `marker` (usually `&` or `§`) into a component.
/// A marker that does not start a known code is kept as written. As in the
/// vanilla client, a colour code ends the decorations before it.
pub fn parse(text: &str, marker: char) -> Json {
    let mut out = Out::default();
    let mut style = Style::default();
    let mut run = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if c != marker || index + 1 >= chars.len() {
            run.push(c);
            index += 1;
            continue;
        }
        let code = chars[index + 1];
        let change = |style: &mut Style, next: Style, run: &mut String, out: &mut Out| {
            if !run.is_empty() {
                out.text(run, style);
                run.clear();
            }
            *style = next;
        };
        if let Some(name) = color::from_code(code) {
            let next = Style {
                color: Some(name.to_owned()),
                ..Style::default()
            };
            change(&mut style, next, &mut run, &mut out);
            index += 2;
        } else if let Some(position) = decoration(code) {
            let mut next = style.clone();
            next.decorations[position] = Some(true);
            change(&mut style, next, &mut run, &mut out);
            index += 2;
        } else if code.eq_ignore_ascii_case(&'r') {
            change(&mut style, Style::default(), &mut run, &mut out);
            index += 2;
        } else if let Some((rgb, length)) = hex(&chars[index + 1..], marker) {
            let next = Style {
                color: Some(color::to_hex(rgb)),
                ..Style::default()
            };
            change(&mut style, next, &mut run, &mut out);
            index += 1 + length;
        } else {
            run.push(c);
            index += 1;
        }
    }
    if !run.is_empty() {
        out.text(&run, &style);
    }
    out.finish()
}

/// `#rrggbb` or `x` followed by six marker-prefixed digits, after a marker.
fn hex(chars: &[char], marker: char) -> Option<(u32, usize)> {
    match chars.first()? {
        '#' => {
            let digits: String = chars.get(1..7)?.iter().collect();
            let rgb = color::hex(&format!("#{digits}"))?;
            Some((rgb, 7))
        }
        'x' | 'X' => {
            let pairs = chars.get(1..13)?;
            let mut digits = String::from("#");
            for pair in pairs.chunks(2) {
                if pair[0] != marker {
                    return None;
                }
                digits.push(pair[1]);
            }
            Some((color::hex(&digits)?, 13))
        }
        _ => None,
    }
}

/// `component` as `§` codes. No vanilla client reads hex codes in strings,
/// so hex colours become the nearest named colour; events cannot be
/// expressed and are dropped. Translations show their fallback or key.
pub fn serialize(component: &Json) -> String {
    let mut out = String::new();
    let mut last: Option<(Option<char>, [bool; 5])> = None;
    write(component, &Style::default(), &mut out, &mut last, 0);
    out
}

fn write(
    component: &Json,
    parent: &Style,
    out: &mut String,
    last: &mut Option<(Option<char>, [bool; 5])>,
    depth: usize,
) {
    if depth > 64 {
        return;
    }
    match component {
        Json::String(text) => run(text, parent, out, last),
        Json::Number(number) => run(&number.to_string(), parent, out, last),
        Json::Bool(value) => run(if *value { "true" } else { "false" }, parent, out, last),
        Json::Array(items) => {
            // The first element is the parent of the others.
            let Some((first, rest)) = items.split_first() else {
                return;
            };
            let style = match first {
                Json::Object(fields) => parent.with(&Style::of(fields)),
                _ => parent.clone(),
            };
            write(first, parent, out, last, depth + 1);
            rest.iter()
                .for_each(|item| write(item, &style, out, last, depth + 1));
        }
        Json::Object(fields) => {
            let style = parent.with(&Style::of(fields));
            let own = if let Some(text) = fields.get("text").and_then(Json::as_str) {
                text.to_owned()
            } else if let Some(key) = fields.get("translate").and_then(Json::as_str) {
                fields
                    .get("fallback")
                    .and_then(Json::as_str)
                    .unwrap_or(key)
                    .to_owned()
            } else {
                let mut plain = String::new();
                super::flatten_own(component, &mut plain);
                plain
            };
            run(&own, &style, out, last);
            if let Some(Json::Array(extra)) = fields.get("extra") {
                extra
                    .iter()
                    .for_each(|item| write(item, &style, out, last, depth + 1));
            }
        }
        Json::Null => {}
    }
}

/// Append `text` with the codes that switch from the last run's style.
fn run(text: &str, style: &Style, out: &mut String, last: &mut Option<(Option<char>, [bool; 5])>) {
    if text.is_empty() {
        return;
    }
    let code = style.color.as_deref().and_then(|name| {
        color::code(name)
            .or_else(|| color::hex(name).and_then(|rgb| color::code(color::nearest(rgb))))
    });
    let decorations = style.decorations.map(|value| value == Some(true));
    let current = (code, decorations);
    if last.as_ref() != Some(&current) {
        let (was_code, was_decorations) = last.unwrap_or((None, [false; 5]));
        // Codes only add decorations, so dropping one or the colour needs a
        // fresh start: a colour code or `§r`.
        let removes = was_decorations
            .iter()
            .zip(decorations)
            .any(|(was, is)| *was && !is);
        if code != was_code
            || removes
            || last.is_none() && (code.is_some() || decorations.contains(&true))
        {
            match code {
                Some(code) => {
                    out.push('§');
                    out.push(code);
                }
                None if last.is_some() => out.push_str("§r"),
                None => {}
            }
            for (index, on) in decorations.iter().enumerate() {
                if *on {
                    out.push('§');
                    out.push(DECORATION_CODES[index]);
                }
            }
        } else {
            for (index, (was, is)) in was_decorations.iter().zip(decorations).enumerate() {
                if is && !was {
                    out.push('§');
                    out.push(DECORATION_CODES[index]);
                }
            }
        }
        *last = Some(current);
    }
    out.push_str(text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codes_become_components() {
        assert_eq!(parse("plain", '&'), json!({"text": "plain"}));
        assert_eq!(
            parse("&cRed &lbold&r plain", '&'),
            json!({"text": "", "extra": [
                {"text": "Red ", "color": "red"},
                {"text": "bold", "color": "red", "bold": true},
                {"text": " plain"},
            ]})
        );
        // A colour code ends earlier decorations.
        assert_eq!(
            parse("§l§6x§ay", '§'),
            json!({"text": "", "extra": [
                {"text": "x", "color": "gold"},
                {"text": "y", "color": "green"},
            ]})
        );
        assert_eq!(
            parse("&#FF8800x", '&'),
            json!({"text": "x", "color": "#ff8800"})
        );
        assert_eq!(
            parse("&x&f&f&8&8&0&0x", '&'),
            json!({"text": "x", "color": "#ff8800"})
        );
        assert_eq!(parse("a & b &z &", '&'), json!({"text": "a & b &z &"}));
        assert_eq!(parse("&#12345 x", '&'), json!({"text": "&#12345 x"}));
    }

    #[test]
    fn components_become_section_codes() {
        assert_eq!(serialize(&json!("plain")), "plain");
        assert_eq!(
            serialize(&parse("&cRed &lbold&r plain", '&')),
            "§cRed §lbold§r plain"
        );
        assert_eq!(serialize(&json!({"text": "x", "color": "#ff8800"})), "§6x");
        assert_eq!(
            serialize(&json!({"text": "a", "bold": true, "extra": [{"text": "b", "bold": false}]})),
            "§la§rb"
        );
        assert_eq!(
            serialize(&json!([{"text": "a", "color": "red"}, "b", {"text": "c", "color": "blue"}])),
            "§cab§9c"
        );
        assert_eq!(
            serialize(&json!({"translate": "x.y", "fallback": "Fallback"})),
            "Fallback"
        );
        for legacy in ["§aGreen §l§nboth§r §7gray", "§6x§ay", "plain §cred"] {
            assert_eq!(serialize(&parse(legacy, '§')), legacy);
        }
    }
}
