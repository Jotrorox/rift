//! Bounded retained streams with optional crash-recoverable local storage.
//!
//! Core broker traffic does not touch this store. A durable publish is acknowledged
//! after its journal write (and `sync_data` when `sync_on_write` is enabled).
//! Consumers use explicit acknowledgements and at-least-once redelivery. Retention
//! can evict unacknowledged records; this is a bounded stream, not an unbounded log.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio::sync::Semaphore;

use super::{Error, Message, Result, subject_matches, validate_subject};

const MAGIC: &[u8; 8] = b"RIFTJS01";
const SUBJECT_LIMIT: usize = 1024;
const MAX_FETCH: usize = 65_536;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamConfig {
    pub subjects: Vec<String>,
    pub max_messages: usize,
    pub max_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_consumers: usize,
    /// When false, successful writes can be lost on machine/power failure.
    pub sync_on_write: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            subjects: vec![">".into()],
            max_messages: 100_000,
            max_bytes: 64 * 1024 * 1024,
            max_payload_bytes: 1024 * 1024,
            max_consumers: 128,
            sync_on_write: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsumerConfig {
    pub filter_subject: String,
    /// Positive whole milliseconds, persisted across restarts.
    pub ack_wait: Duration,
    pub max_ack_pending: usize,
    pub start_sequence: u64,
}

impl Default for ConsumerConfig {
    fn default() -> Self {
        Self {
            filter_subject: ">".into(),
            ack_wait: Duration::from_secs(30),
            max_ack_pending: 128,
            start_sequence: 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub sequence: u64,
    pub timestamp_millis: u64,
    pub message: Message,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub record: Record,
    pub delivery_count: u32,
}

#[derive(Clone)]
pub struct Stream {
    inner: Arc<Inner>,
}

struct Inner {
    config: StreamConfig,
    state: Mutex<State>,
    gate: Arc<Semaphore>,
    durable: bool,
}

#[derive(Clone)]
pub struct DurableConsumer {
    stream: Stream,
    name: Arc<str>,
}

struct State {
    config: StreamConfig,
    records: VecDeque<Record>,
    bytes: usize,
    next_sequence: u64,
    consumers: HashMap<String, ConsumerState>,
    journal: Option<Journal>,
}

#[derive(Clone)]
struct ConsumerState {
    config: ConsumerConfig,
    next_sequence: u64,
    pending: BTreeMap<u64, Pending>,
}

#[derive(Clone)]
struct Pending {
    delivered_at: u64,
    attempts: u32,
}

struct Journal {
    directory: PathBuf,
    file: File,
    _lock: File,
    bytes: u64,
    compacted_bytes: u64,
    failed: bool,
}

enum Event {
    Publish(Record),
    Consumer(String, ConsumerState),
    Fetch(String, u64, Vec<(u64, Pending)>),
    Ack(String, u64),
    NextSequence(u64),
}

impl Stream {
    pub fn memory(config: StreamConfig) -> Result<Self> {
        validate_config(&config)?;
        Ok(Self::from_state(State::empty(config), false))
    }

    /// Opens a directory exclusively. A second live opener fails instead of
    /// creating competing writers. An interrupted final journal write is removed.
    pub async fn open(path: impl AsRef<Path>, config: StreamConfig) -> Result<Self> {
        validate_config(&config)?;
        let path = path.as_ref().to_path_buf();
        let state = tokio::task::spawn_blocking(move || open_state(path, config))
            .await
            .map_err(|e| Error::Storage(format!("storage worker failed: {e}")))??;
        Ok(Self::from_state(state, true))
    }

    fn from_state(state: State, durable: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                config: state.config.clone(),
                state: Mutex::new(state),
                gate: Arc::new(Semaphore::new(1)),
                durable,
            }),
        }
    }

    pub fn config(&self) -> &StreamConfig {
        &self.inner.config
    }
    pub fn is_durable(&self) -> bool {
        self.inner.durable
    }

    async fn run<T, F>(&self, operation: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut State) -> Result<T> + Send + 'static,
    {
        // Acquire before spawning so callers cannot flood the blocking pool.
        // The task owns this permit: cancelling its waiter cannot reorder writes.
        let permit = self
            .inner
            .gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Closed)?;
        if !self.inner.durable {
            let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            return operation(&mut state);
        }
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut state = inner
                .state
                .lock()
                .map_err(|_| Error::Storage("storage mutex poisoned".into()))?;
            operation(&mut state)
        })
        .await
        .map_err(|e| Error::Storage(format!("storage worker failed: {e}")))?
    }

    pub async fn publish(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<Record> {
        validate_subject(subject, false, SUBJECT_LIMIT)?;
        if let Some(reply) = reply {
            validate_subject(reply, false, SUBJECT_LIMIT)?;
        }
        if !self
            .inner
            .config
            .subjects
            .iter()
            .any(|p| subject_matches(p, subject))
        {
            return Err(Error::StreamNotMatched);
        }
        let limit = self
            .inner
            .config
            .max_payload_bytes
            .min(self.inner.config.max_bytes);
        if payload.len() > limit {
            return Err(Error::PayloadTooLarge {
                size: payload.len(),
                limit,
            });
        }
        let message = Message {
            subject: Arc::from(subject),
            reply: reply.map(Arc::from),
            payload,
        };
        let retained_size = message_size(&message);
        if retained_size > self.inner.config.max_bytes {
            return Err(Error::PayloadTooLarge {
                size: retained_size,
                limit: self.inner.config.max_bytes,
            });
        }
        self.run(move |state| {
            if state.next_sequence == u64::MAX {
                return Err(Error::Storage("stream sequence exhausted".into()));
            }
            let record = Record {
                sequence: state.next_sequence,
                timestamp_millis: now_millis(),
                message,
            };
            state.commit(Event::Publish(record.clone()))?;
            Ok(record)
        })
        .await
    }

    pub async fn replay(&self, from_sequence: u64, limit: usize) -> Result<Vec<Record>> {
        validate_fetch(limit)?;
        self.run(move |state| {
            Ok(state
                .records
                .range(state.start_index(from_sequence)..)
                .take(limit)
                .cloned()
                .collect())
        })
        .await
    }

    pub async fn consumer(&self, name: &str, config: ConsumerConfig) -> Result<DurableConsumer> {
        validate_consumer(name, &config)?;
        let owned = name.to_owned();
        self.run(move |state| {
            if let Some(existing) = state.consumers.get(&owned) {
                if existing.config != config {
                    return Err(Error::InvalidConfig(
                        "consumer already exists with different settings".into(),
                    ));
                }
                return Ok(());
            }
            if state.consumers.len() >= state.config.max_consumers {
                return Err(Error::InvalidConfig("stream consumer limit reached".into()));
            }
            let consumer = ConsumerState {
                next_sequence: config.start_sequence,
                config,
                pending: BTreeMap::new(),
            };
            state.commit(Event::Consumer(owned, consumer))
        })
        .await?;
        Ok(DurableConsumer {
            stream: self.clone(),
            name: Arc::from(name),
        })
    }

    pub async fn get_consumer(&self, name: &str) -> Result<DurableConsumer> {
        let owned = name.to_owned();
        self.run(move |state| {
            if state.consumers.contains_key(&owned) {
                Ok(())
            } else {
                Err(Error::ConsumerNotFound)
            }
        })
        .await?;
        Ok(DurableConsumer {
            stream: self.clone(),
            name: Arc::from(name),
        })
    }
}

impl DurableConsumer {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns overdue pending deliveries first, followed by new matching records.
    /// A fetch with no available records returns immediately with an empty vector.
    pub async fn fetch(&self, limit: usize) -> Result<Vec<Delivery>> {
        validate_fetch(limit)?;
        let name = self.name.to_string();
        self.stream
            .run(move |state| {
                let consumer = state.consumers.get(&name).ok_or(Error::ConsumerNotFound)?;
                let now = now_millis();
                let ack_wait = consumer.config.ack_wait.as_millis() as u64;
                let mut result = Vec::new();
                let mut updates = Vec::new();
                let mut new_pending = 0usize;
                for (&sequence, pending) in &consumer.pending {
                    if result.len() == limit {
                        break;
                    }
                    if pending.delivered_at != 0
                        && now.saturating_sub(pending.delivered_at) < ack_wait
                    {
                        continue;
                    }
                    if let Some(record) = state.record(sequence) {
                        let attempts = pending.attempts.saturating_add(1);
                        result.push(Delivery {
                            record: record.clone(),
                            delivery_count: attempts,
                        });
                        updates.push((
                            sequence,
                            Pending {
                                delivered_at: now,
                                attempts,
                            },
                        ));
                    }
                }
                let mut next_sequence = consumer.next_sequence;
                for record in state
                    .records
                    .range(state.start_index(consumer.next_sequence)..)
                {
                    if result.len() == limit
                        || consumer.pending.len() + new_pending >= consumer.config.max_ack_pending
                    {
                        break;
                    }
                    next_sequence = record.sequence + 1;
                    if !subject_matches(&consumer.config.filter_subject, &record.message.subject) {
                        continue;
                    }
                    result.push(Delivery {
                        record: record.clone(),
                        delivery_count: 1,
                    });
                    updates.push((
                        record.sequence,
                        Pending {
                            delivered_at: now,
                            attempts: 1,
                        },
                    ));
                    new_pending += 1;
                }
                if !updates.is_empty() || next_sequence != consumer.next_sequence {
                    state.commit(Event::Fetch(name, next_sequence, updates))?;
                }
                Ok(result)
            })
            .await
    }

    pub async fn ack(&self, sequence: u64) -> Result<()> {
        let name = self.name.to_string();
        self.stream
            .run(move |state| {
                let consumer = state.consumers.get(&name).ok_or(Error::ConsumerNotFound)?;
                if !consumer.pending.contains_key(&sequence) {
                    return Err(Error::InvalidAck);
                }
                state.commit(Event::Ack(name, sequence))
            })
            .await
    }
}

impl State {
    fn empty(config: StreamConfig) -> Self {
        Self {
            config,
            records: VecDeque::new(),
            bytes: 0,
            next_sequence: 1,
            consumers: HashMap::new(),
            journal: None,
        }
    }

    fn record(&self, sequence: u64) -> Option<&Record> {
        let first = self.records.front()?.sequence;
        let index = usize::try_from(sequence.checked_sub(first)?).ok()?;
        self.records.get(index).filter(|r| r.sequence == sequence)
    }

    fn start_index(&self, sequence: u64) -> usize {
        let first = self
            .records
            .front()
            .map_or(self.next_sequence, |r| r.sequence);
        usize::try_from(sequence.saturating_sub(first))
            .unwrap_or(usize::MAX)
            .min(self.records.len())
    }

    fn commit(&mut self, event: Event) -> Result<()> {
        if self.journal.as_ref().is_some_and(|j| j.failed) {
            return Err(Error::Storage(
                "journal write previously failed; reopen the stream".into(),
            ));
        }
        let baseline = (self.config.max_bytes as u64)
            .max(self.journal.as_ref().map_or(0, |j| j.compacted_bytes));
        let threshold = baseline.saturating_mul(2).saturating_add(1024 * 1024);
        if self.journal.as_ref().is_some_and(|j| j.bytes > threshold) {
            self.compact()?;
        }
        if let Some(journal) = &mut self.journal {
            let encoded = encode(&event)?;
            if let Err(error) = write_frame(&mut journal.file, &encoded).and_then(|size| {
                journal.bytes += size;
                if self.config.sync_on_write {
                    journal.file.sync_data()?;
                }
                Ok(size)
            }) {
                journal.failed = true;
                return Err(Error::Io(error));
            }
        }
        self.apply(event)
    }

    fn apply(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Publish(record) => {
                if record.sequence < self.next_sequence
                    || record.sequence == u64::MAX
                    || (!self.records.is_empty() && record.sequence != self.next_sequence)
                {
                    return Err(Error::Storage("non-monotonic stream sequence".into()));
                }
                self.next_sequence = record.sequence + 1;
                self.bytes += message_size(&record.message);
                self.records.push_back(record);
                let mut evicted_any = false;
                while self.records.len() > self.config.max_messages
                    || self.bytes > self.config.max_bytes
                {
                    if let Some(evicted) = self.records.pop_front() {
                        evicted_any = true;
                        self.bytes -= message_size(&evicted.message);
                    }
                }
                if evicted_any {
                    let first = self
                        .records
                        .front()
                        .map_or(self.next_sequence, |r| r.sequence);
                    for consumer in self.consumers.values_mut() {
                        consumer.pending.retain(|&sequence, _| sequence >= first);
                        consumer.next_sequence = consumer.next_sequence.max(first);
                    }
                }
            }
            Event::Consumer(name, consumer) => {
                if !self.consumers.contains_key(&name)
                    && self.consumers.len() >= self.config.max_consumers
                {
                    return Err(Error::Storage(
                        "stored consumers exceed configured limit".into(),
                    ));
                }
                self.consumers.insert(name, consumer);
            }
            Event::Fetch(name, next_sequence, pending) => {
                let consumer = self
                    .consumers
                    .get_mut(&name)
                    .ok_or_else(|| Error::Storage("fetch references absent consumer".into()))?;
                consumer.next_sequence = next_sequence;
                for (sequence, pending) in pending {
                    consumer.pending.insert(sequence, pending);
                }
            }
            Event::Ack(name, sequence) => {
                self.consumers
                    .get_mut(&name)
                    .ok_or_else(|| Error::Storage("ack references absent consumer".into()))?
                    .pending
                    .remove(&sequence);
            }
            Event::NextSequence(next_sequence) => {
                self.next_sequence = self.next_sequence.max(next_sequence);
            }
        }
        Ok(())
    }

    fn compact(&mut self) -> Result<()> {
        let Some(journal) = &mut self.journal else {
            return Ok(());
        };
        let temporary = journal.directory.join("stream.compact");
        let mut replacement = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        replacement.write_all(MAGIC)?;
        let mut bytes = MAGIC.len() as u64;
        for record in &self.records {
            bytes += write_frame(&mut replacement, &encode(&Event::Publish(record.clone()))?)?;
        }
        bytes += write_frame(
            &mut replacement,
            &encode(&Event::NextSequence(self.next_sequence))?,
        )?;
        for (name, consumer) in &self.consumers {
            bytes += write_frame(
                &mut replacement,
                &encode(&Event::Consumer(name.clone(), consumer.clone()))?,
            )?;
        }
        if self.config.sync_on_write {
            replacement.sync_all()?;
        }
        // Open the replacement before rename so a failed open cannot strand the
        // active writer on the old, unlinked journal inode.
        let append = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&temporary)?;
        std::fs::rename(&temporary, journal.directory.join("stream.log"))?;
        journal.file = append;
        journal.bytes = bytes;
        journal.compacted_bytes = bytes;
        if self.config.sync_on_write
            && let Err(error) = sync_directory(&journal.directory)
        {
            journal.failed = true;
            return Err(Error::Io(error));
        }
        Ok(())
    }
}

fn validate_config(config: &StreamConfig) -> Result<()> {
    if config.subjects.is_empty()
        || config.max_messages == 0
        || config.max_bytes == 0
        || config.max_payload_bytes == 0
        || config.max_consumers == 0
        || config.max_payload_bytes > u32::MAX as usize - 4096
    {
        return Err(Error::InvalidConfig(
            "stream limits must be positive and payload must fit a frame".into(),
        ));
    }
    for subject in &config.subjects {
        validate_subject(subject, true, SUBJECT_LIMIT)?;
    }
    Ok(())
}

fn validate_consumer(name: &str, config: &ConsumerConfig) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name.chars().any(|c| c.is_control() || c.is_whitespace())
        || config.ack_wait.as_millis() == 0
        || config.ack_wait.as_millis() > u64::MAX as u128
        || !config.ack_wait.subsec_nanos().is_multiple_of(1_000_000)
        || config.max_ack_pending == 0
        || config.max_ack_pending > MAX_FETCH
        || config.start_sequence == 0
    {
        return Err(Error::InvalidConfig(
            "invalid consumer name or limits".into(),
        ));
    }
    validate_subject(&config.filter_subject, true, SUBJECT_LIMIT)
}

fn validate_fetch(limit: usize) -> Result<()> {
    if limit == 0 || limit > MAX_FETCH {
        Err(Error::InvalidConfig(format!(
            "fetch limit must be in 1..={MAX_FETCH}"
        )))
    } else {
        Ok(())
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn message_size(message: &Message) -> usize {
    message.payload.len()
        + message.subject.len()
        + message.reply.as_ref().map_or(0, |s| s.len())
        + 32
}

fn open_state(directory: PathBuf, config: StreamConfig) -> Result<State> {
    let directory = if directory.is_absolute() {
        directory
    } else {
        std::env::current_dir()?.join(directory)
    };
    let missing: Vec<_> = directory
        .ancestors()
        .take_while(|path| !path.exists())
        .map(Path::to_path_buf)
        .collect();
    std::fs::create_dir_all(&directory)?;
    if config.sync_on_write {
        for created in missing.iter().rev() {
            if let Some(parent) = created.parent() {
                sync_directory(parent)?;
            }
        }
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("stream.lock"))?;
    lock.try_lock().map_err(|e| {
        Error::Storage(format!(
            "stream directory is already open or cannot be locked: {e}"
        ))
    })?;
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(directory.join("stream.log"))?;
    let mut length = file.metadata()?.len();
    if length == 0 {
        file.write_all(MAGIC)?;
        if config.sync_on_write {
            file.sync_all()?;
            sync_directory(&directory)?;
        }
        length = MAGIC.len() as u64;
    }
    file.seek(SeekFrom::Start(0))?;
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(Error::Storage("invalid journal header".into()));
    }
    let mut state = State::empty(config);
    let mut valid_length = MAGIC.len() as u64;
    // Bound each allocation independently of untrusted on-disk lengths.
    let max_frame = state
        .config
        .max_payload_bytes
        .saturating_add(4096)
        .max(MAX_FETCH * 20 + 4096);
    while valid_length < length {
        if length - valid_length < 8 {
            break;
        }
        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        let frame_length = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        if frame_length > max_frame {
            return Err(Error::Storage(
                "journal frame exceeds configured bounds".into(),
            ));
        }
        if length - valid_length - 8 < frame_length as u64 {
            break;
        }
        let mut body = vec![0; frame_length];
        file.read_exact(&mut body)?;
        if checksum(&body) != u32::from_le_bytes(header[4..].try_into().unwrap()) {
            return Err(Error::Storage("journal checksum mismatch".into()));
        }
        state.apply(decode(&body, &state.config)?)?;
        valid_length += 8 + frame_length as u64;
    }
    if valid_length != length {
        // Windows append handles cannot truncate. Keep the normal writer in
        // append mode and recover through a separate handle under the same lock.
        let recovery = OpenOptions::new()
            .write(true)
            .open(directory.join("stream.log"))?;
        recovery.set_len(valid_length)?;
        if state.config.sync_on_write {
            recovery.sync_all()?;
        }
    }
    // Smaller retention settings on reopen also remove obsolete pending state.
    let first = state
        .records
        .front()
        .map_or(state.next_sequence, |r| r.sequence);
    for consumer in state.consumers.values_mut() {
        consumer.pending.retain(|&sequence, _| sequence >= first);
        // No delivery from the previous process can still be in flight through
        // this exclusively opened store. Make it immediately eligible again.
        for pending in consumer.pending.values_mut() {
            pending.delivered_at = 0;
        }
        consumer.next_sequence = consumer.next_sequence.max(first);
    }
    let compacted_bytes = state.bytes as u64
        + state.records.len() as u64
        + 25
        + state
            .consumers
            .iter()
            .map(|(name, consumer)| {
                45 + name.len() as u64
                    + consumer.config.filter_subject.len() as u64
                    + consumer.pending.len() as u64 * 20
            })
            .sum::<u64>();
    state.journal = Some(Journal {
        directory,
        file,
        _lock: lock,
        bytes: valid_length,
        compacted_bytes,
        failed: false,
    });
    Ok(state)
}

fn checksum(bytes: &[u8]) -> u32 {
    bytes.iter().fold(2_166_136_261u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(16_777_619)
    })
}

// Unix needs the directory entry flushed after creation/atomic replacement.
// Rust/Windows does not provide a directory flush operation; file contents are
// still flushed there, but power-loss durability of renamed metadata is weaker.
#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}
#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn write_frame(file: &mut File, body: &[u8]) -> std::io::Result<u64> {
    let length =
        u32::try_from(body.len()).map_err(|_| std::io::Error::other("journal frame too large"))?;
    let mut frame = Vec::with_capacity(body.len() + 8);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&checksum(body).to_le_bytes());
    frame.extend_from_slice(body);
    file.write_all(&frame)?;
    Ok(frame.len() as u64)
}

fn put_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}
fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_pending(out: &mut Vec<u8>, sequence: u64, pending: &Pending) {
    put_u64(out, sequence);
    put_u64(out, pending.delivered_at);
    out.extend_from_slice(&pending.attempts.to_le_bytes());
}

fn encode(event: &Event) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match event {
        Event::Publish(record) => {
            out.push(1);
            put_u64(&mut out, record.sequence);
            put_u64(&mut out, record.timestamp_millis);
            put_string(&mut out, &record.message.subject);
            put_string(&mut out, record.message.reply.as_deref().unwrap_or(""));
            out.extend_from_slice(&(record.message.payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&record.message.payload);
        }
        Event::Consumer(name, consumer) => {
            out.push(2);
            put_string(&mut out, name);
            put_string(&mut out, &consumer.config.filter_subject);
            put_u64(&mut out, consumer.config.ack_wait.as_millis() as u64);
            out.extend_from_slice(&(consumer.config.max_ack_pending as u32).to_le_bytes());
            put_u64(&mut out, consumer.config.start_sequence);
            put_u64(&mut out, consumer.next_sequence);
            out.extend_from_slice(&(consumer.pending.len() as u32).to_le_bytes());
            for (&sequence, pending) in &consumer.pending {
                put_pending(&mut out, sequence, pending);
            }
        }
        Event::Fetch(name, next_sequence, updates) => {
            out.push(3);
            put_string(&mut out, name);
            put_u64(&mut out, *next_sequence);
            out.extend_from_slice(&(updates.len() as u32).to_le_bytes());
            for (sequence, pending) in updates {
                put_pending(&mut out, *sequence, pending);
            }
        }
        Event::Ack(name, sequence) => {
            out.push(4);
            put_string(&mut out, name);
            put_u64(&mut out, *sequence);
        }
        Event::NextSequence(sequence) => {
            out.push(5);
            put_u64(&mut out, *sequence);
        }
    }
    Ok(out)
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Decoder<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| Error::Storage("frame length overflow".into()))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| Error::Storage("truncated journal event".into()))?;
        self.offset = end;
        Ok(value)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String> {
        let length = u16::from_le_bytes(self.take(2)?.try_into().unwrap()) as usize;
        String::from_utf8(self.take(length)?.to_vec())
            .map_err(|_| Error::Storage("invalid journal string".into()))
    }
    fn pending(&mut self) -> Result<Vec<(u64, Pending)>> {
        let count = self.u32()? as usize;
        if count > MAX_FETCH {
            return Err(Error::Storage("pending delivery limit exceeded".into()));
        }
        let mut pending = Vec::with_capacity(count);
        for _ in 0..count {
            let sequence = self.u64()?;
            let delivered_at = self.u64()?;
            let attempts = self.u32()?;
            pending.push((
                sequence,
                Pending {
                    delivered_at,
                    attempts,
                },
            ));
        }
        Ok(pending)
    }
}

fn decode(bytes: &[u8], config: &StreamConfig) -> Result<Event> {
    let mut decoder = Decoder { bytes, offset: 0 };
    let opcode = decoder.take(1)?[0];
    let event = match opcode {
        1 => {
            let sequence = decoder.u64()?;
            let timestamp_millis = decoder.u64()?;
            let subject = decoder.string()?;
            let reply = decoder.string()?;
            validate_subject(&subject, false, SUBJECT_LIMIT)?;
            if !reply.is_empty() {
                validate_subject(&reply, false, SUBJECT_LIMIT)?;
            }
            let length = decoder.u32()? as usize;
            if length > config.max_payload_bytes {
                return Err(Error::Storage(
                    "stored payload exceeds configured limit".into(),
                ));
            }
            let payload = Bytes::copy_from_slice(decoder.take(length)?);
            Event::Publish(Record {
                sequence,
                timestamp_millis,
                message: Message {
                    subject: Arc::from(subject),
                    reply: if reply.is_empty() {
                        None
                    } else {
                        Some(Arc::from(reply))
                    },
                    payload,
                },
            })
        }
        2 => {
            let name = decoder.string()?;
            let filter_subject = decoder.string()?;
            let ack_wait = Duration::from_millis(decoder.u64()?);
            let max_ack_pending = decoder.u32()? as usize;
            let start_sequence = decoder.u64()?;
            let next_sequence = decoder.u64()?;
            let config = ConsumerConfig {
                filter_subject,
                ack_wait,
                max_ack_pending,
                start_sequence,
            };
            validate_consumer(&name, &config)?;
            let pending = decoder.pending()?.into_iter().collect();
            Event::Consumer(
                name,
                ConsumerState {
                    config,
                    next_sequence,
                    pending,
                },
            )
        }
        3 => {
            let name = decoder.string()?;
            let next_sequence = decoder.u64()?;
            Event::Fetch(name, next_sequence, decoder.pending()?)
        }
        4 => {
            let name = decoder.string()?;
            Event::Ack(name, decoder.u64()?)
        }
        5 => Event::NextSequence(decoder.u64()?),
        _ => return Err(Error::Storage("unknown journal event".into())),
    };
    if decoder.offset != bytes.len() {
        return Err(Error::Storage("trailing journal event bytes".into()));
    }
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(1);
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "rift-stream-test-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            )))
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn retention_replay_and_subject_filters() {
        let stream = Stream::memory(StreamConfig {
            subjects: vec!["orders.>".into()],
            max_messages: 2,
            ..Default::default()
        })
        .unwrap();
        assert!(!stream.is_durable());
        assert!(matches!(
            stream.publish("elsewhere", None, Bytes::new()).await,
            Err(Error::StreamNotMatched)
        ));
        for n in 1..=3 {
            assert_eq!(
                stream
                    .publish("orders.new", None, Bytes::from(vec![n]))
                    .await
                    .unwrap()
                    .sequence,
                n as u64
            );
        }
        let records = stream.replay(1, 10).await.unwrap();
        assert_eq!(
            records.iter().map(|r| r.sequence).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(stream.replay(3, 1).await.unwrap()[0].sequence, 3);
    }

    #[tokio::test]
    async fn durable_reopen_ack_and_redelivery() {
        let directory = Directory::new();
        let config = StreamConfig::default();
        let stream = Stream::open(&directory.0, config.clone()).await.unwrap();
        assert!(stream.is_durable());
        stream
            .publish("jobs.run", Some("_INBOX.reply"), Bytes::from_static(b"one"))
            .await
            .unwrap();
        stream
            .publish("jobs.run", None, Bytes::from_static(b"two"))
            .await
            .unwrap();
        let consumer = stream
            .consumer("worker", ConsumerConfig::default())
            .await
            .unwrap();
        let deliveries = consumer.fetch(10).await.unwrap();
        assert_eq!(deliveries.len(), 2);
        consumer.ack(1).await.unwrap();
        assert!(matches!(consumer.ack(1).await, Err(Error::InvalidAck)));
        drop(consumer);
        drop(stream);
        let reopened = Stream::open(&directory.0, config).await.unwrap();
        let records = reopened.replay(1, 10).await.unwrap();
        assert_eq!(records[0].message.reply.as_deref(), Some("_INBOX.reply"));
        let consumer = reopened.get_consumer("worker").await.unwrap();
        let delivery = consumer.fetch(10).await.unwrap();
        assert_eq!(delivery.len(), 1);
        assert_eq!(delivery[0].record.sequence, 2);
        assert_eq!(delivery[0].delivery_count, 2);
        consumer.ack(2).await.unwrap();
        assert!(consumer.fetch(10).await.unwrap().is_empty());
        assert_eq!(
            reopened
                .publish("jobs.run", None, Bytes::new())
                .await
                .unwrap()
                .sequence,
            3
        );
    }

    #[tokio::test]
    async fn pending_limit_filter_and_expiry() {
        let stream = Stream::memory(StreamConfig::default()).unwrap();
        for subject in ["jobs.one", "other.one", "jobs.two"] {
            stream.publish(subject, None, Bytes::new()).await.unwrap();
        }
        let consumer = stream
            .consumer(
                "worker",
                ConsumerConfig {
                    filter_subject: "jobs.*".into(),
                    max_ack_pending: 1,
                    ack_wait: Duration::from_millis(50),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(consumer.fetch(10).await.unwrap()[0].record.sequence, 1);
        assert!(consumer.fetch(10).await.unwrap().is_empty());
        consumer.ack(1).await.unwrap();
        assert_eq!(consumer.fetch(10).await.unwrap()[0].record.sequence, 3);
        assert!(
            stream
                .consumer("worker", ConsumerConfig::default())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn exclusive_open_and_torn_tail_recovery() {
        let directory = Directory::new();
        let config = StreamConfig::default();
        let stream = Stream::open(&directory.0, config.clone()).await.unwrap();
        stream
            .publish("x", None, Bytes::from_static(b"saved"))
            .await
            .unwrap();
        assert!(Stream::open(&directory.0, config.clone()).await.is_err());
        drop(stream);
        let log = directory.0.join("stream.log");
        let valid_length = std::fs::metadata(&log).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(&100u32.to_le_bytes()).unwrap();
        file.write_all(&[0; 9]).unwrap();
        drop(file);
        let reopened = Stream::open(&directory.0, config.clone()).await.unwrap();
        assert_eq!(
            reopened.replay(1, 10).await.unwrap()[0].message.payload,
            "saved"
        );
        assert_eq!(std::fs::metadata(&log).unwrap().len(), valid_length);
        assert_eq!(
            reopened
                .publish("x", None, Bytes::from_static(b"after recovery"))
                .await
                .unwrap()
                .sequence,
            2
        );
        drop(reopened);
        let reopened = Stream::open(&directory.0, config).await.unwrap();
        let records = reopened.replay(1, 10).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].message.payload, "saved");
        assert_eq!(records[1].sequence, 2);
        assert_eq!(records[1].message.payload, "after recovery");
    }

    #[tokio::test]
    async fn corrupt_complete_frame_is_rejected() {
        let directory = Directory::new();
        let config = StreamConfig::default();
        let stream = Stream::open(&directory.0, config.clone()).await.unwrap();
        stream
            .publish("x", None, Bytes::from_static(b"saved"))
            .await
            .unwrap();
        drop(stream);
        let log = directory.0.join("stream.log");
        let mut bytes = std::fs::read(&log).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&log, bytes).unwrap();
        assert!(matches!(
            Stream::open(&directory.0, config).await,
            Err(Error::Storage(_))
        ));
    }

    #[tokio::test]
    async fn concurrent_publishes_have_unique_ordered_sequences() {
        let directory = Directory::new();
        let stream = Stream::open(
            &directory.0,
            StreamConfig {
                sync_on_write: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let stream = stream.clone();
            tasks.push(tokio::spawn(async move {
                stream
                    .publish("x", None, Bytes::new())
                    .await
                    .unwrap()
                    .sequence
            }));
        }
        let mut sequences = Vec::new();
        for task in tasks {
            sequences.push(task.await.unwrap());
        }
        sequences.sort_unstable();
        assert_eq!(sequences, (1..=32).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn compaction_preserves_pending_state_and_sequences() {
        let directory = Directory::new();
        let config = StreamConfig {
            max_bytes: 16 * 1024,
            max_messages: 3,
            sync_on_write: false,
            ..Default::default()
        };
        let stream = Stream::open(&directory.0, config.clone()).await.unwrap();
        let payload = Bytes::from(vec![7; 8192]);
        for _ in 0..160 {
            stream.publish("x", None, payload.clone()).await.unwrap();
        }
        let consumer = stream
            .consumer(
                "worker",
                ConsumerConfig {
                    ack_wait: Duration::from_millis(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let sequence = consumer.fetch(1).await.unwrap()[0].record.sequence;
        // Force compaction with a pending record in its snapshot.
        stream.run(|state| state.compact()).await.unwrap();
        drop(consumer);
        drop(stream);
        tokio::time::sleep(Duration::from_millis(3)).await;
        let stream = Stream::open(&directory.0, config).await.unwrap();
        let consumer = stream.get_consumer("worker").await.unwrap();
        let delivery = consumer.fetch(1).await.unwrap();
        assert_eq!(delivery[0].record.sequence, sequence);
        assert_eq!(delivery[0].delivery_count, 2);
        assert_eq!(
            stream.publish("x", None, payload).await.unwrap().sequence,
            161
        );
        assert!(
            std::fs::metadata(directory.0.join("stream.log"))
                .unwrap()
                .len()
                < 64 * 1024
        );
    }

    #[tokio::test]
    async fn retention_frees_pending_capacity() {
        let stream = Stream::memory(StreamConfig {
            max_messages: 1,
            ..Default::default()
        })
        .unwrap();
        let consumer = stream
            .consumer(
                "worker",
                ConsumerConfig {
                    max_ack_pending: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        stream.publish("x", None, Bytes::new()).await.unwrap();
        consumer.fetch(1).await.unwrap();
        stream.publish("x", None, Bytes::new()).await.unwrap();
        assert_eq!(consumer.fetch(1).await.unwrap()[0].record.sequence, 2);
        assert!(matches!(consumer.ack(1).await, Err(Error::InvalidAck)));
    }

    #[tokio::test]
    async fn cancelled_durable_operation_keeps_its_serialization_permit() {
        let directory = Directory::new();
        let stream = Stream::open(&directory.0, StreamConfig::default())
            .await
            .unwrap();
        let (started, observed) = tokio::sync::oneshot::channel();
        let pending_stream = stream.clone();
        let pending = tokio::spawn(async move {
            pending_stream
                .run(move |state| {
                    let _ = started.send(());
                    std::thread::sleep(Duration::from_millis(20));
                    state.commit(Event::Publish(Record {
                        sequence: state.next_sequence,
                        timestamp_millis: now_millis(),
                        message: Message {
                            subject: Arc::from("x"),
                            reply: None,
                            payload: Bytes::from_static(b"first"),
                        },
                    }))
                })
                .await
        });
        observed.await.unwrap();
        pending.abort();
        assert_eq!(
            stream
                .publish("x", None, Bytes::from_static(b"second"))
                .await
                .unwrap()
                .sequence,
            2
        );
        let records = stream.replay(1, 10).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].message.payload, "first");
    }

    #[tokio::test]
    async fn ack_timeout_redelivers_before_new_messages() {
        let stream = Stream::memory(StreamConfig::default()).unwrap();
        stream.publish("x", None, Bytes::new()).await.unwrap();
        stream.publish("x", None, Bytes::new()).await.unwrap();
        let consumer = stream
            .consumer(
                "worker",
                ConsumerConfig {
                    ack_wait: Duration::from_millis(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(consumer.fetch(1).await.unwrap()[0].record.sequence, 1);
        tokio::time::sleep(Duration::from_millis(3)).await;
        let redelivered = consumer.fetch(1).await.unwrap();
        assert_eq!(redelivered[0].record.sequence, 1);
        assert_eq!(redelivered[0].delivery_count, 2);
        consumer.ack(1).await.unwrap();
        assert_eq!(consumer.fetch(1).await.unwrap()[0].record.sequence, 2);
    }
}
