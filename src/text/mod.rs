//! Player-facing text that Rift sends on someone else's behalf: chat
//! messages, action bars, kicks and the reasons of denied logins and
//! transfers. Each is plain text or a JSON text component, so plugins can
//! add colours, line breaks, hover text and click events.
//!
//! The JSON forms are explicit everywhere: a Lua table, a JSON object or
//! list in a bus reply, or a WIT `*-json` function or action. Rift never
//! guesses JSON from a plain string, so a plugin that relays what a player
//! typed cannot be tricked into sending click events.
//!
//! Markup ([`markup`]) and legacy `&` codes ([`legacy`]) are authoring forms,
//! explicit too: [`Text::mini`] and [`Text::legacy`], or an object
//! `{"mini": "<red>Hi <player>", "args": {"player": "Steve"}}` or
//! `{"legacy": "&cHi", "marker": "&"}` wherever a component is accepted.
//! They are parsed into components at the call and never sent as markup.
//!
//! A `{"message": "invited", "args": {...}}` object is a message from a
//! catalogue ([`catalog`]), resolved at the call too: with the caller's
//! namespace and in the recipient's language, which the code that converts
//! the text names with [`rendering`]. Text for several recipients stays
//! [`Unrendered`] until it is rendered for each.
//!
//! One engine renders for every client ([`Profile`], [`Text::for_client`]):
//! event spellings, hex colours and newer fields are rewritten for older
//! clients, and [`Text::to_legacy`] writes `§` codes for string-only surfaces.
use crate::protocol;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};
use std::{borrow::Cow, cell::RefCell, fmt, io};

pub mod catalog;
mod client;
pub mod color;
pub mod legacy;
pub(crate) mod lua;
pub mod markup;
pub mod messages;
pub mod schema;

pub use markup::{Arg, Args, Styles};

pub use client::Profile;
pub(crate) use client::{for_client, strip_objects};

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
    /// most 32 KiB that a client reads (see [`schema::check`]) and that also
    /// has a network NBT form.
    ///
    /// Authoring objects (`{"mini": ...}`, `{"legacy": ...}`,
    /// `{"message": ...}`) anywhere a component may appear are parsed first,
    /// with the theme in effect and messages resolved for the recipient in
    /// scope (see [`rendering`]).
    pub fn component(mut component: Json, label: &str) -> Result<Self, String> {
        if mentions_authoring(&component, 0) {
            let styles = catalog::theme();
            expand(&mut component, &styles, 0).map_err(|error| format!("{label}: {error}"))?;
        }
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
        schema::check(&component).map_err(|error| format!("{label}: {error}"))?;
        protocol::validate_component(&component).map_err(|error| format!("{label}: {error}"))?;
        // Kept in the 1.21.5 spelling; older clients get theirs when sent.
        if let Some(canonical) = client::canonical(&component) {
            component = canonical;
        }
        Ok(Self::Component(component))
    }

    /// Markup with `args` and theme `styles` (see [`markup`]).
    pub fn mini(markup: &str, args: &Args, styles: &Styles, label: &str) -> Result<Self, String> {
        if markup.is_empty() {
            return Err(format!("{label} must not be empty"));
        }
        if markup.len() > MAX_COMPONENT_BYTES {
            return Err(format!("{label} exceeds 32 KiB"));
        }
        args.validate(label)?;
        let component =
            markup::parse(markup, args, styles).map_err(|error| format!("{label}: {error}"))?;
        Self::component(component, label)
    }

    /// Legacy formatting codes with `marker`, usually `&` (see [`legacy`]).
    pub fn legacy(text: &str, marker: char, label: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Err(format!("{label} must not be empty"));
        }
        if text.len() > MAX_COMPONENT_BYTES {
            return Err(format!("{label} exceeds 32 KiB"));
        }
        if marker.is_alphanumeric() || marker.is_whitespace() {
            return Err(format!("{label}: the marker must be a symbol such as '&'"));
        }
        Self::component(legacy::parse(text, marker), label)
    }

    /// The text with `§` codes, for surfaces that only take strings, such
    /// as server-list hover lines. Plain text is returned as written.
    pub fn to_legacy(&self, profile: Profile) -> String {
        match self {
            Self::Plain(text) => text.clone(),
            Self::Component(component) => legacy::serialize(
                &self::for_client(component, profile).unwrap_or_else(|| component.clone()),
            ),
        }
    }

    /// The component `profile` reads (see [`Profile`]). Plain text is a JSON
    /// string, which every client shows as written.
    pub fn for_client(&self, profile: Profile) -> Cow<'_, Json> {
        match self {
            Self::Plain(text) => Cow::Owned(Json::String(text.clone())),
            Self::Component(component) => match self::for_client(component, profile) {
                Some(converted) => Cow::Owned(converted),
                None => Cow::Borrowed(component),
            },
        }
    }

    /// A JSON text component a backend sent, such as a login disconnect: kept
    /// with its formatting when it is valid, otherwise shown as its words or
    /// as written.
    pub fn from_backend_json(raw: &str) -> Self {
        match serde_json::from_str::<Json>(raw) {
            Ok(component) => {
                Self::from_backend(component).unwrap_or_else(|| Self::Plain(raw.into()))
            }
            Err(_) => Self::Plain(raw.to_owned()),
        }
    }

    /// A text component a backend sent, kept with its formatting when it is
    /// valid, otherwise as its words; `None` when it has no words.
    pub fn from_backend(component: Json) -> Option<Self> {
        let mut words = String::new();
        flatten(&component, &mut words, 0);
        if let Ok(text) = Self::component(component, "backend text") {
            return Some(text);
        }
        (!words.is_empty()).then_some(Self::Plain(words))
    }

    /// The text of a WIT `*-json` argument: a JSON text component, like a
    /// tab list header. Text that does not start with `{`, `[` or `"` is
    /// shown as written.
    pub fn json(text: &str, label: &str) -> Result<Self, String> {
        match Self::json_as_written(text, label)? {
            Self::Component(component) => Self::component(component, label),
            plain => Ok(plain),
        }
    }

    /// [`Text::json`] parsed but not converted: authoring objects stay as
    /// written, for [`Unrendered`].
    pub fn json_as_written(text: &str, label: &str) -> Result<Self, String> {
        if text.len() > MAX_COMPONENT_BYTES {
            return Err(format!("{label} exceeds 32 KiB"));
        }
        if !text.starts_with(['{', '[', '"']) {
            return Ok(Self::Component(Json::String(text.to_owned())));
        }
        serde_json::from_str(text)
            .map(Self::Component)
            .map_err(|error| format!("{label}: invalid JSON: {error}"))
    }

    /// A string (or a number) is plain text; a table is a chat component,
    /// as in `rift.server_list` and `players.tab_list`.
    pub fn lua(value: mlua::Value, label: &str) -> Result<Self, String> {
        match Self::lua_as_written(value, label)? {
            Self::Component(component) => Self::component(component, label),
            plain => Ok(plain),
        }
    }

    /// [`Text::lua`] with a table read but not converted: authoring objects
    /// stay as written, for [`Unrendered`]. Plain text is checked.
    pub fn lua_as_written(value: mlua::Value, label: &str) -> Result<Self, String> {
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
            value @ mlua::Value::Table(_) => Ok(Self::Component(
                crate::server_list::chat_component(value, label)?,
            )),
            value => Err(format!(
                "{label} must be a string or a chat component table, got {}",
                value.type_name()
            )),
        }
    }

    /// Check a value that arrived already built, such as a bus reply.
    pub fn validate(&self, label: &str) -> Result<(), String> {
        self.checked(label).map(drop)
    }

    /// A value that arrived already built, checked and with its authoring
    /// objects converted (see [`Text::component`]).
    pub fn checked(&self, label: &str) -> Result<Self, String> {
        match self {
            Self::Plain(text) => Self::plain(text.as_str(), label),
            Self::Component(component) => Self::component(component.clone(), label),
        }
    }

    /// Markup that writes this text; plain text is escaped.
    pub fn to_mini(&self) -> String {
        match self {
            Self::Plain(text) => markup::escape(text),
            Self::Component(component) => markup::serialize(component),
        }
    }

    /// This text written in `form`, as the `text` interface and `rift.text`
    /// return it.
    pub fn written(&self, form: Form) -> String {
        match form {
            Form::Json => self.for_client(Profile::UNKNOWN).to_string(),
            Form::Plain => self.to_string(),
            Form::Legacy => self.to_legacy(Profile::UNKNOWN),
            Form::Mini => self.to_mini(),
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

/// Text a call addresses to several players, such as a broadcast, kept as
/// written until it is rendered for each recipient: a `message` object in it
/// is resolved in each one's language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unrendered {
    /// Rendered in the default language when it was made, which checks the
    /// call; what every recipient sees when the text names no message.
    default: Text,
    /// The component as written and the caller's namespace, when it names a
    /// message.
    written: Option<(Json, Option<String>)>,
    label: String,
}

impl Unrendered {
    /// `text` as written ([`Text::json_as_written`], [`Text::lua_as_written`]
    /// or plain text) from a caller in `namespace`. Fails when the text does
    /// not render in the default language, so a bad call fails even when
    /// nobody would see it.
    pub fn new(text: Text, namespace: Option<&str>, label: &str) -> Result<Self, String> {
        let default = rendering(namespace, None, || text.checked(label))?;
        let written = match text {
            Text::Component(component) if mentions(&component, &["message"], 0) => {
                Some((component, namespace.map(str::to_owned)))
            }
            _ => None,
        };
        Ok(Self {
            default,
            written,
            label: label.to_owned(),
        })
    }

    /// Whether the text depends on the recipient's language.
    pub fn varies(&self) -> bool {
        self.written.is_some()
    }

    /// The text for a recipient in `language` (`None` for the default).
    pub fn render(&self, language: Option<&str>) -> Result<Text, String> {
        match (&self.written, language) {
            (Some((component, namespace)), Some(_)) => {
                rendering(namespace.as_deref(), language, || {
                    Text::component(component.clone(), &self.label)
                })
            }
            _ => Ok(self.default.clone()),
        }
    }
}

/// Whose messages and which language `message` objects resolve with.
#[derive(Debug, Clone)]
struct Recipient {
    namespace: Option<String>,
    language: Option<String>,
}

thread_local! {
    /// The innermost [`render_scope`] on this thread.
    static RENDERING: RefCell<Option<Recipient>> = const { RefCell::new(None) };
}

/// Restores the enclosing scope when dropped (see [`render_scope`]).
pub struct RenderScope(Option<Recipient>);

impl Drop for RenderScope {
    fn drop(&mut self) {
        let previous = self.0.take();
        RENDERING.with(|rendering| *rendering.borrow_mut() = previous);
    }
}

/// Until the guard drops, resolve `message` objects in text converted on
/// this thread for a caller in `namespace` (a plugin's folder name, whose
/// unqualified keys are its own; `None` for `rift.lua`, script-mode plugins
/// and bus callers) and a recipient in `language` (a normalised tag such as
/// `de_at`; `None` for the default). A thread-local: never hold the guard
/// across an `.await`, and prefer [`rendering`].
#[must_use = "the scope ends when the guard drops"]
pub fn render_scope(namespace: Option<&str>, language: Option<&str>) -> RenderScope {
    let recipient = Recipient {
        namespace: namespace.map(str::to_owned),
        language: language.map(str::to_owned),
    };
    RenderScope(RENDERING.with(|rendering| rendering.borrow_mut().replace(recipient)))
}

/// Run `convert` inside a [`render_scope`] for `namespace` and `language`.
pub fn rendering<R>(
    namespace: Option<&str>,
    language: Option<&str>,
    convert: impl FnOnce() -> R,
) -> R {
    let _scope = render_scope(namespace, language);
    convert()
}

/// What the `text` interface (`render`, `plain`, `to-legacy`, `to-mini`)
/// and the matching `rift.text` functions write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// The component as compact JSON; plain text is a JSON string.
    Json,
    /// The words without formatting.
    Plain,
    /// `§` codes.
    Legacy,
    /// Markup.
    Mini,
}

/// `text` as written ([`Text::json_as_written`], [`Text::lua_as_written`]
/// or plain text), checked and with markup, legacy codes and messages
/// resolved for a caller in `namespace` (see [`rendering`]), in `language`
/// (a tag as a plugin writes it, such as `de-DE`; `None` for the default).
/// The same for every engine, at any time: it does not depend on the scope
/// a hook runs in.
pub fn resolve(
    text: &Text,
    namespace: Option<&str>,
    language: Option<&str>,
    label: &str,
) -> Result<Text, String> {
    let language = language
        .map(|tag| crate::directory::language_tag(tag).map_err(|error| format!("{label}: {error}")))
        .transpose()?;
    rendering(namespace, language.as_deref(), || text.checked(label))
}

/// [`resolve`], written in `form`.
pub fn convert(
    text: &Text,
    namespace: Option<&str>,
    language: Option<&str>,
    form: Form,
    label: &str,
) -> Result<String, String> {
    resolve(text, namespace, language, label).map(|text| text.written(form))
}

/// The caller's namespace and the recipient's language: the innermost
/// scope's, else no namespace and the language of the player whose session
/// runs on this task (see [`catalog::language`]).
fn recipient() -> (Option<String>, Option<String>) {
    match RENDERING.with(|rendering| rendering.borrow().clone()) {
        Some(Recipient {
            namespace,
            language,
        }) => (namespace, language),
        None => (None, catalog::language()),
    }
}

/// Whether an authoring object (`mini`, `legacy` or `message`) appears
/// anywhere.
fn mentions_authoring(value: &Json, depth: usize) -> bool {
    mentions(value, &["mini", "legacy", "message"], depth)
}

/// Whether an object with one of `keys` appears anywhere in `value`.
fn mentions(value: &Json, keys: &[&str], depth: usize) -> bool {
    depth <= 128
        && match value {
            Json::Array(items) => items.iter().any(|item| mentions(item, keys, depth + 1)),
            Json::Object(fields) => {
                keys.iter().any(|key| fields.contains_key(*key))
                    || fields.values().any(|item| mentions(item, keys, depth + 1))
            }
            _ => false,
        }
}

/// Replace authoring objects at component positions with their components.
fn expand(value: &mut Json, styles: &Styles, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("text nests too deeply".into());
    }
    match value {
        Json::Array(items) => items
            .iter_mut()
            .try_for_each(|item| expand(item, styles, depth + 1)),
        Json::Object(fields) => {
            if let Some(component) = authoring(fields, styles, depth)? {
                *value = component;
                return Ok(());
            }
            for key in ["extra", "with", "separator"] {
                if let Some(child) = fields.get_mut(key) {
                    expand(child, styles, depth + 1)?;
                }
            }
            for key in ["hover_event", "hoverEvent"] {
                let Some(Json::Object(event)) = fields.get_mut(key) else {
                    continue;
                };
                for text in ["value", "contents", "name"] {
                    if let Some(child) = event.get_mut(text) {
                        expand(child, styles, depth + 1)?;
                    }
                }
                if let Some(Json::Object(contents)) = event.get_mut("contents")
                    && let Some(name) = contents.get_mut("name")
                {
                    expand(name, styles, depth + 1)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The component of `{"mini": markup, "args": {...}}`,
/// `{"legacy": text, "marker": "&"}` or `{"message": key, "args": {...}}`,
/// or `None` for any other object.
fn authoring(
    fields: &Map<String, Json>,
    styles: &Styles,
    depth: usize,
) -> Result<Option<Json>, String> {
    let only = |allowed: &[&str], form: &str| match fields
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
    {
        Some(key) => Err(format!(
            "a {form} object takes only {}, not {key:?}",
            allowed.join(" and ")
        )),
        None => Ok(()),
    };
    if let Some(markup) = fields.get("mini") {
        only(&["mini", "args"], "mini")?;
        let markup = markup.as_str().ok_or("mini must be a string of markup")?;
        if markup.is_empty() {
            return Err("mini must not be empty".into());
        }
        let args = arguments(fields.get("args"), styles, depth)?;
        args.validate("args")?;
        return markup::parse(markup, &args, styles).map(Some);
    }
    if let Some(text) = fields.get("legacy") {
        only(&["legacy", "marker"], "legacy")?;
        let text = text.as_str().ok_or("legacy must be a string")?;
        if text.is_empty() {
            return Err("legacy must not be empty".into());
        }
        let marker = match fields.get("marker") {
            None => '&',
            Some(Json::String(marker)) => {
                let mut chars = marker.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if !c.is_alphanumeric() && !c.is_whitespace() => c,
                    _ => return Err("marker must be one symbol such as \"&\"".into()),
                }
            }
            Some(_) => return Err("marker must be one symbol such as \"&\"".into()),
        };
        return Ok(Some(legacy::parse(text, marker)));
    }
    if let Some(key) = fields.get("message") {
        only(&["message", "args"], "message")?;
        let key = key
            .as_str()
            .ok_or("message must be a string: the key of a message")?;
        if key.is_empty() {
            return Err("message must not be empty".into());
        }
        // The catalogue checks the names against the message's.
        let args = arguments(fields.get("args"), styles, depth)?;
        let (namespace, language) = recipient();
        return catalog::active()
            .message(namespace.as_deref(), key, &args, language.as_deref())
            .map(Some);
    }
    Ok(None)
}

/// The `args` of a `mini` or `message` object: strings, numbers and
/// booleans shown as written, components (expanded here) and style
/// arguments `{"style": "<markup>"}`.
fn arguments(value: Option<&Json>, styles: &Styles, depth: usize) -> Result<Args, String> {
    let mut args = Args::new();
    let values = match value {
        None => return Ok(args),
        Some(Json::Object(values)) => values,
        Some(_) => {
            return Err("args must be an object of names to text, components or styles".into());
        }
    };
    for (name, value) in values {
        let arg = match value {
            Json::String(text) => Arg::Text(text.clone()),
            Json::Number(number) => Arg::Text(number.to_string()),
            Json::Bool(value) => Arg::Text(value.to_string()),
            // `{"style": "<red>"}`: no component has a `style` key.
            Json::Object(object) if object.contains_key("style") => {
                if object.len() != 1 {
                    return Err(format!("args.{name}: a style argument takes only style"));
                }
                let source = object["style"]
                    .as_str()
                    .ok_or_else(|| format!("args.{name}.style must be a string of markup"))?;
                let style = markup::style_arg(source)
                    .map_err(|error| format!("args.{name}.style: {error}"))?;
                Arg::Style(style)
            }
            Json::Object(_) | Json::Array(_) => {
                let mut component = value.clone();
                expand(&mut component, styles, depth + 1)?;
                Arg::Component(component)
            }
            Json::Null => return Err(format!("args.{name} must not be null")),
        };
        args.insert(name, arg);
    }
    Ok(args)
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

/// Append the visible words of one component object, without its `extra`.
pub(crate) fn flatten_own(component: &Json, out: &mut String) {
    match component {
        Json::Object(fields) => {
            let mut own = fields.clone();
            own.remove("extra");
            flatten(&Json::Object(own), out, 0);
        }
        other => flatten(other, out, 0),
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

/// An error of `kind` whose player-facing text is `text`; [`reason`] shows
/// it with its formatting and `to_string` gives its words.
pub fn error(kind: io::ErrorKind, text: Text) -> io::Error {
    io::Error::new(kind, Denied(text))
}

/// An error of `kind` showing Rift's message `notice`.
pub fn notice_error(kind: io::ErrorKind, notice: catalog::Notice) -> io::Error {
    error(kind, catalog::notice(notice))
}

/// A `PermissionDenied` error carrying `reason`.
pub fn denied(reason: Text) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, Denied(reason))
}

/// The text `error` was made to show the player, if it was made with one.
pub fn shown(error: &io::Error) -> Option<Text> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Denied>())
        .map(|denied| denied.0.clone())
}

/// What to show the player for `error`: a hook's reason as it was given,
/// otherwise the error's message.
pub fn reason(error: &io::Error) -> Text {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<Denied>())
        .map_or_else(|| Text::Plain(error.to_string()), |denied| denied.0.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ProtocolVersion;
    use serde_json::json;

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
    fn markup_and_legacy_build_checked_components() {
        let styles = Styles::default();
        assert_eq!(
            Text::mini(
                "<red>Hi <player>",
                &Args::new().text("player", "<b>x"),
                &styles,
                "message"
            )
            .unwrap(),
            Text::Component(json!({"text": "Hi <b>x", "color": "red"}))
        );
        assert_eq!(
            Text::mini("", &Args::new(), &styles, "message").unwrap_err(),
            "message must not be empty"
        );
        assert_eq!(
            Text::mini("<bolt>", &Args::new(), &styles, "message").unwrap_err(),
            "message: <bolt> at byte 0: unknown tag; not built in, an argument or a style"
        );
        assert!(
            Text::mini(
                "<click:open_url:'javascript:x'>a",
                &Args::new(),
                &styles,
                "message"
            )
            .unwrap_err()
            .contains("http or https")
        );
        assert_eq!(
            Text::legacy("&aGo", '&', "message").unwrap(),
            Text::Component(json!({"text": "Go", "color": "green"}))
        );
        assert!(Text::legacy("&aGo", 'a', "message").is_err());
    }

    #[test]
    fn authoring_objects_expand_wherever_components_go() {
        assert_eq!(
            Text::component(
                json!({"mini": "<red>Hi <name>", "args": {"name": "<b>"}}),
                "m"
            )
            .unwrap(),
            Text::Component(json!({"text": "Hi <b>", "color": "red"}))
        );
        assert_eq!(
            Text::component(
                json!({"text": "a", "extra": [{"legacy": "&cb"}],
                       "hover_event": {"action": "show_text", "value": {"mini": "<green>tip"}}}),
                "m"
            )
            .unwrap(),
            Text::Component(
                json!({"text": "a", "extra": [{"text": "b", "color": "red"}],
                "hover_event": {"action": "show_text", "value": {"text": "tip", "color": "green"}}})
            )
        );
        assert_eq!(
            Text::component(
                json!({"mini": "[<rank>]", "args": {"rank": {"mini": "<gold>Admin"}}}),
                "m"
            )
            .unwrap(),
            Text::Component(json!({"text": "", "extra": [
                {"text": "["}, {"text": "Admin", "color": "gold"}, {"text": "]"},
            ]}))
        );
        assert_eq!(
            Text::component(
                json!({"mini": "<team>[<tag>]</team>",
                       "args": {"team": {"style": "<#55ffff><bold>"}, "tag": "x"}}),
                "m"
            )
            .unwrap(),
            Text::Component(json!({"text": "[x]", "color": "#55ffff", "bold": true}))
        );
        assert_eq!(
            Text::component(
                json!({"mini": "<team>x", "args": {"team": {"style": "<red>", "color": "red"}}}),
                "m"
            )
            .unwrap_err(),
            "m: args.team: a style argument takes only style"
        );
        assert_eq!(
            Text::component(
                json!({"mini": "<team>x", "args": {"team": {"style": 1}}}),
                "m"
            )
            .unwrap_err(),
            "m: args.team.style must be a string of markup"
        );
        assert_eq!(
            Text::component(
                json!({"mini": "<team>x", "args": {"team": {"style": "<red>words"}}}),
                "m"
            )
            .unwrap_err(),
            "m: args.team.style: only formatting tags are allowed, found text \"words\""
        );
        assert_eq!(
            Text::component(json!({"mini": "x", "color": "red"}), "m").unwrap_err(),
            "m: a mini object takes only mini and args, not \"color\""
        );
        assert_eq!(
            Text::component(json!({"mini": "x", "args": {"red": "y"}}), "m").unwrap_err(),
            "m: args: <red> is a built-in tag"
        );
        assert_eq!(
            Text::component(json!({"legacy": "&cx", "marker": "ab"}), "m").unwrap_err(),
            "m: marker must be one symbol such as \"&\""
        );
        // A bare string is never markup.
        assert_eq!(
            Text::component(json!("<red>x"), "m").unwrap(),
            Text::Component(json!("<red>x"))
        );
        assert_eq!(
            Text::plain("<red>x", "m").unwrap(),
            Text::Plain("<red>x".into())
        );
    }

    /// Run `convert` with the test catalogue loading on this thread.
    fn with_messages<R>(convert: impl FnOnce() -> R) -> R {
        let _catalog = catalog::loading(std::sync::Arc::new(catalog::test_catalog()));
        convert()
    }

    #[test]
    fn message_objects_render_at_the_call() {
        with_messages(|| {
            let words = |value: Json| Text::component(value, "m").unwrap().to_string();
            assert_eq!(
                Text::component(json!({"message": "party:created"}), "m").unwrap(),
                Text::Component(json!({"text": "Created."}))
            );
            // A key without a namespace is the caller's own.
            assert_eq!(
                rendering(Some("party"), None, || words(json!({"message": "created"}))),
                "Created."
            );
            // Arguments take the forms of `mini` arguments.
            for (name, expected) in [
                (json!("Steve"), "Hello Steve."),
                (json!(3), "Hello 3."),
                (json!({"mini": "<gold>Steve"}), "Hello Steve."),
                (json!({"text": "Steve", "color": "red"}), "Hello Steve."),
            ] {
                assert_eq!(
                    words(json!({"message": "party:greet", "args": {"name": name}})),
                    expected
                );
            }
            assert_eq!(
                words(json!({"message": "party:tagged",
                             "args": {"team": {"style": "<red>"}, "tag": "x"}})),
                "[x]"
            );
            // It nests like `mini`: in `extra`, in hover text, as an argument.
            assert_eq!(
                Text::component(
                    json!({"text": "a", "extra": [{"message": "party:created"}],
                           "hover_event": {"action": "show_text",
                                           "value": {"message": "party:created"}}}),
                    "m"
                )
                .unwrap(),
                Text::Component(json!({"text": "a", "extra": [{"text": "Created."}],
                    "hover_event": {"action": "show_text", "value": {"text": "Created."}}}))
            );
            assert_eq!(
                words(json!({"mini": "[<x>]", "args": {"x": {"message": "party:created"}}})),
                "[Created.]"
            );
            assert_eq!(
                words(json!({"message": "party:greet",
                             "args": {"name": {"message": "party:created"}}})),
                "Hello Created.."
            );
        });
    }

    #[test]
    fn message_objects_fail_like_the_other_forms() {
        with_messages(|| {
            let error = |value: Json| Text::component(value, "m").unwrap_err();
            assert_eq!(
                error(json!({"message": "party:created", "color": "red"})),
                "m: a message object takes only message and args, not \"color\""
            );
            assert_eq!(
                error(json!({"message": 1})),
                "m: message must be a string: the key of a message"
            );
            assert_eq!(
                error(json!({"message": ""})),
                "m: message must not be empty"
            );
            assert_eq!(
                error(json!({"message": "party:greet", "args": []})),
                "m: args must be an object of names to text, components or styles"
            );
            assert_eq!(
                error(json!({"message": "party:greet"})),
                "m: message \"party:greet\": argument <name> is missing"
            );
            assert_eq!(
                error(json!({"message": "party:greet", "args": {"name": "x", "to": "y"}})),
                "m: message \"party:greet\": the message has no argument <to>"
            );
            assert!(
                error(json!({"message": "party:tagged", "args": {"team": "x", "tag": "y"}}))
                    .contains("<team> is a style argument")
            );
            assert!(error(json!({"message": "party:nope"})).contains("no such message"));
            // Without a caller's namespace a key must name one.
            let unqualified = error(json!({"message": "created"}));
            assert!(unqualified.contains("namespace:key"), "{unqualified}");
            // The forms do not mix.
            assert_eq!(
                error(json!({"mini": "x", "message": "party:created"})),
                "m: a mini object takes only mini and args, not \"message\""
            );
        });
    }

    #[test]
    fn the_recipient_in_scope_decides_the_language() {
        with_messages(|| {
            let created = || {
                Text::component(json!({"message": "party:created"}), "m")
                    .unwrap()
                    .to_string()
            };
            // No scope and no session: the default language.
            assert_eq!(created(), "Created.");
            rendering(Some("party"), Some("de_at"), || {
                assert_eq!(created(), "Erstellt.");
                // An inner scope wins, and the outer one is back after it.
                assert_eq!(rendering(None, None, created), "Created.");
                assert_eq!(created(), "Erstellt.");
                let scope = render_scope(Some("party"), Some("fr"));
                assert_eq!(created(), "Created.");
                drop(scope);
                assert_eq!(created(), "Erstellt.");
            });
            assert_eq!(created(), "Created.");
            // A scope survives a failed conversion.
            assert!(
                rendering(Some("party"), Some("de"), || {
                    Text::component(json!({"message": "nope"}), "m")
                })
                .is_err()
            );
            assert_eq!(created(), "Created.");
        });
    }

    #[tokio::test]
    async fn without_a_scope_the_sessions_language_applies() {
        let _catalog = catalog::loading(std::sync::Arc::new(catalog::test_catalog()));
        let created = || {
            Text::component(json!({"message": "party:created"}), "m")
                .unwrap()
                .to_string()
        };
        let words = catalog::session(async {
            catalog::set_language(Some("de_de"));
            (created(), rendering(None, None, created))
        })
        .await;
        assert_eq!(words, ("Erstellt.".to_owned(), "Created.".to_owned()));
    }

    #[test]
    fn unrendered_text_renders_once_per_language() {
        with_messages(|| {
            let message = Unrendered::new(
                Text::Component(json!({"message": "created"})),
                Some("party"),
                "message",
            )
            .unwrap();
            assert!(message.varies());
            let words = |language| message.render(language).unwrap().to_string();
            assert_eq!(words(None), "Created.");
            assert_eq!(words(Some("de")), "Erstellt.");
            assert_eq!(words(Some("fr_fr")), "Created.");
            // Text without a message is rendered once, when it is made.
            let plain = Unrendered::new(
                Text::Component(json!({"mini": "<red>Hi"})),
                Some("party"),
                "message",
            )
            .unwrap();
            assert!(!plain.varies());
            assert_eq!(
                plain.render(Some("de")).unwrap(),
                Text::Component(json!({"text": "Hi", "color": "red"}))
            );
            // A bad call fails when it is made, whoever would see it.
            assert!(
                Unrendered::new(
                    Text::Component(json!({"message": "created"})),
                    None,
                    "message"
                )
                .is_err()
            );
            assert_eq!(
                Unrendered::new(Text::Plain(String::new()), None, "message").unwrap_err(),
                "message must contain 1..=1024 bytes"
            );
        });
    }

    #[test]
    fn backend_json_keeps_its_formatting() {
        assert_eq!(
            Text::from_backend_json(r#"{"text":"Banned","color":"red"}"#),
            Text::Component(json!({"text": "Banned", "color": "red"}))
        );
        assert_eq!(
            Text::from_backend_json(r#"{"text":"Banned","color":"orange"}"#),
            Text::Plain("Banned".into())
        );
        assert_eq!(
            Text::from_backend_json("not json"),
            Text::Plain("not json".into())
        );
        let error = notice_error(io::ErrorKind::WouldBlock, catalog::Notice::SwitchBusy);
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            reason(&error),
            Text::Plain("The current server is busy. Please retry the switch.".into())
        );
    }

    #[test]
    fn rendering_follows_the_client() {
        let text =
            Text::Component(json!({"text": "x", "color": "#ff8800", "shadow_color": -16777216}));
        let old = Profile::of(ProtocolVersion::new(47).unwrap());
        let new = Profile::of(ProtocolVersion::new(770).unwrap());
        assert_eq!(
            text.for_client(old).into_owned(),
            json!({"text": "x", "color": "gold"})
        );
        assert_eq!(
            text.for_client(new).into_owned(),
            json!({"text": "x", "color": "#ff8800", "shadow_color": -16777216})
        );
        assert!(matches!(text.for_client(new), Cow::Borrowed(_)));
        assert_eq!(text.to_legacy(new), "§6x");
        assert_eq!(Text::Plain("§cx".into()).to_legacy(old), "§cx");
        assert_eq!(
            Text::Plain("x".into()).for_client(old).into_owned(),
            json!("x")
        );
    }

    #[test]
    fn the_text_interface_resolves_then_writes_each_form() {
        with_messages(|| {
            let convert = |text: Json, language: Option<&str>, form: Form| {
                super::convert(
                    &Text::Component(text),
                    Some("party"),
                    language,
                    form,
                    "text.render",
                )
            };
            // Markup, legacy codes and messages, in each form.
            let red = json!({"mini": "<red>Hi"});
            let rendered = convert(red.clone(), None, Form::Json).unwrap();
            assert_eq!(
                serde_json::from_str::<Json>(&rendered).unwrap(),
                json!({"text": "Hi", "color": "red"})
            );
            assert_eq!(convert(red.clone(), None, Form::Plain).unwrap(), "Hi");
            assert_eq!(convert(red.clone(), None, Form::Legacy).unwrap(), "§cHi");
            assert_eq!(convert(red, None, Form::Mini).unwrap(), "<red>Hi</red>");
            assert_eq!(
                convert(json!({"legacy": "&cRed &lbold"}), None, Form::Legacy).unwrap(),
                "§cRed §lbold"
            );
            // The caller's own message by unqualified key, in a language
            // written either way, or the default.
            let greet = json!({"message": "greet", "args": {"name": "Alex"}});
            assert_eq!(
                convert(greet.clone(), None, Form::Plain).unwrap(),
                "Hello Alex."
            );
            assert_eq!(
                convert(greet.clone(), Some("de"), Form::Plain).unwrap(),
                "Hallo Alex."
            );
            assert_eq!(
                convert(greet.clone(), Some("DE"), Form::Plain).unwrap(),
                "Hallo Alex."
            );
            assert_eq!(
                convert(greet.clone(), Some("fr-FR"), Form::Plain).unwrap(),
                "Hello Alex."
            );
            let rendered = convert(greet.clone(), Some("de"), Form::Json).unwrap();
            assert_eq!(
                Text::Component(serde_json::from_str(&rendered).unwrap()).to_string(),
                "Hallo Alex."
            );
            // Another plugin's message by qualified key; none without a
            // namespace for an unqualified one.
            assert_eq!(
                super::convert(
                    &Text::Component(json!({"message": "party:created"})),
                    None,
                    Some("de"),
                    Form::Plain,
                    "rift.text.plain",
                )
                .unwrap(),
                "Erstellt."
            );
            assert!(
                super::convert(
                    &Text::Component(json!({"message": "created"})),
                    None,
                    None,
                    Form::Plain,
                    "rift.text.plain",
                )
                .is_err()
            );
            // An invalid language, key or argument is an error that names
            // the call.
            let error = convert(greet.clone(), Some("d"), Form::Plain).unwrap_err();
            assert!(error.starts_with("text.render: a language is"), "{error}");
            let error = convert(json!({"message": "nope"}), None, Form::Json).unwrap_err();
            assert!(error.starts_with("text.render"), "{error}");
            let error = convert(json!({"message": "greet"}), None, Form::Json).unwrap_err();
            assert!(error.starts_with("text.render"), "{error}");
            // The scope a hook runs in does not leak in.
            assert_eq!(
                rendering(Some("t"), Some("de"), || convert(
                    greet.clone(),
                    None,
                    Form::Plain
                ))
                .unwrap(),
                "Hello Alex."
            );
        });
        // Plain text is shown as written: a JSON string, escaped markup.
        let plain = Text::Plain("<red> \"\\".into());
        let write = |form| super::convert(&plain, None, None, form, "text.render").unwrap();
        assert_eq!(write(Form::Json), r#""<red> \"\\""#);
        assert_eq!(write(Form::Plain), "<red> \"\\");
        assert_eq!(write(Form::Legacy), "<red> \"\\");
        assert_eq!(write(Form::Mini), r#"\<red> "\\"#);
        assert!(super::convert(&Text::Plain(String::new()), None, None, Form::Plain, "p").is_err());
    }
}
