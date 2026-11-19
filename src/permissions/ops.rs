//! Every write to the permission tables as a [`PermOp`], performed by
//! [`execute`] inside the executor's transaction.
//!
//! A revisioned write ([`PermWrite`]) runs one [`PermBody`] through
//! [`Work`], records one `perm_change` row when it did anything and returns
//! the revision. Memory (groups, tracks, the compiled cache, online
//! recompiles and notifications) follows from the authority's apply hook,
//! which reads the new `perm_change` rows, so a write replicated from
//! another peer has the same effects as a local one.
use super::{
    PermError,
    store::{self, Work},
};
use crate::database::{
    self, Conn, Dialect,
    operation::{OrMessage, intern},
    params,
};
use rift_permissions::{
    Change, Group, Limits, Node, SubjectRef, Uuid,
    luckperms::{Import, Skipped},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `perm_meta` key marking the one-time upgrade from the extension store.
pub(super) const MIGRATED: &str = "migrated_extension_store";

/// Every table of the permission store; the authority's hook watches them.
pub const TABLES: &[&str] = &[
    "perm_subject",
    "perm_node",
    "perm_track",
    "perm_registered",
    "perm_meta",
    "perm_change",
];

/// The tables a revisioned write may change.
const WRITE_TABLES: &[&str] = &[
    "perm_subject",
    "perm_node",
    "perm_track",
    "perm_meta",
    "perm_change",
];

/// The SQL that creates the permission tables.
pub(super) fn schema_sql(dialect: Dialect, _from: u32) -> String {
    match dialect {
        Dialect::Sqlite => store::SCHEMA.into(),
        Dialect::Postgres => store::POSTGRES_SCHEMA.into(),
    }
}

/// The permission store's schema component and the version this Rift writes.
pub fn schema_versions() -> Vec<(&'static str, u32)> {
    vec![(store::COMPONENT, store::SCHEMA_VERSION)]
}

/// The tables of `component`, if it is the permission store.
pub fn component_tables(component: &str) -> Option<&'static [&'static str]> {
    (component == store::COMPONENT).then_some(TABLES)
}

/// Migrate the permission tables to `to_version` in the caller's
/// transaction and create the `default` group (with creation time `now`)
/// when it is missing; `None` for another component.
pub fn migrate(
    connection: &Conn,
    component: &str,
    to_version: u32,
    now: u64,
) -> Option<Result<bool, String>> {
    if component != store::COMPONENT {
        return None;
    }
    let result = database::migrate_component(
        connection,
        store::COMPONENT,
        to_version,
        store::SCHEMA_VERSION,
        schema_sql,
    )
    .or_message()
    .and_then(|changed| {
        store::ensure_default(connection, now).or_message()?;
        Ok(changed)
    });
    Some(result)
}

/// A write to the permission store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PermOp {
    /// A revisioned change.
    Write(Box<PermWrite>),
    /// A player joined: record their name, creating their row (no revision).
    Join { uuid: Uuid, name: String, now: u64 },
    /// Replace the permissions `owner` registered (not revisioned).
    Register {
        owner: String,
        permissions: Vec<RegisteredRow>,
    },
}

/// One `perm_registered` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredRow {
    pub node: String,
    pub description: Option<String>,
    /// The default as stored (`true`, `false` or `undefined`).
    pub default: String,
    /// `owner` or `owner/plugin`.
    pub owner: String,
}

/// A change recorded as one revision by `actor`, with the time and the
/// limits it is checked against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermWrite {
    /// [`rift_permissions::Actor`] in its string form.
    pub actor: String,
    pub reason: Option<String>,
    pub now: u64,
    pub limits: Limits,
    pub body: PermBody,
}

/// What a [`PermWrite`] does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermBody {
    /// A normalized [`Change`].
    Change { change: Change },
    /// Delete every node whose expiry is at or before `now`.
    Sweep,
    /// Make the `source = 'config'` group nodes equal `groups`.
    Mirror { groups: BTreeMap<String, Group> },
    /// The one-time upgrade from the extension store, unless it ran.
    MigrateOnce { nodes: Vec<(SubjectRef, Node)> },
    /// A LuckPerms import; `replace` first deletes earlier imported nodes.
    Import { import: Import, replace: bool },
}

/// The result of a [`PermOp`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum PermOutcome {
    /// A new revision. `users` and `groups` are the subjects its ops name.
    Written {
        revision: u64,
        ops: usize,
        users: Vec<Uuid>,
        groups: Vec<String>,
        detail: PermDetail,
    },
    /// Nothing changed policy; `revision` is the current one.
    Unchanged {
        revision: u64,
        detail: PermDetail,
    },
    Joined,
    Registered {
        count: usize,
    },
    Failed {
        failure: PermFailure,
    },
}

impl PermOutcome {
    pub fn failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

/// What a write reports beyond its revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "detail", rename_all = "snake_case")]
pub enum PermDetail {
    None,
    /// [`PermBody::MigrateOnce`]: whether the upgrade ran now, and the nodes
    /// it skipped (as log lines).
    Migrated {
        migrated: bool,
        skipped: Vec<String>,
    },
    /// [`PermBody::Import`]: subjects imported and what was left out.
    Import {
        groups: usize,
        users: usize,
        tracks: usize,
        skipped: Vec<Skipped>,
    },
}

impl PermOp {
    pub fn tables(&self) -> &'static [&'static str] {
        const SUBJECTS: &[&str] = &["perm_subject"];
        const REGISTERED: &[&str] = &["perm_registered"];
        match self {
            Self::Write(_) => WRITE_TABLES,
            Self::Join { .. } => SUBJECTS,
            Self::Register { .. } => REGISTERED,
        }
    }
}

/// Perform `op` in the caller's transaction.
pub fn execute(connection: &Conn, op: &PermOp) -> PermOutcome {
    let result = match op {
        PermOp::Write(write) => perform(connection, write),
        PermOp::Join { uuid, name, now } => {
            store::set_last_name(connection, *uuid, name, *now).map(|()| PermOutcome::Joined)
        }
        PermOp::Register { owner, permissions } => register(connection, owner, permissions),
    };
    result.unwrap_or_else(|error| PermOutcome::Failed {
        failure: PermFailure::from(&error),
    })
}

fn perform(connection: &Conn, write: &PermWrite) -> Result<PermOutcome, PermError> {
    let mut work = Work::new(connection, write.now, write.limits);
    let detail = match &write.body {
        PermBody::Change { change } => {
            work.change(change.clone())?;
            PermDetail::None
        }
        PermBody::Sweep => {
            work.sweep()?;
            PermDetail::None
        }
        PermBody::Mirror { groups } => {
            work.mirror(groups)?;
            PermDetail::None
        }
        PermBody::MigrateOnce { nodes } => migrate_once(&mut work, nodes)?,
        PermBody::Import { import, replace } => import_luckperms(&mut work, import, *replace)?,
    };
    let effects = work.finish()?;
    if effects.ops.is_empty() {
        return Ok(PermOutcome::Unchanged {
            revision: store::revision(connection)?,
            detail,
        });
    }
    let revision = store::record(
        connection,
        write.now,
        &write.actor,
        &effects.ops,
        write.reason.as_deref(),
    )?;
    Ok(PermOutcome::Written {
        revision,
        ops: effects.ops.len(),
        users: effects.users.into_iter().collect(),
        groups: effects.groups.into_iter().collect(),
        detail,
    })
}

/// Write `nodes` once and mark the upgrade done. Nodes that fail validation
/// or limits are skipped and reported.
fn migrate_once(work: &mut Work, nodes: &[(SubjectRef, Node)]) -> Result<PermDetail, PermError> {
    if store::meta(work.connection(), MIGRATED)?.is_some() {
        return Ok(PermDetail::Migrated {
            migrated: false,
            skipped: Vec::new(),
        });
    }
    let mut skipped = Vec::new();
    for (subject, node) in nodes {
        if let Err(error) = work.set_node(subject, node.clone()) {
            if matches!(error, PermError::Storage(_)) {
                return Err(error);
            }
            skipped.push(format!(
                "permission upgrade skipped a node of {subject}: {error}"
            ));
        }
    }
    store::set_meta(work.connection(), MIGRATED, &work.now().to_string())?;
    Ok(PermDetail::Migrated {
        migrated: true,
        skipped,
    })
}

/// Validation and limit failures skip one item; storage failures abort.
fn skip(
    skipped: &mut Vec<Skipped>,
    subject: String,
    key: &str,
    error: PermError,
) -> Result<(), PermError> {
    if matches!(error, PermError::Storage(_)) {
        return Err(error);
    }
    skipped.push(Skipped {
        subject,
        key: key.to_owned(),
        reason: error.to_string(),
    });
    Ok(())
}

/// The writes of a LuckPerms import: additive, nodes land in their slots
/// with source `import`, except that `replace` first removes every earlier
/// `import` node.
fn import_luckperms(
    work: &mut Work,
    import: &Import,
    replace: bool,
) -> Result<PermDetail, PermError> {
    let mut skipped = Vec::new();
    if replace {
        work.delete_imported()?;
    }
    let (mut groups, mut users, mut tracks) = (0, 0, 0);
    for group in &import.groups {
        let label = format!("group:{}", group.name);
        if !work.group_exists(&group.name)?
            && let Err(error) = work.create_group(&group.name, 0, None)
        {
            skip(&mut skipped, label, "", error)?;
            continue;
        }
        groups += 1;
        let subject = SubjectRef::Group(group.name.clone());
        for node in &group.nodes {
            if let Err(error) = work.set_node(&subject, node.clone()) {
                skip(&mut skipped, label.clone(), &node.payload.key(), error)?;
            }
        }
    }
    for user in &import.users {
        let label = format!("user:{}", user.uuid.hyphenated());
        if let Some(name) = &user.last_name {
            store::set_last_name(work.connection(), user.uuid, name, work.now())?;
        }
        users += 1;
        let subject = SubjectRef::User(user.uuid);
        for node in &user.nodes {
            if let Err(error) = work.set_node(&subject, node.clone()) {
                skip(&mut skipped, label.clone(), &node.payload.key(), error)?;
            }
        }
    }
    for track in &import.tracks {
        match work.set_track(&track.name, track.groups.clone()) {
            Ok(()) => tracks += 1,
            Err(error) => skip(&mut skipped, format!("track:{}", track.name), "", error)?,
        }
    }
    Ok(PermDetail::Import {
        groups,
        users,
        tracks,
        skipped,
    })
}

fn register(
    connection: &Conn,
    owner: &str,
    permissions: &[RegisteredRow],
) -> Result<PermOutcome, PermError> {
    connection.execute(
        "DELETE FROM perm_registered WHERE owner = ?1 OR substr(owner, 1, length(?1) + 1) = ?1 || '/'",
        params![owner],
    )?;
    for row in permissions {
        connection.execute(
            "INSERT INTO perm_registered (node, description, dflt, owner) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(node) DO UPDATE SET description = excluded.description,
               dflt = excluded.dflt, owner = excluded.owner",
            params![row.node, row.description, row.default, row.owner],
        )?;
    }
    Ok(PermOutcome::Registered {
        count: permissions.len(),
    })
}

/// A [`PermError`] as data. Converting back gives an error with the same
/// message and code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum PermFailure {
    Invalid {
        field: String,
        value: String,
        reason: String,
    },
    TooManyEntries {
        limit: usize,
    },
    RegexNode {
        key: String,
    },
    Track {
        track: String,
        reason: String,
    },
    /// A validation error of a kind this Rift does not know.
    InvalidOther {
        message: String,
    },
    NotFound {
        what: String,
        name: String,
    },
    Conflict {
        message: String,
    },
    Limit {
        what: String,
        limit: usize,
    },
    Forbidden {
        message: String,
    },
    Deadline,
    Storage {
        message: String,
    },
    Import {
        message: String,
    },
}

impl From<&PermError> for PermFailure {
    fn from(error: &PermError) -> Self {
        use rift_permissions::Error as Invalid;
        match error {
            PermError::Invalid(Invalid::Invalid {
                field,
                value,
                reason,
            }) => Self::Invalid {
                field: (*field).to_owned(),
                value: value.clone(),
                reason: (*reason).to_owned(),
            },
            PermError::Invalid(Invalid::TooManyEntries { limit }) => {
                Self::TooManyEntries { limit: *limit }
            }
            PermError::Invalid(Invalid::RegexNode { key }) => Self::RegexNode { key: key.clone() },
            PermError::Invalid(Invalid::Track { track, reason }) => Self::Track {
                track: track.clone(),
                reason: (*reason).to_owned(),
            },
            PermError::Invalid(other) => Self::InvalidOther {
                message: other.to_string(),
            },
            PermError::NotFound { what, name } => Self::NotFound {
                what: (*what).to_owned(),
                name: name.clone(),
            },
            PermError::Conflict(message) => Self::Conflict {
                message: message.clone(),
            },
            PermError::Limit { what, limit } => Self::Limit {
                what: (*what).to_owned(),
                limit: *limit,
            },
            PermError::Forbidden(message) => Self::Forbidden {
                message: message.clone(),
            },
            PermError::Deadline => Self::Deadline,
            PermError::Storage(message) => Self::Storage {
                message: message.clone(),
            },
            PermError::Import(message) => Self::Import {
                message: message.clone(),
            },
        }
    }
}

impl From<PermFailure> for PermError {
    fn from(failure: PermFailure) -> Self {
        use rift_permissions::Error as Invalid;
        match failure {
            PermFailure::Invalid {
                field,
                value,
                reason,
            } => Self::Invalid(Invalid::Invalid {
                field: intern(&field),
                value,
                reason: intern(&reason),
            }),
            PermFailure::TooManyEntries { limit } => {
                Self::Invalid(Invalid::TooManyEntries { limit })
            }
            PermFailure::RegexNode { key } => Self::Invalid(Invalid::RegexNode { key }),
            PermFailure::Track { track, reason } => Self::Invalid(Invalid::Track {
                track,
                reason: intern(&reason),
            }),
            // Same message and code as a validation error.
            PermFailure::InvalidOther { message } => Self::Import(message),
            PermFailure::NotFound { what, name } => Self::NotFound {
                what: intern(&what),
                name,
            },
            PermFailure::Conflict { message } => Self::Conflict(message),
            PermFailure::Limit { what, limit } => Self::Limit {
                what: intern(&what),
                limit,
            },
            PermFailure::Forbidden { message } => Self::Forbidden(message),
            PermFailure::Deadline => Self::Deadline,
            PermFailure::Storage { message } => Self::Storage(message),
            PermFailure::Import { message } => Self::Import(message),
        }
    }
}
