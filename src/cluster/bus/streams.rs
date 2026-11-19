//! Retained streams in a cluster (docs/clustering.md, "Retained streams").
//!
//! The stream service runs on the leader. Every peer routes its stream
//! handles through [`Backend`], which runs an operation here when this peer
//! leads and otherwise sends it to the leader on the `bus` stream.
//!
//! Durable streams (those with storage) live in the replicated state:
//! appends, consumers and acknowledgements are `stream` entries applied by
//! [`StreamMachine`], with retention applied at apply from the limits the
//! entry carries, so every replica holds the same records. Delivery state
//! (which records are pending, since when, how often delivered) stays in the
//! leader's memory. A new leader starts every consumer at its acknowledged
//! floor and skips the records acknowledged above it, so acknowledged
//! records stay acknowledged and records that were delivered but not
//! acknowledged are delivered again: streams promise at-least-once delivery.
//!
//! Memory streams are not replicated: the leader runs them on its own
//! handle and empties them when it gains leadership.
//!
//! | Table | Key | Holds |
//! | --- | --- | --- |
//! | `bus_streams` | stream | next sequence, retained bytes and records |
//! | `bus_stream_records` | stream, sequence | leader time, subject, reply, payload, size |
//! | `bus_stream_consumers` | stream, name | settings and the acknowledged floor |
//! | `bus_stream_acks` | stream, name, sequence | acknowledgements above the floor |
use super::{BusState, STREAMS};
use crate::cluster::{
    Cluster, StreamKind, hex,
    link::remote_error,
    node::LeaderContext,
    sql_error,
    state::{ApplyContext, Component, Entry, EntryKind, Payload, Reply},
    unhex, wall_clock_ms,
};
use crate::messaging::{
    ConsumerConfig, Delivery, Error, Message, Record, Stream, StreamConfig,
    stream::{BackendFuture, Operation, Outcome, StreamBackend},
    subject_matches,
};
use bytes::{BufMut, Bytes, BytesMut};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, time::Instant};

/// The largest payload a stream entry carries, whatever a stream allows.
const MAX_PAYLOAD: usize = crate::messaging::quic::MAX_PAYLOAD_BYTES;
/// Records read from the replica per lock.
const READ_BATCH: usize = 256;
/// How long an operation keeps looking for a leader.
const SERVE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a request waits for a new leader to catch up.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const VERSION: u32 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS bus_streams (
    stream TEXT PRIMARY KEY NOT NULL,
    next_sequence INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    count INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS bus_stream_records (
    stream TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    time_ms INTEGER NOT NULL,
    subject TEXT NOT NULL,
    reply TEXT,
    payload BLOB NOT NULL,
    size INTEGER NOT NULL,
    PRIMARY KEY (stream, sequence)
) STRICT;
CREATE TABLE IF NOT EXISTS bus_stream_consumers (
    stream TEXT NOT NULL,
    name TEXT NOT NULL,
    filter TEXT NOT NULL,
    ack_wait_ms INTEGER NOT NULL,
    max_ack_pending INTEGER NOT NULL,
    start_sequence INTEGER NOT NULL,
    floor INTEGER NOT NULL,
    PRIMARY KEY (stream, name)
) STRICT;
CREATE TABLE IF NOT EXISTS bus_stream_acks (
    stream TEXT NOT NULL,
    name TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    PRIMARY KEY (stream, name, sequence)
) STRICT;";

/// The content of a `stream` entry (version 1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum StreamEntry {
    /// Append a record and apply retention with the leader's limits.
    Append {
        stream: String,
        subject: String,
        reply: Option<String>,
        /// Hex.
        payload: String,
        max_messages: u64,
        max_bytes: u64,
    },
    /// Create a durable consumer, or confirm an identical one.
    Consumer {
        stream: String,
        name: String,
        filter: String,
        ack_wait_ms: u64,
        max_ack_pending: u64,
        start_sequence: u64,
        max_consumers: u64,
        scope: Option<(String, u64)>,
    },
    DeleteConsumer {
        stream: String,
        name: String,
    },
    Ack {
        stream: String,
        name: String,
        sequence: u64,
    },
}

impl StreamEntry {
    fn entry(&self) -> Entry {
        Entry::Stream(Payload::new(
            VERSION,
            serde_json::to_value(self).unwrap_or(serde_json::Value::Null),
        ))
    }
}

// Refusals carry a code, so the leader can hand the caller the error a
// single Rift would have returned.
const INVALID_CONFIG: &str = "invalid_config: ";
const NOT_FOUND: &str = "consumer_not_found: ";
const TOO_LARGE: &str = "payload_too_large: ";

fn refusal(message: String) -> Error {
    if let Some(rest) = message.strip_prefix(INVALID_CONFIG) {
        Error::InvalidConfig(rest.to_owned())
    } else if message.starts_with(NOT_FOUND) {
        Error::ConsumerNotFound
    } else if let Some(rest) = message.strip_prefix(TOO_LARGE) {
        let mut numbers = rest
            .split(' ')
            .filter_map(|part| part.parse::<usize>().ok());
        match (numbers.next(), numbers.next()) {
            (Some(size), Some(limit)) => Error::PayloadTooLarge { size, limit },
            _ => Error::Storage(message),
        }
    } else {
        Error::Storage(message)
    }
}

/// The `stream` entry kind's component.
pub(super) struct StreamMachine;

impl Component for StreamMachine {
    fn name(&self) -> &'static str {
        "streams"
    }

    fn kinds(&self) -> Vec<EntryKind> {
        vec![EntryKind::Stream]
    }

    fn apply(&self, db: &Connection, entry: &Entry, ctx: &mut ApplyContext) -> io::Result<Reply> {
        let Entry::Stream(payload) = entry else {
            return Ok(Reply::err(format!(
                "component streams does not apply {} entries",
                entry.kind()
            )));
        };
        if payload.v != VERSION {
            return Ok(Reply::err(format!(
                "stream entry version {} is not supported by this peer",
                payload.v
            )));
        }
        let entry: StreamEntry = match serde_json::from_value(payload.data.clone()) {
            Ok(entry) => entry,
            Err(error) => return Ok(Reply::err(format!("malformed stream entry: {error}"))),
        };
        db.execute_batch(SCHEMA).map_err(sql_error)?;
        match entry {
            StreamEntry::Append {
                stream,
                subject,
                reply,
                payload,
                max_messages,
                max_bytes,
            } => append(
                db,
                ctx,
                &stream,
                &subject,
                reply.as_deref(),
                &payload,
                (max_messages, max_bytes),
            ),
            StreamEntry::Consumer {
                stream,
                name,
                filter,
                ack_wait_ms,
                max_ack_pending,
                start_sequence,
                max_consumers,
                scope,
            } => create_consumer(
                db,
                ctx,
                &stream,
                &name,
                &filter,
                [ack_wait_ms, max_ack_pending, start_sequence, max_consumers],
                scope,
            ),
            StreamEntry::DeleteConsumer { stream, name } => {
                let deleted = db
                    .execute(
                        "DELETE FROM bus_stream_consumers WHERE stream = ?1 AND name = ?2",
                        params![stream, name],
                    )
                    .map_err(sql_error)?;
                if deleted == 0 {
                    return Ok(Reply::err(format!("{NOT_FOUND}consumer does not exist")));
                }
                db.execute(
                    "DELETE FROM bus_stream_acks WHERE stream = ?1 AND name = ?2",
                    params![stream, name],
                )
                .map_err(sql_error)?;
                ctx.changes
                    .touch("bus_stream_consumers", format!("{stream}/{name}"));
                Ok(Reply::ok(true))
            }
            StreamEntry::Ack {
                stream,
                name,
                sequence,
            } => acknowledge(db, ctx, &stream, &name, sequence),
        }
    }
}

fn sql(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn message_size(subject: &str, reply: Option<&str>, payload: usize) -> usize {
    payload + subject.len() + reply.map_or(0, str::len) + 32
}

fn first_sequence(db: &Connection, stream: &str) -> io::Result<Option<u64>> {
    let first: Option<i64> = db
        .query_row(
            "SELECT MIN(sequence) FROM bus_stream_records WHERE stream = ?1",
            [stream],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    Ok(first.map(unsigned))
}

fn next_sequence(db: &Connection, stream: &str) -> io::Result<u64> {
    let next: Option<i64> = db
        .query_row(
            "SELECT next_sequence FROM bus_streams WHERE stream = ?1",
            [stream],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?;
    Ok(next.map_or(1, unsigned))
}

fn append(
    db: &Connection,
    ctx: &mut ApplyContext,
    stream: &str,
    subject: &str,
    reply: Option<&str>,
    payload: &str,
    (max_messages, max_bytes): (u64, u64),
) -> io::Result<Reply> {
    let Some(payload) = unhex(payload) else {
        return Ok(Reply::err(format!(
            "{INVALID_CONFIG}the payload is not hex"
        )));
    };
    if payload.len() > MAX_PAYLOAD {
        return Ok(Reply::err(format!(
            "{TOO_LARGE}{} {MAX_PAYLOAD}",
            payload.len()
        )));
    }
    let size = message_size(subject, reply, payload.len());
    if size as u64 > max_bytes {
        return Ok(Reply::err(format!("{TOO_LARGE}{size} {max_bytes}")));
    }
    let current: Option<(i64, i64, i64)> = db
        .query_row(
            "SELECT next_sequence, bytes, count FROM bus_streams WHERE stream = ?1",
            [stream],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let (sequence, mut bytes, mut count) = current.unwrap_or((1, 0, 0));
    let time = sql(ctx.time_ms);
    let size = i64::try_from(size).unwrap_or(i64::MAX);
    db.execute(
        "INSERT INTO bus_stream_records (stream, sequence, time_ms, subject, reply, payload, size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![stream, sequence, time, subject, reply, payload, size],
    )
    .map_err(sql_error)?;
    bytes += size;
    count += 1;
    let mut evicted = false;
    while count > sql(max_messages) || bytes > sql(max_bytes) {
        let oldest: Option<(i64, i64)> = db
            .query_row(
                "SELECT sequence, size FROM bus_stream_records WHERE stream = ?1
                 ORDER BY sequence LIMIT 1",
                [stream],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql_error)?;
        let Some((oldest, oldest_size)) = oldest else {
            break;
        };
        db.execute(
            "DELETE FROM bus_stream_records WHERE stream = ?1 AND sequence = ?2",
            params![stream, oldest],
        )
        .map_err(sql_error)?;
        bytes -= oldest_size;
        count -= 1;
        evicted = true;
    }
    db.execute(
        "INSERT INTO bus_streams (stream, next_sequence, bytes, count) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (stream) DO UPDATE SET next_sequence = excluded.next_sequence,
         bytes = excluded.bytes, count = excluded.count",
        params![stream, sequence + 1, bytes, count],
    )
    .map_err(sql_error)?;
    if evicted {
        // Records that are gone are neither pending nor acknowledged.
        let first = first_sequence(db, stream)?.map_or(sequence + 1, sql);
        db.execute(
            "UPDATE bus_stream_consumers SET floor = ?2 WHERE stream = ?1 AND floor < ?2",
            params![stream, first],
        )
        .map_err(sql_error)?;
        db.execute(
            "DELETE FROM bus_stream_acks WHERE stream = ?1 AND sequence < ?2",
            params![stream, first],
        )
        .map_err(sql_error)?;
    }
    ctx.changes.touch("bus_stream_records", stream);
    Ok(Reply::ok(serde_json::json!({
        "sequence": sequence,
        "time_ms": ctx.time_ms,
    })))
}

fn create_consumer(
    db: &Connection,
    ctx: &mut ApplyContext,
    stream: &str,
    name: &str,
    filter: &str,
    [ack_wait_ms, max_ack_pending, start_sequence, max_consumers]: [u64; 4],
    scope: Option<(String, u64)>,
) -> io::Result<Reply> {
    let existing: Option<(String, i64, i64, i64)> = db
        .query_row(
            "SELECT filter, ack_wait_ms, max_ack_pending, start_sequence
             FROM bus_stream_consumers WHERE stream = ?1 AND name = ?2",
            params![stream, name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let settings = (
        filter.to_owned(),
        sql(ack_wait_ms),
        sql(max_ack_pending),
        sql(start_sequence),
    );
    if let Some(existing) = existing {
        if existing != settings {
            return Ok(Reply::err(format!(
                "{INVALID_CONFIG}consumer already exists with different settings"
            )));
        }
        return Ok(Reply::ok(false));
    }
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM bus_stream_consumers WHERE stream = ?1",
            [stream],
            |row| row.get(0),
        )
        .map_err(sql_error)?;
    if count >= sql(max_consumers) {
        return Ok(Reply::err(format!(
            "{INVALID_CONFIG}stream consumer limit reached"
        )));
    }
    if let Some((prefix, limit)) = scope {
        let owned: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM bus_stream_consumers
                 WHERE stream = ?1 AND substr(name, 1, length(?2)) = ?2",
                params![stream, prefix],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if owned >= sql(limit) {
            return Ok(Reply::err(format!(
                "{INVALID_CONFIG}consumer limit for this identity reached; delete unused consumers"
            )));
        }
    }
    db.execute(
        "INSERT INTO bus_stream_consumers
         (stream, name, filter, ack_wait_ms, max_ack_pending, start_sequence, floor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![stream, name, settings.0, settings.1, settings.2, settings.3],
    )
    .map_err(sql_error)?;
    ctx.changes
        .touch("bus_stream_consumers", format!("{stream}/{name}"));
    Ok(Reply::ok(true))
}

/// Record an acknowledgement and move the consumer's floor past every
/// record below it that is acknowledged, gone or not for this consumer.
fn acknowledge(
    db: &Connection,
    ctx: &mut ApplyContext,
    stream: &str,
    name: &str,
    sequence: u64,
) -> io::Result<Reply> {
    let row: Option<(String, i64)> = db
        .query_row(
            "SELECT filter, floor FROM bus_stream_consumers WHERE stream = ?1 AND name = ?2",
            params![stream, name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(sql_error)?;
    let Some((filter, floor)) = row else {
        return Ok(Reply::err(format!("{NOT_FOUND}consumer does not exist")));
    };
    let mut floor = unsigned(floor);
    if sequence < floor {
        return Ok(Reply::ok(false));
    }
    db.execute(
        "INSERT OR IGNORE INTO bus_stream_acks (stream, name, sequence) VALUES (?1, ?2, ?3)",
        params![stream, name, sql(sequence)],
    )
    .map_err(sql_error)?;
    let next = next_sequence(db, stream)?;
    let first = first_sequence(db, stream)?;
    while floor < next {
        let acked: bool = db
            .query_row(
                "SELECT COUNT(*) > 0 FROM bus_stream_acks
                 WHERE stream = ?1 AND name = ?2 AND sequence = ?3",
                params![stream, name, sql(floor)],
                |row| row.get(0),
            )
            .map_err(sql_error)?;
        if acked {
            floor += 1;
            continue;
        }
        match first {
            None => {
                floor = next;
                break;
            }
            Some(first) if floor < first => {
                floor = first;
                continue;
            }
            Some(_) => {}
        }
        let subject: Option<String> = db
            .query_row(
                "SELECT subject FROM bus_stream_records WHERE stream = ?1 AND sequence = ?2",
                params![stream, sql(floor)],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql_error)?;
        match subject {
            Some(subject) if !subject_matches(&filter, &subject) => floor += 1,
            _ => break,
        }
    }
    db.execute(
        "UPDATE bus_stream_consumers SET floor = ?3 WHERE stream = ?1 AND name = ?2",
        params![stream, name, sql(floor)],
    )
    .map_err(sql_error)?;
    db.execute(
        "DELETE FROM bus_stream_acks WHERE stream = ?1 AND name = ?2 AND sequence < ?3",
        params![stream, name, sql(floor)],
    )
    .map_err(sql_error)?;
    ctx.changes
        .touch("bus_stream_consumers", format!("{stream}/{name}"));
    Ok(Reply::ok(true))
}

/// Whether the stream tables exist yet.
fn has_tables(db: &Connection) -> io::Result<bool> {
    super::tables::table_exists(db, "bus_stream_consumers")
}

struct ConsumerRow {
    filter: String,
    ack_wait_ms: u64,
    max_ack_pending: usize,
    floor: u64,
}

fn consumer_row(db: &Connection, stream: &str, name: &str) -> io::Result<Option<ConsumerRow>> {
    if !has_tables(db)? {
        return Ok(None);
    }
    db.query_row(
        "SELECT filter, ack_wait_ms, max_ack_pending, floor FROM bus_stream_consumers
         WHERE stream = ?1 AND name = ?2",
        params![stream, name],
        |row| {
            Ok(ConsumerRow {
                filter: row.get(0)?,
                ack_wait_ms: unsigned(row.get(1)?),
                max_ack_pending: usize::try_from(unsigned(row.get(2)?)).unwrap_or(usize::MAX),
                floor: unsigned(row.get(3)?),
            })
        },
    )
    .optional()
    .map_err(sql_error)
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    let subject: String = row.get(2)?;
    let reply: Option<String> = row.get(3)?;
    let payload: Vec<u8> = row.get(4)?;
    Ok(Record {
        sequence: unsigned(row.get(0)?),
        timestamp_millis: unsigned(row.get(1)?),
        message: Message {
            subject: Arc::from(subject),
            reply: reply.map(Arc::from),
            payload: Bytes::from(payload),
            sender: None,
        },
    })
}

fn read_records(db: &Connection, stream: &str, from: u64, limit: usize) -> io::Result<Vec<Record>> {
    if !has_tables(db)? {
        return Ok(Vec::new());
    }
    let mut statement = db
        .prepare_cached(
            "SELECT sequence, time_ms, subject, reply, payload FROM bus_stream_records
             WHERE stream = ?1 AND sequence >= ?2 ORDER BY sequence LIMIT ?3",
        )
        .map_err(sql_error)?;
    let rows = statement
        .query_map(
            params![stream, sql(from), i64::try_from(limit).unwrap_or(i64::MAX)],
            record_from_row,
        )
        .map_err(sql_error)?;
    let mut records = Vec::new();
    for row in rows {
        records.push(row.map_err(sql_error)?);
    }
    Ok(records)
}

fn read_record(db: &Connection, stream: &str, sequence: u64) -> io::Result<Option<Record>> {
    Ok(read_records(db, stream, sequence, 1)?
        .into_iter()
        .next()
        .filter(|record| record.sequence == sequence))
}

fn acked(db: &Connection, stream: &str, name: &str) -> io::Result<BTreeSet<u64>> {
    if !has_tables(db)? {
        return Ok(BTreeSet::new());
    }
    let mut statement = db
        .prepare_cached("SELECT sequence FROM bus_stream_acks WHERE stream = ?1 AND name = ?2")
        .map_err(sql_error)?;
    let rows = statement
        .query_map(params![stream, name], |row| row.get::<_, i64>(0))
        .map_err(sql_error)?;
    let mut acked = BTreeSet::new();
    for row in rows {
        acked.insert(unsigned(row.map_err(sql_error)?));
    }
    Ok(acked)
}

struct Pending {
    delivered_at: u64,
    attempts: u32,
}

/// What the leader knows about one consumer beyond the replicated state.
struct LeaderConsumer {
    /// The next record not yet looked at.
    cursor: u64,
    pending: BTreeMap<u64, Pending>,
}

#[derive(Default)]
struct LeaderStream {
    consumers: HashMap<String, LeaderConsumer>,
}

/// One term of this peer's leadership.
struct Term {
    term: u64,
    /// Set once this leader applied everything committed before it led.
    ready: watch::Sender<bool>,
    streams: Mutex<HashMap<String, Arc<tokio::sync::Mutex<LeaderStream>>>>,
}

impl Term {
    fn stream(&self, name: &str) -> Arc<tokio::sync::Mutex<LeaderStream>> {
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(name.to_owned())
            .or_default()
            .clone()
    }
}

/// Why an operation did not run.
enum Failure {
    /// Not here, or not yet: ask the leader again.
    Retry(String),
    /// It ran, or must not run again: tell the caller.
    Error(Error),
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Error(error)
    }
}

fn storage(error: io::Error) -> Failure {
    Failure::Error(Error::Storage(error.to_string()))
}

/// The stream service. Every peer has one; it serves while its peer leads.
pub(super) struct Service {
    state: Arc<BusState>,
    streams: HashMap<String, Stream>,
    term: Mutex<Option<Arc<Term>>>,
}

impl Service {
    pub(super) fn new(state: Arc<BusState>, streams: HashMap<String, Stream>) -> Arc<Self> {
        Arc::new(Self {
            state,
            streams,
            term: Mutex::new(None),
        })
    }

    /// This peer leads in `context.term`: empty the memory streams, catch
    /// up with everything committed, then serve.
    pub(super) async fn gain(&self, context: LeaderContext) {
        let term = Arc::new(Term {
            term: context.term,
            ready: watch::Sender::new(false),
            streams: Mutex::new(HashMap::new()),
        });
        *self.term.lock().unwrap_or_else(|e| e.into_inner()) = Some(term.clone());
        for stream in self.streams.values() {
            stream.reset_memory();
        }
        loop {
            match context.cluster().read_barrier().await {
                Ok(_) => {
                    term.ready.send_replace(true);
                    return;
                }
                Err(error) => {
                    log_debug!("cluster bus: stream service catching up: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    pub(super) fn end(&self, term: u64) {
        let mut current = self.term.lock().unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|current| current.term == term) {
            *current = None;
        }
    }

    /// The current term, once this peer leads and caught up.
    async fn leading(&self, cluster: &Cluster) -> Result<Arc<Term>, Failure> {
        let term = self
            .term
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| Failure::Retry("this peer does not lead".into()))?;
        if !cluster.is_leader_with_lease() || cluster.current_term() != term.term {
            return Err(Failure::Retry("this peer does not lead".into()));
        }
        let mut ready = term.ready.subscribe();
        let wait = async {
            loop {
                if *ready.borrow_and_update() {
                    return true;
                }
                if ready.changed().await.is_err() {
                    return false;
                }
            }
        };
        match tokio::time::timeout(READY_TIMEOUT, wait).await {
            Ok(true) => Ok(term),
            _ => Err(Failure::Retry("the leader is still catching up".into())),
        }
    }

    /// Run `operation` here, as the leader.
    async fn serve(&self, name: &str, operation: Operation) -> Result<Outcome, Failure> {
        let cluster = self
            .state
            .cluster()
            .ok_or_else(|| Failure::Retry("the cluster stopped".into()))?;
        let term = self.leading(&cluster).await?;
        let stream = self.streams.get(name).ok_or_else(|| {
            Failure::Error(Error::InvalidConfig(format!(
                "the leader has no stream {name:?}; every peer must configure the same streams"
            )))
        })?;
        if !stream.is_durable() {
            return Ok(stream.execute_local(operation).await?);
        }
        let config = stream.config();
        match operation {
            Operation::Publish {
                subject,
                reply,
                payload,
            } => {
                self.publish(&cluster, name, config, subject, reply, payload)
                    .await
            }
            Operation::Replay {
                from_sequence,
                limit,
            } => {
                let mut records = Vec::new();
                let mut from = from_sequence;
                while records.len() < limit {
                    let batch = cluster
                        .state()
                        .read(|db| {
                            read_records(db, name, from, READ_BATCH.min(limit - records.len()))
                        })
                        .map_err(storage)?;
                    let Some(last) = batch.last() else { break };
                    from = last.sequence + 1;
                    records.extend(batch);
                }
                Ok(Outcome::Records(records))
            }
            Operation::OpenConsumer {
                name: consumer,
                config: settings,
                scope,
            } => {
                let entry = StreamEntry::Consumer {
                    stream: name.to_owned(),
                    name: consumer,
                    filter: settings.filter_subject,
                    ack_wait_ms: u64::try_from(settings.ack_wait.as_millis()).unwrap_or(u64::MAX),
                    max_ack_pending: settings.max_ack_pending as u64,
                    start_sequence: settings.start_sequence,
                    max_consumers: config.max_consumers as u64,
                    scope: scope.map(|(prefix, limit)| (prefix, limit as u64)),
                };
                propose(&cluster, &entry).await?;
                Ok(Outcome::Done)
            }
            Operation::GetConsumer { name: consumer } => {
                match cluster
                    .state()
                    .read(|db| consumer_row(db, name, &consumer))
                    .map_err(storage)?
                {
                    Some(_) => Ok(Outcome::Done),
                    None => Err(Failure::Error(Error::ConsumerNotFound)),
                }
            }
            Operation::DeleteConsumer { name: consumer } => {
                let leader = term.stream(name);
                let mut leader = leader.lock().await;
                let entry = StreamEntry::DeleteConsumer {
                    stream: name.to_owned(),
                    name: consumer.clone(),
                };
                propose(&cluster, &entry).await?;
                leader.consumers.remove(&consumer);
                Ok(Outcome::Done)
            }
            Operation::Fetch { consumer, limit } => self
                .fetch(&cluster, &term, name, &consumer, limit)
                .await
                .map(Outcome::Deliveries),
            Operation::Ack { consumer, sequence } => {
                self.ack(&cluster, &term, name, &consumer, sequence).await?;
                Ok(Outcome::Done)
            }
        }
    }

    async fn publish(
        &self,
        cluster: &Cluster,
        name: &str,
        config: &StreamConfig,
        subject: String,
        reply: Option<String>,
        payload: Bytes,
    ) -> Result<Outcome, Failure> {
        let entry = StreamEntry::Append {
            stream: name.to_owned(),
            subject: subject.clone(),
            reply: reply.clone(),
            payload: hex(&payload),
            max_messages: config.max_messages as u64,
            max_bytes: config.max_bytes as u64,
        };
        let value = propose(cluster, &entry).await?;
        let sequence = value["sequence"].as_u64().ok_or_else(|| {
            Failure::Error(Error::Storage("the append returned no sequence".into()))
        })?;
        Ok(Outcome::Record(Record {
            sequence,
            timestamp_millis: value["time_ms"].as_u64().unwrap_or(0),
            message: Message {
                subject: Arc::from(subject),
                reply: reply.map(Arc::from),
                payload,
                sender: None,
            },
        }))
    }

    /// Overdue pending records first, then new matching ones, as on a
    /// single Rift.
    async fn fetch(
        &self,
        cluster: &Cluster,
        term: &Term,
        name: &str,
        consumer: &str,
        limit: usize,
    ) -> Result<Vec<Delivery>, Failure> {
        let leader = term.stream(name);
        let mut leader = leader.lock().await;
        let state = cluster.state();
        let row = state
            .read(|db| consumer_row(db, name, consumer))
            .map_err(storage)?
            .ok_or(Failure::Error(Error::ConsumerNotFound))?;
        let (first, acked) = state
            .read(|db| Ok((first_sequence(db, name)?, acked(db, name, consumer)?)))
            .map_err(storage)?;
        let entry = leader
            .consumers
            .entry(consumer.to_owned())
            .or_insert_with(|| LeaderConsumer {
                cursor: row.floor,
                pending: BTreeMap::new(),
            });
        entry.pending.retain(|sequence, _| {
            *sequence >= row.floor
                && first.is_some_and(|first| *sequence >= first)
                && !acked.contains(sequence)
        });
        let now = wall_clock_ms();
        let mut result = Vec::new();
        let overdue: Vec<u64> = entry
            .pending
            .iter()
            .filter(|(_, pending)| {
                pending.delivered_at == 0
                    || now.saturating_sub(pending.delivered_at) >= row.ack_wait_ms
            })
            .map(|(sequence, _)| *sequence)
            .take(limit)
            .collect();
        for sequence in overdue {
            let record = state
                .read(|db| read_record(db, name, sequence))
                .map_err(storage)?;
            match (record, entry.pending.get_mut(&sequence)) {
                (Some(record), Some(pending)) => {
                    pending.attempts = pending.attempts.saturating_add(1);
                    pending.delivered_at = now;
                    result.push(Delivery {
                        record,
                        delivery_count: pending.attempts,
                    });
                }
                _ => {
                    entry.pending.remove(&sequence);
                }
            }
        }
        entry.cursor = entry.cursor.max(row.floor);
        if let Some(first) = first {
            entry.cursor = entry.cursor.max(first);
        }
        'records: while result.len() < limit && entry.pending.len() < row.max_ack_pending {
            let from = entry.cursor;
            let batch = state
                .read(|db| read_records(db, name, from, READ_BATCH))
                .map_err(storage)?;
            if batch.is_empty() {
                break;
            }
            for record in batch {
                if result.len() == limit || entry.pending.len() >= row.max_ack_pending {
                    break 'records;
                }
                entry.cursor = record.sequence + 1;
                if acked.contains(&record.sequence)
                    || entry.pending.contains_key(&record.sequence)
                    || !subject_matches(&row.filter, &record.message.subject)
                {
                    continue;
                }
                entry.pending.insert(
                    record.sequence,
                    Pending {
                        delivered_at: now,
                        attempts: 1,
                    },
                );
                result.push(Delivery {
                    record,
                    delivery_count: 1,
                });
            }
        }
        Ok(result)
    }

    /// Acknowledge a pending record. After a leader change nothing is
    /// pending here yet, so an unacknowledged record of this consumer that
    /// is still retained is accepted too: the previous leader may have
    /// delivered it.
    async fn ack(
        &self,
        cluster: &Cluster,
        term: &Term,
        name: &str,
        consumer: &str,
        sequence: u64,
    ) -> Result<(), Failure> {
        let leader = term.stream(name);
        let mut leader = leader.lock().await;
        let state = cluster.state();
        let row = state
            .read(|db| consumer_row(db, name, consumer))
            .map_err(storage)?
            .ok_or(Failure::Error(Error::ConsumerNotFound))?;
        let pending = leader
            .consumers
            .get(consumer)
            .is_some_and(|entry| entry.pending.contains_key(&sequence));
        if !pending {
            let (record, acked) = state
                .read(|db| Ok((read_record(db, name, sequence)?, acked(db, name, consumer)?)))
                .map_err(storage)?;
            let acceptable = sequence >= row.floor
                && !acked.contains(&sequence)
                && record
                    .is_some_and(|record| subject_matches(&row.filter, &record.message.subject));
            if !acceptable {
                return Err(Failure::Error(Error::InvalidAck));
            }
        }
        let entry = StreamEntry::Ack {
            stream: name.to_owned(),
            name: consumer.to_owned(),
            sequence,
        };
        propose(cluster, &entry).await?;
        if let Some(entry) = leader.consumers.get_mut(consumer) {
            entry.pending.remove(&sequence);
        }
        Ok(())
    }

    /// Run `operation` on the leader, wherever it is.
    async fn run(&self, name: &str, operation: Operation) -> Result<Outcome, Error> {
        let deadline = Instant::now() + SERVE_TIMEOUT;
        let mut delay = Duration::from_millis(25);
        loop {
            let cluster = self.state.cluster().ok_or(Error::Closed)?;
            let leadership = cluster.leadership();
            let attempt = if leadership.is_leader {
                self.serve(name, operation.clone()).await
            } else if let Some(leader) = leadership.leader_name {
                forward(&cluster, &leader, name, &operation, deadline).await
            } else {
                Err(Failure::Retry("there is no leader".into()))
            };
            match attempt {
                Ok(outcome) => return Ok(outcome),
                Err(Failure::Error(error)) => return Err(error),
                Err(Failure::Retry(reason)) => {
                    if Instant::now() + delay >= deadline {
                        return Err(Error::Storage(format!(
                            "the stream service is unavailable: {reason}"
                        )));
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(500));
                }
            }
        }
    }

    /// Answer a request another peer forwarded on the `bus` stream.
    pub(super) async fn handle(&self, body: Bytes) -> Bytes {
        let response = match serde_json::from_slice::<StreamRequest>(&body) {
            Err(error) => StreamResponse::Failed {
                error: WireError::Storage {
                    message: format!("malformed stream request: {error}"),
                },
            },
            Ok(request) => match from_wire(request.op) {
                Err(error) => StreamResponse::Failed {
                    error: wire_error(&error),
                },
                Ok(operation) => match self.serve(&request.stream, operation).await {
                    Ok(outcome) => outcome_to_wire(outcome),
                    Err(Failure::Retry(reason)) => StreamResponse::NotLeader { reason },
                    Err(Failure::Error(error)) => StreamResponse::Failed {
                        error: wire_error(&error),
                    },
                },
            },
        };
        serde_json::to_vec(&response)
            .map(Bytes::from)
            .unwrap_or_default()
    }
}

/// Propose a stream entry from the leader; a refusal is the caller's error.
async fn propose(cluster: &Cluster, entry: &StreamEntry) -> Result<serde_json::Value, Failure> {
    match cluster.propose(entry.entry()).await {
        Ok(Reply::Ok(value)) => Ok(value),
        Ok(Reply::Err(message)) => Err(Failure::Error(refusal(message))),
        Err(error) => Err(Failure::Error(Error::Storage(format!(
            "the stream entry was not committed: {error}"
        )))),
    }
}

/// Send `operation` to the leader `leader`.
async fn forward(
    cluster: &Cluster,
    leader: &str,
    stream: &str,
    operation: &Operation,
    deadline: Instant,
) -> Result<Outcome, Failure> {
    let request = StreamRequest {
        stream: stream.to_owned(),
        op: to_wire(operation),
    };
    let json = serde_json::to_vec(&request)
        .map_err(|error| Failure::Error(Error::Storage(error.to_string())))?;
    let mut body = BytesMut::with_capacity(json.len() + 1);
    body.put_u8(STREAMS);
    body.put_slice(&json);
    let timeout = deadline.saturating_duration_since(Instant::now());
    let publish = matches!(operation, Operation::Publish { .. });
    match cluster
        .links()
        .request(leader, StreamKind::Bus, body.freeze(), timeout)
        .await
    {
        Ok(reply) => {
            let response: StreamResponse = serde_json::from_slice(&reply).map_err(|error| {
                Failure::Error(Error::Storage(format!(
                    "malformed answer from the stream service: {error}"
                )))
            })?;
            outcome_from_wire(response)
        }
        Err(error) if remote_error(&error).is_some() => {
            Err(Failure::Error(Error::Storage(error.to_string())))
        }
        Err(error) => {
            // A publication that may have reached the leader is not sent
            // twice; everything else is safe to ask again.
            let unsent = matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::Unsupported
            );
            if publish && !unsent {
                Err(Failure::Error(if error.kind() == io::ErrorKind::TimedOut {
                    Error::Timeout
                } else {
                    Error::Io(error)
                }))
            } else {
                Err(Failure::Retry(error.to_string()))
            }
        }
    }
}

/// Routes a peer's stream handles to the stream service.
pub(super) struct Backend {
    service: Arc<Service>,
}

impl Backend {
    pub(super) fn new(service: Arc<Service>) -> Self {
        Self { service }
    }
}

impl StreamBackend for Backend {
    fn run(&self, stream: Arc<str>, operation: Operation) -> BackendFuture {
        let service = self.service.clone();
        Box::pin(async move { service.run(&stream, operation).await })
    }
}

#[derive(Serialize, Deserialize)]
struct StreamRequest {
    stream: String,
    op: WireOperation,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WireOperation {
    Publish {
        subject: String,
        reply: Option<String>,
        payload: String,
    },
    Replay {
        from_sequence: u64,
        limit: usize,
    },
    OpenConsumer {
        name: String,
        filter: String,
        ack_wait_ms: u64,
        max_ack_pending: usize,
        start_sequence: u64,
        scope: Option<(String, usize)>,
    },
    GetConsumer {
        name: String,
    },
    DeleteConsumer {
        name: String,
    },
    Fetch {
        consumer: String,
        limit: usize,
    },
    Ack {
        consumer: String,
        sequence: u64,
    },
}

#[derive(Serialize, Deserialize)]
struct WireRecord {
    sequence: u64,
    time_ms: u64,
    subject: String,
    reply: Option<String>,
    payload: String,
}

#[derive(Serialize, Deserialize)]
struct WireDelivery {
    record: WireRecord,
    delivery_count: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum StreamResponse {
    Record { record: WireRecord },
    Records { records: Vec<WireRecord> },
    Deliveries { deliveries: Vec<WireDelivery> },
    Done,
    NotLeader { reason: String },
    Failed { error: WireError },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
enum WireError {
    InvalidSubject { message: String },
    InvalidConfig { message: String },
    PayloadTooLarge { size: usize, limit: usize },
    StreamNotMatched,
    ConsumerNotFound,
    InvalidAck,
    Storage { message: String },
}

fn wire_error(error: &Error) -> WireError {
    match error {
        Error::InvalidSubject(message) => WireError::InvalidSubject {
            message: message.clone(),
        },
        Error::InvalidConfig(message) => WireError::InvalidConfig {
            message: message.clone(),
        },
        Error::PayloadTooLarge { size, limit } => WireError::PayloadTooLarge {
            size: *size,
            limit: *limit,
        },
        Error::StreamNotMatched => WireError::StreamNotMatched,
        Error::ConsumerNotFound => WireError::ConsumerNotFound,
        Error::InvalidAck => WireError::InvalidAck,
        other => WireError::Storage {
            message: other.to_string(),
        },
    }
}

fn error_from_wire(error: WireError) -> Error {
    match error {
        WireError::InvalidSubject { message } => Error::InvalidSubject(message),
        WireError::InvalidConfig { message } => Error::InvalidConfig(message),
        WireError::PayloadTooLarge { size, limit } => Error::PayloadTooLarge { size, limit },
        WireError::StreamNotMatched => Error::StreamNotMatched,
        WireError::ConsumerNotFound => Error::ConsumerNotFound,
        WireError::InvalidAck => Error::InvalidAck,
        WireError::Storage { message } => Error::Storage(message),
    }
}

fn record_to_wire(record: Record) -> WireRecord {
    WireRecord {
        sequence: record.sequence,
        time_ms: record.timestamp_millis,
        subject: record.message.subject.to_string(),
        reply: record.message.reply.as_deref().map(str::to_owned),
        payload: hex(&record.message.payload),
    }
}

fn record_from_wire(record: WireRecord) -> Result<Record, Failure> {
    let payload = unhex(&record.payload)
        .ok_or_else(|| Failure::Error(Error::Storage("a record payload is not hex".into())))?;
    Ok(Record {
        sequence: record.sequence,
        timestamp_millis: record.time_ms,
        message: Message {
            subject: Arc::from(record.subject),
            reply: record.reply.map(Arc::from),
            payload: Bytes::from(payload),
            sender: None,
        },
    })
}

fn to_wire(operation: &Operation) -> WireOperation {
    match operation.clone() {
        Operation::Publish {
            subject,
            reply,
            payload,
        } => WireOperation::Publish {
            subject,
            reply,
            payload: hex(&payload),
        },
        Operation::Replay {
            from_sequence,
            limit,
        } => WireOperation::Replay {
            from_sequence,
            limit,
        },
        Operation::OpenConsumer {
            name,
            config,
            scope,
        } => WireOperation::OpenConsumer {
            name,
            filter: config.filter_subject,
            ack_wait_ms: u64::try_from(config.ack_wait.as_millis()).unwrap_or(u64::MAX),
            max_ack_pending: config.max_ack_pending,
            start_sequence: config.start_sequence,
            scope,
        },
        Operation::GetConsumer { name } => WireOperation::GetConsumer { name },
        Operation::DeleteConsumer { name } => WireOperation::DeleteConsumer { name },
        Operation::Fetch { consumer, limit } => WireOperation::Fetch { consumer, limit },
        Operation::Ack { consumer, sequence } => WireOperation::Ack { consumer, sequence },
    }
}

fn from_wire(operation: WireOperation) -> Result<Operation, Error> {
    Ok(match operation {
        WireOperation::Publish {
            subject,
            reply,
            payload,
        } => Operation::Publish {
            subject,
            reply,
            payload: Bytes::from(
                unhex(&payload).ok_or_else(|| Error::Storage("the payload is not hex".into()))?,
            ),
        },
        WireOperation::Replay {
            from_sequence,
            limit,
        } => Operation::Replay {
            from_sequence,
            limit,
        },
        WireOperation::OpenConsumer {
            name,
            filter,
            ack_wait_ms,
            max_ack_pending,
            start_sequence,
            scope,
        } => Operation::OpenConsumer {
            name,
            config: ConsumerConfig {
                filter_subject: filter,
                ack_wait: Duration::from_millis(ack_wait_ms),
                max_ack_pending,
                start_sequence,
            },
            scope,
        },
        WireOperation::GetConsumer { name } => Operation::GetConsumer { name },
        WireOperation::DeleteConsumer { name } => Operation::DeleteConsumer { name },
        WireOperation::Fetch { consumer, limit } => Operation::Fetch { consumer, limit },
        WireOperation::Ack { consumer, sequence } => Operation::Ack { consumer, sequence },
    })
}

fn outcome_to_wire(outcome: Outcome) -> StreamResponse {
    match outcome {
        Outcome::Record(record) => StreamResponse::Record {
            record: record_to_wire(record),
        },
        Outcome::Records(records) => StreamResponse::Records {
            records: records.into_iter().map(record_to_wire).collect(),
        },
        Outcome::Deliveries(deliveries) => StreamResponse::Deliveries {
            deliveries: deliveries
                .into_iter()
                .map(|delivery| WireDelivery {
                    record: record_to_wire(delivery.record),
                    delivery_count: delivery.delivery_count,
                })
                .collect(),
        },
        Outcome::Done => StreamResponse::Done,
    }
}

fn outcome_from_wire(response: StreamResponse) -> Result<Outcome, Failure> {
    match response {
        StreamResponse::Record { record } => Ok(Outcome::Record(record_from_wire(record)?)),
        StreamResponse::Records { records } => Ok(Outcome::Records(
            records
                .into_iter()
                .map(record_from_wire)
                .collect::<Result<_, _>>()?,
        )),
        StreamResponse::Deliveries { deliveries } => Ok(Outcome::Deliveries(
            deliveries
                .into_iter()
                .map(|delivery| {
                    Ok(Delivery {
                        record: record_from_wire(delivery.record)?,
                        delivery_count: delivery.delivery_count,
                    })
                })
                .collect::<Result<_, Failure>>()?,
        )),
        StreamResponse::Done => Ok(Outcome::Done),
        StreamResponse::NotLeader { reason } => Err(Failure::Retry(reason)),
        StreamResponse::Failed { error } => Err(Failure::Error(error_from_wire(error))),
    }
}
