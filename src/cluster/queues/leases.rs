//! Named leases for plugin work that must run once in the network
//! (docs/clustering.md, decision 8).
//!
//! `rift.cluster.lease(name, ttl_ms)` asks for the lease `name` of the
//! calling plugin (each plugin, and the configuration script, has its own
//! names). Leases are `lease` entries in the Raft log, so every peer applies
//! the same grants in the same order and two peers never hold one lease at
//! once. A grant returns a **fencing token**: a number that grows with every
//! grant of that lease and never goes back, across leader changes and
//! restarts, because it is part of the replicated state. Work done under a
//! lease can carry the token, and whatever receives the work can refuse a
//! token lower than one it has seen, so a holder that lost its lease without
//! noticing cannot do harm.
//!
//! A lease is held while its holder (the peer and its incarnation) is alive
//! and not expired, and its time-to-live has not passed. Time is the
//! leader's stamp on each entry, never a peer's own clock. Asking again
//! while holding the lease extends it and returns the same token;
//! `renew(name, token, ttl_ms)` extends it only for the current holder and
//! token, so a holder that was deposed (its lease ran out or its peer
//! expired, and another took it) learns so; `release(name, token)` gives it
//! up.
//!
//! Without a cluster there is nobody to compete with: a lease is always
//! granted (the same token while it is held), and `renew` succeeds for the
//! current token.
use crate::cluster::{
    Cluster,
    node::PROPOSE_TIMEOUT,
    sql_error,
    state::{ApplyContext, Component, Entry, EntryKind, Payload, Reply},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    future::Future,
    io,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const VERSION: u32 = 1;
/// The table of leases in the replicated database.
pub const TABLE: &str = "cluster_leases";
/// Bounds of a lease's time-to-live.
pub const MIN_TTL_MS: u64 = 100;
pub const MAX_TTL_MS: u64 = 3_600_000;
const MAX_NAME: usize = 128;
/// The owner of the configuration script's leases.
pub const CONFIG_OWNER: &str = "config";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cluster_leases (
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    fence INTEGER NOT NULL,
    peer TEXT,
    incarnation INTEGER NOT NULL,
    expires_ms INTEGER NOT NULL,
    PRIMARY KEY (owner, name)
) STRICT;";

/// The owner of a plugin's leases.
pub fn plugin_owner(plugin: &str) -> String {
    format!("plugin:{plugin}")
}

/// The content of a `lease` entry (version 1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum LeaseEntry {
    /// Grant `owner`'s lease `name` to `peer` (running `incarnation`)
    /// unless another live holder has it.
    Acquire {
        owner: String,
        name: String,
        peer: String,
        incarnation: u64,
        ttl_ms: u64,
    },
    /// Extend the lease if `peer` still holds it with `token`.
    Renew {
        owner: String,
        name: String,
        peer: String,
        incarnation: u64,
        token: u64,
        ttl_ms: u64,
    },
    /// Give the lease up if `peer` holds it with `token`.
    Release {
        owner: String,
        name: String,
        peer: String,
        incarnation: u64,
        token: u64,
    },
}

impl LeaseEntry {
    fn entry(&self) -> Entry {
        Entry::Lease(Payload::new(
            VERSION,
            serde_json::to_value(self).unwrap_or(serde_json::Value::Null),
        ))
    }
}

/// The `lease` entry kind's component. Register it on every peer
/// ([`super::components`]).
pub struct LeaseMachine;

impl Component for LeaseMachine {
    fn name(&self) -> &'static str {
        "leases"
    }

    fn kinds(&self) -> Vec<EntryKind> {
        vec![EntryKind::Lease]
    }

    fn apply(&self, db: &Connection, entry: &Entry, ctx: &mut ApplyContext) -> io::Result<Reply> {
        let Entry::Lease(payload) = entry else {
            return Ok(Reply::err(format!(
                "component leases does not apply {} entries",
                entry.kind()
            )));
        };
        if payload.v != VERSION {
            return Ok(Reply::err(format!(
                "lease entry version {} is not supported by this peer",
                payload.v
            )));
        }
        let entry: LeaseEntry = match serde_json::from_value(payload.data.clone()) {
            Ok(entry) => entry,
            Err(error) => return Ok(Reply::err(format!("malformed lease entry: {error}"))),
        };
        db.execute_batch(SCHEMA).map_err(sql_error)?;
        apply(db, entry, ctx)
    }
}

/// A row of `cluster_leases`.
struct Row {
    fence: i64,
    peer: Option<String>,
    incarnation: i64,
    expires_ms: i64,
}

fn sql(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn read(db: &Connection, owner: &str, name: &str) -> io::Result<Option<Row>> {
    db.query_row(
        "SELECT fence, peer, incarnation, expires_ms FROM cluster_leases WHERE owner = ?1 AND name = ?2",
        params![owner, name],
        |row| {
            Ok(Row {
                fence: row.get(0)?,
                peer: row.get(1)?,
                incarnation: row.get(2)?,
                expires_ms: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(sql_error)
}

/// Whether `peer` runs `incarnation` and has not expired, as of this entry.
fn alive(db: &Connection, peer: &str, incarnation: i64) -> io::Result<bool> {
    db.query_row(
        "SELECT COUNT(*) > 0 FROM cluster_peers WHERE name = ?1 AND incarnation = ?2 AND expired = 0",
        params![peer, incarnation],
        |row| row.get(0),
    )
    .map_err(sql_error)
}

/// The live holder of `row` at `now`, if any.
fn holder(db: &Connection, row: &Row, now: i64) -> io::Result<Option<(String, i64)>> {
    let Some(peer) = &row.peer else {
        return Ok(None);
    };
    if row.expires_ms <= now || !alive(db, peer, row.incarnation)? {
        return Ok(None);
    }
    Ok(Some((peer.clone(), row.incarnation)))
}

fn valid(owner: &str, name: &str, ttl_ms: Option<u64>) -> Result<(), String> {
    if owner.is_empty() || owner.len() > MAX_NAME + 16 {
        return Err("lease owner must be 1..=144 bytes".into());
    }
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(format!("lease name must be 1..={MAX_NAME} bytes"));
    }
    if let Some(ttl) = ttl_ms
        && !(MIN_TTL_MS..=MAX_TTL_MS).contains(&ttl)
    {
        return Err(format!(
            "lease ttl must be {MIN_TTL_MS}..={MAX_TTL_MS} milliseconds"
        ));
    }
    Ok(())
}

fn apply(db: &Connection, entry: LeaseEntry, ctx: &mut ApplyContext) -> io::Result<Reply> {
    let now = sql(ctx.time_ms);
    match entry {
        LeaseEntry::Acquire {
            owner,
            name,
            peer,
            incarnation,
            ttl_ms,
        } => {
            if let Err(message) = valid(&owner, &name, Some(ttl_ms)) {
                return Ok(Reply::err(message));
            }
            let incarnation = sql(incarnation);
            // A peer the leader expired gets nothing until it joined again.
            if !alive(db, &peer, incarnation)? {
                return Ok(Reply::ok(
                    serde_json::json!({ "token": null, "holder": null }),
                ));
            }
            let row = read(db, &owner, &name)?;
            let current = match &row {
                Some(row) => holder(db, row, now)?,
                None => None,
            };
            let expires = now.saturating_add(sql(ttl_ms));
            if let Some((holding, held)) = current {
                if holding != peer || held != incarnation {
                    return Ok(Reply::ok(
                        serde_json::json!({ "token": null, "holder": holding }),
                    ));
                }
                // The holder asks again: extend, same token.
                db.execute(
                    "UPDATE cluster_leases SET expires_ms = ?3 WHERE owner = ?1 AND name = ?2",
                    params![owner, name, expires],
                )
                .map_err(sql_error)?;
                ctx.changes.touch(TABLE, format!("{owner}/{name}"));
                let fence = row.map_or(0, |row| row.fence);
                return Ok(Reply::ok(serde_json::json!({ "token": fence })));
            }
            let fence = row.map_or(0, |row| row.fence).saturating_add(1);
            db.execute(
                "INSERT INTO cluster_leases (owner, name, fence, peer, incarnation, expires_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (owner, name) DO UPDATE SET fence = excluded.fence,
                 peer = excluded.peer, incarnation = excluded.incarnation,
                 expires_ms = excluded.expires_ms",
                params![owner, name, fence, peer, incarnation, expires],
            )
            .map_err(sql_error)?;
            ctx.changes.touch(TABLE, format!("{owner}/{name}"));
            Ok(Reply::ok(serde_json::json!({ "token": fence })))
        }
        LeaseEntry::Renew {
            owner,
            name,
            peer,
            incarnation,
            token,
            ttl_ms,
        } => {
            if let Err(message) = valid(&owner, &name, Some(ttl_ms)) {
                return Ok(Reply::err(message));
            }
            let Some(row) = read(db, &owner, &name)? else {
                return Ok(Reply::ok(false));
            };
            let holds =
                row.fence == sql(token) && holder(db, &row, now)? == Some((peer, sql(incarnation)));
            if !holds {
                return Ok(Reply::ok(false));
            }
            db.execute(
                "UPDATE cluster_leases SET expires_ms = ?3 WHERE owner = ?1 AND name = ?2",
                params![owner, name, now.saturating_add(sql(ttl_ms))],
            )
            .map_err(sql_error)?;
            ctx.changes.touch(TABLE, format!("{owner}/{name}"));
            Ok(Reply::ok(true))
        }
        LeaseEntry::Release {
            owner,
            name,
            peer,
            incarnation,
            token,
        } => {
            if let Err(message) = valid(&owner, &name, None) {
                return Ok(Reply::err(message));
            }
            let Some(row) = read(db, &owner, &name)? else {
                return Ok(Reply::ok(false));
            };
            let holds = row.fence == sql(token)
                && row.peer.as_deref() == Some(peer.as_str())
                && row.incarnation == sql(incarnation);
            if !holds {
                return Ok(Reply::ok(false));
            }
            // The fence stays: the next grant continues from it.
            db.execute(
                "UPDATE cluster_leases SET peer = NULL, expires_ms = 0 WHERE owner = ?1 AND name = ?2",
                params![owner, name],
            )
            .map_err(sql_error)?;
            ctx.changes.touch(TABLE, format!("{owner}/{name}"));
            Ok(Reply::ok(true))
        }
    }
}

fn unsupported(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::Unsupported {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "cluster leases are not enabled yet: every peer must run a Rift that supports them",
        )
    } else {
        error
    }
}

async fn propose(cluster: &Cluster, entry: LeaseEntry) -> io::Result<serde_json::Value> {
    cluster
        .propose(entry.entry())
        .await
        .map_err(unsupported)?
        .into_result()
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))
}

fn check(owner: &str, name: &str, ttl_ms: Option<u64>) -> io::Result<()> {
    valid(owner, name, ttl_ms)
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))
}

/// Ask `cluster` for `owner`'s lease `name` for `ttl_ms`: its token, or
/// `None` while another peer holds it.
pub async fn acquire(
    cluster: &Cluster,
    owner: &str,
    name: &str,
    ttl_ms: u64,
) -> io::Result<Option<u64>> {
    check(owner, name, Some(ttl_ms))?;
    let reply = propose(
        cluster,
        LeaseEntry::Acquire {
            owner: owner.to_owned(),
            name: name.to_owned(),
            peer: cluster.name().to_owned(),
            incarnation: cluster.incarnation(),
            ttl_ms,
        },
    )
    .await?;
    Ok(reply.get("token").and_then(serde_json::Value::as_u64))
}

/// Extend the lease this peer holds with `token`; false once it does not.
pub async fn renew(
    cluster: &Cluster,
    owner: &str,
    name: &str,
    token: u64,
    ttl_ms: u64,
) -> io::Result<bool> {
    check(owner, name, Some(ttl_ms))?;
    let reply = propose(
        cluster,
        LeaseEntry::Renew {
            owner: owner.to_owned(),
            name: name.to_owned(),
            peer: cluster.name().to_owned(),
            incarnation: cluster.incarnation(),
            token,
            ttl_ms,
        },
    )
    .await?;
    Ok(reply.as_bool().unwrap_or(false))
}

/// Give up the lease this peer holds with `token`; false if it did not.
pub async fn release(cluster: &Cluster, owner: &str, name: &str, token: u64) -> io::Result<bool> {
    check(owner, name, None)?;
    let reply = propose(
        cluster,
        LeaseEntry::Release {
            owner: owner.to_owned(),
            name: name.to_owned(),
            peer: cluster.name().to_owned(),
            incarnation: cluster.incarnation(),
            token,
        },
    )
    .await?;
    Ok(reply.as_bool().unwrap_or(false))
}

/// Leases of a process without a cluster: token by `(owner, name)`.
static LOCAL: Mutex<BTreeMap<(String, String), u64>> = Mutex::new(BTreeMap::new());
static LOCAL_FENCE: AtomicU64 = AtomicU64::new(0);

/// How long a blocking call waits for the cluster at most.
const WAIT: Duration = Duration::from_secs(PROPOSE_TIMEOUT.as_secs() + 1);

/// Run `work` on `handle` and wait for it on this (non-async) thread, at
/// most until `deadline`. Safe from any thread: a thread of the runtime
/// itself would time out rather than hang.
fn wait<T: Send + 'static>(
    handle: &tokio::runtime::Handle,
    deadline: Option<Instant>,
    work: impl Future<Output = io::Result<T>> + Send + 'static,
) -> Result<T, String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    handle.spawn(async move {
        let _ = sender.send(work.await);
    });
    let limit = deadline.map_or(WAIT, |deadline| {
        deadline.saturating_duration_since(Instant::now()).min(WAIT)
    });
    match receiver.recv_timeout(limit) {
        Ok(result) => result.map_err(|error| error.to_string()),
        Err(_) => Err("the cluster did not answer in time".into()),
    }
}

/// [`acquire`] for plugin threads and Lua callbacks: on this process's
/// cluster, or granted at once without one.
pub fn lease_blocking(
    handle: &tokio::runtime::Handle,
    deadline: Option<Instant>,
    owner: &str,
    name: &str,
    ttl_ms: u64,
) -> Result<Option<u64>, String> {
    valid(owner, name, Some(ttl_ms))?;
    let Some(cluster) = crate::cluster::global() else {
        let mut local = LOCAL.lock().unwrap_or_else(|e| e.into_inner());
        let token = *local
            .entry((owner.to_owned(), name.to_owned()))
            .or_insert_with(|| LOCAL_FENCE.fetch_add(1, Ordering::Relaxed) + 1);
        return Ok(Some(token));
    };
    let (cluster, owner, name) = (cluster.clone(), owner.to_owned(), name.to_owned());
    wait(handle, deadline, async move {
        acquire(&cluster, &owner, &name, ttl_ms).await
    })
}

/// [`renew`] for plugin threads and Lua callbacks.
pub fn renew_blocking(
    handle: &tokio::runtime::Handle,
    deadline: Option<Instant>,
    owner: &str,
    name: &str,
    token: u64,
    ttl_ms: u64,
) -> Result<bool, String> {
    valid(owner, name, Some(ttl_ms))?;
    let Some(cluster) = crate::cluster::global() else {
        let local = LOCAL.lock().unwrap_or_else(|e| e.into_inner());
        return Ok(local.get(&(owner.to_owned(), name.to_owned())) == Some(&token));
    };
    let (cluster, owner, name) = (cluster.clone(), owner.to_owned(), name.to_owned());
    wait(handle, deadline, async move {
        renew(&cluster, &owner, &name, token, ttl_ms).await
    })
}

/// [`release`] for plugin threads and Lua callbacks.
pub fn release_blocking(
    handle: &tokio::runtime::Handle,
    deadline: Option<Instant>,
    owner: &str,
    name: &str,
    token: u64,
) -> Result<bool, String> {
    valid(owner, name, None)?;
    let Some(cluster) = crate::cluster::global() else {
        let mut local = LOCAL.lock().unwrap_or_else(|e| e.into_inner());
        let key = (owner.to_owned(), name.to_owned());
        if local.get(&key) != Some(&token) {
            return Ok(false);
        }
        local.remove(&key);
        return Ok(true);
    };
    let (cluster, owner, name) = (cluster.clone(), owner.to_owned(), name.to_owned());
    wait(handle, deadline, async move {
        release(&cluster, &owner, &name, token).await
    })
}

/// `rift.cluster.is_leader()`: whether this peer leads with a lease right
/// now. Advisory: leadership can end a moment later; use a lease for work
/// that must not run twice. Always true without a cluster.
pub fn is_leader() -> bool {
    crate::cluster::global().is_none_or(Cluster::is_leader_with_lease)
}

/// `rift.cluster.peer()`: this peer's name, `None` without a cluster.
pub fn peer() -> Option<&'static str> {
    crate::cluster::settings::local_peer()
}

/// A Lua token: tokens stay far below `i64::MAX`.
fn lua_token(token: u64) -> i64 {
    i64::try_from(token).unwrap_or(i64::MAX)
}

fn lua_ttl(ttl_ms: i64) -> mlua::Result<u64> {
    u64::try_from(ttl_ms).map_err(|_| {
        mlua::Error::runtime(format!(
            "lease ttl must be {MIN_TTL_MS}..={MAX_TTL_MS} milliseconds"
        ))
    })
}

/// `rift.cluster` inside the configuration script's callbacks and jobs:
/// `lease(name, ttl_ms)` returns a token or nil, `renew(name, token,
/// ttl_ms)` and `release(name, token)` return whether they took effect,
/// `is_leader()` and `peer()`. Leases belong to `owner`.
pub(crate) fn install_lua(
    lua: &mlua::Lua,
    runtime: &mlua::Table,
    owner: &str,
    deadline: Instant,
) -> mlua::Result<()> {
    fn handle() -> mlua::Result<tokio::runtime::Handle> {
        tokio::runtime::Handle::try_current()
            .map_err(|_| mlua::Error::runtime("rift.cluster: no runtime to reach the cluster"))
    }
    let table = lua.create_table()?;
    let lease_owner = owner.to_owned();
    table.raw_set(
        "lease",
        lua.create_function(move |_, (name, ttl_ms): (String, i64)| {
            crate::script::check_deadline(deadline)?;
            lease_blocking(
                &handle()?,
                Some(deadline),
                &lease_owner,
                &name,
                lua_ttl(ttl_ms)?,
            )
            .map(|token| token.map(lua_token))
            .map_err(mlua::Error::runtime)
        })?,
    )?;
    let renew_owner = owner.to_owned();
    table.raw_set(
        "renew",
        lua.create_function(move |_, (name, token, ttl_ms): (String, i64, i64)| {
            crate::script::check_deadline(deadline)?;
            let Ok(token) = u64::try_from(token) else {
                return Ok(false);
            };
            renew_blocking(
                &handle()?,
                Some(deadline),
                &renew_owner,
                &name,
                token,
                lua_ttl(ttl_ms)?,
            )
            .map_err(mlua::Error::runtime)
        })?,
    )?;
    let release_owner = owner.to_owned();
    table.raw_set(
        "release",
        lua.create_function(move |_, (name, token): (String, i64)| {
            crate::script::check_deadline(deadline)?;
            let Ok(token) = u64::try_from(token) else {
                return Ok(false);
            };
            release_blocking(&handle()?, Some(deadline), &release_owner, &name, token)
                .map_err(mlua::Error::runtime)
        })?,
    )?;
    table.raw_set("is_leader", lua.create_function(|_, ()| Ok(is_leader()))?)?;
    table.raw_set("peer", lua.create_function(|_, ()| Ok(peer()))?)?;
    runtime.raw_set("cluster", table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_a_cluster_leases_are_always_granted() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let owner = plugin_owner("lease-local-test");
        let token = lease_blocking(&handle, None, &owner, "sweeper", 1000)
            .unwrap()
            .unwrap();
        // Asking again while holding it keeps the token.
        assert_eq!(
            lease_blocking(&handle, None, &owner, "sweeper", 1000).unwrap(),
            Some(token)
        );
        assert!(renew_blocking(&handle, None, &owner, "sweeper", token, 1000).unwrap());
        assert!(!renew_blocking(&handle, None, &owner, "sweeper", token + 1, 1000).unwrap());
        assert!(release_blocking(&handle, None, &owner, "sweeper", token).unwrap());
        assert!(!release_blocking(&handle, None, &owner, "sweeper", token).unwrap());
        let next = lease_blocking(&handle, None, &owner, "sweeper", 1000)
            .unwrap()
            .unwrap();
        assert!(next > token);
        assert!(lease_blocking(&handle, None, &owner, "", 1000).is_err());
        assert!(lease_blocking(&handle, None, &owner, "x", 10).is_err());
        assert!(is_leader());
    }
}
