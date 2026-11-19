//! Process ownership for managed backends. Workers own their processes, so
//! cancelling a login or an administrator request cannot abandon a start.
//! Processes run on this host or on a `rift node` agent; see [`backend`].
//! Local servers with `keep_running` outlive the proxy and are adopted by the
//! next one, like servers on a node.
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, io,
    net::SocketAddr,
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::{
    config::{Config, ManagedServer},
    node::{Nodes, protocol},
    players::PlayerRegistry,
    provisioning::Fresh,
};

mod backend;
#[cfg(target_os = "linux")]
pub mod cgroup;
mod kept;
/// Landlock confinement of managed servers (Linux).
#[cfg(target_os = "linux")]
pub mod landlock;
use backend::{Local, ProcessBackend, Remote, Status};
pub(crate) use backend::{spawn as spawn_process, stop_child, write_console};
#[cfg(unix)]
pub(crate) use kept::CONSOLE_FILE as KEPT_CONSOLE_FILE;
pub(crate) use kept::SUPPORTED as KEEP_RUNNING_SUPPORTED;

/// Whether a server Rift started with `keep_running` still runs in
/// `directory`, so its port is in use by a process Rift will adopt.
pub(crate) fn kept_running(directory: &Path) -> bool {
    kept::recorded(directory).is_some()
}

/// The pid of the server Rift started with `keep_running` in `directory`,
/// if that process still runs there.
pub(crate) fn kept_pid(directory: &Path) -> Option<u32> {
    kept::recorded(directory).map(|process| process.pid())
}

/// Start `command` in `directory` as a server with `keep_running` (see
/// [`kept`]): it outlives this process, reads its console from a FIFO in
/// its directory and is recorded there, so a later process adopts it. The
/// launch is otherwise [`spawn_process`]'s. Used by a cluster peer's
/// embedded node agent; local servers start through their worker.
pub(crate) fn spawn_kept(
    command: &[String],
    directory: &Path,
    environment: &crate::config::ProcessEnvironment,
    isolation: &crate::config::Isolation,
    memory_limit: Option<(&str, crate::config::MemoryLimit)>,
) -> io::Result<tokio::process::Child> {
    if !kept::SUPPORTED {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "keep_running requires Linux or macOS",
        ));
    }
    let console = kept::console(directory)?;
    let mut child = backend::spawn_with(
        command,
        directory,
        environment,
        isolation,
        memory_limit,
        Some(console),
    )?;
    // Without its record the next process could not adopt it.
    if let Some(pid) = child.id()
        && let Err(error) = kept::record(directory, pid)
    {
        let _ = child.start_kill();
        kept::forget(directory);
        return Err(error);
    }
    Ok(child)
}

/// Send `line` to the console of the kept server in `directory`.
pub(crate) fn write_kept_console(directory: &Path, line: &str) -> io::Result<()> {
    kept::write_console(directory, line)
}

/// Stop such a server outside a supervisor: `stop` on its console, then a
/// kill after `grace`. Does nothing without a recorded running process.
pub(crate) fn stop_kept(directory: &Path, grace: Duration) -> io::Result<()> {
    match kept::recorded(directory) {
        Some(process) => {
            process.stop(directory, grace)?;
            kept::forget(directory);
            Ok(())
        }
        None => Ok(()),
    }
}

/// Observable lifecycle state of a supervised process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
    /// The server's node is unreachable; its process may still be running.
    Unknown,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// The phase [`Phase::as_str`] names.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "stopped" => Self::Stopped,
            "starting" => Self::Starting,
            "running" => Self::Running,
            "stopping" => Self::Stopping,
            "failed" => Self::Failed,
            "unknown" => Self::Unknown,
            _ => return None,
        })
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

/// An operator or player request that a cluster follower hands to the
/// leader, whose workers alone start and stop servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Forward {
    /// An administrator start ([`ManagedServers::start`]).
    Start,
    /// An administrator stop ([`ManagedServers::stop`]).
    Stop,
    /// Start on connect ([`ManagedServers::ensure_running`]).
    Demand,
    /// Capacity that does not override an administrator stop
    /// ([`ManagedServers::request_scale_start`]).
    ScaleStart,
}

/// A future a [`Coordinator`] returns.
pub type CoordinatorFuture<T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// What the workers of a cluster peer consult (docs/clustering.md, "Work
/// that runs once: the leader"). Only the leader's workers start, stop,
/// adopt, recover and idle-stop servers; the other peers mirror the state
/// the leader records and hand requests to it. A single Rift sets none and
/// behaves as before.
pub trait Coordinator: Send + Sync + fmt::Debug {
    /// The term this peer leads in while its lease holds; `None` otherwise.
    fn acting_term(&self) -> Option<u64>;
    /// The state the leader recorded for `name`, as of this peer's replica.
    fn recorded(&self, name: &str) -> Option<crate::instance_store::ServerState>;
    /// On the leader: record `name`'s state for every peer, decided in
    /// `term`. Must not block; the latest state of a server wins.
    fn record(&self, name: &str, state: crate::instance_store::ServerState, term: u64);
    /// On a follower: run `request` for `name` on the leader, waiting at
    /// most `wait`; a demand returns once this peer sees the server running.
    fn forward(
        &self,
        name: &str,
        request: Forward,
        wait: Duration,
    ) -> CoordinatorFuture<io::Result<()>>;
    /// On the leader, before stopping `name` for being idle: whether every
    /// live peer agreed (none has a player on its way there, none was
    /// unreachable) and no expired peer's players may still be on it.
    fn may_stop(&self, name: &str) -> CoordinatorFuture<bool>;
}

/// The [`Coordinator`] every worker of one [`ManagedServers`] reads.
type CoordinatorSlot = Arc<std::sync::RwLock<Option<Arc<dyn Coordinator>>>>;

/// What a worker remembers of its part in a cluster.
#[derive(Debug, Default)]
struct ClusterView {
    /// The worker acted (as the leader's) at its last look.
    acting: bool,
    /// What it recorded last, or found recorded when it took over.
    recorded: Option<crate::instance_store::ServerState>,
    /// After taking over, a stopped server is not recorded until its agent
    /// reported (or this passed): it may well be running there.
    settle_until: Option<Instant>,
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
    /// The node agent running this server; `None` for this host.
    pub node: Option<String>,
    /// Held by `rift admin move`: stopped, and refusing every start until
    /// the move commits or is abandoned.
    pub moving: bool,
}

/// Child stdout and stderr, appended in the server's working directory.
pub(crate) const LOG_FILE: &str = "rift-server.log";
/// The previous log, replaced at each rotation.
pub(crate) const ROTATED_LOG_FILE: &str = "rift-server.log.1";
/// A server log at least this large is rotated, so one server keeps at most
/// about twice this much output.
pub(crate) const LOG_ROTATE_BYTES: u64 = 32 * 1024 * 1024;
/// How often a running server's log size is checked.
pub(crate) const LOG_CHECK_INTERVAL: Duration = Duration::from_secs(30);

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
    nodes: Nodes,
    logins: LoginSlot,
    /// What servers report about themselves; forgotten when they start.
    states: crate::service_state::ServiceStates,
    /// Set on a cluster peer ([`ManagedServers::set_coordinator`]).
    coordinator: CoordinatorSlot,
}

/// The instance logins every worker reads when it starts a process, set
/// once the bus is up ([`ManagedServers::set_logins`]).
pub(crate) type LoginSlot = Arc<std::sync::RwLock<Option<Arc<crate::instance_login::Logins>>>>;

#[derive(Debug)]
struct Server {
    name: String,
    config: ManagedServer,
    address: Option<SocketAddr>,
    /// The backend address the proxy connects to, also for remote servers.
    endpoint: Option<String>,
    /// An ephemeral instance's per-run copy of its template.
    fresh: Option<Arc<Fresh>>,
    state: Mutex<State>,
    sender: mpsc::Sender<Request>,
    receiver: Mutex<Option<mpsc::Receiver<Request>>>,
    shutdown: AtomicBool,
    /// Controller shutdown rather than removal: remote processes and local
    /// ones with `keep_running` keep running.
    detach: AtomicBool,
    /// A controller shutdown that also stops local `keep_running` processes.
    stop_kept: AtomicBool,
    wake: Notify,
    /// Shared with [`Inner::states`]; a new run starts without a report.
    states: crate::service_state::ServiceStates,
    /// Shared with [`Inner::coordinator`].
    coordinator: CoordinatorSlot,
    cluster: Mutex<ClusterView>,
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
    /// Parked for a move to another node (see [`ManagedServers::park`]).
    moving: bool,
    /// Holds of [`ManagedServers::quiesce`]: while any is held, no new
    /// player is routed here.
    quiesced: usize,
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

/// A server held for a move by [`ManagedServers::park`]: stopped, and
/// refusing every start. Dropping it lifts the hold.
#[derive(Debug)]
pub struct Parked {
    managed: ManagedServers,
    server: Arc<Server>,
    was_running: bool,
    automatic: bool,
}

impl Parked {
    pub fn name(&self) -> &str {
        &self.server.name
    }

    /// Whether the process was running, or may have been on an unreachable
    /// node, when the move began.
    pub fn was_running(&self) -> bool {
        self.was_running
    }

    /// The administrator intent the server had: `false` after `stop`.
    pub fn automatic(&self) -> bool {
        self.automatic
    }

    /// Lift the hold and start the server again if it was running: on its
    /// new node after a committed move, on its old one after an abandoned
    /// move. The start never overrides an administrator stop.
    pub fn resume(self) {
        let (managed, name) = (self.managed.clone(), self.server.name.clone());
        let restart = self.was_running && self.automatic;
        drop(self);
        if restart && let Err(error) = managed.request_scale_start(&name) {
            log_warn!("restart of {name} after its move rejected: {error}");
        }
    }
}

impl Drop for Parked {
    fn drop(&mut self) {
        lock(&self.server.state).moving = false;
    }
}

/// A server that takes no new players while this is held: the hold of
/// [`ManagedServers::quiesce`]. Dropping it lifts the hold.
#[derive(Debug)]
pub struct Quiesced {
    server: Arc<Server>,
}

impl Quiesced {
    /// Whether players are still on their way to the server: reservations
    /// taken before the hold. A leader asking this peer to stop the server
    /// takes that as a veto.
    pub fn has_inbound(&self) -> bool {
        lock(&self.server.state).reservations > 0
    }
}

impl Drop for Quiesced {
    fn drop(&mut self) {
        lock(&self.server.state).quiesced -= 1;
    }
}

/// Register `config`'s secret variables so no child inherits them. Called
/// wherever a configuration reaches the supervisor, before any spawn.
pub fn protect_secrets(config: &Config) {
    crate::environment::protect(
        config
            .secret_variables()
            .into_iter()
            .map(|(_, variable)| variable),
    );
}

impl ManagedServers {
    /// Construction does not need a Tokio runtime and has no side effect
    /// beyond [`protect_secrets`].
    pub fn new(config: &Config, players: Arc<PlayerRegistry>) -> Self {
        let nodes = Nodes::new(crate::messaging::Broker::default(), config);
        Self::with_nodes(config, players, nodes)
    }

    /// Servers with a `node` run through that node's agent in `nodes`.
    pub fn with_nodes(config: &Config, players: Arc<PlayerRegistry>, nodes: Nodes) -> Self {
        protect_secrets(config);
        let states = crate::service_state::ServiceStates::default();
        let coordinator = CoordinatorSlot::default();
        // Restored ephemeral instances learn their markers in `with_persistence`.
        let shared = config
            .instances
            .values()
            .any(|instance| instance.storage == crate::config::InstanceStorage::Ephemeral)
            .then(|| Arc::new(config.clone()));
        let servers = config
            .managed_servers
            .iter()
            .map(|(name, definition)| {
                let endpoint = config
                    .backends
                    .get(name)
                    .map(|backend| backend.address().to_owned());
                let fresh = shared
                    .as_ref()
                    .and_then(|config| Fresh::new(config, name, false));
                let server = new_server(
                    name,
                    definition.clone(),
                    endpoint,
                    &states,
                    &coordinator,
                    fresh,
                );
                (name.clone(), server)
            })
            .collect();
        Self(Arc::new(Inner {
            servers: Mutex::new(servers),
            retired: Mutex::new(BTreeSet::new()),
            players,
            launched: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            instance_store: Mutex::new(None),
            nodes,
            logins: LoginSlot::default(),
            states,
            coordinator,
        }))
    }

    pub fn nodes(&self) -> &Nodes {
        &self.0.nodes
    }

    /// What servers report about themselves on
    /// [`crate::service_state::SUBJECT`], managed or not.
    pub fn service_states(&self) -> &crate::service_state::ServiceStates {
        &self.0.states
    }

    /// Hand each process its bus login from now on (see
    /// [`crate::instance_login`]), and issue the logins of the servers
    /// already registered: static ones and instances restored at startup.
    pub fn set_logins(
        &self,
        logins: Arc<crate::instance_login::Logins>,
        config: &Config,
    ) -> io::Result<()> {
        *self
            .0
            .logins
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(logins);
        let names: Vec<String> = lock(&self.0.servers).keys().cloned().collect();
        for name in names {
            self.issue_login(config, &name)?;
        }
        Ok(())
    }

    /// Issue `name` its login when its definition has a `messaging` block,
    /// at the generation the instance database recorded for its creation
    /// (0 for a static server), and put the bus certificate in its local
    /// directory. Any other server has no login.
    fn issue_login(&self, config: &Config, name: &str) -> io::Result<()> {
        let logins = self
            .0
            .logins
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(logins) = logins else {
            return Ok(());
        };
        let Some(definition) = config.managed_servers.get(name) else {
            return Ok(());
        };
        let Some(grants) = &definition.messaging else {
            logins.unregister(name);
            return Ok(());
        };
        let generation = match lock(&self.0.instance_store).as_ref() {
            Some(store) => store.login_generation(name)?,
            None => 0,
        };
        let group = config
            .instances
            .get(name)
            .map(|instance| instance.group.as_str());
        logins.register(name, generation, group, grants.clone());
        if definition.node.is_none()
            && definition.directory.is_dir()
            && let Err(error) = logins.write_certificate(&definition.directory)
        {
            log_error!("cannot write the bus certificate for {name}: {error}");
        }
        Ok(())
    }

    /// The node running `name`, or `None` for this host and unknown names.
    pub fn node_of(&self, name: &str) -> Option<String> {
        lock(&self.0.servers).get(name)?.config.node.clone()
    }

    /// Managed servers that the controller assigns to `node`.
    pub fn names_on(&self, node: &str) -> Vec<String> {
        lock(&self.0.servers)
            .values()
            .filter(|server| server.config.node.as_deref() == Some(node))
            .map(|server| server.name.clone())
            .collect()
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
            if let Some(fresh) = &server.fresh
                && let Some(plan) = store.plan(name)?
            {
                fresh.set_marker(&plan)?;
            }
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
            launch_worker(
                &runtime,
                server,
                &self.0.players,
                &self.0.nodes,
                &self.0.logins,
            );
        }
        Ok(())
    }

    /// Register a configured backend and give its child process a worker owner.
    /// The registry lock serializes registration against launch and shutdown.
    pub fn add(&self, config: &Config, name: &str) -> io::Result<()> {
        self.add_with(config, name, true)
    }

    /// Register the new worker of an instance that moved to another node,
    /// keeping the administrator intent (`automatic`) of its old worker.
    pub fn add_moved(&self, config: &Config, name: &str, automatic: bool) -> io::Result<()> {
        self.add_with(config, name, automatic)
    }

    fn add_with(&self, config: &Config, name: &str, automatic: bool) -> io::Result<()> {
        protect_secrets(config);
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
        let endpoint = config
            .backends
            .get(name)
            .map(|backend| backend.address().to_owned());
        if definition.node.is_none() {
            loopback_address(endpoint.as_deref().and_then(|e| e.parse().ok()))?;
        } else if endpoint.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a managed server on a node requires a backend address",
            ));
        }
        if definition.command.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed server command is empty",
            ));
        }
        // Before the worker exists, so even an autostarting server's first
        // process has its login.
        self.issue_login(config, name)?;
        let fresh = config
            .instances
            .get(name)
            .is_some_and(|instance| instance.storage == crate::config::InstanceStorage::Ephemeral)
            .then(|| Fresh::new(&Arc::new(config.clone()), name, true))
            .flatten();
        if let Some(fresh) = &fresh
            && let Some(store) = lock(&self.0.instance_store).as_ref()
            && let Some(plan) = store.plan(name)?
        {
            fresh.set_marker(&plan)?;
        }
        let server = new_server(
            name,
            definition.clone(),
            endpoint,
            &self.0.states,
            &self.0.coordinator,
            fresh,
        );
        lock(&server.state).automatic = automatic;
        launch_worker(
            &runtime,
            &server,
            &self.0.players,
            &self.0.nodes,
            &self.0.logins,
        );
        servers.insert(name.to_owned(), server);
        lock(&self.0.retired).remove(name);
        Ok(())
    }

    /// Retire only an unoccupied backend. A detached owner completes cleanup
    /// even if the administrator disconnects while its child is terminating.
    pub async fn remove(&self, name: &str) -> io::Result<()> {
        self.retire(name, false).await
    }

    /// Retire the parked worker of an instance that moved to another node.
    /// Its registration stays; [`add_moved`](Self::add_moved) gives it a new
    /// worker on the new node.
    pub async fn retire_moved(&self, name: &str) -> io::Result<()> {
        self.retire(name, true).await
    }

    async fn retire(&self, name: &str, moved: bool) -> io::Result<()> {
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
            // A cluster follower retires what the leader already removed:
            // the leader checked, and its worker owns the process.
            let passive = self.passive().is_some();
            if !passive && (state.reservations > 0 || self.0.players.count_on_server(name) > 0) {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "managed server has connected players or pending connections",
                ));
            }
            if !passive && state.moving != moved {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    if moved {
                        "managed server is not parked for a move"
                    } else {
                        "managed server is moving to another node; wait for the move to finish"
                    },
                ));
            }
            if !moved
                && !passive
                && let Some(store) = lock(&self.0.instance_store).as_ref()
            {
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
                    inner.states.clear(&name);
                    // The process is gone; so are its login and sessions.
                    if let Some(logins) = inner
                        .logins
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                    {
                        logins.unregister(&name);
                    }
                }
                let _ = reply.send(());
            });
        }
        receive
            .await
            .map_err(|_| unavailable("managed server removal worker stopped"))
    }

    /// Hold `name` for a move to another node: refuse every start from now
    /// on, then stop its process without changing the administrator intent.
    /// Players must have left first. A server whose node is unreachable is
    /// not stopped, since its agent cannot be told; that node stops it when
    /// it reconciles after the move. Dropping the result lifts the hold.
    pub async fn park(&self, name: &str) -> io::Result<Parked> {
        let busy = |message: &str| io::Error::new(io::ErrorKind::ResourceBusy, message);
        let (parked, stopped) = {
            let servers = lock(&self.0.servers);
            self.check_running()?;
            let server = servers
                .get(name)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown managed server"))?
                .clone();
            let mut state = lock(&server.state);
            if server.shutdown.load(Ordering::Acquire) {
                return Err(busy("managed server is retiring"));
            }
            if state.moving {
                return Err(busy("managed server is already moving"));
            }
            if state.reservations > 0 || self.0.players.count_on_server(name) > 0 {
                return Err(busy(
                    "managed server has connected players or pending connections; move them to another server first",
                ));
            }
            if state.pending_stop {
                return Err(busy(
                    "managed server is stopping; try again once it has stopped",
                ));
            }
            let reachable = server
                .config
                .node
                .as_ref()
                .is_none_or(|node| self.0.nodes.online(node));
            let stopped = if reachable && !matches!(state.phase, Phase::Stopped | Phase::Failed) {
                let permit = server
                    .sender
                    .try_reserve()
                    .map_err(|e| unavailable(&e.to_string()))?;
                let (reply, receive) = oneshot::channel();
                permit.send(Request {
                    action: Action::Stop,
                    reply: Some(reply),
                });
                state.pending_stop = true;
                state.recover = false;
                state.retry_at = None;
                server.wake.notify_one();
                Some(receive)
            } else {
                None
            };
            state.moving = true;
            let was_running = matches!(
                state.phase,
                Phase::Running | Phase::Starting | Phase::Unknown
            );
            let automatic = state.automatic;
            drop(state);
            let parked = Parked {
                managed: self.clone(),
                server,
                was_running,
                automatic,
            };
            (parked, stopped)
        };
        if let Some(stopped) = stopped {
            stopped
                .await
                .map_err(|_| unavailable("managed server worker stopped"))??;
        }
        Ok(parked)
    }

    /// Whether players are on their way to `name` from this process: a
    /// pending connection or a transfer holds a reservation until the
    /// player's presence is committed. A cluster leader that wants to stop
    /// `name` (idle or scale-down) asks every peer this, and a `true` is a
    /// veto. Players already on the server are counted from presence.
    pub fn has_reservation_or_inbound(&self, name: &str) -> bool {
        lock(&self.0.servers)
            .get(name)
            .is_some_and(|server| lock(&server.state).reservations > 0)
    }

    /// Pending connections and transfers per server, for the servers that
    /// have any: what a cluster peer reports to the leader every second.
    pub fn reservations(&self) -> BTreeMap<String, usize> {
        lock(&self.0.servers)
            .iter()
            .filter_map(|(name, server)| {
                let reservations = lock(&server.state).reservations;
                (reservations > 0).then(|| (name.clone(), reservations))
            })
            .collect()
    }

    /// Make this peer's workers follow `coordinator`: they act only while
    /// it leads, and mirror the leader's records otherwise. Set once, on a
    /// cluster peer, before the first request.
    pub fn set_coordinator(&self, coordinator: Arc<dyn Coordinator>) {
        *self
            .0
            .coordinator
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(coordinator);
        for server in lock(&self.0.servers).values() {
            server.wake.notify_one();
        }
    }

    /// Stop following a coordinator, when the cluster stops: the workers
    /// act on their own again.
    pub fn clear_coordinator(&self) {
        *self
            .0
            .coordinator
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// The coordinator, while this peer does not lead: requests then go to
    /// the leader.
    fn passive(&self) -> Option<Arc<dyn Coordinator>> {
        coordinator(&self.0.coordinator).filter(|coordinator| coordinator.acting_term().is_none())
    }

    /// The longest a request for `name` handed to the leader may take.
    fn forward_wait(&self, name: &str) -> io::Result<Duration> {
        let servers = lock(&self.0.servers);
        let server = servers
            .get(name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown managed server"))?;
        Ok(server
            .config
            .start_timeout
            .max(server.config.stop_timeout)
            .saturating_add(Duration::from_secs(15)))
    }

    /// Stop routing new players to `name` until the returned hold is
    /// dropped: new reservations fail and the server is not reachable for
    /// routing, while players already there or on their way stay. A
    /// cluster leader takes the hold on every peer before it stops a server,
    /// and the peers drop it once the server stopped or the stop was
    /// abandoned. `None` for a server that is not managed here. Nothing
    /// else changes: a single Rift never takes a hold.
    pub fn quiesce(&self, name: &str) -> Option<Quiesced> {
        let server = lock(&self.0.servers).get(name)?.clone();
        lock(&server.state).quiesced += 1;
        Some(Quiesced { server })
    }

    /// Whether `name` is held by [`park`](Self::park).
    pub fn is_moving(&self, name: &str) -> bool {
        lock(&self.0.servers)
            .get(name)
            .is_some_and(|server| lock(&server.state).moving)
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
        // A cluster follower asks the leader and waits until the server
        // runs as the leader records it.
        if let Some(coordinator) = self.passive() {
            let running = {
                let servers = lock(&self.0.servers);
                match servers.get(name) {
                    Some(server) => {
                        self.check_running()?;
                        let state = lock(&server.state);
                        connect_allowed(server, &state)?;
                        Some(state.phase == Phase::Running)
                    }
                    None => None,
                }
            };
            match running {
                Some(true) => return Ok(()),
                Some(false) => {
                    let wait = self.forward_wait(name)?;
                    return coordinator.forward(name, Forward::Demand, wait).await;
                }
                // Unknown here: answered below as on a single Rift.
                None => {}
            }
        }
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
    /// Remote servers read the same file on their node through its agent.
    pub async fn logs(&self, name: &str, cursor: Option<u64>) -> io::Result<serde_json::Value> {
        let (directory, node) = {
            let servers = lock(&self.0.servers);
            let server = servers
                .get(name)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown managed server"))?;
            (server.config.directory.clone(), server.config.node.clone())
        };
        if let Some(node) = node {
            return self
                .0
                .nodes
                .request(
                    &node,
                    &protocol::Command::Logs {
                        server: name.to_owned(),
                        directory,
                        cursor,
                    },
                    Duration::from_secs(15),
                )
                .await;
        }
        let path = directory.join(LOG_FILE);
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
        // A cluster follower's workers never act: the leader's do.
        if let Some(request) = forward_of(&action)
            && let Some(coordinator) = self.passive()
        {
            let wait = self.forward_wait(name)?;
            let runtime = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
            let future = coordinator.forward(name, request, wait);
            let name = name.to_owned();
            runtime.spawn(async move {
                let result = future.await;
                match reply {
                    Some(reply) => {
                        let _ = reply.send(result);
                    }
                    None => {
                        if let Err(error) = result {
                            log_warn!("{name}: the cluster leader refused {request:?}: {error}");
                        }
                    }
                }
            });
            return Ok(());
        }
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
        // Refused before the durable intent changes, like any rejection.
        if state.moving && matches!(action, Action::Start { .. }) {
            return Err(moving());
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

    /// The state `snapshot` reports for one server; `None` if Rift does not
    /// manage it.
    pub fn phase(&self, name: &str) -> Option<Phase> {
        let servers = lock(&self.0.servers);
        let state = lock(&servers.get(name)?.state);
        Some(if state.pending_stop {
            Phase::Stopping
        } else {
            state.phase
        })
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
                    node: server.config.node.clone(),
                    moving: state.moving,
                }
            })
            .collect()
    }

    /// The proxy must first drain its sessions. Shutdown then forcibly stops all
    /// processes Rift owns, including ones whose sessions exceeded that deadline.
    /// Servers on nodes and local ones with `keep_running` keep running for
    /// the next proxy to adopt.
    pub async fn shutdown(&self) {
        self.shut_down(false).await;
    }

    /// [`shutdown`](Self::shutdown), and stop local `keep_running` servers too.
    pub async fn shutdown_all(&self) {
        self.shut_down(true).await;
    }

    async fn shut_down(&self, stop_kept: bool) {
        let servers: Vec<_> = {
            let servers = lock(&self.0.servers);
            self.0.shutdown.store(true, Ordering::Release);
            for server in servers.values() {
                server.stop_kept.store(stop_kept, Ordering::Release);
                server.detach.store(true, Ordering::Release);
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
            server.detach.store(true, Ordering::Release);
            server.shutdown.store(true, Ordering::Release);
            server.wake.notify_one();
        }
    }
}

fn new_server(
    name: &str,
    config: ManagedServer,
    endpoint: Option<String>,
    states: &crate::service_state::ServiceStates,
    coordinator: &CoordinatorSlot,
    fresh: Option<Fresh>,
) -> Arc<Server> {
    let (sender, receiver) = mpsc::channel(64);
    Arc::new(Server {
        name: name.to_owned(),
        fresh: fresh.map(Arc::new),
        states: states.clone(),
        coordinator: coordinator.clone(),
        cluster: Mutex::new(ClusterView::default()),
        address: endpoint
            .as_deref()
            .and_then(|endpoint| endpoint.parse().ok()),
        endpoint,
        config,
        sender,
        receiver: Mutex::new(Some(receiver)),
        shutdown: AtomicBool::new(false),
        detach: AtomicBool::new(false),
        stop_kept: AtomicBool::new(false),
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
            moving: false,
            quiesced: 0,
        }),
    })
}

fn launch_worker(
    runtime: &tokio::runtime::Handle,
    server: &Arc<Server>,
    players: &Arc<PlayerRegistry>,
    nodes: &Nodes,
    logins: &LoginSlot,
) {
    let receiver = lock(&server.receiver).take().expect("single launch");
    match &server.config.node {
        None => runtime.spawn(worker(
            server.clone(),
            players.clone(),
            receiver,
            Local::new(logins.clone()),
        )),
        Some(node) => runtime.spawn(worker(
            server.clone(),
            players.clone(),
            receiver,
            Remote::new(nodes.clone(), node.clone(), logins.clone()),
        )),
    };
}

/// Process ownership must survive a panic in an unrelated lock holder.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Rotate the server log in `directory` once it holds `limit` bytes: copy it
/// to [`ROTATED_LOG_FILE`], then empty it. The running child appends through
/// its own descriptor, so the file is truncated in place and never renamed;
/// output written between the copy and the truncation is lost. Returns whether
/// the log was rotated.
pub(crate) fn rotate_log(directory: &Path, limit: u64) -> io::Result<bool> {
    use std::fs::OpenOptions;
    let path = directory.join(LOG_FILE);
    // The server can write its own directory: never follow a planted link.
    let open = |options: &mut OpenOptions, path: &Path| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Non-blocking so a planted FIFO cannot stall the check.
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        options.open(path)
    };
    let source = match open(OpenOptions::new().read(true).write(true), &path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let metadata = source.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("server log must be a regular file"));
    }
    if metadata.len() < limit {
        return Ok(false);
    }
    let temporary = directory.join("rift-server.log.rotating");
    match std::fs::remove_file(&temporary) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut target = open(OpenOptions::new().write(true).create_new(true), &temporary)?;
    let copied = io::copy(&mut io::Read::take(&source, metadata.len()), &mut target);
    drop(target);
    if let Err(error) =
        copied.and_then(|_| std::fs::rename(&temporary, directory.join(ROTATED_LOG_FILE)))
    {
        // Without a copy the log still has to stop growing.
        let _ = std::fs::remove_file(&temporary);
        source.set_len(0)?;
        return Err(error);
    }
    source.set_len(0)?;
    Ok(true)
}

/// [`rotate_log`] at the standard size, reporting failures instead of
/// returning them: a log that cannot be rotated never stops its server.
pub(crate) fn rotate_log_reporting(directory: &Path) {
    if let Err(error) = rotate_log(directory, LOG_ROTATE_BYTES) {
        log_error!(
            "cannot rotate {}: {error}",
            directory.join(LOG_FILE).display()
        );
    }
}

pub(crate) fn read_log(path: &Path, cursor: Option<u64>) -> io::Result<serde_json::Value> {
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

fn moving() -> io::Error {
    unavailable("managed server is moving to another node")
}

/// The coordinator in `slot`, if any.
fn coordinator(slot: &CoordinatorSlot) -> Option<Arc<dyn Coordinator>> {
    slot.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// What a cluster follower asks the leader for instead of `action`; `None`
/// for console commands, which reach the server's agent from any peer.
fn forward_of(action: &Action) -> Option<Forward> {
    match action {
        Action::Start {
            mode: StartMode::Manual,
        } => Some(Forward::Start),
        Action::Start {
            mode: StartMode::Scale,
        } => Some(Forward::ScaleStart),
        Action::Start { .. } | Action::Demand(_) => Some(Forward::Demand),
        Action::Stop => Some(Forward::Stop),
        Action::Console(_) => None,
    }
}

/// The state the leader records for `server` (see [`Coordinator::record`]).
fn recordable(server: &Server) -> crate::instance_store::ServerState {
    let state = lock(&server.state);
    let phase = if state.pending_stop {
        Phase::Stopping
    } else {
        state.phase
    };
    crate::instance_store::ServerState {
        phase: phase.as_str().to_owned(),
        last_error: state.last_error.clone(),
        automatic: state.automatic,
        restart_attempts: u32::try_from(state.restart_attempts).unwrap_or(u32::MAX),
    }
}

/// Mirror what the leader recorded for `server` on a follower, whose
/// worker never acts: routing on this peer then sees the leader's view.
fn mirror(server: &Server, recorded: Option<&crate::instance_store::ServerState>) {
    let mut state = lock(&server.state);
    match recorded {
        Some(recorded) => {
            state.phase = Phase::parse(&recorded.phase).unwrap_or(Phase::Unknown);
            state.last_error = recorded.last_error.clone();
            state.automatic = recorded.automatic;
            state.restart_attempts = recorded.restart_attempts as usize;
        }
        None => {
            state.phase = Phase::Stopped;
            state.last_error = None;
        }
    }
    state.recover = state.phase == Phase::Failed;
    state.pid = None;
    state.retry_at = None;
    state.pending_stop = false;
}

/// This peer started to lead: take over `server` from the state the
/// previous leader recorded. A process is adopted from what its agent
/// reports, never from what this worker owned before another peer led.
fn take_over<B: ProcessBackend>(
    server: &Server,
    backend: &mut B,
    recorded: Option<&crate::instance_store::ServerState>,
) {
    backend.forget();
    let mut state = lock(&server.state);
    if let Some(recorded) = recorded {
        state.automatic = recorded.automatic;
        state.restart_attempts = recorded.restart_attempts as usize;
        state.last_error = recorded.last_error.clone();
    }
    state.pid = None;
    state.pending_stop = false;
    state.retry_at = None;
    if state.phase == Phase::Failed {
        // Recovery continues where the previous leader left it.
        state.recover = true;
    } else {
        state.phase = Phase::Stopped;
        state.recover = false;
    }
    state.last_activity = Instant::now();
}

/// The cluster part of one worker tick: whether the worker acts. A
/// follower's worker mirrors the leader's record and does nothing else; a
/// worker whose peer just started to lead takes the server over.
fn follow_cluster<B: ProcessBackend>(server: &Server, backend: &mut B) -> bool {
    let Some(coordinator) = coordinator(&server.coordinator) else {
        return true;
    };
    let recorded = coordinator.recorded(&server.name);
    let mut view = lock(&server.cluster);
    if coordinator.acting_term().is_none() {
        view.acting = false;
        view.recorded = None;
        view.settle_until = None;
        drop(view);
        mirror(server, recorded.as_ref());
        return false;
    }
    if !view.acting {
        view.acting = true;
        view.settle_until = Some(Instant::now() + crate::node::protocol::UNREACHABLE_AFTER);
        drop(view);
        take_over(server, backend, recorded.as_ref());
        lock(&server.cluster).recorded = recorded;
    }
    true
}

/// On the leader: record `server`'s state for every peer when it changed.
fn announce(server: &Server) {
    let Some(coordinator) = coordinator(&server.coordinator) else {
        return;
    };
    let Some(term) = coordinator.acting_term() else {
        return;
    };
    let current = recordable(server);
    let mut view = lock(&server.cluster);
    if !view.acting {
        return;
    }
    if let Some(until) = view.settle_until {
        if current.phase == Phase::Stopped.as_str() && Instant::now() < until {
            return;
        }
        view.settle_until = None;
    }
    if view.recorded.as_ref() != Some(&current) {
        coordinator.record(&server.name, current.clone(), term);
        view.recorded = Some(current);
    }
}

/// Whether this worker may act now: always without a cluster.
fn acting(server: &Server) -> bool {
    coordinator(&server.coordinator).is_none_or(|coordinator| coordinator.acting_term().is_some())
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
    if state.moving {
        return Err(moving());
    }
    if state.quiesced > 0 {
        return Err(unavailable(
            "managed server is being stopped and takes no new players",
        ));
    }
    if server.shutdown.load(Ordering::Acquire)
        || state.pending_stop
        || state.phase == Phase::Stopping
        || !state.automatic
    {
        return Err(unavailable(
            "managed server is stopped by an administrator or shutting down",
        ));
    }
    if state.phase == Phase::Unknown {
        return Err(node_unreachable(server));
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
    if state.moving {
        return Err(moving());
    }
    if server.shutdown.load(Ordering::Acquire) || state.pending_stop || !state.automatic {
        return Err(unavailable(
            "managed server is stopped by an administrator or shutting down",
        ));
    }
    if state.phase == Phase::Failed {
        return Err(recovery_pending(server, state));
    }
    if state.phase == Phase::Unknown {
        return Err(node_unreachable(server));
    }
    Ok(())
}

fn node_unreachable(server: &Server) -> io::Error {
    unavailable(&format!(
        "node {} is unreachable",
        server.config.node.as_deref().unwrap_or("local")
    ))
}

async fn worker<B: ProcessBackend>(
    server: Arc<Server>,
    players: Arc<PlayerRegistry>,
    mut requests: mpsc::Receiver<Request>,
    mut backend: B,
) {
    // A copy left by a run this process did not reap, e.g. before a crash.
    if server.fresh.as_ref().is_some_and(|fresh| !fresh.pristine()) {
        discard_copy(&server, &backend).await;
    }
    // A remote server autostarts once its node is reachable, which may be
    // after the controller itself started.
    let mut autostart = server.config.autostart && lock(&server.state).automatic;
    // In a cluster only the leader's worker starts anything; a follower's
    // mirrors the leader's record from the start.
    let acts = follow_cluster(&server, &mut backend);
    if autostart && acts && backend.available().is_ok() {
        autostart = false;
        let _ = start_process(&server, &mut backend, StartMode::Scale).await;
    }
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut log_checked = Instant::now();
    loop {
        if server.shutdown.load(Ordering::Acquire) {
            break;
        }
        tokio::select! {
            _ = server.wake.notified() => {}
            request = requests.recv() => {
                let Some(request) = request else { break };
                // Queued before this peer stopped leading: the leader decides now.
                if !matches!(request.action, Action::Console(_)) && !acting(&server) {
                    refuse(&server, request, "this peer no longer leads the cluster; retry");
                    continue;
                }
                if matches!(request.action, Action::Start { .. } | Action::Stop) {
                    autostart = false;
                }
                let result = perform(&server, &mut backend, request.action).await;
                announce(&server);
                if let Some(reply) = request.reply {
                    let _ = reply.send(result);
                }
            }
            _ = tick.tick() => {
                if !follow_cluster(&server, &mut backend) {
                    continue;
                }
                if autostart && backend.available().is_ok() {
                    autostart = false;
                    if lock(&server.state).automatic {
                        let _ = start_process(&server, &mut backend, StartMode::Scale).await;
                    }
                }
                supervise(&server, &players, &mut backend).await;
                announce(&server);
                // A node agent rotates the logs of the servers it runs.
                if server.config.node.is_none() && log_checked.elapsed() >= LOG_CHECK_INTERVAL {
                    log_checked = Instant::now();
                    let directory = server.config.directory.clone();
                    let _ = tokio::task::spawn_blocking(move || rotate_log_reporting(&directory)).await;
                }
            }
        }
    }
    if !(server.detach.load(Ordering::Acquire) && backend.survives_controller(&server)) {
        let _ = stop_process(&server, &mut backend).await;
    }
    // Reject queued callers promptly instead of executing stale operations.
    requests.close();
    while let Some(request) = requests.recv().await {
        refuse(&server, request, "managed servers are shutting down");
    }
}

/// Answer `request` with `message` without performing it.
fn refuse(server: &Server, request: Request, message: &str) {
    if let Action::Demand(startup) = request.action {
        finish_startup(server, &startup, Err(unavailable(message)));
    }
    if let Some(reply) = request.reply {
        let _ = reply.send(Err(unavailable(message)));
    }
}

async fn perform<B: ProcessBackend>(
    server: &Server,
    backend: &mut B,
    action: Action,
) -> io::Result<()> {
    match action {
        Action::Start { mode } => start_process(server, backend, mode).await,
        Action::Demand(startup) => {
            let completed = startup.result.borrow().clone();
            match completed {
                // Autostart or an earlier manual start may already have
                // satisfied this queued demand, even on failure.
                Some(result) => copy_result(&result),
                None => start_process(server, backend, StartMode::Demand).await,
            }
        }
        Action::Stop => {
            let result = stop_process(server, backend).await;
            lock(&server.state).pending_stop = false;
            result
        }
        Action::Console(command) => backend.send_command(server, &command).await,
    }
}

/// Reap unexpected exits, run due recovery attempts and apply idle shutdown.
async fn supervise<B: ProcessBackend>(server: &Server, players: &PlayerRegistry, backend: &mut B) {
    check_exit(server, backend).await;
    adopt(server, backend).await;
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
            // Recovery waits for an unreachable node instead of spending retries.
            && backend.available().is_ok()
    };
    if should_recover {
        let _ = start_process(server, backend, StartMode::Recovery).await;
    }
    if let Some(coordinator) = coordinator(&server.coordinator) {
        idle_stop_in_cluster(server, players, backend, coordinator).await;
        return;
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
        let _ = stop_process(server, backend).await;
    }
}

/// The idle stop of a cluster leader: the players of every peer count
/// (presence), and before stopping, every live peer is asked whether a
/// player is on its way there; any veto, unreachable peer or stop hold after
/// a peer expiry keeps the server running for another idle period.
async fn idle_stop_in_cluster<B: ProcessBackend>(
    server: &Server,
    players: &PlayerRegistry,
    backend: &mut B,
    coordinator: Arc<dyn Coordinator>,
) {
    let idle = |state: &mut State| {
        if state.reservations > 0 || players.count_on_server(&server.name) > 0 {
            state.last_activity = Instant::now();
            false
        } else {
            state.phase == Phase::Running
                && !state.pending_stop
                && server
                    .config
                    .idle_timeout
                    .is_some_and(|timeout| state.last_activity.elapsed() >= timeout)
        }
    };
    if !idle(&mut lock(&server.state)) {
        return;
    }
    let agreed = coordinator.may_stop(&server.name).await;
    let should_stop = {
        let mut state = lock(&server.state);
        if !agreed || coordinator.acting_term().is_none() {
            // Asked again only after another idle period.
            state.last_activity = Instant::now();
            false
        } else if idle(&mut state) {
            state.phase = Phase::Stopping;
            state.recover = false;
            state.retry_at = None;
            true
        } else {
            false
        }
    };
    if should_stop {
        let _ = stop_process(server, backend).await;
    }
}

/// Take over a process that kept running while this worker did not own it,
/// e.g. across a controller restart: a remote one, or a local one with
/// `keep_running`. Administrator stops still win.
async fn adopt<B: ProcessBackend>(server: &Server, backend: &mut B) {
    if backend.owns() || server.shutdown.load(Ordering::Acquire) {
        return;
    }
    let Some(pid) = backend.adoptable(server) else {
        return;
    };
    // A local process may have been starting when its proxy went away, so
    // its readiness is awaited again. An agent's report means running.
    let local = server.config.node.is_none();
    let keep = {
        let mut state = lock(&server.state);
        if !matches!(state.phase, Phase::Stopped | Phase::Failed) || state.pending_stop {
            return;
        }
        if state.automatic {
            // A process this worker did not start may have reported
            // anything before; it reports again once it notices.
            server.states.clear(&server.name);
            state.phase = if local {
                Phase::Starting
            } else {
                Phase::Running
            };
            state.pid = pid;
            state.recover = true;
            state.retry_at = None;
            state.last_error = None;
            state.last_activity = Instant::now();
        }
        state.automatic
    };
    backend.adopt(server, pid);
    if !keep {
        let _ = stop_process(server, backend).await;
    } else if local {
        let ready = await_ready(server, backend).await;
        let _ = settle(server, backend, ready).await;
    }
}

async fn check_exit<B: ProcessBackend>(server: &Server, backend: &mut B) {
    match backend.status(server).await {
        Status::Idle => {}
        Status::Running => {
            let mut state = lock(&server.state);
            if state.phase == Phase::Unknown {
                state.phase = Phase::Running;
            }
        }
        Status::Unknown => backend::mark_unknown(server),
        Status::Exited {
            success: true,
            status: _,
        } => {
            record_clean_exit(server);
            discard_copy(server, backend).await;
        }
        Status::Exited { status, .. } => {
            record_failure(server, format!("managed server exited: {status}"));
            discard_copy(server, backend).await;
        }
        Status::Broken(error) => {
            record_failure(server, format!("cannot inspect managed process: {error}"));
            discard_copy(server, backend).await;
        }
    }
}

/// A process that ended on its own with status 0, e.g. a finished game, is
/// stopped rather than failed: no recovery, and its restart budget is kept.
/// It is no administrator stop either, so demand and scaling may start it.
fn record_clean_exit(server: &Server) {
    let mut state = lock(&server.state);
    state.phase = Phase::Stopped;
    state.pid = None;
    state.last_error = None;
    state.retry_at = None;
    state.recover = false;
    state.last_activity = Instant::now();
}

/// Delete an ephemeral instance's copy once no process of it is owned; the
/// next start builds a fresh one. A failure is reported and retried then.
async fn discard_copy<B: ProcessBackend>(server: &Server, backend: &B) {
    let Some(fresh) = server.fresh.clone() else {
        return;
    };
    if backend.owns() {
        return;
    }
    let result = tokio::task::spawn_blocking(move || fresh.discard())
        .await
        .unwrap_or_else(|error| Err(io::Error::other(error)));
    if let Err(error) = result {
        log_error!(
            "cannot delete the ephemeral copy of {}: {error}",
            server.name
        );
        lock(&server.state)
            .last_error
            .get_or_insert_with(|| format!("cannot delete the ephemeral copy: {error}"));
    }
}

fn record_failure(server: &Server, error: String) {
    let mut state = lock(&server.state);
    state.phase = Phase::Failed;
    state.pid = None;
    state.last_error = Some(error);
    state.retry_at = Some(Instant::now() + server.config.restart_delay);
}

async fn start_process<B: ProcessBackend>(
    server: &Server,
    backend: &mut B,
    mode: StartMode,
) -> io::Result<()> {
    let startup = {
        let mut state = lock(&server.state);
        state.startup.get_or_insert_with(Startup::new).clone()
    };
    let result = start_process_inner(server, backend, mode).await;
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

async fn start_process_inner<B: ProcessBackend>(
    server: &Server,
    backend: &mut B,
    mode: StartMode,
) -> io::Result<()> {
    check_exit(server, backend).await;
    adopt(server, backend).await;
    {
        let mut state = lock(&server.state);
        if server.shutdown.load(Ordering::Acquire) || state.pending_stop {
            return Err(unavailable("managed server is stopping"));
        }
        if state.moving {
            return Err(moving());
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
        if state.phase == Phase::Unknown {
            return Err(node_unreachable(server));
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
        // An unreachable node refuses the start without spending a retry.
        if let Err(error) = backend.available() {
            state.last_error = Some(error.to_string());
            return Err(error);
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
        // The previous run's label, properties and drain end with it.
        server.states.clear(&server.name);
    }
    announce(server);
    let result = spawn_ready(server, backend).await;
    settle(server, backend, result).await
}

/// Record how a start, or the adoption of a starting process, ended.
async fn settle<B: ProcessBackend>(
    server: &Server,
    backend: &mut B,
    result: io::Result<()>,
) -> io::Result<()> {
    match result {
        Ok(()) => {
            let mut state = lock(&server.state);
            state.phase = Phase::Running;
            state.retry_at = None;
            state.last_activity = Instant::now();
            Ok(())
        }
        Err(error) => {
            if backend.owns() {
                backend.kill(server).await;
            }
            discard_copy(server, backend).await;
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

async fn spawn_ready<B: ProcessBackend>(server: &Server, backend: &mut B) -> io::Result<()> {
    if let Some(fresh) = server.fresh.clone() {
        tokio::task::spawn_blocking(move || fresh.provision())
            .await
            .map_err(io::Error::other)??;
    }
    backend.start(server).await?;
    lock(&server.state).pid = backend.pid();
    await_ready(server, backend).await
}

/// Wait until the owned process accepts connections, exits, is stopped or
/// exceeds its start timeout.
async fn await_ready<B: ProcessBackend>(server: &Server, backend: &mut B) -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + server.config.start_timeout;
    loop {
        if server.shutdown.load(Ordering::Acquire) || lock(&server.state).pending_stop {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "managed server startup was stopped",
            ));
        }
        exited_during_startup(server, backend, "before becoming ready").await?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "managed server startup timed out",
            ));
        }
        if backend
            .ready(server, remaining.min(Duration::from_millis(100)))
            .await
        {
            exited_during_startup(server, backend, "during readiness").await?;
            return Ok(());
        }
        let retry = deadline.min(tokio::time::Instant::now() + Duration::from_millis(50));
        tokio::select! {
            _ = tokio::time::sleep_until(retry) => {}
            _ = server.wake.notified() => {}
        }
    }
}

async fn exited_during_startup<B: ProcessBackend>(
    server: &Server,
    backend: &mut B,
    when: &str,
) -> io::Result<()> {
    match backend.status(server).await {
        Status::Idle | Status::Running => Ok(()),
        // Even a clean exit means the server never became ready.
        Status::Exited { status, .. } => Err(io::Error::other(format!(
            "managed server exited {when}: {status}"
        ))),
        Status::Broken(error) => Err(io::Error::other(error)),
        Status::Unknown => Err(io::Error::new(
            io::ErrorKind::NotConnected,
            format!(
                "node {} became unreachable while the server was starting",
                server.config.node.as_deref().unwrap_or("local")
            ),
        )),
    }
}

async fn stop_process<B: ProcessBackend>(server: &Server, backend: &mut B) -> io::Result<()> {
    lock(&server.state).phase = Phase::Stopping;
    announce(server);
    let result = if backend.owns() {
        backend.stop(server, server.config.stop_timeout).await
    } else {
        Ok(())
    };
    {
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
    }
    // A failed stop may have left the process running.
    if result.is_ok() {
        discard_copy(server, backend).await;
    }
    result
}
