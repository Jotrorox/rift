//! Claims in the Raft log: the `claim` entry kind, its persistent table and
//! the [`Claims`] that proposes to it.
//!
//! A `claim` entry carries a batch of [`ClaimOp`]s and its result is one
//! `Result<(), Refusal>` per operation, in order. Applying runs the shared
//! [`ClaimTable`] semantics on an in-memory mirror and writes every change to
//! the `cluster_claims` table in the entry's transaction, so the claims
//! survive restarts, travel in snapshots and are part of every checksum.
//! The mirror is loaded from the table before the first apply after a
//! start and rebuilt after a snapshot install, which announces the
//! difference as claim changes.
//!
//! A claim of a login (name, player, reclaim) is refused when its holder is
//! not the current, unexpired incarnation of a member: an operation that a
//! peer proposed before the leader expired it, but that commits afterwards,
//! cannot claim anything. Applying `peer joined` releases the claims of the
//! peer's older incarnations, `peer expired` those of the expired
//! incarnation, and `peer removed` those of every peer that is no member
//! any more.
//!
//! [`RaftClaims`] group-commits: while one entry is in flight, the operations
//! proposed meanwhile are collected into the next one. Releases never wait:
//! they are queued, and kept for the next entry while none can commit.
use crate::{
    cluster::{
        Cluster, Component, Entry, EntryKind, Reply,
        state::{ApplyContext, Changes, Payload, PeerEntry},
    },
    players::{
        PlayerRegistry,
        claims::{
            ClaimChange, ClaimFuture, ClaimOp, ClaimRecord, ClaimTable, Claims, Holder, Refusal,
        },
    },
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

/// The table every peer keeps its claims in.
pub const TABLE: &str = "cluster_claims";
/// Operations in one `claim` entry at most.
const MAX_BATCH: usize = 256;
/// Releases kept for later while no entry commits, at most.
const MAX_RETRIED: usize = 100_000;
/// Pause after a batch could not be committed.
const RETRY: Duration = Duration::from_millis(250);

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS cluster_claims (
    key TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    uuid TEXT,
    peer INTEGER NOT NULL,
    incarnation INTEGER NOT NULL,
    connection INTEGER NOT NULL,
    token INTEGER NOT NULL
) STRICT";

/// The content of a `claim` entry (version 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimBatch {
    pub ops: Vec<ClaimOp>,
}

#[derive(Default)]
struct Mirror {
    loaded: bool,
    table: ClaimTable,
    /// Changes of the entry being applied, announced after it.
    pending: Vec<ClaimChange>,
    /// Players of a peer that just expired, whose servers are held back
    /// from idle stops. Resolved after apply, outside this lock: the
    /// registry consults the claims under its own lock.
    held: Vec<[u8; 16]>,
}

/// The claims of one peer's replica, shared by its [`ClaimComponent`] and
/// its [`RaftClaims`].
pub struct ClaimShared {
    registry: Weak<PlayerRegistry>,
    grace: Duration,
    mirror: Mutex<Mirror>,
    changes: broadcast::Sender<ClaimChange>,
    holds: Mutex<BTreeMap<String, Instant>>,
}

impl ClaimShared {
    pub fn new(registry: &Arc<PlayerRegistry>, grace: Duration) -> Self {
        Self {
            registry: Arc::downgrade(registry),
            grace,
            mirror: Mutex::default(),
            changes: broadcast::channel(4096).0,
            holds: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Mirror> {
        self.mirror.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Load the mirror from the replica if that has not happened yet.
    pub fn ensure_loaded(&self, db: &Connection) -> io::Result<()> {
        let mut mirror = self.lock();
        load_once(&mut mirror, db)
    }

    /// Every live claim of this replica.
    pub fn live(&self) -> Vec<ClaimRecord> {
        self.lock().table.live().cloned().collect()
    }

    /// Claim changes from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<ClaimChange> {
        self.changes.subscribe()
    }

    /// See [`super::Presence::stop_holds`].
    pub fn stop_holds(&self) -> BTreeMap<String, Instant> {
        let now = Instant::now();
        let mut holds = self.holds.lock().unwrap_or_else(PoisonError::into_inner);
        holds.retain(|_, until| *until > now);
        holds.clone()
    }

    /// Apply `op` to the mirror and the table; `ctx` learns what changed.
    fn apply_op(
        &self,
        mirror: &mut Mirror,
        db: &Connection,
        op: &ClaimOp,
        ctx: &mut ApplyContext,
        hold: bool,
    ) -> io::Result<Result<(), Refusal>> {
        let mut changes = Vec::new();
        let result = mirror.table.apply(op, Some(&mut changes));
        for change in &changes {
            match change {
                ClaimChange::Claimed(record) => {
                    db.execute(
                        "INSERT OR REPLACE INTO cluster_claims (key, name, uuid, peer, incarnation, connection, token)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            record.key,
                            record.name,
                            record.uuid.as_ref().map(hex),
                            i64::from(record.holder.peer),
                            to_sql(record.holder.incarnation),
                            to_sql(record.holder.connection),
                            to_sql(record.holder.token)
                        ],
                    )
                    .map_err(crate::cluster::sql_error)?;
                    ctx.changes.touch(TABLE, record.key.clone());
                }
                ClaimChange::Released(record) => {
                    db.execute("DELETE FROM cluster_claims WHERE key = ?1", [&record.key])
                        .map_err(crate::cluster::sql_error)?;
                    ctx.changes.touch(TABLE, record.key.clone());
                    if hold && let Some(uuid) = record.uuid {
                        mirror.held.push(uuid);
                    }
                }
            }
        }
        mirror.pending.extend(changes);
        Ok(result)
    }
}

/// Load the table into `mirror` unless it already holds it.
fn load_once(mirror: &mut Mirror, db: &Connection) -> io::Result<()> {
    if !mirror.loaded {
        mirror.table = load(db)?;
        mirror.loaded = true;
    }
    Ok(())
}

fn table_exists(db: &Connection) -> io::Result<bool> {
    db.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [TABLE],
        |row| row.get(0),
    )
    .map_err(crate::cluster::sql_error)
}

/// The claims stored in `db`; none before the first `claim` entry.
fn load(db: &Connection) -> io::Result<ClaimTable> {
    if !table_exists(db)? {
        return Ok(ClaimTable::default());
    }
    let mut statement = db
        .prepare(
            "SELECT key, name, uuid, peer, incarnation, connection, token FROM cluster_claims ORDER BY key",
        )
        .map_err(crate::cluster::sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .map_err(crate::cluster::sql_error)?;
    let mut records = Vec::new();
    for row in rows {
        let (key, name, uuid, peer, incarnation, connection, token) =
            row.map_err(crate::cluster::sql_error)?;
        let uuid = uuid
            .map(|text| {
                unhex_uuid(&text).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("cluster_claims: {key} has the malformed UUID {text:?}"),
                    )
                })
            })
            .transpose()?;
        records.push(ClaimRecord {
            key,
            name,
            uuid,
            holder: Holder {
                peer: u16::try_from(peer).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("cluster_claims: peer {peer} is out of range"),
                    )
                })?,
                incarnation: from_sql(incarnation),
                connection: from_sql(connection),
                token: from_sql(token),
            },
        });
    }
    Ok(ClaimTable::from_records(records))
}

/// Whether the holder of a login's claim is the current, unexpired
/// incarnation of a member; releases and expiries always apply.
fn admissible(db: &Connection, op: &ClaimOp) -> io::Result<Result<(), Refusal>> {
    let holder = match op {
        ClaimOp::Name { holder, .. }
        | ClaimOp::Player { holder, .. }
        | ClaimOp::Reclaim { holder, .. } => holder,
        ClaimOp::Release { .. } | ClaimOp::ExpirePeer { .. } => return Ok(Ok(())),
    };
    let row: Option<(i64, i64)> = db
        .query_row(
            "SELECT incarnation, expired FROM cluster_peers WHERE raft_id = ?1",
            [i64::from(holder.peer)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(crate::cluster::sql_error)?;
    Ok(match row {
        None => Err(Refusal::Unavailable(format!(
            "peer {} is not a member",
            holder.peer
        ))),
        Some((_, expired)) if expired != 0 => Err(Refusal::Fenced),
        Some((incarnation, _)) if incarnation != to_sql(holder.incarnation) => {
            let stored = from_sql(incarnation);
            Err(Refusal::Unavailable(format!(
                "incarnation {} of peer {} is not its current one: the peer table has incarnation {stored}{}",
                holder.incarnation,
                holder.peer,
                if stored < holder.incarnation {
                    " (its `peer joined` was not applied before this claim)"
                } else {
                    ""
                }
            )))
        }
        Some(_) => Ok(()),
    })
}

/// The `claim` entry kind and the claim releases of `peer` entries.
pub struct ClaimComponent {
    shared: Arc<ClaimShared>,
}

impl ClaimComponent {
    pub fn new(shared: Arc<ClaimShared>) -> Self {
        Self { shared }
    }
}

impl Component for ClaimComponent {
    fn name(&self) -> &'static str {
        "claims"
    }

    fn kinds(&self) -> Vec<EntryKind> {
        vec![EntryKind::Claim]
    }

    fn apply(&self, db: &Connection, entry: &Entry, ctx: &mut ApplyContext) -> io::Result<Reply> {
        let Some(payload) = entry.payload() else {
            return Ok(Reply::err("a claim entry without a payload"));
        };
        if payload.v != 1 {
            return Ok(Reply::err(format!(
                "claim entries of version {} are not supported here",
                payload.v
            )));
        }
        let batch = match ClaimBatch::deserialize(&payload.data) {
            Ok(batch) => batch,
            Err(error) => return Ok(Reply::err(format!("claim entry: {error}"))),
        };
        db.execute_batch(SCHEMA)
            .map_err(crate::cluster::sql_error)?;
        let mut mirror = self.shared.lock();
        load_once(&mut mirror, db)?;
        let mut results = Vec::with_capacity(batch.ops.len());
        for op in &batch.ops {
            let result = match admissible(db, op)? {
                Ok(()) => self.shared.apply_op(&mut mirror, db, op, ctx, false)?,
                Err(refusal) => Err(refusal),
            };
            results.push(result);
        }
        Ok(Reply::ok(results))
    }

    fn peer_applied(
        &self,
        db: &Connection,
        entry: &PeerEntry,
        ctx: &mut ApplyContext,
    ) -> io::Result<()> {
        // Before the first claim entry there is nothing to release, and the
        // table must not appear outside an entry that every peer applies.
        if !table_exists(db)? {
            return Ok(());
        }
        let mut mirror = self.shared.lock();
        load_once(&mut mirror, db)?;
        match entry {
            PeerEntry::Joined {
                raft_id,
                incarnation,
                ..
            } => {
                if let Ok(peer) = u16::try_from(*raft_id) {
                    let op = ClaimOp::ExpirePeer {
                        peer,
                        before: Some(*incarnation),
                    };
                    let _ = self.shared.apply_op(&mut mirror, db, &op, ctx, false)?;
                }
            }
            PeerEntry::Expired {
                name, incarnation, ..
            } => {
                let raft_id: Option<i64> = db
                    .query_row(
                        "SELECT raft_id FROM cluster_peers WHERE name = ?1",
                        [name],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(crate::cluster::sql_error)?;
                if let Some(peer) = raft_id.and_then(|id| u16::try_from(id).ok()) {
                    let op = ClaimOp::ExpirePeer {
                        peer,
                        before: Some(incarnation.saturating_add(1)),
                    };
                    let _ = self.shared.apply_op(&mut mirror, db, &op, ctx, true)?;
                }
            }
            PeerEntry::Removed { .. } => {
                let members: BTreeSet<i64> = {
                    let mut statement = db
                        .prepare("SELECT raft_id FROM cluster_peers")
                        .map_err(crate::cluster::sql_error)?;
                    let ids = statement
                        .query_map([], |row| row.get::<_, i64>(0))
                        .map_err(crate::cluster::sql_error)?;
                    let mut members = BTreeSet::new();
                    for id in ids {
                        members.insert(id.map_err(crate::cluster::sql_error)?);
                    }
                    members
                };
                let orphans: BTreeSet<u16> = mirror
                    .table
                    .live()
                    .map(|record| record.holder.peer)
                    .filter(|peer| !members.contains(&i64::from(*peer)))
                    .collect();
                for peer in orphans {
                    let op = ClaimOp::ExpirePeer { peer, before: None };
                    let _ = self.shared.apply_op(&mut mirror, db, &op, ctx, true)?;
                }
            }
            PeerEntry::Added { .. } => {}
        }
        Ok(())
    }

    fn after_apply(&self, _db: &Connection, _changes: &Changes) {
        let (pending, held) = {
            let mut mirror = self.shared.lock();
            if mirror.pending.is_empty() && mirror.held.is_empty() {
                return;
            }
            (
                std::mem::take(&mut mirror.pending),
                std::mem::take(&mut mirror.held),
            )
        };
        // Before the releases are announced: until then the roster still
        // knows where the expired peer's players were.
        if !held.is_empty()
            && let Some(registry) = self.shared.registry.upgrade()
        {
            let servers: Vec<String> = held
                .iter()
                .filter_map(|uuid| registry.get(uuid))
                .map(|player| player.server)
                .collect();
            let until = Instant::now() + self.shared.grace;
            let mut holds = self
                .shared
                .holds
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for server in servers {
                holds.insert(server, until);
            }
        }
        for change in pending {
            let _ = self.shared.changes.send(change);
        }
    }

    fn full_reload(&self, db: &Connection) -> io::Result<()> {
        let fresh = load(db)?;
        let changes = {
            let mut mirror = self.shared.lock();
            let old: BTreeMap<String, ClaimRecord> = mirror
                .table
                .live()
                .map(|record| (record.key.clone(), record.clone()))
                .collect();
            let new: BTreeMap<String, ClaimRecord> = fresh
                .live()
                .map(|record| (record.key.clone(), record.clone()))
                .collect();
            let mut changes: Vec<ClaimChange> = old
                .iter()
                .filter(|(key, record)| new.get(*key) != Some(*record))
                .map(|(_, record)| ClaimChange::Released(record.clone()))
                .collect();
            changes.extend(
                new.iter()
                    .filter(|(key, record)| old.get(*key) != Some(*record))
                    .map(|(_, record)| ClaimChange::Claimed(record.clone())),
            );
            mirror.table = fresh;
            mirror.loaded = true;
            mirror.pending.clear();
            mirror.held.clear();
            changes
        };
        for change in changes {
            let _ = self.shared.changes.send(change);
        }
        Ok(())
    }
}

/// One operation waiting for the next `claim` entry; releases have no
/// reply.
struct Queued {
    op: ClaimOp,
    reply: Option<oneshot::Sender<Result<(), Refusal>>>,
}

/// [`Claims`] decided by the Raft log: each operation is applied once its
/// entry committed and was applied on this peer.
pub struct RaftClaims {
    shared: Arc<ClaimShared>,
    queue: mpsc::UnboundedSender<Queued>,
}

impl RaftClaims {
    /// The claims of `cluster` and the task that proposes them; abort the
    /// task to stop proposing.
    pub fn new(cluster: Cluster, shared: Arc<ClaimShared>) -> (Self, JoinHandle<()>) {
        let (queue, receiver) = mpsc::unbounded_channel();
        let task = tokio::spawn(propose_batches(cluster, receiver));
        (Self { shared, queue }, task)
    }
}

fn stopping() -> Refusal {
    Refusal::Unavailable("this peer stopped proposing claims".into())
}

impl Claims for RaftClaims {
    fn propose(&self, op: ClaimOp) -> ClaimFuture<Result<(), Refusal>> {
        let (reply, answer) = oneshot::channel();
        if self
            .queue
            .send(Queued {
                op,
                reply: Some(reply),
            })
            .is_err()
        {
            return Box::pin(std::future::ready(Err(stopping())));
        }
        Box::pin(async move { answer.await.unwrap_or_else(|_| Err(stopping())) })
    }

    fn release(&self, key: String, holder: Holder) {
        let _ = self.queue.send(Queued {
            op: ClaimOp::Release { key, holder },
            reply: None,
        });
    }

    fn live(&self) -> Vec<ClaimRecord> {
        self.shared.live()
    }

    fn subscribe(&self) -> broadcast::Receiver<ClaimChange> {
        self.shared.subscribe()
    }

    fn networked(&self) -> bool {
        true
    }
}

/// The release that undoes a granted claim whose caller went away.
fn undo(op: &ClaimOp) -> Option<ClaimOp> {
    match op {
        ClaimOp::Name { key, holder, .. }
        | ClaimOp::Player { key, holder, .. }
        | ClaimOp::Reclaim { key, holder, .. } => Some(ClaimOp::Release {
            key: key.clone(),
            holder: *holder,
        }),
        ClaimOp::Release { .. } | ClaimOp::ExpirePeer { .. } => None,
    }
}

/// Propose `ops` as one entry; the result of each, or why the entry did not
/// commit.
async fn commit(cluster: &Cluster, ops: Vec<ClaimOp>) -> Result<Vec<Result<(), Refusal>>, String> {
    let count = ops.len();
    let data = serde_json::to_value(ClaimBatch { ops }).map_err(|error| error.to_string())?;
    let results = match cluster.propose(Entry::Claim(Payload::new(1, data))).await {
        Ok(Reply::Ok(value)) => serde_json::from_value::<Vec<Result<(), Refusal>>>(value)
            .map_err(|error| format!("claim results: {error}"))?,
        Ok(Reply::Err(reason)) => return Err(reason),
        Err(error) => return Err(error.to_string()),
    };
    if results.len() != count {
        return Err(format!(
            "a claim entry of {count} operations returned {} results",
            results.len()
        ));
    }
    Ok(results)
}

/// Group commit: one entry in flight at a time, the operations queued
/// meanwhile go into the next one.
async fn propose_batches(cluster: Cluster, mut queue: mpsc::UnboundedReceiver<Queued>) {
    let mut retried: Vec<Queued> = Vec::new();
    loop {
        let mut batch = std::mem::take(&mut retried);
        if batch.is_empty() {
            match queue.recv().await {
                Some(queued) => batch.push(queued),
                None => return,
            }
        }
        while batch.len() < MAX_BATCH {
            match queue.try_recv() {
                Ok(queued) => batch.push(queued),
                Err(_) => break,
            }
        }
        let ops: Vec<ClaimOp> = batch.iter().map(|queued| queued.op.clone()).collect();
        match commit(&cluster, ops).await {
            Ok(results) => {
                for (Queued { op, reply }, result) in batch.into_iter().zip(results) {
                    let Some(reply) = reply else { continue };
                    if let Err(result) = reply.send(result)
                        && result.is_ok()
                        && let Some(release) = undo(&op)
                        && retried.len() < MAX_RETRIED
                    {
                        // Granted, but nobody waits for it any more.
                        retried.push(Queued {
                            op: release,
                            reply: None,
                        });
                    }
                }
            }
            Err(reason) => {
                log_debug!("cluster: claims not committed: {reason}");
                for Queued { op, reply } in batch {
                    match reply {
                        Some(reply) => {
                            let _ = reply.send(Err(Refusal::Unavailable(reason.clone())));
                        }
                        None if retried.len() < MAX_RETRIED => {
                            retried.push(Queued { op, reply: None });
                        }
                        None => {}
                    }
                }
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

fn to_sql(value: u64) -> i64 {
    // Bit for bit: SQLite integers are signed.
    value as i64
}

fn from_sql(value: i64) -> u64 {
    value as u64
}

fn hex(uuid: &[u8; 16]) -> String {
    crate::cluster::hex(uuid)
}

fn unhex_uuid(text: &str) -> Option<[u8; 16]> {
    crate::cluster::unhex(text)?.try_into().ok()
}
