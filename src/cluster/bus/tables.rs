//! `bus` entries: node session claims, the bus-login key and the peers' bus
//! addresses, with the in-memory mirrors [`BusState`] keeps of them.
//!
//! | Table | Key | Holds |
//! | --- | --- | --- |
//! | `bus_node_sessions` | node | the peer, incarnation and session id of the node's newest session |
//! | `bus_settings` | `instance_key` | the bus-login key, hex |
//! | `bus_addresses` | peer | the incarnation, messaging address and certificate name a peer announced |
//!
//! A claim or address of a peer counts while the peer runs the incarnation
//! that wrote it and has not expired; the mirrors join with
//! `cluster_peers` for that, so a peer that restarts or expires releases
//! them without entries of its own.
use super::{BusState, LocalSession, PeerAddress};
use crate::cluster::{
    sql_error,
    state::{ApplyContext, Changes, Component, Entry, EntryKind, Payload, Reply},
    unhex,
};
use crate::instance_login::KEY_BYTES;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    io,
    sync::Arc,
};

pub(super) const SESSIONS: &str = "bus_node_sessions";
pub(super) const SETTINGS: &str = "bus_settings";
pub(super) const ADDRESSES: &str = "bus_addresses";
const PEERS: &str = "cluster_peers";
const KEY: &str = "instance_key";
const VERSION: u32 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS bus_node_sessions (
    node TEXT PRIMARY KEY NOT NULL,
    peer TEXT NOT NULL,
    incarnation INTEGER NOT NULL,
    session INTEGER NOT NULL,
    since_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS bus_settings (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS bus_addresses (
    peer TEXT PRIMARY KEY NOT NULL,
    incarnation INTEGER NOT NULL,
    address TEXT NOT NULL,
    server_name TEXT NOT NULL
) STRICT;";

/// The content of a `bus` entry (version 1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(super) enum BusEntry {
    /// The node agent `node` opened session `session` on `peer`: it holds
    /// the node's session from now on, whoever held it before.
    NodeClaim {
        node: String,
        peer: String,
        incarnation: u64,
        session: u64,
    },
    /// That session ended; nothing happens if a newer one holds the node.
    NodeRelease {
        node: String,
        peer: String,
        incarnation: u64,
        session: u64,
    },
    /// The bus-login key, hex. Without `replace`, only if there is none.
    InstanceKey { key: String, replace: bool },
    /// Where bus clients reach `peer` while it runs `incarnation`.
    Address {
        peer: String,
        incarnation: u64,
        address: String,
        server_name: String,
    },
}

impl BusEntry {
    pub(super) fn entry(&self) -> Entry {
        Entry::Bus(Payload::new(
            VERSION,
            serde_json::to_value(self).unwrap_or(serde_json::Value::Null),
        ))
    }
}

/// The `bus` entry kind's component.
pub(super) struct BusTables {
    pub(super) state: Arc<BusState>,
}

impl Component for BusTables {
    fn name(&self) -> &'static str {
        "bus"
    }

    fn kinds(&self) -> Vec<EntryKind> {
        vec![EntryKind::Bus]
    }

    fn apply(&self, db: &Connection, entry: &Entry, ctx: &mut ApplyContext) -> io::Result<Reply> {
        let Entry::Bus(payload) = entry else {
            return Ok(Reply::err(format!(
                "component bus does not apply {} entries",
                entry.kind()
            )));
        };
        if payload.v != VERSION {
            return Ok(Reply::err(format!(
                "bus entry version {} is not supported by this peer",
                payload.v
            )));
        }
        let entry: BusEntry = match serde_json::from_value(payload.data.clone()) {
            Ok(entry) => entry,
            Err(error) => return Ok(Reply::err(format!("malformed bus entry: {error}"))),
        };
        db.execute_batch(SCHEMA).map_err(sql_error)?;
        apply(db, entry, ctx)
    }

    fn after_apply(&self, db: &Connection, changes: &Changes) {
        if let Err(error) = refresh(&self.state, db, changes) {
            log_warn!(
                "cluster bus: refreshing after entry {}: {error}",
                changes.index
            );
        }
    }

    fn full_reload(&self, db: &Connection) -> io::Result<()> {
        reload(&self.state, db)
    }
}

fn sql_u64(value: u64) -> Option<i64> {
    i64::try_from(value).ok()
}

fn apply(db: &Connection, entry: BusEntry, ctx: &mut ApplyContext) -> io::Result<Reply> {
    let time = i64::try_from(ctx.time_ms).unwrap_or(i64::MAX);
    match entry {
        BusEntry::NodeClaim {
            node,
            peer,
            incarnation,
            session,
        } => {
            let (Some(incarnation), Some(session)) = (sql_u64(incarnation), sql_u64(session))
            else {
                return Ok(Reply::err("node claim: value out of range"));
            };
            if node.is_empty() || node.len() > 128 || peer.is_empty() {
                return Ok(Reply::err("node claim: invalid node or peer name"));
            }
            let previous: Option<String> = db
                .query_row(
                    "SELECT peer FROM bus_node_sessions WHERE node = ?1",
                    [&node],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql_error)?;
            db.execute(
                "INSERT INTO bus_node_sessions (node, peer, incarnation, session, since_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (node) DO UPDATE SET peer = excluded.peer,
                 incarnation = excluded.incarnation, session = excluded.session,
                 since_ms = excluded.since_ms",
                params![node, peer, incarnation, session, time],
            )
            .map_err(sql_error)?;
            ctx.changes.touch(SESSIONS, node);
            Ok(Reply::ok(serde_json::json!({ "previous": previous })))
        }
        BusEntry::NodeRelease {
            node,
            peer,
            incarnation,
            session,
        } => {
            let (Some(incarnation), Some(session)) = (sql_u64(incarnation), sql_u64(session))
            else {
                return Ok(Reply::err("node release: value out of range"));
            };
            let released = db
                .execute(
                    "DELETE FROM bus_node_sessions
                     WHERE node = ?1 AND peer = ?2 AND incarnation = ?3 AND session = ?4",
                    params![node, peer, incarnation, session],
                )
                .map_err(sql_error)?;
            if released > 0 {
                ctx.changes.touch(SESSIONS, node);
            }
            Ok(Reply::ok(released > 0))
        }
        BusEntry::InstanceKey { key, replace } => {
            if unhex(&key).is_none_or(|bytes| bytes.len() != KEY_BYTES) {
                return Ok(Reply::err(format!(
                    "the bus-login key must be {KEY_BYTES} bytes in hex"
                )));
            }
            let exists: bool = db
                .query_row(
                    "SELECT COUNT(*) > 0 FROM bus_settings WHERE key = ?1",
                    [KEY],
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            if exists && !replace {
                return Ok(Reply::ok(serde_json::json!({ "set": false })));
            }
            db.execute(
                "INSERT INTO bus_settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![KEY, key.to_ascii_lowercase()],
            )
            .map_err(sql_error)?;
            ctx.changes.touch(SETTINGS, KEY);
            Ok(Reply::ok(serde_json::json!({ "set": true })))
        }
        BusEntry::Address {
            peer,
            incarnation,
            address,
            server_name,
        } => {
            let Some(incarnation) = sql_u64(incarnation) else {
                return Ok(Reply::err("bus address: value out of range"));
            };
            if peer.is_empty()
                || address.is_empty()
                || address.len() > 256
                || server_name.len() > 256
            {
                return Ok(Reply::err("bus address: invalid peer, address or name"));
            }
            db.execute(
                "INSERT INTO bus_addresses (peer, incarnation, address, server_name)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (peer) DO UPDATE SET incarnation = excluded.incarnation,
                 address = excluded.address, server_name = excluded.server_name",
                params![peer, incarnation, address, server_name],
            )
            .map_err(sql_error)?;
            ctx.changes.touch(ADDRESSES, peer);
            Ok(Reply::ok(serde_json::Value::Null))
        }
    }
}

/// Whether `table` exists yet: the tables appear with the first entry that
/// needs them, on every peer at the same index.
pub(super) fn table_exists(db: &Connection, table: &str) -> io::Result<bool> {
    db.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )
    .map_err(sql_error)
}

/// A node session as the table records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Holder {
    pub(super) peer: String,
    pub(super) incarnation: u64,
    pub(super) session: u64,
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// The recorded holder of `node`'s session, live or not.
fn session_row(db: &Connection, node: &str) -> io::Result<Option<Holder>> {
    if !table_exists(db, SESSIONS)? {
        return Ok(None);
    }
    db.query_row(
        "SELECT peer, incarnation, session FROM bus_node_sessions WHERE node = ?1",
        [node],
        |row| {
            Ok(Holder {
                peer: row.get(0)?,
                incarnation: to_u64(row.get(1)?),
                session: to_u64(row.get(2)?),
            })
        },
    )
    .optional()
    .map_err(sql_error)
}

/// Node → the live peer holding its session.
fn live_holders(db: &Connection) -> io::Result<HashMap<String, String>> {
    if !table_exists(db, SESSIONS)? {
        return Ok(HashMap::new());
    }
    let mut statement = db
        .prepare(
            "SELECT s.node, s.peer FROM bus_node_sessions s
             JOIN cluster_peers p ON p.name = s.peer AND p.incarnation = s.incarnation
             WHERE p.expired = 0",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(sql_error)?;
    let mut holders = HashMap::new();
    for row in rows {
        let (node, peer) = row.map_err(sql_error)?;
        holders.insert(node, peer);
    }
    Ok(holders)
}

/// The bus addresses of the live peers, by peer name.
pub(super) fn live_addresses(db: &Connection) -> io::Result<Vec<PeerAddress>> {
    if !table_exists(db, ADDRESSES)? {
        return Ok(Vec::new());
    }
    let mut statement = db
        .prepare(
            "SELECT a.peer, a.address, a.server_name FROM bus_addresses a
             JOIN cluster_peers p ON p.name = a.peer AND p.incarnation = a.incarnation
             WHERE p.expired = 0 ORDER BY a.peer",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok(PeerAddress {
                peer: row.get(0)?,
                address: row.get(1)?,
                server_name: row.get(2)?,
            })
        })
        .map_err(sql_error)?;
    let mut addresses = Vec::new();
    for row in rows {
        addresses.push(row.map_err(sql_error)?);
    }
    Ok(addresses)
}

fn instance_key(db: &Connection) -> io::Result<Option<[u8; KEY_BYTES]>> {
    if !table_exists(db, SETTINGS)? {
        return Ok(None);
    }
    let text: Option<String> = db
        .query_row(
            "SELECT value FROM bus_settings WHERE key = ?1",
            [KEY],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    Ok(text
        .and_then(|text| unhex(&text))
        .and_then(|bytes| <[u8; KEY_BYTES]>::try_from(bytes).ok()))
}

/// Rebuild every mirror from `db`.
pub(super) fn reload(state: &BusState, db: &Connection) -> io::Result<()> {
    *state.holders.write().unwrap_or_else(|e| e.into_inner()) = live_holders(db)?;
    state.set_addresses(live_addresses(db)?);
    state.set_key(instance_key(db)?);
    Ok(())
}

/// After an applied entry: refresh the mirrors it touched, and close this
/// peer's node sessions that another peer's newer session replaced.
fn refresh(state: &Arc<BusState>, db: &Connection, changes: &Changes) -> io::Result<()> {
    let peers = changes.touched(PEERS);
    if changes.touched(SESSIONS) || peers {
        *state.holders.write().unwrap_or_else(|e| e.into_inner()) = live_holders(db)?;
    }
    if let Some(nodes) = changes.tables.get(SESSIONS) {
        replace_sessions(state, db, nodes)?;
    }
    if changes.touched(ADDRESSES) || peers {
        state.set_addresses(live_addresses(db)?);
    }
    if changes.touched(SETTINGS) {
        state.set_key(instance_key(db)?);
    }
    Ok(())
}

/// For each node whose claim changed: a local session that the table now
/// names is claimed; a claimed local session that another peer's session
/// replaced closes. A local session whose own claim has not applied yet
/// stays: its claim comes later in the log and wins then.
fn replace_sessions(
    state: &Arc<BusState>,
    db: &Connection,
    nodes: &BTreeSet<String>,
) -> io::Result<()> {
    let Some((me, incarnation)) = state.me() else {
        return Ok(());
    };
    let mut closed = Vec::new();
    let mut reclaim = Vec::new();
    {
        let mut sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let changed: Vec<String> = if nodes.is_empty() {
            sessions.keys().cloned().collect()
        } else {
            nodes.iter().cloned().collect()
        };
        for node in changed {
            let Some(local) = sessions.get(&node).copied() else {
                continue;
            };
            let Some(holder) = session_row(db, &node)? else {
                continue;
            };
            let mine = holder.peer == me && holder.incarnation == incarnation;
            if mine && holder.session == local.id {
                sessions.insert(
                    node,
                    LocalSession {
                        id: local.id,
                        claimed: true,
                    },
                );
            } else if mine {
                // An older claim of this peer applied after the current
                // session's: claim the current one again.
                sessions.insert(
                    node.clone(),
                    LocalSession {
                        id: local.id,
                        claimed: false,
                    },
                );
                reclaim.push((node, local.id));
            } else if local.claimed {
                sessions.remove(&node);
                closed.push((node, local.id));
            }
        }
    }
    if !closed.is_empty()
        && let Some(nodes) = state.node_logins()
    {
        for (node, session) in closed {
            log_info!(
                "cluster bus: node {node} opened a newer session on another peer; closing this one"
            );
            nodes.close_replaced(&format!("node.{node}"), session);
        }
    }
    for (node, session) in reclaim {
        super::sessions::spawn_claim(state, node, session);
    }
    Ok(())
}
