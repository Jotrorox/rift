//! One model for the text components of every protocol from 1.12 to 26.3.
//!
//! The model holds the union of what any version can express. Reading
//! (see [`super::decode`]) accepts a protocol's own format as leniently as
//! that version's client; writing (see [`super::encode`]) emits exactly the
//! target protocol's format and degrades what it cannot express.
use crate::nbt::{Compound, Nbt};

/// A text component: its content, its style and the siblings appended to it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Component {
    pub content: Content,
    pub style: Style,
    pub extra: Vec<Component>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(String),
    Translate {
        key: String,
        /// 1.19.4 and newer: shown when the client lacks the key.
        fallback: Option<String>,
        args: Vec<Arg>,
    },
    Score {
        name: String,
        objective: String,
        /// Before 1.16 a resolved score could travel with the component.
        value: Option<String>,
    },
    Selector {
        pattern: String,
        /// 1.17 and newer.
        separator: Option<Box<Component>>,
    },
    Keybind(String),
    /// 1.14 and newer (the storage source from 1.15).
    Nbt {
        path: String,
        interpret: bool,
        /// 26.1 and newer.
        plain: bool,
        separator: Option<Box<Component>>,
        source: NbtSource,
    },
    /// 1.21.9 and newer: a sprite or player head drawn inline.
    Object {
        object: ObjectInfo,
        fallback: Option<Box<Component>>,
    },
}

impl Default for Content {
    fn default() -> Self {
        Content::Text(String::new())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum NbtSource {
    Block(String),
    Entity(String),
    Storage(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ObjectInfo {
    /// `atlas` is `None` for the default atlas.
    Atlas {
        atlas: Option<String>,
        sprite: String,
    },
    /// The player profile is passed through as read.
    Player { player: Nbt, hat: bool },
}

/// A translation argument: a component, or (1.20.3 and newer) a string,
/// number or boolean as such.
#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    Component(Box<Component>),
    /// A string or numeric tag; booleans are bytes.
    Primitive(Nbt),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    pub color: Option<Color>,
    /// 1.21.4 and newer: ARGB.
    pub shadow_color: Option<i32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underlined: Option<bool>,
    pub strikethrough: Option<bool>,
    pub obfuscated: Option<bool>,
    pub click: Option<ClickEvent>,
    pub hover: Option<HoverEvent>,
    pub insertion: Option<String>,
    /// 1.16 and newer.
    pub font: Option<String>,
}

impl Style {
    pub fn is_empty(&self) -> bool {
        *self == Style::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// One of the 16 named colours, by its `ChatFormatting` index.
    Named(u8),
    /// 1.16 and newer.
    Rgb(u32),
    /// Before 1.16 `reset` was accepted as a colour.
    Reset,
}

/// The 16 named colours in `ChatFormatting` order, with their RGB values.
pub const NAMED_COLORS: [(&str, u32); 16] = [
    ("black", 0x000000),
    ("dark_blue", 0x0000aa),
    ("dark_green", 0x00aa00),
    ("dark_aqua", 0x00aaaa),
    ("dark_red", 0xaa0000),
    ("dark_purple", 0xaa00aa),
    ("gold", 0xffaa00),
    ("gray", 0xaaaaaa),
    ("dark_gray", 0x555555),
    ("blue", 0x5555ff),
    ("green", 0x55ff55),
    ("aqua", 0x55ffff),
    ("red", 0xff5555),
    ("light_purple", 0xff55ff),
    ("yellow", 0xffff55),
    ("white", 0xffffff),
];

impl Color {
    /// A colour name as JSON and NBT carry it: a named colour, `#RRGGBB`,
    /// or `reset`. `hex` admits custom colours.
    pub fn parse(name: &str, hex: bool) -> Option<Color> {
        if let Some(digits) = name.strip_prefix('#') {
            if !hex || digits.is_empty() || digits.len() > 8 {
                return None;
            }
            let value = u32::from_str_radix(digits, 16).ok()?;
            return (value <= 0xff_ffff).then_some(Color::Rgb(value));
        }
        if name == "reset" {
            return Some(Color::Reset);
        }
        NAMED_COLORS
            .iter()
            .position(|(known, _)| *known == name)
            .map(|index| Color::Named(index as u8))
    }

    pub fn rgb(self) -> Option<u32> {
        match self {
            Color::Named(index) => Some(NAMED_COLORS[usize::from(index)].1),
            Color::Rgb(value) => Some(value),
            Color::Reset => None,
        }
    }

    /// The named colour closest to this one, for clients before 1.16.
    pub fn nearest_named(self) -> Color {
        match self {
            Color::Rgb(value) => {
                let distance = |other: u32| {
                    let channel = |shift: u32| {
                        let a = ((value >> shift) & 0xff) as i32;
                        let b = ((other >> shift) & 0xff) as i32;
                        (a - b) * (a - b)
                    };
                    channel(16) + channel(8) + channel(0)
                };
                let index = (0..16)
                    .min_by_key(|index| distance(NAMED_COLORS[*index].1))
                    .unwrap_or(15);
                Color::Named(index as u8)
            }
            other => other,
        }
    }

    /// The colour as JSON/NBT write it. A named colour keeps its name; a
    /// custom colour that equals a named one is still written in hex, as
    /// vanilla does.
    pub fn name(self) -> String {
        match self {
            Color::Named(index) => NAMED_COLORS[usize::from(index)].0.to_owned(),
            Color::Rgb(value) => format!("#{value:06X}"),
            Color::Reset => "reset".to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ClickEvent {
    OpenUrl(String),
    OpenFile(String),
    RunCommand(String),
    SuggestCommand(String),
    /// The page as text: before 1.21.5 any string, parsed by the client.
    ChangePage(String),
    /// 1.15 and newer.
    CopyToClipboard(String),
    /// 1.21.6 and newer: a dialog ID or inline definition, passed through.
    ShowDialog(Nbt),
    /// 1.21.6 and newer.
    Custom {
        id: String,
        payload: Option<Nbt>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum HoverEvent {
    ShowText(Box<Component>),
    ShowItem(ItemInfo),
    ShowEntity(EntityInfo),
}

/// The item a `show_item` hover event shows.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemInfo {
    /// The namespaced item ID.
    pub id: String,
    pub count: i32,
    /// Before 1.13: the item's data value.
    pub damage: Option<i16>,
    /// Before 1.20.5: the stack's NBT tag.
    pub tag: Option<Compound>,
    /// 1.20.5 and newer: the data component patch in its NBT form.
    pub components: Option<Compound>,
}

/// The entity a `show_entity` hover event shows.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityInfo {
    /// The namespaced entity type.
    pub kind: String,
    pub id: EntityId,
    pub name: Option<Box<Component>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum EntityId {
    Uuid(u128),
    /// Before 1.16 the ID was free text.
    Text(String),
}

impl Component {
    pub fn text(text: impl Into<String>) -> Self {
        Component {
            content: Content::Text(text.into()),
            ..Default::default()
        }
    }

    pub fn translate(key: impl Into<String>, args: Vec<Arg>) -> Self {
        Component {
            content: Content::Translate {
                key: key.into(),
                fallback: None,
                args,
            },
            ..Default::default()
        }
    }

    /// The plain text when this is unstyled text without siblings, as the
    /// 1.20.3+ codecs collapse it to a bare string.
    pub fn as_plain(&self) -> Option<&str> {
        match &self.content {
            Content::Text(text) if self.style.is_empty() && self.extra.is_empty() => Some(text),
            _ => None,
        }
    }

    /// The text this component shows without translations resolved, as
    /// vanilla's `getString` on content it cannot resolve: literal text,
    /// keys, and the resolved score value.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        self.plain_into(&mut out, 0);
        out
    }

    fn plain_into(&self, out: &mut String, depth: usize) {
        if depth > crate::wire::MAX_NBT_DEPTH {
            return;
        }
        match &self.content {
            Content::Text(text) => out.push_str(text),
            Content::Translate {
                key,
                fallback,
                args,
            } => {
                let format = super::legacy::translation(key)
                    .map(str::to_owned)
                    .or_else(|| fallback.clone());
                match format {
                    Some(format) => {
                        super::legacy::substitute(&format, args, out, |arg, out| match arg {
                            Arg::Component(component) => component.plain_into(out, depth + 1),
                            Arg::Primitive(value) => out.push_str(&primitive_text(value)),
                        })
                    }
                    None => out.push_str(key),
                }
            }
            Content::Score { value, .. } => out.push_str(value.as_deref().unwrap_or("")),
            Content::Selector { pattern, .. } => out.push_str(pattern),
            Content::Keybind(key) => out.push_str(key),
            Content::Nbt { .. } => {}
            Content::Object { fallback, .. } => {
                if let Some(fallback) = fallback {
                    fallback.plain_into(out, depth + 1);
                }
            }
        }
        for child in &self.extra {
            child.plain_into(out, depth + 1);
        }
    }

    /// Visit this component and every component nested in it (siblings,
    /// arguments, separators, hover text, entity names), outermost first.
    pub fn walk_mut(&mut self, visit: &mut impl FnMut(&mut Component)) {
        self.walk_inner(visit, 0);
    }

    fn walk_inner(&mut self, visit: &mut impl FnMut(&mut Component), depth: usize) {
        if depth > crate::wire::MAX_NBT_DEPTH {
            return;
        }
        visit(self);
        match &mut self.content {
            Content::Translate { args, .. } => {
                for arg in args {
                    if let Arg::Component(component) = arg {
                        component.walk_inner(visit, depth + 1);
                    }
                }
            }
            Content::Selector {
                separator: Some(separator),
                ..
            }
            | Content::Nbt {
                separator: Some(separator),
                ..
            }
            | Content::Object {
                fallback: Some(separator),
                ..
            } => separator.walk_inner(visit, depth + 1),
            _ => {}
        }
        match &mut self.style.hover {
            Some(HoverEvent::ShowText(text)) => text.walk_inner(visit, depth + 1),
            Some(HoverEvent::ShowEntity(EntityInfo {
                name: Some(name), ..
            })) => name.walk_inner(visit, depth + 1),
            _ => {}
        }
        for child in &mut self.extra {
            child.walk_inner(visit, depth + 1);
        }
    }
}

/// A primitive argument as `String.valueOf` prints it.
pub fn primitive_text(value: &Nbt) -> String {
    match value {
        Nbt::String(text) => text.clone(),
        Nbt::Byte(value) => value.to_string(),
        Nbt::Short(value) => value.to_string(),
        Nbt::Int(value) => value.to_string(),
        Nbt::Long(value) => value.to_string(),
        Nbt::Float(value) => super::snbt::java_float(f64::from(*value), true),
        Nbt::Double(value) => super::snbt::java_float(*value, false),
        other => format!("{other:?}"),
    }
}
