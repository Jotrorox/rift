//! Rift markup: an easy way to write styled text, in the tag syntax many
//! Minecraft developers know from MiniMessage.
//!
//! ```text
//! <gold>Welcome, <player>!</gold> <gray>Click <click:run_command:/spawn><u>here</u></click>
//! <gradient:#ff5555:#5555ff>Rift</gradient> <hover:show_text:'<green>Online'>status</hover>
//! ```
//!
//! Markup is parsed into a JSON text component at the call and never stored
//! or sent as markup. Arguments (`<player>`) are inserted as written: their
//! text is never parsed, so a relayed player name cannot add tags. Styles
//! (`<accent>`) come from the operator's theme.
//!
//! | Tag | Effect |
//! | --- | --- |
//! | `<red>`, `<#ff8800>`, `<color:red>`, `<c:#ff8800>` | colour (`grey`, `dark_grey` also work) |
//! | `<bold>` `<b>`, `<italic>` `<i>` `<em>`, `<underlined>` `<u>`, `<strikethrough>` `<st>`, `<obfuscated>` `<obf>` | decoration; `<!bold>` or `<bold:false>` turns it off |
//! | `<gradient:red:#5555ff[:...]>` | a colour per character, interpolated between the stops |
//! | `<rainbow>`, `<rainbow:0.5>` | a rainbow, optionally starting at another hue (0..1) |
//! | `<hover:show_text:'markup'>` | hover text; also `show_item:id[:count]`, `show_entity:type:uuid[:'name']` |
//! | `<click:run_command:/cmd>` | click; also `suggest_command`, `open_url`, `copy_to_clipboard`, `change_page` |
//! | `<insert:text>` | text inserted into chat on shift-click |
//! | `<font:minecraft:uniform>` | font |
//! | `<shadow:black[:0.5]>`, `<!shadow>` | text shadow colour and opacity (default 0.25) |
//! | `<newline>` `<br>` | line break |
//! | `<key:key.jump>` | the player's key binding |
//! | `<lang:key[:'arg']...>`, `<lang_or:key:'fallback'[:'arg']...>` | translation; arguments are markup |
//! | `<reset>` | close every open tag |
//! | `<name>` | an argument, or a style of the theme |
//!
//! `</tag>` closes the latest open tag of that kind and those opened after
//! it; tags left open end with the text. `\<` writes a literal `<` and `\\`
//! a backslash. A `<` that does not start a well-formed tag, as in `a < b`
//! or `<3 you`, is literal text; a well-formed tag Rift does not know is an
//! error, so typos show up at the call.
use super::color;
use serde_json::{Map, Value as Json, json};
use std::collections::BTreeMap;

/// Tags deeper than this are refused, matching the component depth limit.
const MAX_DEPTH: usize = 32;
/// Characters a single gradient or rainbow may paint; each becomes a
/// component of its own.
const MAX_PAINTED: usize = 512;

/// A value for `<name>` in markup.
#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    /// Shown exactly as written, never parsed.
    Text(String),
    /// A JSON text component, inserted with its own formatting.
    Component(Json),
}

/// Named values for markup arguments.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Args(Vec<(String, Arg)>);

impl Args {
    pub fn new() -> Self {
        Self::default()
    }

    /// `<name>` shows `value` as written.
    pub fn text(mut self, name: &str, value: impl Into<String>) -> Self {
        self.insert(name, Arg::Text(value.into()));
        self
    }

    /// `<name>` shows a component.
    pub fn component(mut self, name: &str, value: Json) -> Self {
        self.insert(name, Arg::Component(value));
        self
    }

    pub fn insert(&mut self, name: &str, value: Arg) {
        let name = name.to_ascii_lowercase();
        match self.0.iter_mut().find(|(key, _)| *key == name) {
            Some((_, slot)) => *slot = value,
            None => self.0.push((name, value)),
        }
    }

    pub fn get(&self, name: &str) -> Option<&Arg> {
        self.0
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(name, _)| name.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Check names: `a-z`, `0-9`, `_` and `-`, not a built-in tag.
    pub fn validate(&self, label: &str) -> Result<(), String> {
        self.names().try_for_each(|name| custom_name(name, label))
    }
}

/// A name for an argument or a style: 1..=64 of `a-z0-9_-`, not a built-in tag.
pub fn custom_name(name: &str, label: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !valid {
        return Err(format!(
            "{label}: {name:?} must be 1..=64 characters of a-z, 0-9, '_' and '-'"
        ));
    }
    if builtin(name) {
        return Err(format!("{label}: <{name}> is a built-in tag"));
    }
    Ok(())
}

/// Formatting that tags apply: unset fields inherit from the enclosing text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Style {
    pub color: Option<String>,
    /// bold, italic, underlined, strikethrough, obfuscated.
    pub decorations: [Option<bool>; 5],
    pub font: Option<String>,
    /// ARGB.
    pub shadow: Option<i64>,
    pub insertion: Option<String>,
    pub click: Option<Json>,
    pub hover: Option<Json>,
}

pub const DECORATIONS: [&str; 5] = [
    "bold",
    "italic",
    "underlined",
    "strikethrough",
    "obfuscated",
];

impl Style {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `self` with every field `inner` sets replaced.
    pub fn with(&self, inner: &Style) -> Style {
        let mut merged = self.clone();
        if inner.color.is_some() {
            merged.color.clone_from(&inner.color);
        }
        for (slot, value) in merged.decorations.iter_mut().zip(inner.decorations) {
            if value.is_some() {
                *slot = value;
            }
        }
        if inner.font.is_some() {
            merged.font.clone_from(&inner.font);
        }
        if inner.shadow.is_some() {
            merged.shadow = inner.shadow;
        }
        if inner.insertion.is_some() {
            merged.insertion.clone_from(&inner.insertion);
        }
        if inner.click.is_some() {
            merged.click.clone_from(&inner.click);
        }
        if inner.hover.is_some() {
            merged.hover.clone_from(&inner.hover);
        }
        merged
    }

    /// Write the style's keys into a component object.
    pub fn write(&self, fields: &mut Map<String, Json>) {
        if let Some(color) = &self.color {
            fields.insert("color".into(), color.clone().into());
        }
        for (name, value) in DECORATIONS.iter().zip(self.decorations) {
            if let Some(value) = value {
                fields.insert((*name).into(), value.into());
            }
        }
        if let Some(font) = &self.font {
            fields.insert("font".into(), font.clone().into());
        }
        if let Some(shadow) = self.shadow {
            fields.insert("shadow_color".into(), shadow.into());
        }
        if let Some(insertion) = &self.insertion {
            fields.insert("insertion".into(), insertion.clone().into());
        }
        if let Some(click) = &self.click {
            fields.insert("click_event".into(), click.clone());
        }
        if let Some(hover) = &self.hover {
            fields.insert("hover_event".into(), hover.clone());
        }
    }

    /// The style of a component object's own keys (either event spelling).
    pub fn of(fields: &Map<String, Json>) -> Style {
        let mut style = Style {
            color: fields
                .get("color")
                .and_then(Json::as_str)
                .map(str::to_owned),
            font: fields.get("font").and_then(Json::as_str).map(str::to_owned),
            shadow: fields.get("shadow_color").and_then(Json::as_i64),
            insertion: fields
                .get("insertion")
                .and_then(Json::as_str)
                .map(str::to_owned),
            click: fields
                .get("click_event")
                .cloned()
                .or_else(|| fields.get("clickEvent").and_then(typed_click)),
            hover: fields
                .get("hover_event")
                .cloned()
                .or_else(|| fields.get("hoverEvent").and_then(typed_hover)),
            ..Style::default()
        };
        for (slot, name) in style.decorations.iter_mut().zip(DECORATIONS) {
            *slot = fields.get(name).and_then(Json::as_bool);
        }
        style
    }
}

/// The theme: named styles usable as tags, such as `<accent>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Styles(BTreeMap<String, Style>);

/// The built-in theme, used for Rift's own messages and `/perms`.
pub const DEFAULT_STYLES: [(&str, &str); 7] = [
    ("heading", "<gold>"),
    ("good", "<green>"),
    ("bad", "<red>"),
    ("muted", "<gray>"),
    ("accent", "<aqua>"),
    ("info", "<yellow>"),
    ("plain", ""),
];

impl Default for Styles {
    fn default() -> Self {
        let mut styles = Self(BTreeMap::new());
        for (name, markup) in DEFAULT_STYLES {
            styles
                .set(name, markup)
                .expect("the built-in theme is valid");
        }
        styles
    }
}

impl Styles {
    /// No styles at all.
    pub fn empty() -> Self {
        Self(BTreeMap::new())
    }

    /// Define `<name>` from markup made only of formatting tags, such as
    /// `<#ffaa00><bold>`.
    pub fn set(&mut self, name: &str, markup: &str) -> Result<(), String> {
        let label = format!("style {name:?}");
        custom_name(name, &label)?;
        let style = style_only(markup).map_err(|error| format!("{label}: {error}"))?;
        self.0.insert(name.to_owned(), style);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Style> {
        self.0.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}

/// Parse `markup` into a JSON text component.
pub fn parse(markup: &str, args: &Args, styles: &Styles) -> Result<Json, String> {
    let nodes = Parser {
        args,
        styles,
        depth: 0,
    }
    .parse(markup)?;
    let mut out = Out::default();
    render(&nodes, &Style::default(), &mut None, &mut out)?;
    Ok(out.finish())
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Text(String),
    Tag(Tag),
}

#[derive(Debug, Clone, PartialEq)]
struct Tag {
    close: bool,
    negate: bool,
    name: String,
    args: Vec<String>,
    at: usize,
}

impl Tag {
    /// The arguments from `from` on, joined by `:` again, for values such
    /// as URLs and identifiers that contain colons.
    fn rest(&self, from: usize) -> Option<String> {
        (self.args.len() > from).then(|| self.args[from..].join(":"))
    }

    fn display(&self) -> String {
        let mut text = String::from("<");
        if self.close {
            text.push('/');
        }
        if self.negate {
            text.push('!');
        }
        text.push_str(&self.name);
        for arg in &self.args {
            text.push(':');
            text.push_str(arg);
        }
        text.push('>');
        text
    }

    fn error(&self, message: impl std::fmt::Display) -> String {
        format!("{} at byte {}: {message}", self.display(), self.at)
    }
}

fn tokenize(markup: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut text = String::new();
    let mut index = 0;
    while index < markup.len() {
        let rest = &markup[index..];
        if let Some(escaped) = rest.strip_prefix('\\')
            && let Some(c @ ('<' | '\\')) = escaped.chars().next()
        {
            text.push(c);
            index += 2;
            continue;
        }
        if rest.starts_with('<')
            && let Some((tag, length)) = tag(rest, index)
        {
            if !text.is_empty() {
                tokens.push(Token::Text(std::mem::take(&mut text)));
            }
            tokens.push(Token::Tag(tag));
            index += length;
            continue;
        }
        let c = rest.chars().next().expect("index is inside the string");
        text.push(c);
        index += c.len_utf8();
    }
    if !text.is_empty() {
        tokens.push(Token::Text(text));
    }
    tokens
}

/// A tag at the start of `text`, and its length in bytes.
fn tag(text: &str, at: usize) -> Option<(Tag, usize)> {
    let bytes = text.as_bytes();
    let mut index = 1;
    let (close, negate) = match bytes.get(index) {
        Some(b'/') => {
            index += 1;
            (true, false)
        }
        Some(b'!') => {
            index += 1;
            (false, true)
        }
        _ => (false, false),
    };
    let start = index;
    while bytes
        .get(index)
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'#' | b'.'))
    {
        index += 1;
    }
    if index == start {
        return None;
    }
    let name = text[start..index].to_ascii_lowercase();
    let mut args = Vec::new();
    loop {
        match bytes.get(index)? {
            b'>' => break,
            b':' => {
                index += 1;
                let (arg, length) = argument(&text[index..])?;
                args.push(arg);
                index += length;
            }
            _ => return None,
        }
    }
    let tag = Tag {
        close,
        negate,
        name,
        args,
        at,
    };
    Some((tag, index + 1))
}

/// One tag argument: quoted with `'` or `"` (where `\` escapes the quote and
/// itself), or a run without `:`, `>`, `<` and line breaks.
fn argument(text: &str) -> Option<(String, usize)> {
    let mut chars = text.char_indices();
    match text.chars().next() {
        Some(quote @ ('\'' | '"')) => {
            chars.next();
            let mut value = String::new();
            let mut escaped = false;
            for (index, c) in chars {
                if escaped {
                    if c != quote && c != '\\' {
                        value.push('\\');
                    }
                    value.push(c);
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == quote {
                    return Some((value, index + 1));
                } else {
                    value.push(c);
                }
            }
            None
        }
        _ => {
            let end = text.find([':', '>', '<', '\n', '\r']).unwrap_or(text.len());
            Some((text[..end].to_owned(), end))
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    /// A finished component (argument, translation, key binding, line break).
    Insert(Json),
    Group(Frame, Vec<Node>),
}

#[derive(Debug, Clone, PartialEq)]
enum Frame {
    Style(Style),
    Gradient(Vec<u32>),
    Rainbow(f64),
}

/// An open tag while parsing: which `</...>` closes it and what it holds.
struct Open {
    key: String,
    frame: Frame,
    children: Vec<Node>,
}

struct Parser<'a> {
    args: &'a Args,
    styles: &'a Styles,
    depth: usize,
}

fn colour_name(name: &str) -> Option<&'static str> {
    let name = match name {
        "grey" => "gray",
        "dark_grey" => "dark_gray",
        other => other,
    };
    color::NAMED
        .iter()
        .find(|(named, _)| *named == name)
        .map(|(named, _)| *named)
}

/// A colour argument: a name or `#rrggbb`, normalised to lower case.
fn colour(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    colour_name(&lower)
        .map(str::to_owned)
        .or_else(|| color::hex(&lower).map(|_| lower))
}

fn decoration(name: &str) -> Option<usize> {
    Some(match name {
        "bold" | "b" => 0,
        "italic" | "i" | "em" => 1,
        "underlined" | "u" => 2,
        "strikethrough" | "st" => 3,
        "obfuscated" | "obf" => 4,
        _ => return None,
    })
}

const BUILTIN: [&str; 23] = [
    "color",
    "colour",
    "c",
    "reset",
    "gradient",
    "rainbow",
    "hover",
    "click",
    "insert",
    "font",
    "shadow",
    "newline",
    "br",
    "key",
    "lang",
    "tr",
    "translate",
    "lang_or",
    "tr_or",
    "translate_or",
    "selector",
    "score",
    "nbt",
];

fn builtin(name: &str) -> bool {
    BUILTIN.contains(&name)
        || colour_name(name).is_some()
        || decoration(name).is_some()
        || name.starts_with('#')
}

/// The key `</...>` uses to find the tag it closes.
fn close_key(name: &str) -> String {
    if let Some(index) = decoration(name) {
        return DECORATIONS[index].to_owned();
    }
    if colour_name(name).is_some() || name.starts_with('#') || matches!(name, "colour" | "c") {
        return "color".to_owned();
    }
    name.to_owned()
}

impl Parser<'_> {
    fn parse(&self, markup: &str) -> Result<Vec<Node>, String> {
        if self.depth > 4 {
            return Err("markup nests hover text and translations too deeply".into());
        }
        let mut stack: Vec<Open> = Vec::new();
        let mut root: Vec<Node> = Vec::new();
        fn children<'v>(stack: &'v mut [Open], root: &'v mut Vec<Node>) -> &'v mut Vec<Node> {
            match stack.last_mut() {
                Some(open) => &mut open.children,
                None => root,
            }
        }
        fn close(stack: &mut Vec<Open>, root: &mut Vec<Node>) {
            let open = stack.pop().expect("a tag is open");
            let node = Node::Group(open.frame, open.children);
            children(stack, root).push(node);
        }
        for token in tokenize(markup) {
            let tag = match token {
                Token::Text(text) => {
                    children(&mut stack, &mut root).push(Node::Text(text));
                    continue;
                }
                Token::Tag(tag) => tag,
            };
            if tag.close {
                let key = close_key(&tag.name);
                let Some(position) = stack.iter().rposition(|open| open.key == key) else {
                    return Err(tag.error("closes no open tag"));
                };
                while stack.len() > position {
                    close(&mut stack, &mut root);
                }
                continue;
            }
            if tag.name == "reset" {
                while !stack.is_empty() {
                    close(&mut stack, &mut root);
                }
                continue;
            }
            match self.tag(&tag)? {
                Parsed::Insert(component) => {
                    children(&mut stack, &mut root).push(Node::Insert(component));
                }
                Parsed::Text(text) => {
                    children(&mut stack, &mut root).push(Node::Text(text));
                }
                Parsed::Open(frame) => {
                    if stack.len() >= MAX_DEPTH {
                        return Err(tag.error(format!("more than {MAX_DEPTH} tags are open")));
                    }
                    stack.push(Open {
                        key: close_key(&tag.name),
                        frame,
                        children: Vec::new(),
                    });
                }
            }
        }
        while !stack.is_empty() {
            close(&mut stack, &mut root);
        }
        Ok(root)
    }

    /// Markup inside a tag argument, such as hover text.
    fn nested(&self, markup: &str) -> Result<Json, String> {
        let nodes = Parser {
            args: self.args,
            styles: self.styles,
            depth: self.depth + 1,
        }
        .parse(markup)?;
        let mut out = Out::default();
        render(&nodes, &Style::default(), &mut None, &mut out)?;
        Ok(out.finish())
    }

    fn tag(&self, tag: &Tag) -> Result<Parsed, String> {
        let name = tag.name.as_str();
        let style = |style: Style| Ok(Parsed::Open(Frame::Style(style)));
        let no_args = || {
            if tag.args.is_empty() {
                Ok(())
            } else {
                Err(tag.error("takes no arguments"))
            }
        };
        if tag.negate && !matches!(name, "shadow") && decoration(name).is_none() {
            return Err(tag.error("only decorations and shadow can be negated"));
        }
        if let Some(index) = decoration(name) {
            let value = match tag.args.as_slice() {
                [] => !tag.negate,
                [value] if !tag.negate && value == "true" => true,
                [value] if !tag.negate && value == "false" => false,
                _ => return Err(tag.error("takes no argument, true or false")),
            };
            let mut decorations = [None; 5];
            decorations[index] = Some(value);
            return style(Style {
                decorations,
                ..Style::default()
            });
        }
        if let Some(named) = colour_name(name) {
            no_args()?;
            return style(Style {
                color: Some(named.to_owned()),
                ..Style::default()
            });
        }
        if name.starts_with('#') {
            no_args()?;
            let color = colour(name).ok_or_else(|| tag.error("is not a #rrggbb colour"))?;
            return style(Style {
                color: Some(color),
                ..Style::default()
            });
        }
        match name {
            "color" | "colour" | "c" => {
                let [value] = tag.args.as_slice() else {
                    return Err(tag.error("takes one colour"));
                };
                let color = colour(value).ok_or_else(|| {
                    tag.error(format!("{value:?} is not a colour name or #rrggbb"))
                })?;
                style(Style {
                    color: Some(color),
                    ..Style::default()
                })
            }
            "gradient" => {
                let stops = tag
                    .args
                    .iter()
                    .map(|value| {
                        colour(value)
                            .and_then(|color| color::parse(&color))
                            .ok_or_else(|| {
                                tag.error(format!("{value:?} is not a colour name or #rrggbb"))
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if stops.len() < 2 {
                    return Err(tag.error("needs at least two colours"));
                }
                Ok(Parsed::Open(Frame::Gradient(stops)))
            }
            "rainbow" => {
                let phase = match tag.args.as_slice() {
                    [] => 0.0,
                    [value] => value
                        .parse::<f64>()
                        .ok()
                        .filter(|phase| (0.0..=1.0).contains(phase))
                        .ok_or_else(|| tag.error("phase must be a number in 0..=1"))?,
                    _ => return Err(tag.error("takes at most a phase")),
                };
                Ok(Parsed::Open(Frame::Rainbow(phase)))
            }
            "font" => {
                let font = tag.rest(0).ok_or_else(|| tag.error("needs a font"))?;
                style(Style {
                    font: Some(font),
                    ..Style::default()
                })
            }
            "insert" => {
                let text = tag.rest(0).ok_or_else(|| tag.error("needs text"))?;
                style(Style {
                    insertion: Some(text),
                    ..Style::default()
                })
            }
            "shadow" => {
                let shadow = if tag.negate {
                    no_args()?;
                    0
                } else {
                    let (color, alpha) = match tag.args.as_slice() {
                        [color] => (color, 0.25),
                        [color, alpha] => (
                            color,
                            alpha
                                .parse::<f64>()
                                .ok()
                                .filter(|alpha| (0.0..=1.0).contains(alpha))
                                .ok_or_else(|| tag.error("opacity must be a number in 0..=1"))?,
                        ),
                        _ => return Err(tag.error("takes a colour and an optional opacity")),
                    };
                    let rgb = colour(color)
                        .and_then(|color| color::parse(&color))
                        .ok_or_else(|| {
                            tag.error(format!("{color:?} is not a colour name or #rrggbb"))
                        })?;
                    let alpha = (alpha * 255.0).round() as i64;
                    // Vanilla stores ARGB as a signed int.
                    i64::from(((alpha as u32) << 24 | rgb) as i32)
                };
                style(Style {
                    shadow: Some(shadow),
                    ..Style::default()
                })
            }
            "click" => {
                let action = tag
                    .args
                    .first()
                    .ok_or_else(|| tag.error("needs an action and a value"))?
                    .to_ascii_lowercase();
                let value = tag
                    .rest(1)
                    .ok_or_else(|| tag.error("needs an action and a value"))?;
                let event = match action.as_str() {
                    "open_url" => json!({"action": action, "url": value}),
                    "run_command" | "suggest_command" => {
                        json!({"action": action, "command": value})
                    }
                    "copy_to_clipboard" => json!({"action": action, "value": value}),
                    "change_page" => {
                        let page = value
                            .trim()
                            .parse::<i64>()
                            .ok()
                            .filter(|page| *page >= 1)
                            .ok_or_else(|| tag.error("change_page needs a positive page"))?;
                        json!({"action": action, "page": page})
                    }
                    _ => {
                        return Err(tag.error(
                            "action must be open_url, run_command, suggest_command, \
                             copy_to_clipboard or change_page",
                        ));
                    }
                };
                style(Style {
                    click: Some(event),
                    ..Style::default()
                })
            }
            "hover" => {
                let action = tag
                    .args
                    .first()
                    .ok_or_else(|| tag.error("needs an action"))?
                    .to_ascii_lowercase();
                let event = match action.as_str() {
                    "show_text" => {
                        let markup = tag.rest(1).ok_or_else(|| tag.error("needs text"))?;
                        let text = self.nested(&markup).map_err(|error| tag.error(error))?;
                        json!({"action": "show_text", "value": text})
                    }
                    "show_item" => {
                        // An id may contain `:`; a trailing number is the count.
                        let rest = &tag.args[1..];
                        let (id, count) = match rest.split_last() {
                            Some((last, before)) if !before.is_empty() => {
                                match last.parse::<i64>() {
                                    Ok(count) => (before.join(":"), Some(count)),
                                    Err(_) => (rest.join(":"), None),
                                }
                            }
                            _ => (rest.join(":"), None),
                        };
                        if id.is_empty() {
                            return Err(tag.error("show_item needs an item id"));
                        }
                        match count {
                            Some(count) if count < 1 => {
                                return Err(tag.error("count must be a positive integer"));
                            }
                            Some(count) => json!({"action": "show_item", "id": id, "count": count}),
                            None => json!({"action": "show_item", "id": id}),
                        }
                    }
                    "show_entity" => {
                        // The type may contain `:`; the UUID splits it from the name.
                        let rest = &tag.args[1..];
                        let uuid_at = rest
                            .iter()
                            .position(|arg| uuid_like(arg))
                            .filter(|index| *index > 0)
                            .ok_or_else(|| {
                                tag.error("show_entity takes an entity type, a UUID and a name")
                            })?;
                        let mut event = json!({
                            "action": "show_entity",
                            "id": rest[..uuid_at].join(":"),
                            "uuid": rest[uuid_at],
                        });
                        if uuid_at + 1 < rest.len() {
                            let name = rest[uuid_at + 1..].join(":");
                            event["name"] = self.nested(&name).map_err(|error| tag.error(error))?;
                        }
                        event
                    }
                    _ => {
                        return Err(tag.error("action must be show_text, show_item or show_entity"));
                    }
                };
                style(Style {
                    hover: Some(event),
                    ..Style::default()
                })
            }
            "newline" | "br" => {
                no_args()?;
                Ok(Parsed::Text("\n".into()))
            }
            "key" => {
                let key = tag
                    .rest(0)
                    .ok_or_else(|| tag.error("needs a key binding"))?;
                Ok(Parsed::Insert(json!({"keybind": key})))
            }
            "lang" | "tr" | "translate" | "lang_or" | "tr_or" | "translate_or" => {
                let fallback = name.ends_with("_or");
                let key = tag
                    .args
                    .first()
                    .ok_or_else(|| tag.error("needs a translation key"))?;
                let mut component = json!({"translate": key});
                let mut rest = &tag.args[1..];
                if fallback {
                    let (first, others) = rest
                        .split_first()
                        .ok_or_else(|| tag.error("needs a fallback after the key"))?;
                    component["fallback"] = first.clone().into();
                    rest = others;
                }
                if !rest.is_empty() {
                    let with = rest
                        .iter()
                        .map(|markup| self.nested(markup))
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| tag.error(error))?;
                    component["with"] = Json::Array(with);
                }
                Ok(Parsed::Insert(component))
            }
            "selector" | "score" | "nbt" => {
                Err(tag.error("is not markup; write the JSON component, which the server resolves"))
            }
            _ => {
                if let Some(style) = self.styles.get(name) {
                    no_args()?;
                    return Ok(Parsed::Open(Frame::Style(style.clone())));
                }
                match self.args.get(name) {
                    Some(arg) => {
                        no_args()?;
                        Ok(match arg {
                            Arg::Text(text) => Parsed::Text(text.clone()),
                            Arg::Component(component) => Parsed::Insert(component.clone()),
                        })
                    }
                    None => Err(tag.error("unknown tag; not built in, an argument or a style")),
                }
            }
        }
    }
}

/// A UUID with or without dashes.
fn uuid_like(text: &str) -> bool {
    let digits = text.bytes().filter(u8::is_ascii_hexdigit).count();
    digits == 32 && text.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

enum Parsed {
    Open(Frame),
    Text(String),
    Insert(Json),
}

/// Paints one colour per character of a gradient or a rainbow.
struct Painter {
    frame: Frame,
    total: usize,
    index: usize,
}

impl Painter {
    fn next(&mut self) -> String {
        let t = if self.total > 1 {
            self.index as f64 / (self.total - 1) as f64
        } else {
            0.0
        };
        self.index += 1;
        let rgb = match &self.frame {
            Frame::Gradient(stops) => color::gradient(stops, t),
            Frame::Rainbow(phase) => {
                // A rainbow does not repeat its first colour at the end.
                let t = if self.total > 0 {
                    (self.index - 1) as f64 / self.total as f64
                } else {
                    0.0
                };
                color::rainbow(t, *phase)
            }
            Frame::Style(_) => 0xffffff,
        };
        color::to_hex(rgb)
    }
}

/// Characters a painter colours: text outside tags that set their own colour.
fn painted(nodes: &[Node]) -> usize {
    nodes
        .iter()
        .map(|node| match node {
            Node::Text(text) => text.chars().filter(|c| *c != '\n').count(),
            Node::Insert(_) => 0,
            Node::Group(Frame::Style(style), children) if style.color.is_none() => {
                painted(children)
            }
            Node::Group(..) => 0,
        })
        .sum()
}

/// Text runs being joined into one component, merging neighbours that share
/// a style.
#[derive(Default)]
pub(super) struct Out {
    parts: Vec<Json>,
}

impl Out {
    pub(super) fn text(&mut self, text: &str, style: &Style) {
        let mut fields = Map::new();
        fields.insert("text".into(), text.into());
        style.write(&mut fields);
        if let Some(Json::Object(last)) = self.parts.last_mut() {
            let same = last.len() == fields.len()
                && last.contains_key("text")
                && last
                    .iter()
                    .all(|(key, value)| key == "text" || fields.get(key) == Some(value));
            if same && let Some(Json::String(previous)) = last.get_mut("text") {
                previous.push_str(text);
                return;
            }
        }
        self.parts.push(Json::Object(fields));
    }

    fn insert(&mut self, component: &Json, style: &Style) {
        if style.is_empty() {
            self.parts.push(component.clone());
            return;
        }
        match component {
            // A finished object takes the style keys it does not set itself.
            Json::Object(own) => {
                let mut fields = Map::new();
                style.write(&mut fields);
                for (key, value) in own {
                    fields.insert(key.clone(), value.clone());
                }
                self.parts.push(Json::Object(fields));
            }
            Json::String(text) => self.text(text, style),
            other => {
                let mut fields = Map::new();
                fields.insert("text".into(), "".into());
                style.write(&mut fields);
                fields.insert("extra".into(), json!([other]));
                self.parts.push(Json::Object(fields));
            }
        }
    }

    pub(super) fn finish(mut self) -> Json {
        match self.parts.len() {
            0 => json!({"text": ""}),
            1 if self.parts[0].is_object() => self.parts.remove(0),
            _ => json!({"text": "", "extra": self.parts}),
        }
    }
}

fn render(
    nodes: &[Node],
    style: &Style,
    painter: &mut Option<Painter>,
    out: &mut Out,
) -> Result<(), String> {
    for node in nodes {
        match node {
            Node::Text(text) => match painter {
                Some(painter) => {
                    for c in text.chars() {
                        let mut buffer = [0; 4];
                        if c == '\n' {
                            out.text("\n", style);
                            continue;
                        }
                        let painted = Style {
                            color: Some(painter.next()),
                            ..Style::default()
                        };
                        out.text(c.encode_utf8(&mut buffer), &style.with(&painted));
                    }
                }
                None => out.text(text, style),
            },
            Node::Insert(component) => out.insert(component, style),
            Node::Group(Frame::Style(inner), children) => {
                let merged = style.with(inner);
                if inner.color.is_some() {
                    render(children, &merged, &mut None, out)?;
                } else {
                    render(children, &merged, painter, out)?;
                }
            }
            Node::Group(frame, children) => {
                let total = painted(children);
                if total > MAX_PAINTED {
                    return Err(format!(
                        "a gradient or rainbow may colour at most {MAX_PAINTED} characters"
                    ));
                }
                let mut inner = Some(Painter {
                    frame: frame.clone(),
                    total,
                    index: 0,
                });
                render(children, style, &mut inner, out)?;
            }
        }
    }
    Ok(())
}

/// A style from markup made only of formatting tags, for the theme.
fn style_only(markup: &str) -> Result<Style, String> {
    let parser = Parser {
        args: &Args::new(),
        styles: &Styles::empty(),
        depth: 0,
    };
    let mut style = Style::default();
    for token in tokenize(markup) {
        match token {
            Token::Text(text) if text.trim().is_empty() => {}
            Token::Text(text) => {
                return Err(format!(
                    "only formatting tags are allowed, found text {text:?}"
                ));
            }
            Token::Tag(tag) => match parser.tag(&tag)? {
                Parsed::Open(Frame::Style(inner)) if !tag.close => style = style.with(&inner),
                _ => {
                    return Err(tag.error(
                        "only colour, decoration, font, shadow, insert, click and hover tags \
                         are allowed",
                    ));
                }
            },
        }
    }
    Ok(style)
}

fn typed_click(event: &Json) -> Option<Json> {
    let action = event.get("action")?.as_str()?;
    let value = event.get("value")?;
    let key = match action {
        "open_url" => "url",
        "run_command" | "suggest_command" => "command",
        "change_page" => {
            let page = match value {
                Json::String(page) => page.trim().parse::<i64>().ok()?,
                other => other.as_i64()?,
            };
            return Some(json!({"action": action, "page": page}));
        }
        _ => "value",
    };
    Some(json!({"action": action, key: value}))
}

fn typed_hover(event: &Json) -> Option<Json> {
    let action = event.get("action")?.as_str()?;
    let contents = event.get("contents").or_else(|| event.get("value"))?;
    (action == "show_text").then(|| json!({"action": action, "value": contents}))
}

/// Markup for `component`: the inverse of [`parse`] for what markup can
/// express. Score, selector, NBT and object content is written as its
/// plain text.
pub fn serialize(component: &Json) -> String {
    let mut out = String::new();
    write_markup(component, &Style::default(), &mut out, 0);
    out
}

/// `text` as markup that shows it as written: `<` and `\` are escaped.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    escape_into(text, &mut out);
    out
}

fn escape_into(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '<' => out.push_str("\\<"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
}

/// A tag argument, quoted when it needs to be.
fn quoted(value: &str) -> String {
    if !value.is_empty() && !value.contains([':', '>', '<', '\'', '"', '\\', '\n', '\r']) {
        return value.to_owned();
    }
    let mut text = String::from("'");
    for c in value.chars() {
        if c == '\'' || c == '\\' {
            text.push('\\');
        }
        text.push(c);
    }
    text.push('\'');
    text
}

/// The opening tags that turn `parent` into `child`, and their closing tags.
fn style_tags(parent: &Style, child: &Style) -> (String, String) {
    let mut open = String::new();
    let mut close = Vec::new();
    if child.color != parent.color
        && let Some(color) = &child.color
    {
        open.push_str(&format!("<{color}>"));
        close.push(format!("</{color}>"));
    }
    for (index, name) in DECORATIONS.iter().enumerate() {
        let (was, is) = (parent.decorations[index], child.decorations[index]);
        if was != is {
            match is {
                Some(true) => open.push_str(&format!("<{name}>")),
                Some(false) => open.push_str(&format!("<!{name}>")),
                None => continue,
            }
            close.push(format!("</{name}>"));
        }
    }
    if child.font != parent.font
        && let Some(font) = &child.font
    {
        open.push_str(&format!("<font:{}>", quoted(font)));
        close.push("</font>".into());
    }
    if child.shadow != parent.shadow
        && let Some(shadow) = child.shadow
    {
        let argb = shadow as u32;
        let alpha = f64::from(argb >> 24) / 255.0;
        open.push_str(&format!(
            "<shadow:{}:{}>",
            color::to_hex(argb & 0xff_ffff),
            (alpha * 1000.0).round() / 1000.0
        ));
        close.push("</shadow>".into());
    }
    if child.insertion != parent.insertion
        && let Some(insertion) = &child.insertion
    {
        open.push_str(&format!("<insert:{}>", quoted(insertion)));
        close.push("</insert>".into());
    }
    if child.click != parent.click
        && let Some(tag) = child.click.as_ref().and_then(click_tag)
    {
        open.push_str(&tag);
        close.push("</click>".into());
    }
    if child.hover != parent.hover
        && let Some(tag) = child.hover.as_ref().and_then(hover_tag)
    {
        open.push_str(&tag);
        close.push("</hover>".into());
    }
    close.reverse();
    (open, close.concat())
}

fn click_tag(event: &Json) -> Option<String> {
    let action = event.get("action")?.as_str()?;
    let value = match action {
        "open_url" => event.get("url")?.as_str()?.to_owned(),
        "run_command" | "suggest_command" => event.get("command")?.as_str()?.to_owned(),
        "change_page" => event.get("page")?.as_i64()?.to_string(),
        "copy_to_clipboard" => event.get("value")?.as_str()?.to_owned(),
        _ => return None,
    };
    Some(format!("<click:{action}:{}>", quoted(&value)))
}

fn hover_tag(event: &Json) -> Option<String> {
    match event.get("action")?.as_str()? {
        "show_text" => Some(format!(
            "<hover:show_text:{}>",
            quoted(&serialize(event.get("value")?))
        )),
        "show_item" => {
            let id = event.get("id")?.as_str()?;
            Some(match event.get("count").and_then(Json::as_i64) {
                Some(count) => format!("<hover:show_item:{}:{count}>", quoted(id)),
                None => format!("<hover:show_item:{}>", quoted(id)),
            })
        }
        "show_entity" => {
            let id = event.get("id")?.as_str()?;
            let uuid = event.get("uuid")?.as_str()?;
            let mut tag = format!("<hover:show_entity:{}:{}", quoted(id), quoted(uuid));
            if let Some(name) = event.get("name") {
                tag.push(':');
                tag.push_str(&quoted(&serialize(name)));
            }
            tag.push('>');
            Some(tag)
        }
        _ => None,
    }
}

fn write_markup(component: &Json, parent: &Style, out: &mut String, depth: usize) {
    if depth > 64 {
        return;
    }
    match component {
        Json::String(text) => escape_into(text, out),
        Json::Number(number) => out.push_str(&number.to_string()),
        Json::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        // A list is its first element with the rest as `extra`.
        Json::Array(items) => {
            let Some((first, rest)) = items.split_first() else {
                return;
            };
            match first {
                Json::Object(fields) => {
                    let mut fields = fields.clone();
                    let mut extra = match fields.remove("extra") {
                        Some(Json::Array(extra)) => extra,
                        _ => Vec::new(),
                    };
                    extra.extend(rest.iter().cloned());
                    fields.insert("extra".into(), Json::Array(extra));
                    write_markup(&Json::Object(fields), parent, out, depth + 1);
                }
                other => {
                    write_markup(other, parent, out, depth + 1);
                    rest.iter()
                        .for_each(|item| write_markup(item, parent, out, depth + 1));
                }
            }
        }
        Json::Object(fields) => {
            let style = parent.with(&Style::of(fields));
            let (open, close) = style_tags(parent, &style);
            out.push_str(&open);
            if let Some(text) = fields.get("text").and_then(Json::as_str) {
                escape_into(text, out);
            } else if let Some(key) = fields.get("translate").and_then(Json::as_str) {
                let fallback = fields.get("fallback").and_then(Json::as_str);
                let mut tag = match fallback {
                    Some(fallback) => format!("<lang_or:{}:{}", quoted(key), quoted(fallback)),
                    None => format!("<lang:{}", quoted(key)),
                };
                if let Some(Json::Array(with)) = fields.get("with") {
                    for argument in with {
                        tag.push(':');
                        tag.push_str(&quoted(&serialize(argument)));
                    }
                }
                tag.push('>');
                out.push_str(&tag);
            } else if let Some(key) = fields.get("keybind").and_then(Json::as_str) {
                out.push_str(&format!("<key:{}>", quoted(key)));
            } else {
                let mut plain = String::new();
                super::flatten_own(component, &mut plain);
                escape_into(&plain, out);
            }
            if let Some(Json::Array(extra)) = fields.get("extra") {
                extra
                    .iter()
                    .for_each(|item| write_markup(item, &style, out, depth + 1));
            }
            out.push_str(&close);
        }
        Json::Null => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mini(markup: &str) -> Json {
        parse(markup, &Args::new(), &Styles::default()).unwrap()
    }

    fn error(markup: &str) -> String {
        parse(markup, &Args::new(), &Styles::default()).unwrap_err()
    }

    #[test]
    fn plain_text_stays_one_component() {
        assert_eq!(mini("hello"), json!({"text": "hello"}));
        assert_eq!(mini(""), json!({"text": ""}));
        assert_eq!(
            mini("a < b and <3 you"),
            json!({"text": "a < b and <3 you"})
        );
        assert_eq!(mini(r"\<red> \\"), json!({"text": r"<red> \"}));
    }

    #[test]
    fn colours_and_decorations_nest_and_close() {
        assert_eq!(
            mini("<red>Hi <bold>there</bold>!</red> ok"),
            json!({"text": "", "extra": [
                {"text": "Hi ", "color": "red"},
                {"text": "there", "color": "red", "bold": true},
                {"text": "!", "color": "red"},
                {"text": " ok"},
            ]})
        );
        assert_eq!(mini("<#FF8800>x"), json!({"text": "x", "color": "#ff8800"}));
        assert_eq!(mini("<color:grey>x"), json!({"text": "x", "color": "gray"}));
        assert_eq!(
            mini("<c:#00ff00>x"),
            json!({"text": "x", "color": "#00ff00"})
        );
        assert_eq!(
            mini("<b><i>x</b>y"),
            json!({"text": "", "extra": [{"text": "x", "bold": true, "italic": true}, {"text": "y"}]})
        );
        assert_eq!(mini("<!italic>x"), json!({"text": "x", "italic": false}));
        assert_eq!(mini("<bold:false>x"), json!({"text": "x", "bold": false}));
        assert_eq!(
            mini("<red><u>a<reset>b"),
            json!({"text": "", "extra": [{"text": "a", "color": "red", "underlined": true}, {"text": "b"}]})
        );
        // A colour closes with any colour tag.
        assert_eq!(
            mini("<red>a</color>b"),
            json!({"text": "", "extra": [{"text": "a", "color": "red"}, {"text": "b"}]})
        );
        assert_eq!(mini("a<newline>b<br>c"), json!({"text": "a\nb\nc"}));
    }

    #[test]
    fn gradients_and_rainbows_colour_each_character() {
        assert_eq!(
            mini("<gradient:#ff0000:#0000ff>abc</gradient>"),
            json!({"text": "", "extra": [
                {"text": "a", "color": "#ff0000"},
                {"text": "b", "color": "#800080"},
                {"text": "c", "color": "#0000ff"},
            ]})
        );
        // An inner colour wins and is not counted.
        assert_eq!(
            mini("<gradient:red:blue>a<green>x</green>b</gradient>"),
            json!({"text": "", "extra": [
                {"text": "a", "color": "#ff5555"},
                {"text": "x", "color": "green"},
                {"text": "b", "color": "#5555ff"},
            ]})
        );
        let rainbow = mini("<rainbow>abc");
        assert_eq!(rainbow["extra"][0]["color"], "#ff0000");
        assert_eq!(rainbow["extra"][1]["color"], "#00ff00");
        assert_eq!(rainbow["extra"][2]["color"], "#0000ff");
        assert_eq!(
            error("<gradient:red>x"),
            "<gradient:red> at byte 0: needs at least two colours"
        );
        assert!(error(&format!("<rainbow>{}", "x".repeat(600))).contains("at most 512"));
    }

    #[test]
    fn events_insertion_font_and_shadow() {
        assert_eq!(
            mini("<click:open_url:https://example.com/a?b=c>site"),
            json!({"text": "site", "click_event": {"action": "open_url", "url": "https://example.com/a?b=c"}})
        );
        assert_eq!(
            mini("<click:run_command:'/say hi: there'>x"),
            json!({"text": "x", "click_event": {"action": "run_command", "command": "/say hi: there"}})
        );
        assert_eq!(
            mini("<click:change_page:3>x"),
            json!({"text": "x", "click_event": {"action": "change_page", "page": 3}})
        );
        assert_eq!(
            mini("<hover:show_text:'<green>Hi <bold>you'>x"),
            json!({"text": "x", "hover_event": {"action": "show_text", "value": {"text": "", "extra": [
                {"text": "Hi ", "color": "green"},
                {"text": "you", "color": "green", "bold": true},
            ]}}})
        );
        assert_eq!(
            mini("<hover:show_item:minecraft:diamond>x")["hover_event"],
            json!({"action": "show_item", "id": "minecraft:diamond"})
        );
        assert_eq!(
            mini("<hover:show_item:minecraft:diamond:3>x")["hover_event"],
            json!({"action": "show_item", "id": "minecraft:diamond", "count": 3})
        );
        assert_eq!(
            mini(
                "<hover:show_entity:minecraft:pig:00000000-0000-0000-0000-000000000001:'<red>Bob'>x"
            )["hover_event"],
            json!({"action": "show_entity", "id": "minecraft:pig",
                   "uuid": "00000000-0000-0000-0000-000000000001",
                   "name": {"text": "Bob", "color": "red"}})
        );
        assert_eq!(
            mini("<hover:show_item:'minecraft:diamond':2>x")["hover_event"],
            json!({"action": "show_item", "id": "minecraft:diamond", "count": 2})
        );
        assert_eq!(mini("<insert:hello>x")["insertion"], "hello");
        assert_eq!(
            mini("<font:minecraft:uniform>x")["font"],
            "minecraft:uniform"
        );
        assert_eq!(mini("<shadow:black:1>x")["shadow_color"], -16777216);
        assert_eq!(mini("<shadow:#ff0000>x")["shadow_color"], 0x40ff0000);
        assert_eq!(mini("<!shadow>x")["shadow_color"], 0);
        assert_eq!(mini("<key:key.jump>"), json!({"keybind": "key.jump"}));
        assert_eq!(
            mini("<lang_or:my.key:'Fallback':'<red>a'>"),
            json!({"translate": "my.key", "fallback": "Fallback", "with": [{"text": "a", "color": "red"}]})
        );
        assert_eq!(
            mini("<red><lang:block.minecraft.stone>"),
            json!({"color": "red", "translate": "block.minecraft.stone"})
        );
    }

    #[test]
    fn arguments_are_never_parsed() {
        let args = Args::new()
            .text("player", "<red>Steve</red>")
            .component("rank", json!({"text": "Admin", "color": "gold"}));
        assert_eq!(
            parse("<gray>Hi <player> [<rank>]", &args, &Styles::default()).unwrap(),
            json!({"text": "", "extra": [
                {"text": "Hi <red>Steve</red> [", "color": "gray"},
                {"color": "gold", "text": "Admin"},
                {"text": "]", "color": "gray"},
            ]})
        );
        assert_eq!(
            parse(
                "<gradient:red:blue><player>",
                &Args::new().text("player", "ab"),
                &Styles::default()
            )
            .unwrap()["extra"][1]["color"],
            "#5555ff"
        );
        assert!(Args::new().text("red", "x").validate("args").is_err());
        assert!(Args::new().text("Bad Name", "x").validate("args").is_err());
        assert!(
            Args::new()
                .text("player_name", "x")
                .validate("args")
                .is_ok()
        );
    }

    #[test]
    fn theme_styles_are_tags() {
        let mut styles = Styles::default();
        styles.set("accent", "<#ffaa00><bold>").unwrap();
        assert_eq!(
            parse("<accent>x</accent>y", &Args::new(), &styles).unwrap(),
            json!({"text": "", "extra": [{"text": "x", "color": "#ffaa00", "bold": true}, {"text": "y"}]})
        );
        assert_eq!(
            styles.set("bad", "<red>text").unwrap_err(),
            "style \"bad\": only formatting tags are allowed, found text \"text\""
        );
        assert_eq!(
            styles.set("bold", "<red>").unwrap_err(),
            "style \"bold\": <bold> is a built-in tag"
        );
        assert!(styles.set("x", "<gradient:red:blue>").is_err());
    }

    #[test]
    fn mistakes_are_errors_with_positions() {
        assert_eq!(
            error("hi <bolt>x"),
            "<bolt> at byte 3: unknown tag; not built in, an argument or a style"
        );
        assert_eq!(error("x</red>"), "</red> at byte 1: closes no open tag");
        assert_eq!(
            error("<color:orange>x"),
            "<color:orange> at byte 0: \"orange\" is not a colour name or #rrggbb"
        );
        assert_eq!(
            error("<click:open_file:/etc>x"),
            "<click:open_file:/etc> at byte 0: action must be open_url, run_command, \
             suggest_command, copy_to_clipboard or change_page"
        );
        assert_eq!(error("<red:x>a"), "<red:x> at byte 0: takes no arguments");
        assert_eq!(
            error("<!red>a"),
            "<!red> at byte 0: only decorations and shadow can be negated"
        );
        assert!(error("<hover:show_text:'<bolt>'>x").contains("unknown tag"));
    }

    #[test]
    fn serialize_round_trips() {
        for markup in [
            "plain",
            "<red>Hi <bold>there</bold>!</red> ok",
            "<#ff8800>x<!italic>y",
            "<click:run_command:'/say a:b'><u>c</u></click>",
            "<hover:show_text:'<green>tip'>x</hover>",
            "<insert:hi>x</insert><font:minecraft:uniform>y",
            "<shadow:#000000:1>x",
            "<lang_or:k:'F':'<red>a'> <key:key.jump>",
            r"literal \<red> and \\",
            "line<br>break",
        ] {
            let parsed = mini(markup);
            let serialized = serialize(&parsed);
            assert_eq!(mini(&serialized), parsed, "{markup} -> {serialized}");
        }
        assert_eq!(
            serialize(&json!({"text": "a", "color": "red"})),
            "<red>a</red>"
        );
        assert_eq!(
            serialize(&json!([{"text": "a", "bold": true}, "b"])),
            "<bold>ab</bold>"
        );
    }
}
