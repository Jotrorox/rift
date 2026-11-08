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
    time::Duration,
};
use worker::{Engines, Instance, Outcome, Registrar, Report};

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

    /// Same code, manifest and folder; only the configuration table may differ.
    fn same_artifact(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && self.manifest == other.manifest
            && self.folder == other.folder
            && self.config_root == other.config_root
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
    plugins: BTreeMap<String, String>,
}

#[derive(Default)]
struct CommandTable(Mutex<Claims>);

impl Registrar for CommandTable {
    fn register(&self, plugin: &str, registration: &api::Registration) -> Result<(), String> {
        let mut claims = self.0.lock().unwrap();
        for command in &registration.commands {
            if claims.script.contains(&command.name) {
                return Err(format!(
                    "command /{} is already registered by the configuration script",
                    command.name
                ));
            }
            if let Some(owner) = claims
                .plugins
                .get(&command.name)
                .filter(|owner| owner.as_str() != plugin)
            {
                return Err(format!(
                    "command /{} is already registered by plugin {owner}",
                    command.name
                ));
            }
        }
        claims.plugins.retain(|_, owner| owner != plugin);
        for command in &registration.commands {
            claims
                .plugins
                .insert(command.name.clone(), plugin.to_owned());
        }
        Ok(())
    }

    fn release(&self, plugin: &str) {
        self.0
            .lock()
            .unwrap()
            .plugins
            .retain(|_, owner| owner != plugin);
    }
}

/// Persistent plugin state shared by every reload generation.
struct Persistent {
    engines: Engines,
    base: Base,
    active: RwLock<Arc<[Arc<Instance>]>>,
    commands: Arc<CommandTable>,
    /// Fans `rift.events.*` out to plugins while any are active.
    events: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
                commands: Arc::default(),
                events: Mutex::default(),
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
        // Plugins that stay untouched keep their commands; a new script
        // command may not take one of them.
        let active = persistent.active.read().unwrap().clone();
        for instance in active.iter() {
            let unchanged = config
                .plugins
                .iter()
                .any(|spec| spec.name == instance.name() && spec.same_artifact(instance.spec()));
            if let Some(registration) = instance.registration().filter(|_| unchanged)
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

    /// Attach the retained streams once the runtime opened them.
    pub fn set_streams(&self, streams: Arc<HashMap<String, crate::messaging::Stream>>) {
        *self.persistent.base.streams.write().unwrap() = streams;
    }

    /// Whether any active plugin may want `hook`: registered it, or is still
    /// starting so its registration is unknown.
    pub fn may_handle(&self, hook: Hook) -> bool {
        self.active().iter().any(|instance| {
            instance
                .registration()
                .is_none_or(|registration| registration.wants(hook))
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
        let previous = persistent.active.read().unwrap().clone();
        persistent.commands.0.lock().unwrap().script = self.script.commands().clone();
        let mut next = Vec::with_capacity(self.specs.len());
        let mut stopping = Vec::new();
        let mut reloads = Vec::new();
        let mut started = Vec::new();
        let start =
            |spec: &PluginSpec, after: Option<Arc<Instance>>, behind: Option<Arc<Instance>>| {
                let engine = spec.manifest.engine.ok_or_else(|| {
                    io::Error::other(format!("plugin {}: not a persistent plugin", spec.name))
                })?;
                let factory = persistent.engines.get(engine).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("plugin {}: {}", spec.name, Engines::unavailable(engine)),
                    )
                })?;
                Instance::start(
                    spec.clone(),
                    factory,
                    persistent.base.clone(),
                    persistent.commands.clone() as Arc<dyn Registrar>,
                    after,
                    behind,
                )
            };
        for spec in self.specs.iter() {
            let old = previous.iter().find(|old| old.name() == spec.name);
            let instance = match old {
                Some(old)
                    if old.spec().same_artifact(spec) && old.config_json() == spec.config_json =>
                {
                    old.clone()
                }
                Some(old)
                    if old.spec().same_artifact(spec)
                        && spec.manifest.hot_reload
                        && old.status() == worker::Status::Running =>
                {
                    reloads.push((old.clone(), spec.clone()));
                    old.clone()
                }
                Some(old) => {
                    stopping.push(old.clone());
                    let instance = start(spec, Some(old.clone()), started.last().cloned())?;
                    started.push(instance.clone());
                    instance
                }
                None => {
                    let instance = start(spec, None, started.last().cloned())?;
                    started.push(instance.clone());
                    instance
                }
            };
            next.push(instance);
        }
        for old in previous.iter() {
            if !self.specs.iter().any(|spec| spec.name == old.name()) {
                stopping.push(old.clone());
            }
        }
        *persistent.active.write().unwrap() = Arc::from(next);
        self.watch_events();
        Ok(async move {
            let mut work = tokio::task::JoinSet::new();
            for instance in stopping {
                work.spawn(async move { instance.shutdown().await });
            }
            for (instance, spec) in reloads {
                work.spawn(async move {
                    match instance.reload(spec.config_json.clone()).await {
                        Ok(()) => eprintln!("rift: plugin {} reloaded its configuration", spec.name),
                        Err(error) => eprintln!(
                            "rift: plugin {}: reload failed; it keeps its previous configuration: {error:.2048}",
                            spec.name
                        ),
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
        let active =
            std::mem::replace(&mut *self.persistent.active.write().unwrap(), Arc::from([]));
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
                    instance.notify(Event::Event(event.clone()));
                }
            }
        }));
    }

    /// Await one persistent plugin and apply its manifest's policies.
    /// `Ok(None)` skips the plugin (`continue`); `Err` denies.
    async fn ask(&self, instance: &Instance, event: Event) -> Result<Option<Reply>, String> {
        let hook = event.hook();
        let manifest = &instance.spec().manifest;
        let deadline = manifest.deadline(hook).unwrap_or(Duration::from_millis(50));
        match instance.call(event, deadline).await {
            Outcome::Reply(reply) => Ok(Some(reply)),
            Outcome::TimedOut => apply(
                instance,
                hook,
                manifest.on_timeout(hook),
                format!("exceeded {} ms", deadline.as_millis()),
            ),
            Outcome::Failed(error) => apply(instance, hook, manifest.on_failure(), error),
        }
    }

    /// An invalid reply is a failure of that plugin.
    fn invalid<T>(
        &self,
        instance: &Instance,
        hook: Hook,
        error: String,
    ) -> Result<Option<T>, String> {
        apply(
            instance,
            hook,
            instance.spec().manifest.on_failure(),
            format!("invalid reply: {error}"),
        )
    }

    async fn action(
        &self,
        instance: &Instance,
        hook: Hook,
        event: Event,
    ) -> Result<Option<Action>, String> {
        match self.ask(instance, event).await? {
            None => Ok(None),
            Some(Reply::Action(action)) => match validate_action(hook, &action) {
                Ok(()) => Ok(Some(action)),
                Err(error) => self.invalid(instance, hook, error),
            },
            Some(other) => self.invalid(instance, hook, mismatch(hook, &other)),
        }
    }

    /// `login`, `initial_server`, `before_transfer`: the configuration script
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
                instance.notify(event);
            }
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
        let hook = Hook::Suggest;
        let reply = self
            .ask(instance, Event::Suggest(input))
            .await
            .map_err(io::Error::other)?;
        let suggestions = match reply {
            None => return Ok(Vec::new()),
            Some(Reply::Suggestions(suggestions)) => validate_suggestions(suggestions),
            Some(other) => Err(mismatch(hook, &other)),
        };
        match suggestions {
            Ok(suggestions) => Ok(suggestions),
            Err(error) => Ok(self
                .invalid(instance, hook, error)
                .map_err(io::Error::other)?
                .unwrap_or_default()),
        }
    }

    /// `route` for persistent plugins, after the script kept the default.
    pub(crate) async fn route(
        &self,
        connection: &crate::hooks::ConnectionInfo,
    ) -> Result<api::RouteDecision, crate::hooks::RouteError> {
        use crate::hooks::{RouteDecision, RouteError};
        let input = api::Connection::from(connection);
        for instance in self.active().iter() {
            let reply = self
                .ask(instance, Event::Route(input.clone()))
                .await
                .map_err(RouteError::Script)?;
            let decision = match reply {
                None => continue,
                Some(Reply::Route(decision)) => match &decision {
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
                Some(other) => Err(mismatch(Hook::Route, &other)),
            };
            let decision = match decision {
                Ok(decision) => Some(decision),
                Err(error) => self
                    .invalid(instance, Hook::Route, error)
                    .map_err(RouteError::Script)?,
            };
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
            let reply = self
                .ask(instance, Event::Status(input.clone()))
                .await
                .map_err(RouteError::Script)?;
            let changes = match reply {
                None | Some(Reply::Status(None)) => continue,
                Some(Reply::Status(Some(changes))) => {
                    crate::server_list::status_override_json(changes)
                }
                Some(other) => Err(mismatch(Hook::Status, &other)),
            };
            match changes {
                Ok(changes) => return Ok(Some(changes)),
                Err(error) => {
                    if let Some(changes) = self
                        .invalid(instance, Hook::Status, error)
                        .map_err(RouteError::Script)?
                    {
                        return Ok(Some(changes));
                    }
                }
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
            let reply = self
                .ask(instance, Event::Http(request.clone()))
                .await
                .map_err(HttpError::Script)?;
            let response = match reply {
                None | Some(Reply::Http(None)) => continue,
                Some(Reply::Http(Some(response))) => validate_response(response),
                Some(other) => Err(mismatch(Hook::Http, &other)),
            };
            match response {
                Ok(response) => return Ok(Some(response)),
                Err(error) => {
                    if let Some(response) = self
                        .invalid(instance, Hook::Http, error)
                        .map_err(HttpError::Script)?
                    {
                        return Ok(Some(response));
                    }
                }
            }
        }
        Ok(None)
    }
}

/// Apply `policy` to a plugin that missed its deadline or failed.
fn apply<T>(
    instance: &Instance,
    hook: Hook,
    policy: Policy,
    error: String,
) -> Result<Option<T>, String> {
    let message = format!("plugin {}: {}: {error:.2048}", instance.name(), hook.name());
    match policy {
        Policy::Continue => {
            eprintln!("rift: {message}; skipped (continue)");
            Ok(None)
        }
        Policy::Deny => Err(message),
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
            | (Hook::Login | Hook::BeforeTransfer, Action::Deny(_))
            | (Hook::InitialServer, Action::Deny(_) | Action::Server(_))
            | (
                Hook::Command,
                Action::Message(_) | Action::Server(_) | Action::Queue(_) | Action::LeaveQueue,
            )
    );
    if !allowed {
        return Err(format!("invalid decision for {}", hook.name()));
    }
    if let Action::Deny(text) | Action::Server(text) | Action::Message(text) | Action::Queue(text) =
        action
        && (text.is_empty() || text.len() > 1024)
    {
        return Err("decision string must contain 1..=1024 bytes".into());
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
        !valid(&suggestion.text) || suggestion.tooltip.as_deref().is_some_and(|tip| !valid(tip))
    }) {
        return Err(
            "suggestion text and tooltip must contain 1..=256 bytes without control characters"
                .into(),
        );
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
