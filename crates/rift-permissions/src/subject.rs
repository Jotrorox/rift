//! Subjects (users and groups), tracks, UUIDs, names and limits.

use crate::{
    context::ContextSet,
    error::{Error, invalid},
    expired,
    node::{Node, Payload},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, str::FromStr};

/// The group every user inherits implicitly. It always exists in the store.
pub const DEFAULT_GROUP: &str = "default";
/// Longest group or track name in bytes.
pub const MAX_NAME_LEN: usize = 64;

/// Whether `name` is a valid group or track name: 1 to 64 bytes of
/// `[a-z0-9_.-]` (the `token()` rule Rift used before).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}

fn normalize_name(field: &'static str, name: &str) -> Result<String, Error> {
    let lower = name.to_ascii_lowercase();
    if valid_name(&lower) {
        Ok(lower)
    } else {
        Err(invalid(field, name, "use 1 to 64 bytes of [a-z0-9_.-]"))
    }
}

/// Lowercase and validate a group name.
pub fn normalize_group(name: &str) -> Result<String, Error> {
    normalize_name("group name", name)
}

/// Lowercase and validate a track name.
pub fn normalize_track(name: &str) -> Result<String, Error> {
    normalize_name("track name", name)
}

/// A player UUID. Displays and serialises as 32 lowercase hex digits without
/// dashes; parsing also accepts uppercase and the dashed 8-4-4-4-12 form.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Uuid([u8; 16]);

impl Uuid {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub const fn from_u128(value: u128) -> Self {
        Self(value.to_be_bytes())
    }

    pub const fn as_u128(&self) -> u128 {
        u128::from_be_bytes(self.0)
    }

    /// Parse 32 hex digits, or the dashed 36-character form.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let bad = || {
            invalid(
                "uuid",
                text,
                "expected 32 hex digits or the dashed 8-4-4-4-12 form",
            )
        };
        let digits: Vec<u8> = match text.len() {
            32 => text.bytes().collect(),
            36 => {
                let bytes = text.as_bytes();
                if [8, 13, 18, 23].iter().any(|&at| bytes[at] != b'-') {
                    return Err(bad());
                }
                text.bytes().filter(|&b| b != b'-').collect()
            }
            _ => return Err(bad()),
        };
        if digits.len() != 32 {
            return Err(bad());
        }
        let mut value = 0u128;
        for digit in digits {
            let nibble = (digit as char).to_digit(16).ok_or_else(bad)?;
            value = value << 4 | u128::from(nibble);
        }
        Ok(Self::from_u128(value))
    }

    /// The dashed form Java's `UUID.toString()` uses.
    pub fn hyphenated(&self) -> String {
        let hex = self.to_string();
        format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        )
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.as_u128())
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({self})")
    }
}

impl FromStr for Uuid {
    type Err = Error;
    fn from_str(text: &str) -> Result<Self, Error> {
        Self::parse(text)
    }
}

impl Serialize for Uuid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Uuid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = std::borrow::Cow::<str>::deserialize(deserializer)?;
        Self::parse(&text).map_err(de::Error::custom)
    }
}

/// Subject kind codes of the `perm_subject.kind` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    User = 0,
    Group = 1,
}

/// A user as the resolver sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub uuid: Uuid,
    /// Last seen username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    /// Explicit primary group; computed when absent (see
    /// [`crate::compiled::compile`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_group: Option<String>,
    /// Own nodes in insertion order. `weight` and `display_name` nodes are
    /// group-only and ignored here.
    #[serde(default)]
    pub nodes: Vec<Node>,
}

impl User {
    /// A user with no nodes: only the implicit `default` group applies.
    pub fn new(uuid: Uuid) -> Self {
        Self {
            uuid,
            last_name: None,
            primary_group: None,
            nodes: Vec::new(),
        }
    }
}

/// A group as the resolver sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    /// Higher weights rank first. Mirrors the group's global `weight` node.
    #[serde(default)]
    pub weight: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Own nodes in insertion order.
    #[serde(default)]
    pub nodes: Vec<Node>,
}

impl Group {
    /// An empty group with weight 0. The name is normalised.
    pub fn new(name: &str) -> Result<Self, Error> {
        Ok(Self {
            name: normalize_group(name)?,
            weight: 0,
            display_name: None,
            nodes: Vec::new(),
        })
    }
}

/// An ordered list of groups for promote and demote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub name: String,
    pub groups: Vec<String>,
}

/// The inherit nodes a promotion or demotion replaces, in the contexts it
/// was asked for. `None` on either side means nothing to remove or add.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackMove {
    pub remove: Option<String>,
    pub add: Option<String>,
}

impl Track {
    /// A track with normalised, distinct group names.
    pub fn new(name: &str, groups: &[&str]) -> Result<Self, Error> {
        let name = normalize_track(name)?;
        let mut normalized = Vec::with_capacity(groups.len());
        for group in groups {
            let group = normalize_group(group)?;
            if normalized.contains(&group) {
                return Err(invalid("track", &name, "lists a group twice"));
            }
            normalized.push(group);
        }
        Ok(Self {
            name,
            groups: normalized,
        })
    }

    /// The index of the user's position on this track, from their unexpired
    /// positive `inherit` nodes whose contexts equal `contexts`. A user with
    /// none is on `default` when the track contains it (the implicit edge).
    fn position(
        &self,
        nodes: &[Node],
        contexts: &ContextSet,
        now: u64,
    ) -> Result<Option<usize>, Error> {
        let mut found = None;
        for node in nodes {
            let Payload::Inherit { group, value: true } = &node.payload else {
                continue;
            };
            if node.contexts != *contexts || expired(node.expires_at, now) {
                continue;
            }
            if let Some(index) = self.groups.iter().position(|g| g == group) {
                if found.is_some_and(|other| other != index) {
                    return Err(self.error("the user is on more than one of its groups"));
                }
                found = Some(index);
            }
        }
        Ok(found.or_else(|| self.groups.iter().position(|g| g == DEFAULT_GROUP)))
    }

    fn error(&self, reason: &'static str) -> Error {
        Error::Track {
            track: self.name.clone(),
            reason,
        }
    }

    /// Move a user up: from nowhere to the first group, otherwise to the next.
    /// Leaving `default` removes nothing because that edge is implicit.
    pub fn promote(
        &self,
        nodes: &[Node],
        contexts: &ContextSet,
        now: u64,
    ) -> Result<TrackMove, Error> {
        let Some(first) = self.groups.first() else {
            return Err(self.error("the track is empty"));
        };
        let Some(index) = self.position(nodes, contexts, now)? else {
            return Ok(TrackMove {
                remove: None,
                add: Some(first.clone()),
            });
        };
        let Some(next) = self.groups.get(index + 1) else {
            return Err(self.error("the user is already on its last group"));
        };
        Ok(TrackMove {
            remove: self.explicit(nodes, index, contexts),
            add: Some(next.clone()),
        })
    }

    /// Move a user down: to the previous group, or off the track from the
    /// first group. Arriving on `default` adds nothing because it is implicit.
    pub fn demote(
        &self,
        nodes: &[Node],
        contexts: &ContextSet,
        now: u64,
    ) -> Result<TrackMove, Error> {
        let Some(index) = self.position(nodes, contexts, now)? else {
            return Err(self.error("the user is not on this track"));
        };
        let remove = self.explicit(nodes, index, contexts);
        if remove.is_none() {
            return Err(self.error("the user is already on its first group"));
        }
        let add = index
            .checked_sub(1)
            .map(|previous| self.groups[previous].clone())
            .filter(|group| group != DEFAULT_GROUP);
        Ok(TrackMove { remove, add })
    }

    /// The group at `index` when the user holds an explicit node for it.
    fn explicit(&self, nodes: &[Node], index: usize, contexts: &ContextSet) -> Option<String> {
        let group = &self.groups[index];
        nodes
            .iter()
            .any(|node| {
                node.contexts == *contexts
                    && matches!(&node.payload, Payload::Inherit { group: g, value: true } if g == group)
            })
            .then(|| group.clone())
    }
}

/// Store limits, sent to backends in the `hello` reply. The defaults keep a
/// compiled subject under 64 KiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    pub groups: usize,
    pub group_nodes: usize,
    pub user_nodes: usize,
    pub parents: usize,
    pub contexts: usize,
    pub tracks: usize,
    /// Cap on all entries of one compiled subject.
    pub compiled_entries: usize,
    /// Most UUIDs in one `subject.batch` request.
    pub batch: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            groups: 1024,
            group_nodes: 256,
            user_nodes: 512,
            parents: 16,
            contexts: 32,
            tracks: 64,
            compiled_entries: 4096,
            batch: 256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_forms() {
        let uuid = Uuid::parse("0123456789ABCDEF0123456789abcdef").unwrap();
        assert_eq!(uuid.to_string(), "0123456789abcdef0123456789abcdef");
        assert_eq!(uuid.hyphenated(), "01234567-89ab-cdef-0123-456789abcdef");
        assert_eq!(Uuid::parse(&uuid.hyphenated()).unwrap(), uuid);
        assert_eq!(Uuid::from_bytes(*uuid.as_bytes()), uuid);
        for bad in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdeg",
            "01234567-89ab-cdef-0123-456789abcdeff",
            "0123456789-ab-cdef-0123-456789abcde",
            "+123456789abcdef0123456789abcdef",
        ] {
            assert!(Uuid::parse(bad).is_err(), "{bad}");
        }
        let json = serde_json::to_string(&uuid).unwrap();
        assert_eq!(json, "\"0123456789abcdef0123456789abcdef\"");
        assert_eq!(serde_json::from_str::<Uuid>(&json).unwrap(), uuid);
    }

    #[test]
    fn names() {
        assert_eq!(normalize_group("VIP").unwrap(), "vip");
        assert!(normalize_group("a:b").is_err());
        assert!(normalize_group("").is_err());
        assert!(normalize_group(&"a".repeat(65)).is_err());
        assert!(valid_name("staff-team_1.eu"));
    }

    fn parent(group: &str) -> Node {
        Node::inherit(group, true).unwrap()
    }

    #[test]
    fn promote_and_demote() {
        let track = Track::new("ranks", &["default", "vip", "mod"]).unwrap();
        let global = ContextSet::new();
        let up = track.promote(&[], &global, 0).unwrap();
        assert_eq!((up.remove, up.add.as_deref()), (None, Some("vip")));
        let up = track.promote(&[parent("vip")], &global, 0).unwrap();
        assert_eq!(
            (up.remove.as_deref(), up.add.as_deref()),
            (Some("vip"), Some("mod"))
        );
        assert!(track.promote(&[parent("mod")], &global, 0).is_err());
        let down = track.demote(&[parent("vip")], &global, 0).unwrap();
        assert_eq!((down.remove.as_deref(), down.add), (Some("vip"), None));
        assert!(track.demote(&[], &global, 0).is_err());
        assert!(
            track
                .promote(&[parent("vip"), parent("mod")], &global, 0)
                .is_err()
        );
        // Other contexts and expired nodes do not count.
        let survival = ContextSet::of("server", "survival").unwrap();
        let elsewhere = parent("vip").with_contexts(survival.clone()).unwrap();
        assert_eq!(
            track
                .promote(std::slice::from_ref(&elsewhere), &global, 0)
                .unwrap()
                .add
                .as_deref(),
            Some("vip")
        );
        assert_eq!(
            track
                .promote(&[elsewhere], &survival, 0)
                .unwrap()
                .add
                .as_deref(),
            Some("mod")
        );
        let lapsed = parent("vip").expiring_at(10);
        assert_eq!(
            track
                .promote(&[lapsed], &global, 10)
                .unwrap()
                .add
                .as_deref(),
            Some("vip")
        );
        let plain = Track::new("staff", &["helper", "mod"]).unwrap();
        let down = plain.demote(&[parent("helper")], &global, 0).unwrap();
        assert_eq!((down.remove.as_deref(), down.add), (Some("helper"), None));
        assert!(Track::new("t", &["a", "A"]).is_err());
    }
}
