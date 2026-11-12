//! Proxy-owned server-list responses: MOTD, version text, player counts,
//! hover sample and favicon.
//!
//! Static settings (including favicon files) are resolved when configuration
//! loads, so answering a ping never touches the filesystem. An optional
//! `on_status` hook runs in a fresh sandboxed VM per ping and may override any
//! field. Every document is bounded to the protocol's 32767-byte status string.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    net::SocketAddr,
    path::Path,
    sync::Arc,
    time::Instant,
};

use mlua::{Function, Value};
use serde_json::{Value as Json, json};

use crate::{
    hooks::RouteError,
    protocol::{Packet, write_string},
    script::ScriptSource,
    text::{Profile, Text},
};

const MAX_RESPONSE_BYTES: usize = 32767;
const MAX_MOTD_BYTES: usize = 4096;
const MAX_TEXT_BYTES: usize = 256;
const MAX_SAMPLE_LINES: usize = 16;
const MAX_COMPONENT_NODES: usize = 1024;
const MAX_COMPONENT_DEPTH: usize = 16;
const MAX_FAVICON_BYTES: usize = 16 * 1024;
const FAVICON_PREFIX: &str = "data:image/png;base64,";
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
// Sample entries are hover text, not real profiles.
const SAMPLE_ID: &str = "00000000-0000-0000-0000-000000000000";

/// One set of server-list settings. Unset fields inherit from the top-level
/// entry, then from Rift's built-in defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// A plain string or a chat component.
    pub motd: Option<Json>,
    pub maintenance_motd: Option<Json>,
    /// Shown as `§` codes: the version name is a string to clients.
    pub version: Option<Text>,
    pub max_players: Option<u32>,
    /// A `data:image/png;base64,...` URL of a 64×64 PNG.
    pub favicon: Option<Arc<str>>,
    /// Hover lines, shown as `§` codes like the version name.
    pub sample: Option<Vec<Text>>,
}

impl Entry {
    fn or(&self, base: &Self) -> Self {
        Self {
            motd: self.motd.clone().or_else(|| base.motd.clone()),
            maintenance_motd: self
                .maintenance_motd
                .clone()
                .or_else(|| base.maintenance_motd.clone()),
            version: self.version.clone().or_else(|| base.version.clone()),
            max_players: self.max_players.or(base.max_players),
            favicon: self.favicon.clone().or_else(|| base.favicon.clone()),
            sample: self.sample.clone().or_else(|| base.sample.clone()),
        }
    }
}

/// `rift.config.server_list`: network-wide defaults plus hostname overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerList {
    pub defaults: Entry,
    /// Normalized exact hostnames or `*.suffix` patterns, matched like routes.
    pub hosts: BTreeMap<String, Entry>,
}

impl ServerList {
    /// Exact hostnames win over wildcards; the longest wildcard suffix wins.
    pub fn entry(&self, hostname: &str) -> Entry {
        let host = self.hosts.get(hostname).or_else(|| {
            self.hosts
                .iter()
                .filter_map(|(pattern, entry)| {
                    let suffix = pattern.strip_prefix('*')?;
                    (hostname.len() > suffix.len() && hostname.ends_with(suffix))
                        .then_some((suffix.len(), entry))
                })
                .max_by_key(|(length, _)| *length)
                .map(|(_, entry)| entry)
        });
        host.map_or_else(|| self.defaults.clone(), |host| host.or(&self.defaults))
    }

    /// Reject settings whose largest possible response exceeds the protocol limit.
    pub(crate) fn validate(&self) -> Result<(), String> {
        for (path, entry) in std::iter::once(("server_list".to_owned(), self.defaults.clone()))
            .chain(self.hosts.iter().map(|(host, entry)| {
                (
                    format!("server_list.hosts[{host:?}]"),
                    entry.or(&self.defaults),
                )
            }))
        {
            for maintenance in [false, true] {
                let mut document = Document::new(i32::MIN, u64::MAX, u64::MAX, Text::from(""));
                document.apply(&entry, maintenance);
                document
                    .packet(Profile::UNKNOWN)
                    .map_err(|error| format!("{path}: {error}"))?;
            }
        }
        Ok(())
    }
}

/// Fields an `on_status` hook may replace for one ping.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StatusOverride {
    pub motd: Option<Json>,
    pub version: Option<Text>,
    pub protocol: Option<i32>,
    pub online: Option<u32>,
    pub max_players: Option<u32>,
    pub sample: Option<Vec<Text>>,
    pub favicon: Option<Arc<str>>,
}

/// Owned inputs for `on_status`, available before any backend contact.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Ping {
    pub listener: String,
    pub hostname: String,
    pub protocol: i32,
    pub peer_addr: SocketAddr,
    /// The routed backend or group, or `None` for an unmatched hostname.
    pub backend: Option<String>,
    pub maintenance: bool,
    pub online: u64,
    pub max_players: u64,
}

/// A complete server-list response after defaults and overrides apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub motd: Json,
    pub version: Text,
    pub protocol: i32,
    pub online: u64,
    pub max_players: u64,
    pub sample: Vec<Text>,
    pub favicon: Option<Arc<str>>,
}

impl Document {
    /// A document with Rift's version name (`status.version`).
    pub fn new(protocol: i32, online: u64, max_players: u64, motd: Text) -> Self {
        Self {
            motd: motd.for_client(Profile::UNKNOWN).into_owned(),
            version: crate::text::catalog::notice(crate::text::catalog::Notice::StatusVersion),
            protocol,
            online,
            max_players,
            sample: Vec::new(),
            favicon: None,
        }
    }

    pub fn apply(&mut self, entry: &Entry, maintenance: bool) {
        let motd = if maintenance {
            entry.maintenance_motd.as_ref()
        } else {
            entry.motd.as_ref()
        };
        if let Some(motd) = motd {
            self.motd = motd.clone();
        }
        if let Some(version) = &entry.version {
            self.version = version.clone();
        }
        if let Some(maximum) = entry.max_players {
            self.max_players = maximum.into();
        }
        if let Some(sample) = &entry.sample {
            self.sample = sample.clone();
        }
        if entry.favicon.is_some() {
            self.favicon = entry.favicon.clone();
        }
    }

    /// Apply all changes or none: an oversized result keeps the current document.
    pub fn apply_override(&mut self, changes: StatusOverride) -> io::Result<()> {
        let mut next = self.clone();
        if let Some(motd) = changes.motd {
            next.motd = motd;
        }
        if let Some(version) = changes.version {
            next.version = version;
        }
        if let Some(protocol) = changes.protocol {
            next.protocol = protocol;
        }
        if let Some(online) = changes.online {
            next.online = online.into();
        }
        if let Some(maximum) = changes.max_players {
            next.max_players = maximum.into();
        }
        if let Some(sample) = changes.sample {
            next.sample = sample;
        }
        if changes.favicon.is_some() {
            next.favicon = changes.favicon;
        }
        next.packet(Profile::UNKNOWN)?;
        *self = next;
        Ok(())
    }

    /// The status response as `profile` reads it: the MOTD is downsampled
    /// like any component, and the version name and hover lines, which
    /// clients read as strings, become `§` codes.
    pub fn packet(&self, profile: Profile) -> io::Result<Packet> {
        let description = match &self.motd {
            Json::String(text) => json!({ "text": text }),
            component => {
                crate::text::for_client(component, profile).unwrap_or_else(|| component.clone())
            }
        };
        let mut players = json!({ "max": self.max_players, "online": self.online });
        if !self.sample.is_empty() {
            players["sample"] = self
                .sample
                .iter()
                .map(|line| json!({ "name": line.to_legacy(profile), "id": SAMPLE_ID }))
                .collect();
        }
        let mut response = json!({
            "version": { "name": self.version.to_legacy(profile), "protocol": self.protocol },
            "players": players,
            "description": description,
            "enforcesSecureChat": false,
        });
        if let Some(favicon) = &self.favicon {
            response["favicon"] = Json::String(favicon.to_string());
        }
        let response = response.to_string();
        if response.len() > MAX_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "server-list response exceeds 32767 bytes",
            ));
        }
        let mut data = Vec::new();
        write_string(&response, &mut data);
        Ok(Packet::new(0, data))
    }
}

/// A string, or a chat component table such as `{ text = "Hi", color = "gold" }`.
pub(crate) fn motd(value: Value, path: &str) -> Result<Json, String> {
    let motd = match value {
        Value::String(text) => Json::String(utf8(&text, path)?),
        Value::Table(_) => {
            let component = chat_component(value, path)?;
            if component.as_object().is_some_and(serde_json::Map::is_empty) {
                return Err(format!("{path}: chat component must not be empty"));
            }
            // Markup objects expand here; the result is checked like any text.
            match Text::component(component, path)? {
                Text::Component(component) => component,
                Text::Plain(text) => Json::String(text),
            }
        }
        _ => {
            return Err(format!(
                "{path}: expected a string or chat component table, got {}",
                value.type_name()
            ));
        }
    };
    if motd.to_string().len() > MAX_MOTD_BYTES {
        return Err(format!("{path}: exceeds {MAX_MOTD_BYTES} bytes"));
    }
    Ok(motd)
}

/// A Lua chat component table as JSON: at most 1024 values in 16 levels.
pub(crate) fn chat_component(value: Value, path: &str) -> Result<Json, String> {
    component(value, 0, &mut 0, path)
}

fn component(value: Value, depth: usize, nodes: &mut usize, path: &str) -> Result<Json, String> {
    *nodes += 1;
    if *nodes > MAX_COMPONENT_NODES || depth > MAX_COMPONENT_DEPTH {
        return Err(format!(
            "{path}: chat component exceeds {MAX_COMPONENT_NODES} values or {MAX_COMPONENT_DEPTH} levels"
        ));
    }
    Ok(match value {
        Value::String(text) => Json::String(utf8(&text, path)?),
        Value::Boolean(value) => Json::Bool(value),
        Value::Integer(value) => Json::from(value),
        Value::Number(value) if value.is_finite() => {
            if value.fract() == 0.0 && value.abs() < 2f64.powi(53) {
                Json::from(value as i64)
            } else {
                Json::from(value)
            }
        }
        Value::Table(table) => {
            let mut object = serde_json::Map::new();
            let mut array = BTreeMap::new();
            for pair in table.pairs::<Value, Value>() {
                let (key, value) = pair.map_err(|error| format!("{path}: {error}"))?;
                let value = component(value, depth + 1, nodes, path)?;
                match key {
                    Value::String(key) => {
                        object.insert(utf8(&key, path)?, value);
                    }
                    Value::Integer(index) if index >= 1 => {
                        array.insert(index, value);
                    }
                    _ => return Err(format!("{path}: chat component keys must be strings")),
                }
            }
            match (object.is_empty(), array.is_empty()) {
                (_, true) => Json::Object(object),
                (true, false) if array.keys().copied().eq(1..=array.len() as i64) => {
                    Json::Array(array.into_values().collect())
                }
                _ => {
                    return Err(format!(
                        "{path}: chat component tables must be either a dense list or have only string keys"
                    ));
                }
            }
        }
        _ => {
            return Err(format!(
                "{path}: unsupported {} in chat component",
                value.type_name()
            ));
        }
    })
}

/// A version name or hover line: a string of at most 256 bytes, or a chat
/// component table (including `{ mini = ... }`) shown as `§` codes.
pub(crate) fn text(value: Value, path: &str) -> Result<Text, String> {
    match value {
        Value::String(text) if text.as_bytes().len() <= MAX_TEXT_BYTES => {
            utf8(&text, path).map(Text::Plain)
        }
        Value::Table(_) => line_component(chat_component(value, path)?, path),
        _ => Err(format!(
            "{path}: expected a string of at most {MAX_TEXT_BYTES} bytes or a chat component table"
        )),
    }
}

/// A component for a string-only field: checked, and at most 256 bytes once
/// written as `§` codes.
fn line_component(component: Json, path: &str) -> Result<Text, String> {
    if component.to_string().len() > MAX_MOTD_BYTES {
        return Err(format!("{path}: exceeds {MAX_MOTD_BYTES} bytes"));
    }
    let text = Text::component(component, path)?;
    if text.to_legacy(Profile::UNKNOWN).len() > MAX_TEXT_BYTES {
        return Err(format!(
            "{path}: shows more than {MAX_TEXT_BYTES} bytes as a string"
        ));
    }
    Ok(text)
}

/// A player count: an integer in `0..=i32::MAX`.
pub(crate) fn count(value: Value, path: &str) -> Result<u32, String> {
    integer(value, path, 0).map(|value| value as u32)
}

/// Clients compare this with their own protocol; `-1` always shows as incompatible.
fn protocol(value: Value, path: &str) -> Result<i32, String> {
    integer(value, path, i32::MIN.into()).map(|value| value as i32)
}

fn integer(value: Value, path: &str, minimum: i64) -> Result<i64, String> {
    let number = match value {
        Value::Integer(value) => Some(value),
        // LuaJIT can represent integral arithmetic results as floating-point numbers.
        Value::Number(value) if value.fract() == 0.0 && value.abs() <= i32::MAX as f64 + 1.0 => {
            Some(value as i64)
        }
        _ => None,
    };
    number
        .filter(|value| (minimum..=i32::MAX.into()).contains(value))
        .ok_or_else(|| format!("{path}: expected an integer in {minimum}..=2147483647"))
}

/// Hover lines shown over the player count.
pub(crate) fn sample(value: Value, path: &str) -> Result<Vec<Text>, String> {
    let Value::Table(table) = value else {
        return Err(format!("{path}: expected a list of strings"));
    };
    let mut lines = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (index, value) = pair.map_err(|error| format!("{path}: {error}"))?;
        let Value::Integer(index) = index else {
            return Err(format!("{path}: expected a list of strings"));
        };
        if lines.len() >= MAX_SAMPLE_LINES {
            return Err(format!("{path}: at most {MAX_SAMPLE_LINES} lines"));
        }
        lines.insert(index, text(value, path)?);
    }
    if lines.keys().copied().ne(1..=lines.len() as i64) {
        return Err(format!("{path}: expected a dense list of strings"));
    }
    Ok(lines.into_values().collect())
}

/// Read a 64×64 PNG once, at configuration load.
pub(crate) fn favicon_file(path: &Path) -> Result<Arc<str>, String> {
    let fail = |message: &dyn std::fmt::Display| format!("{}: {message}", path.display());
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_FAVICON_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| fail(&error))?;
    png_favicon(&bytes).map_err(|message| fail(&message))?;
    Ok(format!("{FAVICON_PREFIX}{}", base64(&bytes)).into())
}

/// A favicon embedded in configuration as a `data:image/png;base64,...` URL,
/// as the web dashboard saves uploads. It obeys the same rules as a file.
pub(crate) fn is_favicon_data(value: &str) -> bool {
    value.starts_with(FAVICON_PREFIX)
}

pub fn favicon_data(url: &str) -> Result<Arc<str>, String> {
    if !valid_favicon(url.as_bytes()) {
        return Err(format!(
            "expected a \"{FAVICON_PREFIX}...\" URL of a PNG up to 16 KiB"
        ));
    }
    let bytes = unbase64(&url[FAVICON_PREFIX.len()..])
        .ok_or_else(|| "favicon data is not valid base64".to_owned())?;
    png_favicon(&bytes)?;
    Ok(url.into())
}

fn png_favicon(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() > MAX_FAVICON_BYTES {
        return Err("favicon exceeds 16 KiB");
    }
    if !bytes.starts_with(PNG_SIGNATURE) || bytes.get(12..16) != Some(b"IHDR") {
        return Err("favicon must be a PNG image");
    }
    let dimension =
        |range: std::ops::Range<usize>| u32::from_be_bytes(bytes[range].try_into().unwrap());
    if bytes.len() < 24 || dimension(16..20) != 64 || dimension(20..24) != 64 {
        return Err("favicon must be 64×64 pixels");
    }
    Ok(())
}

/// A hook-supplied favicon: an already encoded PNG data URL.
fn valid_favicon(url: &[u8]) -> bool {
    let limit = FAVICON_PREFIX.len() + MAX_FAVICON_BYTES.div_ceil(3) * 4;
    url.len() <= limit
        && url.starts_with(FAVICON_PREFIX.as_bytes())
        && url.len() > FAVICON_PREFIX.len()
        && url[FAVICON_PREFIX.len()..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

/// Validate a persistent plugin's `status` reply with the limits `on_status`
/// results obey.
pub(crate) fn status_override_json(
    reply: crate::plugins::api::StatusOverride,
) -> Result<StatusOverride, String> {
    fn nodes(value: &Json, depth: usize, count: &mut usize) -> bool {
        *count += 1;
        if *count > MAX_COMPONENT_NODES || depth > MAX_COMPONENT_DEPTH {
            return false;
        }
        match value {
            Json::Array(values) => values.iter().all(|value| nodes(value, depth + 1, count)),
            Json::Object(values) => values.values().all(|value| nodes(value, depth + 1, count)),
            _ => true,
        }
    }
    let line = |value: Text, path: &str| match value {
        Text::Plain(text) if text.len() > MAX_TEXT_BYTES => Err(format!(
            "{path}: expected a string of at most {MAX_TEXT_BYTES} bytes"
        )),
        Text::Plain(text) => Ok(Text::Plain(text)),
        Text::Component(component) => line_component(component, path),
    };
    let motd = reply
        .motd_json
        .map(|text| {
            let motd: Json = serde_json::from_str(&text)
                .map_err(|_| "motd_json: expected a JSON string or chat component".to_owned())?;
            if !(motd.is_string() || motd.is_object() || motd.is_array())
                || motd.as_object().is_some_and(serde_json::Map::is_empty)
                || !nodes(&motd, 0, &mut 0)
                || motd.to_string().len() > MAX_MOTD_BYTES
            {
                return Err(format!(
                    "motd_json: expected a nonempty JSON string or chat component of at most {MAX_MOTD_BYTES} bytes, {MAX_COMPONENT_NODES} values and {MAX_COMPONENT_DEPTH} levels"
                ));
            }
            if motd.is_string() {
                return Ok(motd);
            }
            match Text::component(motd, "motd_json")? {
                Text::Component(component) => Ok(component),
                Text::Plain(text) => Ok(Json::String(text)),
            }
        })
        .transpose()?;
    let sample = reply
        .sample
        .map(|lines| {
            if lines.len() > MAX_SAMPLE_LINES {
                return Err(format!("sample: at most {MAX_SAMPLE_LINES} lines"));
            }
            lines
                .into_iter()
                .map(|value| line(value, "sample"))
                .collect()
        })
        .transpose()?;
    let favicon = reply
        .favicon
        .map(|url| {
            if !valid_favicon(url.as_bytes()) {
                return Err(format!(
                    "favicon: expected a \"{FAVICON_PREFIX}...\" URL of a PNG up to 16 KiB"
                ));
            }
            Ok(Arc::from(url))
        })
        .transpose()?;
    Ok(StatusOverride {
        motd,
        version: reply
            .version
            .map(|value| line(value, "version"))
            .transpose()?,
        protocol: reply.protocol,
        online: reply.online,
        max_players: reply.max_players,
        sample,
        favicon,
    })
}

pub(crate) fn favicon_url(value: Value, path: &str) -> Result<Arc<str>, String> {
    let valid = match &value {
        Value::String(url) => valid_favicon(&url.as_bytes()),
        _ => false,
    };
    if !valid {
        return Err(format!(
            "{path}: expected a \"{FAVICON_PREFIX}...\" URL of a PNG up to 16 KiB"
        ));
    }
    let Value::String(url) = value else {
        unreachable!()
    };
    Ok(utf8(&url, path)?.into())
}

fn utf8(text: &mlua::LuaString, path: &str) -> Result<String, String> {
    text.to_str()
        .map(|text| text.to_owned())
        .map_err(|_| format!("{path}: expected UTF-8 text"))
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| {
            value | (*byte as u32) << (16 - 8 * index)
        });
        for index in 0..4 {
            encoded.push(if index <= chunk.len() {
                ALPHABET[(value >> (18 - 6 * index) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    encoded
}

/// Standard padded base64, as [`base64`] writes it.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    let text = text.as_bytes();
    if !text.len().is_multiple_of(4) {
        return None;
    }
    let padding = text.iter().rev().take_while(|&&b| b == b'=').count();
    if padding > 2 {
        return None;
    }
    let sextet = |byte: u8| -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let mut decoded = Vec::with_capacity(text.len() / 4 * 3);
    let body = text.len() - padding;
    for (index, chunk) in text.chunks(4).enumerate() {
        let mut value = 0u32;
        let mut length = 0;
        for (offset, &byte) in chunk.iter().enumerate() {
            let position = index * 4 + offset;
            let bits = if position < body { sextet(byte)? } else { 0 };
            if position < body {
                length += 1;
            }
            value |= bits << (18 - 6 * offset);
        }
        decoded.extend(&value.to_be_bytes()[1..length.max(1)]);
    }
    Some(decoded)
}

/// Immutable source snapshot for `on_status`; no Lua handles escape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusScript {
    source: ScriptSource,
}

impl StatusScript {
    pub(crate) fn new(source: &ScriptSource) -> Self {
        Self {
            source: source.clone(),
        }
    }

    pub(crate) fn evaluate(
        &self,
        ping: &Ping,
        deadline: Instant,
        messaging: Option<crate::messaging::Broker>,
    ) -> Result<Option<StatusOverride>, RouteError> {
        let run = || -> mlua::Result<Option<StatusOverride>> {
            let (lua, root) = crate::script::load(&self.source, deadline)?;
            crate::script::install_messaging(&lua, messaging, deadline)?;
            let Value::Table(root) = root else {
                return Err(mlua::Error::runtime("configuration must return a table"));
            };
            let hook: Function = root.raw_get("on_status")?;
            let input = lua.create_table()?;
            input.raw_set("listener", ping.listener.as_str())?;
            input.raw_set("hostname", ping.hostname.as_str())?;
            input.raw_set("protocol", ping.protocol)?;
            input.raw_set("peer_addr", ping.peer_addr.to_string())?;
            input.raw_set("peer_ip", ping.peer_addr.ip().to_string())?;
            input.raw_set("backend", ping.backend.as_deref())?;
            input.raw_set("maintenance", ping.maintenance)?;
            input.raw_set("online", ping.online)?;
            input.raw_set("max_players", ping.max_players)?;
            let result =
                status_override(hook.call::<Value>(input)?).map_err(mlua::Error::runtime)?;
            crate::script::check_deadline(deadline)?;
            Ok(result)
        };
        run().map_err(|error| {
            RouteError::Script(format!("{}: on_status: {error}", self.source.entry.name))
        })
    }
}

pub(crate) fn status_override(value: Value) -> Result<Option<StatusOverride>, String> {
    let table = match value {
        Value::Nil => return Ok(None),
        Value::Table(table) => table,
        _ => return Err("expected nil or a table of server-list fields".into()),
    };
    let mut changes = StatusOverride::default();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|error| error.to_string())?;
        let key = match &key {
            Value::String(key) => key.to_str().map(|key| key.to_owned()).ok(),
            _ => None,
        }
        .unwrap_or_default();
        match key.as_str() {
            "motd" => changes.motd = Some(motd(value, "motd")?),
            "version" => changes.version = Some(text(value, "version")?),
            "protocol" => changes.protocol = Some(protocol(value, "protocol")?),
            "online" => changes.online = Some(count(value, "online")?),
            "max_players" => changes.max_players = Some(count(value, "max_players")?),
            "sample" => changes.sample = Some(sample(value, "sample")?),
            "favicon" => changes.favicon = Some(favicon_url(value, "favicon")?),
            _ => {
                return Err(
                    "unknown field (expected motd, version, protocol, online, max_players, sample, favicon)"
                        .into(),
                );
            }
        }
    }
    Ok(Some(changes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(packet: &Packet) -> Json {
        let mut bytes = packet.data.as_slice();
        serde_json::from_str(crate::protocol::read_string(&mut bytes, 32767).unwrap()).unwrap()
    }

    #[test]
    fn base64_matches_reference_padding() {
        for (input, expected) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"\xff\x00\xfe", "/wD+"),
        ] {
            assert_eq!(base64(input), expected);
            assert_eq!(unbase64(expected).as_deref(), Some(input));
        }
        for invalid in ["Zg=", "Z===", "Zg=a", "Zm9v!A==", "===="] {
            assert_eq!(unbase64(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn favicon_data_urls_must_hold_a_64_pixel_png() {
        let mut png = PNG_SIGNATURE.to_vec();
        png.extend(b"\0\0\0\x0dIHDR");
        png.extend(64u32.to_be_bytes());
        png.extend(64u32.to_be_bytes());
        let url = format!("{FAVICON_PREFIX}{}", base64(&png));
        assert!(is_favicon_data(&url));
        assert_eq!(favicon_data(&url).unwrap().as_ref(), url);
        png[19] = 32;
        let small = format!("{FAVICON_PREFIX}{}", base64(&png));
        assert!(favicon_data(&small).unwrap_err().contains("64×64"));
        let jpeg = format!("{FAVICON_PREFIX}{}", base64(b"\xff\xd8\xff\xe0 not a png"));
        assert!(favicon_data(&jpeg).unwrap_err().contains("PNG"));
        assert!(favicon_data(&format!("{FAVICON_PREFIX}Zg=")).is_err());
        assert!(favicon_data("data:image/jpeg;base64,AAAA").is_err());
    }

    #[test]
    fn hostname_entries_inherit_defaults_and_prefer_exact_then_longest_wildcard() {
        let entry = |motd: &str| Entry {
            motd: Some(Json::String(motd.into())),
            ..Entry::default()
        };
        let list = ServerList {
            defaults: Entry {
                version: Some("Network".into()),
                ..entry("default")
            },
            hosts: BTreeMap::from([
                ("*.example.com".into(), entry("wildcard")),
                ("*.eu.example.com".into(), entry("eu")),
                ("play.eu.example.com".into(), entry("exact")),
            ]),
        };
        for (host, expected) in [
            ("other.net", "default"),
            ("example.com", "default"),
            ("a.example.com", "wildcard"),
            ("a.eu.example.com", "eu"),
            ("play.eu.example.com", "exact"),
        ] {
            let resolved = list.entry(host);
            assert_eq!(resolved.motd, Some(Json::String(expected.into())), "{host}");
            assert_eq!(resolved.version, Some(Text::from("Network")));
        }
    }

    #[test]
    fn document_renders_components_sample_and_favicon_and_rejects_oversized_overrides() {
        let mut document = Document::new(774, 3, 100, Text::from("Rift"));
        document.apply(
            &Entry {
                motd: Some(json!({ "text": "Hi", "color": "gold" })),
                maintenance_motd: Some(Json::String("Down".into())),
                max_players: Some(500),
                sample: Some(vec!["line".into()]),
                favicon: Some("data:image/png;base64,AAAA".into()),
                ..Entry::default()
            },
            false,
        );
        let response = decode(&document.packet(Profile::UNKNOWN).unwrap());
        assert_eq!(
            response["description"],
            json!({ "text": "Hi", "color": "gold" })
        );
        assert_eq!(
            response["players"],
            json!({ "max": 500, "online": 3, "sample": [{ "name": "line", "id": SAMPLE_ID }] })
        );
        assert_eq!(
            response["version"],
            json!({ "name": "Rift", "protocol": 774 })
        );
        assert_eq!(response["favicon"], "data:image/png;base64,AAAA");

        let before = document.clone();
        let huge = format!("{FAVICON_PREFIX}{}", "A".repeat(MAX_RESPONSE_BYTES));
        assert!(
            document
                .apply_override(StatusOverride {
                    online: Some(9),
                    favicon: Some(huge.into()),
                    ..StatusOverride::default()
                })
                .is_err()
        );
        assert_eq!(document, before);
    }

    #[test]
    fn string_fields_become_section_codes_and_the_motd_follows_the_client() {
        let mut document = Document::new(47, 1, 10, Text::from("x"));
        document.motd = json!({"text": "Hi", "color": "#ff8800"});
        document.version = Text::Component(json!({"text": "Net", "color": "gold", "bold": true}));
        document.sample = vec![
            Text::Component(json!({"text": "a", "color": "#5555ff"})),
            Text::from("§cplain"),
        ];
        let old = Profile::of(crate::protocol::ProtocolVersion::new(47).unwrap());
        let response = decode(&document.packet(old).unwrap());
        assert_eq!(
            response["description"],
            json!({"text": "Hi", "color": "gold"})
        );
        assert_eq!(response["version"]["name"], "§6§lNet");
        assert_eq!(
            response["players"]["sample"],
            json!([{"name": "§9a", "id": SAMPLE_ID}, {"name": "§cplain", "id": SAMPLE_ID}])
        );
        let new = Profile::of(crate::protocol::ProtocolVersion::new(774).unwrap());
        let response = decode(&document.packet(new).unwrap());
        assert_eq!(
            response["description"],
            json!({"text": "Hi", "color": "#ff8800"})
        );
    }
}
