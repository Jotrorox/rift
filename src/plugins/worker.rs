//! The persistent runtime: one OS thread per plugin instance.
//!
//! The host talks to an instance only through its bounded queue. Calls run
//! strictly in order on the instance's thread, so engines are single-threaded
//! and need not be `Send`. Callers enforce deadlines with
//! `tokio::time::timeout`; a slow or stuck plugin stalls only its own queue,
//! and a full queue counts as a missed deadline instead of waiting.
//!
//! Engines are created on that thread by an [`EngineFactory`] registered in
//! [`Engines`]. Adding an engine (`lua`, `wasm`) means implementing [`Engine`]
//! and registering a factory in [`Engines::builtin`]; dispatch never changes.
use super::{
    PluginSpec,
    api::{Event, Registration, Reply, SuggestKind},
    manifest::{EngineKind, Restart},
    services::{Base, Services},
};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    time::{Duration, Instant},
};
use tokio::sync::{oneshot, watch};

/// Undelivered jobs per instance. A full queue makes awaited calls miss their
/// deadline at once and drops observer events.
pub const QUEUE: usize = 1024;
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// An instance that ran this long before failing starts over at the minimum
/// backoff; one that keeps failing sooner backs off up to the maximum.
const STABLE: Duration = Duration::from_secs(60);
/// How long a new instance waits for its predecessor's thread beyond the
/// predecessor's shutdown grace period before it abandons that thread and
/// starts anyway: a thread stuck in code the engine cannot interrupt must
/// not keep the plugin down forever.
const ABANDON_MARGIN: Duration = Duration::from_secs(1);
/// Slack on top of an instance's init deadline (and its predecessor's grace)
/// for creating the engine, such as compiling a WebAssembly component, before
/// later plugins stop waiting for it to claim its commands.
const SETTLE_MARGIN: Duration = Duration::from_secs(30);
pub const EVENTS: &str = "rift.events.plugins";

/// Why a call produced no reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The handler failed; the plugin keeps running. Decision hooks apply
    /// `on_failure`.
    Handler(String),
    /// A trap or an error outside a handler. The instance is marked failed
    /// and restarted according to its manifest.
    Fatal(String),
    /// Host only: the instance is failed, restarting or stopping.
    Unavailable(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Handler(message) | Self::Fatal(message) | Self::Unavailable(message) => {
                write!(f, "{message:.2048}")
            }
        }
    }
}

/// One plugin instance inside an engine. Every method runs on the instance's
/// own thread, never concurrently; host API calls go through the
/// [`Services`] handed to the factory. Long-running guest code should poll
/// [`Services::stop_requested`] where the engine can interrupt it.
pub trait Engine {
    /// `config_json` is the `rift.plugin(name, config)` table as JSON (`{}`
    /// without one). An error marks the instance failed.
    fn init(&mut self, config_json: &str) -> Result<Registration, String>;
    /// Dispatch one handler export.
    fn call(&mut self, event: Event) -> Result<Reply, CallError>;
    /// The configuration table changed and the manifest sets `hot_reload`.
    /// An error keeps the instance running with its previous configuration.
    fn reload(&mut self, config_json: &str) -> Result<(), String>;
    /// The last call before the instance is dropped.
    fn shutdown(&mut self, reason: &str);
}

/// Creates an engine instance on the plugin's thread.
pub trait EngineFactory: Send + Sync + 'static {
    fn create(&self, spec: &PluginSpec, services: Services) -> Result<Box<dyn Engine>, String>;
}

impl<F> EngineFactory for F
where
    F: Fn(&PluginSpec, Services) -> Result<Box<dyn Engine>, String> + Send + Sync + 'static,
{
    fn create(&self, spec: &PluginSpec, services: Services) -> Result<Box<dyn Engine>, String> {
        self(spec, services)
    }
}

/// The engines compiled into this build.
#[derive(Clone, Default)]
pub struct Engines(BTreeMap<EngineKind, Arc<dyn EngineFactory>>);

impl Engines {
    /// The engines this build provides. A manifest naming another engine is
    /// reported by `rift check` and refused at startup and reload.
    pub fn builtin() -> Self {
        let engines = Self::default().with(EngineKind::Lua, super::lua::factory);
        #[cfg(feature = "wasm")]
        let engines = engines.with(EngineKind::Wasm, super::wasm::create);
        engines
    }

    /// Why `kind` cannot run in this build, for `rift check` and startup.
    pub fn unavailable(kind: EngineKind) -> String {
        match kind {
            EngineKind::Wasm if !cfg!(feature = "wasm") => {
                "engine \"wasm\" is not available: this Rift was built without WebAssembly support"
                    .into()
            }
            kind => format!(
                "engine \"{}\" is not available in this build of Rift",
                kind.name()
            ),
        }
    }

    pub fn with(mut self, kind: EngineKind, factory: impl EngineFactory) -> Self {
        self.0.insert(kind, Arc::new(factory));
        self
    }

    pub fn get(&self, kind: EngineKind) -> Option<Arc<dyn EngineFactory>> {
        self.0.get(&kind).cloned()
    }

    pub fn available(&self, kind: EngineKind) -> bool {
        self.0.contains_key(&kind)
    }
}

impl std::fmt::Debug for Engines {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set()
            .entries(self.0.keys().map(|kind| kind.name()))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Starting,
    Running,
    Failed,
    Stopped,
}

/// A point-in-time view of one instance, for `rift.events.plugins`, the
/// status API and tests.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    pub name: String,
    pub version: String,
    pub engine: String,
    pub status: Status,
    pub restarts: u32,
    pub last_error: Option<String>,
    pub calls: u64,
}

/// Command ownership across plugins. The host implements this so that a
/// registration colliding with another plugin's command fails to load.
/// Claims belong to one instance (its [`Instance::id`]), so an old
/// generation that exits late never releases what its successor claimed.
pub(crate) trait Registrar: Send + Sync {
    /// Claim `registration`'s commands for instance `id` of `plugin`,
    /// replacing what that instance and its predecessor `replaces` held.
    fn register(
        &self,
        id: u64,
        plugin: &str,
        replaces: Option<u64>,
        registration: &Registration,
    ) -> Result<(), String>;
    /// Drop every claim of instance `id`.
    fn release(&self, id: u64);
}

/// The instance a new one takes over from: an operator restart, a
/// replacement on reload, or a plugin removed earlier that is still
/// stopping when it is added again.
pub(crate) struct Predecessor {
    pub instance: Arc<Instance>,
    /// Keep the predecessor's registration until the new `init` returns its
    /// own: a restarting or replaced plugin keeps its claims meanwhile. A
    /// plugin added again after it was removed starts with none.
    pub inherit: bool,
}

pub(crate) enum Job {
    Call {
        event: Box<Event>,
        /// `None` for observers: fire-and-forget.
        reply: Option<oneshot::Sender<Result<Reply, CallError>>>,
        /// Skip the call when it is dequeued after its caller gave up.
        deadline: Option<Instant>,
    },
    Reload {
        config_json: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Shutdown,
}

/// A handle that lets host services (subscriptions, timers) feed events into
/// their own plugin's queue.
#[derive(Clone)]
pub(crate) struct Queue(SyncSender<Job>);

impl Queue {
    /// Queue an observer event; false when the queue is full or closed.
    pub(crate) fn notify(&self, event: Event) -> bool {
        self.0
            .try_send(Job::Call {
                event: Box::new(event),
                reply: None,
                deadline: None,
            })
            .is_ok()
    }
}

#[derive(Default)]
struct State {
    restarts: u32,
    last_error: Option<String>,
    calls: u64,
    registration: Option<Arc<Registration>>,
}

pub(crate) struct Shared {
    state: Mutex<State>,
    /// The configuration the instance runs with; a hot reload replaces it
    /// and restarts use it.
    config_json: Mutex<String>,
    status: watch::Sender<Status>,
    /// Set when the shutdown grace period expires; engines may poll it.
    pub(crate) stop: AtomicBool,
    /// Set when shutdown is requested; new calls are refused.
    closing: AtomicBool,
    /// Set once the first `init` attempt finished, successfully or not.
    settled: watch::Sender<bool>,
    /// When later plugins stop waiting for `settled`.
    settle_by: Instant,
    exited: watch::Sender<bool>,
    /// When the shutdown grace period ends; set by the first shutdown.
    abandon_at: OnceLock<Instant>,
}

/// What an awaited call produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Reply(Reply),
    /// The deadline passed or the queue was full: `on_timeout` applies.
    TimedOut,
    /// A handler error, a trap or an unavailable instance: `on_failure`.
    Failed(String),
}

/// A running (or failed, or restarting) persistent plugin instance.
pub struct Instance {
    /// Unique per instance in this process.
    id: u64,
    spec: Arc<PluginSpec>,
    queue: SyncSender<Job>,
    shared: Arc<Shared>,
}

/// `Ok(Err(..))` is an error the code returned; `Err(..)` is a panic.
fn guarded<T>(work: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(work)).map_err(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|message| (*message).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        format!("panicked: {message}")
    })
}

fn valid_command(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        && !matches!(name, "server" | "hub")
}

fn valid_permission(node: &str) -> bool {
    !node.is_empty()
        && node.len() <= 64
        && node.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.' || b == b'-'
        })
}

/// Without `authentication.online_mode` player hooks never run: a plugin
/// that relies on one (a ban list on `login`, say) must not start and
/// silently allow everyone.
pub(crate) fn require_online_mode(registration: &Registration) -> Result<(), String> {
    let needs = if !registration.commands.is_empty() {
        Some("commands need".to_owned())
    } else if registration.hooks.is_empty() {
        Some("an empty hook list (every hook, including login) needs".to_owned())
    } else {
        registration
            .hooks
            .iter()
            .find(|hook| hook.is_player())
            .map(|hook| format!("hook {} needs", hook.name()))
    };
    match needs {
        Some(what) => Err(format!(
            "{what} authentication.online_mode; without it player hooks never run"
        )),
        None => Ok(()),
    }
}

/// The same limits the configuration script's commands obey.
pub(crate) fn validate_registration(registration: &Registration) -> Result<(), String> {
    if registration.commands.len() > 64 {
        return Err("a plugin may register at most 64 commands".into());
    }
    let mut names = BTreeSet::new();
    for command in &registration.commands {
        if !valid_command(&command.name) {
            return Err(format!(
                "command {:?}: names are 1..=64 bytes of a-z0-9_- and server and hub are reserved",
                command.name
            ));
        }
        if !names.insert(command.name.as_str()) {
            return Err(format!("command {:?} registered twice", command.name));
        }
        if !valid_permission(&command.permission) {
            return Err(format!(
                "command {:?}: permission must be a nonempty permission node",
                command.name
            ));
        }
        if let SuggestKind::Fixed(entries) = &command.suggest {
            if entries.len() > 1024 {
                return Err(format!(
                    "command {:?}: at most 1024 fixed suggestions",
                    command.name
                ));
            }
            if entries.iter().any(|entry| {
                entry.is_empty() || entry.len() > 256 || entry.chars().any(char::is_control)
            }) {
                return Err(format!(
                    "command {:?}: suggestions must contain 1..=256 bytes without control characters",
                    command.name
                ));
            }
        }
    }
    Ok(())
}

impl Instance {
    /// Spawn the instance's thread. With `after`, the new instance waits for
    /// that predecessor's thread to exit before creating its engine, so two
    /// generations of one plugin do not run at once; a predecessor still
    /// running past its shutdown grace period (plus [`ABANDON_MARGIN`]) is
    /// abandoned. With `behind`, it claims its commands only after that
    /// instance's first `init` settled (or could no longer be waited for),
    /// so plugins claim commands in `rift.plugin()` order; `init` itself
    /// does not wait.
    pub(crate) fn start(
        spec: PluginSpec,
        factory: Arc<dyn EngineFactory>,
        base: Base,
        registrar: Arc<dyn Registrar>,
        after: Option<Predecessor>,
        behind: Option<Arc<Instance>>,
    ) -> io::Result<Arc<Self>> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| io::Error::other("persistent plugins require a Tokio runtime"))?;
        let (queue, receiver) = sync_channel(QUEUE);
        let now = Instant::now();
        let waits = after.as_ref().map_or(Duration::ZERO, |previous| {
            previous.instance.spec.manifest.shutdown_grace() + ABANDON_MARGIN
        });
        let mut settle_by = now + waits + spec.manifest.init_deadline();
        if let Some(earlier) = &behind {
            settle_by = settle_by.max(earlier.shared.settle_by);
        }
        let state = State {
            registration: after
                .as_ref()
                .filter(|previous| previous.inherit)
                .and_then(|previous| previous.instance.registration()),
            ..State::default()
        };
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            config_json: Mutex::new(spec.config_json.clone()),
            status: watch::channel(Status::Starting).0,
            stop: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            settled: watch::channel(false).0,
            settle_by: settle_by + SETTLE_MARGIN,
            exited: watch::channel(false).0,
            abandon_at: OnceLock::new(),
        });
        let spec = Arc::new(spec);
        let instance = Arc::new(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            spec: spec.clone(),
            queue: queue.clone(),
            shared: shared.clone(),
        });
        let worker = Worker {
            id: instance.id,
            spec,
            factory,
            base,
            registrar,
            shared,
            receiver,
            queue: Queue(queue),
            handle,
            replaces: after.as_ref().map(|previous| previous.instance.id),
            after: after.map(|previous| previous.instance),
            behind: Cell::new(behind),
        };
        std::thread::Builder::new()
            .name(format!("rift-plugin-{:.32}", instance.spec.name))
            .spawn(move || worker.run())?;
        Ok(instance)
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    pub fn spec(&self) -> &PluginSpec {
        &self.spec
    }

    pub fn status(&self) -> Status {
        *self.shared.status.borrow()
    }

    pub fn report(&self) -> Report {
        let state = self.shared.state.lock().unwrap();
        Report {
            name: self.spec.name.clone(),
            version: self.spec.manifest.version.clone(),
            engine: self
                .spec
                .manifest
                .engine
                .map_or("script", EngineKind::name)
                .to_owned(),
            status: self.status(),
            restarts: state.restarts,
            last_error: state.last_error.clone(),
            calls: state.calls,
        }
    }

    /// The configuration JSON the instance currently runs with.
    pub fn config_json(&self) -> String {
        self.shared.config_json.lock().unwrap().clone()
    }

    /// What the last successful `init` registered, or what the predecessor
    /// this instance took over from registered until its own `init` returns.
    /// `None` means the instance has made no claims yet (a plugin that is
    /// new and still starting, or never started): the host does not consult
    /// it for awaited hooks.
    pub fn registration(&self) -> Option<Arc<Registration>> {
        self.shared.state.lock().unwrap().registration.clone()
    }

    fn unavailable(&self) -> Option<String> {
        if self.shared.closing.load(Ordering::Acquire) {
            return Some(format!("plugin {} is stopping", self.spec.name));
        }
        match self.status() {
            Status::Failed => Some(format!(
                "plugin {} is failed: {}",
                self.spec.name,
                self.shared
                    .state
                    .lock()
                    .unwrap()
                    .last_error
                    .as_deref()
                    .unwrap_or("unknown error")
            )),
            Status::Stopped => Some(format!("plugin {} is stopped", self.spec.name)),
            Status::Starting | Status::Running => None,
        }
    }

    /// Await one handler under `deadline`. Hooks the instance's registration
    /// (its own or the inherited one) does not list are answered with their
    /// default at once, even while it is failed: only hooks a plugin claimed
    /// can fail. An instance that has made no claims yet is not asked. While
    /// the instance is starting, calls queue behind `init`.
    pub(crate) async fn call(&self, event: Event, deadline: Duration) -> Outcome {
        let hook = event.hook();
        if !self
            .registration()
            .is_some_and(|registration| registration.wants(hook))
        {
            return Outcome::Reply(Reply::default_for(hook));
        }
        if let Some(reason) = self.unavailable() {
            return Outcome::Failed(reason);
        }
        let (reply, answer) = oneshot::channel();
        let until = Instant::now() + deadline;
        match self.queue.try_send(Job::Call {
            event: Box::new(event),
            reply: Some(reply),
            deadline: Some(until),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Outcome::TimedOut,
            Err(TrySendError::Disconnected(_)) => {
                return Outcome::Failed(format!("plugin {} is stopped", self.spec.name));
            }
        }
        match tokio::time::timeout(deadline, answer).await {
            Err(_) => Outcome::TimedOut,
            // The thread skips a call dequeued after its deadline.
            Ok(Err(_)) if Instant::now() >= until => Outcome::TimedOut,
            Ok(Err(_)) => Outcome::Failed(format!(
                "plugin {} stopped before answering",
                self.spec.name
            )),
            Ok(Ok(Ok(reply))) => Outcome::Reply(reply),
            // An engine that interrupts a guest at its deadline may answer
            // just before this timer fires; the deadline still decides.
            Ok(Ok(Err(_))) if Instant::now() >= until => Outcome::TimedOut,
            Ok(Ok(Err(error))) => Outcome::Failed(error.to_string()),
        }
    }

    /// Queue an observer event. False when the plugin does not want it, is
    /// unavailable, or its queue is full.
    pub(crate) fn notify(&self, event: Event) -> bool {
        if self.unavailable().is_some()
            || self
                .registration()
                .is_some_and(|registration| !registration.wants(event.hook()))
        {
            return false;
        }
        Queue(self.queue.clone()).notify(event)
    }

    /// Hot-reload the configuration within the manifest's reload deadline.
    pub(crate) async fn reload(&self, config_json: String) -> Result<(), String> {
        if let Some(reason) = self.unavailable() {
            return Err(reason);
        }
        let (reply, answer) = oneshot::channel();
        self.queue
            .try_send(Job::Reload {
                config_json: config_json.clone(),
                reply,
            })
            .map_err(|_| format!("plugin {} queue is full", self.spec.name))?;
        let deadline = self.spec.manifest.reload_deadline();
        match tokio::time::timeout(deadline, answer).await {
            Err(_) => Err(format!("reload exceeded {} ms", deadline.as_millis())),
            Ok(Err(_)) => Err(format!(
                "plugin {} stopped before reloading",
                self.spec.name
            )),
            Ok(Ok(result)) => {
                result?;
                *self.shared.config_json.lock().unwrap() = config_json;
                Ok(())
            }
        }
    }

    /// Wait until the instance's thread has finished.
    pub(crate) async fn exited(&self) {
        let mut exited = self.shared.exited.subscribe();
        let _ = exited.wait_for(|exited| *exited).await;
    }

    /// Whether the instance's thread has finished.
    pub(crate) fn has_exited(&self) -> bool {
        *self.shared.exited.borrow()
    }

    /// Stop an instance that was started but never made active, without
    /// waiting: it exits once its predecessor wait ends, before creating an
    /// engine if it had not yet.
    pub(crate) fn abort(&self) {
        self.shared.closing.store(true, Ordering::Release);
        let _ = self.queue.try_send(Job::Shutdown);
    }

    /// Wait for the first `init` to finish (including a predecessor's
    /// shutdown), at most the init deadline plus `extra`.
    pub(crate) async fn ready(&self, extra: Duration) -> Result<(), String> {
        let mut status = self.shared.status.subscribe();
        let deadline = self.spec.manifest.init_deadline() + extra;
        match tokio::time::timeout(
            deadline,
            status.wait_for(|status| *status != Status::Starting),
        )
        .await
        {
            Err(_) => Err(format!(
                "init did not finish within {} ms",
                self.spec.manifest.init_deadline().as_millis()
            )),
            Ok(Err(_)) => Err("plugin thread ended".into()),
            Ok(Ok(status)) => match *status {
                Status::Running => Ok(()),
                _ => Err(self
                    .shared
                    .state
                    .lock()
                    .unwrap()
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "plugin stopped".into())),
            },
        }
    }

    /// Ask the instance to run `shutdown` and wait up to its grace period.
    /// An instance that does not finish in time is told to stop and left to
    /// its thread; it no longer receives events either way.
    pub(crate) async fn shutdown(&self) {
        let grace = self.spec.manifest.shutdown_grace();
        let _ = self.shared.abandon_at.set(Instant::now() + grace);
        self.shared.closing.store(true, Ordering::Release);
        // A full queue is fine: the thread checks `closing` after every job.
        let _ = self.queue.try_send(Job::Shutdown);
        if tokio::time::timeout(grace, self.exited()).await.is_err() {
            self.shared.stop.store(true, Ordering::Release);
            eprintln!(
                "rift: plugin {} did not stop within {} ms; abandoning its thread",
                self.spec.name,
                grace.as_millis()
            );
        }
    }
}

enum Served {
    Shutdown,
    /// The instance is already marked failed.
    Fatal,
}

enum Startup {
    /// Retried according to the restart policy.
    Fatal(String),
    /// An invalid or colliding registration: retrying cannot help.
    Conflict(String),
}

struct Worker {
    id: u64,
    spec: Arc<PluginSpec>,
    factory: Arc<dyn EngineFactory>,
    base: Base,
    registrar: Arc<dyn Registrar>,
    shared: Arc<Shared>,
    receiver: Receiver<Job>,
    queue: Queue,
    handle: tokio::runtime::Handle,
    /// The predecessor's instance id, whose claims this instance takes over.
    replaces: Option<u64>,
    after: Option<Arc<Instance>>,
    /// Taken by the first registration (or the first settled `init`).
    behind: Cell<Option<Arc<Instance>>>,
}

impl Worker {
    fn closing(&self) -> bool {
        self.shared.closing.load(Ordering::Acquire)
    }

    /// Block until `signal` is set or `until` passes; with `interruptible`,
    /// also stop once this instance is closing. True when `signal` is set.
    fn wait_signal(
        &self,
        signal: &watch::Sender<bool>,
        until: Instant,
        interruptible: bool,
    ) -> bool {
        let mut receiver = signal.subscribe();
        self.handle.block_on(async {
            loop {
                if *receiver.borrow() {
                    return true;
                }
                let now = Instant::now();
                if now >= until || (interruptible && self.closing()) {
                    return false;
                }
                let step = (until - now).min(Duration::from_millis(50));
                let _ = tokio::time::timeout(step, receiver.wait_for(|set| *set)).await;
            }
        })
    }

    /// Wait for the predecessor's thread to exit, at most until its shutdown
    /// grace period ended plus [`ABANDON_MARGIN`]; then abandon it.
    fn wait_for_predecessor(&self, previous: &Instance) {
        let grace = previous.spec.manifest.shutdown_grace();
        let until = previous
            .shared
            .abandon_at
            .get()
            .copied()
            .unwrap_or_else(|| Instant::now() + grace)
            + ABANDON_MARGIN;
        if !self.wait_signal(&previous.shared.exited, until, false) && !self.closing() {
            eprintln!(
                "rift: plugin {}: the previous instance did not stop within its {} ms grace period; starting without it",
                self.spec.name,
                grace.as_millis()
            );
            self.base.monitor.log(
                &self.spec.name,
                super::api::Level::Warn,
                format!(
                    "the previous instance did not stop within {} ms; its thread is abandoned",
                    grace.as_millis()
                ),
            );
        }
    }

    /// Wait until the plugin before this one in `rift.plugin()` order made
    /// its claims, at most until it should have.
    fn wait_turn(&self) {
        let Some(earlier) = self.behind.take() else {
            return;
        };
        if !self.wait_signal(&earlier.shared.settled, earlier.shared.settle_by, true)
            && !self.closing()
        {
            eprintln!(
                "rift: plugin {}: plugin {} has not finished starting; claiming commands without waiting for it",
                self.spec.name,
                earlier.name()
            );
        }
    }

    fn publish(&self, event: &str, error: Option<&str>) {
        let restarts = self.shared.state.lock().unwrap().restarts;
        let mut body = serde_json::json!({
            "event": event,
            "plugin": self.spec.name,
            "version": self.spec.manifest.version,
            "restarts": restarts,
        });
        if let Some(error) = error {
            body["error"] = error.chars().take(2048).collect::<String>().into();
        }
        // Ephemeral like every other Rift event: nobody listening is fine.
        let _ = self.base.broker.publish(EVENTS, body.to_string().into());
        self.base
            .monitor
            .lifecycle(&self.spec.name, event, None, error);
    }

    fn fail(&self, error: String) {
        eprintln!("rift: plugin {} failed: {error:.2048}", self.spec.name);
        self.shared.state.lock().unwrap().last_error = Some(error.clone());
        self.shared.status.send_replace(Status::Failed);
        self.publish("failed", Some(&error));
    }

    fn run(mut self) {
        // Drop the predecessor once it exited; it must not stay alive here.
        if let Some(previous) = self.after.take() {
            self.wait_for_predecessor(&previous);
        }
        let mut backoff = MIN_BACKOFF;
        let mut first = true;
        let mut attempted = false;
        while !self.closing() {
            attempted = true;
            self.shared.status.send_replace(Status::Starting);
            let services = Services::new(
                self.spec.clone(),
                self.base.clone(),
                self.queue.clone(),
                self.handle.clone(),
                self.shared.clone(),
            );
            let mut ran = false;
            let mut retry = true;
            let started = self.start_engine(&services);
            // Later plugins claim only after this one did, or failed to.
            self.wait_turn();
            self.shared.settled.send_replace(true);
            match started {
                Ok(mut engine) => {
                    ran = true;
                    let started = Instant::now();
                    self.shared.state.lock().unwrap().last_error = None;
                    // Announced before the status changes, so whoever waits
                    // for the instance to run sees its lifecycle in order.
                    self.publish("started", None);
                    self.shared.status.send_replace(Status::Running);
                    match self.serve(engine.as_mut()) {
                        Served::Shutdown => {
                            let reason = if self.closing() { "stop" } else { "shutdown" };
                            if let Err(panic) = guarded(|| engine.shutdown(reason)) {
                                eprintln!("rift: plugin {} shutdown {panic}", self.spec.name);
                            }
                            let _ = guarded(move || drop(engine));
                            services.close();
                            break;
                        }
                        Served::Fatal => {
                            let _ = guarded(move || drop(engine));
                            services.close();
                            if started.elapsed() >= STABLE {
                                backoff = MIN_BACKOFF;
                            }
                        }
                    }
                }
                Err(Startup::Fatal(error)) => {
                    services.close();
                    self.fail(error);
                }
                Err(Startup::Conflict(error)) => {
                    services.close();
                    self.fail(error);
                    retry = false;
                }
            }
            let restart = retry
                && match self.spec.manifest.restart {
                    Restart::Never => false,
                    Restart::OnFailure => ran || !first,
                    Restart::Always => true,
                };
            first = false;
            if !restart {
                self.park();
                break;
            }
            if !self.wait(backoff) {
                break;
            }
            self.shared.state.lock().unwrap().restarts += 1;
            self.publish("restarting", None);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
        self.registrar.release(self.id);
        if !attempted {
            // Aborted before it ever started: nothing to announce.
            self.shared.status.send_replace(Status::Stopped);
        } else if *self.shared.status.borrow() != Status::Failed || self.closing() {
            self.shared.status.send_replace(Status::Stopped);
            self.publish("stopped", None);
        }
        self.shared.settled.send_replace(true);
        self.shared.exited.send_replace(true);
    }

    fn start_engine(&self, services: &Services) -> Result<Box<dyn Engine>, Startup> {
        std::fs::create_dir_all(self.spec.data_dir()).map_err(|error| {
            Startup::Fatal(format!(
                "cannot create {}: {error}",
                self.spec.data_dir().display()
            ))
        })?;
        let factory = self.factory.clone();
        let mut engine = guarded(|| factory.create(&self.spec, services.clone()))
            .and_then(|result| result)
            .map_err(|error| Startup::Fatal(format!("engine: {error}")))?;
        let started = Instant::now();
        let config = self.shared.config_json.lock().unwrap().clone();
        let registration = guarded(|| engine.init(&config))
            .and_then(|result| result)
            .map_err(|error| Startup::Fatal(format!("init: {error}")));
        let registration = match registration {
            Ok(registration) => registration,
            Err(error) => {
                let _ = guarded(move || drop(engine));
                return Err(error);
            }
        };
        let limit = self.spec.manifest.init_deadline();
        if started.elapsed() > limit {
            let _ = guarded(|| engine.shutdown("init deadline exceeded"));
            return Err(Startup::Fatal(format!(
                "init exceeded {} ms",
                limit.as_millis()
            )));
        }
        self.wait_turn();
        if let Err(error) = validate_registration(&registration)
            .and_then(|()| {
                if self.spec.online_mode {
                    Ok(())
                } else {
                    require_online_mode(&registration)
                }
            })
            .and_then(|()| {
                self.registrar
                    .register(self.id, &self.spec.name, self.replaces, &registration)
            })
        {
            let _ = guarded(|| engine.shutdown("registration rejected"));
            return Err(Startup::Conflict(format!("registration: {error}")));
        }
        self.shared.state.lock().unwrap().registration = Some(Arc::new(registration));
        Ok(engine)
    }

    fn serve(&self, engine: &mut dyn Engine) -> Served {
        loop {
            if self.closing() {
                return Served::Shutdown;
            }
            let Ok(job) = self.receiver.recv() else {
                return Served::Shutdown;
            };
            match job {
                Job::Shutdown => return Served::Shutdown,
                Job::Reload { config_json, reply } => match guarded(|| engine.reload(&config_json))
                {
                    Ok(result) => {
                        let _ = reply.send(result);
                    }
                    Err(panic) => {
                        self.fail(format!("reload {panic}"));
                        let _ = reply.send(Err(panic.clone()));
                        return Served::Fatal;
                    }
                },
                Job::Call {
                    event,
                    reply,
                    deadline,
                } => {
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        continue;
                    }
                    let hook = event.hook();
                    let wanted = self
                        .shared
                        .state
                        .lock()
                        .unwrap()
                        .registration
                        .as_ref()
                        .is_none_or(|registration| registration.wants(hook));
                    if !wanted {
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(Reply::default_for(hook)));
                        }
                        continue;
                    }
                    self.shared.state.lock().unwrap().calls += 1;
                    let result = guarded(|| engine.call(*event))
                        .unwrap_or_else(|panic| Err(CallError::Fatal(panic)));
                    let fatal = match &result {
                        Err(CallError::Fatal(error)) => Some(format!("{}: {error}", hook.name())),
                        Err(CallError::Handler(error)) if reply.is_none() => {
                            eprintln!(
                                "rift: plugin {}: {}: {error:.2048}",
                                self.spec.name,
                                hook.name()
                            );
                            None
                        }
                        _ => None,
                    };
                    // Mark the instance failed before its caller learns why.
                    if let Some(error) = &fatal {
                        self.fail(error.clone());
                    }
                    if let Some(reply) = reply {
                        let _ = reply.send(result);
                    }
                    if fatal.is_some() {
                        return Served::Fatal;
                    }
                }
            }
        }
    }

    fn refuse(&self, job: Job) -> bool {
        let reason = format!("plugin {} is unavailable", self.spec.name);
        match job {
            Job::Shutdown => return false,
            Job::Call {
                reply: Some(reply), ..
            } => {
                let _ = reply.send(Err(CallError::Unavailable(reason)));
            }
            Job::Call { reply: None, .. } => {}
            Job::Reload { reply, .. } => {
                let _ = reply.send(Err(reason));
            }
        }
        true
    }

    /// Failed without a restart: refuse work until shutdown.
    fn park(&self) {
        while !self.closing() {
            match self.receiver.recv() {
                Ok(job) => {
                    if !self.refuse(job) {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }

    /// Back off before a restart, refusing work meanwhile. False on shutdown.
    fn wait(&self, backoff: Duration) -> bool {
        let until = Instant::now() + backoff;
        loop {
            if self.closing() {
                return false;
            }
            let remaining = until.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(job) => {
                    if !self.refuse(job) {
                        return false;
                    }
                }
                Err(RecvTimeoutError::Timeout) => return true,
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }
}
