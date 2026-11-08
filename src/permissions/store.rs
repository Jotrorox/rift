//! The `perm_*` tables in the shared database, row codecs, and [`Work`]: the
//! effects of one change inside the authority's transaction, each recorded as
//! an [`Op`] so the change log can replay it exactly.
use super::PermError;
use rift_permissions::{
    Change, ContextSet, DEFAULT_GROUP, Group, Limits, Node, NodeKind, SubjectRef, Track, User,
    Uuid,
    change::{Filter, GroupAttrs, Op, Patch, SubjectFilter},
    node::{NodeKey, NodeRow, Payload},
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::collections::{BTreeMap, BTreeSet};

/// The `rift_schema` component and version of these tables.
pub(super) const COMPONENT: &str = "permissions";
pub(super) const SCHEMA_VERSION: u32 = 1;

/// The tables of `docs/permissions.md`. Subject kinds: 0 user, 1 group; node
/// kinds are [`NodeKind::code`].
pub(super) const SCHEMA: &str = "
CREATE TABLE perm_subject (
  id INTEGER PRIMARY KEY,
  kind INTEGER NOT NULL,
  key TEXT NOT NULL,
  weight INTEGER NOT NULL DEFAULT 0,
  display_name TEXT,
  last_name TEXT,
  primary_group TEXT,
  created_at INTEGER NOT NULL,
  UNIQUE (kind, key)
);
CREATE TABLE perm_node (
  id INTEGER PRIMARY KEY,
  subject_id INTEGER NOT NULL REFERENCES perm_subject(id) ON DELETE CASCADE,
  kind INTEGER NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  priority INTEGER,
  contexts TEXT NOT NULL DEFAULT '',
  expires_at INTEGER,
  source TEXT NOT NULL DEFAULT 'store',
  UNIQUE (subject_id, kind, key, contexts)
);
CREATE INDEX perm_node_group ON perm_node(kind, key) WHERE kind = 1;
CREATE INDEX perm_node_expiry ON perm_node(expires_at) WHERE expires_at IS NOT NULL;
CREATE INDEX perm_subject_name ON perm_subject(last_name COLLATE NOCASE) WHERE kind = 0;
CREATE TABLE perm_track (name TEXT PRIMARY KEY, groups TEXT NOT NULL);
CREATE TABLE perm_registered (node TEXT PRIMARY KEY, description TEXT, dflt TEXT NOT NULL, owner TEXT NOT NULL);
CREATE TABLE perm_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE perm_change (
  revision INTEGER PRIMARY KEY,
  at INTEGER NOT NULL,
  actor TEXT NOT NULL,
  ops TEXT NOT NULL,
  reason TEXT
);
INSERT INTO perm_meta (key, value) VALUES ('revision', '0');
";

const USER: i64 = 0;
const GROUP: i64 = 1;

/// SQLite integers are signed; times past `i64::MAX` mean "never" anyway.
pub(super) fn sql_time(seconds: u64) -> i64 {
    i64::try_from(seconds).unwrap_or(i64::MAX)
}

fn corrupt(what: &str, detail: impl std::fmt::Display) -> PermError {
    PermError::Storage(format!("invalid {what} row: {detail}"))
}

/// Create the `default` group when it is missing; it is never deleted.
pub(super) fn ensure_default(connection: &Connection, now: u64) -> Result<(), PermError> {
    connection.execute(
        "INSERT OR IGNORE INTO perm_subject (kind, key, created_at) VALUES (?1, ?2, ?3)",
        params![GROUP, DEFAULT_GROUP, sql_time(now)],
    )?;
    Ok(())
}

pub(super) fn meta(connection: &Connection, key: &str) -> Result<Option<String>, PermError> {
    Ok(connection
        .query_row("SELECT value FROM perm_meta WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

pub(super) fn set_meta(connection: &Connection, key: &str, value: &str) -> Result<(), PermError> {
    connection.execute(
        "INSERT INTO perm_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub(super) fn revision(connection: &Connection) -> Result<u64, PermError> {
    let value = meta(connection, "revision")?.unwrap_or_else(|| "0".into());
    value.parse().map_err(|_| corrupt("perm_meta", "revision"))
}

/// Append a change row; its rowid is the new revision.
pub(super) fn record(
    connection: &Connection,
    at: u64,
    actor: &str,
    ops: &[Op],
    reason: Option<&str>,
) -> Result<u64, PermError> {
    let json = serde_json::to_string(ops).map_err(|e| PermError::Storage(e.to_string()))?;
    connection.execute(
        "INSERT INTO perm_change (at, actor, ops, reason) VALUES (?1, ?2, ?3, ?4)",
        params![sql_time(at), actor, json, reason],
    )?;
    let revision = connection.last_insert_rowid() as u64;
    set_meta(connection, "revision", &revision.to_string())?;
    Ok(revision)
}

/// The node columns selected by [`node_row`], prefixed by table alias `n`.
pub(super) const NODE_COLUMNS: &str =
    "n.kind, n.key, n.value, n.priority, n.contexts, n.expires_at, n.source";

/// Decode [`NODE_COLUMNS`] starting at column `at`.
pub(super) fn node_row(row: &Row, at: usize) -> Result<Node, PermError> {
    let code: u8 = row.get(at)?;
    let kind = NodeKind::from_code(code).ok_or_else(|| corrupt("perm_node", "kind"))?;
    let row = NodeRow {
        kind,
        key: row.get(at + 1)?,
        value: row.get(at + 2)?,
        priority: row.get(at + 3)?,
        contexts: row.get(at + 4)?,
        expires_at: row
            .get::<_, Option<i64>>(at + 5)?
            .map(|at| at.max(0) as u64),
        source: row.get(at + 6)?,
    };
    Node::try_from(row).map_err(|e| corrupt("perm_node", e))
}

/// A subject from the `kind` and `key` columns.
pub(super) fn subject_ref(kind: i64, key: String) -> Result<SubjectRef, PermError> {
    match kind {
        USER => Ok(SubjectRef::User(
            Uuid::parse(&key).map_err(|e| corrupt("perm_subject", e))?,
        )),
        GROUP => Ok(SubjectRef::Group(key)),
        _ => Err(corrupt("perm_subject", "kind")),
    }
}

fn subject_key(subject: &SubjectRef) -> (i64, String) {
    match subject {
        SubjectRef::User(uuid) => (USER, uuid.to_string()),
        SubjectRef::Group(name) => (GROUP, name.clone()),
    }
}

pub(super) fn subject_id(
    connection: &Connection,
    subject: &SubjectRef,
) -> Result<Option<i64>, PermError> {
    let (kind, key) = subject_key(subject);
    Ok(connection
        .query_row(
            "SELECT id FROM perm_subject WHERE kind = ?1 AND key = ?2",
            params![kind, key],
            |r| r.get(0),
        )
        .optional()?)
}

/// A subject's nodes in insertion order, with their row ids.
fn nodes_of(connection: &Connection, id: i64) -> Result<Vec<(i64, Node)>, PermError> {
    let mut statement = connection.prepare_cached(&format!(
        "SELECT n.id, {NODE_COLUMNS} FROM perm_node n WHERE n.subject_id = ?1 ORDER BY n.id"
    ))?;
    let mut rows = statement.query([id])?;
    let mut nodes = Vec::new();
    while let Some(row) = rows.next()? {
        nodes.push((row.get(0)?, node_row(row, 1)?));
    }
    Ok(nodes)
}

fn group_attrs(connection: &Connection, name: &str) -> Result<Option<GroupAttrs>, PermError> {
    Ok(connection
        .query_row(
            "SELECT weight, display_name FROM perm_subject WHERE kind = ?1 AND key = ?2",
            params![GROUP, name],
            |r| {
                Ok(GroupAttrs {
                    weight: r.get(0)?,
                    display_name: r.get(1)?,
                })
            },
        )
        .optional()?)
}

/// One group with its nodes, or `None` when it does not exist.
pub(super) fn load_group(connection: &Connection, name: &str) -> Result<Option<Group>, PermError> {
    let row: Option<(i64, i32, Option<String>)> = connection
        .query_row(
            "SELECT id, weight, display_name FROM perm_subject WHERE kind = ?1 AND key = ?2",
            params![GROUP, name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((id, weight, display_name)) = row else {
        return Ok(None);
    };
    Ok(Some(Group {
        name: name.to_owned(),
        weight,
        display_name,
        nodes: nodes_of(connection, id)?
            .into_iter()
            .map(|(_, n)| n)
            .collect(),
    }))
}

pub(super) fn load_groups(connection: &Connection) -> Result<BTreeMap<String, Group>, PermError> {
    let names: Vec<String> = connection
        .prepare("SELECT key FROM perm_subject WHERE kind = ?1 ORDER BY key")?
        .query_map([GROUP], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut groups = BTreeMap::new();
    for name in names {
        if let Some(group) = load_group(connection, &name)? {
            groups.insert(name, group);
        }
    }
    Ok(groups)
}

/// A user with their nodes; one without a row is a user with no nodes.
pub(super) fn load_user(connection: &Connection, uuid: Uuid) -> Result<User, PermError> {
    let row: Option<(i64, Option<String>, Option<String>)> = connection
        .query_row(
            "SELECT id, last_name, primary_group FROM perm_subject WHERE kind = ?1 AND key = ?2",
            params![USER, uuid.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((id, last_name, primary_group)) = row else {
        return Ok(User::new(uuid));
    };
    Ok(User {
        uuid,
        last_name,
        primary_group,
        nodes: nodes_of(connection, id)?
            .into_iter()
            .map(|(_, n)| n)
            .collect(),
    })
}

/// Every user that has a row, for exports.
pub(super) fn load_users(connection: &Connection) -> Result<Vec<User>, PermError> {
    let keys: Vec<String> = connection
        .prepare("SELECT key FROM perm_subject WHERE kind = ?1 ORDER BY key")?
        .query_map([USER], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    keys.iter()
        .map(|key| {
            let uuid = Uuid::parse(key).map_err(|e| corrupt("perm_subject", e))?;
            load_user(connection, uuid)
        })
        .collect()
}

pub(super) fn load_tracks(connection: &Connection) -> Result<BTreeMap<String, Track>, PermError> {
    let mut statement = connection.prepare("SELECT name, groups FROM perm_track ORDER BY name")?;
    let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut tracks = BTreeMap::new();
    for row in rows {
        let (name, json) = row?;
        let groups: Vec<String> =
            serde_json::from_str(&json).map_err(|e| corrupt("perm_track", e))?;
        tracks.insert(name.clone(), Track { name, groups });
    }
    Ok(tracks)
}

/// Record a player's current name, creating their row when needed. Names
/// are not policy, so this creates no revision.
pub(super) fn set_last_name(
    connection: &Connection,
    uuid: Uuid,
    name: &str,
    now: u64,
) -> Result<(), PermError> {
    connection.execute(
        "INSERT INTO perm_subject (kind, key, last_name, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(kind, key) DO UPDATE SET last_name = excluded.last_name
         WHERE last_name IS NOT excluded.last_name",
        params![USER, uuid.to_string(), name, sql_time(now)],
    )?;
    Ok(())
}

/// Nodes matching `filter`, in insertion order, at most `limit`. SQL narrows
/// by subject, kind and key; [`Filter::matches`] decides.
pub(super) fn select(
    connection: &Connection,
    filter: &Filter,
    limit: usize,
) -> Result<Vec<(i64, SubjectRef, Node)>, PermError> {
    let subjects = match filter.subjects {
        SubjectFilter::Any => None,
        SubjectFilter::Users => Some(USER),
        SubjectFilter::Groups => Some(GROUP),
    };
    let (subject_kind, subject_key) = filter
        .subject
        .as_ref()
        .map(subject_key)
        .map_or((None, None), |(kind, key)| (Some(kind), Some(key)));
    let mut statement = connection.prepare(&format!(
        "SELECT n.id, s.kind, s.key, {NODE_COLUMNS} FROM perm_node n
         JOIN perm_subject s ON s.id = n.subject_id
         WHERE (?1 IS NULL OR s.kind = ?1) AND (?2 IS NULL OR n.kind = ?2)
           AND (?3 IS NULL OR (s.kind = ?3 AND s.key = ?4))
           AND (?5 IS NULL OR n.key LIKE ?5 ESCAPE '\\')
           AND (?6 IS NULL OR n.source = ?6)
         ORDER BY n.id"
    ))?;
    let mut rows = statement.query(params![
        subjects,
        filter.kind.map(NodeKind::code),
        subject_kind,
        subject_key,
        filter.key_like,
        filter.source.as_ref().map(ToString::to_string),
    ])?;
    let mut found = Vec::new();
    while let Some(row) = rows.next()?
        && found.len() < limit
    {
        let subject = subject_ref(row.get(1)?, row.get(2)?)?;
        let node = node_row(row, 3)?;
        if filter.matches(&subject, &node) {
            found.push((row.get(0)?, subject, node));
        }
    }
    Ok(found)
}

/// The effects of one change. Every effect is pushed to `ops` as it
/// happens, so the list replays exactly in order; group column changes are
/// summarised per group by [`Work::finish`].
pub(super) struct Work<'a> {
    connection: &'a Connection,
    now: u64,
    limits: Limits,
    ops: Vec<Op>,
    /// Touched groups and their columns when first touched (`None`: absent).
    groups: BTreeMap<String, Option<GroupAttrs>>,
}

/// What a change did.
pub(super) struct Effects {
    pub ops: Vec<Op>,
    pub users: BTreeSet<Uuid>,
    pub groups: BTreeSet<String>,
    pub tracks: bool,
}

impl<'a> Work<'a> {
    pub fn new(connection: &'a Connection, now: u64, limits: Limits) -> Self {
        Self {
            connection,
            now,
            limits,
            ops: Vec::new(),
            groups: BTreeMap::new(),
        }
    }

    pub fn connection(&self) -> &Connection {
        self.connection
    }

    pub fn now(&self) -> u64 {
        self.now
    }

    pub fn change(&mut self, change: Change) -> Result<(), PermError> {
        match change {
            Change::SetNode { subject, node } => self.set_node(&subject, node),
            Change::UnsetNode { subject, key } => self.unset_node(&subject, &key),
            Change::ClearSubject { subject, contexts } => self.clear(&subject, contexts.as_ref()),
            Change::CreateGroup {
                name,
                weight,
                display_name,
            } => self.create_group(&name, weight, display_name),
            Change::DeleteGroup { name } => self.delete_group(&name),
            Change::SetWeight { group, weight } => self.set_weight(&group, weight),
            Change::SetDisplayName {
                group,
                display_name,
            } => self.set_display_name(&group, display_name),
            Change::SetPrimaryGroup { user, group } => self.set_primary_group(user, group),
            Change::SetTrack { name, groups } => self.set_track(&name, groups),
            Change::DeleteTrack { name } => self.delete_track(&name),
            Change::Promote {
                user,
                track,
                contexts,
            } => self.move_on_track(user, &track, &contexts, true),
            Change::Demote {
                user,
                track,
                contexts,
            } => self.move_on_track(user, &track, &contexts, false),
            Change::Bulk { filter, patch } => self.bulk(&filter, &patch),
            Change::Batch { changes } => changes.into_iter().try_for_each(|c| self.change(c)),
        }
    }

    fn touch_group(&mut self, name: &str) -> Result<(), PermError> {
        if !self.groups.contains_key(name) {
            let attrs = group_attrs(self.connection, name)?;
            self.groups.insert(name.to_owned(), attrs);
        }
        Ok(())
    }

    /// The subject's row id: users get a row on first write, groups must exist.
    fn subject_row(&mut self, subject: &SubjectRef) -> Result<i64, PermError> {
        match subject {
            SubjectRef::User(uuid) => {
                self.connection.execute(
                    "INSERT OR IGNORE INTO perm_subject (kind, key, created_at) VALUES (?1, ?2, ?3)",
                    params![USER, uuid.to_string(), sql_time(self.now)],
                )?;
                Ok(subject_id(self.connection, subject)?.expect("inserted above"))
            }
            SubjectRef::Group(name) => {
                let id = subject_id(self.connection, subject)?
                    .ok_or_else(|| PermError::not_found("group", name))?;
                self.touch_group(name)?;
                Ok(id)
            }
        }
    }

    fn node_at(&self, id: i64, key: &NodeKey) -> Result<Option<(i64, Node)>, PermError> {
        let mut statement = self.connection.prepare_cached(&format!(
            "SELECT n.id, {NODE_COLUMNS} FROM perm_node n
             WHERE n.subject_id = ?1 AND n.kind = ?2 AND n.key = ?3 AND n.contexts = ?4"
        ))?;
        let mut rows = statement.query(params![
            id,
            key.kind.code(),
            key.key,
            key.contexts.to_string()
        ])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, node_row(row, 1)?))),
            None => Ok(None),
        }
    }

    fn count(&self, id: i64, inherits: bool) -> Result<usize, PermError> {
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM perm_node WHERE subject_id = ?1 AND (?2 = 0 OR kind = 1)",
            params![id, inherits],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Insert `node`, replacing the node in its slot.
    pub fn set_node(&mut self, subject: &SubjectRef, node: Node) -> Result<(), PermError> {
        if node.is_expired(self.now) {
            return Err(PermError::Conflict(
                "the node's expiry is already in the past".into(),
            ));
        }
        if node.kind().group_only() && matches!(subject, SubjectRef::User(_)) {
            return Err(rift_permissions::Error::Invalid {
                field: "node",
                value: node.kind().as_str().into(),
                reason: "applies to groups only",
            }
            .into());
        }
        if node.contexts.len() > self.limits.contexts {
            return Err(PermError::Limit {
                what: "contexts per node",
                limit: self.limits.contexts,
            });
        }
        let id = self.subject_row(subject)?;
        let before = self.node_at(id, &node.key())?.map(|(_, n)| n);
        if before.as_ref() == Some(&node) {
            return Ok(());
        }
        if before.is_none() {
            let (what, limit) = match subject {
                SubjectRef::User(_) => ("nodes per user", self.limits.user_nodes),
                SubjectRef::Group(_) => ("nodes per group", self.limits.group_nodes),
            };
            if self.count(id, false)? >= limit {
                return Err(PermError::Limit { what, limit });
            }
            if node.kind() == NodeKind::Inherit && self.count(id, true)? >= self.limits.parents {
                return Err(PermError::Limit {
                    what: "parents per subject",
                    limit: self.limits.parents,
                });
            }
        }
        let row = NodeRow::from(&node);
        self.connection.execute(
            "INSERT INTO perm_node (subject_id, kind, key, value, priority, contexts, expires_at, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(subject_id, kind, key, contexts) DO UPDATE SET value = excluded.value,
               priority = excluded.priority, expires_at = excluded.expires_at, source = excluded.source",
            params![
                id,
                row.kind.code(),
                row.key,
                row.value,
                row.priority,
                row.contexts,
                row.expires_at.map(sql_time),
                row.source
            ],
        )?;
        self.ops.push(Op::Node {
            subject: subject.clone(),
            before,
            after: Some(node),
        });
        Ok(())
    }

    /// Remove the node in a slot, if any.
    pub fn unset_node(&mut self, subject: &SubjectRef, key: &NodeKey) -> Result<(), PermError> {
        let Some(id) = subject_id(self.connection, subject)? else {
            return match subject {
                SubjectRef::User(_) => Ok(()),
                SubjectRef::Group(name) => Err(PermError::not_found("group", name)),
            };
        };
        if let SubjectRef::Group(name) = subject {
            self.touch_group(name)?;
        }
        if let Some((row, node)) = self.node_at(id, key)? {
            self.remove(subject, row, node)?;
        }
        Ok(())
    }

    fn remove(&mut self, subject: &SubjectRef, row: i64, node: Node) -> Result<(), PermError> {
        self.connection
            .execute("DELETE FROM perm_node WHERE id = ?1", [row])?;
        self.ops.push(Op::Node {
            subject: subject.clone(),
            before: Some(node),
            after: None,
        });
        Ok(())
    }

    fn clear(
        &mut self,
        subject: &SubjectRef,
        contexts: Option<&ContextSet>,
    ) -> Result<(), PermError> {
        let Some(id) = subject_id(self.connection, subject)? else {
            return match subject {
                SubjectRef::User(_) => Ok(()),
                SubjectRef::Group(name) => Err(PermError::not_found("group", name)),
            };
        };
        if let SubjectRef::Group(name) = subject {
            self.touch_group(name)?;
        }
        for (row, node) in nodes_of(self.connection, id)? {
            if contexts.is_none_or(|c| *c == node.contexts) {
                self.remove(subject, row, node)?;
            }
        }
        Ok(())
    }

    pub fn group_exists(&self, name: &str) -> Result<bool, PermError> {
        Ok(group_attrs(self.connection, name)?.is_some())
    }

    pub fn create_group(
        &mut self,
        name: &str,
        weight: i32,
        display_name: Option<String>,
    ) -> Result<(), PermError> {
        if self.group_exists(name)? {
            return Err(PermError::Conflict(format!(
                "group {name:?} already exists"
            )));
        }
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM perm_subject WHERE kind = ?1",
            [GROUP],
            |r| r.get(0),
        )?;
        if count as usize >= self.limits.groups {
            return Err(PermError::Limit {
                what: "groups",
                limit: self.limits.groups,
            });
        }
        self.connection.execute(
            "INSERT INTO perm_subject (kind, key, created_at) VALUES (?1, ?2, ?3)",
            params![GROUP, name, sql_time(self.now)],
        )?;
        let attrs = GroupAttrs {
            weight,
            display_name: display_name.clone(),
        };
        self.ops.push(Op::Group {
            name: name.to_owned(),
            before: None,
            after: Some(attrs.clone()),
        });
        self.groups.insert(name.to_owned(), Some(attrs));
        let subject = SubjectRef::Group(name.to_owned());
        if weight != 0 {
            self.set_node(&subject, Node::weight(weight))?;
        }
        if let Some(display_name) = display_name {
            self.set_node(&subject, Node::display_name(&display_name)?)?;
        }
        self.sync_columns(name)?;
        Ok(())
    }

    fn delete_group(&mut self, name: &str) -> Result<(), PermError> {
        let subject = SubjectRef::Group(name.to_owned());
        self.clear(&subject, None)?;
        let before = self.sync_columns(name)?;
        self.connection.execute(
            "DELETE FROM perm_subject WHERE kind = ?1 AND key = ?2",
            params![GROUP, name],
        )?;
        self.ops.push(Op::Group {
            name: name.to_owned(),
            before,
            after: None,
        });
        self.groups.insert(name.to_owned(), None);
        Ok(())
    }

    fn global(kind: NodeKind) -> NodeKey {
        NodeKey {
            kind,
            key: kind.as_str().to_owned(),
            contexts: ContextSet::new(),
        }
    }

    fn set_weight(&mut self, group: &str, weight: i32) -> Result<(), PermError> {
        let subject = SubjectRef::Group(group.to_owned());
        if weight == 0 {
            self.unset_node(&subject, &Self::global(NodeKind::Weight))
        } else {
            self.set_node(&subject, Node::weight(weight))
        }
    }

    fn set_display_name(&mut self, group: &str, name: Option<String>) -> Result<(), PermError> {
        let subject = SubjectRef::Group(group.to_owned());
        match name {
            None => self.unset_node(&subject, &Self::global(NodeKind::DisplayName)),
            Some(name) => self.set_node(&subject, Node::display_name(&name)?),
        }
    }

    /// Derive the weight and display name columns from the group's global
    /// nodes and return the columns, or `None` for a missing group.
    fn sync_columns(&mut self, name: &str) -> Result<Option<GroupAttrs>, PermError> {
        let subject = SubjectRef::Group(name.to_owned());
        let Some(id) = subject_id(self.connection, &subject)? else {
            return Ok(None);
        };
        let mut attrs = GroupAttrs {
            weight: 0,
            display_name: None,
        };
        for kind in [NodeKind::Weight, NodeKind::DisplayName] {
            match self
                .node_at(id, &Self::global(kind))?
                .map(|(_, n)| n.payload)
            {
                Some(Payload::Weight { weight }) => attrs.weight = weight,
                Some(Payload::DisplayName { name }) => attrs.display_name = Some(name),
                _ => {}
            }
        }
        self.connection.execute(
            "UPDATE perm_subject SET weight = ?1, display_name = ?2 WHERE id = ?3",
            params![attrs.weight, attrs.display_name, id],
        )?;
        Ok(Some(attrs))
    }

    fn set_primary_group(&mut self, user: Uuid, group: Option<String>) -> Result<(), PermError> {
        if let Some(group) = &group
            && !self.group_exists(group)?
        {
            return Err(PermError::not_found("group", group));
        }
        let id = self.subject_row(&SubjectRef::User(user))?;
        let before: Option<String> = self.connection.query_row(
            "SELECT primary_group FROM perm_subject WHERE id = ?1",
            [id],
            |r| r.get(0),
        )?;
        if before == group {
            return Ok(());
        }
        self.connection.execute(
            "UPDATE perm_subject SET primary_group = ?1 WHERE id = ?2",
            params![group, id],
        )?;
        self.ops.push(Op::PrimaryGroup {
            user,
            before,
            after: group,
        });
        Ok(())
    }

    fn track(&self, name: &str) -> Result<Option<Vec<String>>, PermError> {
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT groups FROM perm_track WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(|e| corrupt("perm_track", e)))
            .transpose()
    }

    pub fn set_track(&mut self, name: &str, groups: Vec<String>) -> Result<(), PermError> {
        let before = self.track(name)?;
        if before.as_ref() == Some(&groups) {
            return Ok(());
        }
        if before.is_none() {
            let count: i64 =
                self.connection
                    .query_row("SELECT count(*) FROM perm_track", [], |r| r.get(0))?;
            if count as usize >= self.limits.tracks {
                return Err(PermError::Limit {
                    what: "tracks",
                    limit: self.limits.tracks,
                });
            }
        }
        let json = serde_json::to_string(&groups).map_err(|e| PermError::Storage(e.to_string()))?;
        self.connection.execute(
            "INSERT INTO perm_track (name, groups) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET groups = excluded.groups",
            params![name, json],
        )?;
        self.ops.push(Op::Track {
            name: name.to_owned(),
            before,
            after: Some(groups),
        });
        Ok(())
    }

    fn delete_track(&mut self, name: &str) -> Result<(), PermError> {
        let before = self
            .track(name)?
            .ok_or_else(|| PermError::not_found("track", name))?;
        self.connection
            .execute("DELETE FROM perm_track WHERE name = ?1", [name])?;
        self.ops.push(Op::Track {
            name: name.to_owned(),
            before: Some(before),
            after: None,
        });
        Ok(())
    }

    fn move_on_track(
        &mut self,
        user: Uuid,
        name: &str,
        contexts: &ContextSet,
        up: bool,
    ) -> Result<(), PermError> {
        let groups = self
            .track(name)?
            .ok_or_else(|| PermError::not_found("track", name))?;
        let track = Track {
            name: name.to_owned(),
            groups,
        };
        let nodes = load_user(self.connection, user)?.nodes;
        let step = if up {
            track.promote(&nodes, contexts, self.now)?
        } else {
            track.demote(&nodes, contexts, self.now)?
        };
        let subject = SubjectRef::User(user);
        if let Some(group) = step.remove {
            let key = NodeKey {
                kind: NodeKind::Inherit,
                key: group,
                contexts: contexts.clone(),
            };
            self.unset_node(&subject, &key)?;
        }
        if let Some(group) = step.add {
            let node = Node::inherit(&group, true)?.with_contexts(contexts.clone())?;
            self.set_node(&subject, node)?;
        }
        Ok(())
    }

    fn bulk(&mut self, filter: &Filter, patch: &Patch) -> Result<(), PermError> {
        for (row, subject, node) in select(self.connection, filter, usize::MAX)? {
            match patch.apply(&node)? {
                None => {
                    if let SubjectRef::Group(name) = &subject {
                        self.touch_group(name)?;
                    }
                    self.remove(&subject, row, node)?;
                }
                Some(patched) if patched == node => {}
                Some(patched) => {
                    if patched.key() != node.key() {
                        if let SubjectRef::Group(name) = &subject {
                            self.touch_group(name)?;
                        }
                        self.remove(&subject, row, node)?;
                    }
                    self.set_node(&subject, patched)?;
                }
            }
        }
        Ok(())
    }

    /// Delete every node whose expiry has passed.
    pub fn sweep(&mut self) -> Result<(), PermError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT n.id, s.kind, s.key, {NODE_COLUMNS} FROM perm_node n
             JOIN perm_subject s ON s.id = n.subject_id
             WHERE n.expires_at IS NOT NULL AND n.expires_at <= ?1 ORDER BY n.id"
        ))?;
        let mut rows = statement.query([sql_time(self.now)])?;
        let mut expired = Vec::new();
        while let Some(row) = rows.next()? {
            expired.push((
                row.get::<_, i64>(0)?,
                subject_ref(row.get(1)?, row.get(2)?)?,
                node_row(row, 3)?,
            ));
        }
        drop(rows);
        drop(statement);
        for (row, subject, node) in expired {
            if let SubjectRef::Group(name) = &subject {
                self.touch_group(name)?;
            }
            self.remove(&subject, row, node)?;
        }
        Ok(())
    }

    /// Make the `source = 'config'` nodes equal `groups`, creating missing
    /// groups. Other nodes are left alone.
    pub fn mirror(&mut self, groups: &BTreeMap<String, Group>) -> Result<(), PermError> {
        for name in groups.keys() {
            if !self.group_exists(name)? {
                self.create_group(name, 0, None)?;
            }
        }
        let filter = Filter {
            source: Some(rift_permissions::Source::Config),
            ..Filter::default()
        };
        for (row, subject, node) in select(self.connection, &filter, usize::MAX)? {
            let wanted = match &subject {
                SubjectRef::Group(name) => groups
                    .get(name)
                    .is_some_and(|g| g.nodes.iter().any(|n| n.key() == node.key())),
                SubjectRef::User(_) => false,
            };
            if !wanted {
                if let SubjectRef::Group(name) = &subject {
                    self.touch_group(name)?;
                }
                self.remove(&subject, row, node)?;
            }
        }
        for (name, group) in groups {
            let subject = SubjectRef::Group(name.clone());
            for node in &group.nodes {
                self.set_node(&subject, node.clone())?;
            }
        }
        Ok(())
    }

    /// Delete every node a LuckPerms import wrote, before a `--replace`.
    pub fn delete_imported(&mut self) -> Result<(), PermError> {
        let filter = Filter {
            source: Some(rift_permissions::Source::Import),
            ..Filter::default()
        };
        for (row, subject, node) in select(self.connection, &filter, usize::MAX)? {
            if let SubjectRef::Group(name) = &subject {
                self.touch_group(name)?;
            }
            self.remove(&subject, row, node)?;
        }
        Ok(())
    }

    /// Sync group columns, add one [`Op::Group`] per group whose columns
    /// changed, and return everything this change did.
    pub fn finish(mut self) -> Result<Effects, PermError> {
        let touched = std::mem::take(&mut self.groups);
        for (name, initial) in touched {
            let current = self.sync_columns(&name)?;
            if current != initial {
                self.ops.push(Op::Group {
                    name,
                    before: initial,
                    after: current,
                });
            }
        }
        // A group counts as changed only when one of its nodes or columns did.
        let mut groups = BTreeSet::new();
        let mut users = BTreeSet::new();
        let mut tracks = false;
        for op in &self.ops {
            match op {
                Op::Node {
                    subject: SubjectRef::Group(name),
                    ..
                }
                | Op::Group { name, .. } => {
                    groups.insert(name.clone());
                }
                Op::Node {
                    subject: SubjectRef::User(uuid),
                    ..
                }
                | Op::PrimaryGroup { user: uuid, .. } => {
                    users.insert(*uuid);
                }
                Op::Track { .. } => tracks = true,
            }
        }
        Ok(Effects {
            ops: self.ops,
            users,
            groups,
            tracks,
        })
    }
}
