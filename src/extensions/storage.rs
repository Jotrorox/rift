//! Bounded host-owned state, shared by every callback in an extension runtime.
//!
//! A configured store is a bundled-SQLite database. Each mutation commits only
//! the entries it changes, in WAL mode with `synchronous=FULL`, so a write costs
//! one small append and sync instead of rewriting the store. The connection holds
//! an exclusive lock: one runtime owns a path, and a second process fails at startup.
//!
//! Memory mirrors committed rows. Writers are serialized and change memory only
//! after their transaction commits; readers use the mirror and never wait for a
//! write to sync.
//!
//! Version 1 stores were checksummed binary snapshots. Startup converts one into
//! a database built beside it, atomically replaces the snapshot, and keeps the
//! original bytes in `<file>.v1-backup`.
use mlua::{Lua, Table, Value};
use rusqlite::{Connection, ErrorCode, OpenFlags, params};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, RwLock, TryLockError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_LABEL: usize = 128;
const MAX_VALUE: usize = 64 * 1024;
/// Upper bound of a version 1 snapshot.
const MAX_BYTES: usize = 1024 * 1024;
// LuaJIT numbers cannot distinguish every integer outside this range.
const MAX_INTEGER: mlua::Integer = (1 << 53) - 1;
const MAGIC: &[u8; 8] = b"RIFTKV\0\x01";
const CHECKSUM_BYTES: usize = 32;
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";
/// `PRAGMA application_id`: "RIFT", so another SQLite database is not mistaken for a store.
const APPLICATION_ID: i32 = 0x5249_4654;
const SCHEMA_VERSION: i32 = 1;
const SCHEMA: &str = "CREATE TABLE entries (
    namespace TEXT NOT NULL,
    key TEXT NOT NULL,
    value BLOB NOT NULL,
    PRIMARY KEY (namespace, key)
) STRICT, WITHOUT ROWID;";
/// Namespaces beginning with this prefix belong to the host, e.g. group memberships.
const HOST_PREFIX: &str = "rift.";
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
type Key = (String, String);
pub(super) type Entries = BTreeMap<Key, Vec<u8>>;

/// Limits for one class of data. Host data has its own, so plugins filling the
/// store cannot block permission group changes, and memberships cannot use up
/// plugin space.
struct Quota {
    owner: &'static str,
    entries: usize,
    bytes: usize,
}
const PLUGIN_QUOTA: Quota = Quota {
    owner: "plugin",
    entries: 4096,
    bytes: 1024 * 1024,
};
const HOST_QUOTA: Quota = Quota {
    owner: "host",
    entries: 65536,
    bytes: 8 * 1024 * 1024,
};

fn host_namespace(namespace: &str) -> bool {
    namespace.starts_with(HOST_PREFIX)
}

/// Bytes an entry counts toward its quota: 8 bytes of overhead plus its contents.
fn entry_size((namespace, key): &Key, value: &[u8]) -> usize {
    8 + namespace.len() + key.len() + value.len()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StorageConfig {
    pub path: PathBuf,
}

pub(super) fn parse(value: Value) -> mlua::Result<Option<StorageConfig>> {
    let table = match value {
        Value::Nil => return Ok(None),
        Value::Table(table) => table,
        _ => return Err(mlua::Error::runtime("extensions.storage must be a table")),
    };
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if !matches!(key, Value::String(key) if key.as_bytes().as_ref() == b"path") {
            return Err(mlua::Error::runtime("unknown extensions.storage field"));
        }
    }
    let Value::String(path) = table.raw_get::<Value>("path")? else {
        return Err(mlua::Error::runtime(
            "extensions.storage.path must be a string",
        ));
    };
    let path = path.to_str()?;
    if path.is_empty() || path.contains('\0') {
        return Err(mlua::Error::runtime(
            "extensions.storage.path must be nonempty and contain no NUL bytes",
        ));
    }
    Ok(Some(StorageConfig {
        path: PathBuf::from(path.as_ref()),
    }))
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct Usage {
    entries: usize,
    bytes: usize,
}

/// Committed entries and their quota usage.
#[derive(Default)]
struct Memory {
    entries: Entries,
    plugin: Usage,
    host: Usage,
}

impl Memory {
    fn new(entries: Entries) -> io::Result<Self> {
        let mut memory = Self::default();
        for (key, value) in &entries {
            validate_entry(key, value)?;
            let usage = memory.usage(&key.0);
            usage.entries += 1;
            usage.bytes += entry_size(key, value);
        }
        check(&memory.plugin, &PLUGIN_QUOTA)?;
        check(&memory.host, &HOST_QUOTA)?;
        memory.entries = entries;
        Ok(memory)
    }

    fn usage(&mut self, namespace: &str) -> &mut Usage {
        if host_namespace(namespace) {
            &mut self.host
        } else {
            &mut self.plugin
        }
    }
}

fn check(usage: &Usage, quota: &Quota) -> io::Result<()> {
    if usage.entries > quota.entries {
        return Err(invalid(format!(
            "extension store {} data exceeds {} entries",
            quota.owner, quota.entries
        )));
    }
    if usage.bytes > quota.bytes {
        return Err(invalid(format!(
            "extension store {} data exceeds {} KiB",
            quota.owner,
            quota.bytes / 1024
        )));
    }
    Ok(())
}

/// Pending changes over the committed entries, applied only after they commit.
pub(super) struct Transaction<'a> {
    base: &'a Entries,
    changes: BTreeMap<Key, Option<Vec<u8>>>,
}

impl Transaction<'_> {
    pub(super) fn get(&self, key: &Key) -> Option<&Vec<u8>> {
        match self.changes.get(key) {
            Some(change) => change.as_ref(),
            None => self.base.get(key),
        }
    }

    pub(super) fn insert(&mut self, key: Key, value: Vec<u8>) {
        self.changes.insert(key, Some(value));
    }

    /// Whether an entry was removed.
    pub(super) fn remove(&mut self, key: &Key) -> bool {
        let existed = self.get(key).is_some();
        if existed {
            self.changes.insert(key.clone(), None);
        }
        existed
    }
}

struct Inner {
    memory: RwLock<Memory>,
    /// Serializes writers; `None` for a memory-only store.
    writer: Mutex<Option<Connection>>,
}

#[derive(Clone)]
pub(super) struct Store(Arc<Inner>);

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "extension store deadline exceeded",
        ));
    }
    Ok(())
}

fn exact_integer(value: mlua::Integer) -> io::Result<mlua::Integer> {
    if !(-MAX_INTEGER..=MAX_INTEGER).contains(&value) {
        return Err(invalid(
            "store integer exceeds the exact Lua number range (-9007199254740991..=9007199254740991)",
        ));
    }
    Ok(value)
}

fn label(value: Value, name: &str) -> mlua::Result<String> {
    let Value::String(value) = value else {
        return Err(mlua::Error::runtime(format!(
            "store {name} must be a string"
        )));
    };
    if !(1..=MAX_LABEL).contains(&value.as_bytes().len()) {
        return Err(mlua::Error::runtime(format!(
            "store {name} must contain 1..=128 bytes"
        )));
    }
    Ok(value.to_str()?.to_owned())
}

fn user_namespace(value: Value) -> mlua::Result<String> {
    let namespace = label(value, "namespace")?;
    if host_namespace(&namespace) {
        return Err(mlua::Error::runtime(format!(
            "store namespaces beginning with {HOST_PREFIX:?} are reserved for Rift"
        )));
    }
    Ok(namespace)
}

fn validate_entry((namespace, key): &Key, value: &[u8]) -> io::Result<()> {
    if !(1..=MAX_LABEL).contains(&namespace.len()) || !(1..=MAX_LABEL).contains(&key.len()) {
        return Err(invalid(
            "store namespace and key must contain 1..=128 bytes",
        ));
    }
    if value.len() > MAX_VALUE {
        return Err(invalid("store value exceeds 64 KiB"));
    }
    Ok(())
}

impl Store {
    fn new(memory: Memory, connection: Option<Connection>) -> Self {
        Self(Arc::new(Inner {
            memory: RwLock::new(memory),
            writer: Mutex::new(connection),
        }))
    }

    pub(super) fn open(config: Option<&StorageConfig>) -> io::Result<Self> {
        let Some(path) = config.map(|config| config.path.as_path()) else {
            return Ok(Self::new(Memory::default(), None));
        };
        // Inspect before opening: opening a named pipe can block indefinitely.
        // Check the open descriptor again below in case the path was replaced.
        match fs::metadata(path) {
            Ok(metadata) => validate_metadata(&metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match File::open(path) {
            Ok(file) => {
                validate_metadata(&file.metadata()?)?;
                let mut bytes = Vec::new();
                // `take` consumes the handle, so the file is closed before
                // a migration replaces it (Windows refuses to while open).
                file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
                if bytes.starts_with(SQLITE_HEADER) {
                    // The rest is read through SQLite.
                } else if bytes.starts_with(MAGIC) {
                    let entries = decode(&bytes)?;
                    // Validate before replacing anything.
                    Memory::new(entries.clone())?;
                    persist(&backup_path(path), &bytes)?;
                    create(path, &entries)?;
                } else {
                    return Err(invalid("invalid extension store"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create(path, &Entries::new())?;
            }
            Err(error) => return Err(error),
        }
        let (connection, entries) = open_database(path)?;
        Ok(Self::new(Memory::new(entries)?, Some(connection)))
    }

    fn lock(&self, deadline: Instant) -> io::Result<MutexGuard<'_, Option<Connection>>> {
        loop {
            check_deadline(deadline)?;
            match self.0.writer.try_lock() {
                Ok(writer) => {
                    check_deadline(deadline)?;
                    return Ok(writer);
                }
                Err(TryLockError::Poisoned(_)) => return Err(invalid("extension store poisoned")),
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    fn memory(&self) -> std::sync::RwLockReadGuard<'_, Memory> {
        self.0
            .memory
            .read()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn get(
        &self,
        namespace: String,
        key: String,
        deadline: Instant,
    ) -> io::Result<Option<Vec<u8>>> {
        check_deadline(deadline)?;
        Ok(self.memory().entries.get(&(namespace, key)).cloned())
    }

    /// Committed entries in one namespace, keyed by entry key.
    pub(super) fn namespace(&self, namespace: &str) -> Vec<(String, Vec<u8>)> {
        self.memory()
            .entries
            .iter()
            .filter(|((entry, _), _)| entry == namespace)
            .map(|((_, key), value)| (key.clone(), value.clone()))
            .collect()
    }

    fn mutate<T>(
        &self,
        deadline: Instant,
        change: impl FnOnce(&mut Transaction) -> io::Result<T>,
    ) -> io::Result<T> {
        self.mutate_then(deadline, change, |_| {})
    }

    /// Like `mutate`, then runs `committed` before admitting the next writer so
    /// derived host state observes mutations in commit order.
    pub(super) fn mutate_then<T>(
        &self,
        deadline: Instant,
        change: impl FnOnce(&mut Transaction) -> io::Result<T>,
        committed: impl FnOnce(&T),
    ) -> io::Result<T> {
        let mut writer = self.lock(deadline)?;
        // Only writers change memory and they hold `writer`, so these entries
        // stay current until this mutation applies its own changes.
        let (result, changes, plugin, host) = {
            let memory = self.memory();
            let mut transaction = Transaction {
                base: &memory.entries,
                changes: BTreeMap::new(),
            };
            let result = change(&mut transaction)?;
            let mut changes = transaction.changes;
            changes.retain(|key, value| memory.entries.get(key) != value.as_ref());
            let (mut plugin, mut host) = (memory.plugin, memory.host);
            for (key, value) in &changes {
                let usage = if host_namespace(&key.0) {
                    &mut host
                } else {
                    &mut plugin
                };
                if let Some(old) = memory.entries.get(key) {
                    usage.entries -= 1;
                    usage.bytes -= entry_size(key, old);
                }
                if let Some(value) = value {
                    validate_entry(key, value)?;
                    usage.entries += 1;
                    usage.bytes += entry_size(key, value);
                }
            }
            check(&plugin, &PLUGIN_QUOTA)?;
            check(&host, &HOST_QUOTA)?;
            (result, changes, plugin, host)
        };
        if !changes.is_empty() {
            check_deadline(deadline)?;
            if let Some(connection) = writer.as_mut() {
                commit(connection, &changes, deadline)
                    .map_err(|error| error.into_io("cannot commit extension store change"))?;
            }
            // The commit is durable. Do not return a deadline error after it:
            // memory must agree with the database.
            let mut memory = self
                .0
                .memory
                .write()
                .unwrap_or_else(|error| error.into_inner());
            for (key, value) in changes {
                match value {
                    Some(value) => memory.entries.insert(key, value),
                    None => memory.entries.remove(&key),
                };
            }
            memory.plugin = plugin;
            memory.host = host;
        }
        committed(&result);
        Ok(result)
    }

    pub(super) fn install(&self, lua: &Lua, api: &Table, deadline: Instant) -> mlua::Result<()> {
        crate::script::check_deadline(deadline)?;
        let table = lua.create_table()?;
        let store = self.clone();
        table.set(
            "get",
            lua.create_function(move |lua, (namespace, key): (Value, Value)| {
                let value = store
                    .get(user_namespace(namespace)?, label(key, "key")?, deadline)
                    .map_err(mlua::Error::external)?;
                value.map(|value| lua.create_string(value)).transpose()
            })?,
        )?;
        let store = self.clone();
        table.set(
            "set",
            lua.create_function(move |_, (namespace, key, value): (Value, Value, Value)| {
                let key = (user_namespace(namespace)?, label(key, "key")?);
                let Value::String(value) = value else {
                    return Err(mlua::Error::runtime("store value must be a string"));
                };
                if value.as_bytes().len() > MAX_VALUE {
                    return Err(mlua::Error::runtime("store value exceeds 64 KiB"));
                }
                store
                    .mutate(deadline, |entries| {
                        entries.insert(key, value.as_bytes().to_vec());
                        Ok(())
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
        let store = self.clone();
        table.set(
            "delete",
            lua.create_function(move |_, (namespace, key): (Value, Value)| {
                let key = (user_namespace(namespace)?, label(key, "key")?);
                store
                    .mutate(deadline, |entries| Ok(entries.remove(&key)))
                    .map_err(mlua::Error::external)
            })?,
        )?;
        let store = self.clone();
        table.set(
            "increment",
            lua.create_function(move |_, (namespace, key, delta): (Value, Value, Value)| {
                let key = (user_namespace(namespace)?, label(key, "key")?);
                let Value::Integer(delta) = delta else {
                    return Err(mlua::Error::runtime(
                        "store increment delta must be an integer",
                    ));
                };
                let delta = exact_integer(delta).map_err(mlua::Error::external)?;
                store
                    .mutate(deadline, |entries| {
                        let current = match entries.get(&key) {
                            Some(value) => std::str::from_utf8(value)
                                .ok()
                                .and_then(|value| value.parse::<mlua::Integer>().ok())
                                .ok_or_else(|| invalid("store value is not a decimal integer"))?,
                            None => 0,
                        };
                        let current = exact_integer(current)?;
                        let next = current
                            .checked_add(delta)
                            .ok_or_else(|| invalid("store integer overflow"))?;
                        let next = exact_integer(next)?;
                        entries.insert(key, next.to_string().into_bytes());
                        Ok(next)
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
        api.set("store", table)
    }
}

enum CommitError {
    Database(rusqlite::Error),
    Io(io::Error),
}

impl From<rusqlite::Error> for CommitError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

impl CommitError {
    fn into_io(self, context: &str) -> io::Error {
        match self {
            Self::Io(error) => error,
            Self::Database(error) => io::Error::other(format!("{context}: {error}")),
        }
    }
}

/// Write `changes` in one transaction. Dropping an uncommitted transaction rolls it back.
fn commit(
    connection: &mut Connection,
    changes: &BTreeMap<Key, Option<Vec<u8>>>,
    deadline: Instant,
) -> Result<(), CommitError> {
    let transaction = connection.transaction()?;
    {
        let mut upsert = transaction.prepare_cached(
            "INSERT INTO entries (namespace, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT (namespace, key) DO UPDATE SET value = excluded.value",
        )?;
        let mut delete =
            transaction.prepare_cached("DELETE FROM entries WHERE namespace = ?1 AND key = ?2")?;
        for ((namespace, key), value) in changes {
            match value {
                Some(value) => upsert.execute(params![namespace, key, value])?,
                None => delete.execute(params![namespace, key])?,
            };
        }
    }
    check_deadline(deadline).map_err(CommitError::Io)?;
    transaction.commit()?;
    Ok(())
}

fn database_error(path: &Path, error: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("extension store {}: {error}", path.display()),
    )
}

/// Open an existing store database, take its exclusive lock and load it.
fn open_database(path: &Path) -> io::Result<(Connection, Entries)> {
    let error = |error: rusqlite::Error| match error.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => io::Error::new(
            io::ErrorKind::ResourceBusy,
            format!(
                "extension store {} is in use by another process",
                path.display()
            ),
        ),
        _ => database_error(path, error),
    };
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(error)?;
    connection.busy_timeout(Duration::ZERO).map_err(error)?;
    // Exclusive locking before the first WAL access also keeps the WAL index in
    // memory instead of a shared-memory file.
    connection
        .execute_batch("PRAGMA locking_mode=EXCLUSIVE;")
        .map_err(error)?;
    let pragma = |name: &str| -> io::Result<i32> {
        connection
            .query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
            .map_err(error)
    };
    if pragma("application_id")? != APPLICATION_ID {
        return Err(database_error(path, "not a Rift extension store"));
    }
    let version = pragma("user_version")?;
    if version != SCHEMA_VERSION {
        return Err(database_error(
            path,
            format!("unsupported schema version {version}"),
        ));
    }
    let mode: String = connection
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .map_err(error)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(database_error(path, format!("cannot use WAL, got {mode}")));
    }
    connection
        .execute_batch("PRAGMA synchronous=FULL; BEGIN EXCLUSIVE; COMMIT;")
        .map_err(error)?;
    let check: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(error)?;
    if check != "ok" {
        return Err(database_error(
            path,
            format!("integrity check failed: {check}"),
        ));
    }
    let mut entries = Entries::new();
    {
        let mut statement = connection
            .prepare("SELECT namespace, key, value FROM entries")
            .map_err(error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(error)?;
        for row in rows {
            let (key, value) = row.map_err(error)?;
            validate_entry(&key, &value).map_err(|e| database_error(path, e))?;
            entries.insert(key, value);
        }
    }
    Ok((connection, entries))
}

/// What [`check_file`] found at a store path.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Check {
    /// No file yet; startup creates it.
    Missing,
    /// A v1 snapshot that startup will migrate.
    Snapshot { entries: usize },
    /// A store database that opens and validates.
    Database { entries: usize },
}

/// Validate the store at `path` without changing it. Snapshots are decoded in
/// memory. A database and any `-wal` file left by a crash are copied to a
/// private temporary directory and opened there, so recovery, locking and
/// validation run exactly as at startup without touching the original. Do not
/// call this while Rift runs: the copy may catch a write halfway.
pub(super) fn check_file(path: &Path) -> io::Result<Check> {
    match fs::metadata(path) {
        Ok(metadata) => validate_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Check::Missing),
        Err(error) => return Err(error),
    }
    let file = File::open(path)?;
    validate_metadata(&file.metadata()?)?;
    let mut header = Vec::new();
    (&file)
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut header)?;
    if header.starts_with(MAGIC) {
        let entries = decode(&header)?;
        let count = entries.len();
        Memory::new(entries)?;
        return Ok(Check::Snapshot { entries: count });
    }
    if !header.starts_with(SQLITE_HEADER) {
        return Err(invalid("invalid extension store"));
    }
    drop(file);
    let directory = std::env::temp_dir().join(format!(
        "rift-store-check-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&directory)?;
    let result = (|| {
        let copy = directory.join("store");
        fs::copy(path, &copy)?;
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        match fs::copy(&wal, directory.join("store-wal")) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // Name the original in errors, not the private copy.
        let (connection, entries) = open_database(&copy).map_err(|error| {
            let message = error.to_string();
            let message = message.replace(&copy.display().to_string(), &path.display().to_string());
            io::Error::new(error.kind(), message)
        })?;
        drop(connection);
        let count = entries.len();
        Memory::new(entries)?;
        Ok(Check::Database { entries: count })
    })();
    let _ = fs::remove_dir_all(&directory);
    result
}

/// Build a database with `entries` beside `path`, then atomically move it into place.
fn create(path: &Path, entries: &Entries) -> io::Result<()> {
    let (temporary, file) = temporary_file(parent(path))?;
    drop(file);
    let result = (|| {
        let error = |error: rusqlite::Error| database_error(&temporary, error);
        let mut connection = Connection::open_with_flags(
            &temporary,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(error)?;
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .map_err(error)?;
        let transaction = connection.transaction().map_err(error)?;
        transaction
            .execute_batch(&format!(
                "{SCHEMA} PRAGMA application_id={APPLICATION_ID}; PRAGMA user_version={SCHEMA_VERSION};"
            ))
            .map_err(error)?;
        {
            let mut insert = transaction
                .prepare("INSERT INTO entries (namespace, key, value) VALUES (?1, ?2, ?3)")
                .map_err(error)?;
            for ((namespace, key), value) in entries {
                insert
                    .execute(params![namespace, key, value])
                    .map_err(error)?;
            }
        }
        transaction.commit().map_err(error)?;
        // Closing the last connection checkpoints the WAL into the file.
        connection.close().map_err(|(_, e)| error(e))?;
        // Windows can only flush a file through a handle with write access.
        OpenOptions::new()
            .write(true)
            .open(&temporary)?
            .sync_all()?;
        fs::rename(&temporary, path)?;
        sync_directory(parent(path));
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        let mut wal = temporary.as_os_str().to_owned();
        wal.push("-wal");
        let _ = fs::remove_file(wal);
    }
    result
}

fn backup_path(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_owned();
    backup.push(".v1-backup");
    PathBuf::from(backup)
}

fn validate_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_file() {
        return Err(invalid("extension store must be a regular file"));
    }
    Ok(())
}

#[cfg(test)]
fn encode(entries: &Entries) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for ((namespace, key), value) in entries {
        bytes.extend_from_slice(&(namespace.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(key.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        bytes.extend_from_slice(namespace.as_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(value);
    }
    let checksum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    Ok(bytes)
}

/// Decode a version 1 snapshot.
fn decode(bytes: &[u8]) -> io::Result<Entries> {
    if bytes.len() > MAX_BYTES
        || bytes.len() < MAGIC.len() + 4 + CHECKSUM_BYTES
        || !bytes.starts_with(MAGIC)
    {
        return Err(invalid("invalid extension store snapshot"));
    }
    let (bytes, checksum) = bytes.split_at(bytes.len() - CHECKSUM_BYTES);
    if Sha256::digest(bytes).as_slice() != checksum {
        return Err(invalid("extension store checksum mismatch"));
    }
    fn take<'a>(bytes: &mut &'a [u8], len: usize) -> io::Result<&'a [u8]> {
        if bytes.len() < len {
            return Err(invalid("truncated extension store snapshot"));
        }
        let (value, rest) = bytes.split_at(len);
        *bytes = rest;
        Ok(value)
    }
    let mut bytes = &bytes[MAGIC.len()..];
    let count = u32::from_le_bytes(take(&mut bytes, 4)?.try_into().unwrap()) as usize;
    // Version 1 shared one 4096-entry limit between plugin and host data.
    if count > PLUGIN_QUOTA.entries {
        return Err(invalid("extension store exceeds 4096 entries"));
    }
    let mut entries = Entries::new();
    for _ in 0..count {
        let namespace_len = u16::from_le_bytes(take(&mut bytes, 2)?.try_into().unwrap()) as usize;
        let key_len = u16::from_le_bytes(take(&mut bytes, 2)?.try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(take(&mut bytes, 4)?.try_into().unwrap()) as usize;
        if !(1..=MAX_LABEL).contains(&namespace_len)
            || !(1..=MAX_LABEL).contains(&key_len)
            || value_len > MAX_VALUE
        {
            return Err(invalid("invalid extension store entry lengths"));
        }
        let namespace = std::str::from_utf8(take(&mut bytes, namespace_len)?)
            .map_err(invalid)?
            .to_owned();
        let key = std::str::from_utf8(take(&mut bytes, key_len)?)
            .map_err(invalid)?
            .to_owned();
        let value = take(&mut bytes, value_len)?.to_vec();
        if entries.insert((namespace, key), value).is_some() {
            return Err(invalid("duplicate extension store entry"));
        }
    }
    if !bytes.is_empty() {
        return Err(invalid("trailing data in extension store snapshot"));
    }
    Ok(entries)
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn sync_directory(_directory: &Path) {
    // Best effort: an error here cannot undo the rename.
    #[cfg(unix)]
    if let Ok(directory) = File::open(_directory) {
        let _ = directory.sync_all();
    }
}

fn temporary_file(parent: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..16 {
        let candidate = parent.join(format!(
            ".rift-store-{}-{}.tmp",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot create extension store temporary file",
    ))
}

/// Atomically replace `path` with `bytes`, synced before the rename.
fn persist(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (temporary, mut file) = temporary_file(parent(path))?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        sync_directory(parent(path));
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMBER_NAMESPACE: &str = "rift.groups";

    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rift-store-test-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn config(&self) -> StorageConfig {
            StorageConfig {
                path: self.0.join("state"),
            }
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }
    fn lua(store: &Store) -> Lua {
        let lua = Lua::new();
        let api = lua.create_table().unwrap();
        store.install(&lua, &api, deadline()).unwrap();
        lua.globals().set("rift", api).unwrap();
        lua
    }

    #[test]
    fn config_requires_an_explicit_nonempty_path_and_rejects_unknown_fields() {
        let lua = Lua::new();
        assert!(parse(Value::Nil).unwrap().is_none());
        for source in [
            "true",
            "{}",
            "{path=2}",
            "{path=''}",
            "{path='a', typo=true}",
            "{path='a', [1]=true}",
            "{path='a\\0b'}",
        ] {
            assert!(parse(lua.load(source).eval().unwrap()).is_err(), "{source}");
        }
        assert_eq!(
            parse(lua.load("{path='state.bin'}").eval().unwrap()).unwrap(),
            Some(StorageConfig {
                path: "state.bin".into()
            })
        );
    }

    #[test]
    fn namespaces_binary_values_and_deletes_survive_reopening() {
        let temp = Temporary::new();
        let config = temp.config();
        let store = Store::open(Some(&config)).unwrap();
        lua(&store)
            .load(
                r#"
            assert(rift.store.get('one', 'key') == nil)
            rift.store.set('one', 'key', 'hello\0\255')
            rift.store.set('two', 'key', '')
            assert(rift.store.increment('one', 'counter', 3) == 3)
            assert(rift.store.increment('one', 'counter', -1) == 2)
            rift.store.set('two', 'deleted', 'value')
            assert(rift.store.delete('two', 'deleted'))
            assert(not rift.store.delete('two', 'deleted'))
        "#,
            )
            .exec()
            .unwrap();
        drop(store);
        let reopened = Store::open(Some(&config)).unwrap();
        lua(&reopened)
            .load(
                r#"
            assert(rift.store.get('one', 'key') == 'hello\0\255')
            assert(rift.store.get('two', 'key') == '')
            assert(rift.store.get('one', 'counter') == '2')
            assert(rift.store.get('two', 'deleted') == nil)
        "#,
            )
            .exec()
            .unwrap();
    }

    #[test]
    fn concurrent_lua_callbacks_increment_without_losing_updates() {
        let store = Store::open(None).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || {
                    lua(&store)
                        .load("for i=1,100 do rift.store.increment('shared', 'count', 1) end")
                        .exec()
                        .unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            store
                .get("shared".into(), "count".into(), deadline())
                .unwrap(),
            Some(b"800".to_vec())
        );
    }

    #[test]
    fn invalid_types_limits_and_integer_overflow_leave_values_unchanged() {
        let store = Store::open(None).unwrap();
        lua(&store)
            .load(
                r#"
            rift.store.set('n', 'k', '9223372036854775807')
            assert(not pcall(rift.store.increment, 'n', 'k', 1))
            assert(rift.store.get('n', 'k') == '9223372036854775807')
            rift.store.set('n', 'k', 'not a number')
            assert(not pcall(rift.store.increment, 'n', 'k', 1))
            assert(rift.store.get('n', 'k') == 'not a number')
            assert(not pcall(rift.store.increment, 'n', 'k', 1.5))
            assert(not pcall(rift.store.increment, 'n', 'k', '1'))
            assert(not pcall(rift.store.set, 1, 'k', 'v'))
            assert(not pcall(rift.store.set, 'n', 1, 'v'))
            assert(not pcall(rift.store.set, 'n', 'k', 1))
            assert(not pcall(rift.store.get, '', 'k'))
            assert(not pcall(rift.store.get, 'n', string.rep('k', 129)))
            assert(not pcall(rift.store.set, 'n', 'k', string.rep('v', 65537)))
            assert(rift.store.get('n', 'k') == 'not a number')
            rift.store.set(string.rep('n',128), string.rep('k',128), string.rep('v',65536))
        "#,
            )
            .exec()
            .unwrap();
        assert!(
            store
                .mutate(deadline(), |entries| {
                    for i in 0..PLUGIN_QUOTA.entries {
                        entries.insert(("n".into(), i.to_string()), vec![]);
                    }
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(store.memory().entries.len(), 2);
        assert!(
            store
                .mutate(deadline(), |entries| {
                    for i in 0..16 {
                        entries.insert(("n".into(), i.to_string()), vec![0; MAX_VALUE]);
                    }
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(store.memory().entries.len(), 2);
    }

    #[test]
    fn increment_rejects_values_outside_luajit_exact_integer_range() {
        let store = Store::open(None).unwrap();
        lua(&store)
            .load(
                r#"
            rift.store.set('n', 'positive', '9007199254740990')
            assert(rift.store.increment('n', 'positive', 1) == 9007199254740991)
            assert(not pcall(rift.store.increment, 'n', 'positive', 1))
            assert(rift.store.get('n', 'positive') == '9007199254740991')
            rift.store.set('n', 'negative', '-9007199254740990')
            assert(rift.store.increment('n', 'negative', -1) == -9007199254740991)
            assert(not pcall(rift.store.increment, 'n', 'negative', -1))
            assert(rift.store.get('n', 'negative') == '-9007199254740991')
            assert(not pcall(rift.store.increment, 'n', 'new', 9007199254740992))
            assert(not pcall(rift.store.increment, 'n', 'new', -9007199254740992))
            assert(rift.store.get('n', 'new') == nil)
            assert(rift.store.increment('n', 'new', 9007199254740991) == 9007199254740991)
            assert(rift.store.increment('n', 'new', -9007199254740991) == 0)
            for _, unsafe in ipairs({'9007199254740992', '-9007199254740992'}) do
                rift.store.set('n', 'unsafe', unsafe)
                assert(not pcall(rift.store.increment, 'n', 'unsafe', 0))
                assert(rift.store.get('n', 'unsafe') == unsafe)
            end
        "#,
            )
            .exec()
            .unwrap();
    }

    #[test]
    fn expired_operations_cannot_mutate_state() {
        let store = Store::open(None).unwrap();
        let lua = Lua::new();
        let api = lua.create_table().unwrap();
        let expires = Instant::now() + Duration::from_millis(10);
        store.install(&lua, &api, expires).unwrap();
        lua.globals().set("rift", api).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        lua.load(
            r#"
            assert(not pcall(rift.store.set, 'n', 'k', 'v'))
            assert(not pcall(rift.store.increment, 'n', 'k', 1))
            assert(not pcall(rift.store.delete, 'n', 'k'))
            assert(not pcall(rift.store.get, 'n', 'k'))
        "#,
        )
        .exec()
        .unwrap();
        assert!(store.memory().entries.is_empty());
    }

    #[test]
    fn contended_writes_stop_at_the_deadline_while_reads_continue() {
        let store = Store::open(None).unwrap();
        let held = store.0.writer.lock().unwrap();
        let waiting = store.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let read = waiting.get("n".into(), "k".into(), deadline());
            let write = waiting.mutate(Instant::now() + Duration::from_millis(20), |entries| {
                entries.insert(("n".into(), "k".into()), b"v".to_vec());
                Ok(())
            });
            send.send((read.unwrap(), write.unwrap_err().kind()))
                .unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(1));
        drop(held);
        thread.join().unwrap();
        assert_eq!(result.unwrap(), (None, io::ErrorKind::TimedOut));
        assert!(store.memory().entries.is_empty());
    }

    #[test]
    fn failed_commits_change_neither_memory_nor_the_database() {
        let temp = Temporary::new();
        let config = temp.config();
        let store = Store::open(Some(&config)).unwrap();
        lua(&store)
            .load("rift.store.set('n', 'k', 'old')")
            .exec()
            .unwrap();
        store
            .0
            .writer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .execute_batch(
                "CREATE TEMP TRIGGER fail BEFORE INSERT ON main.entries WHEN NEW.key = 'fail'
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let result = store.mutate(deadline(), |entries| {
            entries.insert(("n".into(), "k".into()), b"new".to_vec());
            entries.insert(("n".into(), "fail".into()), b"v".to_vec());
            Ok(())
        });
        assert!(result.unwrap_err().to_string().contains("injected"));
        let usage = (store.memory().plugin, store.memory().host);
        assert_eq!(
            store.get("n".into(), "k".into(), deadline()).unwrap(),
            Some(b"old".to_vec())
        );
        lua(&store)
            .load("rift.store.set('n', 'other', 'v')")
            .exec()
            .unwrap();
        assert_eq!(usage.0.entries, 1);
        drop(store);
        let reopened = Store::open(Some(&config)).unwrap();
        assert_eq!(
            reopened.namespace("n"),
            vec![
                ("k".to_owned(), b"old".to_vec()),
                ("other".to_owned(), b"v".to_vec())
            ]
        );
        // Only the database remains: no temporary files, WAL or backup.
        drop(reopened);
        assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 1);
    }

    #[test]
    fn a_second_runtime_cannot_open_a_store_in_use() {
        let temp = Temporary::new();
        let config = temp.config();
        let store = Store::open(Some(&config)).unwrap();
        assert_eq!(
            Store::open(Some(&config)).err().unwrap().kind(),
            io::ErrorKind::ResourceBusy
        );
        drop(store);
        Store::open(Some(&config)).unwrap();
    }

    #[test]
    fn version_1_snapshots_are_converted_and_backed_up() {
        let temp = Temporary::new();
        let config = temp.config();
        let entries = BTreeMap::from([
            (("n".into(), "k".into()), b"v\0\xff".to_vec()),
            (("n".into(), "empty".into()), vec![]),
            (
                (MEMBER_NAMESPACE.into(), "member".into()),
                b"staff".to_vec(),
            ),
        ]);
        let snapshot = encode(&entries).unwrap();
        fs::write(&config.path, &snapshot).unwrap();
        let store = Store::open(Some(&config)).unwrap();
        assert_eq!(store.memory().entries, entries);
        drop(store);
        assert!(fs::read(&config.path).unwrap().starts_with(SQLITE_HEADER));
        assert_eq!(fs::read(backup_path(&config.path)).unwrap(), snapshot);
        let reopened = Store::open(Some(&config)).unwrap();
        assert_eq!(reopened.memory().entries, entries);
        assert_eq!(reopened.memory().plugin.entries, 2);
        assert_eq!(reopened.memory().host.entries, 1);
    }

    #[test]
    fn checking_a_store_changes_nothing() {
        let temp = Temporary::new();
        let config = temp.config();
        let wal = PathBuf::from(format!("{}-wal", config.path.display()));
        let listing = || {
            let mut names: Vec<_> = fs::read_dir(&temp.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            names
        };

        assert_eq!(check_file(&config.path).unwrap(), Check::Missing);
        assert!(listing().is_empty());

        let entries = BTreeMap::from([
            (("n".into(), "k".into()), b"v".to_vec()),
            ((MEMBER_NAMESPACE.into(), "m".into()), b"staff".to_vec()),
        ]);
        let snapshot = encode(&entries).unwrap();
        fs::write(&config.path, &snapshot).unwrap();
        assert_eq!(
            check_file(&config.path).unwrap(),
            Check::Snapshot { entries: 2 }
        );
        assert_eq!(fs::read(&config.path).unwrap(), snapshot);
        assert_eq!(listing().len(), 1, "no backup or database is written");

        // Leave committed writes in the WAL, as a crash would.
        fs::remove_file(&config.path).unwrap();
        let store = Store::open(Some(&config)).unwrap();
        lua(&store)
            .load("rift.store.set('a', 'b', 'c') rift.store.set('a', 'd', 'e')")
            .exec()
            .unwrap();
        let before = (fs::read(&config.path).unwrap(), fs::read(&wal).unwrap());
        let names = listing();
        assert_eq!(
            check_file(&config.path).unwrap(),
            Check::Database { entries: 2 }
        );
        assert_eq!(
            (fs::read(&config.path).unwrap(), fs::read(&wal).unwrap()),
            before
        );
        assert_eq!(listing(), names);
        drop(store);

        fs::remove_file(&config.path).unwrap();
        Connection::open(&config.path)
            .unwrap()
            .execute_batch("CREATE TABLE entries (namespace TEXT, key TEXT, value BLOB);")
            .unwrap();
        let error = check_file(&config.path).unwrap_err().to_string();
        assert!(error.contains("not a Rift extension store"), "{error}");
        assert!(
            error.contains(&config.path.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn other_sqlite_databases_are_rejected() {
        let temp = Temporary::new();
        let config = temp.config();
        Connection::open(&config.path)
            .unwrap()
            .execute_batch("CREATE TABLE entries (namespace TEXT, key TEXT, value BLOB);")
            .unwrap();
        let error = Store::open(Some(&config)).err().unwrap();
        assert!(
            error.to_string().contains("not a Rift extension store"),
            "{error}"
        );
    }

    #[test]
    fn invalid_rows_fail_startup() {
        let temp = Temporary::new();
        let config = temp.config();
        drop(Store::open(Some(&config)).unwrap());
        Connection::open(&config.path)
            .unwrap()
            .execute(
                "INSERT INTO entries VALUES ('n', ?1, X'')",
                [&"k".repeat(MAX_LABEL + 1)],
            )
            .unwrap();
        assert!(Store::open(Some(&config)).is_err());
    }

    #[test]
    fn host_data_has_its_own_quota() {
        let store = Store::open(None).unwrap();
        store
            .mutate(deadline(), |entries| {
                for i in 0..PLUGIN_QUOTA.entries {
                    entries.insert(("n".into(), i.to_string()), vec![]);
                }
                Ok(())
            })
            .unwrap();
        let plugin = |key: &str| {
            store.mutate(deadline(), |entries| {
                entries.insert(("n".into(), key.into()), vec![]);
                Ok(())
            })
        };
        assert!(plugin("full").is_err());
        // Replacing an existing plugin entry stays possible at the limit.
        plugin("0").unwrap();
        store
            .mutate(deadline(), |entries| {
                for i in 0..PLUGIN_QUOTA.entries {
                    entries.insert((MEMBER_NAMESPACE.into(), i.to_string()), vec![]);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(store.memory().host.entries, PLUGIN_QUOTA.entries);
        let host = store.mutate(deadline(), |entries| {
            for i in 0..=HOST_QUOTA.entries - PLUGIN_QUOTA.entries {
                entries.insert(("rift.other".into(), i.to_string()), vec![]);
            }
            Ok(())
        });
        assert!(host.unwrap_err().to_string().contains("host data"));
        assert_eq!(store.memory().host.entries, PLUGIN_QUOTA.entries);
        // Freeing plugin space does not depend on host data.
        store
            .mutate(deadline(), |entries| {
                Ok(entries.remove(&("n".into(), "1".into())))
            })
            .unwrap();
        plugin("full").unwrap();
    }

    #[test]
    fn malformed_truncated_oversized_and_duplicate_snapshots_fail_startup() {
        let temp = Temporary::new();
        let config = temp.config();
        let entries = BTreeMap::from([(("n".into(), "k".into()), b"v".to_vec())]);
        let valid = encode(&entries).unwrap();
        let mut trailing = valid.clone();
        trailing.push(0);
        let mut duplicate = valid.clone();
        duplicate[8..12].copy_from_slice(&2u32.to_le_bytes());
        duplicate.extend_from_slice(&valid[12..]);
        let mut invalid_utf8 = valid.clone();
        invalid_utf8[20] = 255;
        for bytes in [
            b"not a store".to_vec(),
            vec![],
            valid[..valid.len() - 1].to_vec(),
            trailing,
            duplicate,
            invalid_utf8,
            vec![0; MAX_BYTES + 1],
        ] {
            fs::write(&config.path, bytes).unwrap();
            assert!(Store::open(Some(&config)).is_err());
        }
    }

    #[test]
    #[cfg(unix)]
    fn opening_a_fifo_fails_instead_of_blocking_startup() {
        let temp = Temporary::new();
        let config = temp.config();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&config.path)
                .status()
                .unwrap()
                .success()
        );
        let (send, receive) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let error = Store::open(Some(&config)).err().unwrap();
            send.send(error.kind()).unwrap();
        });
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            io::ErrorKind::InvalidData
        );
        thread.join().unwrap();
    }
}
