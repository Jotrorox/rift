//! JSON messages between the controller and `rift node` agents.
//!
//! | Subject | Direction | Purpose |
//! | --- | --- | --- |
//! | `rift.node.<n>.cmd` | controller → agent | request/reply [`Command`] |
//! | `rift.node.<n>.state` | agent → controller | [`NodeState`] every 2 s and on change |
use std::{io, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::InstanceStorage;

/// Agents publish their state at least this often.
pub const STATE_INTERVAL: Duration = Duration::from_secs(2);
/// A node without a state snapshot for this long is `unreachable`.
pub const UNREACHABLE_AFTER: Duration = Duration::from_secs(10);
/// Every command payload, reply and snapshot stays below the bus limit.
pub const MAX_MESSAGE: usize = 1024 * 1024;
/// Remote file reads and writes through the bus, before base64 encoding.
pub const MAX_REMOTE_FILE: usize = 512 * 1024;
pub const STATE_PATTERN: &str = "rift.node.*.state";

pub fn command_subject(node: &str) -> String {
    format!("rift.node.{node}.cmd")
}

pub fn state_subject(node: &str) -> String {
    format!("rift.node.{node}.state")
}

/// The node named by a `rift.node.<n>.state` subject.
pub fn state_node(subject: &str) -> Option<&str> {
    subject
        .strip_prefix("rift.node.")?
        .strip_suffix(".state")
        .filter(|node| !node.is_empty() && !node.contains('.'))
}

/// Subject grants for a node's own principal. Rift additionally refuses
/// `rift.node.*` traffic from any other remote identity.
pub fn principal_grants(node: &str) -> (Vec<String>, Vec<String>) {
    (
        vec![state_subject(node), "_INBOX.*".into()],
        vec![command_subject(node)],
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    /// Spawn a server; replies without waiting for readiness.
    Start(Start),
    /// Graceful `stop` on stdin, then kill after `timeout_ms` (0 kills at once).
    Stop {
        server: String,
        timeout_ms: u64,
    },
    Console {
        server: String,
        command: String,
    },
    /// Up to 64 KiB of `rift-server.log`, with the same cursor rules as local logs.
    Logs {
        server: String,
        directory: PathBuf,
        cursor: Option<u64>,
    },
    /// Materialize an instance directory from templates; see [`Prepare`].
    Prepare(Box<Prepare>),
    /// Delete a disposable instance directory after its process stopped.
    Remove {
        server: String,
        directory: PathBuf,
        storage: InstanceStorage,
    },
    /// Browse or edit one server directory.
    Files(Files),
    /// Sent after every (re)connection: stop processes and delete disposable
    /// instance directories that the controller no longer assigns to this node.
    Reconcile {
        servers: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    pub server: String,
    pub command: Vec<String>,
    /// Relative to the agent's `--root`.
    pub directory: PathBuf,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Software {
    pub kind: String,
    pub version: String,
    pub url: Option<String>,
    pub sha256: Option<String>,
}

impl From<&crate::software::Software> for Software {
    fn from(software: &crate::software::Software) -> Self {
        Self {
            kind: software.kind.as_str().into(),
            version: software.version.clone(),
            url: software.url.clone(),
            sha256: software.sha256.clone(),
        }
    }
}

impl TryFrom<&Software> for crate::software::Software {
    type Error = io::Error;
    fn try_from(software: &Software) -> io::Result<Self> {
        let converted = Self {
            kind: crate::software::Kind::parse(&software.kind).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unknown software type")
            })?,
            version: software.version.clone(),
            url: software.url.clone(),
            sha256: software.sha256.clone(),
        };
        converted
            .validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(converted)
    }
}

/// Everything an agent needs to create an instance directory without access
/// to the controller's configuration. Template paths are relative to the
/// controller's `template_storage` and fetched over `/api/files/templates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prepare {
    pub server: String,
    pub group: String,
    pub directory: PathBuf,
    pub template: Option<String>,
    pub storage: InstanceStorage,
    pub port: u16,
    /// `server-ip` in `server.properties`: the node address the proxy uses.
    pub bind: String,
    pub software: Option<Software>,
    pub server_jar: Option<String>,
    pub configs: Vec<String>,
    pub plugins: Vec<String>,
    pub map: Option<String>,
    pub accept_eula: bool,
    /// Velocity secret and online mode for Paper-family software.
    pub forwarding: Option<(String, bool)>,
    /// `(name, value, replace)`: replaced settings always win, others are defaults.
    pub properties: Vec<(String, String, bool)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Files {
    pub server: String,
    pub directory: PathBuf,
    pub method: String,
    pub path: String,
    /// `PUT` creates a folder instead of writing a file.
    #[serde(default)]
    pub folder: bool,
    /// Base64 file contents for `PUT`.
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    /// Random per agent process: a new value means the agent restarted.
    pub agent: String,
    pub version: String,
    /// Increases on every process change, so stale snapshots are recognizable.
    pub revision: u64,
    pub servers: Vec<ProcessState>,
    /// Recent unexpected exits, so the controller can report why.
    #[serde(default)]
    pub exits: Vec<ExitState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessState {
    pub name: String,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitState {
    pub name: String,
    pub status: String,
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard padded base64, for file contents inside JSON commands.
pub fn encode_base64(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                text.push(BASE64[(value >> (18 - 6 * index)) as usize & 63] as char);
            } else {
                text.push('=');
            }
        }
    }
    text
}

pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut output = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let last = index + 1 == bytes.len() / 4;
        let padding = chunk.iter().rev().take_while(|&&byte| byte == b'=').count();
        if padding > 2 || (padding > 0 && !last) {
            return None;
        }
        let mut value = 0u32;
        for &byte in &chunk[..4 - padding] {
            let digit = BASE64.iter().position(|&candidate| candidate == byte)? as u32;
            value = (value << 6) | digit;
        }
        value <<= 6 * padding as u32;
        let decoded = [(value >> 16) as u8, (value >> 8) as u8, value as u8];
        output.extend_from_slice(&decoded[..3 - padding]);
    }
    Some(output)
}

/// `{"ok":true,"result":...}` or `{"ok":false,"error":...,"kind":...}`.
pub fn reply(result: io::Result<Value>) -> Value {
    match result {
        Ok(value) => json!({"ok": true, "result": value}),
        Err(error) => json!({
            "ok": false,
            "error": error.to_string(),
            "kind": kind_name(error.kind()),
        }),
    }
}

pub fn parse_reply(payload: &[u8]) -> io::Result<Value> {
    let value: Value = serde_json::from_slice(payload)
        .map_err(|error| io::Error::other(format!("invalid node reply: {error}")))?;
    if value["ok"] == true {
        return Ok(value["result"].clone());
    }
    let message = value["error"].as_str().unwrap_or("node command failed");
    Err(io::Error::new(
        kind(value["kind"].as_str().unwrap_or("")),
        message.to_owned(),
    ))
}

fn kind_name(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "not_found",
        io::ErrorKind::InvalidInput => "invalid",
        io::ErrorKind::PermissionDenied => "denied",
        io::ErrorKind::AlreadyExists => "exists",
        io::ErrorKind::AddrInUse => "address_in_use",
        io::ErrorKind::NotConnected => "not_connected",
        io::ErrorKind::ResourceBusy => "busy",
        io::ErrorKind::TimedOut => "timed_out",
        io::ErrorKind::FileTooLarge => "too_large",
        io::ErrorKind::IsADirectory => "is_directory",
        io::ErrorKind::Unsupported => "unsupported",
        _ => "other",
    }
}

fn kind(name: &str) -> io::ErrorKind {
    match name {
        "not_found" => io::ErrorKind::NotFound,
        "invalid" => io::ErrorKind::InvalidInput,
        "denied" => io::ErrorKind::PermissionDenied,
        "exists" => io::ErrorKind::AlreadyExists,
        "address_in_use" => io::ErrorKind::AddrInUse,
        "not_connected" => io::ErrorKind::NotConnected,
        "busy" => io::ErrorKind::ResourceBusy,
        "timed_out" => io::ErrorKind::TimedOut,
        "too_large" => io::ErrorKind::FileTooLarge,
        "is_directory" => io::ErrorKind::IsADirectory,
        "unsupported" => io::ErrorKind::Unsupported,
        _ => io::ErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_commands_and_replies_round_trip() {
        assert_eq!(state_node("rift.node.node1.state"), Some("node1"));
        assert_eq!(state_node("rift.node.a.b.state"), None);
        assert_eq!(state_node("rift.node..state"), None);
        let command = Command::Stop {
            server: "game-1".into(),
            timeout_ms: 5,
        };
        let text = serde_json::to_string(&command).unwrap();
        assert_eq!(text, r#"{"op":"stop","server":"game-1","timeout_ms":5}"#);
        assert_eq!(serde_json::from_str::<Command>(&text).unwrap(), command);
        let reply_value = reply(Err(io::Error::new(io::ErrorKind::AddrInUse, "busy port")));
        let error = parse_reply(reply_value.to_string().as_bytes()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(error.to_string(), "busy port");
        assert_eq!(
            parse_reply(reply(Ok(json!({"pid": 7}))).to_string().as_bytes()).unwrap()["pid"],
            7
        );
        for sample in [&b""[..], b"f", b"fo", b"foo", b"foob", b"\x00\xff\x10hello"] {
            let text = encode_base64(sample);
            assert_eq!(decode_base64(&text).unwrap(), sample, "{text}");
        }
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        for invalid in ["Zm8", "Z===", "Zm=v", "Zm8=Zm8=", "Zm.v"] {
            assert!(decode_base64(invalid).is_none(), "{invalid}");
        }
    }
}
