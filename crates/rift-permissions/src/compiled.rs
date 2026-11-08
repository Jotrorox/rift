//! Stage 1: compile a user into a flat, ranked list of entries. Runs on the
//! proxy, the only place that walks inheritance.
//!
//! # Rules
//!
//! 1. The user's own nodes get rank 0.
//! 2. Parents come from unexpired `inherit` nodes, walked breadth-first from
//!    the user, plus the implicit edge to `default` at depth 1. An edge keeps
//!    its contexts: a parent reached through `server=survival` contributes its
//!    nodes with `server=survival` merged in, and contexts accumulate along a
//!    path. A path whose merged contexts give a reserved key two values can
//!    never apply and is dropped; a path's expiry is the earliest expiry on it.
//!    Edges to groups that do not exist are skipped (dangling).
//! 3. A negated `inherit` (value `false`) blocks the group on every path whose
//!    merged contexts include the negation's own merged contexts, so a global
//!    block removes the group entirely and a `server=x` block removes only the
//!    paths that already apply only on `server=x`. Negations are collected
//!    from every subject reachable before blocking is applied, in one pass.
//! 4. A group reached through several paths keeps each path that is not
//!    dominated by another (one with a subset of its contexts that lasts at
//!    least as long). Cycles end because a revisit is always dominated.
//! 5. Groups are ranked 1, 2, 3, … by weight descending, then shortest
//!    depth, then name; `default` is always last.
//! 6. Each path of a group adds a `group` entry, then the group's
//!    permission, prefix, suffix and meta nodes in insertion order, with the
//!    path's contexts merged and the group's rank. Expired nodes are skipped.
//!
//! The primary group is the user's explicit `primary_group` when that group
//! exists, otherwise the heaviest group among the user's own unexpired,
//! global, positive `inherit` nodes that survived blocking (ties by name),
//! otherwise `default`. Expiry inside the compiled subject is exact at check
//! time; changes that depend on expiry during compilation (an expiring block
//! or primary group) take effect when the proxy recompiles after its sweeper
//! removes the node.

use crate::{
    context::ContextSet,
    error::Error,
    expired,
    node::{Node, Payload},
    subject::{DEFAULT_GROUP, Group, User, Uuid},
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap, VecDeque},
    hash::BuildHasher,
};

/// Default cap on all entries of a compiled subject.
pub const MAX_ENTRIES: usize = 4096;

/// What an [`Entry`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EntryKind {
    /// A permission pattern (`"perm"`).
    #[serde(rename = "perm")]
    Permission,
    /// Membership of the group named by `key` (`"group"`); matches the
    /// permission `group.<key>` exactly, as in LuckPerms.
    #[serde(rename = "group")]
    Group,
}

/// One ranked permission or group membership.
///
/// JSON: `{"k": "perm", "key": "a.b", "v": true, "ctx": {}, "rank": 0, "exp": 1762800000}`,
/// `exp` omitted when permanent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Entry {
    #[serde(rename = "k")]
    pub kind: EntryKind,
    /// The pattern, or the group name.
    pub key: String,
    /// Always `true` for group entries.
    #[serde(rename = "v")]
    pub value: bool,
    #[serde(rename = "ctx", default)]
    pub contexts: ContextSet,
    /// 0 for the user's own nodes, 1… per group in rank order.
    pub rank: u32,
    #[serde(rename = "exp", default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// A ranked prefix or suffix. JSON: `{"p": 100, "t": "&6[VIP] ", "ctx": {}, "rank": 1}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Decoration {
    #[serde(rename = "p")]
    pub priority: i32,
    #[serde(rename = "t")]
    pub text: String,
    #[serde(rename = "ctx", default)]
    pub contexts: ContextSet,
    pub rank: u32,
    #[serde(rename = "exp", default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// A ranked meta value. JSON: `{"key": "homes", "v": "5", "ctx": {}, "rank": 1}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetaEntry {
    pub key: String,
    #[serde(rename = "v")]
    pub value: String,
    #[serde(rename = "ctx", default)]
    pub contexts: ContextSet,
    pub rank: u32,
    #[serde(rename = "exp", default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// The flattened view of one user that every backend evaluates. Its JSON is
/// the `rift.perms.update.<uuid>` payload and the `subject.get` reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledSubject {
    /// The store revision this was built from.
    pub revision: u64,
    pub uuid: Uuid,
    pub primary_group: String,
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub prefix: Vec<Decoration>,
    #[serde(default)]
    pub suffix: Vec<Decoration>,
    #[serde(default)]
    pub meta: Vec<MetaEntry>,
}

impl CompiledSubject {
    /// Total entries of all lists, the quantity [`MAX_ENTRIES`] caps.
    pub fn len(&self) -> usize {
        self.entries.len() + self.prefix.len() + self.suffix.len() + self.meta.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The name of the subject behind `rank`: `None` for the user's own rank
    /// 0, otherwise the group whose `group` entry carries that rank.
    pub fn rank_subject(&self, rank: u32) -> Option<&str> {
        self.entries
            .iter()
            .find(|e| e.kind == EntryKind::Group && e.rank == rank && rank > 0)
            .map(|e| e.key.as_str())
    }

    /// The earliest expiry of any entry, when a cache might want to refresh.
    pub fn next_expiry(&self) -> Option<u64> {
        let entries = self.entries.iter().map(|e| e.expires_at);
        let prefix = self.prefix.iter().chain(&self.suffix).map(|d| d.expires_at);
        let meta = self.meta.iter().map(|m| m.expires_at);
        entries.chain(prefix).chain(meta).flatten().min()
    }
}

/// Read access to groups by name, for [`compile`].
pub trait GroupLookup {
    fn group(&self, name: &str) -> Option<&Group>;
}

impl<S: BuildHasher> GroupLookup for HashMap<String, Group, S> {
    fn group(&self, name: &str) -> Option<&Group> {
        self.get(name)
    }
}

impl GroupLookup for BTreeMap<String, Group> {
    fn group(&self, name: &str) -> Option<&Group> {
        self.get(name)
    }
}

/// One way a group is reached.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Path {
    contexts: ContextSet,
    expires_at: Option<u64>,
    depth: u32,
}

/// The earlier of two optional expiries (`None` never expires).
fn earliest(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

/// Whether `a` applies wherever and whenever `b` does.
fn dominates(a: &Path, b: &Path) -> bool {
    let lasts = match (a.expires_at, b.expires_at) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(a), Some(b)) => a >= b,
    };
    lasts && a.contexts.is_subset(&b.contexts)
}

struct Block {
    group: String,
    contexts: ContextSet,
}

/// Surviving paths per group name.
type Paths = BTreeMap<String, Vec<Path>>;

/// Breadth-first walk of inheritance; returns the paths per group and the
/// negations seen on the way.
fn walk(
    user: &User,
    groups: &impl GroupLookup,
    now: u64,
    blocks: &[Block],
    limit: usize,
) -> Result<(Paths, Vec<Block>), Error> {
    let mut paths = Paths::new();
    let mut found = Vec::new();
    let mut queue = VecDeque::new();
    let mut total = 0;
    let mut visit = |group: &str, path: Path, queue: &mut VecDeque<(String, Path)>| {
        if blocks
            .iter()
            .any(|b| b.group == group && b.contexts.is_subset(&path.contexts))
        {
            return Ok(());
        }
        let list = paths.entry(group.to_owned()).or_default();
        if list.iter().any(|known| dominates(known, &path)) {
            return Ok(());
        }
        total += 1;
        if total > limit {
            return Err(Error::TooManyEntries { limit });
        }
        list.push(path.clone());
        queue.push_back((group.to_owned(), path));
        Ok(())
    };
    let edges = |nodes: &[Node], from: &Path, found: &mut Vec<Block>| {
        let mut next = Vec::new();
        for node in nodes {
            let Payload::Inherit { group, value } = &node.payload else {
                continue;
            };
            if node.is_expired(now) {
                continue;
            }
            let contexts = from.contexts.union(&node.contexts);
            if !contexts.is_satisfiable() {
                continue;
            }
            if !value {
                found.push(Block {
                    group: group.clone(),
                    contexts,
                });
            } else if group == DEFAULT_GROUP || groups.group(group).is_some() {
                let path = Path {
                    contexts,
                    expires_at: earliest(from.expires_at, node.expires_at),
                    depth: from.depth + 1,
                };
                next.push((group.clone(), path));
            }
        }
        next
    };
    let root = Path {
        contexts: ContextSet::new(),
        expires_at: None,
        depth: 0,
    };
    for (group, path) in edges(&user.nodes, &root, &mut found) {
        visit(&group, path, &mut queue)?;
    }
    let implicit = Path { depth: 1, ..root };
    visit(DEFAULT_GROUP, implicit, &mut queue)?;
    while let Some((name, from)) = queue.pop_front() {
        if let Some(group) = groups.group(&name) {
            for (parent, path) in edges(&group.nodes, &from, &mut found) {
                visit(&parent, path, &mut queue)?;
            }
        }
    }
    for list in paths.values_mut() {
        let all = std::mem::take(list);
        for (index, path) in all.iter().enumerate() {
            let dominated = all.iter().enumerate().any(|(other, known)| {
                other != index
                    && dominates(known, path)
                    && (!dominates(path, known) || other < index)
            });
            if !dominated {
                list.push(path.clone());
            }
        }
        list.sort();
    }
    paths.retain(|_, list| !list.is_empty());
    Ok((paths, found))
}

struct Builder {
    subject: CompiledSubject,
    limit: usize,
    now: u64,
}

impl Builder {
    fn check(&self) -> Result<(), Error> {
        if self.subject.len() > self.limit {
            return Err(Error::TooManyEntries { limit: self.limit });
        }
        Ok(())
    }

    fn nodes(&mut self, nodes: &[Node], rank: u32, path: &Path) -> Result<(), Error> {
        for node in nodes {
            if node.is_expired(self.now) {
                continue;
            }
            let contexts = path.contexts.union(&node.contexts);
            if !contexts.is_satisfiable() {
                continue;
            }
            let expires_at = earliest(path.expires_at, node.expires_at);
            match &node.payload {
                Payload::Permission { key, value } => self.subject.entries.push(Entry {
                    kind: EntryKind::Permission,
                    key: key.clone(),
                    value: *value,
                    contexts,
                    rank,
                    expires_at,
                }),
                Payload::Prefix { priority, text } | Payload::Suffix { priority, text } => {
                    let decoration = Decoration {
                        priority: *priority,
                        text: text.clone(),
                        contexts,
                        rank,
                        expires_at,
                    };
                    if matches!(node.payload, Payload::Prefix { .. }) {
                        self.subject.prefix.push(decoration);
                    } else {
                        self.subject.suffix.push(decoration);
                    }
                }
                Payload::Meta { key, value } => self.subject.meta.push(MetaEntry {
                    key: key.clone(),
                    value: value.clone(),
                    contexts,
                    rank,
                    expires_at,
                }),
                Payload::Inherit { .. } | Payload::Weight { .. } | Payload::DisplayName { .. } => {
                    continue;
                }
            }
            self.check()?;
        }
        Ok(())
    }
}

/// Compile `user` against `groups` at `now`, tagging the result with
/// `revision`. Fails with [`Error::TooManyEntries`] beyond [`MAX_ENTRIES`].
pub fn compile(
    user: &User,
    groups: &impl GroupLookup,
    now: u64,
    revision: u64,
) -> Result<CompiledSubject, Error> {
    compile_with_limit(user, groups, now, revision, MAX_ENTRIES)
}

/// [`compile`] with an explicit entry cap.
pub fn compile_with_limit(
    user: &User,
    groups: &impl GroupLookup,
    now: u64,
    revision: u64,
    limit: usize,
) -> Result<CompiledSubject, Error> {
    let (_, blocks) = walk(user, groups, now, &[], limit)?;
    let (paths, _) = walk(user, groups, now, &blocks, limit)?;
    let weight = |name: &str| groups.group(name).map_or(0, |g| g.weight);
    let mut ranked: Vec<(&String, &Vec<Path>)> = paths.iter().collect();
    ranked.sort_by_key(|(name, list)| {
        let depth = list.iter().map(|p| p.depth).min();
        (*name == DEFAULT_GROUP, Reverse(weight(name)), depth, *name)
    });
    let mut builder = Builder {
        subject: CompiledSubject {
            revision,
            uuid: user.uuid,
            primary_group: primary_group(user, groups, &paths, now),
            entries: Vec::new(),
            prefix: Vec::new(),
            suffix: Vec::new(),
            meta: Vec::new(),
        },
        limit,
        now,
    };
    let root = Path {
        contexts: ContextSet::new(),
        expires_at: None,
        depth: 0,
    };
    builder.nodes(&user.nodes, 0, &root)?;
    for (rank, (name, list)) in (1..).zip(ranked) {
        for path in list {
            builder.subject.entries.push(Entry {
                kind: EntryKind::Group,
                key: name.clone(),
                value: true,
                contexts: path.contexts.clone(),
                rank,
                expires_at: path.expires_at,
            });
            builder.check()?;
            if let Some(group) = groups.group(name) {
                builder.nodes(&group.nodes, rank, path)?;
            }
        }
    }
    Ok(builder.subject)
}

fn primary_group(user: &User, groups: &impl GroupLookup, paths: &Paths, now: u64) -> String {
    let exists = |name: &str| name == DEFAULT_GROUP || groups.group(name).is_some();
    if let Some(explicit) = user.primary_group.as_deref().filter(|name| exists(name)) {
        return explicit.to_owned();
    }
    user.nodes
        .iter()
        .filter(|node| node.contexts.is_empty() && !expired(node.expires_at, now))
        .filter_map(|node| match &node.payload {
            Payload::Inherit { group, value: true } if paths.contains_key(group) => Some(group),
            _ => None,
        })
        .max_by_key(|name| (groups.group(name).map_or(0, |g| g.weight), Reverse(*name)))
        .map_or_else(|| DEFAULT_GROUP.to_owned(), Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        Uuid::from_u128(1)
    }

    fn group(name: &str, weight: i32, nodes: Vec<Node>) -> (String, Group) {
        let mut group = Group::new(name).unwrap();
        group.weight = weight;
        group.nodes = nodes;
        (name.to_owned(), group)
    }

    fn user(nodes: Vec<Node>) -> User {
        User {
            nodes,
            ..User::new(uuid())
        }
    }

    fn perm(key: &str) -> Node {
        Node::permission(key, true).unwrap()
    }

    fn parent(name: &str) -> Node {
        Node::inherit(name, true).unwrap()
    }

    fn ranks(subject: &CompiledSubject) -> Vec<(String, u32)> {
        subject
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Group)
            .map(|e| (e.key.clone(), e.rank))
            .collect()
    }

    #[test]
    fn orders_by_weight_depth_and_name_with_default_last() {
        let groups = BTreeMap::from([
            group("default", 100, vec![perm("d")]),
            group("vip", 10, vec![parent("member")]),
            group("member", 10, vec![]),
            group("admin", 50, vec![]),
            group("alpha", 10, vec![]),
        ]);
        let subject = compile(
            &user(vec![
                parent("vip"),
                parent("alpha"),
                parent("admin"),
                perm("own"),
            ]),
            &groups,
            0,
            7,
        )
        .unwrap();
        assert_eq!(
            ranks(&subject),
            [
                ("admin".into(), 1),
                ("alpha".into(), 2),
                ("vip".into(), 3),
                ("member".into(), 4),
                ("default".into(), 5)
            ]
        );
        assert_eq!(subject.revision, 7);
        assert_eq!(subject.entries[0].key, "own");
        assert_eq!(subject.primary_group, "admin");
        assert_eq!(subject.rank_subject(3), Some("vip"));
        assert_eq!(subject.rank_subject(0), None);
    }

    #[test]
    fn cycles_terminate_and_dangling_edges_are_skipped() {
        let groups = BTreeMap::from([
            group("a", 0, vec![parent("b"), perm("a")]),
            group("b", 0, vec![parent("a"), parent("ghost"), perm("b")]),
        ]);
        let subject = compile(&user(vec![parent("a")]), &groups, 0, 1).unwrap();
        assert_eq!(
            ranks(&subject),
            [("a".into(), 1), ("b".into(), 2), ("default".into(), 3)]
        );
    }

    #[test]
    fn contextual_edges_merge_and_global_paths_dominate() {
        let survival = ContextSet::of("server", "survival").unwrap();
        let groups = BTreeMap::from([
            group(
                "staff",
                0,
                vec![perm("kick").with_context("world", "w").unwrap()],
            ),
            group("mod", 0, vec![parent("staff")]),
        ]);
        let edge = parent("staff").with_contexts(survival.clone()).unwrap();
        let subject = compile(&user(vec![edge.clone()]), &groups, 0, 1).unwrap();
        let kick = subject.entries.iter().find(|e| e.key == "kick").unwrap();
        assert_eq!(kick.contexts.to_string(), "server=survival;world=w");
        // A global path through `mod` dominates the contextual direct edge.
        let subject = compile(&user(vec![edge, parent("mod")]), &groups, 0, 1).unwrap();
        let kicks: Vec<_> = subject.entries.iter().filter(|e| e.key == "kick").collect();
        assert_eq!(kicks.len(), 1);
        assert_eq!(kicks[0].contexts.to_string(), "world=w");
        // Conflicting reserved keys along a path drop it.
        let lobby = parent("staff").with_context("server", "lobby").unwrap();
        let groups = BTreeMap::from([
            group("staff", 0, vec![perm("kick")]),
            group("mod", 0, vec![lobby]),
        ]);
        let edge = parent("mod").with_contexts(survival).unwrap();
        let subject = compile(&user(vec![edge]), &groups, 0, 1).unwrap();
        assert!(subject.entries.iter().all(|e| e.key != "kick"));
    }

    #[test]
    fn negated_inherit_blocks_every_path() {
        let groups = BTreeMap::from([
            group("vip", 0, vec![perm("fly")]),
            group("donor", 0, vec![parent("vip")]),
        ]);
        let blocked = compile(
            &user(vec![parent("donor"), Node::inherit("vip", false).unwrap()]),
            &groups,
            0,
            1,
        )
        .unwrap();
        assert!(
            blocked
                .entries
                .iter()
                .all(|e| e.key != "fly" && e.key != "vip")
        );
        let no_default = compile(
            &user(vec![Node::inherit("default", false).unwrap()]),
            &groups,
            0,
            1,
        )
        .unwrap();
        assert!(no_default.entries.is_empty());
        // A contextual block removes only paths confined to that context.
        let scoped = Node::inherit("vip", false)
            .unwrap()
            .with_context("server", "s")
            .unwrap();
        let subject = compile(&user(vec![parent("donor"), scoped.clone()]), &groups, 0, 1).unwrap();
        assert!(subject.entries.iter().any(|e| e.key == "fly"));
        let edge = parent("vip").with_context("server", "s").unwrap();
        let subject = compile(&user(vec![edge, scoped]), &groups, 0, 1).unwrap();
        assert!(subject.entries.iter().all(|e| e.key != "fly"));
    }

    #[test]
    fn expiry_skips_and_propagates() {
        let groups = BTreeMap::from([group(
            "vip",
            0,
            vec![perm("fly"), perm("old").expiring_at(5)],
        )]);
        let subject = compile(&user(vec![parent("vip").expiring_at(100)]), &groups, 10, 1).unwrap();
        let fly = subject.entries.iter().find(|e| e.key == "fly").unwrap();
        assert_eq!(fly.expires_at, Some(100));
        assert!(subject.entries.iter().all(|e| e.key != "old"));
        assert_eq!(subject.next_expiry(), Some(100));
        let lapsed = compile(&user(vec![parent("vip").expiring_at(10)]), &groups, 10, 1).unwrap();
        assert!(lapsed.entries.iter().all(|e| e.key != "fly"));
    }

    #[test]
    fn primary_group_rules() {
        let groups = BTreeMap::from([
            group("vip", 10, vec![]),
            group("mod", 20, vec![]),
            group("admin", 99, vec![]),
        ]);
        let contextual = parent("admin").with_context("server", "s").unwrap();
        let mut u = user(vec![parent("vip"), parent("mod"), contextual]);
        assert_eq!(compile(&u, &groups, 0, 1).unwrap().primary_group, "mod");
        u.primary_group = Some("vip".into());
        assert_eq!(compile(&u, &groups, 0, 1).unwrap().primary_group, "vip");
        u.primary_group = Some("deleted".into());
        assert_eq!(compile(&u, &groups, 0, 1).unwrap().primary_group, "mod");
        assert_eq!(
            compile(&user(vec![]), &groups, 0, 1).unwrap().primary_group,
            "default"
        );
    }

    #[test]
    fn collects_decorations_and_meta() {
        let groups = BTreeMap::from([group(
            "vip",
            0,
            vec![
                Node::prefix(100, "[VIP]").unwrap(),
                Node::suffix(1, "!").unwrap(),
                Node::meta("homes", "5").unwrap(),
                Node::weight(3),
            ],
        )]);
        let subject = compile(&user(vec![parent("vip")]), &groups, 0, 1).unwrap();
        assert_eq!(subject.prefix[0].text, "[VIP]");
        assert_eq!(subject.prefix[0].rank, 1);
        assert_eq!(subject.suffix.len(), 1);
        assert_eq!(subject.meta[0].value, "5");
    }

    #[test]
    fn caps_entries() {
        let nodes = (0..10).map(|i| perm(&format!("n{i}"))).collect();
        let groups = BTreeMap::from([group("big", 0, nodes)]);
        let u = user(vec![parent("big")]);
        assert!(compile_with_limit(&u, &groups, 0, 1, 12).is_ok());
        assert_eq!(
            compile_with_limit(&u, &groups, 0, 1, 11),
            Err(Error::TooManyEntries { limit: 11 })
        );
    }

    #[test]
    fn wire_shape() {
        let subject = CompiledSubject {
            revision: 412,
            uuid: Uuid::parse("0123456789abcdef0123456789abcdef").unwrap(),
            primary_group: "vip".into(),
            entries: vec![Entry {
                kind: EntryKind::Permission,
                key: "essentials.fly".into(),
                value: true,
                contexts: ContextSet::of("server", "survival").unwrap(),
                rank: 0,
                expires_at: Some(1762800000),
            }],
            prefix: vec![Decoration {
                priority: 100,
                text: "&6[VIP] ".into(),
                contexts: ContextSet::new(),
                rank: 1,
                expires_at: None,
            }],
            suffix: vec![],
            meta: vec![MetaEntry {
                key: "homes".into(),
                value: "5".into(),
                contexts: ContextSet::new(),
                rank: 1,
                expires_at: None,
            }],
        };
        let json = serde_json::json!({"revision": 412, "uuid": "0123456789abcdef0123456789abcdef",
            "primary_group": "vip",
            "entries": [{"k": "perm", "key": "essentials.fly", "v": true, "ctx": {"server": "survival"},
                         "rank": 0, "exp": 1762800000}],
            "prefix": [{"p": 100, "t": "&6[VIP] ", "ctx": {}, "rank": 1}],
            "suffix": [],
            "meta": [{"key": "homes", "v": "5", "ctx": {}, "rank": 1}]});
        assert_eq!(serde_json::to_value(&subject).unwrap(), json);
        assert_eq!(
            serde_json::from_value::<CompiledSubject>(json).unwrap(),
            subject
        );
    }
}
