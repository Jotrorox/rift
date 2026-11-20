//! The instance database as the cluster's state machine
//! (docs/clustering.md, "Writes to the instance database"): the entry kinds
//! that change it and how every peer applies them.
//!
//! - `operation`: typed [`Operation`]s the leader executed in one captured
//!   transaction, each in its own savepoint, with their outcomes and the
//!   changeset they produced together. Every peer applies the changeset and
//!   calls the hooks of each operation that succeeded.
//! - `kv` and `directory`: the hot paths. They carry the operation itself,
//!   which every peer executes deterministically when the entry applies
//!   (quota checks included), so no changeset and no leader execution is
//!   needed.
//! - `migrate`: one component's schema migration, executed when applied, on
//!   every peer at the same index. The leader proposes one only once a
//!   committed `features` entry says every member supports the version
//!   ([`schema_feature`]).
//!
//! Every operation's outcome is stored under its `(peer, incarnation,
//! serial)` in `cluster_proposals`, so a retry after a leader change gets the
//! stored outcome instead of running again.
use super::{
    state::{ApplyContext, Entry, Origin, Payload, Reply, store_reply},
    unhex,
};
use crate::database::{
    self as instance, Change, ChangedTables, Conn, Database, Operation, Outcome,
    changeset::{self, Changeset},
    operation::{self, MigrateOp},
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io;

/// The version of the `operation`, `kv`, `directory` and `migrate` entry
/// bodies this Rift writes and applies.
pub const VERSION: u32 = 1;

/// One operation the leader executed, with its outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Executed {
    pub origin: Origin,
    pub operation: Operation,
    pub outcome: Outcome,
}

/// The body of an `operation` entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    pub ops: Vec<Executed>,
    /// The changeset of every successful operation together, in hex.
    pub changeset: String,
}

/// An apply hook call an applied entry asks for, made after its
/// transaction committed and while apply still holds the writer.
#[derive(Debug, Clone)]
pub struct Fire {
    tables: ChangedTables,
    operation: Option<Operation>,
    outcome: Option<Outcome>,
}

impl Fire {
    /// The hooks of `operation`'s tables, as a local write calls them.
    fn of(operation: Operation, outcome: Outcome) -> Self {
        Self {
            tables: operation.tables(),
            operation: Some(operation),
            outcome: Some(outcome),
        }
    }

    pub(crate) fn change(&self) -> Change<'_> {
        Change {
            tables: &self.tables,
            operation: self.operation.as_ref(),
            outcome: self.outcome.as_ref(),
            reload: false,
        }
    }
}

/// `operation` entries this peer proposed as leader that are not applied
/// yet: the leader executes new operations on top of their changesets
/// (group commit). Kept by the state machine and changed only while the
/// writer is held, so apply and execution agree on what is in flight.
#[derive(Debug, Default)]
pub struct Pending {
    batches: Vec<PendingBatch>,
}

#[derive(Debug)]
struct PendingBatch {
    origin: Origin,
    changeset: Changeset,
    /// The origins of the operations it carries.
    ops: Vec<Origin>,
}

impl Pending {
    pub fn insert(&mut self, origin: Origin, changeset: Changeset, ops: Vec<Origin>) {
        self.batches.push(PendingBatch {
            origin,
            changeset,
            ops,
        });
    }

    /// The batch proposed under `origin` is applied, refused or lost.
    pub fn remove(&mut self, origin: &Origin) {
        self.batches.retain(|batch| batch.origin != *origin);
    }

    pub fn clear(&mut self) {
        self.batches.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    /// The changesets in flight, in proposal order.
    pub fn changesets(&self) -> Vec<Changeset> {
        self.batches
            .iter()
            .map(|batch| batch.changeset.clone())
            .collect()
    }

    /// Whether the operation proposed under `origin` is in flight.
    pub fn holds(&self, origin: &Origin) -> bool {
        self.batches
            .iter()
            .any(|batch| batch.ops.iter().any(|op| op == origin))
    }
}

/// The entry for `operation` when it has a deterministic kind of its own
/// (`kv`, `directory`); `None` for operations the leader executes.
pub fn own_entry(operation: &Operation) -> io::Result<Option<Entry>> {
    let encode = |value: Result<serde_json::Value, serde_json::Error>| {
        value
            .map(|data| Payload::new(VERSION, data))
            .map_err(super::json_error)
    };
    Ok(match operation {
        Operation::Kv(op) => Some(Entry::Kv(encode(serde_json::to_value(op))?)),
        Operation::Directory(op) => Some(Entry::Directory(encode(serde_json::to_value(op))?)),
        Operation::Instances(_) | Operation::Permissions(_) | Operation::Migrate(_) => None,
    })
}

/// The `migrate` entry for `op`.
pub fn migrate_entry(op: &MigrateOp) -> io::Result<Entry> {
    serde_json::to_value(op)
        .map(|data| Entry::Migrate(Payload::new(VERSION, data)))
        .map_err(super::json_error)
}

/// The outcome an applied entry's reply carries.
pub fn outcome_of(reply: Reply) -> io::Result<Outcome> {
    match reply {
        Reply::Ok(value) => serde_json::from_value(value).map_err(super::json_error),
        Reply::Err(message) => Err(io::Error::other(format!("cluster: {message}"))),
    }
}

/// The feature string a peer announces for each schema version it can run
/// against; a `migrate` entry to `version` needs it enabled.
pub fn schema_feature(component: &str, version: u32) -> String {
    format!("schema:{component}:{version}")
}

/// Every schema feature of this build: each component at every version up
/// to the one it writes.
pub fn schema_features() -> Vec<String> {
    operation::latest_versions()
        .into_iter()
        .flat_map(|(component, latest)| {
            (1..=latest).map(move |version| schema_feature(component, version))
        })
        .collect()
}

/// The components whose schema in `database` is older than this build
/// writes, with their version and this build's: empty when the replica
/// matches. A component newer than this build is an error.
pub fn schema_behind(database: &Database) -> io::Result<Vec<(&'static str, u32, u32)>> {
    let connection = database.read();
    let mut behind = Vec::new();
    for (component, latest) in operation::latest_versions() {
        let version = instance::version(&connection, component).map_err(|e| database.error(e))?;
        if version > latest {
            return Err(database.error(format!(
                "{component} has schema version {version}, newer than this Rift's {latest}; upgrade this peer"
            )));
        }
        if version < latest {
            behind.push((component, version, latest));
        }
    }
    Ok(behind)
}

fn decode<T: DeserializeOwned>(payload: &Payload, kind: &str) -> Result<T, Reply> {
    if payload.v != VERSION {
        return Err(Reply::err(format!(
            "{kind} entry version {} is not one this Rift applies",
            payload.v
        )));
    }
    serde_json::from_value(payload.data.clone())
        .map_err(|error| Reply::err(format!("{kind} entry: {error}")))
}

fn touch(ctx: &mut ApplyContext, operation: &Operation) {
    for table in operation.tables().iter() {
        ctx.changes.touch_table(table);
    }
}

/// Apply an `operation` entry: its changeset, then each operation's stored
/// outcome; the hooks of the successful ones go to `fires`. The flag is set
/// when the changeset conflicts with this replica, which then diverged.
pub(super) fn apply_operation(
    conn: &Conn,
    db: &Connection,
    payload: &Payload,
    ctx: &mut ApplyContext,
    fires: &mut Vec<Fire>,
) -> io::Result<(Reply, bool)> {
    let batch: Batch = match decode(payload, "operation") {
        Ok(batch) => batch,
        Err(reply) => return Ok((reply, false)),
    };
    let Some(bytes) = unhex(&batch.changeset) else {
        return Ok((
            Reply::err("operation entry: the changeset is not hex"),
            false,
        ));
    };
    if let Err(error) = changeset::apply_changeset(conn, &Changeset(bytes)) {
        return Ok((
            Reply::err(format!("operation entry does not apply here: {error}")),
            true,
        ));
    }
    for executed in batch.ops {
        store_reply(db, &executed.origin, &Reply::ok(&executed.outcome))?;
        if !executed.outcome.failed() {
            touch(ctx, &executed.operation);
            fires.push(Fire::of(executed.operation, executed.outcome));
        }
    }
    Ok((Reply::Ok(serde_json::Value::Null), false))
}

/// Execute `operation` deterministically as the entry applies: a failure
/// rolls back alone and is a normal outcome.
fn execute(
    conn: &Conn,
    operation: Operation,
    ctx: &mut ApplyContext,
    fires: &mut Vec<Fire>,
) -> io::Result<Reply> {
    let outcome = operation::execute_in_savepoint(conn, &operation)
        .map_err(|error| io::Error::other(format!("cluster storage: {error}")))?;
    let reply = Reply::ok(&outcome);
    if !outcome.failed() {
        touch(ctx, &operation);
        fires.push(Fire::of(operation, outcome));
    }
    Ok(reply)
}

/// Apply a `kv` entry: the mutation with its read checks and quotas.
pub(super) fn apply_kv(
    conn: &Conn,
    payload: &Payload,
    ctx: &mut ApplyContext,
    fires: &mut Vec<Fire>,
) -> io::Result<Reply> {
    match decode(payload, "kv") {
        Ok(op) => execute(conn, Operation::Kv(op), ctx, fires),
        Err(reply) => Ok(reply),
    }
}

/// Apply a `directory` entry: a player directory upsert.
pub(super) fn apply_directory(
    conn: &Conn,
    payload: &Payload,
    ctx: &mut ApplyContext,
    fires: &mut Vec<Fire>,
) -> io::Result<Reply> {
    match decode(payload, "directory") {
        Ok(op) => execute(conn, Operation::Directory(op), ctx, fires),
        Err(reply) => Ok(reply),
    }
}

/// Apply a `migrate` entry: one component's schema migration.
pub(super) fn apply_migrate(
    conn: &Conn,
    payload: &Payload,
    ctx: &mut ApplyContext,
    fires: &mut Vec<Fire>,
) -> io::Result<Reply> {
    match decode(payload, "migrate") {
        Ok(op) => execute(conn, Operation::Migrate(op), ctx, fires),
        Err(reply) => Ok(reply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_features_cover_every_version_up_to_the_latest() {
        let features = schema_features();
        for (component, latest) in operation::latest_versions() {
            for version in 1..=latest {
                assert!(features.contains(&schema_feature(component, version)));
            }
            assert!(!features.contains(&schema_feature(component, latest + 1)));
        }
    }

    #[test]
    fn pending_batches_keep_their_order_and_operations() {
        let origin = |serial| Origin {
            peer: "p0".into(),
            incarnation: 1,
            serial,
        };
        let mut pending = Pending::default();
        pending.insert(origin(1), Changeset(vec![1]), vec![origin(10)]);
        pending.insert(origin(2), Changeset(vec![2]), vec![origin(11)]);
        assert_eq!(
            pending.changesets(),
            [Changeset(vec![1]), Changeset(vec![2])]
        );
        assert!(pending.holds(&origin(11)));
        pending.remove(&origin(1));
        assert!(!pending.holds(&origin(10)));
        assert_eq!(pending.changesets(), [Changeset(vec![2])]);
        pending.clear();
        assert!(pending.is_empty());
    }
}
