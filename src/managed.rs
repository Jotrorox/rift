//! Local process ownership for managed backends. Workers own child processes,
//! so cancelling a login or an administrator request cannot abandon a start.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io,
    net::SocketAddr,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    process::{Child, Command},
    sync::{Notify, mpsc, oneshot},
};

use crate::{
    config::{Config, ManagedServer},
    players::PlayerRegistry,
};

#[derive(Debug, Clone)]
pub struct ManagedServerSnapshot {
    pub name: String,
    pub state: &'static str,
    pub pid: Option<u32>,
    pub players: usize,
    pub reservations: usize,
    pub automatic_start: bool,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ManagedServers(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    servers: Mutex<BTreeMap<String, Arc<Server>>>,
    retired: Mutex<BTreeSet<String>>,
    players: Arc<PlayerRegistry>,
    launched: AtomicBool,
    shutdown: AtomicBool,
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
    phase: &'static str,
    pid: Option<u32>,
    reservations: usize,
    automatic: bool,
    pending_stop: bool,
    last_error: Option<String>,
    retry_at: Option<Instant>,
    last_activity: Instant,
}

#[derive(Debug, Clone, Copy)]
enum Action {
    Start { manual: bool },
    Stop,
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
        let mut state = self.server.state.lock().unwrap_or_else(|e| e.into_inner());
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
        }))
    }

    /// Call once after startup validation and listener binding have succeeded.
    /// Subsequent calls do not repeat autostart or create additional workers.
    pub fn launch(&self) -> io::Result<()> {
        let servers = self.0.servers.lock().unwrap_or_else(|e| e.into_inner());
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
        let mut servers = self.0.servers.lock().unwrap_or_else(|e| e.into_inner());
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
        let address = config
            .backends
            .get(name)
            .and_then(|backend| backend.address().parse::<SocketAddr>().ok())
            .filter(|address| address.ip().to_canonical().is_loopback() && address.port() != 0)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "managed server requires a literal loopback backend address and nonzero port",
                )
            })?;
        if definition.command.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed server command is empty",
            ));
        }
        let server = new_server(name, definition.clone(), Some(address));
        launch_worker(&runtime, &server, &self.0.players);
        servers.insert(name.to_owned(), server);
        self.0
            .retired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        Ok(())
    }

    /// Retire only an unoccupied backend. A detached owner completes cleanup
    /// even if the administrator disconnects while its child is terminating.
    pub async fn remove(&self, name: &str) -> io::Result<()> {
        let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
        let (reply, receive) = oneshot::channel();
        {
            let servers = self.0.servers.lock().unwrap_or_else(|e| e.into_inner());
            self.check_running()?;
            let server = servers
                .get(name)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown managed server"))?
                .clone();
            let mut state = server.state.lock().unwrap_or_else(|e| e.into_inner());
            if server.shutdown.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "managed server is already retiring",
                ));
            }
            if state.reservations > 0 || player_count(&self.0.players, name) > 0 {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "managed server has connected players or pending connections",
                ));
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
                let mut servers = inner.servers.lock().unwrap_or_else(|e| e.into_inner());
                if servers
                    .get(&name)
                    .is_some_and(|current| Arc::ptr_eq(current, &server))
                {
                    inner
                        .retired
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(name.clone());
                    servers.remove(&name);
                }
                let _ = reply.send(());
            });
        }
        receive
            .await
            .map_err(|_| unavailable("managed server removal worker stopped"))
    }

    pub fn is_managed(&self, name: &str) -> bool {
        let servers = self.0.servers.lock().unwrap_or_else(|e| e.into_inner());
        servers.contains_key(name)
            || self
                .0
                .retired
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(name)
    }

    pub fn can_connect(&self, name: &str) -> bool {
        let servers = self.0.servers.lock().unwrap_or_else(|e| e.into_inner());
        let Some(server) = servers.get(name) else {
            return !self
                .0
                .retired
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(name);
        };
        if !self.0.launched.load(Ordering::Acquire) || self.0.shutdown.load(Ordering::Acquire) {
            return false;
        }
        let state = server.state.lock().unwrap_or_else(|e| e.into_inner());
        connect_allowed(server, &state).is_ok()
    }

    pub fn reserve(&self, name: &str) -> io::Result<Option<ManagedLease>> {
        let servers = self.0.servers.lock().unwrap_or_else(|e| e.into_inner());
        let Some(server) = servers.get(name) else {
            if self
                .0
                .retired
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(name)
            {
                return Err(unavailable("managed server was removed"));
            }
            return Ok(None);
        };
        self.check_running()?;
        let mut state = server.state.lock().unwrap_or_else(|e| e.into_inner());
        connect_allowed(server, &state)?;
        state.reservations += 1;
        state.last_activity = Instant::now();
        Ok(Some(ManagedLease {
            server: server.clone(),
        }))
    }

    pub async fn ensure_running(&self, name: &str) -> io::Result<()> {
        if !self.is_managed(name) {
            return Ok(());
        }
        self.request(name, Action::Start { manual: false }).await
    }

    pub async fn start(&self, name: &str) -> io::Result<()> {
        self.request(name, Action::Start { manual: true }).await
    }

    pub async fn stop(&self, name: &str) -> io::Result<()> {
        self.request(name, Action::Stop).await
    }

    /// Admit an operation without waiting for process readiness or termination.
    /// Failures after admission are visible in `snapshot().last_error`.
    pub fn request_start(&self, name: &str) -> io::Result<()> {
        self.enqueue(name, Action::Start { manual: true }, None)
    }

    pub fn request_stop(&self, name: &str) -> io::Result<()> {
        self.enqueue(name, Action::Stop, None)
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
        let servers = self.0.servers.lock().unwrap_or_else(|e| e.into_inner());
        // Recheck under the same registry lock used by shutdown and removal.
        self.check_running()?;
        let server = servers
            .get(name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown managed server"))?;
        let mut state = server.state.lock().unwrap_or_else(|e| e.into_inner());
        if server.shutdown.load(Ordering::Acquire) {
            return Err(unavailable("managed server is retiring or shutting down"));
        }
        if matches!(action, Action::Stop) {
            if state.reservations > 0 || player_count(&self.0.players, name) > 0 {
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
        server
            .sender
            .try_send(Request { action, reply })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "managed server operation queue is full",
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    unavailable("managed server worker stopped")
                }
            })?;
        if matches!(action, Action::Stop) {
            state.automatic = false;
            state.pending_stop = true;
            server.wake.notify_one();
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<ManagedServerSnapshot> {
        self.0
            .servers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|server| {
                let state = server.state.lock().unwrap_or_else(|e| e.into_inner());
                ManagedServerSnapshot {
                    name: server.name.clone(),
                    state: if state.pending_stop {
                        "stopping"
                    } else {
                        state.phase
                    },
                    pid: state.pid,
                    players: player_count(&self.0.players, &server.name),
                    reservations: state.reservations,
                    automatic_start: state.automatic && server.config.start_on_connect,
                    last_error: state.last_error.clone(),
                }
            })
            .collect()
    }

    /// The proxy must first drain its sessions. Shutdown then forcibly stops all
    /// processes Rift owns, including ones whose sessions exceeded that deadline.
    pub async fn shutdown(&self) {
        let servers: Vec<_> = {
            let servers = self.0.servers.lock().unwrap_or_else(|e| e.into_inner());
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
            phase: "stopped",
            pid: None,
            reservations: 0,
            automatic: true,
            pending_stop: false,
            last_error: None,
            retry_at: None,
            last_activity: Instant::now(),
        }),
    })
}

fn launch_worker(
    runtime: &tokio::runtime::Handle,
    server: &Arc<Server>,
    players: &Arc<PlayerRegistry>,
) {
    let receiver = server
        .receiver
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .expect("single launch");
    runtime.spawn(worker(server.clone(), players.clone(), receiver));
}

fn unavailable(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, message)
}

fn player_count(players: &PlayerRegistry, name: &str) -> usize {
    players
        .snapshot()
        .iter()
        .filter(|player| player.server == name)
        .count()
}

fn connect_allowed(server: &Server, state: &State) -> io::Result<()> {
    if server.shutdown.load(Ordering::Acquire)
        || state.pending_stop
        || state.phase == "stopping"
        || !state.automatic
    {
        return Err(unavailable(
            "managed server is stopped by an administrator or shutting down",
        ));
    }
    if matches!(state.phase, "running" | "starting") {
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

async fn worker(
    server: Arc<Server>,
    players: Arc<PlayerRegistry>,
    mut requests: mpsc::Receiver<Request>,
) {
    let mut child = None;
    if server.config.autostart {
        let _ = start_process(&server, &mut child, true).await;
    }
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if server.shutdown.load(Ordering::Acquire) {
            break;
        }
        tokio::select! {
            _ = server.wake.notified() => {},
            request = requests.recv() => {
                let Some(request) = request else { break; };
                let result = match request.action {
                    Action::Start { manual } => start_process(&server, &mut child, manual).await,
                    Action::Stop => {
                        let result = stop_process(&server, &mut child).await;
                        server.state.lock().unwrap_or_else(|e| e.into_inner()).pending_stop = false;
                        result
                    },
                };
                if let Some(reply) = request.reply { let _ = reply.send(result); }
            },
            _ = tick.tick() => {
                check_exit(&server, &mut child);
                let should_stop = {
                    let mut state = server.state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.reservations > 0 || player_count(&players, &server.name) > 0 {
                        state.last_activity = Instant::now();
                        false
                    } else if state.phase == "running" && server.config.idle_timeout.is_some_and(|timeout| state.last_activity.elapsed() >= timeout) {
                        // This lock also guards new reservations, closing the
                        // race between idle detection and the first stop await.
                        state.phase = "stopping";
                        true
                    } else { false }
                };
                if should_stop { let _ = stop_process(&server, &mut child).await; }
            },
        }
    }
    let _ = stop_process(&server, &mut child).await;
    // Reject queued callers promptly instead of executing stale operations.
    requests.close();
    while let Some(request) = requests.recv().await {
        if let Some(reply) = request.reply {
            let _ = reply.send(Err(unavailable("managed servers are shutting down")));
        }
    }
}

fn check_exit(server: &Server, child: &mut Option<Child>) {
    let Some(process) = child.as_mut() else {
        return;
    };
    match process.try_wait() {
        Ok(Some(status)) => {
            *child = None;
            record_failure(server, format!("managed server exited: {status}"));
        }
        Ok(None) => {}
        Err(error) => record_failure(server, format!("cannot inspect managed process: {error}")),
    }
}

fn record_failure(server: &Server, error: String) {
    let mut state = server.state.lock().unwrap_or_else(|e| e.into_inner());
    state.phase = "failed";
    state.pid = None;
    state.last_error = Some(error);
    state.retry_at = Some(Instant::now() + server.config.restart_delay);
}

async fn start_process(server: &Server, child: &mut Option<Child>, manual: bool) -> io::Result<()> {
    check_exit(server, child);
    {
        let mut state = server.state.lock().unwrap_or_else(|e| e.into_inner());
        if server.shutdown.load(Ordering::Acquire) || state.pending_stop {
            return Err(unavailable("managed server is stopping"));
        }
        if !manual {
            connect_allowed(server, &state)?;
        }
        if manual {
            state.automatic = true;
        }
        if state.phase == "running" {
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
        state.phase = "starting";
        state.last_error = None;
    }
    let result = spawn_ready(server, child).await;
    match result {
        Ok(()) => {
            let mut state = server.state.lock().unwrap_or_else(|e| e.into_inner());
            state.phase = "running";
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
                let mut state = server.state.lock().unwrap_or_else(|e| e.into_inner());
                state.phase = "stopped";
                state.pid = None;
                state.last_error = None;
                state.retry_at = None;
            } else {
                record_failure(server, error.to_string());
            }
            Err(error)
        }
    }
}

async fn spawn_ready(server: &Server, child: &mut Option<Child>) -> io::Result<()> {
    let address = server
        .address
        .filter(|address| address.ip().to_canonical().is_loopback() && address.port() != 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed server requires a literal loopback backend address and nonzero port",
            )
        })?;
    match tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(address)).await {
        Ok(Ok(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "managed backend already has a listener; Rift will not adopt an unowned process",
            ));
        }
        Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "managed backend preflight connection timed out",
            ));
        }
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
        .open(server.config.directory.join("rift-server.log"))?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(&server.config.directory)
        .stdin(Stdio::piped())
        .stderr(Stdio::from(log.try_clone()?))
        .stdout(Stdio::from(log))
        .kill_on_drop(true);
    let process = command.spawn()?;
    server.state.lock().unwrap_or_else(|e| e.into_inner()).pid = process.id();
    *child = Some(process);
    let deadline = tokio::time::Instant::now() + server.config.start_timeout;
    loop {
        if server.shutdown.load(Ordering::Acquire)
            || server
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending_stop
        {
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
        tokio::select! {
            _ = tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + Duration::from_millis(50))) => {},
            _ = server.wake.notified() => {},
        }
    }
}

async fn stop_process(server: &Server, child: &mut Option<Child>) -> io::Result<()> {
    server.state.lock().unwrap_or_else(|e| e.into_inner()).phase = "stopping";
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
    let mut state = server.state.lock().unwrap_or_else(|e| e.into_inner());
    state.phase = if result.is_ok() { "stopped" } else { "failed" };
    state.pid = None;
    state.pending_stop = false;
    if let Err(error) = &result {
        state.last_error = Some(error.to_string());
    }
    result
}
