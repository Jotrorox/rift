//! The replicated state machine.
//!
//! Every committed log entry is applied to one SQLite database in its own
//! transaction, together with `cluster_applied` (the entry's log id, the
//! membership and the leader's time), so a crash never applies an entry twice
//! or skips one: on restart openraft resumes after the recorded log id, and
//! an entry at or below it is skipped if it comes again.
//!
//! # Entries
//!
//! [`Entry`] has one variant per kind of docs/clustering.md, "The state
//! machine". Each carries a version `v`. The framework applies `peer`,
//! `features` and `checksum` itself; every other kind goes to the
//! [`Component`] registered for it, and an entry of a kind nobody registered
//! fails with a "no component registered" result (deterministically, so every
//! peer agrees). Feature gating keeps a kind from being proposed before a
//! committed `features` entry lists it.
//!
//! Applying never reads the clock or a random source. The leader stamps each
//! proposal with its wall clock; apply uses the larger of that stamp and the
//! previous entry's, so times never go backwards across leader changes.
//!
//! # Idempotent proposals
//!
//! A proposal carries `(peer, incarnation, serial)`. Its result is stored
//! under that key in the same transaction, and a proposal that arrives again
//! (a retry after a leader change) gets the stored result instead of being
//! applied twice. Results of older incarnations go when the peer joins with a
//! new one; within an incarnation the last 1024 serials are kept.
//!
//! # Snapshots
//!
//! A snapshot is a copy of the whole database made with SQLite's online
//! backup API, taken while apply is held, so it matches the log id it is
//! labelled with. Installing one pauses apply and leader execution (both
//! need the writer), writes it into the live database through the writer
//! with the backup API (SQLite swaps the pages in one transaction; no file
//! is renamed or replaced, which Windows refuses while handles are open),
//! reopens the writer and read connections, which also drops cached
//! statements, runs every component's [`Component::full_reload`] and every
//! apply hook of the instance database in full reload mode, and resumes.
//!
//! # The instance database
//!
//! The state machine lives in the instance database, opened in
//! [`instance::Mode::Replicated`]: entries are applied on its writer
//! connection, and the apply hooks of its components
//! ([`instance::Database::on_change`]) are called while apply still holds
//! it. The `operation`, `kv`, `directory` and `migrate` kinds are applied
//! here, by [`super::replicated`].
use super::{
    LogEntry, LogId, PeerId, SnapshotMeta, StoredMembership, TypeConfig, json_error,
    replicated::{self, Fire, Pending},
    sql_error,
};
use crate::database::{self as instance, Conn, changeset};
use aws_lc_rs::digest;
use futures_util::StreamExt;
use openraft::{
    EntryPayload, OptionalSend, RaftSnapshotBuilder,
    storage::{EntryResponder, RaftStateMachine},
    type_config::alias::SnapshotOf,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{broadcast, watch};

/// Version of the framework's own tables (`cluster_*`).
pub const SCHEMA_VERSION: i64 = 1;
/// Serials kept per peer incarnation for idempotent retries.
const KEPT_SERIALS: i64 = 1024;
/// Local checksums kept for comparing with a later `verify`.
const KEPT_CHECKSUMS: i64 = 16;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cluster_applied (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    log_id TEXT,
    membership TEXT NOT NULL,
    time_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS cluster_meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS cluster_proposals (
    peer TEXT NOT NULL,
    incarnation INTEGER NOT NULL,
    serial INTEGER NOT NULL,
    reply TEXT NOT NULL,
    PRIMARY KEY (peer, incarnation, serial)
) STRICT;
CREATE TABLE IF NOT EXISTS cluster_peers (
    name TEXT PRIMARY KEY NOT NULL,
    raft_id INTEGER NOT NULL UNIQUE,
    incarnation INTEGER NOT NULL,
    address TEXT NOT NULL,
    public_address TEXT,
    labels TEXT NOT NULL,
    witness INTEGER NOT NULL,
    version TEXT NOT NULL,
    kinds TEXT NOT NULL,
    expired INTEGER NOT NULL,
    since_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS cluster_features (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    kinds TEXT NOT NULL,
    settings TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS cluster_checksums (
    idx INTEGER PRIMARY KEY NOT NULL,
    hash TEXT NOT NULL
) STRICT;";

/// Tables a checksum leaves out: they record where this replica is, not
/// what it holds.
const UNCHECKED_TABLES: &[&str] = &["cluster_applied", "cluster_checksums"];

/// The kinds of log entry. Their names are the wire names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Operation,
    Claim,
    Kv,
    Directory,
    Stream,
    Peer,
    Config,
    Manifest,
    Features,
    Migrate,
    Checksum,
    /// Bus state besides streams: node session claims, the bus-login key
    /// and the peers' bus addresses (`crate::cluster::bus`).
    Bus,
    /// Named leases of plugins with fencing tokens
    /// (`crate::cluster::queues::leases`).
    Lease,
}

impl EntryKind {
    pub const ALL: [Self; 13] = [
        Self::Operation,
        Self::Claim,
        Self::Kv,
        Self::Directory,
        Self::Stream,
        Self::Peer,
        Self::Config,
        Self::Manifest,
        Self::Features,
        Self::Migrate,
        Self::Checksum,
        Self::Bus,
        Self::Lease,
    ];
    /// What the framework applies itself: its own kinds and those of the
    /// instance database (see [`super::replicated`]).
    pub const BUILT_IN: [Self; 7] = [
        Self::Peer,
        Self::Features,
        Self::Checksum,
        Self::Operation,
        Self::Kv,
        Self::Directory,
        Self::Migrate,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Operation => "operation",
            Self::Claim => "claim",
            Self::Kv => "kv",
            Self::Directory => "directory",
            Self::Stream => "stream",
            Self::Peer => "peer",
            Self::Config => "config",
            Self::Manifest => "manifest",
            Self::Features => "features",
            Self::Migrate => "migrate",
            Self::Checksum => "checksum",
            Self::Bus => "bus",
            Self::Lease => "lease",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// Kinds a cluster can always use: they bootstrap the `features` entry
    /// that enables the others.
    pub fn always_enabled(self) -> bool {
        matches!(self, Self::Peer | Self::Features)
    }
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The content of a kind a later step defines; its component decodes `data`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Payload {
    pub v: u32,
    pub data: serde_json::Value,
}

impl Payload {
    pub fn new(v: u32, data: serde_json::Value) -> Self {
        Self { v, data }
    }
}

/// `peer`: membership facts that are not Raft membership.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerEntry {
    /// `rift admin cluster add`: allocates the peer's Raft id (the result).
    Added {
        v: u32,
        name: String,
        address: String,
        witness: bool,
    },
    /// A peer started with a new incarnation. Releasing the claims of its
    /// older incarnations is an apply hook of the presence step.
    Joined {
        v: u32,
        name: String,
        raft_id: PeerId,
        incarnation: u64,
        address: String,
        public_address: Option<String>,
        labels: BTreeMap<String, String>,
        witness: bool,
        version: String,
        kinds: Vec<String>,
    },
    /// The leader has not heard from the peer for `cluster.peer_timeout`.
    Expired {
        v: u32,
        name: String,
        incarnation: u64,
    },
    /// The peer left the membership.
    Removed { v: u32, name: String },
}

/// `features`: what every member supports. Kinds are strings so a newer
/// peer's kinds do not break an older peer's parsing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeaturesEntry {
    pub v: u32,
    pub kinds: BTreeSet<String>,
    pub settings: BTreeMap<String, String>,
}

/// `checksum`: divergence detection in two steps. Applying `mark` hashes
/// the state machine on every peer at the same index and keeps the hash
/// locally; the leader then proposes `verify` with its own hash for that
/// index, and a peer whose hash differs reports itself diverged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumEntry {
    Mark { v: u32 },
    Verify { v: u32, index: u64, hash: String },
}

/// One log entry's content, adjacently tagged: `{"kind": ..., "body": ...}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body", rename_all = "snake_case")]
pub enum Entry {
    Operation(Payload),
    Claim(Payload),
    Kv(Payload),
    Directory(Payload),
    Stream(Payload),
    Peer(PeerEntry),
    Config(Payload),
    Manifest(Payload),
    Features(FeaturesEntry),
    Migrate(Payload),
    Checksum(ChecksumEntry),
    Bus(Payload),
    Lease(Payload),
}

impl Entry {
    pub fn kind(&self) -> EntryKind {
        match self {
            Self::Operation(_) => EntryKind::Operation,
            Self::Claim(_) => EntryKind::Claim,
            Self::Kv(_) => EntryKind::Kv,
            Self::Directory(_) => EntryKind::Directory,
            Self::Stream(_) => EntryKind::Stream,
            Self::Peer(_) => EntryKind::Peer,
            Self::Config(_) => EntryKind::Config,
            Self::Manifest(_) => EntryKind::Manifest,
            Self::Features(_) => EntryKind::Features,
            Self::Migrate(_) => EntryKind::Migrate,
            Self::Checksum(_) => EntryKind::Checksum,
            Self::Bus(_) => EntryKind::Bus,
            Self::Lease(_) => EntryKind::Lease,
        }
    }

    /// The payload of a component-defined kind, `None` for built-in kinds.
    pub fn payload(&self) -> Option<&Payload> {
        match self {
            Self::Operation(payload)
            | Self::Claim(payload)
            | Self::Kv(payload)
            | Self::Directory(payload)
            | Self::Stream(payload)
            | Self::Config(payload)
            | Self::Manifest(payload)
            | Self::Migrate(payload)
            | Self::Bus(payload)
            | Self::Lease(payload) => Some(payload),
            Self::Peer(_) | Self::Features(_) | Self::Checksum(_) => None,
        }
    }
}

/// Who proposed an entry, for idempotent retries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub peer: String,
    pub incarnation: u64,
    pub serial: u64,
}

/// What a log entry carries: the entry, its origin and the leader's stamp.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub origin: Option<Origin>,
    /// The leader's wall clock in Unix milliseconds, set when it appends.
    pub time_ms: u64,
    /// For proposals of leader tasks: the term they were decided in. Apply
    /// refuses the entry if another term's leader appended it, so a deposed
    /// leader's decision never takes effect through the new leader.
    pub term: Option<u64>,
    pub entry: Entry,
}

impl fmt::Display for Proposal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.entry.kind())?;
        if let Some(origin) = &self.origin {
            write!(
                f,
                " from {}#{}/{}",
                origin.peer, origin.incarnation, origin.serial
            )?;
        }
        Ok(())
    }
}

/// The result of applying an entry. `Err` is a deterministic refusal (every
/// peer reaches the same one) and rolls back the entry's writes; failures of
/// the machine itself are `io::Error`s and stop the Raft node instead.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Ok(serde_json::Value),
    Err(String),
}

impl Reply {
    pub fn ok(value: impl Serialize) -> Self {
        match serde_json::to_value(value) {
            Ok(value) => Self::Ok(value),
            Err(error) => Self::Err(format!("result encoding: {error}")),
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self::Err(message.into())
    }

    pub fn is_err(&self) -> bool {
        matches!(self, Self::Err(_))
    }

    pub fn into_result(self) -> Result<serde_json::Value, String> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Err(message) => Err(message),
        }
    }
}

/// What an applied entry changed, for apply hooks: table names, each with
/// the keys that changed (an empty set means "the whole table").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub index: u64,
    pub kind: Option<EntryKind>,
    pub tables: BTreeMap<String, BTreeSet<String>>,
}

impl Changes {
    pub fn new(index: u64, kind: Option<EntryKind>) -> Self {
        Self {
            index,
            kind,
            tables: BTreeMap::new(),
        }
    }

    /// Record that `key` of `table` changed.
    pub fn touch(&mut self, table: &str, key: impl Into<String>) {
        self.tables
            .entry(table.to_owned())
            .or_default()
            .insert(key.into());
    }

    /// Record that `table` changed as a whole.
    pub fn touch_table(&mut self, table: &str) {
        self.tables.entry(table.to_owned()).or_default().clear();
    }

    pub fn touched(&self, table: &str) -> bool {
        self.tables.contains_key(table)
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

/// Where an entry is applied.
#[derive(Clone, Debug)]
pub struct ApplyContext {
    pub index: u64,
    /// The term of the leader that appended the entry.
    pub term: u64,
    /// The entry's time: the leader's stamp, never earlier than the
    /// previous entry's.
    pub time_ms: u64,
    pub origin: Option<Origin>,
    /// Filled by the component with what it changed.
    pub changes: Changes,
}

/// A part of Rift that keeps state in the replicated database: the instance
/// registry, the permission store, the player directory, the key-value
/// store... Register components before the cluster starts (see
/// [`super::node::Options::components`]): every peer must apply every entry
/// with the same components, or the replicas diverge.
pub trait Component: Send + Sync + 'static {
    /// For logs and errors.
    fn name(&self) -> &'static str;

    /// Entry kinds this component applies; each kind has at most one
    /// component. A pure apply hook claims none.
    fn kinds(&self) -> Vec<EntryKind> {
        Vec::new()
    }

    /// Apply `entry` (one of [`Component::kinds`]) inside its transaction.
    /// `db` reads and writes the state; record what changed in
    /// `ctx.changes`. Return [`Reply::Err`] to refuse deterministically,
    /// which rolls back this entry's writes; an `io::Error` means the
    /// replica itself failed and stops this peer's Raft node.
    fn apply(&self, db: &Connection, entry: &Entry, ctx: &mut ApplyContext) -> io::Result<Reply> {
        let _ = (db, ctx);
        Ok(Reply::err(format!(
            "component {} does not apply {} entries",
            self.name(),
            entry.kind()
        )))
    }

    /// After the framework applied a `peer` entry without refusing it, in
    /// the same transaction: release what belonged to an older incarnation
    /// of the peer, or to a peer that expired or was removed. Record what
    /// changed in `ctx.changes`; an `io::Error` stops this peer's Raft node.
    fn peer_applied(
        &self,
        db: &Connection,
        entry: &PeerEntry,
        ctx: &mut ApplyContext,
    ) -> io::Result<()> {
        let _ = (db, entry, ctx);
        Ok(())
    }

    /// After every applied entry that changed something, with what changed:
    /// refresh in-memory mirrors and raise the events a local write would
    /// have raised. Runs while apply waits, so it must be quick; spawn
    /// anything slow.
    fn after_apply(&self, db: &Connection, changes: &Changes) {
        let _ = (db, changes);
    }

    /// After a snapshot install replaced the whole database: reload all
    /// in-memory state from `db`.
    fn full_reload(&self, db: &Connection) -> io::Result<()> {
        let _ = db;
        Ok(())
    }

    /// Feed state that lives outside SQLite tables into a checksum. Tables
    /// are hashed by the framework.
    fn checksum(&self, db: &Connection, digest: &mut digest::Context) -> io::Result<()> {
        let _ = (db, digest);
        Ok(())
    }
}

/// The registered components, by the kinds they apply.
#[derive(Clone, Default)]
pub struct Components {
    by_kind: BTreeMap<EntryKind, Arc<dyn Component>>,
    all: Vec<Arc<dyn Component>>,
}

impl Components {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, component: Arc<dyn Component>) -> io::Result<()> {
        for kind in component.kinds() {
            if EntryKind::BUILT_IN.contains(&kind) {
                return Err(super::invalid(format!(
                    "component {}: {kind} entries are applied by the cluster itself",
                    component.name()
                )));
            }
            if let Some(other) = self.by_kind.get(&kind) {
                return Err(super::invalid(format!(
                    "component {}: {kind} entries already belong to component {}",
                    component.name(),
                    other.name()
                )));
            }
        }
        for kind in component.kinds() {
            self.by_kind.insert(kind, component.clone());
        }
        self.all.push(component);
        Ok(())
    }

    /// Every kind this build can apply: the built-in ones and those of the
    /// registered components. Announced in handshakes and `peer joined`.
    pub fn supported(&self) -> BTreeSet<EntryKind> {
        EntryKind::BUILT_IN
            .into_iter()
            .chain(self.by_kind.keys().copied())
            .collect()
    }
}

/// The SQLite connection under `conn`; the state machine only lives in a
/// SQLite file.
pub(super) fn sqlite(conn: &Conn) -> io::Result<&Connection> {
    conn.sqlite()
        .ok_or_else(|| super::invalid("the cluster state needs a SQLite database"))
}

/// Make a SQLite file durable through a handle that may write: Windows
/// refuses to flush a file opened read-only ("Access is denied").
fn sync_file(path: &Path) -> io::Result<()> {
    fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// A snapshot on disk: a complete copy of the state database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFile {
    pub path: PathBuf,
}

/// A row of `cluster_peers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerRow {
    pub name: String,
    pub raft_id: PeerId,
    /// 0 until the peer first joined.
    pub incarnation: u64,
    pub address: String,
    pub public_address: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub witness: bool,
    pub version: String,
    pub kinds: Vec<String>,
    pub expired: bool,
    pub since_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct CurrentSnapshot {
    meta: SnapshotMeta,
    file: String,
}

/// The state machine. Cheap to clone.
#[derive(Clone)]
pub struct StateMachine {
    inner: Arc<Inner>,
}

struct Inner {
    /// The instance database; apply uses its writer connection.
    db: Arc<instance::Database>,
    components: Components,
    snapshots: PathBuf,
    applied: watch::Sender<u64>,
    changes: broadcast::Sender<Changes>,
    diverged: AtomicBool,
    /// A snapshot or repair copy is being installed.
    installing: AtomicBool,
    /// This peer's `operation` entries that are proposed but not applied
    /// yet, which the leader executes new operations on top of. Changed only
    /// while holding the writer.
    pending: Mutex<Pending>,
}

impl StateMachine {
    /// The state machine in `db`, which must be a SQLite file opened in
    /// [`instance::Mode::Replicated`].
    pub fn open(
        db: Arc<instance::Database>,
        components: Components,
        snapshots: PathBuf,
    ) -> io::Result<Self> {
        if db.mode() != instance::Mode::Replicated {
            return Err(super::invalid(format!(
                "cluster: the instance database {} must be opened for replication",
                db.label()
            )));
        }
        let applied = {
            let conn = db.lock();
            let conn = sqlite(&conn)?;
            prepare(conn)?;
            applied_log_id(conn)?.map_or(0, |log_id| log_id.index)
        };
        fs::create_dir_all(&snapshots).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", snapshots.display()))
        })?;
        let (applied, _) = watch::channel(applied);
        let (changes, _) = broadcast::channel(1024);
        Ok(Self {
            inner: Arc::new(Inner {
                db,
                components,
                snapshots,
                applied,
                changes,
                diverged: AtomicBool::new(false),
                installing: AtomicBool::new(false),
                pending: Mutex::new(Pending::default()),
            }),
        })
    }

    pub fn components(&self) -> &Components {
        &self.inner.components
    }

    /// The instance database the state machine lives in.
    pub fn database(&self) -> &Arc<instance::Database> {
        &self.inner.db
    }

    /// Whether a snapshot or repair copy is being installed: logins should
    /// wait (docs/clustering.md, "Snapshots and log purging").
    pub fn is_installing(&self) -> bool {
        self.inner.installing.load(Ordering::SeqCst)
    }

    /// Run `work` on the writer with this peer's pending `operation`
    /// entries: how the leader executes operations without an apply in
    /// between.
    pub(super) fn with_writer<T>(
        &self,
        work: impl FnOnce(&mut Conn, &mut Pending) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut conn = self.inner.db.lock();
        let mut pending = self.inner.pending.lock().unwrap();
        work(&mut conn, &mut pending)
    }

    /// Forget a pending `operation` entry that will not be applied (its
    /// proposal failed).
    pub(super) fn forget_pending(&self, origin: &Origin) {
        let _conn = self.inner.db.lock();
        self.inner.pending.lock().unwrap().remove(origin);
    }

    /// Whether this peer has pending `operation` entries.
    pub(super) fn has_pending(&self) -> bool {
        !self.inner.pending.lock().unwrap().is_empty()
    }

    /// The index of the last applied entry (0 before the first).
    pub fn applied_index(&self) -> u64 {
        *self.inner.applied.borrow()
    }

    pub fn subscribe_applied(&self) -> watch::Receiver<u64> {
        self.inner.applied.subscribe()
    }

    /// What each applied entry changed, for node-level reactions.
    pub fn subscribe_changes(&self) -> broadcast::Receiver<Changes> {
        self.inner.changes.subscribe()
    }

    /// Wait until the entry at `index` is applied here: read your writes.
    pub async fn wait_applied(&self, index: u64, timeout: Duration) -> io::Result<()> {
        let mut applied = self.subscribe_applied();
        let wait = async {
            loop {
                if *applied.borrow_and_update() >= index {
                    return Ok(());
                }
                if applied.changed().await.is_err() {
                    return Err(io::Error::other("state machine closed"));
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("entry {index} was not applied here in time"),
            )
        })?
    }

    /// Read the state under the writer lock. Keep it short: apply waits.
    pub fn read<T>(&self, read: impl FnOnce(&Connection) -> io::Result<T>) -> io::Result<T> {
        let conn = self.inner.db.lock();
        read(sqlite(&conn)?)
    }

    pub fn peers(&self) -> io::Result<Vec<PeerRow>> {
        self.read(peer_rows)
    }

    /// The committed `features` entry, if any.
    pub fn features(&self) -> io::Result<Option<FeaturesEntry>> {
        self.read(|conn| {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT kinds, settings FROM cluster_features WHERE id = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sql_error)?;
            row.map(|(kinds, settings)| -> io::Result<FeaturesEntry> {
                Ok(FeaturesEntry {
                    v: 1,
                    kinds: serde_json::from_str(&kinds).map_err(json_error)?,
                    settings: serde_json::from_str(&settings).map_err(json_error)?,
                })
            })
            .transpose()
        })
    }

    /// The hash a `checksum mark` entry at `index` produced here.
    pub fn checksum_at(&self, index: u64) -> io::Result<Option<String>> {
        self.read(|conn| {
            conn.query_row(
                "SELECT hash FROM cluster_checksums WHERE idx = ?1",
                [index_sql(index)?],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)
        })
    }

    /// Whether a `checksum verify` found this replica different from the
    /// leader's. Cleared by a repair.
    pub fn is_diverged(&self) -> bool {
        self.inner.diverged.load(Ordering::SeqCst)
    }

    /// The leader time of the last applied entry.
    pub fn applied_time_ms(&self) -> io::Result<u64> {
        self.read(|conn| {
            conn.query_row(
                "SELECT time_ms FROM cluster_applied WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|time| u64::try_from(time).unwrap_or(0))
            .map_err(sql_error)
        })
    }

    /// The last applied log id and membership, as recorded in the database.
    pub fn applied(&self) -> io::Result<(Option<LogId>, StoredMembership)> {
        self.read(|conn| Ok((applied_log_id(conn)?, applied_membership(conn)?)))
    }

    /// A consistent copy of the database at `path` (which must not exist).
    pub fn backup_to(&self, path: &Path) -> io::Result<u64> {
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{}: already exists", path.display()),
            ));
        }
        let applied = {
            let conn = self.inner.db.lock();
            let conn = sqlite(&conn)?;
            let applied = applied_log_id(conn)?.map_or(0, |log_id| log_id.index);
            conn.backup(rusqlite::MAIN_DB, path, rusqlite::backup::NO_PROGRESS)
                .map_err(sql_error)?;
            applied
        };
        // A plain rollback-journal file, readable without `-wal` or `-shm`.
        let copy = Connection::open(path).map_err(sql_error)?;
        copy.execute_batch("PRAGMA journal_mode=DELETE;")
            .map_err(sql_error)?;
        Ok(applied)
    }

    /// Hash every table (by primary key) and every component's extra state.
    pub fn checksum_now(&self) -> io::Result<String> {
        let conn = self.inner.db.lock();
        self.state_hash(&conn)
    }

    /// Replace this replica with the database in `file`, built by a healthy
    /// peer at or after this replica's applied index: the repair after a
    /// detected divergence. Entries the file already contains are skipped
    /// when openraft applies them again.
    pub fn repair_from(&self, file: &Path) -> io::Result<u64> {
        let index = self.install_file(file, None)?;
        self.inner.diverged.store(false, Ordering::SeqCst);
        Ok(index)
    }

    fn apply_one(&self, entry: &LogEntry) -> io::Result<Reply> {
        let conn = self.inner.db.lock();
        let mut fires = Vec::new();
        let (reply, changes) = self.apply_entry(&conn, entry, &mut fires)?;
        // Committed: the instance database's hooks run while apply still
        // holds the writer, as after a local write.
        for fire in &fires {
            self.inner.db.fire(&conn, &fire.change());
        }
        if let Some(changes) = changes {
            if !changes.is_empty() {
                let sql = sqlite(&conn)?;
                for component in &self.inner.components.all {
                    component.after_apply(sql, &changes);
                }
            }
            let _ = self.inner.changes.send(changes);
        }
        drop(conn);
        self.inner.applied.send_if_modified(|applied| {
            if *applied < entry.log_id.index {
                *applied = entry.log_id.index;
                true
            } else {
                false
            }
        });
        Ok(reply)
    }

    /// Apply `entry` in one transaction on the writer `conn`. The instance
    /// database's hooks the entry calls for go to `fires`, for after commit.
    fn apply_entry(
        &self,
        conn: &Conn,
        entry: &LogEntry,
        fires: &mut Vec<Fire>,
    ) -> io::Result<(Reply, Option<Changes>)> {
        let index = entry.log_id.index;
        let sql = sqlite(conn)?;
        if applied_log_id(sql)?.is_some_and(|applied| applied.index >= index) {
            // Already in the database: a restart before openraft learned of
            // the apply, or a repair from a newer copy.
            return Ok((Reply::Ok(serde_json::Value::Null), None));
        }
        // The components of the instance database use `conn` inside this
        // transaction, so it is opened through the shared handle.
        let mut tx = sql.unchecked_transaction().map_err(sql_error)?;
        let previous_time: i64 = tx
            .query_row(
                "SELECT time_ms FROM cluster_applied WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        let previous_time = u64::try_from(previous_time).unwrap_or(0);
        let mut membership = None;
        let mut time = previous_time;
        let (reply, changes) = match &entry.payload {
            EntryPayload::Blank => (Reply::Ok(serde_json::Value::Null), None),
            EntryPayload::Membership(new) => {
                membership = Some(StoredMembership::new(Some(entry.log_id), new.clone()));
                let mut changes = Changes::new(index, None);
                changes.touch_table("cluster_membership");
                (Reply::Ok(serde_json::Value::Null), Some(changes))
            }
            EntryPayload::Normal(proposal) => {
                time = proposal.time_ms.max(previous_time);
                let mut ctx = ApplyContext {
                    index,
                    term: entry.log_id.leader_id.term,
                    time_ms: time,
                    origin: proposal.origin.clone(),
                    changes: Changes::new(index, Some(proposal.entry.kind())),
                };
                let stored = match &proposal.origin {
                    Some(origin) => stored_reply(&tx, origin)?,
                    None => None,
                };
                let reply = match stored {
                    Some(reply) => {
                        ctx.changes = Changes::new(index, Some(proposal.entry.kind()));
                        reply
                    }
                    None => {
                        let reply = match proposal.term {
                            Some(term) if term != ctx.term => Reply::err(format!(
                                "decided in term {term}, appended in term {}; leadership changed",
                                ctx.term
                            )),
                            _ => {
                                let mut savepoint = tx.savepoint().map_err(sql_error)?;
                                let reply = self.dispatch(
                                    conn,
                                    &savepoint,
                                    &proposal.entry,
                                    &mut ctx,
                                    fires,
                                )?;
                                if reply.is_err() {
                                    savepoint.rollback().map_err(sql_error)?;
                                    ctx.changes = Changes::new(index, Some(proposal.entry.kind()));
                                    fires.clear();
                                }
                                savepoint.commit().map_err(sql_error)?;
                                reply
                            }
                        };
                        if let Some(origin) = &proposal.origin {
                            store_reply(&tx, origin, &reply)?;
                        }
                        reply
                    }
                };
                if let (Entry::Operation(_), Some(origin)) = (&proposal.entry, &proposal.origin) {
                    // Applied or refused, it is no longer in flight.
                    self.inner.pending.lock().unwrap().remove(origin);
                }
                (reply, Some(ctx.changes))
            }
        };
        let membership_json = match membership {
            Some(membership) => serde_json::to_string(&membership).map_err(json_error)?,
            None => tx
                .query_row(
                    "SELECT membership FROM cluster_applied WHERE id = 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map_err(sql_error)?,
        };
        let log_id = serde_json::to_string(&entry.log_id).map_err(json_error)?;
        tx.execute(
            "UPDATE cluster_applied SET log_id = ?1, membership = ?2, time_ms = ?3 WHERE id = 1",
            params![
                log_id,
                membership_json,
                i64::try_from(time).unwrap_or(i64::MAX)
            ],
        )
        .map_err(sql_error)?;
        tx.commit().map_err(sql_error)?;
        Ok((reply, changes))
    }

    /// Apply one proposal's entry inside its savepoint. `conn` is the
    /// writer, `db` the same connection inside the savepoint.
    fn dispatch(
        &self,
        conn: &Conn,
        db: &Connection,
        entry: &Entry,
        ctx: &mut ApplyContext,
        fires: &mut Vec<Fire>,
    ) -> io::Result<Reply> {
        match entry {
            Entry::Operation(payload) => {
                let (reply, conflict) = replicated::apply_operation(conn, db, payload, ctx, fires)?;
                if conflict && !self.inner.diverged.swap(true, Ordering::SeqCst) {
                    log_error!(
                        "cluster: entry {} does not apply to this replica, which diverged; it stops taking logins and repairs from the leader",
                        ctx.index
                    );
                }
                Ok(reply)
            }
            Entry::Kv(payload) => replicated::apply_kv(conn, payload, ctx, fires),
            Entry::Directory(payload) => replicated::apply_directory(conn, payload, ctx, fires),
            Entry::Migrate(payload) => replicated::apply_migrate(conn, payload, ctx, fires),
            Entry::Peer(peer) => {
                let reply = apply_peer(db, peer, ctx)?;
                if !reply.is_err() {
                    for component in &self.inner.components.all {
                        component.peer_applied(db, peer, ctx)?;
                    }
                }
                Ok(reply)
            }
            Entry::Features(features) => {
                db.execute(
                    "INSERT INTO cluster_features (id, kinds, settings) VALUES (1, ?1, ?2)
                     ON CONFLICT (id) DO UPDATE SET kinds = excluded.kinds, settings = excluded.settings",
                    params![
                        serde_json::to_string(&features.kinds).map_err(json_error)?,
                        serde_json::to_string(&features.settings).map_err(json_error)?
                    ],
                )
                .map_err(sql_error)?;
                ctx.changes.touch_table("cluster_features");
                Ok(Reply::Ok(serde_json::Value::Null))
            }
            Entry::Checksum(ChecksumEntry::Mark { .. }) => {
                let hash = self.state_hash(conn)?;
                db.execute(
                    "INSERT OR REPLACE INTO cluster_checksums (idx, hash) VALUES (?1, ?2)",
                    params![index_sql(ctx.index)?, hash],
                )
                .map_err(sql_error)?;
                db.execute(
                    "DELETE FROM cluster_checksums WHERE idx NOT IN
                     (SELECT idx FROM cluster_checksums ORDER BY idx DESC LIMIT ?1)",
                    [KEPT_CHECKSUMS],
                )
                .map_err(sql_error)?;
                // The hash stays out of the result: the result is stored for
                // retries and would differ between replicas that diverged.
                Ok(Reply::Ok(serde_json::Value::Null))
            }
            Entry::Checksum(ChecksumEntry::Verify { index, hash, .. }) => {
                let local: Option<String> = db
                    .query_row(
                        "SELECT hash FROM cluster_checksums WHERE idx = ?1",
                        [index_sql(*index)?],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql_error)?;
                if let Some(local) = local
                    && local != *hash
                {
                    // Local only: the replicated state must not depend on it.
                    let first = !self.inner.diverged.swap(true, Ordering::SeqCst);
                    if first {
                        log_error!(
                            "cluster: this replica diverged at entry {index}: hash {local}, leader's {hash}; it stops taking logins and repairs from the leader"
                        );
                    }
                }
                Ok(Reply::Ok(serde_json::Value::Null))
            }
            other => match self.inner.components.by_kind.get(&other.kind()) {
                Some(component) => component.apply(db, other, ctx),
                None => Ok(Reply::err(format!(
                    "no component registered for {} entries",
                    other.kind()
                ))),
            },
        }
    }

    /// Every table by primary key ([`changeset::checksum_except`]), then
    /// every component's extra state.
    fn state_hash(&self, conn: &Conn) -> io::Result<String> {
        let tables = changeset::checksum_except(conn, UNCHECKED_TABLES)
            .map_err(|error| io::Error::other(format!("cluster checksum: {error}")))?;
        let mut context = digest::Context::new(&digest::SHA256);
        context.update(&tables);
        let db = sqlite(conn)?;
        for component in &self.inner.components.all {
            component.checksum(db, &mut context)?;
        }
        Ok(super::hex(context.finish().as_ref()))
    }

    /// Check `file`, write it into the live database, reopen and reload.
    /// With `meta`, the file must be the snapshot it describes; without, it
    /// must not be older than this replica. Returns the applied index.
    fn install_file(&self, file: &Path, meta: Option<&SnapshotMeta>) -> io::Result<u64> {
        let incoming = inspect(file)?;
        if let Some(meta) = meta
            && incoming != meta.last_log_id
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "snapshot {}: holds log id {incoming:?}, labelled {:?}",
                    file.display(),
                    meta.last_log_id
                ),
            ));
        }
        let index = incoming.map_or(0, |log_id| log_id.index);
        // Holding the writer pauses apply and leader execution until the
        // new state is in place and every mirror reloaded.
        let mut conn = self.inner.db.lock();
        let current = applied_log_id(sqlite(&conn)?)?.map_or(0, |log_id| log_id.index);
        if meta.is_none() && index < current {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("repair copy is at entry {index}, this replica at {current}"),
            ));
        }
        self.inner.installing.store(true, Ordering::SeqCst);
        let installed = (|| -> io::Result<()> {
            // Into the live file through the writer: nothing is renamed or
            // replaced, so open read connections (and Windows) do not mind.
            conn.sqlite_mut()
                .ok_or_else(|| super::invalid("the cluster state needs a SQLite database"))?
                .restore(rusqlite::MAIN_DB, file, rusqlite::backup::NO_PROGRESS)
                .map_err(sql_error)?;
            // Fresh connections: no statement cached against the old state.
            self.inner.db.reopen(&mut conn)?;
            prepare(sqlite(&conn)?)?;
            // Whatever this peer had in flight belongs to the old state.
            self.inner.pending.lock().unwrap().clear();
            let sql = sqlite(&conn)?;
            for component in &self.inner.components.all {
                component.full_reload(sql).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("component {} reload: {error}", component.name()),
                    )
                })?;
            }
            self.inner.db.reload_all_on(&conn);
            Ok(())
        })();
        self.inner.installing.store(false, Ordering::SeqCst);
        installed?;
        drop(conn);
        self.inner.applied.send_replace(index);
        let mut changes = Changes::new(index, None);
        for table in ["cluster_peers", "cluster_features", "cluster_membership"] {
            changes.touch_table(table);
        }
        let _ = self.inner.changes.send(changes);
        Ok(index)
    }

    fn current_snapshot(&self) -> io::Result<Option<SnapshotOf<TypeConfig, SnapshotFile>>> {
        let pointer = self.inner.snapshots.join("current.json");
        let text = match fs::read(pointer) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let current: CurrentSnapshot = serde_json::from_slice(&text).map_err(json_error)?;
        let path = self.inner.snapshots.join(current.file.as_str());
        if !path.is_file() {
            return Ok(None);
        }
        Ok(Some(openraft::Snapshot {
            meta: current.meta,
            snapshot: SnapshotFile { path },
        }))
    }

    /// Make `path` (inside the snapshot directory) the current snapshot and
    /// remove the others.
    fn keep_snapshot(&self, meta: &SnapshotMeta, path: &Path) -> io::Result<()> {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| super::invalid("snapshot file name"))?
            .to_owned();
        let pointer = serde_json::to_vec(&CurrentSnapshot {
            meta: meta.clone(),
            file: file.clone(),
        })
        .map_err(json_error)?;
        let temporary = self.inner.snapshots.join("current.json.tmp");
        fs::write(&temporary, pointer)?;
        sync_file(&temporary)?;
        fs::rename(temporary, self.inner.snapshots.join("current.json"))?;
        for old in fs::read_dir(&self.inner.snapshots)? {
            let old = old?;
            let name = old.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".sqlite3") && name != file.as_str() {
                let _ = fs::remove_file(old.path());
            }
        }
        Ok(())
    }

    /// Where an incoming snapshot is written while it arrives.
    pub fn incoming_path(&self, id: &str) -> PathBuf {
        self.inner.snapshots.join(format!("incoming-{id}.part"))
    }

    /// Build a snapshot file of the current state now. It also becomes
    /// this peer's current snapshot.
    pub(crate) fn build_snapshot_file(&self) -> io::Result<SnapshotOf<TypeConfig, SnapshotFile>> {
        let guard = self.inner.db.lock();
        let conn = sqlite(&guard)?;
        let last_log_id = applied_log_id(conn)?;
        let last_membership = applied_membership(conn)?;
        let name = match last_log_id {
            Some(log_id) => format!(
                "{}-{}-{}.sqlite3",
                log_id.leader_id.term, log_id.leader_id.node_id, log_id.index
            ),
            None => "0-0-0.sqlite3".to_owned(),
        };
        let path = self.inner.snapshots.join(&name);
        let temporary = self.inner.snapshots.join(format!("{name}.tmp"));
        let _ = fs::remove_file(&temporary);
        conn.backup(rusqlite::MAIN_DB, &temporary, rusqlite::backup::NO_PROGRESS)
            .map_err(sql_error)?;
        drop(guard);
        {
            // A rollback-journal file: opening it never creates `-wal` and
            // `-shm` files beside it, which would keep it busy on Windows.
            let copy = Connection::open(&temporary).map_err(sql_error)?;
            copy.execute_batch("PRAGMA journal_mode=DELETE;")
                .map_err(sql_error)?;
        }
        sync_file(&temporary)?;
        if path.is_file() {
            // The same log id holds the same state; the file may be open
            // for a transfer, and Windows refuses to replace an open file.
            fs::remove_file(&temporary)?;
        } else {
            fs::rename(temporary, &path)?;
        }
        let meta = SnapshotMeta {
            last_log_id,
            last_membership,
        };
        self.keep_snapshot(&meta, &path)?;
        Ok(openraft::Snapshot {
            meta,
            snapshot: SnapshotFile { path },
        })
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotData = SnapshotFile;
    type SnapshotBuilder = StateMachine;

    async fn applied_state(&mut self) -> Result<(Option<LogId>, StoredMembership), io::Error> {
        self.applied()
    }

    async fn apply<Strm>(&mut self, mut entries: Strm) -> Result<(), io::Error>
    where
        Strm: futures_util::Stream<Item = Result<EntryResponder<TypeConfig>, io::Error>>
            + Unpin
            + OptionalSend,
    {
        while let Some(item) = entries.next().await {
            let (entry, responder) = item?;
            let reply = self.apply_one(&entry)?;
            if let Some(responder) = responder {
                responder.send(reply);
            }
        }
        Ok(())
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta,
        snapshot: SnapshotFile,
    ) -> Result<(), io::Error> {
        self.install_file(&snapshot.path, Some(meta))?;
        let name = match meta.last_log_id {
            Some(log_id) => format!(
                "{}-{}-{}.sqlite3",
                log_id.leader_id.term, log_id.leader_id.node_id, log_id.index
            ),
            None => "0-0-0.sqlite3".to_owned(),
        };
        let kept = self.inner.snapshots.join(name);
        if snapshot.path != kept {
            if kept.is_file() {
                // Already here under its name (and maybe open for a
                // transfer, which Windows would not let a rename replace).
                fs::remove_file(snapshot.path.as_path())?;
            } else {
                fs::rename(snapshot.path.as_path(), &kept)?;
            }
        }
        self.keep_snapshot(meta, &kept)
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<SnapshotOf<TypeConfig, SnapshotFile>>, io::Error> {
        self.current_snapshot()
    }
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachine {
    type SnapshotData = SnapshotFile;

    async fn build_snapshot(&mut self) -> Result<SnapshotOf<TypeConfig, SnapshotFile>, io::Error> {
        self.build_snapshot_file()
    }
}

/// `rift cluster init --from-backup`: make `backup` (a `cluster backup`
/// copy) the state of a new cluster at `state_file`. The data stays; what
/// belonged to the old cluster (peers, applied position, retries, features,
/// checksums, Raft ids) goes.
pub fn prepare_from_backup(state_file: &Path, backup: &Path) -> io::Result<()> {
    if state_file.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{}: already exists; move it away to start a cluster from a backup",
                state_file.display()
            ),
        ));
    }
    inspect(backup)?;
    let mut conn = Connection::open(state_file)
        .map_err(|error| io::Error::other(format!("{}: {error}", state_file.display())))?;
    conn.restore(rusqlite::MAIN_DB, backup, rusqlite::backup::NO_PROGRESS)
        .map_err(sql_error)?;
    prepare(&conn)?;
    let membership = serde_json::to_string(&StoredMembership::default()).map_err(json_error)?;
    conn.execute_batch(
        "DELETE FROM cluster_peers;
         DELETE FROM cluster_proposals;
         DELETE FROM cluster_features;
         DELETE FROM cluster_checksums;
         DELETE FROM cluster_meta WHERE key = 'next_raft_id';",
    )
    .map_err(sql_error)?;
    conn.execute(
        "UPDATE cluster_applied SET log_id = NULL, membership = ?1, time_ms = 0 WHERE id = 1",
        [membership],
    )
    .map_err(sql_error)?;
    Ok(())
}

/// Whether the state file at `path` already applied entries of a cluster.
pub fn has_applied(path: &Path) -> io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| io::Error::other(format!("{}: {error}", path.display())))?;
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'cluster_applied'",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    Ok(exists && applied_log_id(&conn)?.is_some())
}

/// Create the framework tables and the applied row.
fn prepare(conn: &Connection) -> io::Result<()> {
    conn.execute_batch(SCHEMA).map_err(sql_error)?;
    let membership = serde_json::to_string(&StoredMembership::default()).map_err(json_error)?;
    conn.execute(
        "INSERT OR IGNORE INTO cluster_applied (id, log_id, membership, time_ms) VALUES (1, NULL, ?1, 0)",
        [membership],
    )
    .map_err(sql_error)?;
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM cluster_meta WHERE key = 'schema'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    match version.as_deref().map(str::parse::<i64>) {
        None => {
            conn.execute(
                "INSERT INTO cluster_meta (key, value) VALUES ('schema', ?1)",
                [SCHEMA_VERSION.to_string()],
            )
            .map_err(sql_error)?;
        }
        Some(Ok(version)) if version <= SCHEMA_VERSION => {}
        Some(_) => {
            return Err(super::invalid(format!(
                "the cluster state was written by a newer Rift (schema {}); upgrade this peer",
                version.as_deref().unwrap_or("?")
            )));
        }
    }
    Ok(())
}

/// Integrity and schema of a database file; returns its applied log id.
fn inspect(file: &Path) -> io::Result<Option<LogId>> {
    let conn = Connection::open_with_flags(file, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| io::Error::other(format!("{}: {error}", file.display())))?;
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(sql_error)?;
    if check != "ok" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: integrity check failed: {check}", file.display()),
        ));
    }
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM cluster_meta WHERE key = 'schema'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: not a cluster state file: {error}", file.display()),
            )
        })?;
    if version
        .as_deref()
        .and_then(|version| version.parse::<i64>().ok())
        .is_none_or(|version| version > SCHEMA_VERSION)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: schema {version:?} is not one this Rift reads",
                file.display()
            ),
        ));
    }
    applied_log_id(&conn)
}

pub(super) fn applied_log_id(conn: &Connection) -> io::Result<Option<LogId>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT log_id FROM cluster_applied WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?
        .flatten();
    text.map(|text| serde_json::from_str(&text).map_err(json_error))
        .transpose()
}

fn applied_membership(conn: &Connection) -> io::Result<StoredMembership> {
    let text: String = conn
        .query_row(
            "SELECT membership FROM cluster_applied WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    serde_json::from_str(&text).map_err(json_error)
}

pub(super) fn stored_reply(db: &Connection, origin: &Origin) -> io::Result<Option<Reply>> {
    let text: Option<String> = db
        .query_row(
            "SELECT reply FROM cluster_proposals WHERE peer = ?1 AND incarnation = ?2 AND serial = ?3",
            params![origin.peer, index_sql(origin.incarnation)?, index_sql(origin.serial)?],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    text.map(|text| serde_json::from_str(&text).map_err(json_error))
        .transpose()
}

pub(super) fn store_reply(db: &Connection, origin: &Origin, reply: &Reply) -> io::Result<()> {
    let serial = index_sql(origin.serial)?;
    let incarnation = index_sql(origin.incarnation)?;
    db.execute(
        "INSERT OR REPLACE INTO cluster_proposals (peer, incarnation, serial, reply) VALUES (?1, ?2, ?3, ?4)",
        params![
            origin.peer,
            incarnation,
            serial,
            serde_json::to_string(reply).map_err(json_error)?
        ],
    )
    .map_err(sql_error)?;
    db.execute(
        "DELETE FROM cluster_proposals WHERE peer = ?1 AND incarnation = ?2 AND serial <= ?3",
        params![origin.peer, incarnation, serial - KEPT_SERIALS],
    )
    .map_err(sql_error)?;
    Ok(())
}

fn apply_peer(db: &Connection, entry: &PeerEntry, ctx: &mut ApplyContext) -> io::Result<Reply> {
    let time = i64::try_from(ctx.time_ms).unwrap_or(i64::MAX);
    match entry {
        PeerEntry::Added {
            name,
            address,
            witness,
            ..
        } => {
            let existing: Option<i64> = db
                .query_row(
                    "SELECT raft_id FROM cluster_peers WHERE name = ?1",
                    [name],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            if let Some(raft_id) = existing {
                return Ok(Reply::err(format!(
                    "peer {name} already exists with raft id {raft_id}; remove or replace it first"
                )));
            }
            let next: i64 = db
                .query_row(
                    "SELECT value FROM cluster_meta WHERE key = 'next_raft_id'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(sql_error)?
                .and_then(|value| value.parse().ok())
                .unwrap_or(1);
            // Connection ids carry the peer's id in 8 bits
            // (`crate::players::identity`), and ids are never reused.
            let max = i64::from(crate::players::identity::MAX_PEER);
            if next > max {
                return Ok(Reply::err(format!(
                    "cannot add peer {name}: raft ids are never reused and at most {max} can be assigned in one cluster; start a new cluster from a backup (`rift cluster init --from-backup`) to assign ids from 1 again"
                )));
            }
            db.execute(
                "INSERT INTO cluster_peers (name, raft_id, incarnation, address, public_address, labels, witness, version, kinds, expired, since_ms)
                 VALUES (?1, ?2, 0, ?3, NULL, '{}', ?4, '', '[]', 0, ?5)",
                params![name, next, address, i64::from(*witness), time],
            )
            .map_err(sql_error)?;
            db.execute(
                "INSERT INTO cluster_meta (key, value) VALUES ('next_raft_id', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [(next + 1).to_string()],
            )
            .map_err(sql_error)?;
            ctx.changes.touch("cluster_peers", name.clone());
            Ok(Reply::ok(serde_json::json!({ "raft_id": next })))
        }
        PeerEntry::Joined {
            name,
            raft_id,
            incarnation,
            address,
            public_address,
            labels,
            witness,
            version,
            kinds,
            ..
        } => {
            let existing: Option<(i64, i64, i64)> = db
                .query_row(
                    "SELECT raft_id, incarnation, expired FROM cluster_peers WHERE name = ?1",
                    [name],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(sql_error)?;
            let Some((stored_id, stored_incarnation, stored_expired)) = existing else {
                return Ok(Reply::err(format!("peer {name} is not a member")));
            };
            if u64::try_from(stored_id).ok() != Some(*raft_id) {
                return Ok(Reply::err(format!(
                    "peer {name} has raft id {stored_id}, not {raft_id}"
                )));
            }
            let incarnation_sql = index_sql(*incarnation)?;
            // A peer the leader expired joins again with the incarnation it
            // still runs as, once it hears from the cluster again.
            let rejoins = incarnation_sql == stored_incarnation && stored_expired != 0;
            if incarnation_sql <= stored_incarnation && !rejoins {
                return Ok(Reply::err(format!(
                    "peer {name}: incarnation {incarnation} is not newer than {stored_incarnation}"
                )));
            }
            db.execute(
                "UPDATE cluster_peers SET incarnation = ?2, address = ?3, public_address = ?4, labels = ?5,
                 witness = ?6, version = ?7, kinds = ?8, expired = 0, since_ms = ?9 WHERE name = ?1",
                params![
                    name,
                    incarnation_sql,
                    address,
                    public_address,
                    serde_json::to_string(labels).map_err(json_error)?,
                    i64::from(*witness),
                    version,
                    serde_json::to_string(kinds).map_err(json_error)?,
                    time
                ],
            )
            .map_err(sql_error)?;
            db.execute(
                "DELETE FROM cluster_proposals WHERE peer = ?1 AND incarnation < ?2",
                params![name, incarnation_sql],
            )
            .map_err(sql_error)?;
            ctx.changes.touch("cluster_peers", name.clone());
            Ok(Reply::ok(serde_json::json!({ "incarnation": incarnation })))
        }
        PeerEntry::Expired {
            name, incarnation, ..
        } => {
            let changed = db
                .execute(
                    "UPDATE cluster_peers SET expired = 1 WHERE name = ?1 AND incarnation = ?2 AND expired = 0",
                    params![name, index_sql(*incarnation)?],
                )
                .map_err(sql_error)?;
            if changed > 0 {
                ctx.changes.touch("cluster_peers", name.clone());
            }
            Ok(Reply::ok(changed > 0))
        }
        PeerEntry::Removed { name, .. } => {
            let changed = db
                .execute("DELETE FROM cluster_peers WHERE name = ?1", [name])
                .map_err(sql_error)?;
            db.execute("DELETE FROM cluster_proposals WHERE peer = ?1", [name])
                .map_err(sql_error)?;
            if changed > 0 {
                ctx.changes.touch("cluster_peers", name.clone());
            }
            Ok(Reply::ok(changed > 0))
        }
    }
}

fn peer_rows(conn: &Connection) -> io::Result<Vec<PeerRow>> {
    let mut statement = conn
        .prepare(
            "SELECT name, raft_id, incarnation, address, public_address, labels, witness, version, kinds, expired, since_ms
             FROM cluster_peers ORDER BY name",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })
        .map_err(sql_error)?;
    let mut peers = Vec::new();
    for row in rows {
        let (
            name,
            raft_id,
            incarnation,
            address,
            public_address,
            labels,
            witness,
            version,
            kinds,
            expired,
            since,
        ) = row.map_err(sql_error)?;
        peers.push(PeerRow {
            name,
            raft_id: u64::try_from(raft_id).unwrap_or(0),
            incarnation: u64::try_from(incarnation).unwrap_or(0),
            address,
            public_address,
            labels: serde_json::from_str(&labels).map_err(json_error)?,
            witness: witness != 0,
            version,
            kinds: serde_json::from_str(&kinds).map_err(json_error)?,
            expired: expired != 0,
            since_ms: u64::try_from(since).unwrap_or(0),
        });
    }
    Ok(peers)
}

fn index_sql(value: u64) -> io::Result<i64> {
    i64::try_from(value).map_err(|_| super::invalid("value out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_encode_with_kind_and_version() {
        let entry = Entry::Peer(PeerEntry::Expired {
            v: 1,
            name: "eu-2".into(),
            incarnation: 3,
        });
        let text = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            text,
            r#"{"kind":"peer","body":{"expired":{"v":1,"name":"eu-2","incarnation":3}}}"#
        );
        assert_eq!(serde_json::from_str::<Entry>(&text).unwrap(), entry);
        let kv = Entry::Kv(Payload::new(2, serde_json::json!({"owner": "x"})));
        let text = serde_json::to_string(&kv).unwrap();
        assert_eq!(text, r#"{"kind":"kv","body":{"v":2,"data":{"owner":"x"}}}"#);
        assert_eq!(
            serde_json::from_str::<Entry>(&text).unwrap().kind(),
            EntryKind::Kv
        );
        for kind in EntryKind::ALL {
            assert_eq!(EntryKind::parse(kind.name()), Some(kind));
        }
    }

    struct Claims;

    impl Component for Claims {
        fn name(&self) -> &'static str {
            "claims"
        }

        fn kinds(&self) -> Vec<EntryKind> {
            vec![EntryKind::Claim]
        }
    }

    #[test]
    fn components_cannot_take_built_in_or_taken_kinds() {
        struct Peers;
        impl Component for Peers {
            fn name(&self) -> &'static str {
                "peers"
            }
            fn kinds(&self) -> Vec<EntryKind> {
                vec![EntryKind::Peer]
            }
        }
        let mut components = Components::new();
        components.register(Arc::new(Claims)).unwrap();
        assert!(components.register(Arc::new(Claims)).is_err());
        assert!(components.register(Arc::new(Peers)).is_err());
        assert!(components.supported().contains(&EntryKind::Claim));
        assert!(components.supported().contains(&EntryKind::Checksum));
        // The instance database's kinds are built in.
        assert!(components.supported().contains(&EntryKind::Kv));
        assert!(!components.supported().contains(&EntryKind::Stream));
    }
}
