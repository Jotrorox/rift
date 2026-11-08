//! Context sets: the `key=value` conditions a node applies under, and the
//! active contexts a check runs in.
//!
//! A set may hold several values for one key. A node's contexts are satisfied
//! when *every* pair of the node is present in the active set (subset test),
//! so `server=a;server=b` on a node requires both values, which only a
//! multi-valued custom key can provide. LuckPerms' "any value per key" sets are
//! expanded into one node per combination on import, which keeps the subset
//! test exact (see [`crate::luckperms`]). The reserved keys are single-valued
//! in every active set, so a node naming two values for one of them can never
//! apply and is rejected ([`ContextSet::is_satisfiable`]).

use crate::error::{Error, invalid};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de, ser::SerializeMap};
use std::{fmt, str::FromStr};

/// The backend name a player is on.
pub const SERVER: &str = "server";
/// The world name.
pub const WORLD: &str = "world";
/// The dimension, for example `minecraft:the_nether`.
pub const DIMENSION: &str = "dimension";
/// The game mode: `survival`, `creative`, `adventure` or `spectator`.
pub const GAMEMODE: &str = "gamemode";
/// Keys Rift and the adapters fill themselves; each has one active value.
pub const RESERVED: [&str; 4] = [SERVER, WORLD, DIMENSION, GAMEMODE];

/// Longest context key in bytes.
pub const MAX_KEY_LEN: usize = 64;
/// Longest context value in bytes.
pub const MAX_VALUE_LEN: usize = 256;

/// A sorted, duplicate-free set of `key=value` pairs.
///
/// Keys are `[a-z0-9_.:-]`, 1 to 64 bytes. Values are 1 to 256 bytes without
/// control characters, `;` or `=`. ASCII letters in both are lowercased on
/// insertion; other characters are kept as given.
///
/// The canonical string form is `k=v;k2=v2`, sorted by key then value (the
/// `perm_node.contexts` column); the empty set is the empty string. In JSON a
/// set is an object whose values are a string, or an array of strings when a
/// key has more than one value: `{"server": "survival", "region": ["a", "b"]}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContextSet(Vec<(String, String)>);

fn key_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b':' | b'-')
}

fn normalize_key(key: &str) -> Result<String, Error> {
    let key = key.to_ascii_lowercase();
    if key.is_empty() || key.len() > MAX_KEY_LEN || !key.bytes().all(key_byte) {
        return Err(invalid(
            "context key",
            &key,
            "use 1 to 64 bytes of [a-z0-9_.:-]",
        ));
    }
    Ok(key)
}

fn normalize_value(value: &str) -> Result<String, Error> {
    if value.is_empty()
        || value.len() > MAX_VALUE_LEN
        || value
            .chars()
            .any(|c| c.is_control() || c == ';' || c == '=')
    {
        return Err(invalid(
            "context value",
            value,
            "use 1 to 256 bytes without control characters, ';' or '='",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

impl ContextSet {
    /// The empty (global) set.
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// A set with one pair.
    pub fn of(key: &str, value: &str) -> Result<Self, Error> {
        Self::new().with(key, value)
    }

    /// A set from pairs, normalising each.
    pub fn from_pairs<K: AsRef<str>, V: AsRef<str>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Result<Self, Error> {
        let mut set = Self::new();
        for (key, value) in pairs {
            set.insert(key.as_ref(), value.as_ref())?;
        }
        Ok(set)
    }

    /// Parse the canonical `k=v;k2=v2` form; the empty string is the empty set.
    pub fn parse(canonical: &str) -> Result<Self, Error> {
        let mut set = Self::new();
        if canonical.is_empty() {
            return Ok(set);
        }
        for pair in canonical.split(';') {
            let Some((key, value)) = pair.split_once('=') else {
                return Err(invalid("context set", canonical, "expected k=v;k2=v2"));
            };
            set.insert(key, value)?;
        }
        Ok(set)
    }

    /// Add a pair; returns whether it was new.
    pub fn insert(&mut self, key: &str, value: &str) -> Result<bool, Error> {
        let pair = (normalize_key(key)?, normalize_value(value)?);
        match self.0.binary_search(&pair) {
            Ok(_) => Ok(false),
            Err(at) => {
                self.0.insert(at, pair);
                Ok(true)
            }
        }
    }

    /// [`insert`](Self::insert) by value, for chaining.
    pub fn with(mut self, key: &str, value: &str) -> Result<Self, Error> {
        self.insert(key, value)?;
        Ok(self)
    }

    /// Replace every value of `key` with `value`, as an adapter does when a
    /// player changes world.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), Error> {
        let (key, value) = (normalize_key(key)?, normalize_value(value)?);
        self.0.retain(|(k, _)| *k != key);
        let pair = (key, value);
        let at = self.0.binary_search(&pair).unwrap_err();
        self.0.insert(at, pair);
        Ok(())
    }

    /// Remove one pair; returns whether it was present. Arguments are
    /// compared after ASCII lowercasing.
    pub fn remove(&mut self, key: &str, value: &str) -> bool {
        let before = self.0.len();
        self.0
            .retain(|(k, v)| !(k.eq_ignore_ascii_case(key) && v.eq_ignore_ascii_case(value)));
        self.0.len() != before
    }

    /// Remove every value of `key`; returns whether any was present.
    pub fn remove_key(&mut self, key: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
        self.0.len() != before
    }

    /// Whether the normalised pair is present. Does not allocate.
    pub fn contains(&self, key: &str, value: &str) -> bool {
        self.0
            .binary_search_by(|(k, v)| (k.as_str(), v.as_str()).cmp(&(key, value)))
            .is_ok()
    }

    /// The values of `key`, sorted.
    pub fn values<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.iter().filter(move |(k, _)| *k == key).map(|(_, v)| v)
    }

    /// The first value of `key`; for reserved keys, the only one.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    /// Pairs in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Number of pairs; the tie-break "more context keys" counts these.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether every pair of `self` is in `active`: the node-applies test.
    pub fn is_subset(&self, active: &Self) -> bool {
        self.0.len() <= active.0.len() && self.iter().all(|(k, v)| active.contains(k, v))
    }

    /// Both sets' pairs; used to merge an inheritance edge's contexts into a
    /// parent's nodes.
    pub fn union(&self, other: &Self) -> Self {
        if other.is_empty() {
            return self.clone();
        }
        if self.is_empty() {
            return other.clone();
        }
        let mut pairs = Vec::with_capacity(self.0.len() + other.0.len());
        pairs.extend(self.0.iter().cloned());
        pairs.extend(other.0.iter().cloned());
        pairs.sort();
        pairs.dedup();
        Self(pairs)
    }

    /// False when a reserved key has two values, which no active set can
    /// satisfy.
    pub fn is_satisfiable(&self) -> bool {
        self.0
            .windows(2)
            .all(|w| w[0].0 != w[1].0 || !RESERVED.contains(&w[0].0.as_str()))
    }
}

impl fmt::Display for ContextSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (key, value)) in self.iter().enumerate() {
            if index > 0 {
                f.write_str(";")?;
            }
            write!(f, "{key}={value}")?;
        }
        Ok(())
    }
}

impl FromStr for ContextSet {
    type Err = Error;
    fn from_str(canonical: &str) -> Result<Self, Error> {
        Self::parse(canonical)
    }
}

impl Serialize for ContextSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        let mut rest = self.0.as_slice();
        while let Some((key, _)) = rest.first() {
            let count = rest.iter().take_while(|(k, _)| k == key).count();
            let (group, tail) = rest.split_at(count);
            if count == 1 {
                map.serialize_entry(key, &group[0].1)?;
            } else {
                let values: Vec<&str> = group.iter().map(|(_, v)| v.as_str()).collect();
                map.serialize_entry(key, &values)?;
            }
            rest = tail;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ContextSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Values {
            One(String),
            Many(Vec<String>),
        }
        struct Visitor;
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = ContextSet;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object of context keys to a string or an array of strings")
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<ContextSet, A::Error> {
                let mut set = ContextSet::new();
                while let Some((key, values)) = map.next_entry::<String, Values>()? {
                    let values = match values {
                        Values::One(value) => vec![value],
                        Values::Many(values) => values,
                    };
                    for value in values {
                        set.insert(&key, &value).map_err(de::Error::custom)?;
                    }
                }
                Ok(set)
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_form_sorts_and_lowercases() {
        let set = ContextSet::parse("World=Nether;server=survival;region=b;region=a").unwrap();
        assert_eq!(
            set.to_string(),
            "region=a;region=b;server=survival;world=nether"
        );
        assert_eq!(ContextSet::parse(&set.to_string()).unwrap(), set);
        assert_eq!(ContextSet::parse("").unwrap(), ContextSet::new());
        assert_eq!(set.get("server"), Some("survival"));
        assert_eq!(set.values("region").collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn rejects_malformed_pairs() {
        for bad in [
            "server", "server=", "=x", "a b=c", "k=v=w", "k=v;", "k=\u{7}",
        ] {
            assert!(ContextSet::parse(bad).is_err(), "{bad}");
        }
        assert!(ContextSet::of(&"k".repeat(65), "v").is_err());
        assert!(ContextSet::of("k", &"v".repeat(257)).is_err());
        assert!(ContextSet::of("dimension", "minecraft:the_nether").is_ok());
    }

    #[test]
    fn subset_union_and_satisfiable() {
        let node = ContextSet::of("server", "survival").unwrap();
        let active = ContextSet::parse("server=survival;world=world").unwrap();
        assert!(node.is_subset(&active));
        assert!(ContextSet::new().is_subset(&active));
        assert!(!active.is_subset(&node));
        let both = node.union(&ContextSet::of("server", "lobby").unwrap());
        assert_eq!(both.to_string(), "server=lobby;server=survival");
        assert!(!both.is_satisfiable());
        assert!(
            ContextSet::parse("region=a;region=b")
                .unwrap()
                .is_satisfiable()
        );
    }

    #[test]
    fn set_remove_and_contains() {
        let mut set = ContextSet::parse("region=a;region=b;world=w").unwrap();
        set.set("region", "C").unwrap();
        assert_eq!(set.to_string(), "region=c;world=w");
        assert!(set.contains("region", "c"));
        assert!(set.remove("REGION", "C"));
        assert!(!set.remove("region", "c"));
        assert!(set.remove_key("world"));
        assert!(set.is_empty());
    }

    #[test]
    fn json_uses_strings_and_arrays() {
        let set = ContextSet::parse("region=a;region=b;server=survival").unwrap();
        let json = serde_json::to_value(&set).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"region": ["a", "b"], "server": "survival"})
        );
        let back: ContextSet = serde_json::from_value(json).unwrap();
        assert_eq!(back, set);
        let upper: ContextSet = serde_json::from_str(r#"{"Server":"Lobby"}"#).unwrap();
        assert_eq!(upper.to_string(), "server=lobby");
        assert!(serde_json::from_str::<ContextSet>(r#"{"server":""}"#).is_err());
        assert!(serde_json::from_str::<ContextSet>(r#"{"server":1}"#).is_err());
    }
}
