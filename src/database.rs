//! The SQLite file beside the configuration (`instance_database`, by default
//! `rift.sqlite3`), shared by the instance registry and the permission store.
//!
//! One process owns the file: an OS lock on `<file>.lock` is taken at open and
//! released at exit, even after a crash. Opening runs `PRAGMA integrity_check`
//! and refuses a damaged file rather than serving partial state.
//!
//! Each component creates and migrates its own tables and records its schema
//! version in `rift_schema`. Files written before that table existed kept the
//! instance schema version in `PRAGMA user_version`; it seeds the `instances`
//! row once.
use crate::provisioning;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, TryLockError},
    time::{Duration, Instant},
};

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS rift_schema (
    component TEXT PRIMARY KEY NOT NULL,
    version INTEGER NOT NULL
) STRICT;";

/// The component whose version older files kept in `PRAGMA user_version`.
const LEGACY_COMPONENT: &str = "instances";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
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

/// The lock file guarding `path`.
pub(crate) fn lock_path(path: &Path) -> PathBuf {
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    PathBuf::from(lock)
}

#[derive(Debug)]
pub struct Database {
    path: PathBuf,
    connection: Mutex<Connection>,
    // The OS releases this lock even after a crash. Keep the lock file in place.
    _lock: Option<File>,
}

impl Database {
    /// Lock, open (creating it if needed) and check the file.
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = provisioning::checked_path(path)?;
        fs::create_dir_all(path.parent().unwrap())?;
        let lock = private_file(&lock_path(&path))?;
        lock.try_lock().map_err(|error| {
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
            path,
            connection: Mutex::new(connection),
            _lock: Some(lock),
        };
        database.prepare(true)?;
        Ok(database)
    }

    /// Open an existing file read-only, without locking, checking or
    /// migrating it (for `rift check --strict`).
    pub fn read_only(path: &Path) -> io::Result<Self> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| invalid(format!("instance database {}: {e}", path.display())))?;
        let database = Self {
            path: path.to_owned(),
            connection: Mutex::new(connection),
            _lock: None,
        };
        database
            .lock()
            .busy_timeout(Duration::from_secs(2))
            .map_err(|e| database.error(e))?;
        Ok(database)
    }

    /// A private in-memory database, for configurations without a file and
    /// for tests.
    pub fn memory() -> Self {
        let database = Self {
            path: PathBuf::from(":memory:"),
            connection: Mutex::new(Connection::open_in_memory().expect("in-memory SQLite")),
            _lock: None,
        };
        database
            .prepare(false)
            .expect("in-memory SQLite accepts the schema table");
        database
    }

    fn prepare(&self, durable: bool) -> io::Result<()> {
        let connection = self.lock();
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|e| self.error(e))?;
        if durable {
            connection
                .execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
                .map_err(|e| self.error(e))?;
            let check: String = connection
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .map_err(|e| self.error(e))?;
            if check != "ok" {
                return Err(self.error(format!("integrity check failed: {check}")));
            }
        }
        connection
            .execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| self.error(e))?;
        let legacy: u32 = connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| self.error(e))?;
        connection
            .execute_batch(SCHEMA)
            .map_err(|e| self.error(e))?;
        if legacy > 0 {
            connection
                .execute(
                    "INSERT OR IGNORE INTO rift_schema (component, version) VALUES (?1, ?2)",
                    params![LEGACY_COMPONENT, legacy],
                )
                .map_err(|e| self.error(e))?;
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The connection, for one statement or transaction at a time.
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// [`Database::lock`], giving up at `deadline` (for Lua callbacks).
    pub fn lock_until(&self, deadline: Instant) -> io::Result<MutexGuard<'_, Connection>> {
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

    pub fn error(&self, error: impl std::fmt::Display) -> io::Error {
        io::Error::other(format!(
            "instance database {}: {error}",
            self.path.display()
        ))
    }

    /// Bring `component` to version `latest`. `upgrade(from)` returns the SQL
    /// that turns version `from` (0 when the component is new) into `latest`;
    /// it runs in one transaction with the version update. A file written by
    /// a newer Rift is refused.
    pub fn migrate(
        &self,
        component: &str,
        latest: u32,
        upgrade: impl FnOnce(u32) -> String,
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
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| self.error(e))?;
        transaction
            .execute_batch(&upgrade(version))
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
pub fn version(connection: &Connection, component: &str) -> rusqlite::Result<u32> {
    let table: Option<String> = connection
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'rift_schema'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if table.is_some()
        && let Some(version) = connection
            .query_row(
                "SELECT version FROM rift_schema WHERE component = ?1",
                [component],
                |r| r.get(0),
            )
            .optional()?
    {
        return Ok(version);
    }
    if component == LEGACY_COMPONENT {
        return connection.query_row("PRAGMA user_version", [], |r| r.get(0));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            .migrate("permissions", 1, |from| {
                assert_eq!(from, 0);
                "CREATE TABLE p (x);".into()
            })
            .unwrap();
        // Already current: the upgrade is not asked for.
        database
            .migrate("permissions", 1, |_| unreachable!())
            .unwrap();
        assert!(
            database
                .migrate("permissions", 0, |_| String::new())
                .is_err()
        );
        assert!(Database::open(&path).is_err(), "the lock is exclusive");
        drop(database);
        let reopened = Database::open(&path).unwrap();
        assert_eq!(version(&reopened.lock(), "permissions").unwrap(), 1);
        drop(reopened);
        fs::remove_dir_all(directory).unwrap();
    }
}
