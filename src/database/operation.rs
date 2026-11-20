//! Every write to the instance database as a typed, serializable
//! [`Operation`], and its result as an [`Outcome`].
//!
//! An operation carries every input that is not in the database: timestamps,
//! configuration-derived values and limits. What it generates (an instance
//! name, a revision) is part of its outcome. Executing one is therefore
//! deterministic given the database's state and the operation, which lets a
//! cluster leader execute it and replicate the resulting changeset.
//!
//! A failed operation is a normal outcome, not an error: its changes are
//! rolled back and the failure travels as data ([`Outcome::failed`]).
//! Errors of the executor itself (the connection, a deadline, the commit)
//! stay `io::Error`s.
//!
//! The code that performs each operation lives with its component:
//! [`crate::instance_store::execute`], [`crate::permissions::ops::execute`],
//! [`crate::directory::apply`] and [`crate::extensions::storage::apply`].
//! [`execute_operation`] dispatches to them.
use super::{Conn, Database, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashSet},
    io,
    sync::Mutex,
    time::Instant,
};

/// One write to the instance database.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "component", content = "op", rename_all = "snake_case")]
pub enum Operation {
    /// The instance registry, group states and node schedules.
    Instances(crate::instance_store::InstanceOp),
    /// The permission store.
    Permissions(crate::permissions::ops::PermOp),
    /// The player directory (its own deterministic log entry in a cluster).
    Directory(crate::directory::DirectoryOp),
    /// The extension key-value store kept in the instance database (its own
    /// deterministic log entry in a cluster).
    Kv(crate::extensions::storage::KvOp),
    /// A schema migration of one component.
    Migrate(MigrateOp),
}

/// The result of an [`Operation`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "component", content = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Instances(crate::instance_store::InstanceOutcome),
    Permissions(crate::permissions::ops::PermOutcome),
    Directory(crate::directory::DirectoryOutcome),
    Kv(crate::extensions::storage::KvOutcome),
    Migrate(MigrateOutcome),
}

impl Outcome {
    /// Whether the operation failed; its changes must then be rolled back.
    pub fn failed(&self) -> bool {
        match self {
            Self::Instances(outcome) => outcome.failed(),
            Self::Permissions(outcome) => outcome.failed(),
            Self::Directory(outcome) => outcome.failed(),
            Self::Kv(outcome) => outcome.failed(),
            Self::Migrate(outcome) => matches!(outcome, MigrateOutcome::Failed { .. }),
        }
    }
}

impl Operation {
    /// The tables this operation may change: the hooks a successful
    /// execution calls.
    pub fn tables(&self) -> ChangedTables {
        let tables: &[&str] = match self {
            Self::Instances(op) => op.tables(),
            Self::Permissions(op) => op.tables(),
            Self::Directory(_) => crate::directory::TABLES,
            Self::Kv(_) => crate::extensions::storage::TABLES,
            Self::Migrate(op) => return op.tables(),
        };
        tables.iter().copied().collect()
    }
}

/// Upgrade one component's schema to `version` (see
/// [`super::migrate_component`]). `now` is the time startup inserts record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrateOp {
    pub component: String,
    pub version: u32,
    pub now: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum MigrateOutcome {
    /// `changed` is false when the component was at the version already.
    Done {
        changed: bool,
    },
    Failed {
        message: String,
    },
}

impl MigrateOp {
    fn tables(&self) -> ChangedTables {
        let mut tables: ChangedTables = component_tables(&self.component).iter().copied().collect();
        tables.insert("rift_schema");
        tables
    }
}

/// The tables of a schema component.
fn component_tables(component: &str) -> &'static [&'static str] {
    crate::instance_store::component_tables(component)
        .or_else(|| crate::permissions::ops::component_tables(component))
        .or_else(|| crate::directory::component_tables(component))
        .or_else(|| crate::extensions::storage::component_tables(component))
        .unwrap_or(&[])
}

/// Every schema component and the version this Rift writes, for a cluster's
/// `features` and `migrate` entries.
pub fn latest_versions() -> Vec<(&'static str, u32)> {
    let mut versions = crate::instance_store::schema_versions();
    versions.extend(crate::permissions::ops::schema_versions());
    versions.extend(crate::directory::schema_versions());
    versions.extend(crate::extensions::storage::schema_versions());
    versions
}

fn migrate(connection: &Conn, op: &MigrateOp) -> MigrateOutcome {
    let result = crate::instance_store::migrate(connection, &op.component, op.version)
        .or_else(|| crate::permissions::ops::migrate(connection, &op.component, op.version, op.now))
        .or_else(|| crate::directory::migrate(connection, &op.component, op.version))
        .or_else(|| crate::extensions::storage::migrate(connection, &op.component, op.version))
        .unwrap_or_else(|| Err(format!("unknown schema component {:?}", op.component)));
    match result {
        Ok(changed) => MigrateOutcome::Done { changed },
        Err(message) => MigrateOutcome::Failed { message },
    }
}

/// Perform `operation` on `connection`, which the caller has in a
/// transaction (or a savepoint). Deterministic given the database's state and
/// the operation. A failed outcome may leave partial changes behind: the
/// caller rolls them back (see [`execute_in_savepoint`]).
pub fn execute_operation(connection: &Conn, operation: &Operation) -> Outcome {
    match operation {
        Operation::Instances(op) => {
            Outcome::Instances(crate::instance_store::execute(connection, op))
        }
        Operation::Permissions(op) => {
            Outcome::Permissions(crate::permissions::ops::execute(connection, op))
        }
        Operation::Directory(op) => Outcome::Directory(crate::directory::apply(connection, op)),
        Operation::Kv(op) => Outcome::Kv(crate::extensions::storage::apply(connection, op)),
        Operation::Migrate(op) => Outcome::Migrate(migrate(connection, op)),
    }
}

/// The savepoint [`execute_in_savepoint`] uses.
const SAVEPOINT: &str = "rift_operation";

/// [`execute_operation`] in a savepoint of the caller's open transaction: a
/// failed operation rolls back alone, and later operations see the changes
/// of earlier successful ones. This is how a cluster leader executes a batch
/// of operations in one captured transaction. An `Err` means the savepoint
/// itself failed and the transaction should be abandoned.
pub fn execute_in_savepoint(connection: &Conn, operation: &Operation) -> Result<Outcome> {
    connection.execute_batch(&format!("SAVEPOINT {SAVEPOINT}"))?;
    let outcome = execute_operation(connection, operation);
    if outcome.failed() {
        connection.execute_batch(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))?;
    }
    connection.execute_batch(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))?;
    Ok(outcome)
}

/// Where operations run. [`Database::run`] hands every operation to the
/// database's executor.
pub trait Executor: Send + Sync {
    /// Run `operation` and return its outcome once the change is visible
    /// through `database` (so the caller reads its own write) and its apply
    /// hooks have run. With a `deadline`, give up with
    /// [`io::ErrorKind::TimedOut`] once it passes, before the change commits.
    fn run(
        &self,
        database: &Database,
        operation: Operation,
        deadline: Option<Instant>,
    ) -> io::Result<Outcome>;
}

/// The executor of a single Rift: [`Database::execute_locally`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Local;

impl Executor for Local {
    fn run(
        &self,
        database: &Database,
        operation: Operation,
        deadline: Option<Instant>,
    ) -> io::Result<Outcome> {
        database.execute_locally(operation, deadline)
    }
}

/// The names of tables a write changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedTables(BTreeSet<String>);

impl ChangedTables {
    pub fn insert(&mut self, table: impl Into<String>) {
        self.0.insert(table.into());
    }

    pub fn contains(&self, table: &str) -> bool {
        self.0.contains(table)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// Add every table of `other`.
    pub fn extend(&mut self, other: &ChangedTables) {
        self.0.extend(other.0.iter().cloned());
    }
}

impl<S: Into<String>> FromIterator<S> for ChangedTables {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        Self(iter.into_iter().map(Into::into).collect())
    }
}

/// `Result<T, impl Display>` to `Result<T, String>`, for execute functions
/// whose failures travel as messages.
pub(crate) trait OrMessage<T> {
    fn or_message(self) -> std::result::Result<T, String>;
}

impl<T, E: std::fmt::Display> OrMessage<T> for std::result::Result<T, E> {
    fn or_message(self) -> std::result::Result<T, String> {
        self.map_err(|error| error.to_string())
    }
}

/// Most distinct strings [`intern`] keeps.
const MAX_INTERNED: usize = 4096;

/// A `'static` copy of `text`, for error types whose fields are static
/// strings and that come back from an outcome. The values are the literals of
/// this program, so the set stays small; past [`MAX_INTERNED`] distinct
/// values, `"value"` stands in.
pub(crate) fn intern(text: &str) -> &'static str {
    static INTERNED: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut interned = INTERNED.lock().unwrap_or_else(|error| error.into_inner());
    let set = interned.get_or_insert_with(HashSet::new);
    if let Some(known) = set.get(text) {
        return known;
    }
    if set.len() >= MAX_INTERNED {
        return "value";
    }
    let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
    set.insert(leaked);
    leaked
}
