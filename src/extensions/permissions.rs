//! Live v2 grants. Configured policy, durable group memberships and runtime
//! overrides have separate lifetimes.
use super::{fields, storage::Store, token};
use mlua::{Lua, Table, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex},
    time::Instant,
};

/// Permission patterns: exact nodes, `prefix.*` or `*`, mapped to grant/deny.
pub(super) type Nodes = BTreeMap<String, bool>;

/// Host-owned store namespace: UUID -> comma-separated group names.
const MEMBERSHIPS: &str = "rift.groups";
const DEFAULT_GROUP: &str = "default";
const MAX_GROUPS: usize = 64;
const MAX_PARENTS: usize = 16;
const MAX_MEMBERSHIPS: usize = 16;
const MAX_OVERRIDES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Group {
    nodes: Nodes,
    /// The groups listed in `inherits`.
    parents: BTreeSet<String>,
    /// Every group this one inherits from, directly or transitively.
    ancestors: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Policy {
    grants: BTreeMap<String, Nodes>,
    groups: BTreeMap<String, Group>,
}

pub(super) fn valid_uuid(uuid: &str) -> bool {
    uuid == "*" || player_uuid(uuid)
}

fn player_uuid(uuid: &str) -> bool {
    uuid.len() == 32
        && uuid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn pattern(node: &str) -> bool {
    node == "*" || token(node.strip_suffix(".*").unwrap_or(node))
}

/// The most specific matching pattern: the exact node, then `a.b.*`, `a.*`, `*`.
fn lookup(nodes: &Nodes, node: &str) -> Option<bool> {
    if nodes.is_empty() {
        return None;
    }
    if let Some(granted) = nodes.get(node) {
        return Some(*granted);
    }
    let mut prefix = node;
    while let Some((parent, _)) = prefix.rsplit_once('.') {
        if let Some(granted) = nodes.get(&format!("{parent}.*")) {
            return Some(*granted);
        }
        prefix = parent;
    }
    nodes.get("*").copied()
}

fn parse_nodes(table: Table, api_version: u8, path: &str) -> mlua::Result<Nodes> {
    let mut nodes = Nodes::new();
    for entry in table.pairs::<String, Value>() {
        let (node, value) = entry?;
        // Converting to bool would accept any truthy value, including 'false'.
        let Value::Boolean(granted) = value else {
            return Err(mlua::Error::runtime(format!(
                "{path}: {node:?} must be true or false"
            )));
        };
        if api_version == 1 {
            if !token(&node) || !granted {
                return Err(mlua::Error::runtime(
                    "permission grants must be node = true",
                ));
            }
        } else if !pattern(&node) {
            return Err(mlua::Error::runtime(format!(
                "{path}: {node:?} is not a permission node, prefix.* wildcard or *"
            )));
        }
        nodes.insert(node, granted);
    }
    Ok(nodes)
}

impl Policy {
    pub(super) fn parse(extensions: &Table, api_version: u8) -> mlua::Result<Self> {
        let mut grants = BTreeMap::new();
        if let Some(entries) = extensions.raw_get::<Option<Table>>("permissions")? {
            for entry in entries.pairs::<String, Table>() {
                let (uuid, nodes) = entry?;
                if !valid_uuid(&uuid) {
                    return Err(mlua::Error::runtime(
                        "permission keys must be lowercase UUID hex (32 digits) or *",
                    ));
                }
                let nodes = parse_nodes(nodes, api_version, "extensions.permissions")?;
                grants.insert(uuid, nodes);
            }
        }
        let mut parents = BTreeMap::new();
        let mut groups = BTreeMap::new();
        if let Some(entries) = extensions.raw_get::<Option<Table>>("groups")? {
            for entry in entries.pairs::<String, Table>() {
                let (name, group) = entry?;
                let path = format!("extensions.groups.{name}");
                if !token(&name) || groups.len() >= MAX_GROUPS {
                    return Err(mlua::Error::runtime(format!(
                        "invalid or excessive permission group name {name:?} (at most {MAX_GROUPS})"
                    )));
                }
                fields(&group, &["inherits", "permissions"])?;
                let mut inherits = BTreeSet::new();
                if let Some(list) = group.raw_get::<Option<Table>>("inherits")? {
                    let len = list.raw_len();
                    for (index, entry) in list.pairs::<Value, Value>().enumerate() {
                        let (Value::Integer(1..), Value::String(parent)) = entry? else {
                            return Err(mlua::Error::runtime(format!(
                                "{path}.inherits must be a list of group names"
                            )));
                        };
                        if index >= len || index >= MAX_PARENTS {
                            return Err(mlua::Error::runtime(format!(
                                "{path}.inherits must be a list of at most {MAX_PARENTS} group names"
                            )));
                        }
                        inherits.insert(parent.to_str()?.to_owned());
                    }
                }
                let nodes = match group.raw_get::<Option<Table>>("permissions")? {
                    Some(nodes) => parse_nodes(nodes, api_version, &format!("{path}.permissions"))?,
                    None => Nodes::new(),
                };
                parents.insert(name.clone(), inherits.clone());
                groups.insert(
                    name,
                    Group {
                        nodes,
                        parents: inherits,
                        ancestors: BTreeSet::new(),
                    },
                );
            }
        }
        for name in parents.keys() {
            let mut ancestors = BTreeSet::new();
            let mut pending: Vec<&String> = parents[name].iter().collect();
            while let Some(parent) = pending.pop() {
                if parent == name {
                    return Err(mlua::Error::runtime(format!(
                        "permission group {name:?} inherits from itself"
                    )));
                }
                let Some(grandparents) = parents.get(parent) else {
                    return Err(mlua::Error::runtime(format!(
                        "permission group {name:?} inherits from unknown group {parent:?}"
                    )));
                };
                if ancestors.insert(parent.clone()) {
                    pending.extend(grandparents);
                }
            }
            groups.get_mut(name).unwrap().ancestors = ancestors;
        }
        Ok(Self { grants, groups })
    }

    /// Group definitions with the field names of `extensions.groups`.
    pub(super) fn groups_value(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.groups
                .iter()
                .map(|(name, group)| {
                    (
                        name.clone(),
                        serde_json::json!({
                            "inherits": group.parents,
                            "permissions": group.nodes,
                        }),
                    )
                })
                .collect(),
        )
    }

    /// API v1 snapshot: exact configured grants only.
    pub(super) fn grants(&self, uuid: &str, node: &str) -> bool {
        ["*", uuid].iter().any(|key| {
            self.grants
                .get(*key)
                .is_some_and(|nodes| nodes.get(node) == Some(&true))
        })
    }

    /// A group overrides every group it inherits from; unrelated groups that
    /// disagree resolve to deny. Everyone belongs to `default` when it exists.
    fn group_decision(&self, members: Option<&BTreeSet<String>>, node: &str) -> Option<bool> {
        let mut reachable = BTreeSet::new();
        for name in members
            .into_iter()
            .flatten()
            .map(String::as_str)
            .chain([DEFAULT_GROUP])
        {
            if let Some(group) = self.groups.get(name) {
                reachable.insert(name);
                reachable.extend(group.ancestors.iter().map(String::as_str));
            }
        }
        let matches: Vec<(&str, bool)> = reachable
            .into_iter()
            .filter_map(|name| lookup(&self.groups[name].nodes, node).map(|g| (name, g)))
            .collect();
        let mut decision = None;
        for (name, granted) in &matches {
            let overridden = matches
                .iter()
                .any(|(other, _)| self.groups[*other].ancestors.contains(*name));
            if !overridden {
                decision = Some(decision.unwrap_or(true) && *granted);
            }
        }
        decision
    }
}

#[derive(Default)]
struct State {
    policy: Policy,
    memberships: BTreeMap<String, BTreeSet<String>>,
    overrides: BTreeMap<String, Nodes>,
}

#[derive(Clone)]
pub(super) struct Permissions {
    state: Arc<Mutex<State>>,
    store: Store,
}

fn decode_groups(value: &[u8]) -> io::Result<BTreeSet<String>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid permission group membership in extension store",
        )
    };
    let groups: BTreeSet<String> = std::str::from_utf8(value)
        .map_err(|_| invalid())?
        .split(',')
        .map(str::to_owned)
        .collect();
    if groups.len() > MAX_MEMBERSHIPS || !groups.iter().all(|group| token(group)) {
        return Err(invalid());
    }
    Ok(groups)
}

impl Permissions {
    /// Load durable group memberships; overrides always start empty.
    pub(super) fn open(store: Store) -> io::Result<Self> {
        let mut memberships = BTreeMap::new();
        for (uuid, value) in store.namespace(MEMBERSHIPS) {
            if !player_uuid(&uuid) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid permission group member in extension store",
                ));
            }
            memberships.insert(uuid, decode_groups(&value)?);
        }
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                memberships,
                ..State::default()
            })),
            store,
        })
    }

    /// Every stored membership: UUID -> group names.
    pub(super) fn memberships(&self) -> BTreeMap<String, BTreeSet<String>> {
        self.state.lock().unwrap().memberships.clone()
    }

    /// Add or remove a membership with the rules of `rift.permissions.add_group`
    /// and `remove_group`. Returns whether anything changed.
    pub(super) fn change_group(
        &self,
        uuid: &str,
        group: &str,
        add: bool,
        deadline: Instant,
    ) -> io::Result<bool> {
        let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
        if !player_uuid(uuid) || !token(group) {
            return Err(invalid(
                "group membership requires a lowercase player UUID (32 hex digits) and a group name"
                    .into(),
            ));
        }
        if group == DEFAULT_GROUP {
            return Err(invalid(
                "every player belongs to the default group implicitly".into(),
            ));
        }
        // Removing an undefined group stays possible to clean up memberships
        // left behind by a configuration change.
        if add && !self.state.lock().unwrap().policy.groups.contains_key(group) {
            return Err(invalid(format!("unknown permission group {group:?}")));
        }
        self.update_group(uuid.to_owned(), group.to_owned(), add, deadline)
    }

    pub(super) fn replace(&self, policy: &Policy) {
        self.state.lock().unwrap().policy.clone_from(policy);
    }

    /// Precedence: UUID override, `*` override, configured UUID grants, groups,
    /// configured `*` grants. Within each layer the most specific pattern wins.
    pub(super) fn has(&self, uuid: &str, node: &str) -> bool {
        if !token(node) {
            return false;
        }
        let state = self.state.lock().unwrap();
        let layer = |nodes: Option<&Nodes>| nodes.and_then(|nodes| lookup(nodes, node));
        layer(state.overrides.get(uuid))
            .or_else(|| layer(state.overrides.get("*")))
            .or_else(|| layer(state.policy.grants.get(uuid)))
            .or_else(|| {
                state
                    .policy
                    .group_decision(state.memberships.get(uuid), node)
            })
            .or_else(|| layer(state.policy.grants.get("*")))
            .unwrap_or(false)
    }

    fn set(
        &self,
        uuid: String,
        node: String,
        value: Option<bool>,
        deadline: Instant,
    ) -> mlua::Result<()> {
        let mut state = self.state.lock().unwrap();
        crate::script::check_deadline(deadline)?;
        if let Some(value) = value {
            let exists = state
                .overrides
                .get(&uuid)
                .is_some_and(|nodes| nodes.contains_key(&node));
            let count: usize = state.overrides.values().map(Nodes::len).sum();
            if count >= MAX_OVERRIDES && !exists {
                return Err(mlua::Error::runtime(format!(
                    "permission override limit exceeded ({MAX_OVERRIDES})"
                )));
            }
            state.overrides.entry(uuid).or_default().insert(node, value);
        } else if let Some(nodes) = state.overrides.get_mut(&uuid) {
            nodes.remove(&node);
            if nodes.is_empty() {
                state.overrides.remove(&uuid);
            }
        }
        Ok(())
    }

    /// Commit a membership change to the store, then publish it while the store
    /// is still locked so concurrent changes reach memory in commit order.
    fn update_group(
        &self,
        uuid: String,
        group: String,
        add: bool,
        deadline: Instant,
    ) -> io::Result<bool> {
        let key = (MEMBERSHIPS.to_owned(), uuid.clone());
        self.store
            .mutate_then(
                deadline,
                |entries| {
                    let mut groups = match entries.get(&key) {
                        Some(value) => decode_groups(value)?,
                        None => BTreeSet::new(),
                    };
                    let changed = if add {
                        groups.insert(group)
                    } else {
                        groups.remove(&group)
                    };
                    if groups.len() > MAX_MEMBERSHIPS {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "a player can belong to at most {MAX_MEMBERSHIPS} permission groups"
                            ),
                        ));
                    }
                    if groups.is_empty() {
                        entries.remove(&key);
                    } else {
                        let value = groups.iter().map(String::as_str).collect::<Vec<_>>();
                        entries.insert(key.clone(), value.join(",").into_bytes());
                    }
                    Ok((changed, groups))
                },
                |(_, groups)| {
                    let mut state = self.state.lock().unwrap();
                    if groups.is_empty() {
                        state.memberships.remove(&uuid);
                    } else {
                        state.memberships.insert(uuid.clone(), groups.clone());
                    }
                },
            )
            .map(|(changed, _)| changed)
    }

    pub(super) fn install(
        &self,
        lua: &Lua,
        runtime: &Table,
        deadline: Instant,
    ) -> mlua::Result<()> {
        let api = lua.create_table()?;
        let permissions = self.clone();
        api.raw_set(
            "has",
            lua.create_function(move |_, (uuid, node): (String, String)| {
                crate::script::check_deadline(deadline)?;
                validate(&uuid, &node, token)?;
                Ok(permissions.has(&uuid, &node))
            })?,
        )?;
        let permissions = self.clone();
        api.raw_set(
            "set",
            lua.create_function(move |_, (uuid, node, value): (String, String, Value)| {
                crate::script::check_deadline(deadline)?;
                validate(&uuid, &node, pattern)?;
                let value = match value {
                    Value::Nil => None,
                    Value::Boolean(value) => Some(value),
                    _ => {
                        return Err(mlua::Error::runtime(
                            "permission override must be true, false, or nil",
                        ));
                    }
                };
                permissions.set(uuid, node, value, deadline)
            })?,
        )?;
        for (name, add) in [("add_group", true), ("remove_group", false)] {
            let permissions = self.clone();
            api.raw_set(
                name,
                lua.create_function(move |_, (uuid, group): (String, String)| {
                    crate::script::check_deadline(deadline)?;
                    permissions
                        .change_group(&uuid, &group, add, deadline)
                        .map_err(|error| match error.kind() {
                            io::ErrorKind::InvalidInput => mlua::Error::runtime(error.to_string()),
                            _ => mlua::Error::external(error),
                        })
                })?,
            )?;
        }
        let permissions = self.clone();
        api.raw_set(
            "groups",
            lua.create_function(move |lua, uuid: String| {
                crate::script::check_deadline(deadline)?;
                if !player_uuid(&uuid) {
                    return Err(mlua::Error::runtime(
                        "group membership requires a lowercase player UUID (32 hex digits)",
                    ));
                }
                let state = permissions.state.lock().unwrap();
                lua.create_sequence_from(
                    state.memberships.get(&uuid).into_iter().flatten().cloned(),
                )
            })?,
        )?;
        runtime.raw_set("permissions", api)
    }
}

fn validate(uuid: &str, node: &str, node_valid: fn(&str) -> bool) -> mlua::Result<()> {
    if !valid_uuid(uuid) || !node_valid(node) {
        return Err(mlua::Error::runtime(
            "permissions require a lowercase UUID (32 hex digits) or * and a permission node",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const PLAYER: &str = "01010101010101010101010101010101";

    fn permissions(source: &str) -> Permissions {
        let lua = Lua::new();
        let policy = Policy::parse(&lua.load(source).eval::<Table>().unwrap(), 2).unwrap();
        let permissions = Permissions::open(Store::open(None).unwrap()).unwrap();
        permissions.replace(&policy);
        permissions
    }

    fn join(permissions: &Permissions, groups: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(30);
        for group in groups {
            assert!(
                permissions
                    .update_group(PLAYER.into(), (*group).into(), true, deadline)
                    .unwrap()
            );
        }
    }

    #[test]
    fn the_most_specific_pattern_wins_within_a_layer() {
        let permissions = permissions(
            r#"{ permissions = { ['*'] = {
                ['*'] = true, ['network.*'] = false,
                ['network.staff.*'] = true, ['network.staff.ban'] = false,
            } } }"#,
        );
        for (node, granted) in [
            ("lobby", true),
            ("network", true),
            ("network.chat", false),
            ("network.staff", false),
            ("network.staff.kick", true),
            ("network.staff.kick.temporary", true),
            ("network.staff.ban", false),
        ] {
            assert_eq!(permissions.has(PLAYER, node), granted, "{node}");
        }
        // Wildcards are grant patterns, not queries.
        assert!(!permissions.has(PLAYER, "*"));
        assert!(!permissions.has(PLAYER, "lobby.*"));
    }

    #[test]
    fn layers_apply_overrides_then_player_grants_then_groups_then_defaults() {
        let permissions = permissions(&format!(
            r#"{{
                permissions = {{
                    ['*'] = {{ ['chat.*'] = true }},
                    ['{PLAYER}'] = {{ ['chat.shout'] = true }},
                }},
                groups = {{ default = {{ permissions = {{ ['chat.shout'] = false, ['chat.color'] = false }} }} }},
            }}"#
        ));
        assert!(permissions.has(PLAYER, "chat.shout"));
        assert!(!permissions.has(PLAYER, "chat.color"));
        assert!(permissions.has(PLAYER, "chat.send"));
        let deadline = Instant::now() + Duration::from_secs(30);
        permissions
            .set("*".into(), "chat.*".into(), Some(false), deadline)
            .unwrap();
        assert!(!permissions.has(PLAYER, "chat.shout"));
        permissions
            .set(PLAYER.into(), "chat.color".into(), Some(true), deadline)
            .unwrap();
        assert!(permissions.has(PLAYER, "chat.color"));
        assert!(!permissions.has(PLAYER, "chat.send"));
        permissions
            .set("*".into(), "chat.*".into(), None, deadline)
            .unwrap();
        assert!(permissions.has(PLAYER, "chat.shout"));
        assert!(permissions.has(PLAYER, "chat.send"));
    }

    #[test]
    fn groups_override_their_ancestors_and_unrelated_conflicts_deny() {
        let source = r#"{ groups = {
            default = { permissions = { ['chat.send'] = true, ['chat.color'] = false } },
            staff = { inherits = { 'default' }, permissions = { ['chat.color'] = true, ['network.staff'] = true } },
            admin = { inherits = { 'staff' }, permissions = { ['*'] = true, ['network.shutdown'] = false } },
            muted = { permissions = { ['chat.send'] = false } },
            vip = { permissions = { ['chat.color'] = true } },
        } }"#;
        type Case<'a> = (&'a [&'a str], &'a [(&'a str, bool)]);
        let cases: &[Case] = &[
            (
                &[],
                &[
                    ("chat.send", true),
                    ("chat.color", false),
                    ("network.staff", false),
                ],
            ),
            (&["staff"], &[("chat.color", true), ("network.staff", true)]),
            (
                &["admin"],
                &[
                    ("chat.color", true),
                    ("anything.else", true),
                    ("network.shutdown", false),
                ],
            ),
            (
                &["staff", "muted"],
                &[("chat.send", false), ("chat.color", true)],
            ),
            // vip does not inherit default, so their disagreement denies.
            (&["vip"], &[("chat.color", false)]),
            (&["undefined"], &[("chat.send", true)]),
        ];
        for (groups, expected) in cases {
            let permissions = permissions(source);
            join(&permissions, groups);
            for (node, granted) in *expected {
                assert_eq!(permissions.has(PLAYER, node), *granted, "{groups:?} {node}");
            }
        }
    }

    #[test]
    fn memberships_are_written_through_the_store_and_reloaded() {
        let store = Store::open(None).unwrap();
        let permissions = Permissions::open(store.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        for group in ["staff", "builder"] {
            assert!(
                permissions
                    .update_group(PLAYER.into(), group.into(), true, deadline)
                    .unwrap()
            );
        }
        assert!(
            !permissions
                .update_group(PLAYER.into(), "staff".into(), true, deadline)
                .unwrap()
        );
        assert_eq!(
            store.namespace(MEMBERSHIPS),
            vec![(PLAYER.to_owned(), b"builder,staff".to_vec())]
        );
        let reopened = Permissions::open(store.clone()).unwrap();
        assert_eq!(
            reopened.state.lock().unwrap().memberships[PLAYER],
            BTreeSet::from(["builder".into(), "staff".into()])
        );
        for group in ["staff", "builder"] {
            assert!(
                permissions
                    .update_group(PLAYER.into(), group.into(), false, deadline)
                    .unwrap()
            );
        }
        assert!(store.namespace(MEMBERSHIPS).is_empty());
        assert!(permissions.state.lock().unwrap().memberships.is_empty());
        for i in 0..MAX_MEMBERSHIPS {
            permissions
                .update_group(PLAYER.into(), format!("g{i}"), true, deadline)
                .unwrap();
        }
        assert!(
            permissions
                .update_group(PLAYER.into(), "overflow".into(), true, deadline)
                .is_err()
        );
        assert_eq!(
            permissions.state.lock().unwrap().memberships[PLAYER].len(),
            MAX_MEMBERSHIPS
        );
    }

    #[test]
    fn corrupt_memberships_fail_startup() {
        for (key, value) in [
            ("not-a-uuid", &b"staff"[..]),
            ("*", b"staff"),
            (PLAYER, b"Staff"),
            (PLAYER, b"staff,,builder"),
            (PLAYER, b"\xff"),
        ] {
            let store = Store::open(None).unwrap();
            store
                .mutate_then(
                    Instant::now() + Duration::from_secs(30),
                    |entries| {
                        entries.insert((MEMBERSHIPS.into(), key.into()), value.to_vec());
                        Ok(())
                    },
                    |_| {},
                )
                .unwrap();
            assert!(Permissions::open(store).is_err(), "{key} {value:?}");
        }
    }
}
