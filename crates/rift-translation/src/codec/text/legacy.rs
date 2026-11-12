//! Section-sign text: the `§` colour and format codes that 1.12 uses where
//! 1.13 and newer use components (team prefixes and names, objective
//! names, and the custom names other codecs convert).
use super::component::{Arg, Color, Component, Content, Style, primitive_text};
use crate::wire::MAX_NBT_DEPTH;

const SECTION: char = '§';
const COLOR_CODES: &[u8; 16] = b"0123456789abcdef";

/// The colour of a legacy code character, by `ChatFormatting` index.
pub fn color_of_code(code: char) -> Option<u8> {
    COLOR_CODES
        .iter()
        .position(|known| *known as char == code.to_ascii_lowercase())
        .map(|index| index as u8)
}

/// The code character of a named colour.
pub fn code_of_color(index: u8) -> char {
    COLOR_CODES[usize::from(index & 15)] as char
}

/// Legacy text as a component: each run of text with the formatting in
/// effect at that point. A colour code resets the format codes before it,
/// and `§r` resets everything, as legacy rendering does.
pub fn from_legacy(text: &str) -> Component {
    let mut runs: Vec<Component> = Vec::new();
    let mut style = Style::default();
    let mut current = String::new();
    let mut chars = text.chars();
    let flush = |current: &mut String, style: &Style, runs: &mut Vec<Component>| {
        if !current.is_empty() {
            runs.push(Component {
                content: Content::Text(std::mem::take(current)),
                style: style.clone(),
                extra: Vec::new(),
            });
        }
    };
    while let Some(character) = chars.next() {
        if character != SECTION {
            current.push(character);
            continue;
        }
        let Some(code) = chars.next() else {
            break;
        };
        flush(&mut current, &style, &mut runs);
        let code = code.to_ascii_lowercase();
        if let Some(color) = color_of_code(code) {
            style = Style {
                color: Some(Color::Named(color)),
                ..Style::default()
            };
            continue;
        }
        match code {
            'k' => style.obfuscated = Some(true),
            'l' => style.bold = Some(true),
            'm' => style.strikethrough = Some(true),
            'n' => style.underlined = Some(true),
            'o' => style.italic = Some(true),
            'r' => style = Style::default(),
            _ => {}
        }
    }
    flush(&mut current, &style, &mut runs);
    match runs.len() {
        0 => Component::text(""),
        1 if runs[0].style.is_empty() => runs.pop().unwrap_or_default(),
        _ => Component {
            extra: runs,
            ..Component::text("")
        },
    }
}

/// The last colour a legacy string leaves in effect, if any; 1.12 team
/// prefixes colour the name that follows them this way.
pub fn trailing_color(text: &str) -> Option<u8> {
    let mut color = None;
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == SECTION {
            match chars.next().map(|code| code.to_ascii_lowercase()) {
                Some('r') => color = None,
                Some(code) => {
                    if let Some(index) = color_of_code(code) {
                        color = Some(index);
                    }
                }
                None => break,
            }
        }
    }
    color
}

/// The effective formatting legacy text can express.
#[derive(Clone, Copy, Default, PartialEq)]
struct Format {
    color: Option<u8>,
    bold: bool,
    italic: bool,
    underlined: bool,
    strikethrough: bool,
    obfuscated: bool,
}

impl Format {
    fn inherit(self, style: &Style) -> Format {
        Format {
            color: match style.color {
                Some(Color::Reset) => None,
                Some(color) => match color.nearest_named() {
                    Color::Named(index) => Some(index),
                    _ => self.color,
                },
                None => self.color,
            },
            bold: style.bold.unwrap_or(self.bold),
            italic: style.italic.unwrap_or(self.italic),
            underlined: style.underlined.unwrap_or(self.underlined),
            strikethrough: style.strikethrough.unwrap_or(self.strikethrough),
            obfuscated: style.obfuscated.unwrap_or(self.obfuscated),
        }
    }

    fn flags(self) -> [(bool, char); 5] {
        [
            (self.obfuscated, 'k'),
            (self.bold, 'l'),
            (self.strikethrough, 'm'),
            (self.underlined, 'n'),
            (self.italic, 'o'),
        ]
    }
}

/// A component as legacy text with section-sign codes. Translations are
/// resolved from a small table of common keys or the fallback, else shown
/// as their key; scores show their resolved value, selectors their pattern.
pub fn to_legacy(component: &Component) -> String {
    let mut out = String::new();
    let mut state = Format::default();
    emit(component, Format::default(), &mut state, &mut out, 0);
    out
}

fn push_text(text: &str, format: Format, state: &mut Format, out: &mut String) {
    if text.is_empty() {
        return;
    }
    if format != *state {
        let lost_flag = state
            .flags()
            .iter()
            .zip(format.flags())
            .any(|((had, _), (has, _))| *had && !has);
        if lost_flag || (format.color.is_none() && state.color.is_some()) {
            out.push(SECTION);
            out.push('r');
            *state = Format::default();
        }
        if format.color != state.color {
            if let Some(color) = format.color {
                out.push(SECTION);
                out.push(code_of_color(color));
            }
            // A colour code clears the format codes.
            *state = Format {
                color: format.color,
                ..Format::default()
            };
        }
        for ((has, code), (had, _)) in format.flags().into_iter().zip(state.flags()) {
            if has && !had {
                out.push(SECTION);
                out.push(code);
            }
        }
        *state = format;
    }
    out.push_str(text);
}

fn emit(component: &Component, parent: Format, state: &mut Format, out: &mut String, depth: usize) {
    if depth > MAX_NBT_DEPTH {
        return;
    }
    let format = parent.inherit(&component.style);
    match &component.content {
        Content::Text(text) => push_text(text, format, state, out),
        Content::Translate {
            key,
            fallback,
            args,
        } => {
            let template = translation(key)
                .map(str::to_owned)
                .or_else(|| fallback.clone());
            match template {
                Some(template) => {
                    let mut literal = String::new();
                    substitute_parts(&template, args.len(), |part| match part {
                        Part::Literal(text) => literal.push_str(text),
                        Part::Arg(index) => {
                            push_text(&std::mem::take(&mut literal), format, state, out);
                            match &args[index] {
                                Arg::Component(arg) => emit(arg, format, state, out, depth + 1),
                                Arg::Primitive(value) => {
                                    push_text(&primitive_text(value), format, state, out)
                                }
                            }
                        }
                    });
                    push_text(&literal, format, state, out);
                }
                None => push_text(key, format, state, out),
            }
        }
        Content::Score { value, .. } => {
            push_text(value.as_deref().unwrap_or(""), format, state, out)
        }
        Content::Selector { pattern, .. } => push_text(pattern, format, state, out),
        Content::Keybind(key) => push_text(key, format, state, out),
        Content::Nbt { .. } => {}
        Content::Object { fallback, .. } => {
            if let Some(fallback) = fallback {
                emit(fallback, format, state, out, depth + 1);
            }
        }
    }
    for child in &component.extra {
        emit(child, format, state, out, depth + 1);
    }
}

pub(super) enum Part<'a> {
    Literal(&'a str),
    Arg(usize),
}

/// Split a translation template into literal text and argument references
/// (`%s`, `%1$s`, `%%`), as vanilla's `TranslatableContents` does; a
/// reference past the arguments renders as nothing.
pub(super) fn substitute_parts<'a>(
    template: &'a str,
    count: usize,
    mut part: impl FnMut(Part<'a>),
) {
    let mut next = 0;
    let mut rest = template;
    while let Some(index) = rest.find('%') {
        part(Part::Literal(&rest[..index]));
        let after = &rest[index + 1..];
        if let Some(stripped) = after.strip_prefix('%') {
            part(Part::Literal("%"));
            rest = stripped;
            continue;
        }
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        let (position, tail) = if digits > 0 && after[digits..].starts_with('$') {
            (
                after[..digits]
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1)),
                &after[digits + 1..],
            )
        } else {
            let position = next;
            next += 1;
            (Some(position), after)
        };
        if let Some(stripped) = tail.strip_prefix('s').or_else(|| tail.strip_prefix('d')) {
            if let Some(position) = position.filter(|position| *position < count) {
                part(Part::Arg(position));
            }
            rest = stripped;
        } else {
            part(Part::Literal("%"));
            rest = after;
        }
    }
    part(Part::Literal(rest));
}

/// Render a template with its arguments into `out`.
pub(super) fn substitute(
    template: &str,
    args: &[Arg],
    out: &mut String,
    mut arg: impl FnMut(&Arg, &mut String),
) {
    substitute_parts(template, args.len(), |part| match part {
        Part::Literal(text) => out.push_str(text),
        Part::Arg(index) => arg(&args[index], out),
    });
}

/// English templates of the translation keys servers commonly send in
/// places that legacy text has to show.
pub fn translation(key: &str) -> Option<&'static str> {
    Some(match key {
        "chat.type.text" => "<%s> %s",
        "chat.type.announcement" => "[%s] %s",
        "chat.type.emote" => "* %s %s",
        "chat.type.admin" => "[%s: %s]",
        "chat.type.team.text" => "%s <%s> %s",
        "chat.type.team.sent" => "-> %s <%s> %s",
        "commands.message.display.incoming" => "%s whispers to you: %s",
        "commands.message.display.outgoing" => "You whisper to %s: %s",
        "multiplayer.player.joined" => "%s joined the game",
        "multiplayer.player.left" => "%s left the game",
        "multiplayer.disconnect.kicked" => "Kicked by an operator",
        "multiplayer.disconnect.server_shutdown" => "Server closed",
        "death.attack.generic" => "%1$s died",
        _ => return None,
    })
}

/// Legacy text as a JSON component string.
pub fn legacy_to_json(text: &str) -> String {
    super::to_json_string(super::version::LAST_JSON, &from_legacy(text))
}

/// A JSON component string as legacy text. Text that is not a component
/// (plugins send anything) is returned as it is, which is what an old
/// client would have shown.
pub fn json_to_legacy(json: &str) -> String {
    match super::from_json_str(super::version::LAST_JSON, json) {
        Ok(component) => to_legacy(&component),
        Err(_) => json.to_owned(),
    }
}

/// The text of a JSON component string without any formatting.
pub fn plain_text(json: &str) -> String {
    match super::from_json_str(super::version::LAST_JSON, json) {
        Ok(component) => component.plain_text(),
        Err(_) => json.to_owned(),
    }
}

/// Cut legacy text to at most `max` UTF-16 units without leaving a dangling
/// section sign, for the length limits 1.12 enforces.
pub fn truncate(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut units = 0;
    for character in text.chars() {
        let width = character.len_utf16();
        if units + width > max {
            break;
        }
        units += width;
        out.push(character);
    }
    if out.ends_with(SECTION) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_round_trips_through_components() {
        for text in ["plain", "§cRed §lbold§r plain", "§a§oitalic green", "", "§"] {
            let component = from_legacy(text);
            let back = to_legacy(&component);
            assert_eq!(from_legacy(&back), component, "{text} -> {back}");
        }
        assert_eq!(
            to_legacy(&from_legacy("§cRed §lbold§r plain")),
            "§cRed §lbold§r plain"
        );
    }

    #[test]
    fn json_conversions() {
        assert_eq!(
            legacy_to_json("§cRed"),
            r#"{"extra":[{"color":"red","text":"Red"}],"text":""}"#
        );
        assert_eq!(legacy_to_json("plain"), r#"{"text":"plain"}"#);
        assert_eq!(
            json_to_legacy(r#"{"text":"a","color":"gold","extra":[{"text":"b","bold":true},"c"]}"#),
            "§6a§lb§r§6c"
        );
        assert_eq!(json_to_legacy(r##"{"text":"x","color":"#FF6060"}"##), "§cx");
        assert_eq!(
            plain_text(r#"{"translate":"chat.type.text","with":["Bob",{"text":"hi"}]}"#),
            "<Bob> hi"
        );
        assert_eq!(json_to_legacy("{broken"), "{broken");
        assert_eq!(trailing_color("§a§lX§c"), Some(12));
        assert_eq!(trailing_color("§cX§r"), None);
        assert_eq!(truncate("abc§c", 4), "abc");
        assert_eq!(truncate("ab", 4), "ab");
    }

    #[test]
    fn templates_substitute_like_vanilla() {
        let mut out = String::new();
        let args = [
            Arg::Primitive(crate::nbt::Nbt::String("A".into())),
            Arg::Primitive(crate::nbt::Nbt::Int(2)),
        ];
        substitute(
            "%2$s-%1$s %s %% %s %s",
            &args,
            &mut out,
            |arg, out| match arg {
                Arg::Primitive(value) => out.push_str(&primitive_text(value)),
                Arg::Component(_) => {}
            },
        );
        assert_eq!(out, "2-A A % 2 ");
    }
}
