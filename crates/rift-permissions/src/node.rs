//! Nodes: pattern validation, the typed payload, and the mapping onto the
//! `perm_node` storage row.

use crate::{
    context::ContextSet,
    error::{Error, invalid},
    expired,
    subject::normalize_group,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, str::FromStr};

/// Longest permission pattern in bytes.
pub const MAX_NODE_LEN: usize = 256;
/// Longest meta key in bytes.
pub const MAX_META_KEY_LEN: usize = 64;
/// Longest prefix, suffix or display name in bytes.
pub const MAX_TEXT_LEN: usize = 256;
/// Longest meta value in bytes.
pub const MAX_META_VALUE_LEN: usize = 1024;

fn node_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b':' | b'-')
}

/// Whether `node` is a normalised pattern: dot-separated, non-empty segments
/// of `[a-z0-9_:-]`, at most 256 bytes, with `*` only as a whole trailing
/// segment (`a.b`, `a.*`, `*`). Pumpkin's `plugin:node` fits as written.
pub fn valid_node(node: &str) -> bool {
    if node == "*" {
        return true;
    }
    let body = node.strip_suffix(".*").unwrap_or(node);
    node.len() <= MAX_NODE_LEN
        && body
            .split('.')
            .all(|segment| !segment.is_empty() && segment.bytes().all(node_byte))
}

/// Lowercase ASCII letters, as Bukkit does, then validate with [`valid_node`].
pub fn normalize_node(node: &str) -> Result<String, Error> {
    let lower = node.to_ascii_lowercase();
    if valid_node(&lower) {
        Ok(lower)
    } else {
        Err(invalid(
            "permission node",
            node,
            "use dot-separated segments of [a-z0-9_:-], at most 256 bytes, with * only as a whole trailing segment",
        ))
    }
}

/// Lowercase and validate a meta key: 1 to 64 bytes of `[a-z0-9_.:-]`.
pub fn normalize_meta_key(key: &str) -> Result<String, Error> {
    let lower = key.to_ascii_lowercase();
    if lower.is_empty()
        || lower.len() > MAX_META_KEY_LEN
        || !lower.bytes().all(|b| node_byte(b) || b == b'.')
    {
        return Err(invalid(
            "meta key",
            key,
            "use 1 to 64 bytes of [a-z0-9_.:-]",
        ));
    }
    Ok(lower)
}

fn check_text(field: &'static str, text: &str, max: usize) -> Result<(), Error> {
    if text.len() > max || text.chars().any(char::is_control) {
        return Err(invalid(
            field,
            text,
            "too long or contains control characters",
        ));
    }
    Ok(())
}

/// Node kinds with the stable codes of the `perm_node.kind` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Permission = 0,
    Inherit = 1,
    Prefix = 2,
    Suffix = 3,
    Meta = 4,
    Weight = 5,
    DisplayName = 6,
}

impl NodeKind {
    pub const ALL: [Self; 7] = [
        Self::Permission,
        Self::Inherit,
        Self::Prefix,
        Self::Suffix,
        Self::Meta,
        Self::Weight,
        Self::DisplayName,
    ];

    pub const fn code(self) -> u8 {
        self as u8
    }

    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code)).copied()
    }

    /// The JSON and command spelling: `permission`, `display_name`, ...
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Inherit => "inherit",
            Self::Prefix => "prefix",
            Self::Suffix => "suffix",
            Self::Meta => "meta",
            Self::Weight => "weight",
            Self::DisplayName => "display_name",
        }
    }

    /// `weight` and `display_name` are valid only on groups.
    pub const fn group_only(self) -> bool {
        matches!(self, Self::Weight | Self::DisplayName)
    }
}

impl fmt::Display for NodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for NodeKind {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text)
            .ok_or_else(|| invalid("node kind", text, "unknown kind"))
    }
}

/// What a node says. Serialised inline in [`Node`] with a `kind` tag.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    /// Grant (`true`) or deny (`false`) a pattern.
    Permission { key: String, value: bool },
    /// Inherit a group, or with `false` block it from every path.
    Inherit { group: String, value: bool },
    /// Chat prefix; the highest priority wins.
    Prefix { priority: i32, text: String },
    /// Chat suffix; the highest priority wins.
    Suffix { priority: i32, text: String },
    /// Arbitrary key/value metadata.
    Meta { key: String, value: String },
    /// Group weight, mirrored in [`crate::Group::weight`].
    Weight { weight: i32 },
    /// Group display name, mirrored in [`crate::Group::display_name`].
    DisplayName { name: String },
}

impl Payload {
    pub fn kind(&self) -> NodeKind {
        match self {
            Self::Permission { .. } => NodeKind::Permission,
            Self::Inherit { .. } => NodeKind::Inherit,
            Self::Prefix { .. } => NodeKind::Prefix,
            Self::Suffix { .. } => NodeKind::Suffix,
            Self::Meta { .. } => NodeKind::Meta,
            Self::Weight { .. } => NodeKind::Weight,
            Self::DisplayName { .. } => NodeKind::DisplayName,
        }
    }

    /// The storage key: the pattern, the group name, the decimal priority of
    /// a prefix or suffix, the meta key, or the kind name for `weight` and
    /// `display_name`. One node exists per (kind, key, contexts).
    pub fn key(&self) -> String {
        match self {
            Self::Permission { key, .. } | Self::Meta { key, .. } => key.clone(),
            Self::Inherit { group, .. } => group.clone(),
            Self::Prefix { priority, .. } | Self::Suffix { priority, .. } => priority.to_string(),
            Self::Weight { .. } | Self::DisplayName { .. } => self.kind().as_str().to_owned(),
        }
    }

    /// The storage value: `true`/`false`, the text, or the decimal weight.
    pub fn value(&self) -> String {
        match self {
            Self::Permission { value, .. } | Self::Inherit { value, .. } => value.to_string(),
            Self::Prefix { text, .. } | Self::Suffix { text, .. } => text.clone(),
            Self::Meta { value, .. } => value.clone(),
            Self::Weight { weight } => weight.to_string(),
            Self::DisplayName { name } => name.clone(),
        }
    }

    fn normalized(self) -> Result<Self, Error> {
        Ok(match self {
            Self::Permission { key, value } => Self::Permission {
                key: normalize_node(&key)?,
                value,
            },
            Self::Inherit { group, value } => Self::Inherit {
                group: normalize_group(&group)?,
                value,
            },
            Self::Prefix { priority, text } => {
                check_text("prefix", &text, MAX_TEXT_LEN)?;
                Self::Prefix { priority, text }
            }
            Self::Suffix { priority, text } => {
                check_text("suffix", &text, MAX_TEXT_LEN)?;
                Self::Suffix { priority, text }
            }
            Self::Meta { key, value } => {
                check_text("meta value", &value, MAX_META_VALUE_LEN)?;
                Self::Meta {
                    key: normalize_meta_key(&key)?,
                    value,
                }
            }
            Self::Weight { weight } => Self::Weight { weight },
            Self::DisplayName { name } => {
                if name.is_empty() {
                    return Err(invalid("display name", &name, "must not be empty"));
                }
                check_text("display name", &name, MAX_TEXT_LEN)?;
                Self::DisplayName { name }
            }
        })
    }
}

/// Where a node came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// Edited at runtime (`store`).
    #[default]
    Store,
    /// Mirrored from `extensions.groups` in Lua (`config`).
    Config,
    /// Written by a LuckPerms import (`import`).
    Import,
    /// Registered by a backend plugin or identity (`plugin:<name>`).
    Plugin(String),
}

/// Names after a `prefix:` in actor and source strings: 1 to 96 ASCII letters,
/// digits, `_`, `.`, `-` or `:`.
pub(crate) fn valid_label(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 96
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b':'))
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store => f.write_str("store"),
            Self::Config => f.write_str("config"),
            Self::Import => f.write_str("import"),
            Self::Plugin(name) => write!(f, "plugin:{name}"),
        }
    }
}

impl FromStr for Source {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Error> {
        match text {
            "store" => Ok(Self::Store),
            "config" => Ok(Self::Config),
            "import" => Ok(Self::Import),
            _ => match text.strip_prefix("plugin:") {
                Some(name) if valid_label(name) => Ok(Self::Plugin(name.to_owned())),
                _ => Err(invalid(
                    "node source",
                    text,
                    "expected store, config, import or plugin:<name>",
                )),
            },
        }
    }
}

impl Serialize for Source {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<str>::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}

/// A node on a user or group.
///
/// JSON: the payload's fields with its `kind` tag, then `contexts` (omitted
/// when global), `expires_at` (Unix seconds, omitted when permanent) and
/// `source` (omitted when `store`):
///
/// ```json
/// {"kind": "permission", "key": "essentials.fly", "value": true,
///  "contexts": {"server": "survival"}, "expires_at": 1762800000}
/// ```
///
/// Constructors and deserialisation normalise and validate, so a `Node` from
/// either is valid; [`Node::normalized`] re-checks one built field by field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawNode")]
pub struct Node {
    #[serde(flatten)]
    pub payload: Payload,
    #[serde(default, skip_serializing_if = "ContextSet::is_empty")]
    pub contexts: ContextSet,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "is_store")]
    pub source: Source,
}

fn is_store(source: &Source) -> bool {
    *source == Source::Store
}

#[derive(Deserialize)]
struct RawNode {
    #[serde(flatten)]
    payload: Payload,
    #[serde(default)]
    contexts: ContextSet,
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    source: Source,
}

impl TryFrom<RawNode> for Node {
    type Error = Error;
    fn try_from(raw: RawNode) -> Result<Self, Error> {
        Node {
            payload: raw.payload,
            contexts: raw.contexts,
            expires_at: raw.expires_at,
            source: raw.source,
        }
        .normalized()
    }
}

impl Node {
    /// A global, permanent `store` node.
    pub fn new(payload: Payload) -> Result<Self, Error> {
        Ok(Self {
            payload: payload.normalized()?,
            contexts: ContextSet::new(),
            expires_at: None,
            source: Source::Store,
        })
    }

    pub fn permission(pattern: &str, value: bool) -> Result<Self, Error> {
        Self::new(Payload::Permission {
            key: pattern.to_owned(),
            value,
        })
    }

    pub fn inherit(group: &str, value: bool) -> Result<Self, Error> {
        Self::new(Payload::Inherit {
            group: group.to_owned(),
            value,
        })
    }

    pub fn prefix(priority: i32, text: &str) -> Result<Self, Error> {
        Self::new(Payload::Prefix {
            priority,
            text: text.to_owned(),
        })
    }

    pub fn suffix(priority: i32, text: &str) -> Result<Self, Error> {
        Self::new(Payload::Suffix {
            priority,
            text: text.to_owned(),
        })
    }

    pub fn meta(key: &str, value: &str) -> Result<Self, Error> {
        Self::new(Payload::Meta {
            key: key.to_owned(),
            value: value.to_owned(),
        })
    }

    pub fn weight(weight: i32) -> Self {
        Self::new(Payload::Weight { weight }).expect("weights are always valid")
    }

    pub fn display_name(name: &str) -> Result<Self, Error> {
        Self::new(Payload::DisplayName {
            name: name.to_owned(),
        })
    }

    /// Add one context pair.
    pub fn with_context(mut self, key: &str, value: &str) -> Result<Self, Error> {
        self.contexts.insert(key, value)?;
        self.check_contexts()?;
        Ok(self)
    }

    /// Replace the contexts.
    pub fn with_contexts(mut self, contexts: ContextSet) -> Result<Self, Error> {
        self.contexts = contexts;
        self.check_contexts()?;
        Ok(self)
    }

    /// Expire at `at` (Unix seconds).
    pub fn expiring_at(mut self, at: u64) -> Self {
        self.expires_at = Some(at);
        self
    }

    pub fn with_source(mut self, source: Source) -> Self {
        self.source = source;
        self
    }

    pub fn kind(&self) -> NodeKind {
        self.payload.kind()
    }

    /// The identity of the slot this node occupies.
    pub fn key(&self) -> NodeKey {
        NodeKey {
            kind: self.kind(),
            key: self.payload.key(),
            contexts: self.contexts.clone(),
        }
    }

    pub fn is_expired(&self, now: u64) -> bool {
        expired(self.expires_at, now)
    }

    /// Normalise the payload (lowercasing patterns, groups and meta keys) and
    /// validate every field.
    pub fn normalized(mut self) -> Result<Self, Error> {
        self.payload = self.payload.normalized()?;
        self.check_contexts()?;
        Ok(self)
    }

    fn check_contexts(&self) -> Result<(), Error> {
        if self.contexts.is_satisfiable() {
            Ok(())
        } else {
            Err(invalid(
                "node contexts",
                &self.contexts.to_string(),
                "a reserved key has more than one value, so the node could never apply",
            ))
        }
    }
}

/// The identity of a node slot: (kind, key, contexts), the `UNIQUE`
/// constraint of `perm_node`. Setting a node replaces the one in its slot.
///
/// JSON: `{"kind": "prefix", "key": "100", "contexts": {...}}`, `contexts`
/// omitted when global.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NodeKey {
    pub kind: NodeKind,
    pub key: String,
    #[serde(default, skip_serializing_if = "ContextSet::is_empty")]
    pub contexts: ContextSet,
}

impl NodeKey {
    /// Normalise `key` for `kind` (see [`Payload::key`]).
    pub fn new(kind: NodeKind, key: &str, contexts: ContextSet) -> Result<Self, Error> {
        let key = match kind {
            NodeKind::Permission => normalize_node(key)?,
            NodeKind::Inherit => normalize_group(key)?,
            NodeKind::Prefix | NodeKind::Suffix => key
                .parse::<i32>()
                .map_err(|_| invalid("priority", key, "expected a 32-bit integer"))?
                .to_string(),
            NodeKind::Meta => normalize_meta_key(key)?,
            NodeKind::Weight | NodeKind::DisplayName => {
                if key != kind.as_str() {
                    return Err(invalid("node key", key, "must equal the kind name"));
                }
                key.to_owned()
            }
        };
        Ok(Self {
            kind,
            key,
            contexts,
        })
    }

    pub fn normalized(self) -> Result<Self, Error> {
        Self::new(self.kind, &self.key, self.contexts)
    }
}

/// A node as the `perm_node` row stores it. `contexts` is the canonical
/// string and `priority` is set for prefixes and suffixes only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub kind: NodeKind,
    pub key: String,
    pub value: String,
    pub priority: Option<i32>,
    pub contexts: String,
    pub expires_at: Option<u64>,
    pub source: String,
}

impl From<&Node> for NodeRow {
    fn from(node: &Node) -> Self {
        let priority = match node.payload {
            Payload::Prefix { priority, .. } | Payload::Suffix { priority, .. } => Some(priority),
            _ => None,
        };
        Self {
            kind: node.kind(),
            key: node.payload.key(),
            value: node.payload.value(),
            priority,
            contexts: node.contexts.to_string(),
            expires_at: node.expires_at,
            source: node.source.to_string(),
        }
    }
}

impl TryFrom<NodeRow> for Node {
    type Error = Error;
    fn try_from(row: NodeRow) -> Result<Self, Error> {
        let flag = |value: &str| match value {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(invalid("node value", value, "expected true or false")),
        };
        let priority = || {
            row.priority
                .or_else(|| row.key.parse().ok())
                .ok_or_else(|| {
                    invalid(
                        "priority",
                        &row.key,
                        "prefix and suffix rows need a priority",
                    )
                })
        };
        let payload = match row.kind {
            NodeKind::Permission => Payload::Permission {
                key: row.key.clone(),
                value: flag(&row.value)?,
            },
            NodeKind::Inherit => Payload::Inherit {
                group: row.key.clone(),
                value: flag(&row.value)?,
            },
            NodeKind::Prefix => Payload::Prefix {
                priority: priority()?,
                text: row.value,
            },
            NodeKind::Suffix => Payload::Suffix {
                priority: priority()?,
                text: row.value,
            },
            NodeKind::Meta => Payload::Meta {
                key: row.key.clone(),
                value: row.value,
            },
            NodeKind::Weight => Payload::Weight {
                weight: row
                    .value
                    .parse()
                    .map_err(|_| invalid("weight", &row.value, "expected a 32-bit integer"))?,
            },
            NodeKind::DisplayName => Payload::DisplayName { name: row.value },
        };
        Node {
            payload,
            contexts: ContextSet::parse(&row.contexts)?,
            expires_at: row.expires_at,
            source: row.source.parse()?,
        }
        .normalized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn node_patterns() {
        for good in [
            "a",
            "a.b",
            "a.*",
            "*",
            "minecraft:command.gamemode",
            "minecraft:command.*",
            "x-y_z.0",
        ] {
            assert!(valid_node(good), "{good}");
        }
        for bad in [
            "",
            "a.",
            ".a",
            "a..b",
            "a*",
            "a.*.b",
            "*.a",
            "a.**",
            "minecraft:*",
            "A.b",
            "a b",
            "a/b",
        ] {
            assert!(!valid_node(bad), "{bad}");
        }
        assert!(valid_node(&"a".repeat(256)));
        assert!(!valid_node(&"a".repeat(257)));
        assert_eq!(normalize_node("Essentials.FLY").unwrap(), "essentials.fly");
        assert!(normalize_node("é.fly").is_err());
    }

    #[test]
    fn kinds_have_stable_codes() {
        for (code, kind) in NodeKind::ALL.into_iter().enumerate() {
            assert_eq!(usize::from(kind.code()), code);
            assert_eq!(NodeKind::from_code(kind.code()), Some(kind));
            assert_eq!(kind.as_str().parse::<NodeKind>().unwrap(), kind);
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
        }
        assert_eq!(NodeKind::from_code(7), None);
        assert_eq!(NodeKind::DisplayName.code(), 6);
    }

    #[test]
    fn json_shape_is_minimal_and_validated() {
        let node = Node::permission("Essentials.Fly", true)
            .unwrap()
            .with_context("server", "survival")
            .unwrap()
            .expiring_at(5);
        let value = serde_json::to_value(&node).unwrap();
        assert_eq!(
            value,
            json!({"kind": "permission", "key": "essentials.fly", "value": true,
                   "contexts": {"server": "survival"}, "expires_at": 5})
        );
        assert_eq!(serde_json::from_value::<Node>(value).unwrap(), node);
        let plugin: Node = serde_json::from_value(
            json!({"kind": "meta", "key": "Homes", "value": "5", "source": "plugin:essentials"}),
        )
        .unwrap();
        assert_eq!(
            plugin.payload,
            Payload::Meta {
                key: "homes".into(),
                value: "5".into()
            }
        );
        assert_eq!(plugin.source, Source::Plugin("essentials".into()));
        for bad in [
            json!({"kind": "permission", "key": "a..b", "value": true}),
            json!({"kind": "inherit", "group": "a:b", "value": true}),
            json!({"kind": "permission", "key": "a", "value": "yes"}),
            json!({"kind": "prefix", "priority": 1, "text": "a\nb"}),
            json!({"kind": "display_name", "name": ""}),
            json!({"kind": "nope"}),
            json!({"kind": "permission", "key": "a", "value": true, "source": "plugin:"}),
            json!({"kind": "permission", "key": "a", "value": true,
                   "contexts": {"server": ["a", "b"]}}),
        ] {
            assert!(
                serde_json::from_value::<Node>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn rows_round_trip() {
        let nodes = [
            Node::permission("a.*", false).unwrap(),
            Node::inherit("vip", true)
                .unwrap()
                .with_context("server", "s")
                .unwrap(),
            Node::prefix(100, "&6[VIP] ").unwrap().expiring_at(9),
            Node::suffix(-3, "").unwrap(),
            Node::meta("tab-color", "Gold").unwrap(),
            Node::weight(50),
            Node::display_name("V.I.P")
                .unwrap()
                .with_source(Source::Import),
        ];
        for node in nodes {
            let row = NodeRow::from(&node);
            assert_eq!(Node::try_from(row.clone()).unwrap(), node, "{row:?}");
        }
        let row = NodeRow::from(&Node::prefix(100, "x").unwrap());
        assert_eq!((row.key.as_str(), row.priority), ("100", Some(100)));
        let key = NodeKey::new(NodeKind::Permission, "A.B", ContextSet::new()).unwrap();
        assert_eq!(key, Node::permission("a.b", true).unwrap().key());
        assert!(NodeKey::new(NodeKind::Prefix, "x", ContextSet::new()).is_err());
        assert!(NodeKey::new(NodeKind::Weight, "weight", ContextSet::new()).is_ok());
    }

    #[test]
    fn sources() {
        for text in ["store", "config", "import", "plugin:server.lobby"] {
            assert_eq!(text.parse::<Source>().unwrap().to_string(), text);
        }
        assert!("plugin:a b".parse::<Source>().is_err());
        assert!("other".parse::<Source>().is_err());
    }
}
