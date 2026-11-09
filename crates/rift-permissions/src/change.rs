//! The write model: every surface builds a [`Change`] and passes an
//! [`Actor`]; the authority applies it in one transaction and one revision,
//! answers [`Applied`] and publishes a [`ChangeEvent`].

use crate::{
    context::ContextSet,
    error::{Error, invalid},
    node::{Node, NodeKey, NodeKind, Payload, Source, valid_label},
    subject::{DEFAULT_GROUP, Uuid, normalize_group, normalize_track},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, str::FromStr};

/// A user or group. JSON: `{"user": "<32 hex>"}` or `{"group": "vip"}`;
/// string form `user:<hex>` or `group:<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectRef {
    User(Uuid),
    Group(String),
}

impl SubjectRef {
    fn normalized(self) -> Result<Self, Error> {
        Ok(match self {
            Self::User(uuid) => Self::User(uuid),
            Self::Group(name) => Self::Group(normalize_group(&name)?),
        })
    }
}

impl fmt::Display for SubjectRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(uuid) => write!(f, "user:{uuid}"),
            Self::Group(name) => write!(f, "group:{name}"),
        }
    }
}

impl FromStr for SubjectRef {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Error> {
        if let Some(uuid) = text.strip_prefix("user:") {
            Ok(Self::User(Uuid::parse(uuid)?))
        } else if let Some(name) = text.strip_prefix("group:") {
            Ok(Self::Group(normalize_group(name)?))
        } else {
            Err(invalid(
                "subject",
                text,
                "expected user:<uuid> or group:<name>",
            ))
        }
    }
}

/// Who made a change, as recorded in the audit log. Serialised as its string
/// form: `uuid:<hex>`, `operator:<name>`, `lua:<plugin>`, `backend:<name>`,
/// `cli`, `import` or `system` (expiry sweeps, configuration mirroring and
/// migrations). Names are 1 to 96 bytes of ASCII letters, digits, `_.-:`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Actor {
    Player(Uuid),
    Operator(String),
    Lua(String),
    Backend(String),
    Cli,
    Import,
    System,
}

impl fmt::Display for Actor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Player(uuid) => write!(f, "uuid:{uuid}"),
            Self::Operator(name) => write!(f, "operator:{name}"),
            Self::Lua(name) => write!(f, "lua:{name}"),
            Self::Backend(name) => write!(f, "backend:{name}"),
            Self::Cli => f.write_str("cli"),
            Self::Import => f.write_str("import"),
            Self::System => f.write_str("system"),
        }
    }
}

impl FromStr for Actor {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Error> {
        let named = |name: &str, build: fn(String) -> Self| {
            if valid_label(name) {
                Ok(build(name.to_owned()))
            } else {
                Err(invalid(
                    "actor",
                    text,
                    "names are 1 to 96 bytes of [A-Za-z0-9_.:-]",
                ))
            }
        };
        match text.split_once(':') {
            Some(("uuid", uuid)) => Ok(Self::Player(Uuid::parse(uuid)?)),
            Some(("operator", name)) => named(name, Self::Operator),
            Some(("lua", name)) => named(name, Self::Lua),
            Some(("backend", name)) => named(name, Self::Backend),
            None if text == "cli" => Ok(Self::Cli),
            None if text == "import" => Ok(Self::Import),
            None if text == "system" => Ok(Self::System),
            _ => Err(invalid(
                "actor",
                text,
                "expected uuid:, operator:, lua:, backend:, cli, import or system",
            )),
        }
    }
}

impl Serialize for Actor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Actor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<str>::deserialize(deserializer)?;
        text.parse().map_err(de::Error::custom)
    }
}

/// Which subjects a [`Filter`] covers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectFilter {
    #[default]
    Any,
    Users,
    Groups,
}

impl SubjectFilter {
    fn is_any(&self) -> bool {
        *self == Self::Any
    }
}

/// Selects nodes for [`Change::Bulk`]. Every set field must match; an empty
/// filter matches every node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    #[serde(default, skip_serializing_if = "SubjectFilter::is_any")]
    pub subjects: SubjectFilter,
    /// Only this subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<SubjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<NodeKind>,
    /// SQL `LIKE` against the storage key ([`Payload::key`]): `%` any run,
    /// `_` one character, `\` escapes; ASCII case-insensitive like SQLite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_like: Option<String>,
    /// Exact storage value ([`Payload::value`]), e.g. `"false"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Exactly these contexts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contexts: Option<ContextSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

impl Filter {
    pub fn matches(&self, subject: &SubjectRef, node: &Node) -> bool {
        let scope = matches!(
            (self.subjects, subject),
            (SubjectFilter::Any, _)
                | (SubjectFilter::Users, SubjectRef::User(_))
                | (SubjectFilter::Groups, SubjectRef::Group(_))
        );
        scope
            && self.subject.as_ref().is_none_or(|s| s == subject)
            && self.kind.is_none_or(|kind| kind == node.kind())
            && self
                .key_like
                .as_deref()
                .is_none_or(|pattern| like(pattern, &node.payload.key()))
            && self
                .value
                .as_deref()
                .is_none_or(|value| value == node.payload.value())
            && self.contexts.as_ref().is_none_or(|c| *c == node.contexts)
            && self.source.as_ref().is_none_or(|s| *s == node.source)
    }
}

/// SQL `LIKE` as SQLite evaluates it with `ESCAPE '\'`: `%` matches any run,
/// `_` one character, ASCII letters compare case-insensitively.
pub fn like(pattern: &str, text: &str) -> bool {
    enum Token {
        Any,
        One,
        Char(char),
    }
    let mut tokens = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '%' => Token::Any,
            '_' => Token::One,
            '\\' => Token::Char(chars.next().unwrap_or('\\')),
            c => Token::Char(c),
        });
    }
    // matched[j]: the first j tokens match the text consumed so far.
    let mut matched = vec![false; tokens.len() + 1];
    matched[0] = true;
    for (j, token) in tokens.iter().enumerate() {
        matched[j + 1] = matched[j] && matches!(token, Token::Any);
    }
    for c in text.chars() {
        let mut next = vec![false; tokens.len() + 1];
        for (j, token) in tokens.iter().enumerate() {
            next[j + 1] = match token {
                Token::Any => next[j] || matched[j + 1],
                Token::One => matched[j],
                Token::Char(t) => matched[j] && t.eq_ignore_ascii_case(&c),
            };
        }
        matched = next;
    }
    matched[tokens.len()]
}

/// What [`Change::Bulk`] does to each selected node. `delete` wins over
/// every other field; otherwise each set field is written. `value` applies
/// to permission and inherit nodes only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<bool>,
    /// Move the node to these contexts; it replaces a node already there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contexts: Option<ContextSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Remove the expiry.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub permanent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
}

impl Patch {
    /// The patched node, or `None` to delete it. Equal to the input when the
    /// patch changes nothing for this node.
    pub fn apply(&self, node: &Node) -> Result<Option<Node>, Error> {
        if self.delete {
            return Ok(None);
        }
        let mut node = node.clone();
        if let (Some(new), Payload::Permission { value, .. } | Payload::Inherit { value, .. }) =
            (self.value, &mut node.payload)
        {
            *value = new;
        }
        if let Some(contexts) = &self.contexts {
            node = node.with_contexts(contexts.clone())?;
        }
        if self.permanent {
            node.expires_at = None;
        } else if let Some(at) = self.expires_at {
            node.expires_at = Some(at);
        }
        if let Some(source) = &self.source {
            node.source = source.clone();
        }
        Ok(Some(node))
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One write. JSON is tagged by `op` with the variant name in snake case:
///
/// ```json
/// {"op": "set_node", "subject": {"user": "0123…"}, "node": {"kind": "permission", "key": "fly", "value": true}}
/// {"op": "unset_node", "subject": {"group": "vip"}, "kind": "prefix", "key": "100", "contexts": {"server": "s"}}
/// {"op": "bulk", "filter": {"kind": "permission", "key_like": "essentials.%"}, "patch": {"value": false}}
/// ```
///
/// Call [`Change::normalized`] on input from outside before applying it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    /// Insert the node, replacing the one in its slot ([`Node::key`]).
    SetNode {
        subject: SubjectRef,
        node: Node,
    },
    /// Remove the node in a slot.
    UnsetNode {
        subject: SubjectRef,
        #[serde(flatten)]
        key: NodeKey,
    },
    /// Remove every node of a subject, or only those with exactly `contexts`.
    ClearSubject {
        subject: SubjectRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contexts: Option<ContextSet>,
    },
    CreateGroup {
        name: String,
        #[serde(default)]
        weight: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
    },
    /// Delete a group and its nodes; `inherit` nodes naming it stay dangling.
    /// `default` cannot be deleted.
    DeleteGroup {
        name: String,
    },
    /// Set the weight column and the group's global `weight` node.
    SetWeight {
        group: String,
        weight: i32,
    },
    /// Set or clear the display name column and global `display_name` node.
    SetDisplayName {
        group: String,
        display_name: Option<String>,
    },
    /// Set or clear a user's explicit primary group.
    SetPrimaryGroup {
        user: Uuid,
        group: Option<String>,
    },
    /// Create or replace a track.
    SetTrack {
        name: String,
        groups: Vec<String>,
    },
    DeleteTrack {
        name: String,
    },
    /// Move a user up a track in `contexts` ([`crate::Track::promote`]).
    Promote {
        user: Uuid,
        track: String,
        #[serde(default, skip_serializing_if = "ContextSet::is_empty")]
        contexts: ContextSet,
    },
    /// Move a user down a track in `contexts` ([`crate::Track::demote`]).
    Demote {
        user: Uuid,
        track: String,
        #[serde(default, skip_serializing_if = "ContextSet::is_empty")]
        contexts: ContextSet,
    },
    /// Patch every node the filter selects.
    Bulk {
        filter: Filter,
        patch: Patch,
    },
    /// Several changes in one transaction and one revision; not nested.
    Batch {
        changes: Vec<Change>,
    },
}

impl Change {
    /// Normalise names, check node validity and structural rules: group-only
    /// nodes on groups, `default` not deleted, tracks without duplicates,
    /// batches flat and non-empty. Store limits are the authority's job.
    pub fn normalized(self) -> Result<Self, Error> {
        let group = |name: String| normalize_group(&name);
        Ok(match self {
            Self::SetNode { subject, node } => {
                let subject = subject.normalized()?;
                let node = node.normalized()?;
                if node.kind().group_only() && matches!(subject, SubjectRef::User(_)) {
                    return Err(invalid(
                        "node",
                        node.kind().as_str(),
                        "applies to groups only",
                    ));
                }
                Self::SetNode { subject, node }
            }
            Self::UnsetNode { subject, key } => Self::UnsetNode {
                subject: subject.normalized()?,
                key: key.normalized()?,
            },
            Self::ClearSubject { subject, contexts } => Self::ClearSubject {
                subject: subject.normalized()?,
                contexts,
            },
            Self::CreateGroup {
                name,
                weight,
                display_name,
            } => {
                if let Some(display_name) = &display_name {
                    Node::display_name(display_name)?;
                }
                Self::CreateGroup {
                    name: group(name)?,
                    weight,
                    display_name,
                }
            }
            Self::DeleteGroup { name } => {
                let name = group(name)?;
                if name == DEFAULT_GROUP {
                    return Err(invalid(
                        "group",
                        &name,
                        "the default group cannot be deleted",
                    ));
                }
                Self::DeleteGroup { name }
            }
            Self::SetWeight {
                group: name,
                weight,
            } => Self::SetWeight {
                group: group(name)?,
                weight,
            },
            Self::SetDisplayName {
                group: name,
                display_name,
            } => {
                if let Some(display_name) = &display_name {
                    Node::display_name(display_name)?;
                }
                Self::SetDisplayName {
                    group: group(name)?,
                    display_name,
                }
            }
            Self::SetPrimaryGroup { user, group: name } => Self::SetPrimaryGroup {
                user,
                group: name.map(group).transpose()?,
            },
            Self::SetTrack { name, groups } => {
                let names: Vec<&str> = groups.iter().map(String::as_str).collect();
                let track = crate::Track::new(&name, &names)?;
                Self::SetTrack {
                    name: track.name,
                    groups: track.groups,
                }
            }
            Self::DeleteTrack { name } => Self::DeleteTrack {
                name: normalize_track(&name)?,
            },
            Self::Promote {
                user,
                track,
                contexts,
            } => Self::Promote {
                user,
                track: normalize_track(&track)?,
                contexts,
            },
            Self::Demote {
                user,
                track,
                contexts,
            } => Self::Demote {
                user,
                track: normalize_track(&track)?,
                contexts,
            },
            Self::Bulk { mut filter, patch } => {
                filter.subject = filter.subject.map(SubjectRef::normalized).transpose()?;
                if patch.is_empty() {
                    return Err(invalid("bulk patch", "{}", "sets nothing"));
                }
                Self::Bulk { filter, patch }
            }
            Self::Batch { changes } => {
                if changes.is_empty() {
                    return Err(invalid("batch", "[]", "is empty"));
                }
                let changes = changes
                    .into_iter()
                    .map(|change| match change {
                        Self::Batch { .. } => Err(invalid("batch", "batch", "cannot be nested")),
                        change => change.normalized(),
                    })
                    .collect::<Result<_, _>>()?;
                Self::Batch { changes }
            }
        })
    }
}

/// The reply to an applied change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    /// The new revision, or the current one when nothing changed.
    pub revision: u64,
    /// Number of [`Op`]s recorded; 0 means nothing changed and no revision
    /// was created.
    pub ops: usize,
    /// Users whose compiled subjects changed.
    pub changed_users: Vec<Uuid>,
    /// Groups whose own data changed.
    pub changed_groups: Vec<String>,
}

/// A group's columns, for [`Op::Group`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupAttrs {
    pub weight: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// One recorded effect of a change, as before/after. `null` on one side means
/// created or removed. JSON is tagged by `op`: `node`, `group`, `track` or
/// `primary_group`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Node {
        subject: SubjectRef,
        before: Option<Node>,
        after: Option<Node>,
    },
    Group {
        name: String,
        before: Option<GroupAttrs>,
        after: Option<GroupAttrs>,
    },
    Track {
        name: String,
        before: Option<Vec<String>>,
        after: Option<Vec<String>>,
    },
    PrimaryGroup {
        user: Uuid,
        before: Option<String>,
        after: Option<String>,
    },
}

/// A committed change: the `perm_change` row with the affected subjects, as
/// published on `rift.perms.update`, `rift.events.permissions` and the
/// `rift.perms.changes` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeEvent {
    pub revision: u64,
    /// Commit time, Unix seconds.
    pub at: u64,
    pub actor: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub ops: Vec<Op>,
    /// Set when `ops` was cut short to bound the message size; the full list
    /// is in the audit log.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ops_truncated: bool,
    /// Users whose compiled subjects changed.
    #[serde(default)]
    pub changed: Vec<Uuid>,
    /// Groups whose own data changed.
    #[serde(default)]
    pub groups: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HEX: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn subject_and_actor_forms() {
        let user = SubjectRef::User(Uuid::parse(HEX).unwrap());
        assert_eq!(serde_json::to_value(&user).unwrap(), json!({"user": HEX}));
        assert_eq!(user.to_string().parse::<SubjectRef>().unwrap(), user);
        assert_eq!(
            "group:VIP".parse::<SubjectRef>().unwrap(),
            SubjectRef::Group("vip".into())
        );
        for text in [
            format!("uuid:{HEX}"),
            "operator:alice".into(),
            "lua:shop".into(),
            "backend:server.lobby".into(),
            "cli".into(),
            "import".into(),
            "system".into(),
        ] {
            let actor: Actor = text.parse().unwrap();
            assert_eq!(actor.to_string(), text);
            assert_eq!(serde_json::to_value(&actor).unwrap(), json!(text));
        }
        for bad in ["", "uuid:x", "operator:", "lua:a b", "root", "cli:x"] {
            assert!(bad.parse::<Actor>().is_err(), "{bad}");
        }
    }

    #[test]
    fn change_json() {
        let unset: Change = serde_json::from_value(json!({
            "op": "unset_node", "subject": {"group": "vip"}, "kind": "prefix", "key": "100",
            "contexts": {"server": "s"}
        }))
        .unwrap();
        let Change::UnsetNode { key, .. } = &unset else {
            panic!()
        };
        assert_eq!((key.kind, key.key.as_str()), (NodeKind::Prefix, "100"));
        assert_eq!(
            serde_json::to_value(&unset).unwrap(),
            json!({"op": "unset_node", "subject": {"group": "vip"}, "kind": "prefix", "key": "100",
                   "contexts": {"server": "s"}})
        );
        let bulk: Change = serde_json::from_value(json!({
            "op": "bulk", "filter": {"kind": "permission", "key_like": "essentials.%"},
            "patch": {"value": false}
        }))
        .unwrap();
        assert_eq!(
            serde_json::from_value::<Change>(serde_json::to_value(&bulk).unwrap()).unwrap(),
            bulk
        );
        let set: Change = serde_json::from_value(json!({
            "op": "set_node", "subject": {"user": HEX},
            "node": {"kind": "permission", "key": "Fly", "value": true}
        }))
        .unwrap();
        let Change::SetNode { node, .. } = &set else {
            panic!()
        };
        assert_eq!(node.payload.key(), "fly");
    }

    #[test]
    fn normalization_rules() {
        let user = SubjectRef::User(Uuid::from_u128(1));
        let weight = Change::SetNode {
            subject: user.clone(),
            node: Node::weight(1),
        };
        assert!(weight.normalized().is_err());
        assert!(
            Change::DeleteGroup {
                name: "Default".into()
            }
            .normalized()
            .is_err()
        );
        let create = Change::CreateGroup {
            name: "VIP".into(),
            weight: 1,
            display_name: None,
        };
        assert_eq!(
            create.normalized().unwrap(),
            Change::CreateGroup {
                name: "vip".into(),
                weight: 1,
                display_name: None
            }
        );
        let nested = Change::Batch {
            changes: vec![Change::Batch { changes: vec![] }],
        };
        assert!(nested.normalized().is_err());
        assert!(Change::Batch { changes: vec![] }.normalized().is_err());
        let track = Change::SetTrack {
            name: "R".into(),
            groups: vec!["a".into(), "a".into()],
        };
        assert!(track.normalized().is_err());
        let bulk = Change::Bulk {
            filter: Filter::default(),
            patch: Patch::default(),
        };
        assert!(bulk.normalized().is_err());
    }

    #[test]
    fn filters_and_patches() {
        let group = SubjectRef::Group("vip".into());
        let node = Node::permission("essentials.fly", true).unwrap();
        let filter = Filter {
            kind: Some(NodeKind::Permission),
            key_like: Some("Essentials.%".into()),
            ..Filter::default()
        };
        assert!(filter.matches(&group, &node));
        assert!(
            !Filter {
                subjects: SubjectFilter::Users,
                ..filter.clone()
            }
            .matches(&group, &node)
        );
        assert!(
            !Filter {
                value: Some("false".into()),
                ..filter.clone()
            }
            .matches(&group, &node)
        );
        assert!(
            like("a_c", "abc")
                && !like("a_c", "ac")
                && like("%", "")
                && like("a\\%", "a%")
                && !like("a\\%", "ab")
        );
        let patch = Patch {
            value: Some(false),
            expires_at: Some(9),
            ..Patch::default()
        };
        let patched = patch.apply(&node).unwrap().unwrap();
        assert_eq!(
            patched.payload,
            Payload::Permission {
                key: "essentials.fly".into(),
                value: false
            }
        );
        assert_eq!(patched.expires_at, Some(9));
        assert_eq!(
            Patch {
                delete: true,
                ..Patch::default()
            }
            .apply(&node)
            .unwrap(),
            None
        );
        let meta = Node::meta("k", "v").unwrap();
        assert_eq!(patch.apply(&meta).unwrap().unwrap().payload, meta.payload);
    }

    #[test]
    fn event_shape() {
        let event = ChangeEvent {
            revision: 413,
            at: 1762800000,
            actor: Actor::Cli,
            reason: None,
            ops: vec![Op::Node {
                subject: SubjectRef::Group("vip".into()),
                before: None,
                after: Some(Node::permission("fly", true).unwrap()),
            }],
            ops_truncated: false,
            changed: vec![Uuid::parse(HEX).unwrap()],
            groups: vec!["vip".into()],
        };
        let json = json!({"revision": 413, "at": 1762800000, "actor": "cli",
            "ops": [{"op": "node", "subject": {"group": "vip"}, "before": null,
                     "after": {"kind": "permission", "key": "fly", "value": true}}],
            "changed": [HEX], "groups": ["vip"]});
        assert_eq!(serde_json::to_value(&event).unwrap(), json);
        assert_eq!(serde_json::from_value::<ChangeEvent>(json).unwrap(), event);
    }
}
