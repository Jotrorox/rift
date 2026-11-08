//! LuckPerms compatibility: string nodes (`group.x`, `prefix.N.text`, ...) in
//! both directions and the `/lp export` JSON file.
//!
//! # Assumptions about the export format
//!
//! `/lp export` (LuckPerms 5) writes, possibly gzipped:
//!
//! ```json
//! {"metadata": {...},
//!  "groups": {"admin": {"nodes": [{"type": "permission", "key": "*", "value": true}]}},
//!  "users": {"069a79f4-44e9-4726-a5be-fca90e38aaf5": {"username": "Notch",
//!            "primaryGroup": "default", "nodes": [...]}},
//!  "tracks": {"staff": {"groups": ["helper", "mod", "admin"]}}}
//! ```
//!
//! Each node is `{type, key, value, context?, expiry?}` with `expiry` in Unix
//! seconds and `context` an object whose values are a string or an array of
//! strings. Rift decodes the node from `key` alone; `type` is informational
//! except that `regex_permission` is refused. `metadata` and unknown fields
//! are ignored. `primaryGroup` is not imported: LuckPerms' default
//! `parents-by-weight` mode computes it, which is what Rift does too.
//!
//! LuckPerms context sets mean "any listed value per key"; Rift's mean "all
//! pairs" ([`crate::context`]). The importer therefore writes one node per
//! combination of values, and drops the legacy `server=global` and
//! `world=global` pairs, which meant "no context".

use crate::{
    context::{self, ContextSet},
    error::{Error, invalid},
    node::{Node, NodeKind, Payload, Source, normalize_node},
    subject::{Group, Track, User, Uuid, normalize_group},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Most nodes one shorthand key or one context set may expand into.
pub const MAX_EXPANSION: usize = 256;

/// Split at the first `.` not escaped by a backslash.
fn split_unescaped(text: &str) -> Option<(&str, &str)> {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'.' => return Some((&text[..index], &text[index + 1..])),
            _ => index += 1,
        }
    }
    None
}

fn unescape(text: &str) -> String {
    text.replace("\\.", ".")
}

fn escape(text: &str) -> String {
    text.replace('.', "\\.")
}

/// Strip an ASCII-case-insensitive prefix.
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

/// Expand LuckPerms shorthand: `a.{b,c}` and `a.(b|c)` become `a.b` and
/// `a.c`; several groups multiply.
pub fn expand_shorthand(key: &str) -> Result<Vec<String>, Error> {
    let mut results = vec![String::new()];
    let mut rest = key;
    while let Some(open) = rest.find(['{', '(']) {
        let (close, separator) = if rest.as_bytes()[open] == b'{' {
            ('}', ',')
        } else {
            (')', '|')
        };
        let Some(length) = rest[open..].find(close) else {
            return Err(invalid("permission node", key, "unclosed shorthand group"));
        };
        let (head, group) = (&rest[..open], &rest[open + 1..open + length]);
        let mut next = Vec::new();
        for prefix in &results {
            for option in group.split(separator) {
                next.push(format!("{prefix}{head}{option}"));
            }
        }
        if next.len() > MAX_EXPANSION {
            return Err(invalid(
                "permission node",
                key,
                "shorthand expands to too many nodes",
            ));
        }
        results = next;
        rest = &rest[open + length + 1..];
    }
    Ok(results.into_iter().map(|prefix| prefix + rest).collect())
}

/// Decode one LuckPerms node key into payloads (several when shorthand
/// expands). Chat meta nodes (`prefix`, `suffix`, `meta`, `weight`,
/// `displayname`) must have value `true`.
pub fn parse_key(key: &str, value: bool) -> Result<Vec<Payload>, Error> {
    if strip_prefix_ci(key, "r=").is_some() {
        return Err(Error::RegexNode {
            key: key.to_owned(),
        });
    }
    let chat = |kind: &str| -> Result<(), Error> {
        if value {
            Ok(())
        } else {
            Err(invalid(
                "LuckPerms node",
                key,
                match kind {
                    "meta" => "negated meta nodes have no meaning",
                    _ => "negated chat meta nodes have no meaning",
                },
            ))
        }
    };
    let priority_and_text = |rest: &str| -> Result<(i32, String), Error> {
        let (priority, text) = split_unescaped(rest)
            .ok_or_else(|| invalid("LuckPerms node", key, "expected <priority>.<text>"))?;
        let priority = priority
            .parse()
            .map_err(|_| invalid("LuckPerms node", key, "priority must be a 32-bit integer"))?;
        Ok((priority, unescape(text)))
    };
    let payload = if let Some(group) = strip_prefix_ci(key, "group.") {
        Payload::Inherit {
            group: normalize_group(group)?,
            value,
        }
    } else if let Some(rest) = strip_prefix_ci(key, "prefix.") {
        chat("prefix")?;
        let (priority, text) = priority_and_text(rest)?;
        Payload::Prefix { priority, text }
    } else if let Some(rest) = strip_prefix_ci(key, "suffix.") {
        chat("suffix")?;
        let (priority, text) = priority_and_text(rest)?;
        Payload::Suffix { priority, text }
    } else if let Some(rest) = strip_prefix_ci(key, "meta.") {
        chat("meta")?;
        let (meta, text) = split_unescaped(rest)
            .ok_or_else(|| invalid("LuckPerms node", key, "expected meta.<key>.<value>"))?;
        Payload::Meta {
            key: unescape(meta),
            value: unescape(text),
        }
    } else if let Some(weight) = strip_prefix_ci(key, "weight.") {
        chat("weight")?;
        Payload::Weight {
            weight: weight
                .parse()
                .map_err(|_| invalid("LuckPerms node", key, "weight must be a 32-bit integer"))?,
        }
    } else if let Some(name) = strip_prefix_ci(key, "displayname.") {
        chat("displayname")?;
        Payload::DisplayName {
            name: name.to_owned(),
        }
    } else {
        return expand_shorthand(key)?
            .into_iter()
            .map(|pattern| {
                Ok(Payload::Permission {
                    key: normalize_node(&pattern)?,
                    value,
                })
            })
            .collect();
    };
    Ok(vec![payload])
}

/// Expand a LuckPerms context set (any value per key) into Rift sets (all
/// pairs), one per combination, dropping `server=global` and `world=global`.
pub fn expand_contexts(contexts: &ContextSet) -> Result<Vec<ContextSet>, Error> {
    let mut results = vec![ContextSet::new()];
    let mut keys: Vec<&str> = contexts.iter().map(|(key, _)| key).collect();
    keys.dedup();
    for key in keys {
        let values: Vec<&str> = contexts
            .values(key)
            .filter(|value| {
                !(*value == "global" && matches!(key, context::SERVER | context::WORLD))
            })
            .collect();
        if values.is_empty() {
            continue;
        }
        let mut next = Vec::with_capacity(results.len() * values.len());
        for set in &results {
            for value in &values {
                next.push(set.clone().with(key, value)?);
            }
        }
        if next.len() > MAX_EXPANSION {
            return Err(invalid(
                "context set",
                &contexts.to_string(),
                "expands to too many combinations",
            ));
        }
        results = next;
    }
    Ok(results)
}

/// Decode a LuckPerms node into Rift nodes with source `import`.
pub fn parse_node(node: &ExportNode) -> Result<Vec<Node>, Error> {
    if node.kind.as_deref() == Some("regex_permission") {
        return Err(Error::RegexNode {
            key: node.key.clone(),
        });
    }
    let mut contexts = ContextSet::new();
    for (key, values) in &node.context {
        for value in values.as_slice() {
            contexts.insert(key, value)?;
        }
    }
    let mut nodes = Vec::new();
    for payload in parse_key(&node.key, node.value)? {
        for contexts in expand_contexts(&contexts)? {
            let mut built = Node::new(payload.clone())?.with_contexts(contexts)?;
            built.expires_at = node.expiry;
            nodes.push(built.with_source(Source::Import));
        }
    }
    Ok(nodes)
}

/// The LuckPerms `type` of a node kind.
pub const fn node_type(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Permission => "permission",
        NodeKind::Inherit => "inheritance",
        NodeKind::Prefix => "prefix",
        NodeKind::Suffix => "suffix",
        NodeKind::Meta => "meta",
        NodeKind::Weight => "weight",
        NodeKind::DisplayName => "display_name",
    }
}

/// Encode a payload as a LuckPerms key and value. Dots in prefix, suffix and
/// meta text are escaped as `\.`.
pub fn to_key(payload: &Payload) -> (String, bool) {
    match payload {
        Payload::Permission { key, value } => (key.clone(), *value),
        Payload::Inherit { group, value } => (format!("group.{group}"), *value),
        Payload::Prefix { priority, text } => (format!("prefix.{priority}.{}", escape(text)), true),
        Payload::Suffix { priority, text } => (format!("suffix.{priority}.{}", escape(text)), true),
        Payload::Meta { key, value } => (format!("meta.{}.{}", escape(key), escape(value)), true),
        Payload::Weight { weight } => (format!("weight.{weight}"), true),
        Payload::DisplayName { name } => (format!("displayname.{name}"), true),
    }
}

/// A context value in the export: one string or several.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextValues {
    One(String),
    Many(Vec<String>),
}

impl ContextValues {
    pub fn as_slice(&self) -> &[String] {
        match self {
            Self::One(value) => std::slice::from_ref(value),
            Self::Many(values) => values,
        }
    }
}

fn yes() -> bool {
    true
}

/// One node of the export file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportNode {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub key: String,
    #[serde(default = "yes")]
    pub value: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, ContextValues>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<u64>,
}

impl From<&Node> for ExportNode {
    fn from(node: &Node) -> Self {
        let (key, value) = to_key(&node.payload);
        // LuckPerms reads several values of a key as "any", Rift as "all";
        // only single-valued keys translate, so the first value is kept.
        let mut context = BTreeMap::new();
        for (k, v) in node.contexts.iter() {
            context
                .entry(k.to_owned())
                .or_insert_with(|| ContextValues::One(v.to_owned()));
        }
        Self {
            kind: Some(node_type(node.kind()).to_owned()),
            key,
            value,
            context,
            expiry: node.expires_at,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportGroup {
    #[serde(default)]
    pub nodes: Vec<ExportNode>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportUser {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(
        rename = "primaryGroup",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub primary_group: Option<String>,
    #[serde(default)]
    pub nodes: Vec<ExportNode>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportTrack {
    #[serde(default)]
    pub groups: Vec<String>,
}

/// The `/lp export` document. Users are keyed by dashed UUID.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Export {
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub groups: BTreeMap<String, ExportGroup>,
    #[serde(default)]
    pub users: BTreeMap<String, ExportUser>,
    #[serde(default)]
    pub tracks: BTreeMap<String, ExportTrack>,
}

/// A node or subject the importer left out, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    /// `user:<uuid>`, `group:<name>` or `track:<name>` as written in the file.
    pub subject: String,
    /// The node key; empty when the whole subject was skipped.
    pub key: String,
    pub reason: String,
}

/// The typed result of reading an export.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    pub users: Vec<User>,
    pub groups: Vec<Group>,
    pub tracks: Vec<Track>,
    pub skipped: Vec<Skipped>,
}

/// Decode a subject's nodes, listing failures in `skipped`. Users may not
/// hold group-only nodes.
fn import_nodes(
    subject: &str,
    list: &[ExportNode],
    skipped: &mut Vec<Skipped>,
    group: bool,
) -> Vec<Node> {
    let mut out = Vec::new();
    for node in list {
        let parsed = parse_node(node).and_then(|parsed| {
            if !group && parsed.iter().any(|n| n.kind().group_only()) {
                Err(invalid(
                    "LuckPerms node",
                    &node.key,
                    "weight and display name apply to groups only",
                ))
            } else {
                Ok(parsed)
            }
        });
        match parsed {
            Ok(parsed) => out.extend(parsed),
            Err(error) => skipped.push(Skipped {
                subject: subject.to_owned(),
                key: node.key.clone(),
                reason: error.to_string(),
            }),
        }
    }
    out
}

impl Export {
    /// Convert to Rift subjects. Invalid nodes and subjects are listed in
    /// [`Import::skipped`] instead of failing the whole file. A group's
    /// weight and display name come from its global `weight` and
    /// `displayname` nodes, which are also kept as nodes.
    pub fn import(&self) -> Import {
        let mut import = Import::default();
        for (name, data) in &self.groups {
            let label = format!("group:{name}");
            let Ok(mut group) = Group::new(name) else {
                import.skipped.push(Skipped {
                    subject: label,
                    key: String::new(),
                    reason: "invalid group name".into(),
                });
                continue;
            };
            group.nodes = import_nodes(&label, &data.nodes, &mut import.skipped, true);
            for node in group.nodes.iter().filter(|n| n.contexts.is_empty()) {
                match &node.payload {
                    Payload::Weight { weight } => group.weight = *weight,
                    Payload::DisplayName { name } => group.display_name = Some(name.clone()),
                    _ => {}
                }
            }
            import.groups.push(group);
        }
        for (key, data) in &self.users {
            let label = format!("user:{key}");
            let Ok(uuid) = Uuid::parse(key) else {
                import.skipped.push(Skipped {
                    subject: label,
                    key: String::new(),
                    reason: "invalid UUID".into(),
                });
                continue;
            };
            import.users.push(User {
                uuid,
                last_name: data.username.clone(),
                primary_group: None,
                nodes: import_nodes(&label, &data.nodes, &mut import.skipped, false),
            });
        }
        for (name, data) in &self.tracks {
            let groups: Vec<&str> = data.groups.iter().map(String::as_str).collect();
            match Track::new(name, &groups) {
                Ok(track) => import.tracks.push(track),
                Err(error) => import.skipped.push(Skipped {
                    subject: format!("track:{name}"),
                    key: String::new(),
                    reason: error.to_string(),
                }),
            }
        }
        import
    }

    /// The export LuckPerms reads back with `/lp import`.
    pub fn from_model(users: &[User], groups: &[Group], tracks: &[Track]) -> Self {
        let encode = |nodes: &[Node]| nodes.iter().map(ExportNode::from).collect();
        Self {
            metadata: serde_json::json!({"generatedBy": "rift"}),
            groups: groups
                .iter()
                .map(|g| {
                    (
                        g.name.clone(),
                        ExportGroup {
                            nodes: encode(&g.nodes),
                        },
                    )
                })
                .collect(),
            users: users
                .iter()
                .map(|u| {
                    let user = ExportUser {
                        username: u.last_name.clone(),
                        primary_group: u.primary_group.clone(),
                        nodes: encode(&u.nodes),
                    };
                    (u.uuid.hyphenated(), user)
                })
                .collect(),
            tracks: tracks
                .iter()
                .map(|t| {
                    (
                        t.name.clone(),
                        ExportTrack {
                            groups: t.groups.clone(),
                        },
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(key: &str) -> Payload {
        let mut payloads = parse_key(key, true).unwrap();
        assert_eq!(payloads.len(), 1, "{key}");
        payloads.remove(0)
    }

    #[test]
    fn string_nodes() {
        assert_eq!(
            one("group.VIP"),
            Payload::Inherit {
                group: "vip".into(),
                value: true
            }
        );
        assert_eq!(
            one("prefix.100.&6[V.I.P] "),
            Payload::Prefix {
                priority: 100,
                text: "&6[V.I.P] ".into()
            }
        );
        assert_eq!(
            one("suffix.-5.\\.end"),
            Payload::Suffix {
                priority: -5,
                text: ".end".into()
            }
        );
        assert_eq!(
            one("meta.tab\\.color.Gold\\.ish"),
            Payload::Meta {
                key: "tab.color".into(),
                value: "Gold.ish".into()
            }
        );
        assert_eq!(one("weight.50"), Payload::Weight { weight: 50 });
        assert_eq!(
            one("displayname.Very Important"),
            Payload::DisplayName {
                name: "Very Important".into()
            }
        );
        assert_eq!(
            parse_key("Essentials.Fly", false).unwrap(),
            [Payload::Permission {
                key: "essentials.fly".into(),
                value: false
            }]
        );
        assert!(matches!(
            parse_key("r=essentials\\..*", true),
            Err(Error::RegexNode { .. })
        ));
        assert!(matches!(
            parse_key("R=x", true),
            Err(Error::RegexNode { .. })
        ));
        assert!(parse_key("prefix.x.y", true).is_err());
        assert!(parse_key("prefix.100", true).is_err());
        assert!(parse_key("meta.homes.5", false).is_err());
        assert!(parse_key("a b", true).is_err());
    }

    #[test]
    fn shorthand() {
        assert_eq!(expand_shorthand("a.{b,c}.d").unwrap(), ["a.b.d", "a.c.d"]);
        assert_eq!(
            expand_shorthand("x.(a|b).{c,d}").unwrap(),
            ["x.a.c", "x.a.d", "x.b.c", "x.b.d"]
        );
        assert_eq!(expand_shorthand("plain").unwrap(), ["plain"]);
        assert!(expand_shorthand("a.{b").is_err());
        let keys: Vec<_> = parse_key("essentials.{fly,heal}", true)
            .unwrap()
            .into_iter()
            .map(|p| p.key())
            .collect();
        assert_eq!(keys, ["essentials.fly", "essentials.heal"]);
    }

    #[test]
    fn contexts_expand_to_combinations() {
        let node: ExportNode = serde_json::from_value(json!({
            "type": "permission", "key": "fly", "value": true, "expiry": 99,
            "context": {"server": ["a", "b"], "world": "global", "gamemode": "creative"}
        }))
        .unwrap();
        let nodes = parse_node(&node).unwrap();
        let sets: Vec<String> = nodes.iter().map(|n| n.contexts.to_string()).collect();
        assert_eq!(
            sets,
            ["gamemode=creative;server=a", "gamemode=creative;server=b"]
        );
        assert!(
            nodes
                .iter()
                .all(|n| n.expires_at == Some(99) && n.source == Source::Import)
        );
        let regex: ExportNode =
            serde_json::from_value(json!({"type": "regex_permission", "key": "x"})).unwrap();
        assert!(matches!(parse_node(&regex), Err(Error::RegexNode { .. })));
    }

    #[test]
    fn export_round_trip() {
        let file = json!({
            "metadata": {"generatedBy": "LuckPerms"},
            "groups": {
                "Admin": {"nodes": [
                    {"type": "permission", "key": "*", "value": true},
                    {"type": "weight", "key": "weight.100", "value": true},
                    {"type": "display_name", "key": "displayname.Admins", "value": true},
                    {"type": "prefix", "key": "prefix.100.&c[Admin] ", "value": true,
                     "context": {"server": "survival"}},
                    {"type": "regex_permission", "key": "r=.*", "value": true}
                ]}
            },
            "users": {
                "069a79f4-44e9-4726-a5be-fca90e38aaf5": {"username": "Notch", "primaryGroup": "admin",
                    "nodes": [{"type": "inheritance", "key": "group.admin", "value": true},
                              {"type": "weight", "key": "weight.5", "value": true}]},
                "not-a-uuid": {"nodes": []}
            },
            "tracks": {"staff": {"groups": ["default", "admin"]}}
        });
        let export: Export = serde_json::from_value(file).unwrap();
        let import = export.import();
        let admin = &import.groups[0];
        assert_eq!((admin.name.as_str(), admin.weight), ("admin", 100));
        assert_eq!(admin.display_name.as_deref(), Some("Admins"));
        assert_eq!(admin.nodes.len(), 4);
        assert_eq!(
            import.users[0].uuid.to_string(),
            "069a79f444e94726a5befca90e38aaf5"
        );
        assert_eq!(import.users[0].last_name.as_deref(), Some("Notch"));
        assert_eq!(import.users[0].nodes.len(), 1);
        assert_eq!(import.tracks[0].groups, ["default", "admin"]);
        let reasons: Vec<_> = import
            .skipped
            .iter()
            .map(|s| (s.subject.as_str(), s.key.as_str()))
            .collect();
        assert_eq!(
            reasons,
            [
                ("group:Admin", "r=.*"),
                ("user:069a79f4-44e9-4726-a5be-fca90e38aaf5", "weight.5"),
                ("user:not-a-uuid", ""),
            ]
        );
        let back = Export::from_model(&import.users, &import.groups, &import.tracks);
        let again = back.import();
        assert_eq!(again.groups, import.groups);
        assert_eq!(again.users, import.users);
        assert_eq!(again.tracks, import.tracks);
        assert!(again.skipped.is_empty());
        let prefix = &back.groups["admin"].nodes[3];
        assert_eq!(prefix.key, "prefix.100.&c[Admin] ");
        assert_eq!(prefix.kind.as_deref(), Some("prefix"));
    }

    #[test]
    fn keys_round_trip() {
        for node in [
            Node::permission("minecraft:command.*", false).unwrap(),
            Node::inherit("vip", true).unwrap(),
            Node::prefix(-1, "a.b").unwrap(),
            Node::meta("k.x", "v.y").unwrap(),
            Node::weight(7),
            Node::display_name("Name").unwrap(),
        ] {
            let (key, value) = to_key(&node.payload);
            assert_eq!(
                parse_key(&key, value).unwrap(),
                std::slice::from_ref(&node.payload),
                "{key}"
            );
        }
    }
}
