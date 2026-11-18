//! The instance database, shared by the instance registry, the permission
//! store, the player directory and (with `extensions.storage.database`) the
//! extension store.
//!
//! It is either a SQLite file beside the configuration (`instance_database`,
//! by default `rift.sqlite3`) or a PostgreSQL database
//! (`database.backend = "postgres"`). Components use one connection API,
//! [`Conn`], and write their SQL once with `?N` placeholders; the few
//! constructs the two dialects spell differently go through [`Dialect`].
//!
//! One process owns the database. For a file, an OS lock on `<file>.lock` is
//! taken at open and released at exit, even after a crash; opening runs
//! `PRAGMA integrity_check` and refuses a damaged file rather than serving
//! partial state. For PostgreSQL, the owner holds a session advisory lock
//! (see [`postgres`]); a standby Rift (`database.standby`) waits for it, so a
//! second controller takes over when the first stops.
//!
//! Each component creates and migrates its own tables and records its schema
//! version in `rift_schema`. SQLite files written before that table existed
//! kept the instance schema version in `PRAGMA user_version`; it seeds the
//! `instances` row once.
pub mod copy;
pub(crate) mod postgres;
mod value;

pub(crate) use value::params;
pub use value::{Error, FromValue, Kind, OptionalExtension, Result, Row, RowIndex, ToValue, Value};

use crate::provisioning;
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, TryLockError},
    time::{Duration, Instant},
};

const SQLITE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS rift_schema (
    component TEXT PRIMARY KEY NOT NULL,
    version INTEGER NOT NULL
) STRICT;";

const POSTGRES_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS rift_schema (
    component TEXT COLLATE \"C\" PRIMARY KEY NOT NULL,
    version BIGINT NOT NULL
);";

/// The component whose version older files kept in `PRAGMA user_version`.
const LEGACY_COMPONENT: &str = "instances";

/// The environment variable `database.url_env` names by default.
pub const DEFAULT_URL_ENV: &str = "RIFT_DATABASE_URL";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// The SQL dialect of a connection, for the few constructs SQLite and
/// PostgreSQL spell differently. Everything else is written once.
///
/// PostgreSQL tables use `TEXT COLLATE "C"` (bytewise, like SQLite's BINARY)
/// and `BIGINT` for every integer and boolean, so ordering, uniqueness and
/// integer reads match SQLite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
}

impl Dialect {
    /// ASCII case-insensitive `LIKE` (SQLite's `LIKE` is).
    pub fn like(self) -> &'static str {
        match self {
            Self::Sqlite => "LIKE",
            Self::Postgres => "ILIKE",
        }
    }

    /// The larger of two values: SQLite's two-argument `MAX`.
    pub fn greatest(self) -> &'static str {
        match self {
            Self::Sqlite => "MAX",
            Self::Postgres => "GREATEST",
        }
    }

    /// `expr` folded for an ASCII case-insensitive comparison or ordering
    /// (SQLite's `COLLATE NOCASE`).
    pub fn nocase(self, expr: &str) -> String {
        match self {
            Self::Sqlite => format!("{expr} COLLATE NOCASE"),
            Self::Postgres => format!("lower({expr} COLLATE \"C\")"),
        }
    }

    /// The 1-based position of `needle` in `haystack`, 0 when absent.
    pub fn position(self, haystack: &str, needle: &str) -> String {
        match self {
            Self::Sqlite => format!("instr({haystack}, {needle})"),
            Self::Postgres => format!("strpos({haystack}, {needle})"),
        }
    }
}

/// Where `rift.config.database.url` comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Url {
    /// `database.url`, written in the configuration.
    Literal(String),
    /// `database.url_env`: read from this environment variable at startup.
    Env(String),
}

/// `rift.config.database` with `backend = "postgres"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostgresConfig {
    pub url: Url,
    /// Certificates trusted for TLS in addition to the public CAs.
    pub ca: Option<PathBuf>,
    /// Wait for the Rift that owns the database to stop instead of failing.
    pub standby: bool,
    /// Keep Rift's tables in this schema, created when missing.
    pub schema: Option<String>,
}

impl PostgresConfig {
    /// The environment variable holding the URL, which managed servers must
    /// never inherit.
    pub fn url_variable(&self) -> Option<&str> {
        match &self.url {
            Url::Env(name) => Some(name),
            Url::Literal(_) => None,
        }
    }

    fn url(&self) -> io::Result<String> {
        match &self.url {
            Url::Literal(url) => Ok(url.clone()),
            Url::Env(name) => match std::env::var(name) {
                Ok(url) if !url.trim().is_empty() => Ok(url),
                _ => Err(invalid(format!(
                    "database.url_env: {name} is not set; put the PostgreSQL connection URL in it"
                ))),
            },
        }
    }

    fn settings(&self) -> io::Result<postgres::Settings> {
        let config: tokio_postgres::Config = self
            .url()?
            .parse()
            .map_err(|error| invalid(format!("database URL: {error}")))?;
        Ok(postgres::Settings {
            label: self.label_of(&config),
            config,
            tls: postgres::MakeRustls::new(self.ca.as_deref())?,
            schema: self.schema.clone(),
        })
    }

    fn label_of(&self, config: &tokio_postgres::Config) -> String {
        let mut label = postgres::label(config);
        if let Some(schema) = &self.schema {
            label.push_str(&format!(" (schema {schema})"));
        }
        label
    }

    /// The database for messages, without its password.
    pub fn describe(&self) -> String {
        match self.url().ok().and_then(|url| url.parse().ok()) {
            Some(config) => self.label_of(&config),
            None => match &self.url {
                Url::Env(name) => format!("PostgreSQL database from {name}"),
                Url::Literal(_) => "PostgreSQL database".to_owned(),
            },
        }
    }
}

/// Where the instance database lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Sqlite(PathBuf),
    Postgres(PostgresConfig),
}

impl Location {
    /// The database for messages, without any password.
    pub fn describe(&self) -> String {
        match self {
            Self::Sqlite(path) => path.display().to_string(),
            Self::Postgres(config) => config.describe(),
        }
    }
}

fn private_file(path: &Path) -> io::Result<File> {
    provisioning::checked_path(path)?;
    if fs::symlink_metadata(path).is_ok_and(|m| !m.is_file()) {
        return Err(invalid("instance database and lock must be regular files"));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// How long [`lock_exclusive`] waits for a lock held elsewhere.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Take an exclusive lock on `file`, waiting up to two seconds if it is held.
///
/// A lock released in this process can stay held for a moment: a child forked
/// by another thread keeps a copy of the descriptor until it calls `exec`,
/// which then closes it. A lock still held after the wait belongs to another
/// process, normally a running Rift.
pub(crate) fn lock_exclusive(file: &File) -> std::result::Result<(), fs::TryLockError> {
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result,
        }
    }
}

/// The lock file guarding `path`.
pub(crate) fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

enum Backend {
    Sqlite(Connection),
    Postgres(postgres::Session),
}

/// One connection to the instance database, of either backend. Statements
/// use `?N` placeholders and [`params!`] values.
pub struct Conn(Backend);

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Backend::Sqlite(_) => f.write_str("Conn(SQLite)"),
            Backend::Postgres(session) => write!(f, "Conn({session:?})"),
        }
    }
}

impl Conn {
    pub fn dialect(&self) -> Dialect {
        match self.0 {
            Backend::Sqlite(_) => Dialect::Sqlite,
            Backend::Postgres(_) => Dialect::Postgres,
        }
    }

    /// Run one statement; the number of rows it changed.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        match &self.0 {
            Backend::Sqlite(connection) => Ok(connection
                .prepare_cached(sql)?
                .execute(rusqlite::params_from_iter(params.iter()))?),
            Backend::Postgres(session) => session.execute(sql, params),
        }
    }

    /// Run statements without parameters, separated by semicolons.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        match &self.0 {
            Backend::Sqlite(connection) => Ok(connection.execute_batch(sql)?),
            Backend::Postgres(session) => session.execute_batch(sql),
        }
    }

    fn rows<E: From<Error>>(
        &self,
        sql: &str,
        params: &[Value],
        mut each: impl FnMut(Row) -> std::result::Result<bool, E>,
    ) -> std::result::Result<(), E> {
        match &self.0 {
            Backend::Sqlite(connection) => {
                let mut statement = connection.prepare_cached(sql).map_err(Error::from)?;
                let columns = statement.column_count();
                let mut rows = statement
                    .query(rusqlite::params_from_iter(params.iter()))
                    .map_err(Error::from)?;
                while let Some(row) = rows.next().map_err(Error::from)? {
                    let mut values = Vec::with_capacity(columns);
                    for index in 0..columns {
                        values.push(Value::from_sqlite(
                            row.get_ref(index).map_err(Error::from)?,
                        )?);
                    }
                    if !each(Row(values))? {
                        break;
                    }
                }
                Ok(())
            }
            Backend::Postgres(session) => session.each(sql, params, each),
        }
    }

    /// Stream the rows of a query into `each` until it returns `false`.
    /// `each` must not use the connection itself.
    pub fn each<E: From<Error>>(
        &self,
        sql: &str,
        params: &[Value],
        mut each: impl FnMut(&Row) -> std::result::Result<bool, E>,
    ) -> std::result::Result<(), E> {
        self.rows(sql, params, |row| each(&row))
    }

    /// Every row of a query.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        self.rows(sql, params, |row| -> Result<bool> {
            rows.push(row);
            Ok(true)
        })?;
        Ok(rows)
    }

    /// The first row of a query through `map`, or [`Error::NoRows`].
    pub fn query_row<T>(
        &self,
        sql: &str,
        params: &[Value],
        map: impl FnOnce(&Row) -> Result<T>,
    ) -> Result<T> {
        let mut first = None;
        self.rows(sql, params, |row| -> Result<bool> {
            first = Some(row);
            Ok(false)
        })?;
        map(&first.ok_or(Error::NoRows)?)
    }

    /// Every row of a query through `map`.
    pub fn query_map<T>(
        &self,
        sql: &str,
        params: &[Value],
        mut map: impl FnMut(&Row) -> Result<T>,
    ) -> Result<Vec<T>> {
        let mut mapped = Vec::new();
        self.rows(sql, params, |row| -> Result<bool> {
            mapped.push(map(&row)?);
            Ok(true)
        })?;
        Ok(mapped)
    }

    fn begin(&self, immediate: bool) -> Result<()> {
        match &self.0 {
            Backend::Sqlite(connection) => Ok(connection.execute_batch(if immediate {
                "BEGIN IMMEDIATE"
            } else {
                "BEGIN DEFERRED"
            })?),
            // One process owns the database, so there is no write lock to
            // take early.
            Backend::Postgres(session) => session.begin(),
        }
    }

    fn finish(&self, commit: bool) -> Result<()> {
        match &self.0 {
            Backend::Sqlite(connection) => {
                Ok(connection.execute_batch(if commit { "COMMIT" } else { "ROLLBACK" })?)
            }
            Backend::Postgres(session) => session.finish(commit),
        }
    }

    /// Start a transaction; dropping it uncommitted rolls it back.
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        self.begin(false)?;
        Ok(Transaction {
            connection: self,
            done: false,
        })
    }

    /// A transaction that takes SQLite's write lock at once.
    pub fn transaction_immediate(&mut self) -> Result<Transaction<'_>> {
        self.begin(true)?;
        Ok(Transaction {
            connection: self,
            done: false,
        })
    }

    /// The SQLite connection, for file-only pragmas.
    pub(crate) fn sqlite(&self) -> Option<&Connection> {
        match &self.0 {
            Backend::Sqlite(connection) => Some(connection),
            Backend::Postgres(_) => None,
        }
    }
}

/// An open transaction; it derefs to its [`Conn`].
pub struct Transaction<'a> {
    connection: &'a Conn,
    done: bool,
}

impl Transaction<'_> {
    pub fn commit(mut self) -> Result<()> {
        let result = self.connection.finish(true);
        // A failed COMMIT leaves the transaction open; Drop rolls it back.
        self.done = result.is_ok();
        result
    }
}

impl std::ops::Deref for Transaction<'_> {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        self.connection
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.connection.finish(false);
        }
    }
}

#[derive(Debug)]
pub struct Database {
    /// The file path or the URL without its password, for messages.
    label: String,
    /// The SQLite file, if this is one.
    path: Option<PathBuf>,
    connection: Mutex<Conn>,
    // The OS releases this lock even after a crash. Keep the lock file in place.
    _lock: Option<File>,
}

impl Database {
    /// Open the database at `location` for this process: lock it, create
    /// what is missing and check it.
    pub fn connect(location: &Location) -> io::Result<Self> {
        match location {
            Location::Sqlite(path) => Self::open(path),
            Location::Postgres(config) => {
                let ownership = if config.standby {
                    postgres::Ownership::Standby
                } else {
                    postgres::Ownership::Own { wait: LOCK_WAIT }
                };
                let database = Self::postgres(config, ownership)?;
                database.prepare(true)?;
                Ok(database)
            }
        }
    }

    /// Open an existing database read-only, without locking, checking or
    /// migrating it (for `rift check --strict`).
    pub fn connect_read_only(location: &Location) -> io::Result<Self> {
        match location {
            Location::Sqlite(path) => Self::read_only(path),
            Location::Postgres(config) => Self::postgres(config, postgres::Ownership::None),
        }
    }

    fn postgres(config: &PostgresConfig, ownership: postgres::Ownership) -> io::Result<Self> {
        let settings = config.settings()?;
        let label = settings.label.clone();
        let session = postgres::Session::open(Arc::new(settings), ownership)
            .map_err(|error| io::Error::other(format!("instance database {label}: {error}")))?;
        Ok(Self {
            label,
            path: None,
            connection: Mutex::new(Conn(Backend::Postgres(session))),
            _lock: None,
        })
    }

    /// Lock, open (creating it if needed) and check a SQLite file.
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = provisioning::checked_path(path)?;
        fs::create_dir_all(path.parent().unwrap())?;
        let lock = private_file(&lock_path(&path))?;
        lock_exclusive(&lock).map_err(|error| {
            io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "instance database {} is already in use or cannot be locked: {error}",
                    path.display()
                ),
            )
        })?;
        private_file(&path)?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| invalid(format!("instance database {}: {e}", path.display())))?;
        let database = Self {
            label: path.display().to_string(),
            path: Some(path),
            connection: Mutex::new(Conn(Backend::Sqlite(connection))),
            _lock: Some(lock),
        };
        database.prepare(true)?;
        Ok(database)
    }

    /// Open an existing SQLite file read-only, without locking, checking or
    /// migrating it (for `rift check --strict`).
    pub fn read_only(path: &Path) -> io::Result<Self> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| invalid(format!("instance database {}: {e}", path.display())))?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|e| invalid(format!("instance database {}: {e}", path.display())))?;
        Ok(Self {
            label: path.display().to_string(),
            path: Some(path.to_owned()),
            connection: Mutex::new(Conn(Backend::Sqlite(connection))),
            _lock: None,
        })
    }

    /// A private in-memory SQLite database, for configurations without a
    /// file and for tests.
    pub fn memory() -> Self {
        let database = Self {
            label: ":memory:".into(),
            path: None,
            connection: Mutex::new(Conn(Backend::Sqlite(
                Connection::open_in_memory().expect("in-memory SQLite"),
            ))),
            _lock: None,
        };
        database
            .prepare(false)
            .expect("in-memory SQLite accepts the schema table");
        database
    }

    fn prepare(&self, durable: bool) -> io::Result<()> {
        let connection = self.lock();
        let Some(sqlite) = connection.sqlite() else {
            return connection
                .execute_batch(POSTGRES_SCHEMA)
                .map_err(|e| self.error(e));
        };
        sqlite
            .busy_timeout(Duration::from_secs(2))
            .map_err(|e| self.error(e))?;
        if durable {
            sqlite
                .execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
                .map_err(|e| self.error(e))?;
            let check: String = sqlite
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .map_err(|e| self.error(e))?;
            if check != "ok" {
                return Err(self.error(format!("integrity check failed: {check}")));
            }
        }
        sqlite
            .execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| self.error(e))?;
        let legacy: u32 = sqlite
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| self.error(e))?;
        sqlite
            .execute_batch(SQLITE_SCHEMA)
            .map_err(|e| self.error(e))?;
        if legacy > 0 {
            sqlite
                .execute(
                    "INSERT OR IGNORE INTO rift_schema (component, version) VALUES (?1, ?2)",
                    rusqlite::params![LEGACY_COMPONENT, legacy],
                )
                .map_err(|e| self.error(e))?;
        }
        Ok(())
    }

    /// The SQLite file, or `None` for PostgreSQL and in-memory databases.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The file path or the URL without its password.
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn dialect(&self) -> Dialect {
        self.lock().dialect()
    }

    /// The connection, for one statement or transaction at a time.
    pub fn lock(&self) -> MutexGuard<'_, Conn> {
        self.connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// [`Database::lock`], giving up at `deadline` (for Lua callbacks).
    pub fn lock_until(&self, deadline: Instant) -> io::Result<MutexGuard<'_, Conn>> {
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "script deadline exceeded",
                ));
            }
            match self.connection.try_lock() {
                Ok(connection) => return Ok(connection),
                Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    /// For PostgreSQL: the Rift holding the database's lock, other than this
    /// connection. Always `None` for SQLite.
    pub fn owner(&self) -> io::Result<Option<String>> {
        let connection = self.lock();
        match &connection.0 {
            Backend::Sqlite(_) => Ok(None),
            Backend::Postgres(session) => session
                .holder()
                .map(|holder| holder.map(|holder| holder.to_string()))
                .map_err(|e| self.error(e)),
        }
    }

    pub fn error(&self, error: impl std::fmt::Display) -> io::Error {
        io::Error::other(format!("instance database {}: {error}", self.label))
    }

    /// Bring `component` to version `latest`. `upgrade(dialect, from)`
    /// returns the SQL that turns version `from` (0 when the component is
    /// new) into `latest`; it runs in one transaction with the version
    /// update. A database written by a newer Rift is refused.
    pub fn migrate(
        &self,
        component: &str,
        latest: u32,
        upgrade: impl FnOnce(Dialect, u32) -> String,
    ) -> io::Result<()> {
        let mut connection = self.lock();
        let version = version(&connection, component).map_err(|e| self.error(e))?;
        if version == latest {
            return Ok(());
        }
        if version > latest {
            return Err(self.error(format!(
                "unsupported schema version {version} of {component}"
            )));
        }
        let dialect = connection.dialect();
        let transaction = connection
            .transaction_immediate()
            .map_err(|e| self.error(e))?;
        transaction
            .execute_batch(&upgrade(dialect, version))
            .map_err(|e| self.error(e))?;
        transaction
            .execute(
                "INSERT INTO rift_schema (component, version) VALUES (?1, ?2)
                 ON CONFLICT(component) DO UPDATE SET version = excluded.version",
                params![component, latest],
            )
            .map_err(|e| self.error(e))?;
        transaction.commit().map_err(|e| self.error(e))
    }
}

/// The schema version of `component`, 0 when it has no tables yet. Works on
/// read-only connections and on files that predate `rift_schema`.
pub fn version(connection: &Conn, component: &str) -> Result<u32> {
    let tables: i64 = connection.query_row(
        match connection.dialect() {
            Dialect::Sqlite => {
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'rift_schema'"
            }
            Dialect::Postgres => {
                "SELECT count(*) FROM information_schema.tables
                 WHERE table_schema = current_schema() AND table_name = 'rift_schema'"
            }
        },
        params![],
        |r| r.get(0),
    )?;
    if tables > 0
        && let Some(version) = connection
            .query_row(
                "SELECT version FROM rift_schema WHERE component = ?1",
                params![component],
                |r| r.get(0),
            )
            .optional()?
    {
        return Ok(version);
    }
    if component == LEGACY_COMPONENT && connection.dialect() == Dialect::Sqlite {
        return connection.query_row("PRAGMA user_version", params![], |r| r.get(0));
    }
    Ok(0)
}

/// Whether `component` has tables in this database (version above 0).
pub fn has_component(connection: &Conn, component: &str) -> Result<bool> {
    version(connection, component).map(|version| version > 0)
}

/// Tests against a real PostgreSQL server, when `RIFT_TEST_POSTGRES_URL`
/// names one: each call gets a fresh schema of its own.
#[cfg(test)]
pub(crate) fn test_postgres() -> Option<Location> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let url = std::env::var("RIFT_TEST_POSTGRES_URL")
        .ok()
        .filter(|url| !url.is_empty())?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    Some(Location::Postgres(PostgresConfig {
        url: Url::Literal(url),
        ca: None,
        standby: false,
        schema: Some(format!(
            "rift_test_{}_{nanos:x}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )),
    }))
}

/// [`test_postgres`], or a note that the test was skipped.
#[cfg(test)]
pub(crate) fn test_postgres_or_skip(test: &str) -> Option<Location> {
    let location = test_postgres();
    if location.is_none() {
        println!("{test}: skipped, RIFT_TEST_POSTGRES_URL is not set");
    }
    location
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_released_shortly_after_open_starts_is_taken() {
        let directory = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("rift-database-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("rift.sqlite3");
        // Another descriptor holds the lock, as a forked child briefly does.
        let held = private_file(&lock_path(&path)).unwrap();
        held.try_lock().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(held);
        });
        let database = Database::open(&path).unwrap();
        release.join().unwrap();
        drop(database);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn components_migrate_independently_and_legacy_versions_seed_once() {
        let directory = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("rift-database-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let path = directory.join("rift.sqlite3");
        {
            fs::create_dir_all(&directory).unwrap();
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch("CREATE TABLE old (x); PRAGMA user_version=2;")
                .unwrap();
        }
        let database = Database::open(&path).unwrap();
        assert_eq!(version(&database.lock(), "instances").unwrap(), 2);
        assert_eq!(version(&database.lock(), "permissions").unwrap(), 0);
        database
            .migrate("permissions", 1, |dialect, from| {
                assert_eq!((dialect, from), (Dialect::Sqlite, 0));
                "CREATE TABLE p (x);".into()
            })
            .unwrap();
        // Already current: the upgrade is not asked for.
        database
            .migrate("permissions", 1, |_, _| unreachable!())
            .unwrap();
        assert!(
            database
                .migrate("permissions", 0, |_, _| String::new())
                .is_err()
        );
        assert!(Database::open(&path).is_err(), "the lock is exclusive");
        drop(database);
        let reopened = Database::open(&path).unwrap();
        assert_eq!(version(&reopened.lock(), "permissions").unwrap(), 1);
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }

    /// The connection API both backends share.
    fn exercise(database: &Database) {
        let blob = match database.dialect() {
            Dialect::Sqlite => "BLOB",
            Dialect::Postgres => "BYTEA",
        };
        database
            .migrate("conn_test", 1, |_, _| {
                format!(
                    "CREATE TABLE conn_test (id BIGINT PRIMARY KEY, name TEXT, data {blob}, flag BIGINT NOT NULL DEFAULT 0)"
                )
            })
            .unwrap();
        let mut connection = database.lock();
        for id in 1..=3i64 {
            let name = (id != 2).then(|| format!("n{id}"));
            connection
                .execute(
                    "INSERT INTO conn_test (id, name, data, flag) VALUES (?1, ?2, ?3, ?4)",
                    params![id, name, vec![id as u8], id == 3],
                )
                .unwrap();
        }
        // Typed NULL parameters work in `?N IS NULL OR` filters.
        let filter = "SELECT id FROM conn_test WHERE (?1 IS NULL OR name = ?1) ORDER BY id";
        let all: Vec<i64> = connection
            .query_map(filter, params![None::<String>], |r| r.get(0))
            .unwrap();
        assert_eq!(all, [1, 2, 3]);
        let one: Vec<i64> = connection
            .query_map(filter, params![Some("n3")], |r| r.get(0))
            .unwrap();
        assert_eq!(one, [3]);
        let row = connection
            .query_row(
                "SELECT name, data, flag FROM conn_test WHERE id = ?1",
                params![3i64],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, bool>(2)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row, (Some("n3".to_owned()), vec![3], true));
        assert!(matches!(
            connection.query_row("SELECT id FROM conn_test WHERE id = 9", params![], |r| r
                .get::<_, i64>(0)),
            Err(Error::NoRows)
        ));
        let mut seen = Vec::new();
        connection
            .each(
                "SELECT id FROM conn_test ORDER BY id",
                params![],
                |row| -> Result<bool> {
                    seen.push(row.get::<_, i64>(0)?);
                    Ok(seen.len() < 2)
                },
            )
            .unwrap();
        assert_eq!(seen, [1, 2], "each stops when asked");
        {
            let transaction = connection.transaction().unwrap();
            transaction
                .execute("DELETE FROM conn_test", params![])
                .unwrap();
            // Dropped: rolled back.
        }
        let count = |connection: &Conn| -> i64 {
            connection
                .query_row("SELECT count(*) FROM conn_test", params![], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count(&connection), 3);
        {
            let transaction = connection.transaction_immediate().unwrap();
            let changed = transaction
                .execute("DELETE FROM conn_test WHERE id = ?1", params![1i64])
                .unwrap();
            assert_eq!(changed, 1);
            transaction.commit().unwrap();
        }
        assert_eq!(count(&connection), 2);
        let returned: i64 = connection
            .query_row(
                "INSERT INTO conn_test (id) VALUES ((SELECT COALESCE(MAX(id), 0) + 1 FROM conn_test)) RETURNING id",
                params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(returned, 4);
        let like = format!(
            "SELECT count(*) FROM conn_test WHERE name {} ?1 ESCAPE '\\'",
            connection.dialect().like()
        );
        let matched: i64 = connection
            .query_row(&like, params!["N%"], |r| r.get(0))
            .unwrap();
        assert_eq!(matched, 1, "LIKE ignores ASCII case on both backends");
    }

    #[test]
    fn the_connection_api_works_on_sqlite() {
        exercise(&Database::memory());
    }

    #[test]
    fn the_connection_api_works_on_postgres() {
        let Some(location) = test_postgres_or_skip("the_connection_api_works_on_postgres") else {
            return;
        };
        let database = Database::connect(&location).unwrap();
        assert_eq!(database.dialect(), Dialect::Postgres);
        assert!(database.path().is_none());
        exercise(&database);
    }

    #[test]
    fn one_rift_owns_a_postgres_schema() {
        let Some(location) = test_postgres_or_skip("one_rift_owns_a_postgres_schema") else {
            return;
        };
        let database = Database::connect(&location).unwrap();
        let error = Database::connect(&location).unwrap_err().to_string();
        assert!(error.contains("already in use"), "{error}");
        // A read-only check sees the owner without taking anything.
        let check = Database::connect_read_only(&location).unwrap();
        assert!(check.owner().unwrap().is_some());
        drop(database);
        // The lock goes with the session.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match Database::connect(&location) {
                Ok(_) => break,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => panic!("{error}"),
            }
        }
    }

    #[test]
    fn a_standby_takes_over_when_the_owner_stops() {
        let Some(Location::Postgres(mut config)) =
            test_postgres_or_skip("a_standby_takes_over_when_the_owner_stops")
        else {
            return;
        };
        let owner = Database::connect(&Location::Postgres(config.clone())).unwrap();
        config.standby = true;
        let standby = std::thread::spawn(move || Database::connect(&Location::Postgres(config)));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!standby.is_finished(), "the standby waits");
        drop(owner);
        let database = standby.join().unwrap().unwrap();
        assert_eq!(database.dialect(), Dialect::Postgres);
    }

    #[test]
    fn dialect_spellings() {
        assert_eq!(Dialect::Postgres.like(), "ILIKE");
        assert_eq!(Dialect::Sqlite.nocase("x"), "x COLLATE NOCASE");
        assert_eq!(Dialect::Postgres.position("a", "?1"), "strpos(a, ?1)");
    }
}
