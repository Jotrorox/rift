//! The instance database, shared by the instance registry, the permission
//! store, the player directory and (with `extensions.storage.database`) the
//! extension store.
//!
//! It is either a SQLite file beside the configuration (`instance_database`,
//! by default `rift.sqlite3`), a PostgreSQL database
//! (`database.backend = "postgres"`) or a MariaDB database
//! (`database.backend = "mariadb"`). Components use one connection API,
//! [`Conn`], and write their SQL once with `?N` placeholders; the few
//! constructs the dialects spell differently go through [`Dialect`], and the
//! MariaDB backend translates the rest (see [`mariadb`]).
//!
//! One process owns the database. For a file, an OS lock on `<file>.lock` is
//! taken at open and released at exit, even after a crash; opening runs
//! `PRAGMA integrity_check` and refuses a damaged file rather than serving
//! partial state. On a server, the owner holds a session lock (an advisory
//! lock in PostgreSQL, see [`postgres`]; a named lock in MariaDB); a standby
//! Rift (`database.standby`) waits for it, so a second controller takes over
//! when the first stops.
//!
//! Each component creates and migrates its own tables and records its schema
//! version in `rift_schema`. SQLite files written before that table existed
//! kept the instance schema version in `PRAGMA user_version`; it seeds the
//! `instances` row once.
//!
//! # Writes
//!
//! Every write is a typed [`Operation`] run through [`Database::run`]. The
//! database's [`Executor`] decides where it runs; the default one,
//! [`Local`], executes it on this connection in one transaction and then
//! calls the apply hooks ([`Database::on_change`]) of the tables it changed,
//! still holding the connection. Components keep their in-memory mirrors
//! current only through those hooks, so a write that arrives from elsewhere
//! (a changeset applied with [`changeset::apply_changeset`], followed by
//! [`Database::fire`]) refreshes them exactly as a local one does.
pub mod changeset;
pub mod copy;
pub(crate) mod mariadb;
pub mod operation;
pub(crate) mod postgres;
mod value;

pub use operation::{ChangedTables, Executor, Local, Operation, Outcome};
pub(crate) use value::params;
pub use value::{Error, FromValue, Kind, OptionalExtension, Result, Row, RowIndex, ToValue, Value};

use crate::provisioning;
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, RwLock, TryLockError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
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

const MARIADB_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS rift_schema (
    component VARCHAR(255) PRIMARY KEY NOT NULL,
    version BIGINT NOT NULL
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_nopad_bin";

/// The component whose version older files kept in `PRAGMA user_version`.
const LEGACY_COMPONENT: &str = "instances";

/// The environment variable `database.url_env` names by default.
pub const DEFAULT_URL_ENV: &str = "RIFT_DATABASE_URL";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// The SQL dialect of a connection, for the few constructs SQLite,
/// PostgreSQL and MariaDB spell differently. Everything else is written once.
///
/// PostgreSQL tables use `TEXT COLLATE "C"` (bytewise, like SQLite's BINARY)
/// and `BIGINT` for every integer and boolean, so ordering, uniqueness and
/// integer reads match SQLite. MariaDB tables do the same with the
/// `utf8mb4_nopad_bin` collation, `VARCHAR` for keyed text (MariaDB cannot
/// index `TEXT` whole) and `LONGBLOB` for blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
    Mariadb,
}

impl Dialect {
    /// `expr LIKE pattern`, ignoring ASCII case (SQLite's `LIKE` does). The
    /// caller appends any `ESCAPE` clause.
    pub fn like(self, expr: &str, pattern: &str) -> String {
        match self {
            Self::Sqlite => format!("{expr} LIKE {pattern}"),
            Self::Postgres => format!("{expr} ILIKE {pattern}"),
            Self::Mariadb => format!("LOWER({expr}) LIKE LOWER({pattern})"),
        }
    }

    /// The larger of two values: SQLite's two-argument `MAX`.
    pub fn greatest(self) -> &'static str {
        match self {
            Self::Sqlite => "MAX",
            Self::Postgres | Self::Mariadb => "GREATEST",
        }
    }

    /// `expr` folded for an ASCII case-insensitive comparison or ordering
    /// (SQLite's `COLLATE NOCASE`).
    pub fn nocase(self, expr: &str) -> String {
        match self {
            Self::Sqlite => format!("{expr} COLLATE NOCASE"),
            Self::Postgres => format!("lower({expr} COLLATE \"C\")"),
            Self::Mariadb => format!("LOWER({expr})"),
        }
    }

    /// The 1-based position of `needle` in `haystack`, 0 when absent.
    pub fn position(self, haystack: &str, needle: &str) -> String {
        match self {
            Self::Sqlite | Self::Mariadb => format!("instr({haystack}, {needle})"),
            Self::Postgres => format!("strpos({haystack}, {needle})"),
        }
    }

    /// The length of the text `expr` in bytes of UTF-8.
    pub fn octets(self, expr: &str) -> String {
        match self {
            Self::Sqlite => format!("length(CAST({expr} AS BLOB))"),
            Self::Postgres | Self::Mariadb => format!("octet_length({expr})"),
        }
    }

    /// The type of a blob column.
    pub fn blob(self) -> &'static str {
        match self {
            Self::Sqlite => "BLOB",
            Self::Postgres => "BYTEA",
            Self::Mariadb => "LONGBLOB",
        }
    }
}

/// A database server Rift can keep the instance database on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Postgres,
    Mariadb,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::Mariadb => "MariaDB",
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

/// `rift.config.database` with `backend = "postgres"` or `"mariadb"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub engine: Engine,
    pub url: Url,
    /// Certificates trusted for TLS in addition to the public CAs.
    pub ca: Option<PathBuf>,
    /// Wait for the Rift that owns the database to stop instead of failing.
    pub standby: bool,
    /// Keep Rift's tables in this schema (PostgreSQL) or database (MariaDB),
    /// created when missing.
    pub schema: Option<String>,
}

impl ServerConfig {
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
                    "database.url_env: {name} is not set; put the {} connection URL in it",
                    self.engine.name()
                ))),
            },
        }
    }

    fn postgres_settings(&self) -> io::Result<postgres::Settings> {
        let config: tokio_postgres::Config = self
            .url()?
            .parse()
            .map_err(|error| invalid(format!("database URL: {error}")))?;
        Ok(postgres::Settings {
            label: self.with_schema(postgres::label(&config)),
            config,
            tls: postgres::MakeRustls::new(self.ca.as_deref())?,
            schema: self.schema.clone(),
        })
    }

    fn mariadb_settings(&self) -> io::Result<mariadb::Settings> {
        let url = self.url()?;
        let label = self.label_of(&url).unwrap_or_else(|| self.describe());
        mariadb::Settings::new(&url, self.ca.as_deref(), self.schema.clone(), label)
    }

    fn with_schema(&self, mut label: String) -> String {
        if let Some(schema) = &self.schema {
            let what = match self.engine {
                Engine::Postgres => "schema",
                Engine::Mariadb => "database",
            };
            label.push_str(&format!(" ({what} {schema})"));
        }
        label
    }

    /// `url` without its password, if it parses.
    fn label_of(&self, url: &str) -> Option<String> {
        let label = match self.engine {
            Engine::Postgres => postgres::label(&url.parse::<tokio_postgres::Config>().ok()?),
            Engine::Mariadb => mariadb::label(&mariadb::options(url).ok()?),
        };
        Some(self.with_schema(label))
    }

    /// The database for messages, without its password.
    pub fn describe(&self) -> String {
        match self.url().ok().and_then(|url| self.label_of(&url)) {
            Some(label) => label,
            None => match &self.url {
                Url::Env(name) => format!("{} database from {name}", self.engine.name()),
                Url::Literal(_) => format!("{} database", self.engine.name()),
            },
        }
    }
}

/// Where the instance database lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Sqlite(PathBuf),
    Server(ServerConfig),
}

impl Location {
    /// The database for messages, without any password.
    pub fn describe(&self) -> String {
        match self {
            Self::Sqlite(path) => path.display().to_string(),
            Self::Server(config) => config.describe(),
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
    Mariadb(mariadb::Session),
}

/// How a server session treats the database's lock when it is held
/// elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ownership {
    /// Read-only, without the lock (`rift check --strict`).
    None,
    /// Take the lock, waiting up to `wait` for it.
    Own { wait: Duration },
    /// Wait for the lock as long as it takes (`database.standby`).
    Standby,
}

/// One connection to the instance database, of either backend. Statements
/// use `?N` placeholders and [`params!`] values.
pub struct Conn(Backend);

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Backend::Sqlite(_) => f.write_str("Conn(SQLite)"),
            Backend::Postgres(session) => write!(f, "Conn({session:?})"),
            Backend::Mariadb(session) => write!(f, "Conn({session:?})"),
        }
    }
}

impl Conn {
    pub fn dialect(&self) -> Dialect {
        match self.0 {
            Backend::Sqlite(_) => Dialect::Sqlite,
            Backend::Postgres(_) => Dialect::Postgres,
            Backend::Mariadb(_) => Dialect::Mariadb,
        }
    }

    /// Run one statement; the number of rows it changed.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        match &self.0 {
            Backend::Sqlite(connection) => Ok(connection
                .prepare_cached(sql)?
                .execute(rusqlite::params_from_iter(params.iter()))?),
            Backend::Postgres(session) => session.execute(sql, params),
            Backend::Mariadb(session) => session.execute(sql, params),
        }
    }

    /// Run statements without parameters, separated by semicolons.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        match &self.0 {
            Backend::Sqlite(connection) => Ok(connection.execute_batch(sql)?),
            Backend::Postgres(session) => session.execute_batch(sql),
            Backend::Mariadb(session) => session.execute_batch(sql),
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
            Backend::Mariadb(session) => session.each(sql, params, each),
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
            Backend::Mariadb(session) => session.begin(),
        }
    }

    fn finish(&self, commit: bool) -> Result<()> {
        match &self.0 {
            Backend::Sqlite(connection) => {
                Ok(connection.execute_batch(if commit { "COMMIT" } else { "ROLLBACK" })?)
            }
            Backend::Postgres(session) => session.finish(commit),
            Backend::Mariadb(session) => session.finish(commit),
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

    /// A SQLite connection opened elsewhere (such as a state machine's
    /// writer), so [`operation::execute_operation`] and [`changeset`] can
    /// use it. Foreign keys must be on, as [`Database::open`] sets them.
    pub fn from_sqlite(connection: Connection) -> Self {
        Self(Backend::Sqlite(connection))
    }

    /// The SQLite connection back, `None` for a server backend.
    pub fn into_sqlite(self) -> Option<Connection> {
        match self.0 {
            Backend::Sqlite(connection) => Some(connection),
            Backend::Postgres(_) | Backend::Mariadb(_) => None,
        }
    }

    /// The SQLite connection, for file-only pragmas.
    pub(crate) fn sqlite(&self) -> Option<&Connection> {
        match &self.0 {
            Backend::Sqlite(connection) => Some(connection),
            Backend::Postgres(_) | Backend::Mariadb(_) => None,
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

/// How a SQLite file is opened.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// One connection for reads and writes, `journal_mode=DELETE` and
    /// `synchronous=FULL`: a single Rift.
    #[default]
    Single,
    /// A replica of the cluster's state machine: WAL with
    /// `synchronous=NORMAL` (an entry lost in a crash is applied again from
    /// the Raft log), one writer connection ([`Database::lock`]) and
    /// [`READERS`] read-only connections ([`Database::read`]). Every table
    /// must have a primary key, since changesets skip tables without one.
    Replicated,
}

/// Read connections of a [`Mode::Replicated`] database.
pub const READERS: usize = 4;

/// Identifies a hook registered with [`Database::on_change`].
pub type HookId = u64;

/// What an apply hook is told.
#[derive(Debug, Clone, Copy)]
pub struct Change<'a> {
    /// The tables the write changed.
    pub tables: &'a ChangedTables,
    /// The operation, when the write was one ([`Database::run`], or an
    /// applied log entry that carries its operations).
    pub operation: Option<&'a Operation>,
    /// The operation's result.
    pub outcome: Option<&'a Outcome>,
    /// Full reload: the whole database may have changed (after a snapshot
    /// install). Every hook is called, and `tables` is empty.
    pub reload: bool,
}

type HookFn = dyn Fn(&Conn, &Change<'_>) + Send + Sync;

struct Hook {
    id: HookId,
    tables: BTreeSet<String>,
    callback: Arc<HookFn>,
}

pub struct Database {
    /// The file path or the URL without its password, for messages.
    label: String,
    /// The SQLite file, if this is one.
    path: Option<PathBuf>,
    /// The only connection, or the writer of a replicated database.
    connection: Mutex<Conn>,
    /// Read-only connections of a replicated database; empty otherwise.
    readers: Vec<Mutex<Conn>>,
    next_reader: AtomicUsize,
    mode: Mode,
    executor: RwLock<Arc<dyn Executor>>,
    hooks: RwLock<Vec<Hook>>,
    next_hook: AtomicU64,
    // The OS releases this lock even after a crash. Keep the lock file in place.
    _lock: Option<File>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("label", &self.label)
            .field("path", &self.path)
            .field("mode", &self.mode)
            .field("readers", &self.readers.len())
            .finish_non_exhaustive()
    }
}

impl Database {
    fn with_connection(
        label: String,
        path: Option<PathBuf>,
        connection: Conn,
        lock: Option<File>,
    ) -> Self {
        Self {
            label,
            path,
            connection: Mutex::new(connection),
            readers: Vec::new(),
            next_reader: AtomicUsize::new(0),
            mode: Mode::Single,
            executor: RwLock::new(Arc::new(Local)),
            hooks: RwLock::new(Vec::new()),
            next_hook: AtomicU64::new(1),
            _lock: lock,
        }
    }

    /// Open the database at `location` for this process: lock it, create
    /// what is missing and check it.
    pub fn connect(location: &Location) -> io::Result<Self> {
        match location {
            Location::Sqlite(path) => Self::open(path),
            Location::Server(config) => {
                let ownership = if config.standby {
                    Ownership::Standby
                } else {
                    Ownership::Own { wait: LOCK_WAIT }
                };
                let database = Self::server(config, ownership)?;
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
            Location::Server(config) => Self::server(config, Ownership::None),
        }
    }

    fn server(config: &ServerConfig, ownership: Ownership) -> io::Result<Self> {
        let failed = |label: &str, error: Error| {
            io::Error::other(format!("instance database {label}: {error}"))
        };
        let (label, backend) = match config.engine {
            Engine::Postgres => {
                let settings = config.postgres_settings()?;
                let label = settings.label.clone();
                let session = postgres::Session::open(Arc::new(settings), ownership)
                    .map_err(|error| failed(&label, error))?;
                (label, Backend::Postgres(session))
            }
            Engine::Mariadb => {
                let settings = config.mariadb_settings()?;
                let label = settings.label.clone();
                let session = mariadb::Session::open(Arc::new(settings), ownership)
                    .map_err(|error| failed(&label, error))?;
                (label, Backend::Mariadb(session))
            }
        };
        Ok(Self::with_connection(label, None, Conn(backend), None))
    }

    /// Lock, open (creating it if needed) and check a SQLite file.
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with(path, Mode::Single)
    }

    /// [`Database::open`] in `mode`.
    pub fn open_with(path: &Path, mode: Mode) -> io::Result<Self> {
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
        let mut database = Self::with_connection(
            path.display().to_string(),
            Some(path.clone()),
            Conn(Backend::Sqlite(connection)),
            Some(lock),
        );
        database.mode = mode;
        database.prepare(true)?;
        if mode == Mode::Replicated {
            for _ in 0..READERS {
                let reader = Connection::open_with_flags(
                    &path,
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(|e| database.error(e))?;
                reader
                    .busy_timeout(Duration::from_secs(2))
                    .map_err(|e| database.error(e))?;
                reader
                    .execute_batch("PRAGMA query_only=ON;")
                    .map_err(|e| database.error(e))?;
                database
                    .readers
                    .push(Mutex::new(Conn(Backend::Sqlite(reader))));
            }
            changeset::assert_primary_keys(&database.lock()).map_err(|e| database.error(e))?;
        }
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
        Ok(Self::with_connection(
            path.display().to_string(),
            Some(path.to_owned()),
            Conn(Backend::Sqlite(connection)),
            None,
        ))
    }

    /// A private in-memory SQLite database, for configurations without a
    /// file and for tests.
    pub fn memory() -> Self {
        let database = Self::with_connection(
            ":memory:".into(),
            None,
            Conn(Backend::Sqlite(
                Connection::open_in_memory().expect("in-memory SQLite"),
            )),
            None,
        );
        database
            .prepare(false)
            .expect("in-memory SQLite accepts the schema table");
        database
    }

    fn prepare(&self, durable: bool) -> io::Result<()> {
        let connection = self.lock();
        let Some(sqlite) = connection.sqlite() else {
            let schema = match connection.dialect() {
                Dialect::Mariadb => MARIADB_SCHEMA,
                _ => POSTGRES_SCHEMA,
            };
            return connection.execute_batch(schema).map_err(|e| self.error(e));
        };
        sqlite
            .busy_timeout(Duration::from_secs(2))
            .map_err(|e| self.error(e))?;
        if durable {
            sqlite
                .execute_batch(match self.mode {
                    Mode::Single => "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;",
                    Mode::Replicated => "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;",
                })
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

    /// The SQLite file, or `None` for server and in-memory databases.
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

    /// How this database was opened. PostgreSQL and in-memory databases are
    /// always [`Mode::Single`].
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The connection, for one statement or transaction at a time. In a
    /// replicated database this is the writer.
    pub fn lock(&self) -> MutexGuard<'_, Conn> {
        self.connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// A connection for reads: one of the read connections of a replicated
    /// database (a free one if any is), otherwise [`Database::lock`]. Reads
    /// see every committed write.
    pub fn read(&self) -> MutexGuard<'_, Conn> {
        if self.readers.is_empty() {
            return self.lock();
        }
        let start = self.next_reader.fetch_add(1, Ordering::Relaxed);
        for offset in 0..self.readers.len() {
            match self.readers[(start + offset) % self.readers.len()].try_lock() {
                Ok(reader) => return reader,
                Err(TryLockError::Poisoned(error)) => return error.into_inner(),
                Err(TryLockError::WouldBlock) => {}
            }
        }
        self.readers[start % self.readers.len()]
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    // Writes.

    /// Run `operation` through this database's executor: locally with the
    /// default [`Local`], on the cluster leader with the cluster's.
    pub fn run(&self, operation: Operation) -> io::Result<Outcome> {
        self.executor().run(self, operation, None)
    }

    /// [`Database::run`], giving up at `deadline` (for Lua callbacks and
    /// plugin hooks) with [`io::ErrorKind::TimedOut`].
    pub fn run_until(&self, operation: Operation, deadline: Instant) -> io::Result<Outcome> {
        self.executor().run(self, operation, Some(deadline))
    }

    fn executor(&self) -> Arc<dyn Executor> {
        self.executor
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Replace the executor; [`Local`] until then.
    pub fn set_executor(&self, executor: Arc<dyn Executor>) {
        *self
            .executor
            .write()
            .unwrap_or_else(|error| error.into_inner()) = executor;
    }

    /// Execute `operation` on this connection, as [`Local`] does: in one
    /// transaction that commits unless the outcome is a failure (which is
    /// rolled back and returned as a normal result), then call the hooks of
    /// the tables it changed while still holding the connection. With a
    /// `deadline`, waiting for the connection and committing both stop once
    /// it passes.
    pub fn execute_locally(
        &self,
        operation: Operation,
        deadline: Option<Instant>,
    ) -> io::Result<Outcome> {
        let mut connection = match deadline {
            Some(deadline) => self.lock_until(deadline)?,
            None => self.lock(),
        };
        let outcome = {
            let transaction = connection
                .transaction_immediate()
                .map_err(|e| self.error(e))?;
            let outcome = operation::execute_operation(&transaction, &operation);
            if outcome.failed() {
                // Dropping the transaction rolls it back.
                return Ok(outcome);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "script deadline exceeded",
                ));
            }
            transaction.commit().map_err(|e| self.error(e))?;
            outcome
        };
        let tables = operation.tables();
        self.fire(
            &connection,
            &Change {
                tables: &tables,
                operation: Some(&operation),
                outcome: Some(&outcome),
                reload: false,
            },
        );
        Ok(outcome)
    }

    // Apply hooks.

    /// Call `hook` after every write that changes one of `tables`, and on a
    /// full reload. Hooks run in registration order on the writing thread,
    /// holding the connection, which they get as their first argument: a
    /// hook reads through it and must not lock the database or run an
    /// operation itself. Keep the returned id to [`Database::remove_hook`]:
    /// a hook must not own the database (through an `Arc`), or neither is
    /// ever dropped.
    pub fn on_change(
        &self,
        tables: &[&str],
        hook: impl Fn(&Conn, &Change<'_>) + Send + Sync + 'static,
    ) -> HookId {
        let id = self.next_hook.fetch_add(1, Ordering::Relaxed);
        self.hooks
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .push(Hook {
                id,
                tables: tables.iter().map(|table| (*table).to_owned()).collect(),
                callback: Arc::new(hook),
            });
        id
    }

    pub fn remove_hook(&self, id: HookId) {
        self.hooks
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|hook| hook.id != id);
    }

    /// Call the hooks `change` concerns: those of a changed table, or every
    /// hook on a reload. The caller holds `connection` (the writer) and has
    /// committed the change. Local writes do this themselves; the cluster
    /// calls it after applying a log entry.
    pub fn fire(&self, connection: &Conn, change: &Change<'_>) {
        let hooks: Vec<Arc<HookFn>> = self
            .hooks
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|hook| {
                change.reload
                    || hook
                        .tables
                        .iter()
                        .any(|table| change.tables.contains(table))
            })
            .map(|hook| hook.callback.clone())
            .collect();
        for hook in hooks {
            hook(connection, change);
        }
    }

    /// Call every hook in full reload mode, as after a snapshot install.
    pub fn reload_all(&self) {
        let connection = self.lock();
        self.reload_all_on(&connection);
    }

    /// [`Database::reload_all`] while the caller holds the connection.
    pub fn reload_all_on(&self, connection: &Conn) {
        let tables = ChangedTables::default();
        self.fire(
            connection,
            &Change {
                tables: &tables,
                operation: None,
                outcome: None,
                reload: true,
            },
        );
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

    /// On a server: the Rift holding the database's lock, other than this
    /// connection. Always `None` for SQLite.
    pub fn owner(&self) -> io::Result<Option<String>> {
        let connection = self.lock();
        match &connection.0 {
            Backend::Sqlite(_) => Ok(None),
            Backend::Postgres(session) => session
                .holder()
                .map(|holder| holder.map(|holder| holder.to_string()))
                .map_err(|e| self.error(e)),
            Backend::Mariadb(session) => session
                .holder()
                .map(|holder| holder.map(|holder| holder.to_string()))
                .map_err(|e| self.error(e)),
        }
    }

    pub fn error(&self, error: impl std::fmt::Display) -> io::Error {
        io::Error::other(format!("instance database {}: {error}", self.label))
    }

    /// Bring `component` to version `latest` when this process opens it.
    /// `upgrade(dialect, from)` returns the SQL that turns version `from` (0
    /// when the component is new) into `latest`; it runs in one transaction
    /// with the version update (see [`migrate_component`]). A database
    /// written by a newer Rift is refused.
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
        let transaction = connection
            .transaction_immediate()
            .map_err(|e| self.error(e))?;
        migrate_component(&transaction, component, latest, latest, upgrade)
            .map_err(|e| self.error(e))?;
        transaction.commit().map_err(|e| self.error(e))
    }
}

/// Bring `component` to `to_version` inside the caller's transaction:
/// run `upgrade(dialect, from)`, the SQL that turns version `from` into
/// `latest` (the version this Rift writes), and record the version. Nothing
/// happens when the component is at `to_version` already; a component newer
/// than `latest`, or a target other than `latest`, is refused. `true` when
/// the schema changed. Each component's `migrate` function is built on this,
/// for startup and for a replicated `migrate` entry alike.
pub fn migrate_component(
    connection: &Conn,
    component: &str,
    to_version: u32,
    latest: u32,
    upgrade: impl FnOnce(Dialect, u32) -> String,
) -> Result<bool> {
    let current = version(connection, component)?;
    if current == to_version {
        return Ok(false);
    }
    if current > latest {
        return Err(Error::other(format!(
            "unsupported schema version {current} of {component}"
        )));
    }
    if to_version != latest {
        return Err(Error::other(format!(
            "{component} cannot be migrated to version {to_version}; this Rift writes version {latest}"
        )));
    }
    connection.execute_batch(&upgrade(connection.dialect(), current))?;
    connection.execute(
        "INSERT INTO rift_schema (component, version) VALUES (?1, ?2)
         ON CONFLICT(component) DO UPDATE SET version = excluded.version",
        params![component, latest],
    )?;
    Ok(true)
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
            Dialect::Mariadb => {
                "SELECT count(*) FROM information_schema.TABLES
                 WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'rift_schema'"
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

/// Tests against a real database server, when `RIFT_TEST_POSTGRES_URL` or
/// `RIFT_TEST_MARIADB_URL` names one (PostgreSQL first): each call gets a
/// fresh schema (PostgreSQL) or database (MariaDB) of its own.
#[cfg(test)]
pub(crate) fn test_server() -> Option<Location> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let variable = |name: &str| std::env::var(name).ok().filter(|url| !url.is_empty());
    let (engine, url) = match variable("RIFT_TEST_POSTGRES_URL") {
        Some(url) => (Engine::Postgres, url),
        None => (Engine::Mariadb, variable("RIFT_TEST_MARIADB_URL")?),
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    Some(Location::Server(ServerConfig {
        engine,
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

/// [`test_server`], or a note that the test was skipped.
#[cfg(test)]
pub(crate) fn test_server_or_skip(test: &str) -> Option<Location> {
    let location = test_server();
    if location.is_none() {
        println!(
            "{test}: skipped, neither RIFT_TEST_POSTGRES_URL nor RIFT_TEST_MARIADB_URL is set"
        );
    }
    location
}

#[cfg(test)]
mod seam_tests;

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

    /// The connection API every backend shares.
    fn exercise(database: &Database) {
        let blob = database.dialect().blob();
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
                "INSERT INTO conn_test (id) SELECT COALESCE(MAX(id), 0) + 1 FROM conn_test RETURNING id",
                params![],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(returned, 4);
        let like = format!(
            "SELECT count(*) FROM conn_test WHERE {} ESCAPE '\\'",
            connection.dialect().like("name", "?1")
        );
        let matched: i64 = connection
            .query_row(&like, params!["N%"], |r| r.get(0))
            .unwrap();
        assert_eq!(matched, 1, "LIKE ignores ASCII case on every backend");
        // Upserts count one row whether they insert or update, and one
        // parameter can appear twice.
        for expected in [1, 1] {
            let changed = connection
                .execute(
                    "INSERT INTO conn_test (id, name) VALUES (?1, ?2)
                     ON CONFLICT(id) DO UPDATE SET name = excluded.name",
                    params![9i64, "nine"],
                )
                .unwrap();
            assert_eq!(changed, expected);
        }
        let ignored = connection
            .execute(
                "INSERT INTO conn_test (id, name) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
                params![9i64, "other"],
            )
            .unwrap();
        assert_eq!(ignored, 0);
        // An UPDATE counts the rows it matched, even when nothing changes.
        let unchanged = connection
            .execute(
                "UPDATE conn_test SET name = ?2 WHERE id = ?1 AND name = ?2",
                params![9i64, "nine"],
            )
            .unwrap();
        assert_eq!(unchanged, 1);
    }

    #[test]
    fn the_connection_api_works_on_sqlite() {
        exercise(&Database::memory());
    }

    #[test]
    fn the_connection_api_works_on_a_server() {
        let Some(location) = test_server_or_skip("the_connection_api_works_on_a_server") else {
            return;
        };
        let database = Database::connect(&location).unwrap();
        assert_ne!(database.dialect(), Dialect::Sqlite);
        assert!(database.path().is_none());
        exercise(&database);
    }

    #[test]
    fn one_rift_owns_a_server_database() {
        let Some(location) = test_server_or_skip("one_rift_owns_a_server_database") else {
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
        let Some(Location::Server(mut config)) =
            test_server_or_skip("a_standby_takes_over_when_the_owner_stops")
        else {
            return;
        };
        let owner = Database::connect(&Location::Server(config.clone())).unwrap();
        config.standby = true;
        let standby = std::thread::spawn(move || Database::connect(&Location::Server(config)));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!standby.is_finished(), "the standby waits");
        drop(owner);
        let database = standby.join().unwrap().unwrap();
        assert_ne!(database.dialect(), Dialect::Sqlite);
    }

    #[test]
    fn dialect_spellings() {
        assert_eq!(Dialect::Postgres.like("a", "?1"), "a ILIKE ?1");
        assert_eq!(Dialect::Mariadb.like("a", "?1"), "LOWER(a) LIKE LOWER(?1)");
        assert_eq!(Dialect::Mariadb.position("a", "?1"), "instr(a, ?1)");
        assert_eq!(Dialect::Sqlite.nocase("x"), "x COLLATE NOCASE");
        assert_eq!(Dialect::Postgres.position("a", "?1"), "strpos(a, ?1)");
    }
}
