//! Dialogs that plugins show to players (Minecraft 1.21.6 and newer).
//!
//! A plugin describes a dialog in the vanilla JSON format (the one data packs
//! use for `data/<namespace>/dialog/*.json`). Rift validates it against the
//! subset every supported client decodes identically, so a mistake fails the
//! plugin's call instead of disconnecting the player, and rewrites the
//! buttons that should come back to the plugin:
//!
//! - A `custom` or `dynamic/custom` action whose `id` has no namespace
//!   (`"submit"`) is the plugin's own. Rift sends it as
//!   `rift:dialog/<serial>/<index>` and keeps its `payload` or `additions`,
//!   so the client never sees or changes them. A click comes back to the
//!   plugin that showed the dialog with the action name, the player's input
//!   values (for `dynamic/custom`) and the kept payload.
//! - A namespaced `id` (`"myserver:vote"`) is sent as written and its clicks
//!   reach the backend, as if the backend had shown the dialog. The `rift`
//!   namespace is reserved.
//!
//! Only inline dialogs are accepted: a registry reference that the client
//! does not know would disconnect it. Item bodies are not accepted for the
//! same reason (an item one client version lacks fails to decode).
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// The largest dialog definition, as compact JSON.
pub const MAX_DIALOG_BYTES: usize = 64 * 1024;
/// Plugin actions per dialog, including nested dialogs.
const MAX_CALLBACKS: usize = 64;
/// Inputs per dialog.
const MAX_INPUTS: usize = 32;
/// Buttons per list.
const MAX_BUTTONS: usize = 64;
/// Levels of dialogs inside `dialog_list` and `show_dialog`.
const MAX_NESTING: usize = 4;
const MAX_STRING: usize = 4096;
/// Nesting and value limits for dialog tables built in Lua.
const MAX_LUA_DEPTH: usize = 32;
const MAX_LUA_VALUES: usize = 8192;

/// The prefix of the click identifiers Rift gives plugin actions.
pub(crate) const CLICK_PREFIX: &str = "rift:dialog/";

/// Who showed a dialog, and therefore receives its clicks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// The configuration script (`rift.on("dialog", ...)`).
    Script,
    /// A persistent plugin, by name.
    Plugin(String),
}

/// The kind of an input, for typing the values a click reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Boolean,
    Number,
    Option,
}

/// One plugin action of a dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Callback {
    /// The action name the plugin wrote.
    id: String,
    /// The `payload` (`custom`) or `additions` (`dynamic/custom`) Rift kept.
    payload: Option<Value>,
    /// Whether the client sends the input values (`dynamic/custom`).
    dynamic: bool,
    /// Whether a click closes the dialog (its `after_action` is `close`).
    closes: bool,
    /// The inputs of the dialog the button belongs to.
    inputs: Vec<(String, Kind)>,
}

/// A validated dialog, ready to send to any 1.21.6+ client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    serial: u64,
    wire: Value,
    callbacks: Vec<Callback>,
}

/// A click on a plugin action, as delivered to the plugin.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Response {
    /// The action name the plugin wrote (`"submit"`).
    pub action: String,
    /// The player's input values by key: strings for text and single-option
    /// inputs, booleans and numbers. Empty for a `custom` action. Values come
    /// from the client: check them like any player input.
    pub values: Map<String, Value>,
    /// The action's `payload` or `additions`, exactly as the plugin wrote it.
    pub payload: Option<Value>,
}

impl Dialog {
    /// Validate a dialog definition and prepare it for sending.
    pub fn parse(spec: &Value) -> Result<Self, String> {
        let size = serde_json::to_vec(spec)
            .map_err(|error| error.to_string())?
            .len();
        if size > MAX_DIALOG_BYTES {
            return Err(format!("dialog exceeds {} KiB", MAX_DIALOG_BYTES / 1024));
        }
        static SERIAL: AtomicU64 = AtomicU64::new(1);
        let mut builder = Builder {
            serial: SERIAL.fetch_add(1, Ordering::Relaxed),
            callbacks: Vec::new(),
        };
        let wire = builder.dialog(spec, "dialog", 0)?;
        Ok(Self {
            serial: builder.serial,
            wire,
            callbacks: builder.callbacks,
        })
    }

    /// Validate a dialog given as JSON text.
    pub fn parse_json(text: &str) -> Result<Self, String> {
        if text.len() > MAX_DIALOG_BYTES {
            return Err(format!("dialog exceeds {} KiB", MAX_DIALOG_BYTES / 1024));
        }
        let spec: Value =
            serde_json::from_str(text).map_err(|error| format!("dialog: invalid JSON: {error}"))?;
        Self::parse(&spec)
    }

    /// The dialog as the client receives it.
    pub fn wire(&self) -> &Value {
        &self.wire
    }

    /// Match a click on this dialog. `None` for clicks on another dialog or
    /// on an action this dialog does not have. The flag says whether the
    /// click closed the dialog.
    pub fn resolve(&self, id: &str, payload: Option<Value>) -> Option<(Response, bool)> {
        let (serial, index) = id.strip_prefix(CLICK_PREFIX)?.split_once('/')?;
        if serial.parse::<u64>().ok()? != self.serial {
            return None;
        }
        let callback = self.callbacks.get(index.parse::<usize>().ok()?)?;
        let mut values = Map::new();
        if callback.dynamic
            && let Some(Value::Object(mut sent)) = payload
        {
            for (key, kind) in &callback.inputs {
                let typed = match (kind, sent.remove(key)) {
                    (Kind::Text | Kind::Option, Some(Value::String(text))) => Value::String(text),
                    (Kind::Boolean, Some(Value::Number(byte))) if byte.as_i64() == Some(1) => {
                        Value::Bool(true)
                    }
                    (Kind::Boolean, Some(Value::Number(byte))) if byte.as_i64() == Some(0) => {
                        Value::Bool(false)
                    }
                    (Kind::Boolean, Some(Value::Bool(value))) => Value::Bool(value),
                    (Kind::Number, Some(Value::Number(number))) => Value::Number(number),
                    // A missing or mistyped value from a modified client.
                    _ => continue,
                };
                values.insert(key.clone(), typed);
            }
        }
        Some((
            Response {
                action: callback.id.clone(),
                values,
                payload: callback.payload.clone(),
            },
            callback.closes,
        ))
    }
}

struct Builder {
    serial: u64,
    callbacks: Vec<Callback>,
}

fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{path} must be an object"))
}

/// The fields of one dialog object, checked for unknown keys up front.
struct Fields<'a> {
    map: &'a Map<String, Value>,
    path: &'a str,
}

impl<'a> Fields<'a> {
    fn new(value: &'a Value, path: &'a str, allowed: &[&str]) -> Result<Self, String> {
        let map = object(value, path)?;
        if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(format!(
                "{path}: unknown field {key:?} (expected one of {})",
                allowed.join(", ")
            ));
        }
        Ok(Self { map, path })
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key).filter(|value| !value.is_null())
    }

    fn path(&self, key: &str) -> String {
        format!("{}.{key}", self.path)
    }

    fn component(
        &self,
        key: &str,
        required: bool,
        out: &mut Map<String, Value>,
    ) -> Result<(), String> {
        match self.get(key) {
            Some(value) => {
                out.insert(key.into(), text(value, &self.path(key))?);
                Ok(())
            }
            None if required => Err(format!("{} is required", self.path(key))),
            None => Ok(()),
        }
    }

    fn boolean(&self, key: &str, out: &mut Map<String, Value>) -> Result<Option<bool>, String> {
        match self.get(key) {
            Some(Value::Bool(value)) => {
                out.insert(key.into(), Value::Bool(*value));
                Ok(Some(*value))
            }
            Some(_) => Err(format!("{} must be a boolean", self.path(key))),
            None => Ok(None),
        }
    }

    fn integer(
        &self,
        key: &str,
        range: std::ops::RangeInclusive<i64>,
        out: &mut Map<String, Value>,
    ) -> Result<Option<i64>, String> {
        match self.get(key) {
            Some(value) => {
                let number = value
                    .as_i64()
                    .filter(|number| range.contains(number))
                    .ok_or_else(|| {
                        format!(
                            "{} must be an integer from {} to {}",
                            self.path(key),
                            range.start(),
                            range.end()
                        )
                    })?;
                out.insert(key.into(), Value::from(number));
                Ok(Some(number))
            }
            None => Ok(None),
        }
    }

    fn number(&self, key: &str, out: &mut Map<String, Value>) -> Result<Option<f64>, String> {
        match self.get(key) {
            Some(value) => {
                let number = value
                    .as_f64()
                    .filter(|number| number.is_finite() && number.abs() <= f64::from(f32::MAX))
                    .ok_or_else(|| format!("{} must be a number", self.path(key)))?;
                out.insert(key.into(), value.clone());
                Ok(Some(number))
            }
            None => Ok(None),
        }
    }

    fn string(
        &self,
        key: &str,
        required: bool,
        out: &mut Map<String, Value>,
    ) -> Result<Option<&'a str>, String> {
        match self.get(key) {
            Some(Value::String(text)) if text.len() <= MAX_STRING => {
                out.insert(key.into(), Value::String(text.clone()));
                Ok(Some(text))
            }
            Some(_) => Err(format!(
                "{} must be a string of at most {MAX_STRING} bytes",
                self.path(key)
            )),
            None if required => Err(format!("{} is required", self.path(key))),
            None => Ok(None),
        }
    }

    /// A list, allowing `{}` for an empty one (an empty Lua table).
    fn list(&self, key: &str) -> Result<Option<Vec<&'a Value>>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Array(items)) => Ok(Some(items.iter().collect())),
            Some(Value::Object(map)) if map.is_empty() => Ok(Some(Vec::new())),
            Some(_) => Err(format!("{} must be a list", self.path(key))),
        }
    }

    /// A `type` field, with or without the `minecraft:` namespace.
    fn kind(&self, kinds: &[&'a str]) -> Result<&'a str, String> {
        let Some(Value::String(kind)) = self.get("type") else {
            return Err(format!("{} requires a type", self.path));
        };
        let short = kind.strip_prefix("minecraft:").unwrap_or(kind);
        kinds
            .iter()
            .find(|candidate| **candidate == short)
            .copied()
            .ok_or_else(|| {
                format!(
                    "{}: unsupported type {kind:?} (expected one of {})",
                    self.path,
                    kinds.join(", ")
                )
            })
    }
}

/// Fields every dialog type accepts.
const COMMON: &[&str] = &[
    "type",
    "title",
    "external_title",
    "can_close_with_escape",
    "pause",
    "after_action",
    "body",
    "inputs",
];

/// A width in vanilla's `Dialog.WIDTH_CODEC` range.
const WIDTH: std::ops::RangeInclusive<i64> = 1..=1024;
const POSITIVE: std::ops::RangeInclusive<i64> = 1..=i32::MAX as i64;

/// Whether `name` is a vanilla template variable (`StringTemplate`).
fn variable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A dialog text: a JSON text component, with markup objects (`{"mini":
/// ...}`) expanded and checked like every text Rift sends. An empty string
/// stays empty.
fn text(value: &Value, path: &str) -> Result<Value, String> {
    if value.as_str() == Some("") {
        return Ok(value.clone());
    }
    Ok(match crate::text::Text::component(value.clone(), path)? {
        crate::text::Text::Component(component) => component,
        crate::text::Text::Plain(text) => Value::String(text),
    })
}

/// Characters a chat command may contain (`StringUtil.isAllowedChatCharacter`).
pub(crate) fn chat_string(text: &str) -> bool {
    text.chars().all(|c| c != '§' && c >= ' ' && c != '\u{7f}')
}

/// An identifier (`namespace:path`) as Minecraft parses it.
pub(crate) fn identifier(id: &str) -> bool {
    let (namespace, path) = id.split_once(':').unwrap_or(("minecraft", id));
    let valid = |text: &str, slash: bool| {
        text.bytes().all(|b| {
            b.is_ascii_lowercase()
                || b.is_ascii_digit()
                || matches!(b, b'_' | b'-' | b'.')
                || (slash && b == b'/')
        })
    };
    !namespace.is_empty() && !path.is_empty() && valid(namespace, false) && valid(path, true)
}

impl Builder {
    fn dialog(&mut self, value: &Value, path: &str, depth: usize) -> Result<Value, String> {
        if depth > MAX_NESTING {
            return Err(format!(
                "{path}: dialogs nest more than {MAX_NESTING} levels"
            ));
        }
        let map = object(value, path)?;
        let kind = Fields { map, path }.kind(&[
            "notice",
            "confirmation",
            "multi_action",
            "server_links",
            "dialog_list",
        ])?;
        let extra: &[&str] = match kind {
            "notice" => &["action"],
            "confirmation" => &["yes", "no"],
            "multi_action" => &["actions", "columns", "exit_action"],
            "server_links" => &["exit_action", "columns", "button_width"],
            _ => &["dialogs", "exit_action", "columns", "button_width"],
        };
        let allowed: Vec<&str> = COMMON.iter().chain(extra).copied().collect();
        let fields = Fields::new(value, path, &allowed)?;
        let mut out = Map::new();
        out.insert("type".into(), Value::String(format!("minecraft:{kind}")));
        fields.component("title", true, &mut out)?;
        fields.component("external_title", false, &mut out)?;
        fields.boolean("can_close_with_escape", &mut out)?;
        let pause = fields.boolean("pause", &mut out)?.unwrap_or(true);
        let after = fields
            .string("after_action", false, &mut out)?
            .unwrap_or("close");
        if !matches!(after, "close" | "none" | "wait_for_response") {
            return Err(format!(
                "{}: expected close, none or wait_for_response",
                fields.path("after_action")
            ));
        }
        if pause && after == "none" {
            return Err(format!(
                "{path}: a dialog that pauses the game must use after_action close or wait_for_response"
            ));
        }
        let closes = after == "close";
        if let Some(body) = fields.get("body") {
            out.insert("body".into(), self.body(body, &fields.path("body"))?);
        }
        let mut inputs = Vec::new();
        if let Some(list) = fields.list("inputs")? {
            if list.len() > MAX_INPUTS {
                return Err(format!(
                    "{}: at most {MAX_INPUTS} inputs",
                    fields.path("inputs")
                ));
            }
            let mut wire = Vec::new();
            for (index, input) in list.into_iter().enumerate() {
                let input_path = format!("{}[{}]", fields.path("inputs"), index + 1);
                let (key, kind, value) = self.input(input, &input_path)?;
                if inputs.iter().any(|(existing, _)| *existing == key) {
                    return Err(format!("{input_path}: duplicate key {key:?}"));
                }
                inputs.push((key, kind));
                wire.push(value);
            }
            out.insert("inputs".into(), Value::Array(wire));
        }
        let scope = Scope {
            closes,
            inputs: &inputs,
            depth,
        };
        let button =
            |builder: &mut Self, key: &str, out: &mut Map<String, Value>, required: bool| {
                match fields.get(key) {
                    Some(value) => {
                        let button = builder.button(value, &fields.path(key), &scope)?;
                        out.insert(key.into(), button);
                        Ok(())
                    }
                    None if required => Err(format!("{} is required", fields.path(key))),
                    None => Ok(()),
                }
            };
        match kind {
            "notice" => button(self, "action", &mut out, false)?,
            "confirmation" => {
                button(self, "yes", &mut out, true)?;
                button(self, "no", &mut out, true)?;
            }
            "multi_action" => {
                let list = fields
                    .list("actions")?
                    .filter(|list| !list.is_empty())
                    .ok_or_else(|| {
                        format!("{} must list at least one button", fields.path("actions"))
                    })?;
                if list.len() > MAX_BUTTONS {
                    return Err(format!(
                        "{}: at most {MAX_BUTTONS} buttons",
                        fields.path("actions")
                    ));
                }
                let mut buttons = Vec::new();
                for (index, item) in list.into_iter().enumerate() {
                    let item_path = format!("{}[{}]", fields.path("actions"), index + 1);
                    buttons.push(self.button(item, &item_path, &scope)?);
                }
                out.insert("actions".into(), Value::Array(buttons));
                fields.integer("columns", POSITIVE, &mut out)?;
                button(self, "exit_action", &mut out, false)?;
            }
            "server_links" => {
                fields.integer("columns", POSITIVE, &mut out)?;
                fields.integer("button_width", WIDTH, &mut out)?;
                button(self, "exit_action", &mut out, false)?;
            }
            _ => {
                let list = fields
                    .list("dialogs")?
                    .filter(|list| !list.is_empty())
                    .ok_or_else(|| {
                        format!("{} must list at least one dialog", fields.path("dialogs"))
                    })?;
                if list.len() > MAX_BUTTONS {
                    return Err(format!(
                        "{}: at most {MAX_BUTTONS} dialogs",
                        fields.path("dialogs")
                    ));
                }
                let mut dialogs = Vec::new();
                for (index, item) in list.into_iter().enumerate() {
                    let item_path = format!("{}[{}]", fields.path("dialogs"), index + 1);
                    if !item.is_object() {
                        return Err(format!(
                            "{item_path}: only inline dialogs are supported, not registry references"
                        ));
                    }
                    dialogs.push(self.dialog(item, &item_path, depth + 1)?);
                }
                out.insert("dialogs".into(), Value::Array(dialogs));
                fields.integer("columns", POSITIVE, &mut out)?;
                fields.integer("button_width", WIDTH, &mut out)?;
                button(self, "exit_action", &mut out, false)?;
            }
        }
        Ok(Value::Object(out))
    }

    /// One body element or a list of them. A string, or a text component
    /// object (one without `type`), is a plain message.
    fn body(&mut self, value: &Value, path: &str) -> Result<Value, String> {
        let message = |value: &Value, path: &str| -> Result<Value, String> {
            let component = value.is_string()
                || value
                    .as_object()
                    .is_some_and(|fields| !fields.contains_key("type"));
            if component {
                let contents = text(value, path)?;
                return Ok(
                    serde_json::json!({"type": "minecraft:plain_message", "contents": contents}),
                );
            }
            let kind = object(value, path)?.get("type").and_then(Value::as_str);
            if kind.is_some_and(|kind| kind.strip_prefix("minecraft:").unwrap_or(kind) == "item") {
                return Err(format!(
                    "{path}: item bodies are not supported (an item one client version lacks would disconnect it)"
                ));
            }
            let fields = Fields::new(value, path, &["type", "contents", "width"])?;
            fields.kind(&["plain_message"])?;
            let mut out = Map::new();
            out.insert(
                "type".into(),
                Value::String("minecraft:plain_message".into()),
            );
            fields.component("contents", true, &mut out)?;
            fields.integer("width", WIDTH, &mut out)?;
            Ok(Value::Object(out))
        };
        match value {
            Value::Array(items) => {
                if items.len() > MAX_BUTTONS {
                    return Err(format!("{path}: at most {MAX_BUTTONS} body elements"));
                }
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| message(item, &format!("{path}[{}]", index + 1)))
                    .collect::<Result<_, _>>()
                    .map(Value::Array)
            }
            Value::Object(map) if map.is_empty() => Ok(Value::Array(Vec::new())),
            value => message(value, path),
        }
    }

    fn input(&mut self, value: &Value, path: &str) -> Result<(String, Kind, Value), String> {
        let map = object(value, path)?;
        let kind =
            Fields { map, path }.kind(&["text", "boolean", "single_option", "number_range"])?;
        let extra: &[&str] = match kind {
            "text" => &[
                "width",
                "label_visible",
                "initial",
                "max_length",
                "multiline",
            ],
            "boolean" => &["initial", "on_true", "on_false"],
            "single_option" => &["label_visible", "width", "options"],
            _ => &["label_format", "width", "start", "end", "step", "initial"],
        };
        let allowed: Vec<&str> = ["type", "key", "label"]
            .iter()
            .chain(extra)
            .copied()
            .collect();
        let fields = Fields::new(value, path, &allowed)?;
        let mut out = Map::new();
        out.insert("type".into(), Value::String(format!("minecraft:{kind}")));
        let key = fields.string("key", true, &mut out)?.unwrap_or_default();
        if !variable(key) {
            return Err(format!(
                "{}: must be 1..=64 letters, digits or '_'",
                fields.path("key")
            ));
        }
        fields.component("label", true, &mut out)?;
        let kind = match kind {
            "text" => {
                fields.integer("width", WIDTH, &mut out)?;
                fields.boolean("label_visible", &mut out)?;
                let max_length = fields
                    .integer("max_length", POSITIVE, &mut out)?
                    .unwrap_or(32);
                if let Some(initial) = fields.string("initial", false, &mut out)?
                    && initial.encode_utf16().count() as i64 > max_length
                {
                    return Err(format!(
                        "{}: longer than max_length ({max_length})",
                        fields.path("initial")
                    ));
                }
                if let Some(multiline) = fields.get("multiline") {
                    let path = fields.path("multiline");
                    let options = Fields::new(multiline, &path, &["max_lines", "height"])?;
                    let mut wire = Map::new();
                    options.integer("max_lines", POSITIVE, &mut wire)?;
                    options.integer("height", 1..=512, &mut wire)?;
                    out.insert("multiline".into(), Value::Object(wire));
                }
                Kind::Text
            }
            "boolean" => {
                fields.boolean("initial", &mut out)?;
                fields.string("on_true", false, &mut out)?;
                fields.string("on_false", false, &mut out)?;
                Kind::Boolean
            }
            "single_option" => {
                fields.boolean("label_visible", &mut out)?;
                fields.integer("width", WIDTH, &mut out)?;
                let list = fields
                    .list("options")?
                    .filter(|list| !list.is_empty())
                    .ok_or_else(|| {
                        format!("{} must list at least one option", fields.path("options"))
                    })?;
                if list.len() > MAX_BUTTONS {
                    return Err(format!(
                        "{}: at most {MAX_BUTTONS} options",
                        fields.path("options")
                    ));
                }
                let mut options = Vec::new();
                let mut initial = false;
                for (index, option) in list.into_iter().enumerate() {
                    let path = format!("{}[{}]", fields.path("options"), index + 1);
                    let mut wire = Map::new();
                    if let Value::String(id) = option {
                        wire.insert("id".into(), Value::String(id.clone()));
                    } else {
                        let entry = Fields::new(option, &path, &["id", "display", "initial"])?;
                        entry.string("id", true, &mut wire)?;
                        entry.component("display", false, &mut wire)?;
                        if entry.boolean("initial", &mut wire)? == Some(true) {
                            if initial {
                                return Err(format!("{path}: only one option may be initial"));
                            }
                            initial = true;
                        }
                    }
                    if wire["id"]
                        .as_str()
                        .is_none_or(|id| id.is_empty() || id.len() > MAX_STRING)
                    {
                        return Err(format!(
                            "{path}: id must be a string of 1..={MAX_STRING} bytes"
                        ));
                    }
                    options.push(Value::Object(wire));
                }
                out.insert("options".into(), Value::Array(options));
                Kind::Option
            }
            _ => {
                fields.string("label_format", false, &mut out)?;
                fields.integer("width", WIDTH, &mut out)?;
                let start = fields.number("start", &mut out)?;
                let end = fields.number("end", &mut out)?;
                let (Some(start), Some(end)) = (start, end) else {
                    return Err(format!("{path}: number_range requires start and end"));
                };
                if let Some(step) = fields.number("step", &mut out)?
                    && step <= 0.0
                {
                    return Err(format!("{}: must be greater than 0", fields.path("step")));
                }
                if let Some(initial) = fields.number("initial", &mut out)?
                    && !(start.min(end)..=start.max(end)).contains(&initial)
                {
                    return Err(format!(
                        "{}: must lie between start and end",
                        fields.path("initial")
                    ));
                }
                Kind::Number
            }
        };
        Ok((key.to_owned(), kind, Value::Object(out)))
    }

    fn button(&mut self, value: &Value, path: &str, scope: &Scope<'_>) -> Result<Value, String> {
        let fields = Fields::new(value, path, &["label", "tooltip", "width", "action"])?;
        let mut out = Map::new();
        fields.component("label", true, &mut out)?;
        fields.component("tooltip", false, &mut out)?;
        fields.integer("width", WIDTH, &mut out)?;
        if let Some(action) = fields.get("action") {
            out.insert(
                "action".into(),
                self.action(action, &fields.path("action"), scope)?,
            );
        }
        Ok(Value::Object(out))
    }

    fn action(&mut self, value: &Value, path: &str, scope: &Scope<'_>) -> Result<Value, String> {
        let map = object(value, path)?;
        let kind = Fields { map, path }.kind(&[
            "open_url",
            "run_command",
            "suggest_command",
            "change_page",
            "copy_to_clipboard",
            "show_dialog",
            "custom",
            "dynamic/run_command",
            "dynamic/custom",
        ])?;
        let extra: &[&str] = match kind {
            "open_url" => &["url"],
            "run_command" | "suggest_command" => &["command"],
            "change_page" => &["page"],
            "copy_to_clipboard" => &["value"],
            "show_dialog" => &["dialog"],
            "custom" => &["id", "payload"],
            "dynamic/run_command" => &["template"],
            _ => &["id", "additions"],
        };
        let allowed: Vec<&str> = ["type"].iter().chain(extra).copied().collect();
        let fields = Fields::new(value, path, &allowed)?;
        let mut out = Map::new();
        out.insert("type".into(), Value::String(format!("minecraft:{kind}")));
        match kind {
            "open_url" => {
                let url = fields.string("url", true, &mut out)?.unwrap_or_default();
                let valid = (url.starts_with("https://") || url.starts_with("http://"))
                    && url.len() > "https://".len()
                    && !url.chars().any(|c| c.is_whitespace() || c.is_control());
                if !valid {
                    return Err(format!(
                        "{}: must be an http or https URL",
                        fields.path("url")
                    ));
                }
            }
            "run_command" | "suggest_command" => {
                let command = fields
                    .string("command", true, &mut out)?
                    .unwrap_or_default();
                if !chat_string(command) {
                    return Err(format!(
                        "{}: must not contain control characters or '§'",
                        fields.path("command")
                    ));
                }
            }
            "change_page" => {
                fields
                    .integer("page", POSITIVE, &mut out)?
                    .ok_or_else(|| format!("{} is required", fields.path("page")))?;
            }
            "copy_to_clipboard" => {
                fields.string("value", true, &mut out)?;
            }
            "show_dialog" => {
                let dialog = fields
                    .get("dialog")
                    .ok_or_else(|| format!("{} is required", fields.path("dialog")))?;
                if !dialog.is_object() {
                    return Err(format!(
                        "{}: only inline dialogs are supported, not registry references",
                        fields.path("dialog")
                    ));
                }
                let nested = self.dialog(dialog, &fields.path("dialog"), scope.depth + 1)?;
                out.insert("dialog".into(), nested);
            }
            "dynamic/run_command" => {
                let template = fields
                    .string("template", true, &mut out)?
                    .unwrap_or_default();
                template_variables(template, scope.inputs)
                    .map_err(|error| format!("{}: {error}", fields.path("template")))?;
            }
            _ => {
                let dynamic = kind == "dynamic/custom";
                let id = match fields.get("id") {
                    Some(Value::String(id)) => id.as_str(),
                    _ => return Err(format!("{} must be a string", fields.path("id"))),
                };
                let extra = if dynamic { "additions" } else { "payload" };
                let payload = fields.get(extra).cloned();
                if dynamic && payload.as_ref().is_some_and(|value| !value.is_object()) {
                    return Err(format!("{} must be an object", fields.path(extra)));
                }
                if let Some(payload) = &payload {
                    // Kept or sent, it must have an NBT form.
                    crate::protocol::dialog::encodable(payload)
                        .map_err(|error| format!("{}: {error}", fields.path(extra)))?;
                }
                if id.contains(':') {
                    if !identifier(id) {
                        return Err(format!(
                            "{}: {id:?} is not a valid identifier",
                            fields.path("id")
                        ));
                    }
                    if id.starts_with("rift:") {
                        return Err(format!(
                            "{}: the rift namespace is reserved",
                            fields.path("id")
                        ));
                    }
                    // Someone else's action: sent as written, clicks reach the backend.
                    out.insert("id".into(), Value::String(id.to_owned()));
                    if let Some(payload) = payload {
                        out.insert(extra.into(), payload);
                    }
                } else {
                    if id.is_empty() || id.len() > 64 || id.chars().any(char::is_control) {
                        return Err(format!(
                            "{}: an action name must contain 1..=64 bytes without control characters",
                            fields.path("id")
                        ));
                    }
                    if self.callbacks.len() >= MAX_CALLBACKS {
                        return Err(format!(
                            "{path}: at most {MAX_CALLBACKS} plugin actions per dialog"
                        ));
                    }
                    let index = self.callbacks.len();
                    out.insert(
                        "id".into(),
                        Value::String(format!("{CLICK_PREFIX}{}/{index}", self.serial)),
                    );
                    self.callbacks.push(Callback {
                        id: id.to_owned(),
                        payload,
                        dynamic,
                        closes: scope.closes,
                        inputs: scope.inputs.to_vec(),
                    });
                }
            }
        }
        Ok(Value::Object(out))
    }
}

/// What a button's action may refer to.
struct Scope<'a> {
    closes: bool,
    inputs: &'a [(String, Kind)],
    depth: usize,
}

/// Check a `dynamic/run_command` template: every `$(name)` names an input.
fn template_variables(template: &str, inputs: &[(String, Kind)]) -> Result<(), String> {
    if !chat_string(template) {
        return Err("must not contain control characters or '§'".into());
    }
    let mut rest = template;
    while let Some(start) = rest.find("$(") {
        let after = &rest[start + 2..];
        let end = after
            .find(')')
            .ok_or_else(|| "unterminated $( in template".to_owned())?;
        let name = &after[..end];
        if !variable(name) {
            return Err(format!("invalid template variable $({name})"));
        }
        if !inputs.iter().any(|(key, _)| key == name) {
            return Err(format!("$({name}) names no input of this dialog"));
        }
        rest = &after[end + 1..];
    }
    Ok(())
}

/// A dialog table built in Lua as JSON. Tables with only the keys 1..n are
/// lists; an empty table is an empty object, which list fields also accept.
pub(crate) fn from_lua(value: mlua::Value) -> Result<Value, String> {
    fn convert(value: mlua::Value, depth: usize, count: &mut usize) -> Result<Value, String> {
        *count += 1;
        if depth > MAX_LUA_DEPTH || *count > MAX_LUA_VALUES {
            return Err(format!(
                "dialog exceeds {MAX_LUA_VALUES} values or {MAX_LUA_DEPTH} levels"
            ));
        }
        Ok(match value {
            mlua::Value::Nil => Value::Null,
            mlua::Value::Boolean(value) => Value::Bool(value),
            mlua::Value::Integer(value) => Value::from(value),
            mlua::Value::Number(value) if value.is_finite() => {
                if value.fract() == 0.0 && value.abs() < 2f64.powi(53) {
                    Value::from(value as i64)
                } else {
                    Value::from(value)
                }
            }
            mlua::Value::String(text) => Value::String(
                text.to_str()
                    .map_err(|_| "dialog strings must be UTF-8".to_owned())?
                    .to_owned(),
            ),
            mlua::Value::Table(table) => {
                let mut object = Map::new();
                let mut array = std::collections::BTreeMap::new();
                for pair in table.pairs::<mlua::Value, mlua::Value>() {
                    let (key, value) = pair.map_err(|error| error.to_string())?;
                    let value = convert(value, depth + 1, count)?;
                    match key {
                        mlua::Value::String(key) => {
                            let key = key
                                .to_str()
                                .map_err(|_| "dialog keys must be UTF-8".to_owned())?
                                .to_owned();
                            object.insert(key, value);
                        }
                        mlua::Value::Integer(index) if index >= 1 => {
                            array.insert(index, value);
                        }
                        _ => return Err("dialog table keys must be strings or list indices".into()),
                    }
                }
                match (object.is_empty(), array.is_empty()) {
                    (_, true) => Value::Object(object),
                    (true, false) if array.keys().copied().eq(1..=array.len() as i64) => {
                        Value::Array(array.into_values().collect())
                    }
                    _ => {
                        return Err(
                            "dialog tables must be either a dense list or have only string keys"
                                .into(),
                        );
                    }
                }
            }
            other => return Err(format!("unsupported {} in dialog", other.type_name())),
        })
    }
    convert(value, 0, &mut 0)
}

#[cfg(test)]
mod tests;
