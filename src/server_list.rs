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
    pub version: Option<String>,
    pub max_players: Option<u32>,
    /// A `data:image/png;base64,...` URL of a 64×64 PNG.
    pub favicon: Option<Arc<str>>,
    pub sample: Option<Vec<String>>,
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
                let mut document = Document::new(i32::MIN, u64::MAX, u64::MAX, "");
                document.apply(&entry, maintenance);
                document
                    .packet()
                    .map_err(|error| format!("{path}: {error}"))?;
            }
        }
        Ok(())
    }
}

/// Fields an `on_status` hook may replace for one ping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusOverride {
    pub motd: Option<Json>,
    pub version: Option<String>,
    pub protocol: Option<i32>,
    pub online: Option<u32>,
    pub max_players: Option<u32>,
    pub sample: Option<Vec<String>>,
    pub favicon: Option<Arc<str>>,
}

/// Owned inputs for `on_status`, available before any backend contact.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub version: String,
    pub protocol: i32,
    pub online: u64,
    pub max_players: u64,
    pub sample: Vec<String>,
    pub favicon: Option<Arc<str>>,
}

impl Document {
    pub fn new(protocol: i32, online: u64, max_players: u64, motd: &str) -> Self {
        Self {
            motd: Json::String(motd.to_owned()),
            version: "Rift".into(),
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
        next.packet()?;
        *self = next;
        Ok(())
    }

    pub fn packet(&self) -> io::Result<Packet> {
        let description = match &self.motd {
            Json::String(text) => json!({ "text": text }),
            component => component.clone(),
        };
        let mut players = json!({ "max": self.max_players, "online": self.online });
        if !self.sample.is_empty() {
            players["sample"] = self
                .sample
                .iter()
                .map(|name| json!({ "name": name, "id": SAMPLE_ID }))
                .collect();
        }
        let mut response = json!({
            "version": { "name": self.version, "protocol": self.protocol },
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
            let mut nodes = 0;
            let component = component(value, 0, &mut nodes, path)?;
            if component.as_object().is_some_and(serde_json::Map::is_empty) {
                return Err(format!("{path}: chat component must not be empty"));
            }
            component
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

pub(crate) fn text(value: Value, path: &str) -> Result<String, String> {
    match value {
        Value::String(text) if text.as_bytes().len() <= MAX_TEXT_BYTES => utf8(&text, path),
        _ => Err(format!(
            "{path}: expected a string of at most {MAX_TEXT_BYTES} bytes"
        )),
    }
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
pub(crate) fn sample(value: Value, path: &str) -> Result<Vec<String>, String> {
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
    if bytes.len() > MAX_FAVICON_BYTES {
        return Err(fail(&"favicon exceeds 16 KiB"));
    }
    if !bytes.starts_with(PNG_SIGNATURE) || bytes.get(12..16) != Some(b"IHDR") {
        return Err(fail(&"favicon must be a PNG image"));
    }
    let dimension =
        |range: std::ops::Range<usize>| u32::from_be_bytes(bytes[range].try_into().unwrap());
    if bytes.len() < 24 || dimension(16..20) != 64 || dimension(20..24) != 64 {
        return Err(fail(&"favicon must be 64×64 pixels"));
    }
    Ok(format!("{FAVICON_PREFIX}{}", base64(&bytes)).into())
}

/// A hook-supplied favicon: an already encoded PNG data URL.
pub(crate) fn favicon_url(value: Value, path: &str) -> Result<Arc<str>, String> {
    let limit = FAVICON_PREFIX.len() + MAX_FAVICON_BYTES.div_ceil(3) * 4;
    let valid = match &value {
        Value::String(url) => {
            let url = url.as_bytes();
            url.len() <= limit
                && url.starts_with(FAVICON_PREFIX.as_bytes())
                && url.len() > FAVICON_PREFIX.len()
                && url[FAVICON_PREFIX.len()..]
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
        }
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

fn status_override(value: Value) -> Result<Option<StatusOverride>, String> {
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
        }
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
            assert_eq!(resolved.version.as_deref(), Some("Network"));
        }
    }

    #[test]
    fn document_renders_components_sample_and_favicon_and_rejects_oversized_overrides() {
        let mut document = Document::new(774, 3, 100, "Rift");
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
        let response = decode(&document.packet().unwrap());
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
}
