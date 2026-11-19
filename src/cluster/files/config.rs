//! `config` entries: configuration deployments, the operator audit trail
//! and what each peer runs.
//!
//! | Table | Key | Holds |
//! | --- | --- | --- |
//! | `config_deployments` | id | the last [`HISTORY`] deployments: leader time, actor, action, revision, SHA-256 and text of the source, the shared sections the proposer evaluated |
//! | `config_audit` | id | the last [`AUDIT`] operator audit records |
//! | `config_peers` | peer | the SHA-256 of the source each peer runs, the deployment it is, its shared sections, and why its last reload failed |
//!
//! The tables appear with the first entry that needs them, on every peer
//! at the same index.
use super::shared::{Shared, sha256};
use crate::cluster::{
    sql_error,
    state::{ApplyContext, Entry, Payload, Reply},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

pub(super) const DEPLOYMENTS: &str = "config_deployments";
pub(super) const AUDIT_TABLE: &str = "config_audit";
pub(super) const PEERS: &str = "config_peers";
const VERSION: u32 = 1;
/// Deployments kept, as in `deployments.json` on a single Rift.
pub const HISTORY: i64 = 64;
/// Audit records kept, as in `audit.jsonl` on a single Rift.
pub const AUDIT: i64 = 1024;
/// The largest source a deployment carries (`control::MAX_SOURCE`).
pub const MAX_SOURCE: usize = 256 * 1024;
/// Part of the refusal of a deployment that was not evaluated for every
/// member: the proposer evaluates it again and retries.
pub(super) const NOT_VALIDATED: &str = "was not evaluated for peer";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS config_deployments (
    id INTEGER PRIMARY KEY NOT NULL,
    time_ms INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    revision TEXT NOT NULL,
    digest TEXT NOT NULL,
    source TEXT NOT NULL,
    shared TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS config_audit (
    id INTEGER PRIMARY KEY NOT NULL,
    record TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS config_peers (
    peer TEXT PRIMARY KEY NOT NULL,
    digest TEXT NOT NULL,
    deployment INTEGER,
    shared TEXT NOT NULL,
    error TEXT,
    time_ms INTEGER NOT NULL
) STRICT;";

/// A deployment imported from `deployments.json` by `rift cluster init`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedDeployment {
    pub time_ms: u64,
    pub actor: String,
    pub action: String,
    pub revision: String,
    pub source: String,
}

/// The content of a `config` entry (version 1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ConfigEntry {
    /// A configuration deployment. `members` holds the shared hash the
    /// proposer evaluated for every member; apply refuses the deployment
    /// unless it covers every member and they all agree.
    Deploy {
        actor: String,
        action: String,
        revision: String,
        source: String,
        shared: Shared,
        members: BTreeMap<String, String>,
    },
    /// Operator audit records, in order.
    Audit { records: Vec<Value> },
    /// What `peer` runs: the SHA-256 of its source, the shared sections it
    /// evaluates to, and why its last reload failed.
    Report {
        peer: String,
        digest: String,
        shared: Shared,
        error: Option<String>,
    },
    /// History and audit records of a single Rift, from `rift cluster init`.
    Import {
        deployments: Vec<ImportedDeployment>,
        audit: Vec<Value>,
    },
}

impl ConfigEntry {
    pub fn entry(&self) -> Entry {
        Entry::Config(Payload::new(
            VERSION,
            serde_json::to_value(self).unwrap_or(Value::Null),
        ))
    }
}

/// One deployment as recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Deployment {
    pub id: u64,
    pub time_ms: u64,
    pub actor: String,
    pub action: String,
    pub revision: String,
    /// SHA-256 of the source.
    pub digest: String,
    #[serde(skip)]
    pub source: String,
    pub shared: Shared,
}

/// One peer's report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PeerReport {
    pub peer: String,
    pub digest: String,
    /// The deployment whose source the peer runs, if it runs a deployed one.
    pub deployment: Option<u64>,
    pub shared: Shared,
    pub error: Option<String>,
    pub time_ms: u64,
}

fn sql_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// Apply a `config` entry inside its transaction.
pub(super) fn apply(
    db: &Connection,
    payload: &Payload,
    ctx: &mut ApplyContext,
) -> io::Result<Reply> {
    if payload.v != VERSION {
        return Ok(Reply::err(format!(
            "config entry version {} is not supported by this peer",
            payload.v
        )));
    }
    let entry: ConfigEntry = match serde_json::from_value(payload.data.clone()) {
        Ok(entry) => entry,
        Err(error) => return Ok(Reply::err(format!("malformed config entry: {error}"))),
    };
    db.execute_batch(SCHEMA).map_err(sql_error)?;
    let time = sql_i64(ctx.time_ms);
    match entry {
        ConfigEntry::Deploy {
            actor,
            action,
            revision,
            source,
            shared,
            members,
        } => {
            if source.len() > MAX_SOURCE {
                return Ok(Reply::err("configuration exceeds 256 KiB source limit"));
            }
            let names = member_names(db)?;
            if let Some(missing) = names.iter().find(|name| !members.contains_key(*name)) {
                return Ok(Reply::err(format!(
                    "the deployment {NOT_VALIDATED} {missing}; evaluate it again"
                )));
            }
            let differing: BTreeSet<&String> = members
                .iter()
                .filter(|(_, hash)| **hash != shared.hash)
                .map(|(name, _)| name)
                .collect();
            if !differing.is_empty() {
                return Ok(Reply::err(format!(
                    "the shared configuration sections differ on {}; the deployment is not activated",
                    differing
                        .into_iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            let id = insert_deployment(db, time, &actor, &action, &revision, &source, &shared)?;
            ctx.changes.touch(DEPLOYMENTS, id.to_string());
            Ok(Reply::ok(
                serde_json::json!({ "id": id, "revision": revision }),
            ))
        }
        ConfigEntry::Audit { records } => {
            for record in &records {
                insert_audit(db, record)?;
            }
            prune_audit(db)?;
            ctx.changes.touch_table(AUDIT_TABLE);
            Ok(Reply::ok(records.len()))
        }
        ConfigEntry::Report {
            peer,
            digest,
            shared,
            error,
        } => {
            if peer.is_empty() || peer.len() > 128 {
                return Ok(Reply::err("config report: invalid peer name"));
            }
            let deployment: Option<i64> = db
                .query_row(
                    "SELECT id FROM config_deployments WHERE digest = ?1 ORDER BY id DESC LIMIT 1",
                    [&digest],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            let error: Option<String> = error.map(|error| error.chars().take(2000).collect());
            db.execute(
                "INSERT INTO config_peers (peer, digest, deployment, shared, error, time_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (peer) DO UPDATE SET digest = excluded.digest,
                 deployment = excluded.deployment, shared = excluded.shared,
                 error = excluded.error, time_ms = excluded.time_ms",
                params![
                    peer,
                    digest,
                    deployment,
                    serde_json::to_string(&shared).map_err(crate::cluster::json_error)?,
                    error,
                    time
                ],
            )
            .map_err(sql_error)?;
            ctx.changes.touch(PEERS, peer);
            Ok(Reply::ok(deployment))
        }
        ConfigEntry::Import { deployments, audit } => {
            for deployment in &deployments {
                if deployment.source.len() > MAX_SOURCE {
                    return Ok(Reply::err(
                        "an imported deployment exceeds the 256 KiB source limit",
                    ));
                }
                let id = insert_deployment(
                    db,
                    sql_i64(deployment.time_ms),
                    &deployment.actor,
                    &deployment.action,
                    &deployment.revision,
                    &deployment.source,
                    &Shared::default(),
                )?;
                ctx.changes.touch(DEPLOYMENTS, id.to_string());
            }
            for record in &audit {
                insert_audit(db, record)?;
            }
            prune_audit(db)?;
            if !audit.is_empty() {
                ctx.changes.touch_table(AUDIT_TABLE);
            }
            Ok(Reply::ok(serde_json::json!({
                "deployments": deployments.len(),
                "audit": audit.len(),
            })))
        }
    }
}

fn insert_deployment(
    db: &Connection,
    time: i64,
    actor: &str,
    action: &str,
    revision: &str,
    source: &str,
    shared: &Shared,
) -> io::Result<i64> {
    let id: i64 = db
        .query_row(
            "SELECT COALESCE(MAX(id), 0) + 1 FROM config_deployments",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    let actor: String = actor.chars().take(128).collect();
    let action: String = action.chars().take(64).collect();
    db.execute(
        "INSERT INTO config_deployments (id, time_ms, actor, action, revision, digest, source, shared)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            time,
            actor,
            action,
            revision,
            sha256(source.as_bytes()),
            source,
            serde_json::to_string(shared).map_err(crate::cluster::json_error)?
        ],
    )
    .map_err(sql_error)?;
    db.execute(
        "DELETE FROM config_deployments WHERE id <= ?1",
        [id - HISTORY],
    )
    .map_err(sql_error)?;
    Ok(id)
}

fn insert_audit(db: &Connection, record: &Value) -> io::Result<()> {
    db.execute(
        "INSERT INTO config_audit (id, record)
         VALUES ((SELECT COALESCE(MAX(id), 0) + 1 FROM config_audit), ?1)",
        [record.to_string()],
    )
    .map_err(sql_error)?;
    Ok(())
}

fn prune_audit(db: &Connection) -> io::Result<()> {
    db.execute(
        "DELETE FROM config_audit WHERE id <= (SELECT MAX(id) FROM config_audit) - ?1",
        [AUDIT],
    )
    .map_err(sql_error)?;
    Ok(())
}

/// Whether `table` exists yet.
pub(super) fn table_exists(db: &Connection, table: &str) -> io::Result<bool> {
    db.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )
    .map_err(sql_error)
}

const DEPLOYMENT_COLUMNS: &str = "id, time_ms, actor, action, revision, digest, source, shared";

fn deployment_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(Deployment, String)> {
    Ok((
        Deployment {
            id: to_u64(row.get(0)?),
            time_ms: to_u64(row.get(1)?),
            actor: row.get(2)?,
            action: row.get(3)?,
            revision: row.get(4)?,
            digest: row.get(5)?,
            source: row.get(6)?,
            shared: Shared::default(),
        },
        row.get(7)?,
    ))
}

fn with_shared((mut deployment, shared): (Deployment, String)) -> Deployment {
    deployment.shared = serde_json::from_str(&shared).unwrap_or_default();
    deployment
}

/// The last deployment, if any.
pub fn latest(db: &Connection) -> io::Result<Option<Deployment>> {
    if !table_exists(db, DEPLOYMENTS)? {
        return Ok(None);
    }
    db.query_row(
        &format!("SELECT {DEPLOYMENT_COLUMNS} FROM config_deployments ORDER BY id DESC LIMIT 1"),
        [],
        deployment_row,
    )
    .optional()
    .map_err(sql_error)
    .map(|row| row.map(with_shared))
}

/// Every retained deployment, newest first.
pub fn deployments(db: &Connection) -> io::Result<Vec<Deployment>> {
    if !table_exists(db, DEPLOYMENTS)? {
        return Ok(Vec::new());
    }
    let mut statement = db
        .prepare(&format!(
            "SELECT {DEPLOYMENT_COLUMNS} FROM config_deployments ORDER BY id DESC"
        ))
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], deployment_row)
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    Ok(rows.into_iter().map(with_shared).collect())
}

/// The source of deployment `id`, while it is retained.
pub fn source(db: &Connection, id: u64) -> io::Result<Option<String>> {
    if !table_exists(db, DEPLOYMENTS)? {
        return Ok(None);
    }
    db.query_row(
        "SELECT source FROM config_deployments WHERE id = ?1",
        [sql_i64(id)],
        |row| row.get(0),
    )
    .optional()
    .map_err(sql_error)
}

/// Every retained audit record, oldest first.
pub fn audit_records(db: &Connection) -> io::Result<Vec<Value>> {
    if !table_exists(db, AUDIT_TABLE)? {
        return Ok(Vec::new());
    }
    let mut statement = db
        .prepare("SELECT record FROM config_audit ORDER BY id")
        .map_err(sql_error)?;
    let texts = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    Ok(texts
        .into_iter()
        .filter_map(|text| serde_json::from_str(&text).ok())
        .collect())
}

/// Every peer's last report, by peer name.
pub fn reports(db: &Connection) -> io::Result<BTreeMap<String, PeerReport>> {
    if !table_exists(db, PEERS)? {
        return Ok(BTreeMap::new());
    }
    let mut statement = db
        .prepare("SELECT peer, digest, deployment, shared, error, time_ms FROM config_peers")
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    Ok(rows
        .into_iter()
        .map(|(peer, digest, deployment, shared, error, time)| {
            (
                peer.clone(),
                PeerReport {
                    peer,
                    digest,
                    deployment: deployment.map(to_u64),
                    shared: serde_json::from_str(&shared).unwrap_or_default(),
                    error,
                    time_ms: to_u64(time),
                },
            )
        })
        .collect())
}

/// The names of every member, from the framework's peer table.
pub(super) fn member_names(db: &Connection) -> io::Result<BTreeSet<String>> {
    let mut statement = db
        .prepare("SELECT name FROM cluster_peers ORDER BY name")
        .map_err(sql_error)?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sql_error)?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(sql_error)?;
    Ok(names)
}
