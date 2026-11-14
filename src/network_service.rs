//! The network API for backends, as `rift.network.<op>` requests on the
//! message bus: which servers are up, who is online where, and moving a
//! player to a server or to any instance of a service group.
//!
//! A lobby selector needs exactly this, and before it had two bad choices:
//! the BungeeCord plugin channel, which needs a carrier player and has no
//! replies for transfers, or `rift.control` and `rift.players`, which need
//! `control = true` and `players = true` and so let one backend stop servers
//! and kick anyone. A login Rift issues to an instance can hold neither.
//!
//! These subjects need only a publish grant, which the `backend` preset
//! includes. Reads answer for the whole network. `connect` is scoped to the
//! caller: a login named `server.<name>` moves the players on `<name>`, and
//! only a login with `players = true` (or Rift's own plugins and
//! configuration script) moves anyone else.
use serde_json::{Map, Value};

/// Every `rift.network.*` subject starts with this.
pub const PREFIX: &str = "rift.network";
/// Payloads are a few names.
pub const MAX_PAYLOAD: usize = 8 * 1024;

/// The operations.
pub const OPERATIONS: &[&str] = &["servers", "players", "player", "connect"];

/// Where `connect` sends a player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// A configured server; a service group's name is accepted as well, as
    /// everywhere else a server is named.
    Server(String),
    /// Any instance of this service group: Rift picks one as it does for a
    /// route to the group. A name that is not a service group is refused.
    Group(String),
}

impl Destination {
    /// The server or group name.
    pub fn name(&self) -> &str {
        match self {
            Self::Server(name) | Self::Group(name) => name,
        }
    }
}

/// One decoded `rift.network` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Servers that are up: all, one, or a service group's instances.
    Servers { server: Option<String> },
    /// Online players: all, or those on a server or service group.
    Players { server: Option<String> },
    /// One player by name or UUID.
    Player { player: String },
    /// Move a player.
    Connect {
        player: String,
        destination: Destination,
    },
}

impl Request {
    /// Decode `operation` (the subject after `rift.network.`) and its JSON
    /// object payload. Unknown operations and fields are errors.
    pub fn decode(operation: &str, payload: &[u8]) -> Result<Self, String> {
        if payload.len() > MAX_PAYLOAD {
            return Err("network payload exceeds 8 KiB".into());
        }
        let value: Value = if payload.iter().all(u8::is_ascii_whitespace) {
            Value::Object(Map::new())
        } else {
            serde_json::from_slice(payload).map_err(|_| "expected a JSON object")?
        };
        let object = value.as_object().ok_or("expected a JSON object")?;
        let fields: &[&str] = match operation {
            "servers" | "players" => &["server"],
            "player" => &["player"],
            "connect" => &["player", "server", "group"],
            _ => return Err(format!("unknown network operation {operation:?}")),
        };
        if let Some(field) = object.keys().find(|key| !fields.contains(&key.as_str())) {
            return Err(format!("unknown field {field:?} for network.{operation}"));
        }
        let name = |field: &str, required: bool| match object.get(field) {
            None | Some(Value::Null) if !required => Ok(None),
            Some(Value::String(value)) if !value.is_empty() && value.len() <= 256 => {
                Ok(Some(value.clone()))
            }
            _ => Err(format!("{field} must be a string of 1..=256 bytes")),
        };
        let required = |field: &str| name(field, true).map(Option::unwrap_or_default);
        Ok(match operation {
            "servers" => Self::Servers {
                server: name("server", false)?,
            },
            "players" => Self::Players {
                server: name("server", false)?,
            },
            "player" => Self::Player {
                player: required("player")?,
            },
            "connect" => Self::Connect {
                player: required("player")?,
                destination: match (name("server", false)?, name("group", false)?) {
                    (Some(server), None) => Destination::Server(server),
                    (None, Some(group)) => Destination::Group(group),
                    _ => return Err("connect takes exactly one of server and group".into()),
                },
            },
            _ => unreachable!("checked above"),
        })
    }
}

/// Whether the login `sender` may move a player who is on `server`.
///
/// Requests without a sender come from the configuration script, and those
/// with a `:` in it from a persistent plugin (see
/// [`crate::players_service::plugin_sender`]); both already have the whole
/// `players` interface. A remote login speaks for the players of the server
/// it is named after, or for everyone when `unrestricted` (its principal has
/// `players = true`).
pub fn may_connect(sender: Option<&str>, server: &str, unrestricted: bool) -> bool {
    match sender {
        None => true,
        Some(local) if local.contains(':') => true,
        Some(login) => {
            unrestricted
                || login
                    .strip_prefix(crate::permissions::control::BACKEND_PRINCIPAL_PREFIX)
                    .is_some_and(|name| name == server)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_name_servers_groups_and_players() {
        assert_eq!(
            Request::decode("servers", b"").unwrap(),
            Request::Servers { server: None }
        );
        assert_eq!(
            Request::decode("players", br#"{"server":"lobby"}"#).unwrap(),
            Request::Players {
                server: Some("lobby".into())
            }
        );
        assert_eq!(
            Request::decode("connect", br#"{"player":"Alice","group":"lobby"}"#).unwrap(),
            Request::Connect {
                player: "Alice".into(),
                destination: Destination::Group("lobby".into())
            }
        );
        for (operation, payload, message) in [
            ("connect", r#"{"player":"A"}"#, "exactly one of"),
            (
                "connect",
                r#"{"player":"A","server":"x","group":"y"}"#,
                "exactly one of",
            ),
            ("connect", r#"{"server":"x"}"#, "player must be a string"),
            ("player", "{}", "player must be a string"),
            ("servers", r#"{"target":"x"}"#, "unknown field"),
            ("stop", "{}", "unknown network operation"),
            ("servers", "[]", "expected a JSON object"),
        ] {
            let error = Request::decode(operation, payload.as_bytes()).unwrap_err();
            assert!(error.contains(message), "{operation} {payload}: {error}");
        }
    }

    #[test]
    fn a_backend_login_moves_only_its_own_players() {
        assert!(may_connect(Some("server.lobby-1"), "lobby-1", false));
        assert!(!may_connect(Some("server.lobby-1"), "lobby-2", false));
        assert!(!may_connect(Some("server.lobby"), "lobby-1", false));
        assert!(!may_connect(Some("lobby-1"), "lobby-1", false));
        assert!(may_connect(Some("friends"), "lobby-1", true));
        assert!(may_connect(Some("plugin:selector"), "lobby-1", false));
        assert!(may_connect(None, "lobby-1", false));
    }
}
