//! The unified plugin system. A plugin is anything that receives Rift events
//! and calls the Rift host API; execution modes differ only in how and where
//! the code runs:
//!
//! - `script`: the sandboxed configuration script (`rift.lua`), position 0.
//!   Code that arrives through the configuration path stays sandboxed.
//! - `persistent`: installed on disk with a `plugin.toml`, trusted, one OS
//!   thread per plugin ([`worker`]), in `rift.plugin()` order.
//! - `remote`: bus responders (`extensions.decisions`), last.
//!
//! [`Host`] owns that chain. Within one decision event the first answer
//! other than `continue` wins, so a deny stops the chain, and a missed
//! deadline or a failure denies unless the plugin's manifest says otherwise:
//! Rift never treats an error as permission to proceed. Observers go to
//! every plugin. Each command has exactly one owner; a name registered twice
//! is a load error.
pub mod api;
#[cfg(test)]
pub(crate) mod conformance;
pub mod lua;
pub mod manifest;
pub mod monitor;
pub(crate) mod remote;
pub mod script;
pub mod services;
#[cfg(feature = "wasm")]
pub mod wasm;
pub mod worker;

use api::{Action, Context, Event, Hook, Reply, Suggestion};
use manifest::Policy;
use services::Base;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    io,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use worker::{Engines, Instance, Outcome, Predecessor, Registrar, Report};

/// Compiled WebAssembly components, under the configuration directory.
pub(crate) const CACHE_DIRECTORY: &str = "local/plugincache";

/// One `rift.plugin(name, config)` entry for a folder with a persistent
/// `plugin.toml`. Two specs describe the same instance when they are equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSpec {
    pub name: String,
    /// `plugins/<name>` under the configuration directory.
    pub folder: PathBuf,
    /// The configuration directory (the `host` filesystem grant).
    pub config_root: PathBuf,
    pub manifest: manifest::Manifest,
    /// The configuration table as JSON; `{}` without one.
    pub config_json: String,
    /// SHA-256 of the manifest and every artifact in the folder except
    /// `data/`, for the reload diff.
    pub fingerprint: String,
    /// `authentication.online_mode`: without it player hooks never run, so
    /// a plugin that registers one is refused at `init`.
    pub online_mode: bool,
    /// `authentication.signed_chat`: `false` means backends see every
    /// player's chat as unsigned, so dropping a message cannot break a chain.
    pub signed_chat: bool,
}

impl PluginSpec {
    /// The plugin's private, writable directory.
    pub fn data_dir(&self) -> PathBuf {
        self.folder.join("data")
    }

    pub fn entry(&self) -> Option<PathBuf> {
        self.manifest
            .entry
            .as_ref()
            .map(|entry| self.folder.join(entry))
    }

    /// A WebAssembly plugin must not be able to write code Rift runs: an
    /// absolute filesystem grant (always read-write) may not contain or lie
    /// inside the compiled component cache or the plugins folder. The `host`
    /// grant is read-only and its own data folder comes with `data`. Lua
    /// plugins run with Rift's privileges, so their grants are not checked.
    pub fn check_grants(&self) -> Result<(), String> {
        if self.manifest.engine != Some(manifest::EngineKind::Wasm) {
            return Ok(());
        }
        let protected = [
            (
                resolve(&self.config_root.join(CACHE_DIRECTORY)),
                "the compiled plugin cache",
            ),
            (
                resolve(self.folder.parent().unwrap_or(&self.folder)),
                "the plugins folder",
            ),
        ];
        for grant in &self.manifest.capabilities.filesystem {
            let manifest::Filesystem::Path(path) = grant else {
                continue;
            };
            let resolved = resolve(path);
            if let Some((_, what)) = protected.iter().find(|(directory, _)| {
                resolved.starts_with(directory) || directory.starts_with(&resolved)
            }) {
                return Err(format!(
                    "capabilities.filesystem: {} overlaps {what}; a WebAssembly plugin may not write code Rift runs (use \"data\" for the plugin's own folder)",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    /// Same code, manifest and folder; only the configuration table may differ.
    /// A change of `online_mode` or `signed_chat` restarts the plugin, so its
    /// registration is checked again.
    fn same_artifact(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && self.manifest == other.manifest
            && self.folder == other.folder
            && self.config_root == other.config_root
            && self.online_mode == other.online_mode
            && self.signed_chat == other.signed_chat
    }
}

/// `path` with symbolic links resolved as far as it exists, so that two
/// spellings of one directory compare equal.
fn resolve(path: &std::path::Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(existing) {
            return rest
                .iter()
                .rev()
                .fold(canonical, |path: PathBuf, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_owned(),
        }
    }
}

/// Who answers a command.
#[derive(Clone)]
pub(crate) enum CommandOwner {
    /// `extensions.commands` / `rift.command` in the configuration script.
    Script,
    Plugin(Arc<Instance>, api::CommandSpec),
}

/// Command names claimed by the configuration script and by running plugins.
#[derive(Default)]
struct Claims {
    script: BTreeSet<String>,
    /// Command name to the instance id and plugin name that claimed it.
    plugins: BTreeMap<String, (u64, String)>,
}

#[derive(Default)]
struct CommandTable(Mutex<Claims>);

impl Registrar for CommandTable {
    fn register(
        &self,
        id: u64,
        plugin: &str,
        replaces: Option<u64>,
        registration: &api::Registration,
    ) -> Result<(), String> {
        let mut claims = self.0.lock().unwrap();
        let mine = |owner: u64| owner == id || Some(owner) == replaces;
        for command in &registration.commands {
            if claims.script.contains(&command.name) {
                return Err(format!(
                    "command /{} is already registered by the configuration script",
                    command.name
                ));
            }
            if let Some((_, owner)) = claims
                .plugins
                .get(&command.name)
                .filter(|(owner, _)| !mine(*owner))
            {
                return Err(format!(
                    "command /{} is already registered by plugin {owner}",
                    command.name
                ));
            }
        }
        claims.plugins.retain(|_, (owner, _)| !mine(*owner));
        for command in &registration.commands {
            claims
                .plugins
                .insert(command.name.clone(), (id, plugin.to_owned()));
        }
        Ok(())
    }

    fn release(&self, id: u64) {
        self.0
            .lock()
            .unwrap()
            .plugins
            .retain(|_, (owner, _)| *owner != id);
    }
}

/// Persistent plugin state shared by every reload generation.
struct Persistent {
    engines: Engines,
    base: Base,
    active: RwLock<Arc<[Arc<Instance>]>>,
    /// The plugin set of the last activation, so an operator restart uses
    /// the configuration as configured even while a hot reload is running.
    specs: RwLock<Arc<[PluginSpec]>>,
    /// Instances that were removed or replaced and may still be stopping,
    /// by name: a plugin added again waits for the old generation.
    stopping: Mutex<BTreeMap<String, Arc<Instance>>>,
    commands: Arc<CommandTable>,
    /// Fans `rift.events.*` out to plugins while any are active.
    events: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Serializes changes to `active`: activation, operator restarts and
    /// shutdown each replace the whole set.
    changes: Mutex<()>,
}

impl Drop for Persistent {
    fn drop(&mut self) {
        if let Some(task) = self.events.get_mut().unwrap().take() {
            task.abort();
        }
    }
}

/// The dispatch chain for one configuration generation. Cheap to clone. The
/// persistent instances behind it are shared with every other generation, so
/// sessions that started under an older configuration see the active set.
#[derive(Clone)]
pub struct Host {
    persistent: Arc<Persistent>,
    /// This generation's plugins, applied by [`Host::activate`].
    specs: Arc<[PluginSpec]>,
    script: Arc<script::ScriptPlugin>,
    remote: remote::Remote,
}

impl Host {
    /// Validate `config.plugins` against the compiled-in engines and the
    /// commands of plugins that stay active. Nothing starts until
    /// [`Host::activate`], which runs only after a reload commits.
    pub(crate) fn new(
        config: &crate::config::Config,
        base: Base,
        previous: Option<&Self>,
        engines: Engines,
    ) -> io::Result<Self> {
        let persistent = match previous {
            Some(previous) => previous.persistent.clone(),
            None => Arc::new(Persistent {
                engines,
                base: base.clone(),
                active: RwLock::new(Arc::from([])),
                specs: RwLock::new(Arc::from([])),
                stopping: Mutex::default(),
                commands: Arc::default(),
                events: Mutex::default(),
                changes: Mutex::default(),
            }),
        };
        for spec in &config.plugins {
            if let Some(engine) = spec.manifest.engine
                && !persistent.engines.available(engine)
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("plugin {}: {}", spec.name, Engines::unavailable(engine)),
                ));
            }
        }
        let script = script::ScriptPlugin::new(config);
        // Plugins that stay configured keep their commands: unchanged and
        // hot-reloaded ones as they are, replaced ones until their new
        // `init` registers. A new script command may not take one of them.
        let active = persistent.active.read().unwrap().clone();
        for instance in active.iter() {
            let configured = config
                .plugins
                .iter()
                .any(|spec| spec.name == instance.name());
            if let Some(registration) = instance.registration().filter(|_| configured)
                && let Some(command) = registration
                    .commands
                    .iter()
                    .find(|command| script.commands().contains(&command.name))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "command /{} is registered by plugin {} and by the configuration script",
                        command.name,
                        instance.name()
                    ),
                ));
            }
        }
        let decisions = config
            .extensions
            .as_ref()
            .map(|extensions| extensions.decisions())
            .cloned()
            .unwrap_or_default();
        Ok(Self {
            remote: remote::Remote::new(
                &decisions,
                persistent.base.broker.clone(),
                previous.map(|previous| &previous.remote),
            ),
            persistent,
            specs: Arc::from(config.plugins.clone()),
            script: Arc::new(script),
        })
    }

    #[cfg(test)]
    pub(crate) fn remote_slots(&self, hook: &str) -> Arc<tokio::sync::Semaphore> {
        self.remote.slots(hook)
    }

    /// Whether this generation configures persistent plugins.
    pub fn has_plugins(&self) -> bool {
        !self.specs.is_empty()
    }

    pub fn script(&self) -> &script::ScriptPlugin {
        &self.script
    }

    /// The active persistent instances in `rift.plugin()` order.
    pub fn active(&self) -> Arc<[Arc<Instance>]> {
        self.persistent.active.read().unwrap().clone()
    }

    pub fn reports(&self) -> Vec<Report> {
        self.active()
            .iter()
            .map(|instance| instance.report())
            .collect()
    }

    /// Log tails, counters and lifecycle notifications, shared by every
    /// generation.
    pub fn monitor(&self) -> &Arc<monitor::Monitor> {
        &self.persistent.base.monitor
    }

    /// Every plugin in dispatch order: the configuration script, the
    /// persistent plugins in `rift.plugin()` order, then the bus responders.
    pub fn statuses(&self) -> Vec<PluginStatus> {
        let mut statuses = Vec::new();
        if !self.script.is_empty() {
            statuses.push(PluginStatus {
                name: SCRIPT.into(),
                mode: "script",
                engine: Some("lua".into()),
                version: None,
                status: "active".into(),
                restarts: 0,
                calls: 0,
                last_error: None,
                hooks: self.script.hooks().map(Hook::name).collect(),
                commands: self.script.commands().iter().cloned().collect(),
                capabilities: Some("sandboxed".into()),
                restartable: false,
            });
        }
        let monitor = self.monitor();
        for instance in self.active().iter() {
            let report = instance.report();
            let counters = monitor.counters(instance.name());
            let registration = instance.registration();
            statuses.push(PluginStatus {
                name: report.name,
                mode: "persistent",
                engine: Some(report.engine),
                version: Some(report.version),
                status: serde_json::to_value(report.status)
                    .ok()
                    .and_then(|status| status.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                restarts: counters.restarts,
                calls: counters.total_calls(),
                last_error: report.last_error,
                hooks: registration.as_ref().map_or_else(Vec::new, |registration| {
                    Hook::ALL
                        .into_iter()
                        .filter(|hook| registration.wants(*hook))
                        .map(Hook::name)
                        .collect()
                }),
                commands: registration.as_ref().map_or_else(Vec::new, |registration| {
                    registration
                        .commands
                        .iter()
                        .map(|command| command.name.clone())
                        .collect()
                }),
                capabilities: Some(instance.spec().manifest.describe_capabilities()),
                restartable: true,
            });
        }
        let hooks: Vec<&'static str> = self
            .remote
            .hooks()
            .filter_map(|hook| Hook::parse(hook).map(Hook::name))
            .collect();
        if !hooks.is_empty() {
            statuses.push(PluginStatus {
                name: REMOTE.into(),
                mode: "remote",
                engine: None,
                version: None,
                status: "active".into(),
                restarts: 0,
                calls: 0,
                last_error: None,
                hooks,
                commands: Vec::new(),
                capabilities: None,
                restartable: false,
            });
        }
        statuses
    }

    /// Stop one persistent plugin and start a fresh instance of it with the
    /// same artifact and current configuration, in the same position. The
    /// new instance is active when this returns and initializes once the old
    /// one has stopped; the future finishes stopping the old instance and
    /// must be driven. `actor` is recorded in the plugin's log and the audit
    /// log.
    pub fn restart(
        &self,
        name: &str,
        actor: &str,
    ) -> io::Result<impl Future<Output = ()> + Send + 'static> {
        let persistent = &self.persistent;
        let _changes = persistent.changes.lock().unwrap();
        let active = persistent.active.read().unwrap().clone();
        let Some(index) = active.iter().position(|instance| instance.name() == name) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                if name == SCRIPT || name == REMOTE {
                    format!("{name} cannot be restarted; reload the configuration instead")
                } else {
                    format!("no persistent plugin named {name:?} is running")
                },
            ));
        };
        let old = active[index].clone();
        // The configured table, not the one the old instance runs with: a
        // hot reload may still be on its way to it.
        let spec = persistent
            .specs
            .read()
            .unwrap()
            .iter()
            .find(|spec| spec.name == name)
            .cloned()
            .unwrap_or_else(|| {
                let mut spec = old.spec().clone();
                spec.config_json = old.config_json();
                spec
            });
        let factory = spec
            .manifest
            .engine
            .and_then(|engine| persistent.engines.get(engine))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("plugin {name}: its engine is not available in this build of Rift"),
                )
            })?;
        let version = spec.manifest.version.clone();
        let instance = Instance::start(
            spec,
            factory,
            persistent.base.clone(),
            persistent.commands.clone() as Arc<dyn Registrar>,
            Some(Predecessor {
                instance: old.clone(),
                inherit: true,
            }),
            None,
        )?;
        let mut next = active.to_vec();
        next[index] = instance;
        *persistent.active.write().unwrap() = Arc::from(next);
        persistent
            .stopping
            .lock()
            .unwrap()
            .insert(name.to_owned(), old.clone());
        persistent
            .base
            .monitor
            .lifecycle(name, "restart_requested", Some(actor), None);
        let body = serde_json::json!({
            "event": "restart_requested",
            "plugin": name,
            "version": version,
            "actor": actor,
        });
        let _ = persistent
            .base
            .broker
            .publish(worker::EVENTS, body.to_string().into());
        Ok(async move { old.shutdown().await })
    }

    /// Attach the retained streams once the runtime opened them.
    pub fn set_streams(&self, streams: Arc<HashMap<String, crate::messaging::Stream>>) {
        *self.persistent.base.streams.write().unwrap() = streams;
    }

    /// Whether any active plugin claims `hook`. A plugin that is new and
    /// still starting has claimed nothing yet; a restarting or replaced one
    /// keeps its predecessor's claims until its own `init` returns.
    pub fn may_handle(&self, hook: Hook) -> bool {
        self.active().iter().any(|instance| {
            instance
                .registration()
                .is_some_and(|registration| registration.wants(hook))
        })
    }

    /// Apply this generation's plugin set: unchanged plugins are untouched; a
    /// changed configuration table hot-reloads (`hot_reload = true`) or
    /// replaces the instance; a changed artifact or manifest replaces it; a
    /// removed plugin stops. The new set is active when this returns; the
    /// future finishes stopping and reloading instances and must be driven
    /// (awaited or spawned). Call only after the configuration committed.
    pub fn activate(&self) -> io::Result<impl Future<Output = ()> + Send + 'static> {
        let persistent = &self.persistent;
        let _changes = persistent.changes.lock().unwrap();
        let previous = persistent.active.read().unwrap().clone();
        // Everything that can fail is checked before anything starts.
        let factories = self
            .specs
            .iter()
            .map(|spec| {
                let engine = spec.manifest.engine.ok_or_else(|| {
                    io::Error::other(format!("plugin {}: not a persistent plugin", spec.name))
                })?;
                persistent.engines.get(engine).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("plugin {}: {}", spec.name, Engines::unavailable(engine)),
                    )
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let script_commands = std::mem::replace(
            &mut persistent.commands.0.lock().unwrap().script,
            self.script.commands().clone(),
        );
        let mut stopping_by_name = persistent.stopping.lock().unwrap();
        stopping_by_name.retain(|_, instance| !instance.has_exited());
        // Before any worker starts: `init` may log straight away.
        persistent
            .base
            .monitor
            .track(self.specs.iter().map(|spec| spec.name.as_str()));
        let mut next = Vec::with_capacity(self.specs.len());
        let mut stopping = Vec::new();
        let mut reloads = Vec::new();
        let mut started: Vec<Arc<Instance>> = Vec::new();
        let mut failure = None;
        for (spec, factory) in self.specs.iter().zip(factories) {
            let old = previous.iter().find(|old| old.name() == spec.name);
            let after = match old {
                Some(old)
                    if old.spec().same_artifact(spec) && old.config_json() == spec.config_json =>
                {
                    next.push(old.clone());
                    continue;
                }
                Some(old)
                    if old.spec().same_artifact(spec)
                        && spec.manifest.hot_reload
                        && old.status() == worker::Status::Running =>
                {
                    reloads.push((old.clone(), spec.clone()));
                    next.push(old.clone());
                    continue;
                }
                // A replacement keeps the old instance's claims until its
                // own `init` returns.
                Some(old) => Some(Predecessor {
                    instance: old.clone(),
                    inherit: true,
                }),
                // Added (again): wait for a generation that is still
                // stopping, but claim nothing it held.
                None => stopping_by_name.get(&spec.name).map(|old| Predecessor {
                    instance: old.clone(),
                    inherit: false,
                }),
            };
            match Instance::start(
                spec.clone(),
                factory,
                persistent.base.clone(),
                persistent.commands.clone() as Arc<dyn Registrar>,
                after,
                started.last().cloned(),
            ) {
                Ok(instance) => {
                    if let Some(old) = old {
                        stopping.push(old.clone());
                    }
                    started.push(instance.clone());
                    next.push(instance);
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = failure {
            // Nothing of this activation takes effect.
            for instance in &started {
                instance.abort();
            }
            persistent.commands.0.lock().unwrap().script = script_commands;
            persistent
                .base
                .monitor
                .retain(previous.iter().map(|instance| instance.name()));
            return Err(error);
        }
        for old in previous.iter() {
            if !self.specs.iter().any(|spec| spec.name == old.name()) {
                stopping.push(old.clone());
            }
        }
        for old in &stopping {
            stopping_by_name.insert(old.name().to_owned(), old.clone());
        }
        drop(stopping_by_name);
        persistent
            .base
            .monitor
            .retain(self.specs.iter().map(|spec| spec.name.as_str()));
        *persistent.active.write().unwrap() = Arc::from(next);
        *persistent.specs.write().unwrap() = self.specs.clone();
        self.watch_events();
        let monitor = persistent.base.monitor.clone();
        Ok(async move {
            let mut work = tokio::task::JoinSet::new();
            for instance in stopping {
                work.spawn(async move { instance.shutdown().await });
            }
            for (instance, spec) in reloads {
                let monitor = monitor.clone();
                work.spawn(async move {
                    match instance.reload(spec.config_json.clone()).await {
                        Ok(()) => {
                            eprintln!("rift: plugin {} reloaded its configuration", spec.name);
                            monitor.lifecycle(&spec.name, "reloaded", None, None);
                        }
                        Err(error) => {
                            eprintln!(
                                "rift: plugin {}: reload failed; it keeps its previous configuration: {error:.2048}",
                                spec.name
                            );
                            monitor.lifecycle(&spec.name, "reload_failed", None, Some(&error));
                        }
                    }
                });
            }
            while work.join_next().await.is_some() {}
            drop(started);
        })
    }

    /// Startup: activate and wait for every plugin's `init`. Any failure or
    /// command collision stops all plugins and fails startup.
    pub async fn start(&self) -> io::Result<()> {
        self.activate()?.await;
        let mut failure = None;
        for instance in self.active().iter() {
            if let Err(error) = instance.ready(Duration::from_secs(1)).await {
                failure = Some(format!("plugin {}: {error}", instance.name()));
                break;
            }
        }
        if let Some(error) = failure {
            self.shutdown().await;
            return Err(io::Error::other(error));
        }
        Ok(())
    }

    /// Stop every plugin within its grace period.
    pub async fn shutdown(&self) {
        let active = {
            let _changes = self.persistent.changes.lock().unwrap();
            std::mem::replace(&mut *self.persistent.active.write().unwrap(), Arc::from([]))
        };
        if let Some(task) = self.persistent.events.lock().unwrap().take() {
            task.abort();
        }
        let mut work = tokio::task::JoinSet::new();
        for instance in active.iter() {
            let instance = instance.clone();
            work.spawn(async move { instance.shutdown().await });
        }
        while work.join_next().await.is_some() {}
    }

    /// One subscription to `rift.events.>` while plugins are active.
    fn watch_events(&self) {
        let mut events = self.persistent.events.lock().unwrap();
        if self.persistent.active.read().unwrap().is_empty() {
            if let Some(task) = events.take() {
                task.abort();
            }
            return;
        }
        if events.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }
        let broker = self.persistent.base.broker.clone();
        let persistent = Arc::downgrade(&self.persistent);
        let Ok(mut subscription) = broker.subscribe("rift.events.>", None) else {
            eprintln!("rift: plugins will not receive rift.events: subscription refused");
            return;
        };
        *events = Some(tokio::spawn(async move {
            loop {
                let message = match subscription.recv().await {
                    Ok(message) => message,
                    Err(crate::messaging::Error::SlowConsumer) => {
                        match broker.subscribe("rift.events.>", None) {
                            Ok(next) => {
                                subscription = next;
                                continue;
                            }
                            Err(_) => break,
                        }
                    }
                    Err(_) => break,
                };
                let Some(persistent) = persistent.upgrade() else {
                    break;
                };
                let event = api::RiftEvent {
                    subject: message.subject.to_string(),
                    payload_json: String::from_utf8_lossy(&message.payload).into_owned(),
                };
                let active = persistent.active.read().unwrap().clone();
                for instance in active.iter() {
                    notify(
                        &persistent.base.monitor,
                        instance,
                        Event::Event(event.clone()),
                    );
                }
            }
        }));
    }

    /// Await one persistent plugin, check its reply with `validate` and
    /// apply its manifest's policies; an invalid reply is a failure of that
    /// plugin. `Ok(None)` skips the plugin (`continue`); `Err` denies. Every
    /// call to a hook the plugin wants is counted for the metrics.
    async fn ask<T>(
        &self,
        instance: &Instance,
        event: Event,
        validate: impl FnOnce(Reply) -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        use monitor::Outcome as Counted;
        let hook = event.hook();
        let manifest = &instance.spec().manifest;
        let deadline = manifest.deadline(hook).unwrap_or(Duration::from_millis(50));
        // The instance answers hooks it did not claim with their default at
        // once; those are not calls.
        let wanted = instance
            .registration()
            .is_some_and(|registration| registration.wants(hook));
        let started = Instant::now();
        let outcome = instance.call(event, deadline).await;
        let elapsed = Some(started.elapsed());
        let monitor = self.monitor();
        let (counted, policy, error) = match outcome {
            Outcome::Reply(reply) => match validate(reply) {
                Ok(value) => {
                    if wanted {
                        monitor.call(instance.name(), hook, Counted::Ok, elapsed);
                    }
                    return Ok(Some(value));
                }
                Err(error) => (
                    Counted::Error,
                    manifest.on_failure(),
                    format!("invalid reply: {error}"),
                ),
            },
            Outcome::TimedOut => (
                Counted::Timeout,
                manifest.on_timeout(hook),
                format!("exceeded {} ms", deadline.as_millis()),
            ),
            Outcome::Failed(error) => (Counted::Error, manifest.on_failure(), error),
        };
        monitor.call(instance.name(), hook, counted, elapsed);
        apply(monitor, instance, hook, policy, error)
    }

    async fn action(
        &self,
        instance: &Instance,
        hook: Hook,
        event: Event,
    ) -> Result<Option<Action>, String> {
        self.ask(instance, event, |reply| match reply {
            Reply::Action(action) => validate_action(hook, &action).map(|()| action),
            other => Err(mismatch(hook, &other)),
        })
        .await
    }

    /// `login`, `initial_server`, `before_transfer`, `kick`: the configuration script
    /// (when it defines the hook), then persistent plugins in order, then the
    /// bus responder. The first answer other than `continue` wins.
    pub(crate) async fn decide(
        &self,
        hook: &str,
        input: &Context,
        script: Option<impl Future<Output = io::Result<Action>>>,
    ) -> io::Result<Action> {
        if let Some(script) = script {
            let action = script.await?;
            if action != Action::Continue {
                return Ok(action);
            }
        }
        if let Some(parsed) = Hook::parse(hook) {
            for instance in self.active().iter() {
                let event = match parsed {
                    Hook::Login => Event::Login(input.clone()),
                    Hook::InitialServer => Event::InitialServer(input.clone()),
                    Hook::BeforeTransfer => Event::BeforeTransfer(input.clone()),
                    Hook::Kick => Event::Kick(input.clone()),
                    _ => break,
                };
                if let Some(action) = self
                    .action(instance, parsed, event)
                    .await
                    .map_err(io::Error::other)?
                    && action != Action::Continue
                {
                    return Ok(action);
                }
            }
        }
        self.remote.decide(hook, input).await
    }

    /// `chat`: the configuration script (when it defines the hook), then the
    /// persistent plugins that handle chat, in order. The first deny wins.
    /// There is no bus responder for chat.
    pub(crate) async fn chat(
        &self,
        input: &Context,
        script: Option<impl Future<Output = io::Result<Action>>>,
    ) -> io::Result<Action> {
        if let Some(script) = script {
            let action = script.await?;
            if action != Action::Continue {
                return Ok(action);
            }
        }
        for instance in self.active().iter() {
            if let Some(action) = self
                .action(instance, Hook::Chat, Event::Chat(input.clone()))
                .await
                .map_err(io::Error::other)?
                && action != Action::Continue
            {
                return Ok(action);
            }
        }
        Ok(Action::Continue)
    }

    /// `join`, `after_transfer`, `disconnect` for every persistent plugin;
    /// fire-and-forget. The script's observer runs separately, as before.
    pub(crate) fn observe(&self, hook: &str, input: &Context) {
        let event = |input: Context| match hook {
            "join" => Some(Event::Join(input)),
            "after_transfer" => Some(Event::AfterTransfer(input)),
            "disconnect" => Some(Event::Disconnect(input)),
            _ => None,
        };
        for instance in self.active().iter() {
            if let Some(event) = event(input.clone()) {
                notify(self.monitor(), instance, event);
            }
        }
    }

    /// Queue a dialog click for the persistent plugin `name`, which showed
    /// the dialog. Dropped when no plugin of that name is active.
    pub(crate) fn dialog(&self, name: &str, input: Context) {
        if let Some(instance) = self
            .active()
            .iter()
            .find(|instance| instance.name() == name)
        {
            notify(self.monitor(), instance, Event::Dialog(input));
        }
    }

    /// The single owner of a command name, if any.
    pub(crate) fn command_owner(&self, name: &str) -> Option<CommandOwner> {
        if self.script.commands().contains(name) {
            return Some(CommandOwner::Script);
        }
        self.active().iter().find_map(|instance| {
            let registration = instance.registration()?;
            let command = registration
                .commands
                .iter()
                .find(|command| command.name == name)?;
            Some(CommandOwner::Plugin(instance.clone(), command.clone()))
        })
    }

    /// Commands registered by persistent plugins, in plugin order.
    pub(crate) fn plugin_commands(&self) -> Vec<api::CommandSpec> {
        self.active()
            .iter()
            .filter_map(|instance| instance.registration())
            .flat_map(|registration| registration.commands.clone())
            .collect()
    }

    /// Run a plugin-owned command. The caller has checked its permission.
    pub(crate) async fn command(&self, instance: &Instance, input: Context) -> io::Result<Action> {
        Ok(self
            .action(instance, Hook::Command, Event::Command(input))
            .await
            .map_err(io::Error::other)?
            .unwrap_or(Action::Continue))
    }

    /// Ask a plugin-owned command's `suggest` export.
    pub(crate) async fn suggest(
        &self,
        instance: &Instance,
        input: Context,
    ) -> io::Result<Vec<Suggestion>> {
        let suggestions = self
            .ask(instance, Event::Suggest(input), |reply| match reply {
                Reply::Suggestions(suggestions) => validate_suggestions(suggestions),
                other => Err(mismatch(Hook::Suggest, &other)),
            })
            .await
            .map_err(io::Error::other)?;
        Ok(suggestions.unwrap_or_default())
    }

    /// `route` for persistent plugins, after the script kept the default.
    pub(crate) async fn route(
        &self,
        connection: &crate::hooks::ConnectionInfo,
    ) -> Result<api::RouteDecision, crate::hooks::RouteError> {
        use crate::hooks::{RouteDecision, RouteError};
        let input = api::Connection::from(connection);
        for instance in self.active().iter() {
            let decision = self
                .ask(instance, Event::Route(input.clone()), |reply| match reply {
                    Reply::Route(decision) => match &decision {
                        RouteDecision::Backend(name) if name.trim().is_empty() => {
                            Err("backend name must not be empty".to_owned())
                        }
                        RouteDecision::Reject {
                            reason: Some(reason),
                        } if reason.len() > 1024 => {
                            Err("reject reason must contain at most 1024 bytes".to_owned())
                        }
                        _ => Ok(decision),
                    },
                    other => Err(mismatch(Hook::Route, &other)),
                })
                .await
                .map_err(RouteError::Script)?;
            if let Some(decision) = decision
                && decision != RouteDecision::Default
            {
                return Ok(decision);
            }
        }
        Ok(RouteDecision::Default)
    }

    /// `status` for persistent plugins, after the script kept the default.
    pub(crate) async fn status(
        &self,
        ping: &crate::server_list::Ping,
    ) -> Result<Option<crate::server_list::StatusOverride>, crate::hooks::RouteError> {
        use crate::hooks::RouteError;
        let input = api::Ping::from(ping);
        for instance in self.active().iter() {
            let changes = self
                .ask(
                    instance,
                    Event::Status(input.clone()),
                    |reply| match reply {
                        Reply::Status(None) => Ok(None),
                        Reply::Status(Some(changes)) => {
                            crate::server_list::status_override_json(changes).map(Some)
                        }
                        other => Err(mismatch(Hook::Status, &other)),
                    },
                )
                .await
                .map_err(RouteError::Script)?;
            if let Some(Some(changes)) = changes {
                return Ok(Some(changes));
            }
        }
        Ok(None)
    }

    /// `http` for persistent plugins, after the script declined (or has no
    /// `on_http`). `None` means nobody claimed the request.
    pub async fn http(
        &self,
        request: &crate::http_script::HttpRequest,
    ) -> Result<Option<api::HttpResponse>, crate::http_script::HttpError> {
        use crate::http_script::HttpError;
        for instance in self.active().iter() {
            let response = self
                .ask(
                    instance,
                    Event::Http(request.clone()),
                    |reply| match reply {
                        Reply::Http(None) => Ok(None),
                        Reply::Http(Some(response)) => validate_response(response).map(Some),
                        other => Err(mismatch(Hook::Http, &other)),
                    },
                )
                .await
                .map_err(HttpError::Script)?;
            if let Some(Some(response)) = response {
                return Ok(Some(response));
            }
        }
        Ok(None)
    }
}

/// The pseudo-plugin names of the configuration script and the bus
/// responders in [`Host::statuses`]; neither is a valid plugin name.
pub const SCRIPT: &str = "(script)";
pub const REMOTE: &str = "(remote)";

/// One entry of [`Host::statuses`], for `rift admin plugins`, the status
/// API, the dashboard and `rift.control.plugins`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PluginStatus {
    pub name: String,
    /// `script`, `persistent` or `remote`.
    pub mode: &'static str,
    pub engine: Option<String>,
    pub version: Option<String>,
    /// `starting`, `running`, `failed` or `stopped`; `active` for the
    /// configuration script and the bus responders.
    pub status: String,
    /// Automatic and operator restarts since Rift started.
    pub restarts: u64,
    /// Calls since Rift started: awaited hooks, and observer events, timer
    /// ticks and bus messages offered to the plugin's queue.
    pub calls: u64,
    pub last_error: Option<String>,
    /// The hooks the plugin receives (all of them for an empty registration).
    pub hooks: Vec<&'static str>,
    pub commands: Vec<String>,
    /// The effective grants, as `rift check` prints them.
    pub capabilities: Option<String>,
    pub restartable: bool,
}

/// Queue an observer event for one plugin and count it.
fn notify(monitor: &monitor::Monitor, instance: &Instance, event: Event) {
    let hook = event.hook();
    if instance
        .registration()
        .is_some_and(|registration| !registration.wants(hook))
    {
        return;
    }
    let outcome = if instance.notify(event) {
        monitor::Outcome::Queued
    } else {
        monitor::Outcome::Dropped
    };
    monitor.call(instance.name(), hook, outcome, None);
}

/// Apply `policy` to a plugin that missed its deadline or failed.
fn apply<T>(
    monitor: &monitor::Monitor,
    instance: &Instance,
    hook: Hook,
    policy: Policy,
    error: String,
) -> Result<Option<T>, String> {
    let message = format!("plugin {}: {}: {error:.2048}", instance.name(), hook.name());
    match policy {
        Policy::Continue => {
            eprintln!("rift: {message}; skipped (continue)");
            monitor.log(
                instance.name(),
                api::Level::Warn,
                format!("{}: {error:.2048}; skipped (continue)", hook.name()),
            );
            Ok(None)
        }
        Policy::Deny => {
            monitor.log(
                instance.name(),
                api::Level::Warn,
                format!("{}: {error:.2048}; denied", hook.name()),
            );
            Err(message)
        }
    }
}

fn mismatch(hook: Hook, reply: &Reply) -> String {
    let kind = match reply {
        Reply::None => "none",
        Reply::Route(_) => "route",
        Reply::Action(_) => "action",
        Reply::Suggestions(_) => "suggestions",
        Reply::Status(_) => "status",
        Reply::Http(_) => "http",
    };
    format!("{} cannot answer with a {kind} reply", hook.name())
}

/// The same per-hook rules as Lua results and bus replies.
fn validate_action(hook: Hook, action: &Action) -> Result<(), String> {
    let allowed = matches!(
        (hook, action),
        (_, Action::Continue)
            | (
                Hook::Login | Hook::BeforeTransfer | Hook::Chat,
                Action::Deny(_)
            )
            | (
                Hook::InitialServer | Hook::Kick,
                Action::Deny(_) | Action::Server(_)
            )
            | (
                Hook::Command,
                Action::Message(_) | Action::Server(_) | Action::Queue(_) | Action::LeaveQueue,
            )
    );
    if !allowed {
        let action: String = format!("{action:?}").chars().take(256).collect();
        return Err(format!("invalid decision for {}: {action}", hook.name()));
    }
    match action {
        Action::Deny(text) => text.validate("deny reason")?,
        Action::Message(text) => text.validate("message")?,
        Action::Server(text) | Action::Queue(text) if text.is_empty() || text.len() > 1024 => {
            return Err("decision string must contain 1..=1024 bytes".into());
        }
        _ => {}
    }
    Ok(())
}

fn validate_suggestions(suggestions: Vec<Suggestion>) -> Result<Vec<Suggestion>, String> {
    if suggestions.len() > 1024 {
        return Err("suggest may return at most 1024 entries".into());
    }
    let valid =
        |text: &str| !text.is_empty() && text.len() <= 256 && !text.chars().any(char::is_control);
    if suggestions.iter().any(|suggestion| {
        !valid(&suggestion.text)
            || matches!(&suggestion.tooltip, Some(crate::text::Text::Plain(tip)) if !valid(tip))
    }) {
        return Err(
            "suggestion text and tooltip must contain 1..=256 bytes without control characters"
                .into(),
        );
    }
    for suggestion in &suggestions {
        if let Some(tooltip @ crate::text::Text::Component(_)) = &suggestion.tooltip {
            tooltip.validate("suggestion tooltip")?;
        }
    }
    Ok(suggestions)
}

fn validate_response(response: api::HttpResponse) -> Result<api::HttpResponse, String> {
    if !(200..=599).contains(&response.status) {
        return Err("HTTP response status must be in 200..=599".into());
    }
    if !crate::http_script::valid_content_type(&response.content_type) {
        return Err("HTTP content_type must be a valid MIME type of at most 256 bytes".into());
    }
    if response.body.len() > crate::http_script::MAX_HTTP_BODY_BYTES {
        return Err("HTTP response body exceeds 256 KiB".into());
    }
    if matches!(response.status, 204 | 205 | 304) && !response.body.is_empty() {
        return Err("HTTP response status requires an empty body".into());
    }
    Ok(response)
}

#[cfg(test)]
mod tests;
