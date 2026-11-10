//! Writing text components in each protocol's format: vanilla's Gson
//! serializer before 1.20.3 (style keys, then `extra`, then the content) and
//! the codec from 1.20.3 on (content, `extra`, style; plain text collapses to
//! a string; NBT compounds in Java `HashMap` order).
//!
//! What the target cannot express degrades to something it reads: custom
//! colours become the nearest named colour, unknown click actions and
//! hover data are dropped, content types it lacks become their fallback or
//! empty text with the style and siblings kept.
use super::{component::*, java_order, json::Json, snbt, uuid_ints, version as v};
use crate::nbt::{Compound, Nbt};

/// A value being written, before it becomes JSON or NBT.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Tree {
    Bool(bool),
    Int(i32),
    Str(String),
    List(Vec<Tree>),
    Map(Vec<(&'static str, Tree)>),
    /// Passed through: dialogs, payloads, profiles, components, UUIDs.
    Nbt(Nbt),
}

impl Tree {
    fn str(text: impl Into<String>) -> Self {
        Tree::Str(text.into())
    }

    pub(super) fn to_json(&self) -> Json {
        match self {
            Tree::Bool(value) => Json::Bool(*value),
            Tree::Int(value) => Json::int(i64::from(*value)),
            Tree::Str(text) => Json::str(text.clone()),
            Tree::List(values) => Json::Array(values.iter().map(Tree::to_json).collect()),
            Tree::Map(entries) => Json::Object(
                entries
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.to_json()))
                    .collect(),
            ),
            Tree::Nbt(nbt) => nbt_to_json(nbt),
        }
    }

    pub(super) fn to_nbt(&self) -> Nbt {
        match self {
            Tree::Bool(value) => Nbt::Byte(i8::from(*value)),
            Tree::Int(value) => Nbt::Int(*value),
            Tree::Str(text) => Nbt::String(text.clone()),
            Tree::List(values) => homogenize(values.iter().map(Tree::to_nbt).collect()),
            Tree::Map(entries) => {
                let keys: Vec<&str> = entries.iter().map(|(key, _)| *key).collect();
                Nbt::Compound(Compound(
                    java_order(&keys)
                        .into_iter()
                        .map(|index| (entries[index].0.to_owned(), entries[index].1.to_nbt()))
                        .collect(),
                ))
            }
            Tree::Nbt(nbt) => nbt.clone(),
        }
    }
}

/// A list as vanilla writes one: of one element type, or else of
/// compounds with every other value (and every compound that looks like a
/// wrapper) wrapped under the key `""`.
pub(super) fn homogenize(values: Vec<Nbt>) -> Nbt {
    let Some(first) = values.first() else {
        return Nbt::List(values);
    };
    let kind = first.kind();
    if values.iter().all(|value| value.kind() == kind) {
        return Nbt::List(values);
    }
    Nbt::List(
        values
            .into_iter()
            .map(|value| match value {
                Nbt::Compound(compound) if !(compound.len() == 1 && compound.contains("")) => {
                    Nbt::Compound(compound)
                }
                other => Nbt::Compound(Compound(vec![(String::new(), other)])),
            })
            .collect(),
    )
}

/// NBT as JSON the way `NbtOps` to `JsonOps` converts it.
pub fn nbt_to_json(nbt: &Nbt) -> Json {
    match nbt {
        Nbt::Byte(value) => Json::int(i64::from(*value)),
        Nbt::Short(value) => Json::int(i64::from(*value)),
        Nbt::Int(value) => Json::int(i64::from(*value)),
        Nbt::Long(value) => Json::int(*value),
        Nbt::Float(value) => Json::double(f64::from(*value)),
        Nbt::Double(value) => Json::double(*value),
        Nbt::String(text) => Json::str(text.clone()),
        Nbt::ByteArray(values) => Json::Array(
            values
                .iter()
                .map(|value| Json::int(i64::from(*value as i8)))
                .collect(),
        ),
        Nbt::IntArray(values) => Json::Array(
            values
                .iter()
                .map(|value| Json::int(i64::from(*value)))
                .collect(),
        ),
        Nbt::LongArray(values) => {
            Json::Array(values.iter().map(|value| Json::int(*value)).collect())
        }
        Nbt::List(values) => Json::Array(
            values
                .iter()
                .map(|value| match value {
                    Nbt::Compound(compound) if compound.len() == 1 && compound.contains("") => {
                        nbt_to_json(compound.get("").unwrap_or(value))
                    }
                    other => nbt_to_json(other),
                })
                .collect(),
        ),
        Nbt::Compound(compound) => Json::Object(
            compound
                .iter()
                .map(|(key, value)| (key.to_owned(), nbt_to_json(value)))
                .collect(),
        ),
    }
}

/// Write `component` for `protocol`. Before 1.20.3 the result is always an
/// object (as vanilla wrote it); from 1.20.3 plain text is a bare string.
pub(super) fn encode(protocol: i32, component: &Component) -> Tree {
    Writer { protocol }.component(component, 0)
}

struct Writer {
    protocol: i32,
}

impl Writer {
    fn codec(&self) -> bool {
        self.protocol >= v::NBT_ENCODING
    }

    fn component(&self, component: &Component, depth: usize) -> Tree {
        if depth > crate::wire::MAX_NBT_DEPTH {
            return Tree::str("");
        }
        let (content, extra) = self.degrade(component);
        let content = content.as_ref().unwrap_or(&component.content);
        let extra = extra.as_deref().unwrap_or(&component.extra);
        if self.codec()
            && let Content::Text(text) = content
            && extra.is_empty()
            && self.style_entries(&component.style, depth).is_empty()
        {
            return Tree::str(text.clone());
        }
        let mut entries = Vec::new();
        let content_entries = self.content(content, depth);
        let style_entries = self.style_entries(&component.style, depth);
        let extra_entry = (!extra.is_empty()).then(|| {
            (
                "extra",
                Tree::List(
                    extra
                        .iter()
                        .map(|child| self.component(child, depth + 1))
                        .collect(),
                ),
            )
        });
        if self.codec() {
            entries.extend(content_entries);
            entries.extend(extra_entry);
            entries.extend(style_entries);
        } else {
            entries.extend(style_entries);
            entries.extend(extra_entry);
            entries.extend(content_entries);
        }
        Tree::Map(entries)
    }

    /// Content the target lacks, replaced: the new content and, when the
    /// replacement adds a sibling, the new sibling list.
    fn degrade(&self, component: &Component) -> (Option<Content>, Option<Vec<Component>>) {
        let empty = || Some(Content::Text(String::new()));
        match &component.content {
            Content::Nbt { .. } if self.protocol < v::NBT_CONTENT => (empty(), None),
            Content::Nbt {
                source: NbtSource::Storage(_),
                ..
            } if self.protocol < v::NBT_STORAGE => (empty(), None),
            Content::Score {
                value: Some(value), ..
            } if self.protocol >= v::HOVER_CONTENTS => (Some(Content::Text(value.clone())), None),
            Content::Object { fallback, .. } if self.protocol < v::OBJECT_CONTENT => {
                let mut extra = component.extra.clone();
                if let Some(fallback) = fallback {
                    extra.insert(0, (**fallback).clone());
                }
                (empty(), Some(extra))
            }
            _ => (None, None),
        }
    }

    fn content(&self, content: &Content, depth: usize) -> Vec<(&'static str, Tree)> {
        let mut entries = Vec::new();
        match content {
            Content::Text(text) => entries.push(("text", Tree::str(text.clone()))),
            Content::Translate {
                key,
                fallback,
                args,
            } => {
                entries.push(("translate", Tree::str(key.clone())));
                if let Some(fallback) = fallback
                    .as_ref()
                    .filter(|_| self.protocol >= v::TRANSLATE_FALLBACK)
                {
                    entries.push(("fallback", Tree::str(fallback.clone())));
                }
                if !args.is_empty() {
                    entries.push((
                        "with",
                        Tree::List(args.iter().map(|arg| self.argument(arg, depth)).collect()),
                    ));
                }
            }
            Content::Score {
                name,
                objective,
                value,
            } => {
                let mut score = vec![
                    ("name", Tree::str(name.clone())),
                    ("objective", Tree::str(objective.clone())),
                ];
                if let Some(value) = value.as_ref().filter(|_| self.protocol < v::HOVER_CONTENTS) {
                    score.push(("value", Tree::str(value.clone())));
                }
                entries.push(("score", Tree::Map(score)));
            }
            Content::Selector { pattern, separator } => {
                entries.push(("selector", Tree::str(pattern.clone())));
                self.separator(separator, depth, &mut entries);
            }
            Content::Keybind(key) => entries.push(("keybind", Tree::str(key.clone()))),
            Content::Nbt {
                path,
                interpret,
                plain,
                separator,
                source,
            } => {
                entries.push(("nbt", Tree::str(path.clone())));
                // The codec leaves out defaults; the old serializer did not.
                if *interpret || !self.codec() {
                    entries.push(("interpret", Tree::Bool(*interpret)));
                }
                if *plain && self.protocol >= v::NBT_PLAIN {
                    entries.push(("plain", Tree::Bool(true)));
                }
                self.separator(separator, depth, &mut entries);
                entries.push(match source {
                    NbtSource::Block(position) => ("block", Tree::str(position.clone())),
                    NbtSource::Entity(selector) => ("entity", Tree::str(selector.clone())),
                    NbtSource::Storage(id) => ("storage", Tree::str(id.clone())),
                });
            }
            Content::Object { object, fallback } => {
                match object {
                    ObjectInfo::Atlas { atlas, sprite } => {
                        if let Some(atlas) = atlas.as_ref().filter(|atlas| !is_default_atlas(atlas))
                        {
                            entries.push(("atlas", Tree::str(atlas.clone())));
                        }
                        entries.push(("sprite", Tree::str(sprite.clone())));
                    }
                    ObjectInfo::Player { player, hat } => {
                        entries.push(("player", Tree::Nbt(player.clone())));
                        if !hat {
                            entries.push(("hat", Tree::Bool(false)));
                        }
                    }
                }
                if let Some(fallback) = fallback {
                    entries.push(("fallback", self.component(fallback, depth + 1)));
                }
            }
        }
        entries
    }

    fn separator(
        &self,
        separator: &Option<Box<Component>>,
        depth: usize,
        entries: &mut Vec<(&'static str, Tree)>,
    ) {
        if let Some(separator) = separator.as_ref().filter(|_| self.protocol >= v::SEPARATOR) {
            entries.push(("separator", self.component(separator, depth + 1)));
        }
    }

    fn argument(&self, arg: &Arg, depth: usize) -> Tree {
        match arg {
            Arg::Component(component) => {
                // The codec writes plain-text arguments as strings.
                if self.codec()
                    && let Some(text) = component.as_plain()
                {
                    return Tree::str(text);
                }
                self.component(component, depth + 1)
            }
            Arg::Primitive(value) if self.codec() => Tree::Nbt(value.clone()),
            Arg::Primitive(value) => Tree::str(primitive_text(value)),
        }
    }

    fn style_entries(&self, style: &Style, depth: usize) -> Vec<(&'static str, Tree)> {
        let mut entries = Vec::new();
        let color = style.color.and_then(|color| match color {
            Color::Reset if self.protocol >= v::HEX_COLORS => None,
            Color::Rgb(_) if self.protocol < v::HEX_COLORS => Some(color.nearest_named()),
            other => Some(other),
        });
        let flags = [
            ("bold", style.bold),
            ("italic", style.italic),
            ("underlined", style.underlined),
            ("strikethrough", style.strikethrough),
            ("obfuscated", style.obfuscated),
        ];
        let click = style.click.as_ref().and_then(|click| self.click(click));
        let hover = style
            .hover
            .as_ref()
            .and_then(|hover| self.hover(hover, depth));
        let font = style.font.as_ref().filter(|_| self.protocol >= v::FONT);
        let (click_key, hover_key) = if self.protocol >= v::SNAKE_CASE_EVENTS {
            ("click_event", "hover_event")
        } else {
            ("clickEvent", "hoverEvent")
        };
        if self.codec() {
            if let Some(color) = color {
                entries.push(("color", Tree::str(color.name())));
            }
            if let Some(shadow) = style
                .shadow_color
                .filter(|_| self.protocol >= v::SHADOW_COLOR)
            {
                entries.push(("shadow_color", Tree::Int(shadow)));
            }
            for (key, flag) in flags {
                if let Some(flag) = flag {
                    entries.push((key, Tree::Bool(flag)));
                }
            }
            entries.extend(click.map(|click| (click_key, click)));
            entries.extend(hover.map(|hover| (hover_key, hover)));
            if let Some(insertion) = &style.insertion {
                entries.push(("insertion", Tree::str(insertion.clone())));
            }
            if let Some(font) = font {
                entries.push(("font", Tree::str(font.clone())));
            }
        } else {
            for (key, flag) in flags {
                if let Some(flag) = flag {
                    entries.push((key, Tree::Bool(flag)));
                }
            }
            if let Some(color) = color {
                entries.push(("color", Tree::str(color.name())));
            }
            if let Some(insertion) = &style.insertion {
                entries.push(("insertion", Tree::str(insertion.clone())));
            }
            entries.extend(click.map(|click| (click_key, click)));
            entries.extend(hover.map(|hover| (hover_key, hover)));
            if let Some(font) = font {
                entries.push(("font", Tree::str(font.clone())));
            }
        }
        entries
    }

    fn click(&self, click: &ClickEvent) -> Option<Tree> {
        let snake = self.protocol >= v::SNAKE_CASE_EVENTS;
        let (action, key, value) = match click {
            ClickEvent::OpenUrl(url) => {
                if snake && !is_web_url(url) {
                    return None;
                }
                ("open_url", "url", Tree::str(url.clone()))
            }
            // Servers may not send it; clients ignore or reject it.
            ClickEvent::OpenFile(_) => return None,
            ClickEvent::RunCommand(command) => (
                "run_command",
                "command",
                Tree::str(if snake {
                    chat_string(command, false)
                } else {
                    command.clone()
                }),
            ),
            ClickEvent::SuggestCommand(command) => (
                "suggest_command",
                "command",
                Tree::str(if snake {
                    chat_string(command, true)
                } else {
                    command.clone()
                }),
            ),
            ClickEvent::ChangePage(page) => {
                if snake {
                    let page: i32 = page.trim().parse().ok().filter(|page| *page >= 1)?;
                    ("change_page", "page", Tree::Int(page))
                } else {
                    ("change_page", "value", Tree::str(page.clone()))
                }
            }
            ClickEvent::CopyToClipboard(text) => {
                if self.protocol < v::COPY_TO_CLIPBOARD {
                    return None;
                }
                ("copy_to_clipboard", "value", Tree::str(text.clone()))
            }
            ClickEvent::ShowDialog(dialog) => {
                if self.protocol < v::DIALOG_CLICK {
                    return None;
                }
                ("show_dialog", "dialog", Tree::Nbt(dialog.clone()))
            }
            ClickEvent::Custom { id, payload } => {
                if self.protocol < v::DIALOG_CLICK {
                    return None;
                }
                let mut entries = vec![
                    ("action", Tree::str("custom")),
                    ("id", Tree::str(id.clone())),
                ];
                if let Some(payload) = payload {
                    entries.push(("payload", Tree::Nbt(payload.clone())));
                }
                return Some(Tree::Map(entries));
            }
        };
        let key = if snake { key } else { "value" };
        Some(Tree::Map(vec![("action", Tree::str(action)), (key, value)]))
    }

    fn hover(&self, hover: &HoverEvent, depth: usize) -> Option<Tree> {
        let protocol = self.protocol;
        if protocol < v::HOVER_CONTENTS {
            let (action, value) = match hover {
                HoverEvent::ShowText(text) => ("show_text", self.component(text, depth + 1)),
                HoverEvent::ShowItem(item) => {
                    let mut stack = Compound::new();
                    stack.insert("id", item.id.clone());
                    stack.insert(
                        "Count",
                        item.count.clamp(i8::MIN.into(), i8::MAX.into()) as i8,
                    );
                    if protocol < v::FLATTENING {
                        stack.insert("Damage", item.damage.unwrap_or(0));
                    }
                    if let Some(tag) = &item.tag {
                        stack.insert("tag", tag.clone());
                    }
                    let text = snbt::print(protocol, &Nbt::Compound(stack));
                    ("show_item", Tree::Map(vec![("text", Tree::str(text))]))
                }
                HoverEvent::ShowEntity(entity) => {
                    let mut tag = Compound::new();
                    if let Some(name) = &entity.name {
                        let name = if protocol < v::FLATTENING {
                            super::legacy::to_legacy(name)
                        } else {
                            self.component(name, depth + 1).to_json().write()
                        };
                        tag.insert("name", name);
                    }
                    tag.insert("type", entity.kind.clone());
                    tag.insert(
                        "id",
                        match &entity.id {
                            EntityId::Uuid(uuid) => super::format_uuid(*uuid),
                            EntityId::Text(text) => text.clone(),
                        },
                    );
                    let text = snbt::print(protocol, &Nbt::Compound(tag));
                    ("show_entity", Tree::Map(vec![("text", Tree::str(text))]))
                }
            };
            return Some(Tree::Map(vec![
                ("action", Tree::str(action)),
                ("value", value),
            ]));
        }
        let snake = protocol >= v::SNAKE_CASE_EVENTS;
        match hover {
            HoverEvent::ShowText(text) => Some(Tree::Map(vec![
                ("action", Tree::str("show_text")),
                (
                    if snake { "value" } else { "contents" },
                    self.component(text, depth + 1),
                ),
            ])),
            HoverEvent::ShowItem(item) => {
                let mut fields = vec![("id", Tree::str(item.id.clone()))];
                if item.count != 1 || protocol >= v::ITEM_COUNT_REQUIRED {
                    fields.push(("count", Tree::Int(item.count)));
                }
                if protocol < v::ITEM_COMPONENTS {
                    if let Some(tag) = &item.tag {
                        fields.push((
                            "tag",
                            Tree::str(snbt::print(protocol, &Nbt::Compound(tag.clone()))),
                        ));
                    }
                } else if let Some(components) = item
                    .components
                    .as_ref()
                    .filter(|components| !components.is_empty())
                {
                    fields.push(("components", Tree::Nbt(Nbt::Compound(components.clone()))));
                }
                Some(self.typed("show_item", fields, snake))
            }
            HoverEvent::ShowEntity(entity) => {
                let EntityId::Uuid(uuid) = entity.id else {
                    return None;
                };
                let uuid = if self.codec() {
                    Tree::Nbt(Nbt::IntArray(uuid_ints(uuid).to_vec()))
                } else {
                    Tree::str(super::format_uuid(uuid))
                };
                let mut fields = if snake {
                    vec![("id", Tree::str(entity.kind.clone())), ("uuid", uuid)]
                } else {
                    vec![("type", Tree::str(entity.kind.clone())), ("id", uuid)]
                };
                if let Some(name) = &entity.name {
                    fields.push(("name", self.component(name, depth + 1)));
                }
                Some(self.typed("show_entity", fields, snake))
            }
        }
    }

    /// A hover event with its fields inline (1.21.5) or under `contents`.
    fn typed(&self, action: &'static str, fields: Vec<(&'static str, Tree)>, inline: bool) -> Tree {
        let mut entries = vec![("action", Tree::str(action))];
        if inline {
            entries.extend(fields);
        } else {
            entries.push(("contents", Tree::Map(fields)));
        }
        Tree::Map(entries)
    }
}

fn is_default_atlas(atlas: &str) -> bool {
    atlas == "minecraft:blocks" || atlas == "blocks"
}

/// What 1.21.5's `UNTRUSTED_URI` codec accepts: an http or https URL.
fn is_web_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !url
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// A command as 1.21.5's chat-string codec allows it: no section signs or
/// control characters (suggestions may keep line breaks).
fn chat_string(command: &str, keep_newlines: bool) -> String {
    command
        .chars()
        .filter(|character| {
            (*character != '§' && *character >= ' ' && *character != '\u{7f}')
                || (keep_newlines && *character == '\n')
        })
        .collect()
}
