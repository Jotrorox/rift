//! What a replicated SQLite instance database needs beyond the connection
//! API: changesets from SQLite's session extension, captured from a
//! transaction that is rolled back and applied on another replica; a
//! deterministic checksum of the whole database; and the check that every
//! table has a primary key, without which sessions record nothing.
//!
//! The session functions are declared here instead of through rusqlite's
//! `session` feature, which needs bindgen and libclang at build time. The
//! bundled SQLite is compiled with `SQLITE_ENABLE_SESSION` and
//! `SQLITE_ENABLE_PREUPDATE_HOOK` (`LIBSQLITE3_FLAGS` in
//! `.cargo/config.toml`), so the symbols are in the library rusqlite links.
use super::{ChangedTables, Conn, Error, Result, Transaction, Value, params};
use rusqlite::ffi;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    ptr,
};

/// `sqlite3_session`, opaque.
type Session = c_void;

/// `sqlite3_changeset_iter`, opaque.
type ChangesetIter = c_void;

type ConflictHandler = extern "C" fn(*mut c_void, c_int, *mut ChangesetIter) -> c_int;
type FilterHandler = extern "C" fn(*mut c_void, *const c_char) -> c_int;

/// Foreign key actions are not taken while applying: the changeset already
/// holds the rows a cascade changed.
const SQLITE_CHANGESETAPPLY_FKNOACTION: c_int = 0x0008;
const SQLITE_CHANGESET_ABORT: c_int = 2;

unsafe extern "C" {
    fn sqlite3session_create(
        db: *mut ffi::sqlite3,
        database: *const c_char,
        session: *mut *mut Session,
    ) -> c_int;
    fn sqlite3session_delete(session: *mut Session);
    fn sqlite3session_attach(session: *mut Session, table: *const c_char) -> c_int;
    fn sqlite3session_changeset(
        session: *mut Session,
        size: *mut c_int,
        changeset: *mut *mut c_void,
    ) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn sqlite3changeset_apply_v2(
        db: *mut ffi::sqlite3,
        size: c_int,
        changeset: *mut c_void,
        filter: Option<FilterHandler>,
        conflict: Option<ConflictHandler>,
        context: *mut c_void,
        rebase: *mut *mut c_void,
        rebase_size: *mut c_int,
        flags: c_int,
    ) -> c_int;
    fn sqlite3changeset_start(
        iterator: *mut *mut ChangesetIter,
        size: c_int,
        changeset: *mut c_void,
    ) -> c_int;
    fn sqlite3changeset_next(iterator: *mut ChangesetIter) -> c_int;
    fn sqlite3changeset_op(
        iterator: *mut ChangesetIter,
        table: *mut *const c_char,
        columns: *mut c_int,
        operation: *mut c_int,
        indirect: *mut c_int,
    ) -> c_int;
    fn sqlite3changeset_finalize(iterator: *mut ChangesetIter) -> c_int;
}

/// The net row changes of a transaction, in SQLite's changeset format.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changeset(pub Vec<u8>);

impl Changeset {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The tables this changeset changes.
    pub fn tables(&self) -> Result<ChangedTables> {
        let mut tables = ChangedTables::default();
        if self.0.is_empty() {
            return Ok(tables);
        }
        let size = self.size()?;
        let mut iterator: *mut ChangesetIter = ptr::null_mut();
        // SAFETY: the buffer outlives the iterator, which only reads it and
        // is finalized below.
        let code =
            unsafe { sqlite3changeset_start(&mut iterator, size, self.0.as_ptr() as *mut c_void) };
        if code != ffi::SQLITE_OK {
            return Err(failure("changeset", code));
        }
        loop {
            // SAFETY: `iterator` is live until finalized.
            let code = unsafe { sqlite3changeset_next(iterator) };
            if code != ffi::SQLITE_ROW {
                break;
            }
            let mut table: *const c_char = ptr::null();
            let (mut columns, mut operation, mut indirect) = (0, 0, 0);
            // SAFETY: the iterator points at a change; the name it returns
            // lives as long as the iterator.
            let code = unsafe {
                sqlite3changeset_op(
                    iterator,
                    &mut table,
                    &mut columns,
                    &mut operation,
                    &mut indirect,
                )
            };
            if code == ffi::SQLITE_OK && !table.is_null() {
                // SAFETY: SQLite returns a NUL-terminated table name.
                tables.insert(
                    unsafe { CStr::from_ptr(table) }
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        // SAFETY: finalized once; it reports an error the iteration hit.
        let code = unsafe { sqlite3changeset_finalize(iterator) };
        if code != ffi::SQLITE_OK {
            return Err(failure("changeset", code));
        }
        Ok(tables)
    }

    fn size(&self) -> Result<c_int> {
        c_int::try_from(self.0.len()).map_err(|_| Error::other("changeset exceeds 2 GiB"))
    }
}

fn failure(what: &str, code: c_int) -> Error {
    // SAFETY: `sqlite3_errstr` returns a static NUL-terminated string.
    let text = unsafe { CStr::from_ptr(ffi::sqlite3_errstr(code)) };
    Error::other(format!("{what}: {} (code {code})", text.to_string_lossy()))
}

fn handle(connection: &Conn) -> Result<*mut ffi::sqlite3> {
    let sqlite = connection
        .sqlite()
        .ok_or_else(|| Error::other("changesets need a SQLite database"))?;
    // SAFETY: the handle is used only while the caller holds `connection`.
    Ok(unsafe { sqlite.handle() })
}

/// A session recording every table of `main`, deleted on drop.
struct Recording(*mut Session);

impl Recording {
    fn start(db: *mut ffi::sqlite3) -> Result<Self> {
        let mut session: *mut Session = ptr::null_mut();
        // SAFETY: `db` is an open connection; "main" is NUL-terminated.
        let code = unsafe { sqlite3session_create(db, c"main".as_ptr(), &mut session) };
        if code != ffi::SQLITE_OK {
            return Err(failure("session", code));
        }
        let recording = Self(session);
        // SAFETY: a null table name attaches every table.
        let code = unsafe { sqlite3session_attach(recording.0, ptr::null()) };
        if code != ffi::SQLITE_OK {
            return Err(failure("session", code));
        }
        Ok(recording)
    }

    fn changeset(&self) -> Result<Changeset> {
        let mut size: c_int = 0;
        let mut buffer: *mut c_void = ptr::null_mut();
        // SAFETY: the session is live; SQLite allocates the buffer.
        let code = unsafe { sqlite3session_changeset(self.0, &mut size, &mut buffer) };
        if code != ffi::SQLITE_OK {
            return Err(failure("session", code));
        }
        let bytes = if buffer.is_null() || size <= 0 {
            Vec::new()
        } else {
            // SAFETY: SQLite returned `size` bytes at `buffer`.
            unsafe { std::slice::from_raw_parts(buffer as *const u8, size as usize) }.to_vec()
        };
        // SAFETY: the buffer came from SQLite's allocator; null is allowed.
        unsafe { ffi::sqlite3_free(buffer) };
        Ok(Changeset(bytes))
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        // SAFETY: created by `sqlite3session_create` and deleted once,
        // before the connection closes (the caller holds it).
        unsafe { sqlite3session_delete(self.0) };
    }
}

/// Run `body` in a transaction recorded by a session attached to every
/// table, take the changeset, then roll the transaction back: the database is
/// left as it was. Savepoints `body` rolls back are not in the changeset.
pub fn capture<T>(
    connection: &mut Conn,
    body: impl FnOnce(&Transaction<'_>) -> T,
) -> Result<(T, Changeset)> {
    capture_on(connection, &[], body)
}

/// [`capture`] on top of changesets that are not applied yet (a leader's
/// entries in flight): they are applied first in the same transaction,
/// unrecorded, so the result holds only what `body` changed and applies
/// after them.
pub fn capture_on<T>(
    connection: &mut Conn,
    base: &[Changeset],
    body: impl FnOnce(&Transaction<'_>) -> T,
) -> Result<(T, Changeset)> {
    let db = handle(connection)?;
    let transaction = connection.transaction_immediate()?;
    for changeset in base {
        apply_changeset(&transaction, changeset)?;
    }
    let recording = Recording::start(db)?;
    let value = body(&transaction);
    let changeset = recording.changeset()?;
    drop(recording);
    // Dropping the transaction rolls it back.
    drop(transaction);
    Ok((value, changeset))
}

extern "C" fn abort_on_conflict(_: *mut c_void, _: c_int, _: *mut ChangesetIter) -> c_int {
    SQLITE_CHANGESET_ABORT
}

/// Apply `changeset` inside the caller's transaction. Foreign key actions
/// are not taken (the changeset holds the cascaded rows), and any conflict
/// aborts the whole apply: a replica that conflicts has diverged. Returns the
/// tables it changed, for [`super::Database::fire`].
pub fn apply_changeset(connection: &Conn, changeset: &Changeset) -> Result<ChangedTables> {
    let tables = changeset.tables()?;
    if changeset.is_empty() {
        return Ok(tables);
    }
    let db = handle(connection)?;
    let size = changeset.size()?;
    // SAFETY: the buffer outlives the call and is only read; the handlers
    // are plain functions.
    let code = unsafe {
        sqlite3changeset_apply_v2(
            db,
            size,
            changeset.0.as_ptr() as *mut c_void,
            None,
            Some(abort_on_conflict),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            SQLITE_CHANGESETAPPLY_FKNOACTION,
        )
    };
    if code != ffi::SQLITE_OK {
        return Err(failure("changeset apply aborted", code));
    }
    Ok(tables)
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Every table of the database in name order, without SQLite's own.
fn tables(connection: &Conn) -> Result<Vec<String>> {
    connection.query_map(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
         ORDER BY name",
        params![],
        |row| row.get(0),
    )
}

/// The primary key columns of `table` in key order.
fn primary_key(connection: &Conn, table: &str) -> Result<Vec<String>> {
    connection.query_map(
        "SELECT name FROM pragma_table_info(?1) WHERE pk > 0 ORDER BY pk",
        params![table],
        |row| row.get(0),
    )
}

/// Refuse a database with a table that has no primary key: a session would
/// silently leave its changes out of every changeset.
pub fn assert_primary_keys(connection: &Conn) -> Result<()> {
    if connection.sqlite().is_none() {
        return Err(Error::other("replication needs a SQLite database"));
    }
    let mut missing = Vec::new();
    for table in tables(connection)? {
        if primary_key(connection, &table)?.is_empty() {
            missing.push(table);
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::other(format!(
            "tables without a primary key cannot be replicated: {}",
            missing.join(", ")
        )))
    }
}

/// A SHA-256 over every table, its name and its rows in primary key order,
/// each value with its type. Two replicas with the same contents have the
/// same checksum.
pub fn checksum(connection: &Conn) -> Result<[u8; 32]> {
    if connection.sqlite().is_none() {
        return Err(Error::other("checksums need a SQLite database"));
    }
    let mut hasher = Sha256::new();
    for table in tables(connection)? {
        let key = primary_key(connection, &table)?;
        let order = if key.is_empty() {
            "rowid".to_owned()
        } else {
            key.iter()
                .map(|column| quote(column))
                .collect::<Vec<_>>()
                .join(", ")
        };
        hasher.update((table.len() as u64).to_le_bytes());
        hasher.update(table.as_bytes());
        connection.each(
            &format!("SELECT * FROM {} ORDER BY {order}", quote(&table)),
            params![],
            |row| -> Result<bool> {
                hasher.update(b"r");
                for value in row.values() {
                    match value {
                        Value::Null(_) => hasher.update(b"n"),
                        Value::Integer(integer) => {
                            hasher.update(b"i");
                            hasher.update(integer.to_le_bytes());
                        }
                        Value::Real(real) => {
                            hasher.update(b"f");
                            hasher.update(real.to_bits().to_le_bytes());
                        }
                        Value::Text(text) => {
                            hasher.update(b"t");
                            hasher.update((text.len() as u64).to_le_bytes());
                            hasher.update(text.as_bytes());
                        }
                        Value::Blob(bytes) => {
                            hasher.update(b"b");
                            hasher.update((bytes.len() as u64).to_le_bytes());
                            hasher.update(bytes);
                        }
                    }
                }
                Ok(true)
            },
        )?;
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(hasher.finalize().as_slice());
    Ok(digest)
}
