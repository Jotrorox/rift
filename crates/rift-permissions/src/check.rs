//! Stage 2: answer checks from a compiled subject. Runs anywhere and
//! allocates nothing for an already-lowercase node.
//!
//! # Rules
//!
//! 1. Entries that are expired (`exp <= now`) or whose contexts are not a
//!    subset of the active contexts are ignored.
//! 2. The most specific match wins: the exact node, then the longest
//!    `prefix.*` (by prefix length), then `*`. `a.*` matches `a.b` and
//!    `a.b.c` but not `a`. A `group` entry for `vip` matches `group.vip`
//!    exactly.
//! 3. Ties: more context pairs win, then the lower rank, then `false` beats
//!    `true`.
//! 4. No match is [`Tristate::Undefined`]; so is a node that is not a valid
//!    pattern after ASCII lowercasing.

use crate::{
    compiled::{CompiledSubject, Decoration, Entry, EntryKind},
    context::ContextSet,
    expired,
    node::{normalize_node, valid_node},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{borrow::Cow, cmp::Reverse, collections::BTreeMap};

/// A check result that keeps "not set" apart from "denied". JSON: `true`,
/// `false` or `null`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Tristate {
    True,
    False,
    #[default]
    Undefined,
}

impl Tristate {
    /// The answer, or `default` when undefined.
    pub fn as_bool(self, default: bool) -> bool {
        self.as_option().unwrap_or(default)
    }

    pub fn as_option(self) -> Option<bool> {
        match self {
            Self::True => Some(true),
            Self::False => Some(false),
            Self::Undefined => None,
        }
    }

    pub fn is_defined(self) -> bool {
        self != Self::Undefined
    }
}

impl From<bool> for Tristate {
    fn from(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }
}

impl From<Option<bool>> for Tristate {
    fn from(value: Option<bool>) -> Self {
        value.map_or(Self::Undefined, Self::from)
    }
}

impl Serialize for Tristate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_option().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Tristate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<bool>::deserialize(deserializer).map(Self::from)
    }
}

/// How closely a pattern matches a node; greater is more specific.
/// JSON: `"any"`, `{"prefix": 4}` (prefix length in bytes, including the dot)
/// or `"exact"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Specificity {
    Any,
    Prefix(usize),
    Exact,
}

/// How `pattern` matches the normalised `node`, if at all.
pub fn pattern_match(pattern: &str, node: &str) -> Option<Specificity> {
    if pattern == node {
        Some(Specificity::Exact)
    } else if pattern == "*" {
        Some(Specificity::Any)
    } else {
        let prefix = pattern.strip_suffix('*')?;
        (prefix.ends_with('.') && node.len() > prefix.len() && node.starts_with(prefix))
            .then_some(Specificity::Prefix(prefix.len()))
    }
}

/// How `entry` matches the normalised `node`, if at all.
pub fn entry_match(entry: &Entry, node: &str) -> Option<Specificity> {
    match entry.kind {
        EntryKind::Permission => pattern_match(&entry.key, node),
        EntryKind::Group => {
            (node.strip_prefix("group.") == Some(entry.key.as_str())).then_some(Specificity::Exact)
        }
    }
}

/// The ordering key of a matching entry; the maximum wins.
type Precedence = (Specificity, usize, Reverse<u32>, bool);

fn precedence(entry: &Entry, specificity: Specificity) -> Precedence {
    (
        specificity,
        entry.contexts.len(),
        Reverse(entry.rank),
        !entry.value,
    )
}

fn normalized(node: &str) -> Option<Cow<'_, str>> {
    if valid_node(node) {
        Some(Cow::Borrowed(node))
    } else {
        normalize_node(node).ok().map(Cow::Owned)
    }
}

/// Resolve `node` for `subject` in the `active` contexts at `now`.
pub fn check(subject: &CompiledSubject, node: &str, active: &ContextSet, now: u64) -> Tristate {
    let Some(node) = normalized(node) else {
        return Tristate::Undefined;
    };
    let mut best: Option<(Precedence, bool)> = None;
    for entry in &subject.entries {
        let Some(specificity) = entry_match(entry, &node) else {
            continue;
        };
        if expired(entry.expires_at, now) || !entry.contexts.is_subset(active) {
            continue;
        }
        let key = precedence(entry, specificity);
        if best.is_none_or(|(known, _)| key > known) {
            best = Some((key, entry.value));
        }
    }
    best.map_or(Tristate::Undefined, |(_, value)| value.into())
}

/// Why an entry did not decide a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// It decided the check.
    Won,
    Expired,
    /// Its contexts are not all active.
    ContextMismatch,
    LessSpecific,
    FewerContexts,
    HigherRank,
    /// Same specificity, contexts and rank: `false` beats `true`.
    DenyWins,
    /// Identical precedence and value to the winner, which came first.
    Duplicate,
}

/// A matching entry and how it fared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub entry: Entry,
    pub specificity: Specificity,
    pub outcome: Outcome,
}

/// The explanation behind a check, for `/perms trace`. `losers` lists every
/// other entry matching the node, in compiled order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trace {
    pub node: String,
    pub result: Tristate,
    pub winner: Option<Candidate>,
    pub losers: Vec<Candidate>,
}

/// [`check`] with every matching entry and why it won or lost.
pub fn trace(subject: &CompiledSubject, node: &str, active: &ContextSet, now: u64) -> Trace {
    let Some(node) = normalized(node) else {
        return Trace {
            node: node.to_owned(),
            result: Tristate::Undefined,
            winner: None,
            losers: Vec::new(),
        };
    };
    let mut matching = Vec::new();
    let mut best: Option<(usize, Precedence)> = None;
    for entry in &subject.entries {
        let Some(specificity) = entry_match(entry, &node) else {
            continue;
        };
        let outcome = if expired(entry.expires_at, now) {
            Some(Outcome::Expired)
        } else if !entry.contexts.is_subset(active) {
            Some(Outcome::ContextMismatch)
        } else {
            let key = precedence(entry, specificity);
            if best.is_none_or(|(_, known)| key > known) {
                best = Some((matching.len(), key));
            }
            None
        };
        matching.push((entry, specificity, outcome));
    }
    let mut winner = None;
    let mut losers = Vec::new();
    for (index, (entry, specificity, outcome)) in matching.into_iter().enumerate() {
        let outcome = outcome.unwrap_or_else(|| match best {
            Some((won, _)) if won == index => Outcome::Won,
            Some((_, (spec, contexts, rank, deny))) => {
                if specificity < spec {
                    Outcome::LessSpecific
                } else if entry.contexts.len() < contexts {
                    Outcome::FewerContexts
                } else if Reverse(entry.rank) < rank {
                    Outcome::HigherRank
                } else if !entry.value != deny {
                    Outcome::DenyWins
                } else {
                    Outcome::Duplicate
                }
            }
            None => unreachable!("an applicable entry always sets a best match"),
        });
        let candidate = Candidate {
            entry: entry.clone(),
            specificity,
            outcome,
        };
        if outcome == Outcome::Won {
            winner = Some(candidate);
        } else {
            losers.push(candidate);
        }
    }
    Trace {
        node: node.into_owned(),
        result: winner
            .as_ref()
            .map_or(Tristate::Undefined, |c| c.entry.value.into()),
        winner,
        losers,
    }
}

/// Chat metadata in one context. JSON:
/// `{"prefix": "&c[Admin] ", "suffix": null, "meta": {"homes": "5"}, "primary_group": "admin"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaView {
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub meta: BTreeMap<String, String>,
    pub primary_group: String,
}

/// The winning decoration: highest priority, then lower rank, then more
/// context pairs, then the first listed.
fn decoration<'a>(list: &'a [Decoration], active: &ContextSet, now: u64) -> Option<&'a str> {
    type Key = (i32, Reverse<u32>, usize);
    let mut best: Option<(&Decoration, Key)> = None;
    for item in list {
        if expired(item.expires_at, now) || !item.contexts.is_subset(active) {
            continue;
        }
        let key = (item.priority, Reverse(item.rank), item.contexts.len());
        if best.is_none_or(|(_, known)| key > known) {
            best = Some((item, key));
        }
    }
    best.map(|(item, _)| item.text.as_str())
}

/// Prefix, suffix, meta and primary group for `subject` in `active` at
/// `now`. Per meta key the lowest rank wins, then more context pairs, then
/// the first listed.
pub fn meta(subject: &CompiledSubject, active: &ContextSet, now: u64) -> MetaView {
    let mut best: BTreeMap<&str, (&str, (Reverse<u32>, usize))> = BTreeMap::new();
    for item in &subject.meta {
        if expired(item.expires_at, now) || !item.contexts.is_subset(active) {
            continue;
        }
        let key = (Reverse(item.rank), item.contexts.len());
        match best.get(item.key.as_str()) {
            Some((_, known)) if key <= *known => {}
            _ => {
                best.insert(&item.key, (&item.value, key));
            }
        }
    }
    MetaView {
        prefix: decoration(&subject.prefix, active, now).map(str::to_owned),
        suffix: decoration(&subject.suffix, active, now).map(str::to_owned),
        meta: best
            .into_iter()
            .map(|(key, (value, _))| (key.to_owned(), value.to_owned()))
            .collect(),
        primary_group: subject.primary_group.clone(),
    }
}

/// The groups the user is in for `active` at `now`, in rank order, each once.
pub fn groups<'a>(subject: &'a CompiledSubject, active: &ContextSet, now: u64) -> Vec<&'a str> {
    let mut names: Vec<&str> = Vec::new();
    for entry in &subject.entries {
        if entry.kind == EntryKind::Group
            && !expired(entry.expires_at, now)
            && entry.contexts.is_subset(active)
            && !names.contains(&entry.key.as_str())
        {
            names.push(&entry.key);
        }
    }
    names
}

impl CompiledSubject {
    /// See [`check`].
    pub fn check(&self, node: &str, active: &ContextSet, now: u64) -> Tristate {
        check(self, node, active, now)
    }

    /// See [`trace`].
    pub fn trace(&self, node: &str, active: &ContextSet, now: u64) -> Trace {
        trace(self, node, active, now)
    }

    /// See [`meta`].
    pub fn meta(&self, active: &ContextSet, now: u64) -> MetaView {
        meta(self, active, now)
    }

    /// See [`groups`].
    pub fn groups(&self, active: &ContextSet, now: u64) -> Vec<&str> {
        groups(self, active, now)
    }

    /// Whether the user is in `group` (directly or inherited) for `active`.
    pub fn in_group(&self, group: &str, active: &ContextSet, now: u64) -> bool {
        self.entries.iter().any(|entry| {
            entry.kind == EntryKind::Group
                && entry.key.eq_ignore_ascii_case(group)
                && !expired(entry.expires_at, now)
                && entry.contexts.is_subset(active)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compiled::MetaEntry, subject::Uuid};

    fn entry(key: &str, value: bool, contexts: &str, rank: u32) -> Entry {
        Entry {
            kind: EntryKind::Permission,
            key: key.into(),
            value,
            contexts: ContextSet::parse(contexts).unwrap(),
            rank,
            expires_at: None,
        }
    }

    fn subject(entries: Vec<Entry>) -> CompiledSubject {
        CompiledSubject {
            revision: 1,
            uuid: Uuid::from_u128(1),
            primary_group: "default".into(),
            entries,
            prefix: vec![],
            suffix: vec![],
            meta: vec![],
        }
    }

    fn ctx(text: &str) -> ContextSet {
        ContextSet::parse(text).unwrap()
    }

    #[test]
    fn specificity() {
        assert_eq!(pattern_match("a.b", "a.b"), Some(Specificity::Exact));
        assert_eq!(pattern_match("a.*", "a.b.c"), Some(Specificity::Prefix(2)));
        assert_eq!(
            pattern_match("a.b.*", "a.b.c"),
            Some(Specificity::Prefix(4))
        );
        assert_eq!(pattern_match("a.*", "a"), None);
        assert_eq!(pattern_match("a.*", "ab.c"), None);
        assert_eq!(pattern_match("*", "a"), Some(Specificity::Any));
        assert!(Specificity::Exact > Specificity::Prefix(200));
        assert!(Specificity::Prefix(4) > Specificity::Prefix(2));
        assert!(Specificity::Prefix(2) > Specificity::Any);
    }

    #[test]
    fn precedence_order() {
        let s = subject(vec![
            entry("*", true, "", 0),
            entry("a.*", false, "", 3),
            entry("a.b", true, "", 5),
        ]);
        let none = ContextSet::new();
        assert_eq!(s.check("a.b", &none, 0), Tristate::True);
        assert_eq!(s.check("a.c", &none, 0), Tristate::False);
        assert_eq!(s.check("z", &none, 0), Tristate::True);
        assert_eq!(s.check("A.B", &none, 0), Tristate::True);
        assert_eq!(s.check("a..b", &none, 0), Tristate::Undefined);
        assert_eq!(subject(vec![]).check("a", &none, 0), Tristate::Undefined);
        // Contexts, then rank, then deny.
        let s = subject(vec![
            entry("x", true, "", 0),
            entry("x", false, "server=s", 2),
            entry("y", true, "", 2),
            entry("y", false, "", 1),
            entry("z", true, "", 1),
            entry("z", false, "", 1),
        ]);
        assert_eq!(s.check("x", &none, 0), Tristate::True);
        assert_eq!(s.check("x", &ctx("server=s"), 0), Tristate::False);
        assert_eq!(s.check("y", &none, 0), Tristate::False);
        assert_eq!(s.check("z", &none, 0), Tristate::False);
    }

    #[test]
    fn expiry_is_exact() {
        let mut e = entry("fly", true, "", 0);
        e.expires_at = Some(10);
        let s = subject(vec![e]);
        assert_eq!(s.check("fly", &ContextSet::new(), 9), Tristate::True);
        assert_eq!(s.check("fly", &ContextSet::new(), 10), Tristate::Undefined);
    }

    #[test]
    fn group_entries_answer_group_nodes() {
        let mut vip = entry("vip", true, "server=s", 1);
        vip.kind = EntryKind::Group;
        let s = subject(vec![vip]);
        assert_eq!(s.check("group.vip", &ctx("server=s"), 0), Tristate::True);
        assert_eq!(
            s.check("group.vip", &ContextSet::new(), 0),
            Tristate::Undefined
        );
        assert_eq!(s.check("vip", &ctx("server=s"), 0), Tristate::Undefined);
        assert!(s.in_group("VIP", &ctx("server=s"), 0));
        assert_eq!(s.groups(&ctx("server=s"), 0), ["vip"]);
    }

    #[test]
    fn trace_explains_losers() {
        let mut old = entry("a.b", true, "", 0);
        old.expires_at = Some(1);
        let s = subject(vec![
            old,
            entry("a.b", true, "server=x", 0),
            entry("a.*", true, "", 0),
            entry("a.b", true, "", 2),
            entry("a.b", true, "", 1),
            entry("a.b", false, "", 1),
            entry("a.b", false, "", 1),
            entry("other", true, "", 0),
        ]);
        let t = s.trace("A.B", &ContextSet::new(), 5);
        assert_eq!(t.node, "a.b");
        assert_eq!(t.result, Tristate::False);
        assert_eq!(t.winner.as_ref().unwrap().entry, entry("a.b", false, "", 1));
        let outcomes: Vec<_> = t.losers.iter().map(|c| c.outcome).collect();
        assert_eq!(
            outcomes,
            [
                Outcome::Expired,
                Outcome::ContextMismatch,
                Outcome::LessSpecific,
                Outcome::HigherRank,
                Outcome::DenyWins,
                Outcome::Duplicate,
            ]
        );
        let s = subject(vec![entry("a", true, "", 0), entry("a", true, "w=x", 0)]);
        let t = s.trace("a", &ctx("w=x"), 0);
        assert_eq!(t.losers[0].outcome, Outcome::FewerContexts);
        assert_eq!(
            s.trace("a..", &ContextSet::new(), 0).result,
            Tristate::Undefined
        );
    }

    #[test]
    fn meta_view() {
        let deco = |p, t: &str, c: &str, rank| Decoration {
            priority: p,
            text: t.into(),
            contexts: ctx(c),
            rank,
            expires_at: None,
        };
        let meta = |k: &str, v: &str, c: &str, rank| MetaEntry {
            key: k.into(),
            value: v.into(),
            contexts: ctx(c),
            rank,
            expires_at: None,
        };
        let mut s = subject(vec![]);
        s.prefix = vec![
            deco(10, "low", "", 0),
            deco(50, "far", "", 3),
            deco(50, "near", "", 2),
            deco(99, "elsewhere", "server=x", 0),
        ];
        s.meta = vec![
            meta("homes", "1", "", 2),
            meta("homes", "3", "", 1),
            meta("homes", "9", "server=s", 1),
            meta("color", "red", "", 4),
        ];
        let view = s.meta(&ctx("server=s"), 0);
        assert_eq!(view.prefix.as_deref(), Some("near"));
        assert_eq!(view.suffix, None);
        assert_eq!(view.meta["homes"], "9");
        assert_eq!(view.meta["color"], "red");
        assert_eq!(view.primary_group, "default");
        assert_eq!(s.meta(&ContextSet::new(), 0).meta["homes"], "3");
    }

    #[test]
    fn tristate() {
        assert!(Tristate::Undefined.as_bool(true));
        assert!(!Tristate::False.as_bool(true));
        assert_eq!(serde_json::to_string(&Tristate::Undefined).unwrap(), "null");
        assert_eq!(
            serde_json::from_str::<Tristate>("false").unwrap(),
            Tristate::False
        );
        assert_eq!(Tristate::from(Some(true)), Tristate::True);
    }
}
