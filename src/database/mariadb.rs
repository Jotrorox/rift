//! The MariaDB backend of [`Conn`](super::Conn).
//!
//! Each connection runs on its own thread with a small Tokio runtime that
//! drives `mysql_async`; callers hand it one request at a time and wait for
//! the answer, so the synchronous database API works from any thread, also
//! from inside Rift's own runtime, exactly like SQLite and PostgreSQL do.
//!
//! Components write their SQL once for every backend (`?N` placeholders,
//! `ON CONFLICT` upserts, a column named `key`); [`translate`] rewrites it.
//! Sessions run with `ANSI_QUOTES` (`"` quotes identifiers),
//! `PIPES_AS_CONCAT` (`||` concatenates), `NO_BACKSLASH_ESCAPES` and strict
//! tables, and compare text bytewise (`utf8mb4_nopad_bin`, like SQLite's
//! BINARY and PostgreSQL's `"C"`). `INSERT ... RETURNING` needs MariaDB 10.5
//! or newer; MySQL lacks it and is refused.
//!
//! A Rift that owns the database holds a named lock (`GET_LOCK`), the
//! counterpart of the SQLite lock file and of PostgreSQL's advisory lock.
//! When its connection drops, it reconnects and takes the lock again; if
//! another Rift took it meanwhile, this one has been replaced and exits
//! rather than write beside it.
//!
//! MariaDB commits DDL implicitly, so a migration that fails halfway leaves
//! the tables it already created behind.
use super::{
    Ownership,
    value::{Error, Kind, Result, Row, Value},
};
use mysql_async::{Opts, OptsBuilder, Params, consts::ColumnType, prelude::Queryable};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io,
    path::Path,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

/// How long one statement may take before the connection is given up.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a connection may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a reconnecting owner waits for its previous session to release
/// the lock before it gives up.
const RECLAIM_WAIT: Duration = Duration::from_secs(30);
/// An idle connection is pinged this often, so a closed one is noticed (and
/// replaced) before the next statement, and `wait_timeout` never expires.
const IDLE_PING: Duration = Duration::from_secs(30);
/// Prepared statements kept per connection.
const MAX_STATEMENTS: usize = 512;
/// The oldest MariaDB with `INSERT ... RETURNING`.
const MINIMUM_VERSION: (u32, u32) = (10, 5);
/// The character set of byte strings: such a column holds a blob.
const BINARY: u16 = 63;

/// The name of this database's lock: one per database of a server, so Rifts
/// in different databases do not exclude each other.
const LOCK_NAME: &str = "CONCAT('rift:', MD5(DATABASE()))";

/// Run on every new connection, before anything else.
const SESSION: &[&str] = &[
    "SET SESSION sql_mode = 'ANSI_QUOTES,PIPES_AS_CONCAT,NO_BACKSLASH_ESCAPES,STRICT_ALL_TABLES,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION'",
    "SET NAMES utf8mb4 COLLATE utf8mb4_nopad_bin",
];

/// Column names of Rift's tables that MariaDB reserves; [`translate`]
/// quotes them.
const RESERVED: &[&str] = &["key", "groups"];
/// Words after which `KEY` is the keyword, not the column.
const KEY_KEYWORDS: &[&str] = &["primary", "foreign", "unique", "duplicate"];

/// A statement rewritten for MariaDB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Translated {
    pub sql: String,
    /// The parameter (from 0) behind each `?`, in order.
    pub order: Vec<usize>,
    /// An `ON CONFLICT ... DO UPDATE`: MariaDB counts an updated row twice.
    pub upsert: bool,
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// `ON CONFLICT [(columns)] DO UPDATE SET` or `ON CONFLICT [(columns)] DO
/// NOTHING` starting after `ON` at `at`: where it ends, and whether it is
/// `DO NOTHING`.
fn on_conflict(bytes: &[u8], at: usize) -> Option<(usize, bool)> {
    let skip = |mut at: usize| {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        at
    };
    let word = |at: usize, expected: &str| -> Option<usize> {
        let end = at + expected.len();
        (end <= bytes.len()
            && bytes[at..end].eq_ignore_ascii_case(expected.as_bytes())
            && !bytes.get(end).copied().is_some_and(is_word_byte))
        .then_some(end)
    };
    let mut at = word(skip(at), "conflict")?;
    at = skip(at);
    if bytes.get(at) == Some(&b'(') {
        at += bytes[at..].iter().position(|byte| *byte == b')')? + 1;
        at = skip(at);
    }
    at = skip(word(at, "do")?);
    if let Some(end) = word(at, "nothing") {
        return Some((end, true));
    }
    at = word(at, "update")?;
    Some((word(skip(at), "set")?, false))
}

/// Rewrite a statement written for SQLite and PostgreSQL for MariaDB,
/// leaving string literals, quoted identifiers and comments alone:
///
/// - `?N` (and bare `?`) become `?`, with [`Translated::order`] naming the
///   parameter each one binds, so one parameter can appear twice;
/// - `ON CONFLICT (...) DO UPDATE SET` becomes `ON DUPLICATE KEY UPDATE` and
///   `excluded.column` becomes `VALUES("column")`;
/// - `ON CONFLICT DO NOTHING` becomes `INSERT IGNORE`;
/// - the reserved column names `key` and `groups` are quoted.
pub(super) fn translate(sql: &str) -> Result<Translated> {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut order = Vec::new();
    let mut next = 0;
    let mut upsert = false;
    let mut ignore = false;
    let mut previous: Option<&str> = None;
    let mut after_dot = false;
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        match byte {
            b'\'' | b'"' | b'`' => {
                let end = bytes[at + 1..]
                    .iter()
                    .position(|inner| *inner == byte)
                    .map_or(bytes.len(), |offset| at + 1 + offset + 1);
                out.push_str(&sql[at..end]);
                at = end;
                previous = None;
                after_dot = false;
            }
            b'-' if bytes.get(at + 1) == Some(&b'-') => {
                let end = bytes[at..]
                    .iter()
                    .position(|inner| *inner == b'\n')
                    .map_or(bytes.len(), |offset| at + offset + 1);
                out.push_str(&sql[at..end]);
                at = end;
            }
            b'?' => {
                let start = at + 1;
                let mut end = start;
                while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                    end += 1;
                }
                let index = if end == start {
                    next += 1;
                    next - 1
                } else {
                    match sql[start..end].parse::<usize>() {
                        Ok(number) if number > 0 => number - 1,
                        _ => {
                            return Err(Error::other(format!(
                                "invalid placeholder ?{}",
                                &sql[start..end]
                            )));
                        }
                    }
                };
                order.push(index);
                out.push('?');
                at = end;
                previous = None;
                after_dot = false;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = at;
                while bytes.get(at).copied().is_some_and(is_word_byte) {
                    at += 1;
                }
                let word = &sql[start..at];
                if !after_dot
                    && word.eq_ignore_ascii_case("excluded")
                    && bytes.get(at) == Some(&b'.')
                {
                    let column = at + 1;
                    let mut end = column;
                    while bytes.get(end).copied().is_some_and(is_word_byte) {
                        end += 1;
                    }
                    if end > column {
                        out.push_str(&format!("VALUES(\"{}\")", &sql[column..end]));
                        at = end;
                        previous = None;
                        after_dot = false;
                        continue;
                    }
                }
                if !after_dot
                    && word.eq_ignore_ascii_case("on")
                    && let Some((end, nothing)) = on_conflict(bytes, at)
                {
                    if nothing {
                        ignore = true;
                    } else {
                        upsert = true;
                        out.push_str("ON DUPLICATE KEY UPDATE");
                    }
                    at = end;
                    previous = None;
                    continue;
                }
                let keyword = word.eq_ignore_ascii_case("key")
                    && previous.is_some_and(|previous| {
                        KEY_KEYWORDS
                            .iter()
                            .any(|keyword| previous.eq_ignore_ascii_case(keyword))
                    });
                if !after_dot
                    && !keyword
                    && RESERVED
                        .iter()
                        .any(|reserved| word.eq_ignore_ascii_case(reserved))
                {
                    out.push('"');
                    out.push_str(word);
                    out.push('"');
                } else {
                    out.push_str(word);
                }
                previous = Some(word);
                after_dot = false;
            }
            byte if byte.is_ascii_digit() => {
                let start = at;
                while bytes
                    .get(at)
                    .is_some_and(|inner| inner.is_ascii_alphanumeric() || *inner == b'.')
                {
                    at += 1;
                }
                out.push_str(&sql[start..at]);
                previous = None;
                after_dot = false;
            }
            b'.' => {
                out.push('.');
                at += 1;
                after_dot = true;
            }
            byte => {
                let length = sql[at..].chars().next().map_or(1, char::len_utf8);
                out.push_str(&sql[at..at + length]);
                at += length;
                if !byte.is_ascii_whitespace() {
                    previous = None;
                    after_dot = false;
                }
            }
        }
    }
    if ignore {
        let start = out.len() - out.trim_start().len();
        let head = &out.as_bytes()[start..];
        if head.len() < 6 || !head[..6].eq_ignore_ascii_case(b"insert") {
            return Err(Error::other("ON CONFLICT DO NOTHING outside an INSERT"));
        }
        out.insert_str(start + 6, " IGNORE");
    }
    Ok(Translated {
        sql: out,
        order,
        upsert,
    })
}

fn to_mysql(value: &Value) -> mysql_async::Value {
    match value {
        Value::Null(_) => mysql_async::Value::NULL,
        Value::Integer(integer) => mysql_async::Value::Int(*integer),
        Value::Real(real) => mysql_async::Value::Double(*real),
        Value::Text(text) => mysql_async::Value::Bytes(text.as_bytes().to_vec()),
        Value::Blob(bytes) => mysql_async::Value::Bytes(bytes.clone()),
    }
}

fn integer_type(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::MYSQL_TYPE_TINY
            | ColumnType::MYSQL_TYPE_SHORT
            | ColumnType::MYSQL_TYPE_INT24
            | ColumnType::MYSQL_TYPE_LONG
            | ColumnType::MYSQL_TYPE_LONGLONG
            | ColumnType::MYSQL_TYPE_YEAR
    )
}

fn decimal_type(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL
    )
}

fn real_type(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE
    )
}

/// A number sent as text (`DECIMAL`, as `SUM` returns): an integer when it
/// has no fraction, otherwise a real.
fn number(index: usize, bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::Conversion(format!("column {index}: invalid number")))?;
    let whole = match text.split_once('.') {
        None => Some(text),
        Some((whole, fraction)) => fraction.bytes().all(|b| b == b'0').then_some(whole),
    };
    if let Some(whole) = whole
        && let Ok(integer) = whole.parse::<i64>()
    {
        return Ok(Value::Integer(integer));
    }
    text.parse::<f64>()
        .map(Value::Real)
        .map_err(|_| Error::Conversion(format!("column {index}: invalid number {text}")))
}

fn value(index: usize, value: mysql_async::Value, ty: ColumnType, binary: bool) -> Result<Value> {
    use mysql_async::Value as My;
    Ok(match value {
        My::NULL => Value::Null(if integer_type(ty) || decimal_type(ty) {
            Kind::Integer
        } else if real_type(ty) {
            Kind::Real
        } else if binary {
            Kind::Blob
        } else {
            Kind::Text
        }),
        My::Int(integer) => Value::Integer(integer),
        My::UInt(integer) => Value::Integer(i64::try_from(integer).unwrap_or(i64::MAX)),
        My::Float(real) => Value::Real(real.into()),
        My::Double(real) => Value::Real(real),
        My::Bytes(bytes) if integer_type(ty) || decimal_type(ty) || real_type(ty) => {
            number(index, &bytes)?
        }
        My::Bytes(bytes) if binary => Value::Blob(bytes),
        My::Bytes(bytes) => Value::Text(String::from_utf8(bytes).map_err(|error| {
            Error::Conversion(format!("column {index}: invalid UTF-8 text: {error}"))
        })?),
        My::Date(..) | My::Time(..) => {
            return Err(Error::Conversion(format!(
                "column {index} has unsupported type {ty:?}"
            )));
        }
    })
}

fn row(row: mysql_async::Row) -> Result<Row> {
    let columns: Vec<(ColumnType, bool)> = row
        .columns_ref()
        .iter()
        .map(|column| (column.column_type(), column.character_set() == BINARY))
        .collect();
    row.unwrap_raw()
        .into_iter()
        .zip(columns)
        .enumerate()
        .map(|(index, (raw, (ty, binary)))| {
            value(index, raw.unwrap_or(mysql_async::Value::NULL), ty, binary)
        })
        .collect::<Result<Vec<_>>>()
        .map(Row)
}

/// Everything a connection needs, resolved from `rift.config.database`.
pub(crate) struct Settings {
    pub opts: Opts,
    /// The URL without its password, for messages.
    pub label: String,
    /// `database.schema`: the database to use, created when missing.
    pub schema: Option<String>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

/// Parse a `mariadb://` (or `mysql://`) URL with `mysql_async`'s query
/// parameters, such as `require_ssl=true`.
pub(crate) fn options(url: &str) -> std::result::Result<Opts, String> {
    let url = match url.split_once("://") {
        Some((scheme, rest))
            if scheme.eq_ignore_ascii_case("mariadb") || scheme.eq_ignore_ascii_case("mysql") =>
        {
            format!("mysql://{rest}")
        }
        _ => return Err("expected a mariadb:// URL".into()),
    };
    Opts::from_url(&url).map_err(|error| error.to_string())
}

/// The URL `opts` connects to, without its password.
pub(crate) fn label(opts: &Opts) -> String {
    let user = opts
        .user()
        .map(|user| format!("{user}@"))
        .unwrap_or_default();
    format!(
        "mariadb://{user}{}:{}/{}",
        opts.ip_or_hostname(),
        opts.tcp_port(),
        opts.db_name().unwrap_or_default()
    )
}

impl Settings {
    /// Connection settings for `url`, trusting `ca` for TLS in addition to
    /// the public CAs. A `ca` turns TLS on; so does `require_ssl=true`.
    pub(crate) fn new(
        url: &str,
        ca: Option<&Path>,
        schema: Option<String>,
        label: String,
    ) -> io::Result<Self> {
        let base = options(url).map_err(|error| invalid(format!("database URL: {error}")))?;
        let mut ssl = base.ssl_opts().cloned();
        if let Some(ca) = ca {
            std::fs::metadata(ca)
                .map_err(|error| invalid(format!("database.ca {}: {error}", ca.display())))?;
            ssl = Some(
                ssl.unwrap_or_default()
                    .with_root_certs(vec![ca.to_path_buf().into()]),
            );
        }
        if ssl.is_some() {
            // mysql_async builds its TLS configuration with the process-wide
            // provider; Rift uses aws-lc-rs everywhere.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
        let opts = OptsBuilder::from_opts(base)
            .ssl_opts(ssl)
            // Like SQLite and PostgreSQL, count the rows an UPDATE matched,
            // not only those it changed.
            .client_found_rows(true)
            .stmt_cache_size(MAX_STATEMENTS)
            .tcp_keepalive(Some(Duration::from_secs(60)))
            .init(SESSION.to_vec())
            .into();
        Ok(Self {
            opts,
            label,
            schema,
        })
    }
}

/// `name` as a quoted identifier (the session uses `ANSI_QUOTES`).
fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Whether `version` (`SELECT VERSION()`) is a MariaDB Rift supports.
fn supported(version: &str) -> Result<()> {
    let mut numbers = version
        .split(|c: char| !c.is_ascii_digit())
        .map(|part| part.parse::<u32>().unwrap_or(0));
    let found = (numbers.next().unwrap_or(0), numbers.next().unwrap_or(0));
    if !version.to_ascii_lowercase().contains("mariadb") {
        return Err(Error::other(format!(
            "server {version} is not MariaDB; Rift needs MariaDB {}.{} or newer (MySQL lacks INSERT ... RETURNING)",
            MINIMUM_VERSION.0, MINIMUM_VERSION.1
        )));
    }
    if found < MINIMUM_VERSION {
        return Err(Error::other(format!(
            "MariaDB {version} is too old; Rift needs {}.{} or newer",
            MINIMUM_VERSION.0, MINIMUM_VERSION.1
        )));
    }
    Ok(())
}

enum Op {
    /// A prepared statement; its rows and affected-row count.
    Run {
        sql: String,
        params: Vec<mysql_async::Value>,
    },
    /// Statements without parameters, separated by semicolons.
    Batch(String),
}

struct Outcome {
    rows: Vec<Row>,
    affected: u64,
}

struct Request {
    op: Op,
    reply: mpsc::SyncSender<Result<Outcome>>,
}

/// One connection, on its own thread.
struct Worker {
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
    /// The server's id of the connection (`CONNECTION_ID()`).
    id: u32,
}

async fn run(connection: &mut mysql_async::Conn, op: Op) -> Result<Outcome> {
    match op {
        Op::Batch(sql) => {
            connection.query_drop(sql).await?;
            Ok(Outcome {
                rows: Vec::new(),
                affected: 0,
            })
        }
        Op::Run { sql, params } => {
            let params = if params.is_empty() {
                Params::Empty
            } else {
                Params::Positional(params)
            };
            let mut result = connection.exec_iter(sql, params).await?;
            let mut rows = Vec::new();
            let mut failed = None;
            result
                .for_each(|next| {
                    if failed.is_none() {
                        match row(next) {
                            Ok(row) => rows.push(row),
                            Err(error) => failed = Some(error),
                        }
                    }
                })
                .await?;
            let affected = result.affected_rows();
            result.drop_result().await?;
            match failed {
                Some(error) => Err(error),
                None => Ok(Outcome { rows, affected }),
            }
        }
    }
}

/// Open a connection on its own thread.
fn connect(settings: &Settings) -> Result<Worker> {
    let opts = settings.opts.clone();
    let (ready, connected) = mpsc::channel();
    std::thread::Builder::new()
        .name("rift-mariadb".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready.send(Err(Error::other(format!(
                        "cannot start the database thread: {error}"
                    ))));
                    return;
                }
            };
            runtime.block_on(async move {
                let mut connection =
                    match tokio::time::timeout(CONNECT_TIMEOUT, mysql_async::Conn::new(opts)).await
                    {
                        Ok(Ok(connection)) => connection,
                        Ok(Err(error)) => {
                            let _ = ready.send(Err(Error::Mariadb(error)));
                            return;
                        }
                        Err(_) => {
                            let _ = ready.send(Err(Error::other(format!(
                                "timed out after {} s connecting",
                                CONNECT_TIMEOUT.as_secs()
                            ))));
                            return;
                        }
                    };
                let (requests, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Request>();
                let id = connection.id();
                if ready.send(Ok(Worker { requests, id })).is_err() {
                    return;
                }
                loop {
                    let request = match tokio::time::timeout(IDLE_PING, receiver.recv()).await {
                        Ok(Some(request)) => request,
                        // The session let go of the connection.
                        Ok(None) => break,
                        Err(_) => {
                            match tokio::time::timeout(STATEMENT_TIMEOUT, connection.ping()).await {
                                Ok(Ok(())) => continue,
                                Ok(Err(error)) => {
                                    crate::log_warn!("MariaDB connection closed: {error}");
                                }
                                Err(_) => {
                                    crate::log_warn!(
                                        "MariaDB connection closed: no answer to a ping"
                                    );
                                }
                            }
                            // The session sees the closed channel and
                            // reconnects before its next statement.
                            return;
                        }
                    };
                    let result = match tokio::time::timeout(
                        STATEMENT_TIMEOUT,
                        run(&mut connection, request.op),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            let _ = request.reply.send(Err(Error::other(format!(
                                "no answer from the database within {} s",
                                STATEMENT_TIMEOUT.as_secs()
                            ))));
                            return;
                        }
                    };
                    let fatal = matches!(&result, Err(Error::Mariadb(error)) if error.is_fatal());
                    let _ = request.reply.send(result);
                    if fatal {
                        return;
                    }
                }
                let _ = connection.disconnect().await;
            });
        })
        .map_err(|error| Error::other(format!("cannot start the database thread: {error}")))?;
    connected
        .recv()
        .map_err(|_| Error::other("the database thread stopped while connecting"))?
}

/// Who holds the lock: the connection id, with its account and address when
/// this account may see them.
pub(crate) struct Holder {
    pub id: i64,
    pub account: Option<(String, String)>,
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.account {
            Some((user, host)) => write!(f, "connection {} of {user} from {host}", self.id),
            None => write!(f, "connection {}", self.id),
        }
    }
}

/// One connection with its translated statements.
pub(crate) struct Session {
    settings: Arc<Settings>,
    /// Holds the lock, and must keep holding it across reconnects.
    owner: bool,
    read_only: bool,
    worker: RefCell<Option<Arc<Worker>>>,
    /// The id of the connection before the current one, so a reconnecting
    /// owner recognises its own stale session.
    previous: Cell<Option<u32>>,
    statements: RefCell<HashMap<String, Arc<Translated>>>,
    transaction: Cell<bool>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("database", &self.settings.label)
            .finish_non_exhaustive()
    }
}

impl Session {
    pub(crate) fn open(settings: Arc<Settings>, ownership: Ownership) -> Result<Self> {
        let session = Self {
            settings,
            owner: ownership != Ownership::None,
            read_only: ownership == Ownership::None,
            worker: RefCell::new(None),
            previous: Cell::new(None),
            statements: RefCell::new(HashMap::new()),
            transaction: Cell::new(false),
        };
        let mut worker = Arc::new(connect(&session.settings)?);
        session.configure(&worker)?;
        match ownership {
            Ownership::None => {}
            Ownership::Own { wait } => {
                let deadline = Instant::now() + wait;
                while !session.try_lock(&worker)? {
                    if Instant::now() >= deadline {
                        let holder = session
                            .holder_on(&worker)?
                            .map_or_else(|| "another session".to_owned(), |h| h.to_string());
                        return Err(Error::other(format!(
                            "already in use by {holder}; another Rift owns this database (set database.standby = true to wait for it)"
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            Ownership::Standby => {
                let mut announced = false;
                loop {
                    match session.try_lock(&worker) {
                        Ok(true) => break,
                        Ok(false) => {}
                        Err(error) => {
                            // A standby may wait for days; ride out database
                            // restarts instead of giving up.
                            crate::log_warn!(
                                "standing by: database {}: {error}; reconnecting",
                                session.settings.label
                            );
                            std::thread::sleep(Duration::from_secs(5));
                            match connect(&session.settings) {
                                Ok(next) => {
                                    worker = Arc::new(next);
                                    session.configure(&worker)?;
                                }
                                Err(error) => crate::log_warn!(
                                    "standing by: database {}: {error}",
                                    session.settings.label
                                ),
                            }
                            continue;
                        }
                    }
                    if !announced {
                        let holder = session
                            .holder_on(&worker)?
                            .map_or_else(|| "another session".to_owned(), |h| h.to_string());
                        crate::log_info!(
                            "standing by: database {} is owned by {holder}; starting once it is released",
                            session.settings.label
                        );
                        announced = true;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                if announced {
                    crate::log_info!("took over database {}", session.settings.label);
                }
            }
        }
        *session.worker.borrow_mut() = Some(worker);
        Ok(session)
    }

    /// Send `op` to `worker` and wait for the answer; a lost connection is
    /// forgotten, so the next statement reconnects.
    fn on(&self, worker: &Worker, op: Op) -> Result<Outcome> {
        let lost = || Error::other("the database connection was lost");
        let (reply, answer) = mpsc::sync_channel(1);
        if worker.requests.send(Request { op, reply }).is_err() {
            self.forget();
            return Err(lost());
        }
        // The worker gives up after STATEMENT_TIMEOUT itself; this is a
        // backstop for a thread that stopped answering altogether.
        match answer.recv_timeout(STATEMENT_TIMEOUT + Duration::from_secs(5)) {
            Ok(Err(Error::Mariadb(error))) if error.is_fatal() => {
                self.forget();
                Err(Error::Mariadb(error))
            }
            Ok(Err(Error::Other(message))) => {
                // Only the worker's timeout produces these; it has closed
                // the connection.
                self.forget();
                Err(Error::Other(message))
            }
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.forget();
                Err(Error::other(format!(
                    "no answer from the database within {} s",
                    STATEMENT_TIMEOUT.as_secs()
                )))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.forget();
                Err(lost())
            }
        }
    }

    fn query_on(&self, worker: &Worker, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let translated = translate(sql)?;
        let params = bind(&translated, params)?;
        Ok(self
            .on(
                worker,
                Op::Run {
                    sql: translated.sql,
                    params,
                },
            )?
            .rows)
    }

    fn batch_on(&self, worker: &Worker, sql: String) -> Result<()> {
        self.on(worker, Op::Batch(sql)).map(|_| ())
    }

    fn configure(&self, worker: &Worker) -> Result<()> {
        let version: String = first(self.query_on(worker, "SELECT VERSION()", &[])?)?.get(0)?;
        supported(&version)?;
        if let Some(schema) = &self.settings.schema {
            // CREATE DATABASE IF NOT EXISTS needs the CREATE privilege even
            // when the database exists, so look first.
            let found: i64 = first(self.query_on(
                worker,
                "SELECT count(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?1",
                &[Value::Text(schema.clone())],
            )?)?
            .get(0)?;
            let schema = if found > 0 {
                schema.as_str()
            } else if self.read_only {
                // A check cannot create it. information_schema has no Rift
                // tables either, so the check sees what startup would create
                // the database to be: empty.
                "information_schema"
            } else {
                self.batch_on(
                    worker,
                    format!(
                        "CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_nopad_bin",
                        identifier(schema)
                    ),
                )?;
                schema.as_str()
            };
            self.batch_on(worker, format!("USE {}", identifier(schema)))?;
        }
        let database: Option<String> =
            first(self.query_on(worker, "SELECT DATABASE()", &[])?)?.get(0)?;
        if database.is_none() {
            return Err(Error::other(
                "no database selected; name it in the URL (mariadb://host/<database>) or set database.schema",
            ));
        }
        if self.read_only {
            self.batch_on(worker, "SET SESSION TRANSACTION READ ONLY".into())?;
        }
        Ok(())
    }

    fn try_lock(&self, worker: &Worker) -> Result<bool> {
        let taken: Option<i64> =
            first(self.query_on(worker, &format!("SELECT GET_LOCK({LOCK_NAME}, 0)"), &[])?)?
                .get(0)?;
        taken
            .map(|taken| taken == 1)
            .ok_or_else(|| Error::other("cannot take the database lock"))
    }

    fn holder_on(&self, worker: &Worker) -> Result<Option<Holder>> {
        let id: Option<i64> =
            first(self.query_on(worker, &format!("SELECT IS_USED_LOCK({LOCK_NAME})"), &[])?)?
                .get(0)?;
        let Some(id) = id.filter(|id| *id != i64::from(worker.id)) else {
            return Ok(None);
        };
        // Other accounts' connections are visible only with PROCESS.
        let account = self
            .query_on(
                worker,
                "SELECT USER, HOST FROM information_schema.PROCESSLIST WHERE ID = ?1",
                &[Value::Integer(id)],
            )?
            .first()
            .map(|row| Ok::<_, Error>((row.get(0)?, row.get(1)?)))
            .transpose()?;
        Ok(Some(Holder { id, account }))
    }

    /// The connection holding this database's lock other than this one.
    pub(crate) fn holder(&self) -> Result<Option<Holder>> {
        let worker = self.worker()?;
        self.holder_on(&worker)
    }

    /// After a reconnect: take the lock back. This Rift's previous
    /// connection the server has not noticed is gone yet is killed; a lock
    /// held by anyone else means another Rift has taken over.
    fn reclaim(&self, worker: &Worker) -> Result<()> {
        let deadline = Instant::now() + RECLAIM_WAIT;
        loop {
            if self.try_lock(worker)? {
                return Ok(());
            }
            match self.holder_on(worker)? {
                Some(holder) if self.previous.get().map(i64::from) == Some(holder.id) => {
                    crate::log_warn!(
                        "ending this Rift's previous database connection ({}) to take its lock back",
                        holder.id
                    );
                    let _ = self.batch_on(worker, format!("KILL CONNECTION {}", holder.id));
                }
                Some(holder) => {
                    crate::log_error!(
                        "database {} is now owned by {holder}: another Rift took over while the connection was down; exiting so two Rifts never write at once",
                        self.settings.label
                    );
                    std::process::exit(1);
                }
                None => {}
            }
            if Instant::now() >= deadline {
                return Err(Error::other(
                    "cannot take the database lock back after reconnecting",
                ));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    /// The live connection, reconnecting (and reclaiming the lock) when it
    /// was lost outside a transaction.
    fn worker(&self) -> Result<Arc<Worker>> {
        let current = self.worker.borrow().clone();
        if let Some(worker) = current {
            if !worker.requests.is_closed() {
                return Ok(worker);
            }
            self.forget();
        }
        if self.transaction.get() {
            return Err(Error::other("the database connection was lost"));
        }
        crate::log_warn!("reconnecting to database {}", self.settings.label);
        let worker = Arc::new(connect(&self.settings)?);
        self.configure(&worker)?;
        if self.owner {
            self.reclaim(&worker)?;
        }
        *self.worker.borrow_mut() = Some(worker.clone());
        Ok(worker)
    }

    /// Drop the connection; the next statement reconnects.
    fn forget(&self) {
        if let Some(worker) = self.worker.borrow_mut().take() {
            self.previous.set(Some(worker.id));
        }
    }

    fn translated(&self, sql: &str) -> Result<Arc<Translated>> {
        if let Some(translated) = self.statements.borrow().get(sql) {
            return Ok(translated.clone());
        }
        let translated = Arc::new(translate(sql)?);
        let mut statements = self.statements.borrow_mut();
        if statements.len() >= MAX_STATEMENTS {
            statements.clear();
        }
        statements.insert(sql.to_owned(), translated.clone());
        Ok(translated)
    }

    fn run(&self, sql: &str, params: &[Value]) -> Result<(Outcome, bool)> {
        let translated = self.translated(sql)?;
        let params = bind(&translated, params)?;
        let worker = self.worker()?;
        let outcome = self.on(
            &worker,
            Op::Run {
                sql: translated.sql.clone(),
                params,
            },
        )?;
        Ok((outcome, translated.upsert))
    }

    pub(crate) fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        let (outcome, upsert) = self.run(sql, params)?;
        // An upsert reports 2 for an updated row; SQLite and PostgreSQL 1.
        let affected = if upsert {
            outcome.affected.min(1)
        } else {
            outcome.affected
        };
        Ok(usize::try_from(affected).unwrap_or(usize::MAX))
    }

    pub(crate) fn execute_batch(&self, sql: &str) -> Result<()> {
        let worker = self.worker()?;
        self.batch_on(&worker, sql.to_owned())
    }

    /// Feed the rows of a query into `each` until it returns `false`.
    pub(crate) fn each<E: From<Error>>(
        &self,
        sql: &str,
        params: &[Value],
        mut each: impl FnMut(Row) -> std::result::Result<bool, E>,
    ) -> std::result::Result<(), E> {
        let (outcome, _) = self.run(sql, params)?;
        for row in outcome.rows {
            if !each(row)? {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn begin(&self) -> Result<()> {
        self.execute_batch("BEGIN")?;
        self.transaction.set(true);
        Ok(())
    }

    pub(crate) fn finish(&self, commit: bool) -> Result<()> {
        let result = self.execute_batch(if commit { "COMMIT" } else { "ROLLBACK" });
        if !commit || result.is_ok() {
            self.transaction.set(false);
        }
        result
    }
}

/// The parameters of `translated` in placeholder order.
fn bind(translated: &Translated, params: &[Value]) -> Result<Vec<mysql_async::Value>> {
    translated
        .order
        .iter()
        .map(|index| {
            params
                .get(*index)
                .map(to_mysql)
                .ok_or_else(|| Error::other(format!("no value for parameter ?{}", index + 1)))
        })
        .collect()
}

fn first(rows: Vec<Row>) -> Result<Row> {
    rows.into_iter().next().ok_or(Error::NoRows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql(statement: &str) -> String {
        translate(statement).unwrap().sql
    }

    #[test]
    fn placeholders_become_positional_with_their_order() {
        let translated = translate(
            "SELECT a FROM t WHERE (?1 IS NULL OR b = ?1) AND c LIKE ?2 ESCAPE '\\' AND d = '?' -- ?\n AND e = ?",
        )
        .unwrap();
        assert_eq!(
            translated.sql,
            "SELECT a FROM t WHERE (? IS NULL OR b = ?) AND c LIKE ? ESCAPE '\\' AND d = '?' -- ?\n AND e = ?"
        );
        assert_eq!(translated.order, [0, 0, 1, 0]);
        assert!(translate("SELECT ?0").is_err());
    }

    #[test]
    fn reserved_column_names_are_quoted() {
        assert_eq!(
            sql("SELECT key, n.key, \"key\" FROM perm_meta WHERE key = ?1"),
            "SELECT \"key\", n.key, \"key\" FROM perm_meta WHERE \"key\" = ?"
        );
        assert_eq!(
            sql("INSERT INTO perm_track (name, groups) VALUES (?1, ?2)"),
            "INSERT INTO perm_track (name, \"groups\") VALUES (?, ?)"
        );
        assert_eq!(
            sql("CREATE TABLE t (k TEXT, PRIMARY KEY (k))"),
            "CREATE TABLE t (k TEXT, PRIMARY KEY (k))"
        );
    }

    #[test]
    fn upserts_become_on_duplicate_key_update() {
        let translated = translate(
            "INSERT INTO perm_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .unwrap();
        assert_eq!(
            translated.sql,
            "INSERT INTO perm_meta (\"key\", value) VALUES (?, ?)
             ON DUPLICATE KEY UPDATE value = VALUES(\"value\")"
        );
        assert!(translated.upsert);
        assert_eq!(
            sql("INSERT INTO g (name,generation) VALUES (?1,1)
                 ON CONFLICT (name) DO UPDATE SET generation=g.generation+1"),
            "INSERT INTO g (name,generation) VALUES (?,1)
                 ON DUPLICATE KEY UPDATE generation=g.generation+1"
        );
        let ignored =
            translate("INSERT INTO s (kind, key) VALUES (?1, ?2)\n ON CONFLICT DO NOTHING")
                .unwrap();
        assert_eq!(
            ignored.sql.trim_end(),
            "INSERT IGNORE INTO s (kind, \"key\") VALUES (?, ?)"
        );
        assert!(!ignored.upsert);
        // Joins keep their ON.
        assert_eq!(
            sql("SELECT 1 FROM a JOIN b ON b.id = a.id"),
            "SELECT 1 FROM a JOIN b ON b.id = a.id"
        );
    }

    #[test]
    fn versions_are_checked() {
        assert!(supported("10.11.6-MariaDB-0+deb12u1").is_ok());
        assert!(supported("11.4.2-MariaDB").is_ok());
        assert!(supported("10.4.30-MariaDB").is_err());
        assert!(supported("8.4.0").is_err());
    }

    #[test]
    fn decimals_become_integers_when_whole() {
        assert_eq!(number(0, b"42").unwrap(), Value::Integer(42));
        assert_eq!(number(0, b"42.000").unwrap(), Value::Integer(42));
        assert_eq!(number(0, b"1.5").unwrap(), Value::Real(1.5));
    }

    #[test]
    fn urls_parse_and_label_without_the_password() {
        let opts = options("mariadb://rift:secret@db.internal:3307/state").unwrap();
        assert_eq!(label(&opts), "mariadb://rift@db.internal:3307/state");
        assert!(options("postgres://db/rift").is_err());
        assert!(options("mariadb://db/rift?sslmode=require").is_err());
    }
}
