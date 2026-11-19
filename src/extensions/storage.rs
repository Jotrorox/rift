//! Bounded host-owned state, shared by every callback in an extension runtime.
//!
//! A configured store is either a bundled-SQLite database of its own (`path`)
//! or the table `extension_entries` in the instance database
//! (`database = true`), which is a SQLite file, PostgreSQL or MariaDB. Each mutation
//! commits only the entries it changes. A file store runs in WAL mode with
//! `synchronous=FULL`, so a write costs one small append and sync instead of
//! rewriting the store; its connection holds an exclusive lock: one runtime
//! owns a path, and a second process fails at startup. The instance database
//! has one owner already, so a store kept there needs no lock of its own.
//!
//! Memory mirrors committed rows. Writers are serialized and change memory only
//! after their transaction commits; readers use the mirror and never wait for a
//! write to sync.
//!
//! A mutation of the store in the instance database is a [`KvOp`] (its own
//! deterministic log entry in a cluster): it carries the values the mutation
//! read, the changes and the quotas, and [`apply`] checks the reads and the
//! quotas against the database before writing. Its memory follows only from
//! the apply hook the store registers.
//!
//! Version 1 stores were checksummed binary snapshots. Startup converts one into
//! a database built beside it, atomically replaces the snapshot, and keeps the
//! original bytes in `<file>.v1-backup`.
use crate::database::{
    self, Conn, Database, Dialect, HookId, OptionalExtension as _,
    operation::{Operation, OrMessage, Outcome},
};
use mlua::{Lua, Table, Value};
use rusqlite::{Connection, ErrorCode, OpenFlags, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, RwLock, TryLockError, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_LABEL: usize = 128;
const MAX_VALUE: usize = 64 * 1024;
/// Upper bound of a version 1 snapshot.
const MAX_BYTES: usize = 1024 * 1024;
// LuaJIT numbers cannot distinguish every integer outside this range.
const MAX_INTEGER: i64 = (1 << 53) - 1;
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
/// The component name and schema version of `extension_entries` in the
/// instance database.
const SHARED_COMPONENT: &str = "extension_store";
const SHARED_VERSION: u32 = 1;
const SHARED_UPSERT: &str =
    "INSERT INTO extension_entries (namespace, key, value) VALUES (?1, ?2, ?3)
     ON CONFLICT (namespace, key) DO UPDATE SET value = excluded.value";
const SHARED_DELETE: &str = "DELETE FROM extension_entries WHERE namespace = ?1 AND key = ?2";
/// Namespaces beginning with this prefix belong to the host, e.g. group memberships.
const HOST_PREFIX: &str = "rift.";
/// Persistent plugins' private namespaces, `plugin.<name>.<namespace>`. Each
/// plugin has its own quota and, like host data, is out of reach of the
/// configuration script's `rift.store`.
const PLUGIN_PREFIX: &str = "plugin.";
/// Configurable quota bounds. Memory mirrors every entry, so the totals stay finite.
const MAX_QUOTA_ENTRIES: usize = 1 << 20;
const MAX_QUOTA_BYTES: usize = 1 << 30;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
pub(crate) type Key = (String, String);
pub(super) type Entries = BTreeMap<Key, Vec<u8>>;

/// Limits for one owner's data. Host data, the configuration script and each
/// persistent plugin have their own, so one plugin filling its space cannot
/// block another plugin, `rift.store` or permission group changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    pub entries: usize,
    pub bytes: usize,
}
const DEFAULT_QUOTA: Quota = Quota {
    entries: 4096,
    bytes: 1024 * 1024,
};
const HOST_QUOTA: Quota = Quota {
    entries: 65536,
    bytes: 8 * 1024 * 1024,
};

/// Quotas for data other than the host's: `default` for the configuration
/// script and every plugin, unless `plugins` names one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quotas {
    pub default: Quota,
    pub plugins: BTreeMap<String, Quota>,
}

impl Default for Quotas {
    fn default() -> Self {
        Self {
            default: DEFAULT_QUOTA,
            plugins: BTreeMap::new(),
        }
    }
}

impl Quotas {
    fn get(&self, owner: &Owner) -> Quota {
        match owner {
            Owner::Script => self.default,
            Owner::Plugin(name) => self.plugins.get(name).copied().unwrap_or(self.default),
        }
    }
}

/// Who a non-host namespace's data counts against.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Owner {
    /// `rift.store` namespaces of the configuration script.
    Script,
    /// `plugin.<name>.<namespace>`. Plugin names contain no dots.
    Plugin(String),
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Script => f.write_str("configuration script"),
            Self::Plugin(name) => write!(f, "plugin {name:?}"),
        }
    }
}

fn host_namespace(namespace: &str) -> bool {
    namespace.starts_with(HOST_PREFIX)
}

/// The owner of a namespace, or `None` for host data.
fn owner(namespace: &str) -> Option<Owner> {
    if host_namespace(namespace) {
        return None;
    }
    Some(match namespace.strip_prefix(PLUGIN_PREFIX) {
        Some(rest) => Owner::Plugin(rest.split('.').next().unwrap_or_default().to_owned()),
        None => Owner::Script,
    })
}

/// Bytes an entry counts toward its quota: 8 bytes of overhead plus its contents.
fn entry_size((namespace, key): &Key, value: &[u8]) -> usize {
    8 + namespace.len() + key.len() + value.len()
}

/// Where a configured store keeps its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// `path`: a SQLite file of the store's own.
    File(PathBuf),
    /// `database = true`: `extension_entries` in the instance database.
    Database,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StorageConfig {
    pub target: Target,
    pub quotas: Quotas,
}

/// Reject keys of `table` outside `allowed`.
fn only_fields(table: &Table, allowed: &[&str], name: &str) -> mlua::Result<()> {
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let known = matches!(&key, Value::String(key)
            if allowed.iter().any(|field| key.as_bytes().as_ref() == field.as_bytes()));
        if !known {
            return Err(mlua::Error::runtime(format!("unknown {name} field")));
        }
    }
    Ok(())
}

fn quota_limit(table: &Table, field: &str, name: &str, max: usize) -> mlua::Result<Option<usize>> {
    match table.raw_get::<Value>(field)? {
        Value::Nil => Ok(None),
        Value::Integer(value) if value >= 1 && value as u64 <= max as u64 => {
            Ok(Some(value as usize))
        }
        _ => Err(mlua::Error::runtime(format!(
            "{name}.{field} must be an integer from 1 to {max}"
        ))),
    }
}

/// `{ entries = N, bytes = N }`; a missing field keeps `base`'s.
fn parse_quota(value: Value, name: &str, base: Quota) -> mlua::Result<Quota> {
    let Value::Table(table) = value else {
        return Err(mlua::Error::runtime(format!("{name} must be a table")));
    };
    only_fields(&table, &["entries", "bytes"], name)?;
    Ok(Quota {
        entries: quota_limit(&table, "entries", name, MAX_QUOTA_ENTRIES)?.unwrap_or(base.entries),
        bytes: quota_limit(&table, "bytes", name, MAX_QUOTA_BYTES)?.unwrap_or(base.bytes),
    })
}

fn parse_quotas(value: Value) -> mlua::Result<Quotas> {
    const NAME: &str = "extensions.storage.quota";
    let mut quotas = Quotas::default();
    let table = match value {
        Value::Nil => return Ok(quotas),
        Value::Table(table) => table,
        _ => return Err(mlua::Error::runtime(format!("{NAME} must be a table"))),
    };
    only_fields(&table, &["entries", "bytes", "plugins"], NAME)?;
    quotas.default = Quota {
        entries: quota_limit(&table, "entries", NAME, MAX_QUOTA_ENTRIES)?
            .unwrap_or(DEFAULT_QUOTA.entries),
        bytes: quota_limit(&table, "bytes", NAME, MAX_QUOTA_BYTES)?.unwrap_or(DEFAULT_QUOTA.bytes),
    };
    match table.raw_get::<Value>("plugins")? {
        Value::Nil => {}
        Value::Table(plugins) => {
            for pair in plugins.pairs::<Value, Value>() {
                let (name, quota) = pair?;
                let name = match &name {
                    Value::String(name) => name.to_str()?.to_owned(),
                    _ => String::new(),
                };
                if !crate::plugins::manifest::valid_name(&name) {
                    return Err(mlua::Error::runtime(format!(
                        "{NAME}.plugins keys must be plugin names"
                    )));
                }
                let quota = parse_quota(quota, &format!("{NAME}.plugins.{name}"), quotas.default)?;
                quotas.plugins.insert(name, quota);
            }
        }
        _ => {
            return Err(mlua::Error::runtime(format!(
                "{NAME}.plugins must be a table"
            )));
        }
    }
    Ok(quotas)
}

pub(super) fn parse(value: Value) -> mlua::Result<Option<StorageConfig>> {
    let table = match value {
        Value::Nil => return Ok(None),
        Value::Table(table) => table,
        _ => return Err(mlua::Error::runtime("extensions.storage must be a table")),
    };
    only_fields(&table, &["path", "database", "quota"], "extensions.storage")?;
    let path = match table.raw_get::<Value>("path")? {
        Value::Nil => None,
        Value::String(path) => {
            let path = path.to_str()?;
            if path.is_empty() || path.contains('\0') {
                return Err(mlua::Error::runtime(
                    "extensions.storage.path must be nonempty and contain no NUL bytes",
                ));
            }
            Some(PathBuf::from(path.as_ref()))
        }
        _ => {
            return Err(mlua::Error::runtime(
                "extensions.storage.path must be a string",
            ));
        }
    };
    // `database = false` means the same as leaving it out.
    let database = match table.raw_get::<Value>("database")? {
        Value::Nil => false,
        Value::Boolean(database) => database,
        _ => {
            return Err(mlua::Error::runtime(
                "extensions.storage.database must be a boolean",
            ));
        }
    };
    let target = match (path, database) {
        (Some(path), false) => Target::File(path),
        (None, true) => Target::Database,
        (Some(_), true) => {
            return Err(mlua::Error::runtime(
                "extensions.storage: set either path or database = true, not both",
            ));
        }
        (None, false) => {
            return Err(mlua::Error::runtime(
                "extensions.storage needs path (a store file) or database = true (the instance database)",
            ));
        }
    };
    Ok(Some(StorageConfig {
        target,
        quotas: parse_quotas(table.raw_get("quota")?)?,
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
    /// Usage of each owner with at least one entry.
    owners: BTreeMap<Owner, Usage>,
    host: Usage,
}

impl Memory {
    /// Host data must fit its fixed quota. Owners may exceed theirs, e.g.
    /// after the configuration lowers one; they can then only shrink.
    fn new(entries: Entries) -> io::Result<Self> {
        let mut memory = Self::default();
        for (key, value) in &entries {
            validate_entry(key, value)?;
            let usage = match owner(&key.0) {
                None => &mut memory.host,
                Some(owner) => memory.owners.entry(owner).or_default(),
            };
            usage.entries += 1;
            usage.bytes += entry_size(key, value);
        }
        check(&memory.host, &HOST_QUOTA, "host")?;
        memory.entries = entries;
        Ok(memory)
    }

    /// Install committed changes and their usage.
    fn apply<'a>(&mut self, changes: impl IntoIterator<Item = (&'a Key, &'a Option<Vec<u8>>)>) {
        let Self {
            entries,
            owners,
            host,
        } = self;
        for (key, value) in changes {
            let who = owner(&key.0);
            let usage = match &who {
                None => &mut *host,
                Some(who) => owners.entry(who.clone()).or_default(),
            };
            if let Some(old) = entries.get(key) {
                usage.entries = usage.entries.saturating_sub(1);
                usage.bytes = usage.bytes.saturating_sub(entry_size(key, old));
            }
            if let Some(value) = value {
                usage.entries += 1;
                usage.bytes += entry_size(key, value);
            }
            if let Some(who) = who
                && owners.get(&who) == Some(&Usage::default())
            {
                owners.remove(&who);
            }
            match value {
                Some(value) => entries.insert(key.clone(), value.clone()),
                None => entries.remove(key),
            };
        }
    }
}

fn check(usage: &Usage, quota: &Quota, owner: impl std::fmt::Display) -> io::Result<()> {
    if usage.entries > quota.entries {
        return Err(invalid(format!(
            "extension store {owner} data exceeds {} entries",
            quota.entries
        )));
    }
    if usage.bytes > quota.bytes {
        return Err(invalid(format!(
            "extension store {owner} data exceeds {} KiB",
            quota.bytes.div_ceil(1024)
        )));
    }
    Ok(())
}

/// Like [`check`], but a measure already over its quota may stay or shrink.
fn check_growth(old: &Usage, new: &Usage, quota: &Quota, owner: &Owner) -> io::Result<()> {
    let capped = |old: usize, new: usize, max: usize| if new <= old { max.max(new) } else { max };
    check(
        new,
        &Quota {
            entries: capped(old.entries, new.entries, quota.entries),
            bytes: capped(old.bytes, new.bytes, quota.bytes),
        },
        owner,
    )
}

/// Pending changes over the committed entries, applied only after they commit.
pub(crate) struct Transaction<'a> {
    base: &'a Entries,
    changes: BTreeMap<Key, Option<Vec<u8>>>,
    /// The committed value of every key read before this mutation changed
    /// it: what the mutation's result depends on.
    reads: RefCell<BTreeMap<Key, Option<Vec<u8>>>>,
}

impl Transaction<'_> {
    pub(crate) fn get(&self, key: &Key) -> Option<&Vec<u8>> {
        match self.changes.get(key) {
            Some(change) => change.as_ref(),
            None => {
                let value = self.base.get(key);
                self.reads
                    .borrow_mut()
                    .entry(key.clone())
                    .or_insert_with(|| value.cloned());
                value
            }
        }
    }

    pub(crate) fn insert(&mut self, key: Key, value: Vec<u8>) {
        self.changes.insert(key, Some(value));
    }

    /// Whether an entry was removed.
    pub(crate) fn remove(&mut self, key: &Key) -> bool {
        let existed = self.get(key).is_some();
        if existed {
            self.changes.insert(key.clone(), None);
        }
        existed
    }
}

/// Where commits go.
enum Writer {
    /// A memory-only store.
    Memory,
    /// The store's own SQLite file.
    File(Connection),
    /// `extension_entries` in the instance database.
    Shared(Arc<Database>),
}

struct Inner {
    memory: RwLock<Memory>,
    quotas: Quotas,
    /// Serializes writers.
    writer: Mutex<Writer>,
    /// The apply hook of a store in the instance database.
    hook: Option<HookId>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(hook) = self.hook
            && let Writer::Shared(database) =
                self.writer.get_mut().unwrap_or_else(|e| e.into_inner())
        {
            database.remove_hook(hook);
        }
    }
}

impl Inner {
    /// The apply hook: install the changes of a mutation, or reload the
    /// table when the change came without them (a full reload).
    fn refresh(&self, connection: &Conn, change: &database::Change<'_>) {
        let mut memory = self.memory.write().unwrap_or_else(|e| e.into_inner());
        if !change.reload
            && let Some(Outcome::Kv(KvOutcome::Applied { changes })) = change.outcome
        {
            memory.apply(changes.iter().map(|entry| (&entry.key, &entry.value)));
            return;
        }
        match load_shared(connection).and_then(Memory::new) {
            Ok(fresh) => *memory = fresh,
            Err(error) => log_error!("extension store not reloaded: {error}"),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Store(Arc<Inner>);

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn deadline_exceeded() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "extension store deadline exceeded")
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(deadline_exceeded());
    }
    Ok(())
}

fn exact_integer(value: i64) -> io::Result<i64> {
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
    if namespace.starts_with(PLUGIN_PREFIX) {
        return Err(mlua::Error::runtime(format!(
            "store namespaces beginning with {PLUGIN_PREFIX:?} are reserved for persistent plugins"
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
    fn new(memory: Memory, quotas: Quotas, writer: Writer) -> Self {
        Self(Arc::new(Inner {
            memory: RwLock::new(memory),
            quotas,
            writer: Mutex::new(writer),
            hook: None,
        }))
    }

    /// A store in the instance database, whose memory follows the table
    /// through an apply hook.
    fn shared(memory: Memory, quotas: Quotas, database: Arc<Database>) -> Self {
        Self(Arc::new_cyclic(|inner: &Weak<Inner>| {
            let watched = inner.clone();
            let hook = database.on_change(TABLES, move |connection, change| {
                if let Some(inner) = watched.upgrade() {
                    inner.refresh(connection, change);
                }
            });
            Inner {
                memory: RwLock::new(memory),
                quotas,
                writer: Mutex::new(Writer::Shared(database)),
                hook: Some(hook),
            }
        }))
    }

    /// Open the configured store, or a memory-only one for `None`.
    /// `database` is the instance database, which the database target needs.
    pub(crate) fn open(
        config: Option<&StorageConfig>,
        database: Option<&Arc<Database>>,
    ) -> io::Result<Self> {
        let Some(config) = config else {
            return Ok(Self::new(
                Memory::default(),
                Quotas::default(),
                Writer::Memory,
            ));
        };
        let path = match &config.target {
            Target::File(path) => path.as_path(),
            Target::Database => {
                let Some(database) = database else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "extensions.storage.database needs the instance database, which Rift opens when it runs from a configuration file",
                    ));
                };
                prepare_shared(database)?;
                // Loaded and watched under the connection, so no write falls
                // between them.
                let connection = database.lock();
                let entries = load_shared(&connection).map_err(|e| shared_error(database, e))?;
                let store = Self::shared(
                    Memory::new(entries)?,
                    config.quotas.clone(),
                    database.clone(),
                );
                drop(connection);
                return Ok(store);
            }
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
        Ok(Self::new(
            Memory::new(entries)?,
            config.quotas.clone(),
            Writer::File(connection),
        ))
    }

    fn lock(&self, deadline: Instant) -> io::Result<MutexGuard<'_, Writer>> {
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

    pub(crate) fn get(
        &self,
        namespace: String,
        key: String,
        deadline: Instant,
    ) -> io::Result<Option<Vec<u8>>> {
        check_deadline(deadline)?;
        Ok(self.memory().entries.get(&(namespace, key)).cloned())
    }

    /// Add `delta` to a decimal integer entry (missing counts as 0) within
    /// the exact Lua number range, as `rift.store.increment` does.
    pub(crate) fn increment(&self, key: Key, delta: i64, deadline: Instant) -> io::Result<i64> {
        let delta = exact_integer(delta)?;
        self.mutate(deadline, |entries| {
            let current = match entries.get(&key) {
                Some(value) => std::str::from_utf8(value)
                    .ok()
                    .and_then(|value| value.parse::<i64>().ok())
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
    }

    /// Committed entries in one namespace, keyed by entry key.
    pub(crate) fn namespace(&self, namespace: &str) -> Vec<(String, Vec<u8>)> {
        self.memory()
            .entries
            .iter()
            .filter(|((entry, _), _)| entry == namespace)
            .map(|((_, key), value)| (key.clone(), value.clone()))
            .collect()
    }

    pub(crate) fn mutate<T>(
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
        let (result, changes, reads) = {
            let memory = self.memory();
            let mut transaction = Transaction {
                base: &memory.entries,
                changes: BTreeMap::new(),
                reads: RefCell::new(BTreeMap::new()),
            };
            let result = change(&mut transaction)?;
            let reads = transaction.reads.into_inner();
            let mut changes = transaction.changes;
            changes.retain(|key, value| memory.entries.get(key) != value.as_ref());
            let mut host = memory.host;
            // Old and new usage of each owner this mutation touches.
            let mut owners: BTreeMap<Owner, (Usage, Usage)> = BTreeMap::new();
            for (key, value) in &changes {
                let usage = match owner(&key.0) {
                    None => &mut host,
                    Some(owner) => {
                        let current = memory.owners.get(&owner).copied().unwrap_or_default();
                        &mut owners.entry(owner).or_insert((current, current)).1
                    }
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
            check(&host, &HOST_QUOTA, "host")?;
            for (owner, (old, new)) in &owners {
                check_growth(old, new, &self.0.quotas.get(owner), owner)?;
            }
            (result, changes, reads)
        };
        if !changes.is_empty() {
            check_deadline(deadline)?;
            match &mut *writer {
                Writer::Memory => {}
                Writer::File(connection) => commit(connection, &changes, deadline)
                    .map_err(|error| error.into_io("cannot commit extension store change"))?,
                // Memory follows from the apply hook before this returns.
                Writer::Shared(database) => {
                    return commit_shared(database, &self.0.quotas, reads, changes, deadline).map(
                        |()| {
                            committed(&result);
                            result
                        },
                    );
                }
            }
            // The commit is durable. Do not return a deadline error after it:
            // memory must agree with the database.
            self.0
                .memory
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .apply(&changes);
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
                store
                    .increment(key, delta, deadline)
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

/// [`commit`] for a store in the instance database: one [`KvOp`] through
/// the database's executor. The connection is shared with the other
/// components, so waiting for it also stops at `deadline`.
fn commit_shared(
    database: &Database,
    quotas: &Quotas,
    reads: BTreeMap<Key, Option<Vec<u8>>>,
    changes: BTreeMap<Key, Option<Vec<u8>>>,
    deadline: Instant,
) -> io::Result<()> {
    let entries = |map: BTreeMap<Key, Option<Vec<u8>>>| {
        map.into_iter()
            .map(|(key, value)| KvEntry { key, value })
            .collect()
    };
    let op = KvOp::Mutate {
        quotas: quotas.clone(),
        expect: entries(reads),
        changes: entries(changes),
    };
    let outcome = database
        .run_until(Operation::Kv(op), deadline)
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => deadline_exceeded(),
            _ => error,
        })?;
    match outcome {
        Outcome::Kv(KvOutcome::Applied { .. }) => Ok(()),
        Outcome::Kv(KvOutcome::Rejected { message }) => Err(invalid(message)),
        Outcome::Kv(KvOutcome::Conflict { key }) => Err(database.error(format!(
            "extension store entry {:?} in {:?} changed during the mutation; retry",
            key.1, key.0
        ))),
        Outcome::Kv(KvOutcome::Failed { message }) => {
            Err(database.error(format!("cannot commit extension store change: {message}")))
        }
        other => Err(database.error(format!("unexpected outcome {other:?}"))),
    }
}

/// The table of a store in the instance database.
pub const TABLES: &[&str] = &["extension_entries"];

/// One entry of a [`KvOp`]: a value, or `None` for none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvEntry {
    /// Namespace and key.
    pub key: (String, String),
    pub value: Option<Vec<u8>>,
}

/// A mutation of the store in the instance database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum KvOp {
    /// Write `changes` in one transaction if every entry in `expect` still
    /// holds the value the mutation read, and if the quotas (`quotas` for
    /// scripts and plugins, the fixed host quota for host data) allow the
    /// growth.
    Mutate {
        quotas: Quotas,
        expect: Vec<KvEntry>,
        changes: Vec<KvEntry>,
    },
}

/// The result of a [`KvOp`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum KvOutcome {
    /// Written; the entries that differed from the table.
    Applied {
        changes: Vec<KvEntry>,
    },
    /// An entry the mutation read has changed since: run it again.
    Conflict {
        key: (String, String),
    },
    /// Invalid, or over a quota; nothing was written.
    Rejected {
        message: String,
    },
    Failed {
        message: String,
    },
}

impl KvOutcome {
    pub fn failed(&self) -> bool {
        !matches!(self, Self::Applied { .. })
    }
}

fn current(connection: &Conn, (namespace, key): &Key) -> database::Result<Option<Vec<u8>>> {
    connection
        .query_row(
            "SELECT value FROM extension_entries WHERE namespace = ?1 AND key = ?2",
            crate::database::params![namespace, key],
            |row| row.get(0),
        )
        .optional()
}

/// The usage of one owner (`None`: host data) in the table, through ranges
/// of the primary key.
fn usage(connection: &Conn, owner: Option<&Owner>) -> database::Result<Usage> {
    let dialect = connection.dialect();
    // MariaDB casts to a 64-bit integer as SIGNED; it has no BIGINT cast.
    let integer = match dialect {
        Dialect::Mariadb => "SIGNED",
        Dialect::Sqlite | Dialect::Postgres => "BIGINT",
    };
    let measure = format!(
        "SELECT count(*), CAST(COALESCE(SUM(8 + {} + {} + length(value)), 0) AS {integer})
         FROM extension_entries WHERE ",
        dialect.octets("namespace"),
        dialect.octets("key"),
    );
    let (filter, bounds): (&str, Vec<String>) = match owner {
        None => (
            "namespace >= ?1 AND namespace < ?2",
            vec![HOST_PREFIX.to_owned(), "rift/".to_owned()],
        ),
        Some(Owner::Plugin(name)) => (
            "namespace = ?1 OR (namespace >= ?2 AND namespace < ?3)",
            vec![
                format!("{PLUGIN_PREFIX}{name}"),
                format!("{PLUGIN_PREFIX}{name}."),
                format!("{PLUGIN_PREFIX}{name}/"),
            ],
        ),
        Some(Owner::Script) => (
            "namespace < ?1 OR (namespace >= ?2 AND namespace < ?3) OR namespace >= ?4",
            vec![
                PLUGIN_PREFIX.to_owned(),
                "plugin/".to_owned(),
                HOST_PREFIX.to_owned(),
                "rift/".to_owned(),
            ],
        ),
    };
    let values: Vec<crate::database::Value> = bounds
        .iter()
        .map(crate::database::ToValue::to_value)
        .collect();
    let (entries, bytes): (i64, i64) =
        connection.query_row(&format!("{measure}{filter}"), &values, |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    Ok(Usage {
        entries: entries.max(0) as usize,
        bytes: bytes.max(0) as usize,
    })
}

/// Perform `op` in the caller's transaction.
pub fn apply(connection: &Conn, op: &KvOp) -> KvOutcome {
    let KvOp::Mutate {
        quotas,
        expect,
        changes,
    } = op;
    mutate(connection, quotas, expect, changes).unwrap_or_else(|error| KvOutcome::Failed {
        message: error.to_string(),
    })
}

fn mutate(
    connection: &Conn,
    quotas: &Quotas,
    expect: &[KvEntry],
    changes: &[KvEntry],
) -> database::Result<KvOutcome> {
    for entry in expect {
        if current(connection, &entry.key)? != entry.value {
            return Ok(KvOutcome::Conflict {
                key: entry.key.clone(),
            });
        }
    }
    // The entries that differ from the table, and how each owner's usage
    // changes: (entries, bytes).
    let mut effective = Vec::new();
    let mut host: Option<(i64, i64)> = None;
    let mut owners: BTreeMap<Owner, (i64, i64)> = BTreeMap::new();
    for entry in changes {
        let old = current(connection, &entry.key)?;
        if old == entry.value {
            continue;
        }
        if let Some(value) = &entry.value
            && let Err(error) = validate_entry(&entry.key, value)
        {
            return Ok(KvOutcome::Rejected {
                message: error.to_string(),
            });
        }
        let delta = match owner(&entry.key.0) {
            None => host.get_or_insert((0, 0)),
            Some(owner) => owners.entry(owner).or_insert((0, 0)),
        };
        if let Some(old) = &old {
            delta.0 -= 1;
            delta.1 -= entry_size(&entry.key, old) as i64;
        }
        if let Some(value) = &entry.value {
            delta.0 += 1;
            delta.1 += entry_size(&entry.key, value) as i64;
        }
        effective.push(entry.clone());
    }
    let grown = |old: Usage, (entries, bytes): (i64, i64)| Usage {
        entries: (old.entries as i64 + entries).max(0) as usize,
        bytes: (old.bytes as i64 + bytes).max(0) as usize,
    };
    if let Some(delta) = host {
        let new = grown(usage(connection, None)?, delta);
        if let Err(error) = check(&new, &HOST_QUOTA, "host") {
            return Ok(KvOutcome::Rejected {
                message: error.to_string(),
            });
        }
    }
    for (owner, delta) in &owners {
        // Shrinking always passes; only growth needs the current usage.
        if delta.0 <= 0 && delta.1 <= 0 {
            continue;
        }
        let old = usage(connection, Some(owner))?;
        if let Err(error) = check_growth(&old, &grown(old, *delta), &quotas.get(owner), owner) {
            return Ok(KvOutcome::Rejected {
                message: error.to_string(),
            });
        }
    }
    for entry in &effective {
        let (namespace, key) = &entry.key;
        match &entry.value {
            Some(value) => connection.execute(
                SHARED_UPSERT,
                crate::database::params![namespace, key, value],
            )?,
            None => connection.execute(SHARED_DELETE, crate::database::params![namespace, key])?,
        };
    }
    Ok(KvOutcome::Applied { changes: effective })
}

fn shared_schema(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Sqlite => {
            "CREATE TABLE extension_entries (
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                value BLOB NOT NULL,
                PRIMARY KEY (namespace, key)
            ) STRICT, WITHOUT ROWID;"
        }
        Dialect::Postgres => {
            "CREATE TABLE extension_entries (
                namespace TEXT COLLATE \"C\" NOT NULL,
                key TEXT COLLATE \"C\" NOT NULL,
                value BYTEA NOT NULL,
                PRIMARY KEY (namespace, key)
            );"
        }
        // Namespaces and keys are at most MAX_LABEL bytes.
        Dialect::Mariadb => {
            "CREATE TABLE extension_entries (
                namespace VARCHAR(255) NOT NULL,
                \"key\" VARCHAR(255) NOT NULL,
                value LONGBLOB NOT NULL,
                PRIMARY KEY (namespace, \"key\")
            ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin;"
        }
    }
}

fn shared_sql(dialect: Dialect, _from: u32) -> String {
    shared_schema(dialect).to_owned()
}

/// Create or migrate `extension_entries` in the instance database.
pub(super) fn prepare_shared(database: &Database) -> io::Result<()> {
    database.migrate(SHARED_COMPONENT, SHARED_VERSION, shared_sql)
}

/// The store's schema component in the instance database and the version
/// this Rift writes.
pub fn schema_versions() -> Vec<(&'static str, u32)> {
    vec![(SHARED_COMPONENT, SHARED_VERSION)]
}

/// The tables of `component`, if it is the store's.
pub fn component_tables(component: &str) -> Option<&'static [&'static str]> {
    (component == SHARED_COMPONENT).then_some(TABLES)
}

/// Migrate `extension_entries` to `to_version` in the caller's transaction;
/// `None` for another component.
pub fn migrate(
    connection: &Conn,
    component: &str,
    to_version: u32,
) -> Option<Result<bool, String>> {
    (component == SHARED_COMPONENT).then(|| {
        database::migrate_component(
            connection,
            SHARED_COMPONENT,
            to_version,
            SHARED_VERSION,
            shared_sql,
        )
        .or_message()
    })
}

fn shared_error(database: &Database, error: impl std::fmt::Display) -> io::Error {
    database.error(format!("extension store: {error}"))
}

/// Every entry of `extension_entries`, validated.
fn load_shared(connection: &Conn) -> io::Result<Entries> {
    let rows = connection
        .query_map(
            "SELECT namespace, key, value FROM extension_entries",
            crate::database::params![],
            |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut entries = Entries::new();
    for (key, value) in rows {
        validate_entry(&key, &value)?;
        entries.insert(key, value);
    }
    Ok(entries)
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
    if count > DEFAULT_QUOTA.entries {
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
                target: Target::File(self.0.join("state")),
                quotas: Quotas::default(),
            }
        }
    }
    impl StorageConfig {
        fn path(&self) -> &Path {
            match &self.target {
                Target::File(path) => path,
                Target::Database => panic!("not a file store"),
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
                target: Target::File("state.bin".into()),
                quotas: Quotas::default(),
            })
        );
    }

    #[test]
    fn config_keeps_the_store_in_the_instance_database_with_database_true() {
        let lua = Lua::new();
        for (source, message) in [
            ("{database=false}", "needs path"),
            ("{database=1}", "must be a boolean"),
            ("{database='yes'}", "must be a boolean"),
            ("{path='a', database=true}", "not both"),
            ("{database=true, typo=1}", "unknown"),
        ] {
            let error = parse(lua.load(source).eval().unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{source}: {error}");
        }
        assert_eq!(
            parse(
                lua.load("{database=true, quota={entries=10}}")
                    .eval()
                    .unwrap()
            )
            .unwrap(),
            Some(StorageConfig {
                target: Target::Database,
                quotas: Quotas {
                    default: Quota {
                        entries: 10,
                        bytes: DEFAULT_QUOTA.bytes,
                    },
                    plugins: BTreeMap::new(),
                },
            })
        );
        // `database = false` is the same as leaving it out.
        assert_eq!(
            parse(lua.load("{path='a', database=false}").eval().unwrap())
                .unwrap()
                .unwrap()
                .target,
            Target::File("a".into())
        );
    }

    fn shared(entries: usize) -> StorageConfig {
        let mut quotas = Quotas::default();
        quotas.default.entries = entries;
        StorageConfig {
            target: Target::Database,
            quotas,
        }
    }

    /// The database target on either backend: entries survive reopening
    /// from the same database, and quotas still apply.
    fn exercise_shared(database: &Arc<Database>) {
        let config = shared(4);
        let store = Store::open(Some(&config), Some(database)).unwrap();
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
        // The script now has three of its four entries.
        let fill = |store: &Store, count: usize| {
            store.mutate(deadline(), |entries| {
                for i in 0..count {
                    entries.insert(("fill".into(), i.to_string()), Vec::new());
                }
                Ok(())
            })
        };
        let error = fill(&store, 2).unwrap_err().to_string();
        assert!(error.contains("exceeds 4 entries"), "{error}");
        fill(&store, 1).unwrap();
        store
            .mutate(deadline(), |entries| {
                entries.insert(("plugin.p.data".into(), "k".into()), b"v".to_vec());
                entries.insert((MEMBER_NAMESPACE.into(), "m".into()), b"staff".to_vec());
                Ok(())
            })
            .unwrap();
        drop(store);
        // The store does not hold the connection between writes.
        let rows: i64 = database
            .lock()
            .query_row(
                "SELECT count(*) FROM extension_entries",
                crate::database::params![],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 6);
        let reopened = Store::open(Some(&config), Some(database)).unwrap();
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
        assert_eq!(
            reopened.namespace("plugin.p.data"),
            vec![("k".to_owned(), b"v".to_vec())]
        );
        assert_eq!(reopened.memory().host.entries, 1);
        // Usage is counted again on open.
        assert!(fill(&reopened, 2).is_err());
        assert_eq!(
            reopened.namespace("fill"),
            vec![("0".to_owned(), Vec::new())]
        );
    }

    #[test]
    fn the_store_can_live_in_the_instance_database() {
        let database = Arc::new(Database::memory());
        exercise_shared(&database);
        let rows = |database: &Arc<Database>| -> Vec<(String, String)> {
            database
                .lock()
                .query_map(
                    "SELECT namespace, key FROM extension_entries ORDER BY namespace, key",
                    crate::database::params![],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap()
        };
        let before = rows(&database);
        // A failed commit changes neither memory nor the table.
        let store = Store::open(Some(&shared(16)), Some(&database)).unwrap();
        database
            .lock()
            .execute_batch(
                "CREATE TEMP TRIGGER fail BEFORE INSERT ON main.extension_entries WHEN NEW.key = 'fail'
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let error = store
            .mutate(deadline(), |entries| {
                entries.insert(("one".into(), "key".into()), b"new".to_vec());
                entries.insert(("one".into(), "fail".into()), b"v".to_vec());
                Ok(())
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("injected"), "{error}");
        assert_eq!(
            store.get("one".into(), "key".into(), deadline()).unwrap(),
            Some(b"hello\0\xff".to_vec())
        );
        assert_eq!(rows(&database), before);
        // Writers wait for the shared connection only until their deadline.
        let held = database.lock();
        let error = store
            .mutate(Instant::now() + Duration::from_millis(20), |entries| {
                entries.insert(("one".into(), "late".into()), b"v".to_vec());
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(held);
        assert_eq!(
            store.get("one".into(), "late".into(), deadline()).unwrap(),
            None
        );
        drop(store);
        // Invalid rows fail startup, as in a store file.
        database
            .lock()
            .execute(
                "INSERT INTO extension_entries VALUES ('n', ?1, X'')",
                crate::database::params!["k".repeat(MAX_LABEL + 1)],
            )
            .unwrap();
        assert!(Store::open(Some(&shared(16)), Some(&database)).is_err());
    }

    #[test]
    fn the_store_can_live_on_a_server() {
        let Some(location) = crate::database::test_server_or_skip("the_store_can_live_on_a_server")
        else {
            return;
        };
        let database = Arc::new(Database::connect(&location).unwrap());
        assert_ne!(database.dialect(), Dialect::Sqlite);
        exercise_shared(&database);
    }

    #[test]
    fn the_database_target_needs_the_instance_database() {
        let error = Store::open(Some(&shared(16)), None).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("instance database"), "{error}");
    }

    #[test]
    fn quota_config_sets_a_default_and_per_plugin_overrides() {
        let lua = Lua::new();
        for source in [
            "{path='a', quota=1}",
            "{path='a', quota={typo=1}}",
            "{path='a', quota={entries=0}}",
            "{path='a', quota={entries=1.5}}",
            "{path='a', quota={bytes=2^31}}",
            "{path='a', quota={plugins=1}}",
            "{path='a', quota={plugins={['bad.name']={entries=1}}}}",
            "{path='a', quota={plugins={[1]={entries=1}}}}",
            "{path='a', quota={plugins={p=1}}}",
            "{path='a', quota={plugins={p={plugins={}}}}}",
        ] {
            assert!(parse(lua.load(source).eval().unwrap()).is_err(), "{source}");
        }
        let config = parse(
            lua.load(
                "{path='a', quota={entries=10, plugins={big={bytes=4096}, small={entries=2}}}}",
            )
            .eval()
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        let default = Quota {
            entries: 10,
            bytes: DEFAULT_QUOTA.bytes,
        };
        assert_eq!(config.quotas.default, default);
        assert_eq!(
            config.quotas.get(&Owner::Plugin("big".into())),
            Quota {
                entries: 10,
                bytes: 4096
            }
        );
        assert_eq!(
            config.quotas.get(&Owner::Plugin("small".into())),
            Quota {
                entries: 2,
                bytes: DEFAULT_QUOTA.bytes
            }
        );
        assert_eq!(config.quotas.get(&Owner::Plugin("other".into())), default);
        assert_eq!(config.quotas.get(&Owner::Script), default);
    }

    #[test]
    fn namespaces_binary_values_and_deletes_survive_reopening() {
        let temp = Temporary::new();
        let config = temp.config();
        let store = Store::open(Some(&config), None).unwrap();
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
        let reopened = Store::open(Some(&config), None).unwrap();
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
        let store = Store::open(None, None).unwrap();
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
        let store = Store::open(None, None).unwrap();
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
                    for i in 0..DEFAULT_QUOTA.entries {
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
        let store = Store::open(None, None).unwrap();
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
    fn host_and_persistent_plugin_namespaces_are_out_of_reach_of_scripts() {
        let store = Store::open(None, None).unwrap();
        store
            .mutate(Instant::now() + Duration::from_secs(1), |entries| {
                entries.insert(("plugin.ban.list".into(), "k".into()), b"v".to_vec());
                Ok(())
            })
            .unwrap();
        let lua = Lua::new();
        let api = lua.create_table().unwrap();
        store
            .install(&lua, &api, Instant::now() + Duration::from_secs(5))
            .unwrap();
        lua.globals().set("rift", api).unwrap();
        lua.load(
            r#"
            for _, namespace in ipairs({ 'rift.groups', 'plugin.ban.list', 'plugin.' }) do
                local ok, error = pcall(rift.store.get, namespace, 'k')
                assert(not ok and tostring(error):find('reserved'), namespace)
                assert(not pcall(rift.store.set, namespace, 'k', 'forged'))
                assert(not pcall(rift.store.delete, namespace, 'k'))
                assert(not pcall(rift.store.increment, namespace, 'k', 1))
            end
            rift.store.set('plugins', 'k', 'ok')
        "#,
        )
        .exec()
        .unwrap();
        let entries = &store.memory().entries;
        assert_eq!(entries[&("plugin.ban.list".into(), "k".into())], b"v");
    }

    #[test]
    fn expired_operations_cannot_mutate_state() {
        let store = Store::open(None, None).unwrap();
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
        let store = Store::open(None, None).unwrap();
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
        let store = Store::open(Some(&config), None).unwrap();
        lua(&store)
            .load("rift.store.set('n', 'k', 'old')")
            .exec()
            .unwrap();
        {
            let writer = store.0.writer.lock().unwrap();
            let Writer::File(connection) = &*writer else {
                panic!("a file store");
            };
            connection
                .execute_batch(
                    "CREATE TEMP TRIGGER fail BEFORE INSERT ON main.entries WHEN NEW.key = 'fail'
                     BEGIN SELECT RAISE(ABORT, 'injected'); END;",
                )
                .unwrap();
        }
        let result = store.mutate(deadline(), |entries| {
            entries.insert(("n".into(), "k".into()), b"new".to_vec());
            entries.insert(("n".into(), "fail".into()), b"v".to_vec());
            Ok(())
        });
        assert!(result.unwrap_err().to_string().contains("injected"));
        let usage = store.memory().owners[&Owner::Script];
        assert_eq!(
            store.get("n".into(), "k".into(), deadline()).unwrap(),
            Some(b"old".to_vec())
        );
        lua(&store)
            .load("rift.store.set('n', 'other', 'v')")
            .exec()
            .unwrap();
        assert_eq!(usage.entries, 1);
        drop(store);
        let reopened = Store::open(Some(&config), None).unwrap();
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
        let store = Store::open(Some(&config), None).unwrap();
        assert_eq!(
            Store::open(Some(&config), None).err().unwrap().kind(),
            io::ErrorKind::ResourceBusy
        );
        drop(store);
        Store::open(Some(&config), None).unwrap();
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
        fs::write(config.path(), &snapshot).unwrap();
        let store = Store::open(Some(&config), None).unwrap();
        assert_eq!(store.memory().entries, entries);
        drop(store);
        assert!(fs::read(config.path()).unwrap().starts_with(SQLITE_HEADER));
        assert_eq!(fs::read(backup_path(config.path())).unwrap(), snapshot);
        let reopened = Store::open(Some(&config), None).unwrap();
        assert_eq!(reopened.memory().entries, entries);
        assert_eq!(
            reopened
                .memory()
                .owners
                .values()
                .map(|usage| usage.entries)
                .sum::<usize>(),
            2
        );
        assert_eq!(reopened.memory().host.entries, 1);
    }

    #[test]
    fn checking_a_store_changes_nothing() {
        let temp = Temporary::new();
        let config = temp.config();
        let wal = PathBuf::from(format!("{}-wal", config.path().display()));
        let listing = || {
            let mut names: Vec<_> = fs::read_dir(&temp.0)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            names
        };

        assert_eq!(check_file(config.path()).unwrap(), Check::Missing);
        assert!(listing().is_empty());

        let entries = BTreeMap::from([
            (("n".into(), "k".into()), b"v".to_vec()),
            ((MEMBER_NAMESPACE.into(), "m".into()), b"staff".to_vec()),
        ]);
        let snapshot = encode(&entries).unwrap();
        fs::write(config.path(), &snapshot).unwrap();
        assert_eq!(
            check_file(config.path()).unwrap(),
            Check::Snapshot { entries: 2 }
        );
        assert_eq!(fs::read(config.path()).unwrap(), snapshot);
        assert_eq!(listing().len(), 1, "no backup or database is written");

        // Leave committed writes in the WAL, as a crash would.
        fs::remove_file(config.path()).unwrap();
        let store = Store::open(Some(&config), None).unwrap();
        lua(&store)
            .load("rift.store.set('a', 'b', 'c') rift.store.set('a', 'd', 'e')")
            .exec()
            .unwrap();
        let before = (fs::read(config.path()).unwrap(), fs::read(&wal).unwrap());
        let names = listing();
        assert_eq!(
            check_file(config.path()).unwrap(),
            Check::Database { entries: 2 }
        );
        assert_eq!(
            (fs::read(config.path()).unwrap(), fs::read(&wal).unwrap()),
            before
        );
        assert_eq!(listing(), names);
        drop(store);

        fs::remove_file(config.path()).unwrap();
        Connection::open(config.path())
            .unwrap()
            .execute_batch("CREATE TABLE entries (namespace TEXT, key TEXT, value BLOB);")
            .unwrap();
        let error = check_file(config.path()).unwrap_err().to_string();
        assert!(error.contains("not a Rift extension store"), "{error}");
        assert!(
            error.contains(&config.path().display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn other_sqlite_databases_are_rejected() {
        let temp = Temporary::new();
        let config = temp.config();
        Connection::open(config.path())
            .unwrap()
            .execute_batch("CREATE TABLE entries (namespace TEXT, key TEXT, value BLOB);")
            .unwrap();
        let error = Store::open(Some(&config), None).err().unwrap();
        assert!(
            error.to_string().contains("not a Rift extension store"),
            "{error}"
        );
    }

    #[test]
    fn invalid_rows_fail_startup() {
        let temp = Temporary::new();
        let config = temp.config();
        drop(Store::open(Some(&config), None).unwrap());
        Connection::open(config.path())
            .unwrap()
            .execute(
                "INSERT INTO entries VALUES ('n', ?1, X'')",
                [&"k".repeat(MAX_LABEL + 1)],
            )
            .unwrap();
        assert!(Store::open(Some(&config), None).is_err());
    }

    #[test]
    fn each_plugin_and_the_script_have_their_own_quota() {
        let temp = Temporary::new();
        let mut config = temp.config();
        config.quotas.default.entries = 2;
        config.quotas.plugins.insert(
            "big".into(),
            Quota {
                entries: 3,
                bytes: DEFAULT_QUOTA.bytes,
            },
        );
        let store = Store::open(Some(&config), None).unwrap();
        let fill = |store: &Store, namespace: &str, count: usize| {
            store.mutate(deadline(), |entries| {
                for i in 0..count {
                    entries.insert((namespace.into(), i.to_string()), vec![]);
                }
                Ok(())
            })
        };
        fill(&store, "plugin.a.one", 1).unwrap();
        fill(&store, "plugin.a.two", 1).unwrap();
        // Namespaces of one plugin share its quota.
        let error = fill(&store, "plugin.a.three", 1).unwrap_err().to_string();
        assert!(
            error.contains("plugin \"a\" data exceeds 2 entries"),
            "{error}"
        );
        // Another plugin, the script and an overridden plugin are unaffected.
        fill(&store, "plugin.b.one", 2).unwrap();
        fill(&store, "script", 2).unwrap();
        assert!(
            fill(&store, "other", 1)
                .unwrap_err()
                .to_string()
                .contains("configuration script")
        );
        fill(&store, "plugin.big.one", 3).unwrap();
        assert!(fill(&store, "plugin.big.two", 1).is_err());
        assert_eq!(store.memory().owners.len(), 4);
        // Removing an owner's last entry forgets it.
        store
            .mutate(deadline(), |entries| {
                entries.remove(&("plugin.b.one".into(), "0".into()));
                entries.remove(&("plugin.b.one".into(), "1".into()));
                Ok(())
            })
            .unwrap();
        assert_eq!(store.memory().owners.len(), 3);
        drop(store);

        // A lowered quota does not fail startup; the owner can only shrink.
        config.quotas.default.entries = 1;
        let store = Store::open(Some(&config), None).unwrap();
        // Replacing a value keeps the entry count, so it is still allowed.
        store
            .mutate(deadline(), |entries| {
                entries.insert(("plugin.a.one".into(), "0".into()), b"larger".to_vec());
                Ok(())
            })
            .unwrap();
        assert!(fill(&store, "plugin.a.three", 1).is_err());
        store
            .mutate(deadline(), |entries| {
                Ok(entries.remove(&("plugin.a.one".into(), "0".into())))
            })
            .unwrap();
        assert!(
            fill(&store, "plugin.a.three", 1).is_err(),
            "still at the lowered limit"
        );
    }

    #[test]
    fn host_data_has_its_own_quota() {
        let store = Store::open(None, None).unwrap();
        store
            .mutate(deadline(), |entries| {
                for i in 0..DEFAULT_QUOTA.entries {
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
                for i in 0..DEFAULT_QUOTA.entries {
                    entries.insert((MEMBER_NAMESPACE.into(), i.to_string()), vec![]);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(store.memory().host.entries, DEFAULT_QUOTA.entries);
        let host = store.mutate(deadline(), |entries| {
            for i in 0..=HOST_QUOTA.entries - DEFAULT_QUOTA.entries {
                entries.insert(("rift.other".into(), i.to_string()), vec![]);
            }
            Ok(())
        });
        assert!(host.unwrap_err().to_string().contains("host data"));
        assert_eq!(store.memory().host.entries, DEFAULT_QUOTA.entries);
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
            fs::write(config.path(), bytes).unwrap();
            assert!(Store::open(Some(&config), None).is_err());
        }
    }

    #[test]
    #[cfg(unix)]
    fn opening_a_fifo_fails_instead_of_blocking_startup() {
        let temp = Temporary::new();
        let config = temp.config();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(config.path())
                .status()
                .unwrap()
                .success()
        );
        let (send, receive) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let error = Store::open(Some(&config), None).err().unwrap();
            send.send(error.kind()).unwrap();
        });
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            io::ErrorKind::InvalidData
        );
        thread.join().unwrap();
    }
}
