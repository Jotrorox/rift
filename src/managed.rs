//! Local process ownership for managed backends. Workers own child processes,
//! so cancelling a login or an administrator request cannot abandon a start.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::OpenOptions,
    io,
    net::SocketAddr,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    process::{Child, Command},
    sync::{Notify, mpsc, oneshot, watch},
};

use crate::{
    config::{Config, ManagedServer},
    players::PlayerRegistry,
};

/// Observable lifecycle state of a supervised process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq<&str> for Phase {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[derive(Debug, Clone)]
pub struct ManagedServerSnapshot {
    pub name: String,
    pub state: Phase,
    pub pid: Option<u32>,
    pub players: usize,
    pub reservations: usize,
    pub automatic_start: bool,
    pub automatic_enabled: bool,
    pub last_error: Option<String>,
    pub restart_attempts: usize,
    pub restart_exhausted: bool,
}

/// Child stdout and stderr, appended in the server's working directory.
const LOG_FILE: &str = "rift-server.log";

#[derive(Debug, Clone)]
pub struct ManagedServers(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    servers: Mutex<BTreeMap<String, Arc<Server>>>,
    retired: Mutex<BTreeSet<String>>,
    players: Arc<PlayerRegistry>,
    launched: AtomicBool,
    shutdown: AtomicBool,
    instance_store: Mutex<Option<Arc<crate::instance_store::Store>>>,
}

#[derive(Debug)]
struct Server {
    name: String,
    config: ManagedServer,
    address: Option<SocketAddr>,
    state: Mutex<State>,
    sender: mpsc::Sender<Request>,
    receiver: Mutex<Option<mpsc::Receiver<Request>>>,
    shutdown: AtomicBool,
    wake: Notify,
}

#[derive(Debug)]
struct State {
    phase: Phase,
    pid: Option<u32>,
    reservations: usize,
    automatic: bool,
    pending_stop: bool,
    last_error: Option<String>,
    retry_at: Option<Instant>,
    restart_attempts: usize,
    recover: bool,
    last_activity: Instant,
    startup: Option<Arc<Startup>>,
}

/// One worker-owned result, retained for every caller even if a later start
/// begins before an earlier caller wakes up. No caller owns the startup task.
#[derive(Debug)]
struct Startup {
    result: watch::Sender<Option<Arc<io::Result<()>>>>,
}

impl Startup {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            result: watch::channel(None).0,
        })
    }

    async fn wait(&self, server: &Server) -> io::Result<()> {
        let mut receive = self.result.subscribe();
        loop {
            if let Some(result) = receive.borrow().as_ref() {
                return copy_result(result);
            }
            tokio::select! {
                changed = receive.changed() => {
                    changed.map_err(|_| unavailable("managed server worker stopped"))?;
                }
                _ = server.sender.closed() => {
                    return match receive.borrow().as_ref() {
                        Some(result) => copy_result(result),
                        None => Err(unavailable("managed server worker stopped")),
                    };
                }
            }
        }
    }
}

fn copy_result(result: &io::Result<()>) -> io::Result<()> {
    result.as_ref().map(|_| ()).map_err(|error| {
        if let Some(code) = error.raw_os_error() {
            io::Error::from_raw_os_error(code)
        } else {
            io::Error::new(error.kind(), error.to_string())
        }
    })
}

#[derive(Debug, Clone)]
enum Action {
    Start { mode: StartMode },
    Demand(Arc<Startup>),
    Stop,
    Console(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartMode {
    Manual,
    Demand,
    Scale,
    Recovery,
}

#[derive(Debug)]
struct Request {
    action: Action,
    reply: Option<oneshot::Sender<io::Result<()>>>,
}

/// Hold through backend establishment until player presence is committed.
/// Dropping a failed or cancelled connection releases its idle protection.
#[derive(Debug)]
pub struct ManagedLease {
    server: Arc<Server>,
}

impl Drop for ManagedLease {
    fn drop(&mut self) {
        let mut state = lock(&self.server.state);
        state.reservations -= 1;
        state.last_activity = Instant::now();
    }
}

impl ManagedServers {
    /// Construction is side-effect free and does not need a Tokio runtime.
    pub fn new(config: &Config, players: Arc<PlayerRegistry>) -> Self {
        let servers = config
            .managed_servers
            .iter()
            .map(|(name, definition)| {
                let address = config
                    .backends
                    .get(name)
                    .and_then(|backend| backend.address().parse().ok());
                (name.clone(), new_server(name, definition.clone(), address))
            })
            .collect();
        Self(Arc::new(Inner {
            servers: Mutex::new(servers),
            retired: Mutex::new(BTreeSet::new()),
            players,
            launched: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            instance_store: Mutex::new(None),
        }))
    }

    /// Attach restored stop intent before launching any autostart workers.
    pub fn with_persistence(&self, store: Arc<crate::instance_store::Store>) -> io::Result<()> {
        let servers = lock(&self.0.servers);
        if self.0.launched.load(Ordering::Acquire) {
            return Err(unavailable(
                "attach instance persistence before launching workers",
            ));
        }
        let stopped = store.stopped_names()?;
        for (name, server) in servers.iter() {
            lock(&server.state).automatic = !stopped.contains(name);
        }
        *lock(&self.0.instance_store) = Some(store);
        Ok(())
    }

    /// Call once after startup validation and listener binding have succeeded.
    /// Subsequent calls do not repeat autostart or create additional workers.
    pub fn launch(&self) -> io::Result<()> {
        let servers = lock(&self.0.servers);
        if self.0.shutdown.load(Ordering::Acquire) {
            return Err(unavailable("managed servers are shutting down"));
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        if self.0.launched.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        for server in servers.values() {
            launch_worker(&runtime, server, &self.0.players);
        }
        Ok(())
    }

    /// Register a configured backend and give its child process a worker owner.
    /// The registry lock serializes registration against launch and shutdown.
    pub fn add(&self, config: &Config, name: &str) -> io::Result<()> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let mut servers = lock(&self.0.servers);
        self.check_running()?;
        if servers.contains_key(name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "managed server already exists",
            ));
        }
        let definition = config.managed_servers.get(name).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "unknown managed server definition")
        })?;
        let address = loopback_address(
            config
                .backends
                .get(name)
                .and_then(|backend| backend.address().parse().ok()),
        )?;
        if definition.command.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed server command is empty",
            ));
        }
        let server = new_server(name, definition.clone(), Some(address));
        launch_worker(&runtime, &server, &self.0.players);
        servers.insert(name.to_owned(), server);
        lock(&self.0.retired).remove(name);
        Ok(())
    }

    /// Retire only an unoccupied backend. A detached owner completes cleanup
    /// even if the administrator disconnects while its child is terminating.
    pub async fn remove(&self, name: &str) -> io::Result<()> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let (reply, receive) = oneshot::channel();
        {
            let servers = lock(&self.0.servers);
            self.check_running()?;
            let server = servers
                .get(name)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown managed server"))?
                .clone();
            let mut state = lock(&server.state);
            if server.shutdown.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "managed server is already retiring",
                ));
            }
            if state.reservations > 0 || self.0.players.count_on_server(name) > 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "managed server has connected players or pending connections",
                ));
            }
            if let Some(store) = lock(&self.0.instance_store).as_ref() {
                store.begin_removal(name)?;
            }
            state.automatic = false;
            state.pending_stop = true;
            server.shutdown.store(true, Ordering::Release);
            server.wake.notify_one();
            drop(state);
            let inner = self.0.clone();
            let name = name.to_owned();
            runtime.spawn(async move {
                // Closing the channel happens only after process termination
                // and reaping, including a start currently awaiting readiness.
                server.sender.closed().await;
                let mut servers = lock(&inner.servers);
                if servers
                    .get(&name)
                    .is_some_and(|current| Arc::ptr_eq(current, &server))
                {
                    lock(&inner.retired).insert(name.clone());
                    servers.remove(&name);
                }
                let _ = reply.send(());
            });
        }
        receive
            .await
            .map_err(|_| unavailable("managed server removal worker stopped"))
    }

    fn is_retired(&self, name: &str) -> bool {
        lock(&self.0.retired).contains(name)
    }

    pub fn is_managed(&self, name: &str) -> bool {
        let servers = lock(&self.0.servers);
        servers.contains_key(name) || self.is_retired(name)
    }

    pub fn can_connect(&self, name: &str) -> bool {
        let servers = lock(&self.0.servers);
        let Some(server) = servers.get(name) else {
            return !self.is_retired(name);
        };
        if !self.0.launched.load(Ordering::Acquire) || self.0.shutdown.load(Ordering::Acquire) {
            return false;
        }
        let state = lock(&server.state);
        connect_allowed(server, &state).is_ok()
    }

    pub fn reserve(&self, name: &str) -> io::Result<Option<ManagedLease>> {
        let servers = lock(&self.0.servers);
        let Some(server) = servers.get(name) else {
            if self.is_retired(name) {
                return Err(unavailable("managed server was removed"));
            }
            return Ok(None);
        };
        self.check_running()?;
        let mut state = lock(&server.state);
        connect_allowed(server, &state)?;
        state.reservations += 1;
        state.last_activity = Instant::now();
        Ok(Some(ManagedLease {
            server: server.clone(),
        }))
    }

    pub async fn ensure_running(&self, name: &str) -> io::Result<()> {
        let (startup, server) = {
            let servers = lock(&self.0.servers);
            let Some(server) = servers.get(name) else {
                if self.is_retired(name) {
                    return Err(unavailable("managed server was removed"));
                }
                return Ok(());
            };
            self.check_running()?;
            let mut state = lock(&server.state);
            connect_allowed(server, &state)?;
            let startup = if let Some(startup) = &state.startup {
                startup.clone()
            } else {
                let startup = Startup::new();
                // Only the first caller queues work. Publish under the same
                // lock as admission so concurrent logins join this result.
                send_request(
                    server,
                    Request {
                        action: Action::Demand(startup.clone()),
                        reply: None,
                    },
                )?;
                state.startup = Some(startup.clone());
                startup
            };
            (startup, server.clone())
        };
        startup.wait(&server).await
    }

    pub async fn start(&self, name: &str) -> io::Result<()> {
        self.request(
            name,
            Action::Start {
                mode: StartMode::Manual,
            },
        )
        .await
    }

    pub async fn stop(&self, name: &str) -> io::Result<()> {
        self.request(name, Action::Stop).await
    }

    /// Admit an operation without waiting for process readiness or termination.
    /// Failures after admission are visible in `snapshot().last_error`.
    pub fn request_start(&self, name: &str) -> io::Result<()> {
        self.enqueue(
            name,
            Action::Start {
                mode: StartMode::Manual,
            },
            None,
        )
    }

    /// Request capacity without overriding an administrator stop or resetting
    /// the bounded recovery budget. Admission does not wait for readiness.
    pub fn request_scale_start(&self, name: &str) -> io::Result<()> {
        self.enqueue(
            name,
            Action::Start {
                mode: StartMode::Scale,
            },
            None,
        )
    }

    pub fn request_stop(&self, name: &str) -> io::Result<()> {
        self.enqueue(name, Action::Stop, None)
    }

    /// Send exactly one server command through the owned child's stdin.
    pub async fn console(&self, name: &str, command: String) -> io::Result<()> {
        if command.trim().is_empty()
            || command.len() > 4096
            || command.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "console command must contain 1..=4096 bytes without control characters",
            ));
        }
        self.request(name, Action::Console(command)).await
    }

    /// Bounded incremental reads from the same file used by stdout/stderr.
    /// An absent cursor tails the file; a cursor past EOF reports truncation.
    pub async fn logs(&self, name: &str, cursor: Option<u64>) -> io::Result<serde_json::Value> {
        let path = {
            let servers = lock(&self.0.servers);
            let server = servers
                .get(name)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown managed server"))?;
            server.config.directory.join(LOG_FILE)
        };
        tokio::task::spawn_blocking(move || read_log(&path, cursor))
            .await
            .map_err(io::Error::other)?
    }

    async fn request(&self, name: &str, action: Action) -> io::Result<()> {
        let (reply, receive) = oneshot::channel();
        self.enqueue(name, action, Some(reply))?;
        receive
            .await
            .map_err(|_| unavailable("managed server worker stopped"))?
    }

    fn check_running(&self) -> io::Result<()> {
        if !self.0.launched.load(Ordering::Acquire) {
            return Err(unavailable("managed servers have not been launched"));
        }
        if self.0.shutdown.load(Ordering::Acquire) {
            return Err(unavailable("managed servers are shutting down"));
        }
        Ok(())
    }

    fn enqueue(
        &self,
        name: &str,
        action: Action,
        reply: Option<oneshot::Sender<io::Result<()>>>,
    ) -> io::Result<()> {
        self.check_running()?;
        let servers = lock(&self.0.servers);
        // Recheck under the same registry lock used by shutdown and removal.
        self.check_running()?;
        let server = servers
            .get(name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown managed server"))?;
        let mut state = lock(&server.state);
        if server.shutdown.load(Ordering::Acquire) {
            return Err(unavailable("managed server is retiring or shutting down"));
        }
        if matches!(action, Action::Console(_))
            && (state.phase != Phase::Running || state.pending_stop)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "managed server is not running",
            ));
        }
        let stopping = matches!(action, Action::Stop);
        if matches!(
            action,
            Action::Start {
                mode: StartMode::Scale
            }
        ) {
            automatic_start_allowed(server, &state)?;
        }
        if stopping {
            if state.reservations > 0 || self.0.players.count_on_server(name) > 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "managed server has connected players or pending connections",
                ));
            }
            if state.pending_stop {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "managed server is already stopping",
                ));
            }
        }
        // Reserve a queue slot first; a rejected command must never change the
        // durable intent. Write intent before the worker can execute it.
        let permit = server
            .sender
            .try_reserve()
            .map_err(|e| unavailable(&e.to_string()))?;
        if (stopping
            || matches!(
                action,
                Action::Start {
                    mode: StartMode::Manual
                }
            ))
            && let Some(store) = lock(&self.0.instance_store).as_ref()
        {
            store.set_explicit_stop(name, stopping)?;
        }
        permit.send(Request { action, reply });
        if stopping {
            state.automatic = false;
            state.pending_stop = true;
            state.recover = false;
            state.retry_at = None;
            server.wake.notify_one();
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ManagedServerSnapshot> {
        // Read presence once so transfers cannot be counted on both backends
        // (or neither) while this snapshot visits individual server states.
        let counts = self.0.players.server_counts();
        lock(&self.0.servers)
            .values()
            .map(|server| {
                let state = lock(&server.state);
                ManagedServerSnapshot {
                    name: server.name.clone(),
                    state: if state.pending_stop {
                        Phase::Stopping
                    } else {
                        state.phase
                    },
                    pid: state.pid,
                    players: counts.get(&server.name).copied().unwrap_or(0),
                    reservations: state.reservations,
                    automatic_start: state.automatic && server.config.start_on_connect,
                    automatic_enabled: state.automatic,
                    last_error: state.last_error.clone(),
                    restart_attempts: state.restart_attempts,
                    restart_exhausted: restart_exhausted(server, &state),
                }
            })
            .collect()
    }

    /// The proxy must first drain its sessions. Shutdown then forcibly stops all
    /// processes Rift owns, including ones whose sessions exceeded that deadline.
    pub async fn shutdown(&self) {
        let servers: Vec<_> = {
            let servers = lock(&self.0.servers);
            self.0.shutdown.store(true, Ordering::Release);
            for server in servers.values() {
                server.shutdown.store(true, Ordering::Release);
                server.wake.notify_one();
            }
            servers.values().cloned().collect()
        };
        if !self.0.launched.load(Ordering::Acquire) {
            return;
        }
        // All workers stop concurrently, including ones already retiring.
        for server in servers {
            server.sender.closed().await;
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        for server in self
            .servers
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            server.shutdown.store(true, Ordering::Release);
            server.wake.notify_one();
        }
    }
}

fn new_server(name: &str, config: ManagedServer, address: Option<SocketAddr>) -> Arc<Server> {
    let (sender, receiver) = mpsc::channel(64);
    Arc::new(Server {
        name: name.to_owned(),
        config,
        address,
        sender,
        receiver: Mutex::new(Some(receiver)),
        shutdown: AtomicBool::new(false),
        wake: Notify::new(),
        state: Mutex::new(State {
            phase: Phase::Stopped,
            pid: None,
            reservations: 0,
            automatic: true,
            pending_stop: false,
            last_error: None,
            retry_at: None,
            restart_attempts: 0,
            recover: false,
            last_activity: Instant::now(),
            startup: None,
        }),
    })
}

fn launch_worker(
    runtime: &tokio::runtime::Handle,
    server: &Arc<Server>,
    players: &Arc<PlayerRegistry>,
) {
    let receiver = lock(&server.receiver).take().expect("single launch");
    runtime.spawn(worker(server.clone(), players.clone(), receiver));
}

/// Process ownership must survive a panic in an unrelated lock holder.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn read_log(path: &Path, cursor: Option<u64>) -> io::Result<serde_json::Value> {
    use std::io::{Read, Seek, SeekFrom};
    const LIMIT: u64 = 64 * 1024;
    let missing =
        || serde_json::json!({"text": "", "cursor": 0, "truncated": cursor.is_some_and(|c| c > 0)});
    // Reject symlinks and special files planted in a server directory.
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(io::Error::other("server log must be a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(missing()),
        Err(error) => return Err(error),
    }
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(missing()),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("server log must be a regular file"));
    }
    let size = metadata.len();
    let start = cursor
        .filter(|cursor| *cursor <= size)
        .unwrap_or_else(|| size.saturating_sub(LIMIT));
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(LIMIT).read_to_end(&mut bytes)?;
    let end = start + bytes.len() as u64;
    Ok(serde_json::json!({
        "text": String::from_utf8_lossy(&bytes),
        "cursor": end,
        "truncated": cursor.is_some_and(|cursor| cursor > size),
        "has_more": end < size,
    }))
}

async fn write_console(child: &mut Option<Child>, command: &str) -> io::Result<()> {
    let stdin = child
        .as_mut()
        .and_then(|process| process.stdin.as_mut())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "managed server console is unavailable",
            )
        })?;
    let write = async {
        stdin.write_all(command.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    };
    tokio::time::timeout(Duration::from_secs(2), write)
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "server console write timed out",
            ))
        })
}

/// Rift only supervises processes listening on a local, explicitly numbered port.
fn loopback_address(address: Option<SocketAddr>) -> io::Result<SocketAddr> {
    address
        .filter(|address| address.ip().to_canonical().is_loopback() && address.port() != 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed server requires a literal loopback backend address and nonzero port",
            )
        })
}

fn unavailable(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, message)
}

fn send_request(server: &Server, request: Request) -> io::Result<()> {
    server
        .sender
        .try_send(request)
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => io::Error::new(
                io::ErrorKind::WouldBlock,
                "managed server operation queue is full",
            ),
            mpsc::error::TrySendError::Closed(_) => unavailable("managed server worker stopped"),
        })
}

fn connect_allowed(server: &Server, state: &State) -> io::Result<()> {
    if server.shutdown.load(Ordering::Acquire)
        || state.pending_stop
        || state.phase == Phase::Stopping
        || !state.automatic
    {
        return Err(unavailable(
            "managed server is stopped by an administrator or shutting down",
        ));
    }
    if state.phase == Phase::Failed && state.recover {
        return Err(recovery_pending(server, state));
    }
    if matches!(state.phase, Phase::Running | Phase::Starting) {
        return Ok(());
    }
    if !server.config.start_on_connect {
        return Err(unavailable("managed server requires an explicit start"));
    }
    if state
        .retry_at
        .is_some_and(|deadline| Instant::now() < deadline)
    {
        return Err(unavailable(
            "managed server is cooling down after a failed start",
        ));
    }
    Ok(())
}

fn recovery_pending(server: &Server, state: &State) -> io::Error {
    unavailable(if restart_exhausted(server, state) {
        "managed server exhausted its restart retries; an explicit start is required"
    } else {
        "managed server is awaiting automatic recovery"
    })
}

fn restart_exhausted(server: &Server, state: &State) -> bool {
    state.phase == Phase::Failed
        && state.recover
        && state.restart_attempts >= server.config.restart_retries
}

fn automatic_start_allowed(server: &Server, state: &State) -> io::Result<()> {
    if server.shutdown.load(Ordering::Acquire) || state.pending_stop || !state.automatic {
        return Err(unavailable(
            "managed server is stopped by an administrator or shutting down",
        ));
    }
    if state.phase == Phase::Failed {
        return Err(recovery_pending(server, state));
    }
    Ok(())
}

async fn worker(
    server: Arc<Server>,
    players: Arc<PlayerRegistry>,
    mut requests: mpsc::Receiver<Request>,
) {
    let mut child = None;
    if server.config.autostart && lock(&server.state).automatic {
        let _ = start_process(&server, &mut child, StartMode::Scale).await;
    }
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if server.shutdown.load(Ordering::Acquire) {
            break;
        }
        tokio::select! {
            _ = server.wake.notified() => {}
            request = requests.recv() => {
                let Some(request) = request else { break };
                let result = perform(&server, &mut child, request.action).await;
                if let Some(reply) = request.reply {
                    let _ = reply.send(result);
                }
            }
            _ = tick.tick() => supervise(&server, &players, &mut child).await,
        }
    }
    let _ = stop_process(&server, &mut child).await;
    // Reject queued callers promptly instead of executing stale operations.
    requests.close();
    while let Some(request) = requests.recv().await {
        if let Action::Demand(startup) = request.action {
            finish_startup(
                &server,
                &startup,
                Err(unavailable("managed servers are shutting down")),
            );
        }
        if let Some(reply) = request.reply {
            let _ = reply.send(Err(unavailable("managed servers are shutting down")));
        }
    }
}

async fn perform(server: &Server, child: &mut Option<Child>, action: Action) -> io::Result<()> {
    match action {
        Action::Start { mode } => start_process(server, child, mode).await,
        Action::Demand(startup) => {
            let completed = startup.result.borrow().clone();
            match completed {
                // Autostart or an earlier manual start may already have
                // satisfied this queued demand, even on failure.
                Some(result) => copy_result(&result),
                None => start_process(server, child, StartMode::Demand).await,
            }
        }
        Action::Stop => {
            let result = stop_process(server, child).await;
            lock(&server.state).pending_stop = false;
            result
        }
        Action::Console(command) => write_console(child, &command).await,
    }
}

/// Reap unexpected exits, run due recovery attempts and apply idle shutdown.
async fn supervise(server: &Server, players: &PlayerRegistry, child: &mut Option<Child>) {
    check_exit(server, child).await;
    let should_recover = {
        let state = lock(&server.state);
        state.phase == Phase::Failed
            && state.recover
            && !state.pending_stop
            && !server.shutdown.load(Ordering::Acquire)
            && state.restart_attempts < server.config.restart_retries
            && state
                .retry_at
                .is_some_and(|deadline| Instant::now() >= deadline)
    };
    if should_recover {
        let _ = start_process(server, child, StartMode::Recovery).await;
    }
    let should_stop = {
        let mut state = lock(&server.state);
        if state.reservations > 0 || players.count_on_server(&server.name) > 0 {
            state.last_activity = Instant::now();
            false
        } else if state.phase == Phase::Running
            && server
                .config
                .idle_timeout
                .is_some_and(|timeout| state.last_activity.elapsed() >= timeout)
        {
            // This lock also guards new reservations, closing the race
            // between idle detection and the first stop await.
            state.phase = Phase::Stopping;
            state.recover = false;
            state.retry_at = None;
            true
        } else {
            false
        }
    };
    if should_stop {
        let _ = stop_process(server, child).await;
    }
}

async fn check_exit(server: &Server, child: &mut Option<Child>) {
    let Some(process) = child.as_mut() else {
        return;
    };
    match process.try_wait() {
        Ok(Some(status)) => {
            *child = None;
            record_failure(server, format!("managed server exited: {status}"));
        }
        Ok(None) => {}
        Err(error) => {
            // Reap an uninspectable child before replacement so two processes
            // can never be owned for the same managed backend.
            if let Some(mut process) = child.take() {
                let _ = process.kill().await;
                let _ = process.wait().await;
            }
            record_failure(server, format!("cannot inspect managed process: {error}"));
        }
    }
}

fn record_failure(server: &Server, error: String) {
    let mut state = lock(&server.state);
    state.phase = Phase::Failed;
    state.pid = None;
    state.last_error = Some(error);
    state.retry_at = Some(Instant::now() + server.config.restart_delay);
}

async fn start_process(
    server: &Server,
    child: &mut Option<Child>,
    mode: StartMode,
) -> io::Result<()> {
    let startup = {
        let mut state = lock(&server.state);
        state.startup.get_or_insert_with(Startup::new).clone()
    };
    let result = start_process_inner(server, child, mode).await;
    finish_startup(server, &startup, copy_result(&result));
    result
}

fn finish_startup(server: &Server, startup: &Arc<Startup>, result: io::Result<()>) {
    let mut state = lock(&server.state);
    // A queued demand can outlive the start that completed it. Never replace
    // that result or clear the shared result of a newer attempt.
    if state
        .startup
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, startup))
    {
        startup.result.send_replace(Some(Arc::new(result)));
        state.startup = None;
    }
}

async fn start_process_inner(
    server: &Server,
    child: &mut Option<Child>,
    mode: StartMode,
) -> io::Result<()> {
    check_exit(server, child).await;
    {
        let mut state = lock(&server.state);
        if server.shutdown.load(Ordering::Acquire) || state.pending_stop {
            return Err(unavailable("managed server is stopping"));
        }
        match mode {
            StartMode::Demand => connect_allowed(server, &state)?,
            StartMode::Scale => automatic_start_allowed(server, &state)?,
            StartMode::Recovery
                if !state.automatic
                    || !state.recover
                    || state.restart_attempts >= server.config.restart_retries =>
            {
                return Err(unavailable(
                    "managed server automatic recovery was cancelled",
                ));
            }
            _ => {}
        }
        if state.phase == Phase::Running {
            if mode == StartMode::Manual {
                state.automatic = true;
                state.restart_attempts = 0;
            }
            return Ok(());
        }
        if state
            .retry_at
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return Err(unavailable(
                "managed server is cooling down after a failed start",
            ));
        }
        if mode == StartMode::Manual {
            state.automatic = true;
            state.restart_attempts = 0;
        }
        if mode == StartMode::Recovery {
            state.restart_attempts += 1;
        }
        state.recover = true;
        state.phase = Phase::Starting;
        state.last_error = None;
    }
    let result = spawn_ready(server, child).await;
    match result {
        Ok(()) => {
            let mut state = lock(&server.state);
            state.phase = Phase::Running;
            state.retry_at = None;
            state.last_activity = Instant::now();
            Ok(())
        }
        Err(error) => {
            if let Some(mut process) = child.take() {
                let _ = process.kill().await;
                let _ = process.wait().await;
            }
            if error.kind() == io::ErrorKind::Interrupted {
                // An intentional stop during startup is not a failed launch
                // and must not impose crash backoff on a later explicit start.
                let mut state = lock(&server.state);
                state.phase = Phase::Stopped;
                state.pid = None;
                state.last_error = None;
                state.retry_at = None;
                state.recover = false;
            } else {
                record_failure(server, error.to_string());
            }
            Err(error)
        }
    }
}

async fn spawn_ready(server: &Server, child: &mut Option<Child>) -> io::Result<()> {
    let address = loopback_address(server.address)?;
    // Probe local port ownership directly. Closed-port TCP refusal can take
    // longer than a short preflight deadline on Windows.
    match std::net::TcpListener::bind(address) {
        Ok(listener) => drop(listener),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "managed backend port is in use; Rift will not adopt an unowned process",
            ));
        }
        Err(error) => return Err(error),
    }
    let (program, arguments) = server.config.command.split_first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed server command is empty",
        )
    })?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(server.config.directory.join(LOG_FILE))?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(&server.config.directory)
        .stdin(Stdio::piped())
        .stderr(Stdio::from(log.try_clone()?))
        .stdout(Stdio::from(log))
        .kill_on_drop(true);
    let process = command.spawn()?;
    lock(&server.state).pid = process.id();
    *child = Some(process);
    let deadline = tokio::time::Instant::now() + server.config.start_timeout;
    loop {
        if server.shutdown.load(Ordering::Acquire) || lock(&server.state).pending_stop {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "managed server startup was stopped",
            ));
        }
        let process = child.as_mut().expect("owned startup process");
        if let Some(status) = process.try_wait()? {
            return Err(io::Error::other(format!(
                "managed server exited before becoming ready: {status}"
            )));
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "managed server startup timed out",
            ));
        }
        if let Ok(Ok(_)) = tokio::time::timeout(
            remaining.min(Duration::from_millis(100)),
            TcpStream::connect(address),
        )
        .await
        {
            if let Some(status) = process.try_wait()? {
                return Err(io::Error::other(format!(
                    "managed server exited during readiness: {status}"
                )));
            }
            return Ok(());
        }
        let retry = deadline.min(tokio::time::Instant::now() + Duration::from_millis(50));
        tokio::select! {
            _ = tokio::time::sleep_until(retry) => {}
            _ = server.wake.notified() => {}
        }
    }
}

async fn stop_process(server: &Server, child: &mut Option<Child>) -> io::Result<()> {
    lock(&server.state).phase = Phase::Stopping;
    let result = if let Some(mut process) = child.take() {
        let graceful = async {
            if let Some(mut stdin) = process.stdin.take() {
                let _ = stdin.write_all(b"stop\n").await;
                let _ = stdin.shutdown().await;
            }
            process.wait().await
        };
        match tokio::time::timeout(server.config.stop_timeout, graceful).await {
            Ok(result) => result.map(|_| ()),
            Err(_) => {
                let result = process.kill().await;
                // `kill` normally waits too; explicitly reap on error as well.
                let waited = process.wait().await;
                result.and(waited.map(|_| ()))
            }
        }
    } else {
        Ok(())
    };
    let mut state = lock(&server.state);
    state.phase = if result.is_ok() {
        Phase::Stopped
    } else {
        Phase::Failed
    };
    state.pid = None;
    state.pending_stop = false;
    state.recover = false;
    state.retry_at = None;
    if let Err(error) = &result {
        state.last_error = Some(error.to_string());
    }
    result
}
