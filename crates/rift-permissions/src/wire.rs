//! Bus subjects and JSON messages of the permission protocol, version
//! [`PROTOCOL_VERSION`]. `docs/permissions-protocol.md` is the normative
//! description; every request is answered with a [`Reply`] envelope.

use crate::{
    context::ContextSet,
    node::{Node, Payload},
    subject::{Group, Limits, Uuid},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::fmt;

pub use crate::{
    change::{Applied, Change, ChangeEvent},
    compiled::CompiledSubject,
};

/// Protocol version announced in `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Backend → proxy, REQUEST [`HelloRequest`] → [`HelloReply`].
pub const HELLO: &str = "rift.perms.hello";
/// Backend → proxy, REQUEST [`SubjectGetRequest`] → [`CompiledSubject`].
pub const SUBJECT_GET: &str = "rift.perms.subject.get";
/// Backend → proxy, REQUEST [`SubjectBatchRequest`] → [`SubjectBatchReply`].
pub const SUBJECT_BATCH: &str = "rift.perms.subject.batch";
/// Backend → proxy, REQUEST [`GroupsGetRequest`] → [`GroupsReply`].
pub const GROUPS_GET: &str = "rift.perms.groups.get";
/// Backend → proxy, REQUEST [`ApplyRequest`] → [`Applied`].
pub const APPLY: &str = "rift.perms.apply";
/// Proxy → all, PUBLISH [`ChangeEvent`] after every committed write.
pub const UPDATE: &str = "rift.perms.update";
/// Prefix of `rift.perms.update.<uuid>`: proxy → all, PUBLISH [`CompiledSubject`].
pub const UPDATE_PREFIX: &str = "rift.perms.update.";
/// Subscription pattern for every pushed snapshot.
pub const UPDATE_ALL: &str = "rift.perms.update.>";
/// Retained stream (and its subject) carrying every [`ChangeEvent`].
pub const CHANGES: &str = "rift.perms.changes";
/// Backend → proxy, PUBLISH_ACK [`RegisterRequest`].
pub const REGISTER: &str = "rift.perms.register";
/// Backend → proxy, REQUEST [`CommandRequest`] → [`CommandReply`].
pub const COMMAND: &str = "rift.perms.command";
/// Backend → proxy, PUBLISH [`ContextReport`].
pub const CONTEXT: &str = "rift.perms.context";
/// Proxy → all, PUBLISH [`ChangeEvent`], alongside the other `rift.events.*`.
pub const EVENTS: &str = "rift.events.permissions";

/// `rift.perms.update.<uuid>` for one player.
pub fn update_subject(uuid: Uuid) -> String {
    format!("{UPDATE_PREFIX}{uuid}")
}

/// The UUID of a `rift.perms.update.<uuid>` subject.
pub fn parse_update_subject(subject: &str) -> Option<Uuid> {
    let hex = subject.strip_prefix(UPDATE_PREFIX)?;
    (hex.len() == 32).then(|| Uuid::parse(hex).ok()).flatten()
}

/// Machine-readable error codes in [`ErrorReply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed JSON or a value that fails validation.
    InvalidRequest,
    /// The caller may not do this.
    Forbidden,
    /// A named group, track or player does not exist.
    NotFound,
    /// A store limit would be exceeded.
    Limit,
    /// The change conflicts with current state (for example a track move).
    Conflict,
    /// The proxy cannot serve the request right now.
    Unavailable,
    /// Anything else; also assumed when `code` is missing or unknown.
    #[serde(other)]
    Internal,
}

/// A failed request: `{"ok": false, "error": "message", "code": "not_found"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorReply {
    pub code: ErrorCode,
    pub error: String,
}

/// The reply envelope of every request, as on `rift.control.*`:
/// `{"ok": true, "data": ...}` or [`ErrorReply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply<T> {
    Ok(T),
    Err(ErrorReply),
}

impl<T> Reply<T> {
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Err(ErrorReply {
            code,
            error: message.into(),
        })
    }

    pub fn into_result(self) -> Result<T, ErrorReply> {
        match self {
            Self::Ok(data) => Ok(data),
            Self::Err(error) => Err(error),
        }
    }
}

impl<T> From<Result<T, ErrorReply>> for Reply<T> {
    fn from(result: Result<T, ErrorReply>) -> Self {
        result.map_or_else(Self::Err, Self::Ok)
    }
}

impl fmt::Display for ErrorReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for ErrorReply {}

impl<T: Serialize> Serialize for Reply<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(untagged)]
        enum Out<'a, T> {
            Ok {
                ok: bool,
                data: &'a T,
            },
            Err {
                ok: bool,
                error: &'a str,
                code: ErrorCode,
            },
        }
        match self {
            Self::Ok(data) => Out::Ok { ok: true, data },
            Self::Err(e) => Out::<T>::Err {
                ok: false,
                error: &e.error,
                code: e.code,
            },
        }
        .serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Reply<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct In<T> {
            ok: bool,
            data: Option<T>,
            error: Option<String>,
            code: Option<ErrorCode>,
        }
        let raw = In::<T>::deserialize(deserializer)?;
        if raw.ok {
            raw.data
                .map(Self::Ok)
                .ok_or_else(|| de::Error::missing_field("data"))
        } else {
            Ok(Self::Err(ErrorReply {
                code: raw.code.unwrap_or(ErrorCode::Internal),
                error: raw.error.unwrap_or_default(),
            }))
        }
    }
}

/// `rift.perms.hello` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloRequest {
    /// The backend's server name, the `server` context it fills.
    pub server: String,
    /// The last revision the backend's replica saw; 0 when empty.
    pub revision: u64,
    pub client_version: String,
}

/// `rift.perms.hello` reply data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReply {
    /// The proxy's current revision; a different local revision means resync.
    pub revision: u64,
    pub protocol: u32,
    /// The proxy's clock (Unix seconds), to spot skew against expiries.
    pub now: u64,
    pub limits: Limits,
    /// Context keys backends may report on `rift.perms.context`.
    pub external_contexts: Vec<String>,
}

/// `rift.perms.subject.get` request; the reply data is a [`CompiledSubject`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectGetRequest {
    pub uuid: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

/// `rift.perms.subject.batch` request: at most [`Limits::batch`] UUIDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectBatchRequest {
    pub uuids: Vec<Uuid>,
}

/// `rift.perms.subject.batch` reply data, one subject per requested UUID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectBatchReply {
    /// The proxy's revision when the batch was built.
    pub revision: u64,
    pub subjects: Vec<CompiledSubject>,
}

/// `rift.perms.groups.get` request. `{}` lists every group without
/// permission nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupsGetRequest {
    /// Only these groups (at most 64); all when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
    /// Also include permission nodes; only allowed together with `names`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub permissions: bool,
}

/// One group in [`GroupsReply`]: its columns and its `inherit`, `prefix`,
/// `suffix` and `meta` nodes, plus `permission` nodes when requested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub name: String,
    pub weight: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub nodes: Vec<Node>,
}

impl GroupInfo {
    /// Summarise `group`, with permission nodes only when `permissions`.
    pub fn from_group(group: &Group, permissions: bool) -> Self {
        let nodes = group
            .nodes
            .iter()
            .filter(|node| match node.payload {
                Payload::Permission { .. } => permissions,
                Payload::Weight { .. } | Payload::DisplayName { .. } => false,
                _ => true,
            })
            .cloned()
            .collect();
        Self {
            name: group.name.clone(),
            weight: group.weight,
            display_name: group.display_name.clone(),
            nodes,
        }
    }
}

/// `rift.perms.groups.get` reply data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupsReply {
    pub revision: u64,
    pub groups: Vec<GroupInfo>,
}

/// `rift.perms.apply` request. The proxy records the actor itself: the
/// player named by `actor_uuid` (who must hold the matching `rift.perms.*`
/// node and be on the sending backend), otherwise `backend:<identity>`, which
/// requires a principal with `control = true`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyRequest {
    pub change: Change,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_uuid: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A plugin-declared default for a node nobody has set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionDefault {
    /// `"true"`: everyone.
    True,
    /// `"false"`: nobody.
    False,
    /// `"op"`, or `"op:<level>"` for Pumpkin's op levels 0 to 4.
    Op(Option<u8>),
    /// `"not_op"`: everyone except operators.
    NotOp,
}

impl fmt::Display for PermissionDefault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::True => f.write_str("true"),
            Self::False => f.write_str("false"),
            Self::Op(None) => f.write_str("op"),
            Self::Op(Some(level)) => write!(f, "op:{level}"),
            Self::NotOp => f.write_str("not_op"),
        }
    }
}

impl std::str::FromStr for PermissionDefault {
    type Err = crate::Error;
    fn from_str(text: &str) -> Result<Self, crate::Error> {
        match text {
            "true" => Ok(Self::True),
            "false" => Ok(Self::False),
            "op" => Ok(Self::Op(None)),
            "not_op" => Ok(Self::NotOp),
            _ => match text
                .strip_prefix("op:")
                .and_then(|level| level.parse().ok())
            {
                Some(level @ 0..=4) => Ok(Self::Op(Some(level))),
                _ => Err(crate::error::invalid(
                    "permission default",
                    text,
                    "expected true, false, op, op:<0-4> or not_op",
                )),
            },
        }
    }
}

impl Serialize for PermissionDefault {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PermissionDefault {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<str>::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}

/// One plugin-registered permission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredPermission {
    pub node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub default: PermissionDefault,
    /// The plugin on the backend that declared it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
}

/// `rift.perms.register` payload: a JSON array of at most 4096 entries.
pub type RegisterRequest = Vec<RegisteredPermission>;

/// `rift.perms.command` request: a forwarded `/perms` command line split
/// into arguments, without the command name. `actor_uuid` is absent for the
/// backend console.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_uuid: Option<Uuid>,
    pub args: Vec<String>,
}

/// `rift.perms.command` reply data: Adventure component JSON to show the
/// sender and the proxy's revision after the command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReply {
    pub revision: u64,
    pub component: serde_json::Value,
}

/// `rift.perms.context` payload: custom contexts for a player on the
/// sending backend. Replaces that player's previous report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReport {
    pub uuid: Uuid,
    pub contexts: ContextSet,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reply_envelope() {
        let ok: Reply<u64> = Reply::Ok(5);
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({"ok": true, "data": 5})
        );
        let err: Reply<u64> = Reply::error(ErrorCode::NotFound, "no such group");
        let json = json!({"ok": false, "error": "no such group", "code": "not_found"});
        assert_eq!(serde_json::to_value(&err).unwrap(), json);
        assert_eq!(serde_json::from_value::<Reply<u64>>(json).unwrap(), err);
        let legacy: Reply<u64> =
            serde_json::from_value(json!({"ok": false, "error": "x"})).unwrap();
        assert_eq!(legacy.into_result().unwrap_err().code, ErrorCode::Internal);
        let unknown: Reply<u64> =
            serde_json::from_value(json!({"ok": false, "error": "x", "code": "new_code"})).unwrap();
        assert_eq!(unknown.into_result().unwrap_err().code, ErrorCode::Internal);
        assert!(serde_json::from_value::<Reply<u64>>(json!({"ok": true})).is_err());
    }

    #[test]
    fn update_subjects() {
        let uuid = Uuid::from_u128(0xabc);
        let subject = update_subject(uuid);
        assert_eq!(
            subject,
            "rift.perms.update.00000000000000000000000000000abc"
        );
        assert_eq!(parse_update_subject(&subject), Some(uuid));
        assert_eq!(parse_update_subject(UPDATE), None);
        assert_eq!(parse_update_subject("rift.perms.update.x"), None);
    }

    #[test]
    fn defaults() {
        for text in ["true", "false", "op", "op:2", "not_op"] {
            assert_eq!(text.parse::<PermissionDefault>().unwrap().to_string(), text);
        }
        assert!("op:5".parse::<PermissionDefault>().is_err());
        assert!("yes".parse::<PermissionDefault>().is_err());
    }

    #[test]
    fn group_info_hides_permissions_unless_asked() {
        let mut group = Group::new("vip").unwrap();
        group.nodes = vec![
            Node::permission("fly", true).unwrap(),
            Node::prefix(1, "[V]").unwrap(),
            Node::weight(3),
        ];
        assert_eq!(GroupInfo::from_group(&group, false).nodes.len(), 1);
        assert_eq!(GroupInfo::from_group(&group, true).nodes.len(), 2);
    }
}
