//! The PostgreSQL backend of [`Conn`](super::Conn).
//!
//! Each connection runs on its own thread with a small Tokio runtime that
//! drives the socket; callers block on the client's futures with
//! [`block_on`], so the synchronous database API works from any thread, also
//! from inside Rift's own runtime, exactly like SQLite does.
//!
//! A Rift that owns the database holds a session-level advisory lock, the
//! counterpart of the SQLite lock file. When its connection drops, it
//! reconnects and takes the lock again; if another Rift took it meanwhile,
//! this one has been replaced and exits rather than write beside it.
use super::value::{Error, Kind, Result, Row, Value};
use bytes::BytesMut;
use rustls::pki_types::{CertificateDer, ServerName, pem::PemObject};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    future::Future,
    io,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::{
    Client, Socket, Statement,
    tls::{ChannelBinding, MakeTlsConnect, TlsConnect, TlsStream},
    types::{IsNull, ToSql, Type},
};

/// The first half of the advisory lock key, "RIFT"; the second half is a
/// hash of the current schema, so Rifts in different schemas of one database
/// do not exclude each other.
const LOCK_CLASS: i32 = 0x5249_4654;
/// How long one statement may take before the connection is given up.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Connection timeout unless the URL sets `connect_timeout`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a reconnecting owner waits for its previous session to release
/// the lock before it gives up.
const RECLAIM_WAIT: Duration = Duration::from_secs(30);
/// Prepared statements kept per connection.
const MAX_STATEMENTS: usize = 512;

/// Run `future` on this thread until it finishes or `timeout` passes.
fn block_on<F: Future>(future: F, timeout: Duration) -> Option<F::Output> {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let deadline = Instant::now() + timeout;
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Some(output);
        }
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        std::thread::park_timeout(deadline - now);
    }
}

/// Rewrite `?N` (and bare `?`) placeholders as PostgreSQL's `$N`, leaving
/// string literals, quoted identifiers and comments alone.
pub(super) fn translate(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut chars = sql.chars().peekable();
    let mut next = 1;
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                out.push(c);
                for inner in chars.by_ref() {
                    out.push(inner);
                    if inner == c {
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                out.push(c);
                for inner in chars.by_ref() {
                    out.push(inner);
                    if inner == '\n' {
                        break;
                    }
                }
            }
            '?' => {
                out.push('$');
                let mut digits = String::new();
                while let Some(digit) = chars.peek().copied().filter(char::is_ascii_digit) {
                    digits.push(digit);
                    chars.next();
                }
                if digits.is_empty() {
                    out.push_str(&next.to_string());
                    next += 1;
                } else {
                    out.push_str(&digits);
                }
            }
            c => out.push(c),
        }
    }
    out
}

fn pg_type(kind: Kind) -> Type {
    match kind {
        Kind::Integer => Type::INT8,
        Kind::Real => Type::FLOAT8,
        Kind::Text => Type::TEXT,
        Kind::Blob => Type::BYTEA,
    }
}

impl ToSql for Value {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self {
            Self::Null(_) => Ok(IsNull::Yes),
            Self::Integer(integer) => match *ty {
                Type::INT8 => integer.to_sql(ty, out),
                Type::INT4 => i32::try_from(*integer)?.to_sql(ty, out),
                Type::INT2 => i16::try_from(*integer)?.to_sql(ty, out),
                Type::BOOL => (*integer != 0).to_sql(ty, out),
                Type::FLOAT8 => (*integer as f64).to_sql(ty, out),
                _ => Err(format!("cannot bind an integer as {ty}").into()),
            },
            Self::Real(real) => match *ty {
                Type::FLOAT8 => real.to_sql(ty, out),
                Type::FLOAT4 => (*real as f32).to_sql(ty, out),
                _ => Err(format!("cannot bind a real as {ty}").into()),
            },
            Self::Text(text) => text.as_str().to_sql(ty, out),
            Self::Blob(bytes) => bytes.as_slice().to_sql(ty, out),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    tokio_postgres::types::to_sql_checked!();
}

fn row(row: &tokio_postgres::Row) -> Result<Row> {
    let mut values = Vec::with_capacity(row.len());
    for (index, column) in row.columns().iter().enumerate() {
        let ty = column.type_();
        let value = match *ty {
            Type::INT8 => row
                .try_get::<_, Option<i64>>(index)?
                .map_or(Value::Null(Kind::Integer), Value::Integer),
            Type::INT4 => row
                .try_get::<_, Option<i32>>(index)?
                .map_or(Value::Null(Kind::Integer), |v| Value::Integer(v.into())),
            Type::INT2 => row
                .try_get::<_, Option<i16>>(index)?
                .map_or(Value::Null(Kind::Integer), |v| Value::Integer(v.into())),
            Type::OID => row
                .try_get::<_, Option<u32>>(index)?
                .map_or(Value::Null(Kind::Integer), |v| Value::Integer(v.into())),
            Type::BOOL => row
                .try_get::<_, Option<bool>>(index)?
                .map_or(Value::Null(Kind::Integer), |v| Value::Integer(v.into())),
            Type::FLOAT8 => row
                .try_get::<_, Option<f64>>(index)?
                .map_or(Value::Null(Kind::Real), Value::Real),
            Type::FLOAT4 => row
                .try_get::<_, Option<f32>>(index)?
                .map_or(Value::Null(Kind::Real), |v| Value::Real(v.into())),
            Type::BYTEA => row
                .try_get::<_, Option<Vec<u8>>>(index)?
                .map_or(Value::Null(Kind::Blob), Value::Blob),
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => row
                .try_get::<_, Option<String>>(index)?
                .map_or(Value::Null(Kind::Text), Value::Text),
            _ => {
                return Err(Error::Conversion(format!(
                    "column {index} has unsupported type {ty}"
                )));
            }
        };
        values.push(value);
    }
    Ok(Row(values))
}

/// Verifies the server with the public CAs plus `database.ca`.
#[derive(Clone)]
pub(crate) struct MakeRustls(Arc<rustls::ClientConfig>);

impl MakeRustls {
    pub(crate) fn new(ca: Option<&Path>) -> io::Result<Self> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(ca) = ca {
            let certificates = CertificateDer::pem_file_iter(ca)
                .and_then(|certificates| certificates.collect::<std::result::Result<Vec<_>, _>>())
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("database.ca {}: {error}", ca.display()),
                    )
                })?;
            if certificates.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("database.ca {}: no certificates found", ca.display()),
                ));
            }
            for certificate in certificates {
                roots.add(certificate).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("database.ca {}: {error}", ca.display()),
                    )
                })?;
            }
        }
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(Self(Arc::new(config)))
    }
}

impl MakeTlsConnect<Socket> for MakeRustls {
    type Stream = RustlsStream;
    type TlsConnect = RustlsConnect;
    type Error = io::Error;

    fn make_tls_connect(&mut self, domain: &str) -> io::Result<RustlsConnect> {
        Ok(RustlsConnect {
            config: self.0.clone(),
            // Unix sockets have no name; they never use TLS.
            name: ServerName::try_from(domain.to_owned()).ok(),
        })
    }
}

pub(crate) struct RustlsConnect {
    config: Arc<rustls::ClientConfig>,
    name: Option<ServerName<'static>>,
}

impl TlsConnect<Socket> for RustlsConnect {
    type Stream = RustlsStream;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<RustlsStream>> + Send>>;

    fn connect(self, stream: Socket) -> Self::Future {
        Box::pin(async move {
            let name = self.name.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the database host is not a valid TLS server name",
                )
            })?;
            let stream = tokio_rustls::TlsConnector::from(self.config)
                .connect(name, stream)
                .await?;
            Ok(RustlsStream(stream))
        })
    }
}

pub(crate) struct RustlsStream(tokio_rustls::client::TlsStream<Socket>);

impl AsyncRead for RustlsStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for RustlsStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

impl TlsStream for RustlsStream {
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

/// Everything a connection needs, resolved from `rift.config.database`.
pub(crate) struct Settings {
    pub config: tokio_postgres::Config,
    pub tls: MakeRustls,
    /// The URL without its password, for messages.
    pub label: String,
    /// `database.schema`: created when missing and put first on the
    /// search path of every connection.
    pub schema: Option<String>,
}

/// `name` as a quoted SQL identifier.
pub(crate) fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The URL `config` connects to, without its password.
pub(crate) fn label(config: &tokio_postgres::Config) -> String {
    use tokio_postgres::config::Host;
    let user = config
        .get_user()
        .map(|user| format!("{user}@"))
        .unwrap_or_default();
    let ports = config.get_ports();
    let hosts: Vec<String> = config
        .get_hosts()
        .iter()
        .enumerate()
        .map(|(index, host)| {
            let host = match host {
                Host::Tcp(host) => host.clone(),
                #[allow(unreachable_patterns)]
                _ => "localhost".to_owned(),
            };
            match ports.get(index).or(ports.first()) {
                Some(port) => format!("{host}:{port}"),
                None => host,
            }
        })
        .collect();
    let database = config.get_dbname().unwrap_or_default();
    format!("postgres://{user}{}/{database}", hosts.join(","))
}

/// Open a connection on its own thread and return its client.
fn connect(settings: &Settings, application_name: &str) -> Result<Client> {
    let mut config = settings.config.clone();
    config.application_name(application_name);
    let timeout = config
        .get_connect_timeout()
        .copied()
        .unwrap_or(CONNECT_TIMEOUT);
    let tls = settings.tls.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("rift-postgres".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = sender.send(Err(Error::other(format!(
                        "cannot start the database thread: {error}"
                    ))));
                    return;
                }
            };
            runtime.block_on(async move {
                match tokio::time::timeout(timeout, config.connect(tls)).await {
                    Ok(Ok((client, connection))) => {
                        if sender.send(Ok(client)).is_err() {
                            return;
                        }
                        // Ends once the client is dropped, or with the error
                        // that closed the connection.
                        if let Err(error) = connection.await {
                            crate::log_warn!("PostgreSQL connection closed: {error}");
                        }
                    }
                    Ok(Err(error)) => {
                        let _ = sender.send(Err(Error::Postgres(error)));
                    }
                    Err(_) => {
                        let _ = sender.send(Err(Error::other(format!(
                            "timed out after {} s connecting",
                            timeout.as_secs()
                        ))));
                    }
                }
            });
        })
        .map_err(|error| Error::other(format!("cannot start the database thread: {error}")))?;
    receiver
        .recv()
        .map_err(|_| Error::other("the database thread stopped while connecting"))?
}

/// The SQL that names the advisory lock of the current schema.
const LOCK_KEY: &str = "1380533844, hashtext(current_schema())";
const _: () = assert!(LOCK_CLASS == 1_380_533_844);

/// The session holding this schema's lock: `application_name` and address,
/// with its backend pid.
const HOLDER: &str = "SELECT a.pid, COALESCE(a.application_name, ''),
        COALESCE(host(a.client_addr), 'a local socket')
    FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
    WHERE l.locktype = 'advisory' AND l.granted AND l.objsubid = 2
      AND l.classid = 1380533844::oid AND l.objid = hashtext(current_schema())::oid
      AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())
      AND l.pid <> pg_backend_pid()
    LIMIT 1";

/// Who holds the lock: (backend pid, application name, address).
pub(crate) struct Holder {
    pub pid: i64,
    pub application: String,
    pub address: String,
}

impl std::fmt::Display for Holder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = if self.application.is_empty() {
            "a client"
        } else {
            &self.application
        };
        write!(f, "{name} from {}", self.address)
    }
}

/// How [`Session::open`] treats a lock held elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ownership {
    /// Read-only, without the lock (`rift check --strict`).
    None,
    /// Take the lock, waiting up to `wait` for it.
    Own { wait: Duration },
    /// Wait for the lock as long as it takes (`database.standby`).
    Standby,
}

/// One connection with its statement cache.
pub(crate) struct Session {
    settings: Arc<Settings>,
    application_name: String,
    /// Holds the advisory lock, and must keep holding it across reconnects.
    owner: bool,
    read_only: bool,
    client: RefCell<Option<Arc<Client>>>,
    statements: RefCell<HashMap<String, Statement>>,
    transaction: Cell<bool>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("database", &self.settings.label)
            .field("application_name", &self.application_name)
            .finish_non_exhaustive()
    }
}

/// The `application_name` of every session of this process: unique per
/// process, so a reconnecting owner recognises its own stale session.
fn application_name() -> String {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        format!(
            "rift/{:x}-{:x}",
            std::process::id(),
            nanos & 0xffff_ffff_ffff
        )
    })
    .clone()
}

impl Session {
    pub(crate) fn open(settings: Arc<Settings>, ownership: Ownership) -> Result<Self> {
        let session = Self {
            settings,
            application_name: application_name(),
            owner: ownership != Ownership::None,
            read_only: ownership == Ownership::None,
            client: RefCell::new(None),
            statements: RefCell::new(HashMap::new()),
            transaction: Cell::new(false),
        };
        let mut client = Arc::new(connect(&session.settings, &session.application_name)?);
        session.configure(&client)?;
        match ownership {
            Ownership::None => {}
            Ownership::Own { wait } => {
                let deadline = Instant::now() + wait;
                while !session.try_lock(&client)? {
                    if Instant::now() >= deadline {
                        let holder = session
                            .holder_on(&client)?
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
                    match session.try_lock(&client) {
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
                            match connect(&session.settings, &session.application_name) {
                                Ok(next) => {
                                    client = Arc::new(next);
                                    session.configure(&client)?;
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
                            .holder_on(&client)?
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
        *session.client.borrow_mut() = Some(client);
        Ok(session)
    }

    fn configure(&self, client: &Client) -> Result<()> {
        let mut setup = String::from("SET client_min_messages = warning;");
        if let Some(schema) = &self.settings.schema {
            // CREATE SCHEMA IF NOT EXISTS needs CREATE on the database even
            // when the schema exists, so look first.
            if !self.read_only {
                let found = self.wait(
                    client,
                    client.query_one(
                        "SELECT count(*) FROM pg_namespace WHERE nspname = $1",
                        &[schema],
                    ),
                )?;
                if found.try_get::<_, i64>(0)? == 0 {
                    setup.push_str(&format!(" CREATE SCHEMA {};", identifier(schema)));
                }
            }
            setup.push_str(&format!(" SET search_path TO {};", identifier(schema)));
        }
        if self.read_only {
            setup.push_str(" SET default_transaction_read_only = on;");
        }
        self.wait(client, client.batch_execute(&setup))
    }

    fn try_lock(&self, client: &Client) -> Result<bool> {
        let row = self
            .wait(
                client,
                client.query_one(
                    format!("SELECT pg_try_advisory_lock({LOCK_KEY})").as_str(),
                    &[],
                ),
            )
            .and_then(|row| row.try_get::<_, Option<bool>>(0).map_err(Error::from))?;
        row.ok_or_else(|| {
            Error::other("PostgreSQL has no current schema; set search_path in the database URL")
        })
    }

    fn holder_on(&self, client: &Client) -> Result<Option<Holder>> {
        let rows = self.wait(client, client.query(HOLDER, &[]))?;
        let holder =
            |row: &tokio_postgres::Row| -> std::result::Result<Holder, tokio_postgres::Error> {
                Ok(Holder {
                    pid: row.try_get::<_, i32>(0)?.into(),
                    application: row.try_get(1)?,
                    address: row.try_get(2)?,
                })
            };
        Ok(rows.first().map(holder).transpose()?)
    }

    /// The session holding this schema's lock other than this one, if any.
    pub(crate) fn holder(&self) -> Result<Option<Holder>> {
        let client = self.client()?;
        self.holder_on(&client)
    }

    /// After a reconnect: take the lock back. A previous session of this
    /// process the server has not noticed is gone yet is terminated; a lock
    /// held by anyone else means another Rift has taken over.
    fn reclaim(&self, client: &Client) -> Result<()> {
        let deadline = Instant::now() + RECLAIM_WAIT;
        loop {
            if self.try_lock(client)? {
                return Ok(());
            }
            match self.holder_on(client)? {
                Some(holder) if holder.application == self.application_name => {
                    crate::log_warn!(
                        "ending this Rift's previous database session (pid {}) to take its lock back",
                        holder.pid
                    );
                    let _ = self.wait(
                        client,
                        client.execute("SELECT pg_terminate_backend($1)", &[&(holder.pid as i32)]),
                    );
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

    /// The live client, reconnecting (and reclaiming the lock) when the
    /// connection was lost outside a transaction.
    fn client(&self) -> Result<Arc<Client>> {
        let current = self.client.borrow().clone();
        if let Some(client) = current {
            if !client.is_closed() {
                return Ok(client);
            }
            self.forget();
        }
        if self.transaction.get() {
            return Err(Error::other("the database connection was lost"));
        }
        crate::log_warn!("reconnecting to database {}", self.settings.label);
        let client = Arc::new(connect(&self.settings, &self.application_name)?);
        self.configure(&client)?;
        if self.owner {
            self.reclaim(&client)?;
        }
        *self.client.borrow_mut() = Some(client.clone());
        Ok(client)
    }

    /// Drop the connection; the next statement reconnects.
    fn forget(&self) {
        self.client.borrow_mut().take();
        self.statements.borrow_mut().clear();
    }

    /// Wait for `future` of `client`; a timeout or a closed connection
    /// drops the connection.
    fn wait<T>(
        &self,
        client: &Client,
        future: impl Future<Output = std::result::Result<T, tokio_postgres::Error>>,
    ) -> Result<T> {
        match block_on(future, STATEMENT_TIMEOUT) {
            Some(Ok(value)) => Ok(value),
            Some(Err(error)) => {
                if client.is_closed() {
                    self.forget();
                }
                Err(Error::Postgres(error))
            }
            None => {
                self.forget();
                Err(Error::other(format!(
                    "no answer from the database within {} s",
                    STATEMENT_TIMEOUT.as_secs()
                )))
            }
        }
    }

    fn statement(&self, client: &Client, sql: &str, params: &[Value]) -> Result<Statement> {
        let types: Vec<Type> = params.iter().map(|value| pg_type(value.kind())).collect();
        let mut key: String = params
            .iter()
            .map(|value| match value.kind() {
                Kind::Integer => 'i',
                Kind::Real => 'r',
                Kind::Text => 't',
                Kind::Blob => 'b',
            })
            .collect();
        key.push('\0');
        key.push_str(sql);
        if let Some(statement) = self.statements.borrow().get(&key) {
            return Ok(statement.clone());
        }
        let statement = self.wait(client, client.prepare_typed(&translate(sql), &types))?;
        let mut statements = self.statements.borrow_mut();
        if statements.len() >= MAX_STATEMENTS {
            statements.clear();
        }
        statements.insert(key, statement.clone());
        Ok(statement)
    }

    pub(crate) fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        let client = self.client()?;
        let statement = self.statement(&client, sql, params)?;
        let count = self.wait(&client, client.execute_raw(&statement, params.iter()))?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    pub(crate) fn execute_batch(&self, sql: &str) -> Result<()> {
        let client = self.client()?;
        self.wait(&client, client.batch_execute(sql))
    }

    /// Stream the rows of a query into `each` until it returns `false`.
    pub(crate) fn each<E: From<Error>>(
        &self,
        sql: &str,
        params: &[Value],
        mut each: impl FnMut(Row) -> std::result::Result<bool, E>,
    ) -> std::result::Result<(), E> {
        use futures_util::StreamExt;
        let client = self.client()?;
        let statement = self.statement(&client, sql, params)?;
        let stream = self.wait(&client, client.query_raw(&statement, params.iter()))?;
        let mut stream = std::pin::pin!(stream);
        while let Some(next) = self.wait(&client, async {
            Ok::<_, tokio_postgres::Error>(stream.next().await)
        })? {
            let next = next.map_err(|error| {
                if client.is_closed() {
                    self.forget();
                }
                Error::Postgres(error)
            })?;
            if !each(row(&next)?)? {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_become_numbered_outside_literals() {
        assert_eq!(
            translate(
                "SELECT a FROM t WHERE b = ?1 AND c LIKE ?2 ESCAPE '\\' AND d = '?' -- ?\n AND e = ?"
            ),
            "SELECT a FROM t WHERE b = $1 AND c LIKE $2 ESCAPE '\\' AND d = '?' -- ?\n AND e = $1"
        );
        assert_eq!(translate("\"?\" = ?12"), "\"?\" = $12");
    }

    #[test]
    fn labels_leave_out_the_password() {
        let config: tokio_postgres::Config = "postgres://rift:secret@db.internal:5433/state"
            .parse()
            .unwrap();
        assert_eq!(label(&config), "postgres://rift@db.internal:5433/state");
    }
}
