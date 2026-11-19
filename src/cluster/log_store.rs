//! The Raft log and vote in `rift.raft.sqlite3`.
//!
//! WAL with `synchronous=FULL`: an entry or vote acknowledged to the leader
//! must survive a crash, or this peer could vote twice in one term or lose a
//! committed entry. The file also holds what makes this peer itself: its
//! cluster id, its Raft id and name (assigned when it was added), and the
//! incarnation counter, raised at every start.
//!
//! A peer whose Raft file is lost must not come back under its old Raft id.
//! The handshake refuses a peer that presents no Raft id under a name that is
//! still a member, and `rift admin cluster replace <name>` re-adds it under a
//! new id (see [`super::membership`]).
use super::{LogEntry, LogId, PeerId, PeerNode, TypeConfig, Vote, json_error, sql_error};
use openraft::{
    LogState, OptionalSend, RaftLogReader,
    storage::{IOFlushed, RaftLogStorage},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    io,
    ops::{Bound, RangeBounds},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS raft_log (
    idx INTEGER PRIMARY KEY NOT NULL,
    entry BLOB NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS raft_meta (
    key TEXT PRIMARY KEY NOT NULL,
    value BLOB NOT NULL
) STRICT;";

const VOTE: &str = "vote";
const PURGED: &str = "purged";
const COMMITTED: &str = "committed";
const CLUSTER_ID: &str = "cluster_id";
const RAFT_ID: &str = "raft_id";
const PEER: &str = "peer";
const INCARNATION: &str = "incarnation";

/// Who this peer is, as its Raft file records it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalIdentity {
    pub cluster_id: Option<String>,
    pub raft_id: Option<PeerId>,
    pub peer: Option<String>,
    pub incarnation: u64,
}

/// The log store. Cheap to clone: openraft's replication tasks read through
/// clones while the core appends.
#[derive(Clone)]
pub struct LogStore {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Connection>,
    /// Bytes of log entries currently stored, for the 64 MiB purge rule.
    bytes: AtomicU64,
    path: PathBuf,
}

impl LogStore {
    pub fn open(path: &Path) -> io::Result<Self> {
        let conn = Connection::open(path)
            .map_err(|error| io::Error::other(format!("{}: {error}", path.display())))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(sql_error)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .map_err(sql_error)?;
        conn.execute_batch(SCHEMA).map_err(sql_error)?;
        let bytes: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(length(entry)), 0) FROM raft_log",
                [],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        Ok(Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                bytes: AtomicU64::new(u64::try_from(bytes).unwrap_or(0)),
                path: path.to_path_buf(),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Bytes of log entries kept; the node builds a snapshot (and openraft
    /// purges behind it) once this passes 64 MiB.
    pub fn log_bytes(&self) -> u64 {
        self.inner.bytes.load(Ordering::Relaxed)
    }

    pub fn identity(&self) -> io::Result<LocalIdentity> {
        let conn = self.inner.conn.lock().unwrap();
        Ok(LocalIdentity {
            cluster_id: meta(&conn, CLUSTER_ID)?,
            raft_id: meta(&conn, RAFT_ID)?,
            peer: meta(&conn, PEER)?,
            incarnation: meta(&conn, INCARNATION)?.unwrap_or(0),
        })
    }

    /// Record the cluster, Raft id and name this file belongs to. Refuses to
    /// change values already recorded: a Raft id is never reassigned.
    pub fn adopt(&self, cluster_id: &str, raft_id: PeerId, peer: &str) -> io::Result<()> {
        let mut conn = self.inner.conn.lock().unwrap();
        let tx = conn.transaction().map_err(sql_error)?;
        let current = (
            meta::<String>(&tx, CLUSTER_ID)?,
            meta::<PeerId>(&tx, RAFT_ID)?,
            meta::<String>(&tx, PEER)?,
        );
        match current {
            (None, None, None) => {
                set_meta(&tx, CLUSTER_ID, &cluster_id)?;
                set_meta(&tx, RAFT_ID, &raft_id)?;
                set_meta(&tx, PEER, &peer)?;
            }
            (Some(id), Some(raft), Some(name))
                if id == cluster_id && raft == raft_id && name == peer => {}
            (id, raft, name) => {
                return Err(super::invalid(format!(
                    "{}: already belongs to cluster {} as {} (raft id {}); refusing to become {peer} (raft id {raft_id}) of cluster {cluster_id}",
                    self.inner.path.display(),
                    id.as_deref().unwrap_or("?"),
                    name.as_deref().unwrap_or("?"),
                    raft.map_or_else(|| "?".to_owned(), |raft| raft.to_string()),
                )));
            }
        }
        tx.commit().map_err(sql_error)
    }

    /// Raise and persist the incarnation counter; returns the new value.
    /// Called once per start, before the peer announces itself.
    pub fn next_incarnation(&self) -> io::Result<u64> {
        let mut conn = self.inner.conn.lock().unwrap();
        let tx = conn.transaction().map_err(sql_error)?;
        let next = meta::<u64>(&tx, INCARNATION)?.unwrap_or(0) + 1;
        set_meta(&tx, INCARNATION, &next)?;
        tx.commit().map_err(sql_error)?;
        Ok(next)
    }

    /// No vote and no entries: this peer never took part in a cluster.
    pub fn is_pristine(&self) -> io::Result<bool> {
        let conn = self.inner.conn.lock().unwrap();
        let entries: i64 = conn
            .query_row("SELECT COUNT(*) FROM raft_log", [], |row| row.get(0))
            .map_err(sql_error)?;
        Ok(entries == 0
            && meta::<Vote>(&conn, VOTE)?.is_none()
            && meta::<LogId>(&conn, PURGED)?.is_none())
    }

    /// `rift cluster recover`: make this peer the only voter. Keeps every
    /// entry it has, appends a membership entry naming only itself in a new
    /// term and votes for itself in that term, so the restarted peer elects
    /// itself without anyone else. Entries only the lost peers held are gone.
    pub fn recover_single(&self, raft_id: PeerId, node: PeerNode) -> io::Result<()> {
        let mut conn = self.inner.conn.lock().unwrap();
        let tx = conn.transaction().map_err(sql_error)?;
        let vote: Option<Vote> = meta(&tx, VOTE)?;
        let purged: Option<LogId> = meta(&tx, PURGED)?;
        let last = last_log_id(&tx)?.or(purged);
        let term = vote
            .map(|vote| vote.leader_id.term)
            .into_iter()
            .chain(last.map(|log_id| log_id.leader_id.term))
            .max()
            .unwrap_or(0)
            + 1;
        let index = last.map_or(1, |log_id| log_id.index + 1);
        let membership = super::Membership::new(
            vec![BTreeSet::from([raft_id])],
            BTreeMap::from([(raft_id, node)]),
        )
        .map_err(|error| super::invalid(error.to_string()))?;
        let leader = openraft::impls::leader_id_adv::LeaderId {
            term,
            node_id: raft_id,
        };
        let entry: LogEntry = openraft::Entry {
            log_id: openraft::LogId::new(leader, index),
            payload: openraft::EntryPayload::Membership(membership),
        };
        let body = serde_json::to_vec(&entry).map_err(json_error)?;
        tx.execute(
            "INSERT OR REPLACE INTO raft_log (idx, entry) VALUES (?1, ?2)",
            params![index_sql(index)?, body],
        )
        .map_err(sql_error)?;
        set_meta(&tx, VOTE, &Vote::new(term, raft_id))?;
        tx.commit().map_err(sql_error)?;
        self.inner
            .bytes
            .fetch_add(body.len() as u64, Ordering::Relaxed);
        Ok(())
    }

    fn entries(&self, start: u64, end: u64) -> io::Result<Vec<LogEntry>> {
        let conn = self.inner.conn.lock().unwrap();
        let mut statement = conn
            .prepare_cached("SELECT entry FROM raft_log WHERE idx >= ?1 AND idx < ?2 ORDER BY idx")
            .map_err(sql_error)?;
        let rows = statement
            .query_map(params![index_sql(start)?, index_sql(end)?], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(sql_error)?;
        let mut entries = Vec::new();
        for body in rows {
            let body = body.map_err(sql_error)?;
            entries.push(serde_json::from_slice::<LogEntry>(&body).map_err(json_error)?);
        }
        Ok(entries)
    }

    fn append_now<I>(&self, entries: I) -> io::Result<()>
    where
        I: IntoIterator<Item = LogEntry>,
    {
        let mut conn = self.inner.conn.lock().unwrap();
        let tx = conn.transaction().map_err(sql_error)?;
        let mut added = 0u64;
        {
            let mut insert = tx
                .prepare_cached("INSERT OR REPLACE INTO raft_log (idx, entry) VALUES (?1, ?2)")
                .map_err(sql_error)?;
            for entry in entries {
                let body = serde_json::to_vec(&entry).map_err(json_error)?;
                added += body.len() as u64;
                insert
                    .execute(params![index_sql(entry.log_id.index)?, body])
                    .map_err(sql_error)?;
            }
        }
        tx.commit().map_err(sql_error)?;
        self.inner.bytes.fetch_add(added, Ordering::Relaxed);
        Ok(())
    }

    fn delete_where(&self, condition: &str, index: u64, purged: Option<&LogId>) -> io::Result<()> {
        let mut conn = self.inner.conn.lock().unwrap();
        let tx = conn.transaction().map_err(sql_error)?;
        let removed: i64 = tx
            .query_row(
                &format!("SELECT COALESCE(SUM(length(entry)), 0) FROM raft_log WHERE {condition}"),
                params![index_sql(index)?],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        tx.execute(
            &format!("DELETE FROM raft_log WHERE {condition}"),
            params![index_sql(index)?],
        )
        .map_err(sql_error)?;
        if let Some(purged) = purged {
            set_meta(&tx, PURGED, purged)?;
        }
        tx.commit().map_err(sql_error)?;
        let removed = u64::try_from(removed).unwrap_or(0);
        let _ = self
            .inner
            .bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bytes| {
                Some(bytes.saturating_sub(removed))
            });
        Ok(())
    }

    fn write_meta<T: Serialize>(&self, key: &str, value: &T) -> io::Result<()> {
        let conn = self.inner.conn.lock().unwrap();
        set_meta(&conn, key, value)
    }

    fn read_meta<T: DeserializeOwned>(&self, key: &str) -> io::Result<Option<T>> {
        let conn = self.inner.conn.lock().unwrap();
        meta(&conn, key)
    }
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<LogEntry>, io::Error> {
        let start = match range.start_bound() {
            Bound::Included(start) => *start,
            Bound::Excluded(start) => start.saturating_add(1),
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(end) => end.saturating_add(1),
            Bound::Excluded(end) => *end,
            Bound::Unbounded => i64::MAX as u64,
        };
        self.entries(start, end)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote>, io::Error> {
        self.read_meta(VOTE)
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = LogStore;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, io::Error> {
        let conn = self.inner.conn.lock().unwrap();
        let purged: Option<LogId> = meta(&conn, PURGED)?;
        let last = last_log_id(&conn)?.or(purged);
        Ok(LogState {
            last_purged_log_id: purged,
            last_log_id: last,
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote) -> Result<(), io::Error> {
        self.write_meta(VOTE, vote)
    }

    async fn save_committed(&mut self, committed: Option<LogId>) -> Result<(), io::Error> {
        self.write_meta(COMMITTED, &committed)
    }

    async fn read_committed(&mut self) -> Result<Option<LogId>, io::Error> {
        Ok(self.read_meta::<Option<LogId>>(COMMITTED)?.flatten())
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: IOFlushed<TypeConfig>,
    ) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = LogEntry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        // synchronous=FULL: the commit returns once the entries are on disk.
        let result = self.append_now(entries);
        callback.io_completed(match &result {
            Ok(()) => Ok(()),
            Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
        });
        result
    }

    async fn truncate_after(&mut self, last_log_id: Option<LogId>) -> Result<(), io::Error> {
        let keep = last_log_id.map_or(0, |log_id| log_id.index);
        match last_log_id {
            Some(_) => self.delete_where("idx > ?1", keep, None),
            None => self.delete_where("idx >= ?1", 0, None),
        }
    }

    async fn purge(&mut self, log_id: LogId) -> Result<(), io::Error> {
        self.delete_where("idx <= ?1", log_id.index, Some(&log_id))
    }
}

fn index_sql(index: u64) -> io::Result<i64> {
    i64::try_from(index).map_err(|_| super::invalid("log index out of range"))
}

fn last_log_id(conn: &Connection) -> io::Result<Option<LogId>> {
    let body: Option<Vec<u8>> = conn
        .query_row(
            "SELECT entry FROM raft_log ORDER BY idx DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    body.map(|body| {
        serde_json::from_slice::<LogEntry>(&body)
            .map(|entry| entry.log_id)
            .map_err(json_error)
    })
    .transpose()
}

fn meta<T: DeserializeOwned>(conn: &Connection, key: &str) -> io::Result<Option<T>> {
    let body: Option<Vec<u8>> = conn
        .query_row("SELECT value FROM raft_meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(sql_error)?;
    body.map(|body| serde_json::from_slice(&body).map_err(json_error))
        .transpose()
}

fn set_meta<T: Serialize + ?Sized>(conn: &Connection, key: &str, value: &T) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(json_error)?;
    conn.execute(
        "INSERT INTO raft_meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, body],
    )
    .map_err(sql_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::state::{Entry, Origin, Proposal};

    pub(crate) fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::AtomicU32;
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let directory = std::env::temp_dir().join(format!(
            "rift-cluster-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn entry(term: u64, index: u64) -> LogEntry {
        openraft::Entry {
            log_id: openraft::LogId::new(
                openraft::impls::leader_id_adv::LeaderId { term, node_id: 1 },
                index,
            ),
            payload: openraft::EntryPayload::Normal(Proposal {
                origin: Some(Origin {
                    peer: "a".into(),
                    incarnation: 1,
                    serial: index,
                }),
                time_ms: 0,
                term: None,
                entry: Entry::Checksum(crate::cluster::state::ChecksumEntry::Mark { v: 1 }),
            }),
        }
    }

    #[tokio::test]
    async fn appends_truncates_purges_and_survives_reopening() {
        let directory = scratch("log");
        let path = directory.join("rift.raft.sqlite3");
        let mut store = LogStore::open(&path).unwrap();
        assert!(store.is_pristine().unwrap());
        store
            .append((1..=5).map(|index| entry(1, index)), IOFlushed::noop())
            .await
            .unwrap();
        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_log_id.unwrap().index, 5);
        assert_eq!(store.try_get_log_entries(2..4).await.unwrap().len(), 2);
        store
            .truncate_after(Some(openraft::LogId::new(
                openraft::impls::leader_id_adv::LeaderId {
                    term: 1,
                    node_id: 1,
                },
                3,
            )))
            .await
            .unwrap();
        assert_eq!(store.try_get_log_entries(..).await.unwrap().len(), 3);
        let purged = openraft::LogId::new(
            openraft::impls::leader_id_adv::LeaderId {
                term: 1,
                node_id: 1,
            },
            2,
        );
        store.purge(purged).await.unwrap();
        store.save_vote(&Vote::new(4, 2)).await.unwrap();
        drop(store);
        let mut store = LogStore::open(&path).unwrap();
        let state = store.get_log_state().await.unwrap();
        assert_eq!(state.last_purged_log_id, Some(purged));
        assert_eq!(state.last_log_id.unwrap().index, 3);
        assert_eq!(store.read_vote().await.unwrap(), Some(Vote::new(4, 2)));
        assert!(!store.is_pristine().unwrap());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn identity_is_adopted_once_and_incarnations_rise() {
        let directory = scratch("identity");
        let store = LogStore::open(&directory.join("rift.raft.sqlite3")).unwrap();
        assert_eq!(store.next_incarnation().unwrap(), 1);
        assert_eq!(store.next_incarnation().unwrap(), 2);
        store.adopt("c1", 3, "eu-3").unwrap();
        store.adopt("c1", 3, "eu-3").unwrap();
        assert!(store.adopt("c1", 4, "eu-3").is_err());
        assert!(store.adopt("c2", 3, "eu-3").is_err());
        let identity = store.identity().unwrap();
        assert_eq!(identity.raft_id, Some(3));
        assert_eq!(identity.incarnation, 2);
        let _ = std::fs::remove_dir_all(directory);
    }
}
