//! Authenticated extensions. Rust owns durable state, live permissions, scheduled
//! work, admission leases and FIFO tickets; Lua invocations remain isolated.
use crate::{
    auth::AuthenticatedProfile, config::Config, messaging::Broker, protocol::Suggestion, script,
};
use mlua::{Function, Table, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

pub(crate) mod decisions;
mod integrations;
pub(crate) mod permissions;
pub(crate) mod players;
mod scheduler;
pub(crate) mod storage;
pub use players::{PlayerAction, PlayerHandle, Players};
pub use scheduler::Scheduler;

const HOOKS: &[&str] = &[
    "login",
    "initial_server",
    "join",
    "disconnect",
    "before_transfer",
    "after_transfer",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionScript {
    source: script::ScriptSource,
    api_version: u8,
    storage: Option<storage::StorageConfig>,
    integrations: integrations::Integrations,
    jobs: BTreeMap<String, Duration>,
    hooks: BTreeSet<String>,
    commands: BTreeMap<String, Command>,
    permissions: permissions::Policy,
    pub queues: BTreeMap<String, usize>,
    decisions: BTreeMap<String, decisions::Decision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Command {
    permission: String,
    suggest: Suggest,
}

/// How a command's arguments are completed: not at all, from online player
/// names, from the servers the player may join, or by a Lua function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Suggest {
    None,
    Players,
    Servers,
    Lua,
}

/// Suggestions per response after prefix filtering, and entries a `suggest`
/// function may return before filtering.
const MAX_SUGGESTIONS: usize = 100;
const MAX_SUGGESTION_RESULTS: usize = 1024;

fn invalid(message: impl ToString) -> io::Error {
    io::Error::other(message.to_string())
}
pub(crate) fn fields(table: &Table, allowed: &[&str]) -> mlua::Result<()> {
    for pair in table.clone().pairs::<String, Value>() {
        let (key, _) = pair?;
        if !allowed.contains(&key.as_str()) {
            return Err(mlua::Error::runtime(format!(
                "unknown extension field {key}"
            )));
        }
    }
    Ok(())
}
pub(crate) fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.' || b == b'-'
        })
}
impl ExtensionScript {
    /// Player hooks the script defines (`login`, `join`, ...).
    pub fn hook_names(&self) -> impl Iterator<Item = &str> {
        self.hooks.iter().map(String::as_str)
    }

    pub fn command_names(&self) -> impl Iterator<Item = &str> {
        self.commands.keys().map(String::as_str)
    }

    pub fn has_jobs(&self) -> bool {
        !self.jobs.is_empty()
    }

    /// Hooks answered by bus responders (`extensions.decisions`).
    pub fn decision_hooks(&self) -> impl Iterator<Item = &str> {
        self.decisions.keys().map(String::as_str)
    }

    pub(crate) fn decisions(&self) -> &BTreeMap<String, decisions::Decision> {
        &self.decisions
    }

    /// `extensions.groups` with their own permissions and `inherits` lists.
    /// API v1 has no groups.
    pub fn permission_groups(&self) -> Option<serde_json::Value> {
        (self.api_version == 2).then(|| self.permissions.groups_value())
    }

    /// Environment variables the extension configuration reads, as
    /// `(configuration field, variable name)`. Values are never stored.
    pub fn environment(&self) -> Vec<(String, String)> {
        integrations::environment(&self.integrations)
    }

    /// Fail when a variable from [`ExtensionScript::environment`] is unset or
    /// unusable. Messages name the variable, never its value.
    pub fn check_environment(&self) -> Result<(), String> {
        integrations::check_environment(&self.integrations)
    }

    /// Whether a reload can replace `old` with `new`: enablement, API version,
    /// storage path and queue capacities are fixed until a restart.
    pub fn live_compatible(old: Option<&Self>, new: Option<&Self>) -> Result<(), String> {
        match (old, new) {
            (Some(old), Some(new))
                if old.api_version == new.api_version
                    && old.storage == new.storage
                    && old.queues == new.queues =>
            {
                Ok(())
            }
            (None, None) => Ok(()),
            _ => Err(
                "extension enablement, API version, storage path and queue capacities require a restart"
                    .into(),
            ),
        }
    }

    /// Check that the durable store opens, without creating or migrating it,
    /// and describe what was found. Returns `None` for in-memory storage.
    /// While Rift runs it holds the store, so only the path is reported.
    pub fn check_storage(&self, running: bool) -> io::Result<Option<String>> {
        let Some(config) = &self.storage else {
            return Ok(None);
        };
        let path = config.path.display();
        if running {
            return Ok(Some(format!(
                "extension store {path} is held by the running Rift; not checked"
            )));
        }
        Ok(Some(match storage::check_file(&config.path)? {
            storage::Check::Missing => {
                crate::preflight::check_creatable(&config.path)?;
                format!("extension store {path} does not exist yet; startup will create it")
            }
            storage::Check::Snapshot { entries } => format!(
                "extension store {path} is a v1 snapshot with {entries} entries; startup will migrate it"
            ),
            storage::Check::Database { entries } => {
                format!("extension store {path} opens ({entries} entries)")
            }
        }))
    }

    pub(crate) fn parse(root: &Table, source: &script::ScriptSource) -> mlua::Result<Option<Self>> {
        let value: Value = root.raw_get("extensions")?;
        if value.is_nil() {
            return Ok(None);
        }
        let Value::Table(table) = value else {
            return Err(mlua::Error::runtime("extensions must be a table"));
        };
        let api_version = match table.raw_get::<Value>("api_version")? {
            Value::Integer(1) => 1,
            Value::Integer(2) => 2,
            _ => {
                return Err(mlua::Error::runtime(
                    "extensions.api_version must be 1 or 2",
                ));
            }
        };
        fields(
            &table,
            &[
                "api_version",
                "login",
                "initial_server",
                "join",
                "disconnect",
                "before_transfer",
                "after_transfer",
                "commands",
                "permissions",
                "groups",
                "queues",
                "storage",
                "integrations",
                "jobs",
                "decisions",
            ],
        )?;
        if api_version == 1 {
            for name in ["storage", "integrations", "jobs", "groups", "decisions"] {
                if !table.raw_get::<Value>(name)?.is_nil() {
                    return Err(mlua::Error::runtime(format!(
                        "extensions.{name} requires api_version = 2"
                    )));
                }
            }
        }
        let mut storage = storage::parse(table.raw_get("storage")?)?;
        if let Some(storage) = &mut storage
            && storage.path.is_relative()
        {
            storage.path = source.root.join(&storage.path);
        }
        let integrations = integrations::parse(table.raw_get("integrations")?)?;
        let jobs = scheduler::parse(table.raw_get("jobs")?)?;
        let decisions = decisions::parse(table.raw_get("decisions")?)?;
        let mut hooks = BTreeSet::new();
        for hook in HOOKS {
            match table.raw_get::<Value>(*hook)? {
                Value::Nil => {}
                Value::Function(_) => {
                    hooks.insert((*hook).to_owned());
                }
                _ => {
                    return Err(mlua::Error::runtime(format!(
                        "extensions.{hook} must be a function"
                    )));
                }
            }
        }
        let mut commands = BTreeMap::new();
        if let Some(entries) = table.raw_get::<Option<Table>>("commands")? {
            for entry in entries.pairs::<String, Table>() {
                let (name, command) = entry?;
                fields(&command, &["permission", "run", "suggest"])?;
                if !token(&name)
                    || name.contains('.')
                    || matches!(name.as_str(), "server" | "hub")
                    || commands.len() >= 64
                {
                    return Err(mlua::Error::runtime(
                        "invalid, reserved, or excessive extension command name",
                    ));
                }
                let permission: String = command.raw_get("permission")?;
                if !token(&permission) {
                    return Err(mlua::Error::runtime(
                        "command permission must be a nonempty permission node",
                    ));
                }
                command.raw_get::<Function>("run")?;
                let suggest = match command.raw_get::<Value>("suggest")? {
                    Value::Nil => Suggest::None,
                    Value::Function(_) => Suggest::Lua,
                    Value::String(kind) if kind.as_bytes().as_ref() == b"players" => {
                        Suggest::Players
                    }
                    Value::String(kind) if kind.as_bytes().as_ref() == b"servers" => {
                        Suggest::Servers
                    }
                    _ => {
                        return Err(mlua::Error::runtime(format!(
                            "commands.{name}.suggest must be a function, \"players\" or \"servers\""
                        )));
                    }
                };
                commands.insert(
                    name,
                    Command {
                        permission,
                        suggest,
                    },
                );
            }
        }
        let permissions = permissions::Policy::parse(&table, api_version)?;
        let mut queues = BTreeMap::new();
        if let Some(entries) = table.raw_get::<Option<Table>>("queues")? {
            for entry in entries.pairs::<String, usize>() {
                let (backend, capacity) = entry?;
                if !(1..=100_000).contains(&capacity) {
                    return Err(mlua::Error::runtime("queue capacity must be 1..=100000"));
                }
                queues.insert(backend, capacity);
            }
        }
        Ok(Some(Self {
            source: source.clone(),
            api_version,
            storage,
            integrations,
            jobs,
            hooks,
            commands,
            permissions,
            queues,
            decisions,
        }))
    }
    fn evaluate(
        &self,
        hook: &str,
        input: &Context,
        deadline: Instant,
        runtime: &Extensions,
    ) -> io::Result<Action> {
        self.run(
            hook,
            input,
            deadline,
            runtime,
            || Action::Message("You do not have permission to use this command.".into()),
            parse_action,
        )
    }

    /// Run one callback. `denied` answers a command or suggestion whose
    /// permission was revoked after the host's first check.
    fn run<T>(
        &self,
        hook: &str,
        input: &Context,
        deadline: Instant,
        runtime: &Extensions,
        denied: impl FnOnce() -> T,
        parse: impl FnOnce(&str, Value) -> mlua::Result<T>,
    ) -> io::Result<T> {
        let run = || -> mlua::Result<T> {
            let (lua, root) = script::load(&self.source, deadline)?;
            script::install_messaging(&lua, Some(runtime.broker.clone()), deadline)?;
            if self.api_version == 2 {
                let api: Table = lua.named_registry_value("rift.runtime")?;
                runtime.store.install(&lua, &api, deadline)?;
                runtime.permissions.install(&lua, &api, deadline)?;
                runtime.players.install(&lua, &api, deadline)?;
                integrations::install(&lua, &api, &self.integrations, deadline)?;
            }
            let Value::Table(root) = root else {
                return Err(mlua::Error::runtime("configuration must return a table"));
            };
            let extensions: Table = root.raw_get("extensions")?;
            let callback: Function = if hook == "command" || hook == "suggest" {
                extensions
                    .raw_get::<Table>("commands")?
                    .raw_get::<Table>(input.command.as_deref().unwrap())?
                    .raw_get(if hook == "command" { "run" } else { "suggest" })?
            } else if hook == "job" {
                extensions
                    .raw_get::<Table>("jobs")?
                    .raw_get::<Table>(input.command.as_deref().unwrap())?
                    .raw_get("run")?
            } else {
                extensions.raw_get(hook)?
            };
            // Recheck after worker scheduling/source evaluation, before executing user code.
            if (hook == "command" || hook == "suggest")
                && !runtime.permits(
                    &input.uuid,
                    &self.commands[input.command.as_deref().unwrap()].permission,
                )
            {
                return Ok(denied());
            }
            let ctx = lua.create_table()?;
            ctx.set("api_version", self.api_version)?;
            if hook == "job" {
                ctx.set("job", input.command.as_deref())?;
                let value: Value = callback.call(ctx)?;
                script::check_deadline(deadline)?;
                return parse(hook, value);
            }
            ctx.set("connection_id", input.id.to_string())?;
            ctx.set("uuid", input.uuid.as_str())?;
            ctx.set("name", input.name.as_str())?;
            ctx.set("authenticated", true)?;
            ctx.set("listener", input.listener.as_str())?;
            ctx.set("hostname", input.hostname.as_str())?;
            ctx.set("peer_ip", input.peer_ip.as_str())?;
            ctx.set("protocol", input.protocol)?;
            ctx.set("server", input.server.as_deref())?;
            ctx.set("default_server", input.default_server.as_deref())?;
            ctx.set("target", input.target.as_deref())?;
            ctx.set("reason", input.reason.as_deref())?;
            ctx.set("success", input.success)?;
            ctx.set("command", input.command.as_deref())?;
            ctx.set("args", input.args.as_str())?;
            if hook == "suggest" {
                ctx.set("word", input.word.as_str())?;
                ctx.set("position", input.position)?;
            }
            let permissions = runtime.clone();
            let uuid = input.uuid.clone();
            ctx.set(
                "has_permission",
                lua.create_function(move |_, node: String| {
                    script::check_deadline(deadline)?;
                    Ok(permissions.permits(&uuid, &node))
                })?,
            )?;
            let value: Value = callback.call(ctx)?;
            script::check_deadline(deadline)?;
            parse(hook, value)
        };
        run().map_err(|error| invalid(format!("extensions.{hook}: {error:.2048}")))
    }
}

/// A decision hook's answer. Serialized like a Lua result table:
/// `"continue"`, `{"deny":"reason"}`, `"leave_queue"`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Continue,
    Deny(String),
    Server(String),
    Message(String),
    Queue(String),
    LeaveQueue,
}
pub(crate) fn parse_action(hook: &str, value: Value) -> mlua::Result<Action> {
    if value.is_nil() {
        return Ok(Action::Continue);
    }
    let Value::Table(table) = value else {
        return Err(mlua::Error::runtime(
            "extension result must be nil or a decision table",
        ));
    };
    let entries = table
        .pairs::<String, Value>()
        .collect::<mlua::Result<Vec<_>>>()?;
    if entries.len() != 1 {
        return Err(mlua::Error::runtime(
            "extension result requires exactly one field",
        ));
    }
    let (key, value) = &entries[0];
    let allowed = match hook {
        "login" | "before_transfer" => &["deny"][..],
        "initial_server" => &["deny", "server"][..],
        "command" => &["message", "server", "queue", "leave_queue"][..],
        _ => &[][..],
    };
    if !allowed.contains(&key.as_str()) {
        return Err(mlua::Error::runtime("invalid decision for this hook"));
    }
    if key == "leave_queue" && *value == Value::Boolean(true) {
        return Ok(Action::LeaveQueue);
    }
    let Value::String(value) = value else {
        return Err(mlua::Error::runtime("decision requires a string"));
    };
    let value = value.to_str()?.to_owned();
    if value.is_empty() || value.len() > 1024 {
        return Err(mlua::Error::runtime(
            "decision string must contain 1..=1024 bytes",
        ));
    }
    Ok(match key.as_str() {
        "deny" => Action::Deny(value),
        "server" => Action::Server(value),
        "message" => Action::Message(value),
        "queue" => Action::Queue(value),
        _ => return Err(mlua::Error::runtime("invalid decision")),
    })
}

/// A `suggest` result: an array of strings or `{ text = ..., tooltip = ... }`.
pub(crate) fn parse_suggestions(_hook: &str, value: Value) -> mlua::Result<Vec<Suggestion>> {
    let wrong = || {
        mlua::Error::runtime(
            "suggest must return an array of strings or { text = ..., tooltip = ... } tables",
        )
    };
    let text = |value: Value, field: &str| -> mlua::Result<String> {
        let Value::String(value) = value else {
            return Err(wrong());
        };
        let value = value.to_str()?.to_owned();
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(mlua::Error::runtime(format!(
                "suggestion {field} must contain 1..=256 bytes without control characters"
            )));
        }
        Ok(value)
    };
    let Value::Table(table) = value else {
        return if value.is_nil() {
            Ok(Vec::new())
        } else {
            Err(wrong())
        };
    };
    let length = table.raw_len();
    if length > MAX_SUGGESTION_RESULTS {
        return Err(mlua::Error::runtime(format!(
            "suggest may return at most {MAX_SUGGESTION_RESULTS} entries"
        )));
    }
    if table.clone().pairs::<Value, Value>().count() != length {
        return Err(wrong());
    }
    let mut suggestions = Vec::with_capacity(length);
    for entry in table.sequence_values::<Value>() {
        suggestions.push(match entry? {
            Value::Table(entry) => {
                fields(&entry, &["text", "tooltip"])?;
                Suggestion {
                    text: text(entry.raw_get("text")?, "text")?,
                    tooltip: match entry.raw_get::<Value>("tooltip")? {
                        Value::Nil => None,
                        tooltip => Some(text(tooltip, "tooltip")?),
                    },
                }
            }
            entry => Suggestion {
                text: text(entry, "text")?,
                tooltip: None,
            },
        });
    }
    Ok(suggestions)
}

/// Suggestions for `word` from plain names, sorted ignoring ASCII case.
pub(crate) fn complete_names(mut names: Vec<String>, word: &str) -> Vec<Suggestion> {
    names.sort_by_key(|name| name.to_ascii_lowercase());
    let suggestions = names
        .into_iter()
        .map(|text| Suggestion {
            text,
            tooltip: None,
        })
        .collect();
    filter_suggestions(suggestions, word)
}

/// Keep suggestions that extend `word`, ignoring ASCII case, without
/// duplicates and at most [`MAX_SUGGESTIONS`].
fn filter_suggestions(suggestions: Vec<Suggestion>, word: &str) -> Vec<Suggestion> {
    let mut seen = BTreeSet::new();
    suggestions
        .into_iter()
        .filter(|suggestion| {
            suggestion
                .text
                .get(..word.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(word))
                && seen.insert(suggestion.text.clone())
        })
        .take(MAX_SUGGESTIONS)
        .collect()
}

/// The session a player hook is about; the serde form uses the WIT field
/// names (`connection_id`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Context {
    #[serde(rename = "connection_id")]
    pub id: u64,
    pub uuid: String,
    pub name: String,
    pub listener: String,
    pub hostname: String,
    pub peer_ip: String,
    pub protocol: i32,
    pub server: Option<String>,
    pub default_server: Option<String>,
    pub target: Option<String>,
    pub reason: Option<String>,
    pub success: Option<bool>,
    pub command: Option<String>,
    pub args: String,
    /// The argument being completed and its 1-based position, for `suggest`.
    pub word: String,
    pub position: usize,
}
impl Context {
    pub fn authenticated(id: u64, profile: &AuthenticatedProfile) -> Self {
        Self {
            id,
            uuid: profile.uuid.iter().map(|b| format!("{b:02x}")).collect(),
            name: profile.name.clone(),
            ..Self::default()
        }
    }
}
#[derive(Default)]
struct HostState {
    leases: BTreeMap<u64, BTreeSet<String>>,
    queues: BTreeMap<String, VecDeque<(u64, Instant)>>,
}
#[derive(Clone)]
pub struct Extensions {
    script: Option<ExtensionScript>,
    backends: Arc<Mutex<BTreeSet<String>>>,
    groups: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
    /// Workers for player callbacks. Scheduled jobs, which may wait seconds
    /// on HTTP calls, use `job_slots` and never hold these.
    slots: Arc<Semaphore>,
    job_slots: Arc<Semaphore>,
    host: Arc<Mutex<HostState>>,
    broker: Broker,
    store: storage::Store,
    permissions: permissions::Permissions,
    players: Players,
    job_gates: Arc<Mutex<BTreeMap<String, Weak<Semaphore>>>>,
    /// The dispatch chain: this script, persistent plugins, bus responders.
    plugins: crate::plugins::Host,
}
impl Extensions {
    /// Live FIFO pressure, excluding expired tickets even while their session
    /// is busy elsewhere. Scaling must not keep capacity for stale demand.
    pub fn queue_lengths(&self) -> BTreeMap<String, usize> {
        let host = self.host.lock().unwrap();
        let now = Instant::now();
        host.queues
            .iter()
            .map(|(name, queue)| {
                let live = queue
                    .iter()
                    .filter(|(_, entered)| now.duration_since(*entered) < Duration::from_secs(300))
                    .count();
                (name.clone(), live)
            })
            .collect()
    }

    pub fn new(config: &Config, broker: Broker, previous: Option<&Self>) -> io::Result<Self> {
        Self::with_engines(
            config,
            broker,
            previous,
            crate::plugins::worker::Engines::builtin(),
        )
    }

    /// Like [`Extensions::new`] with an explicit engine set. `engines` only
    /// matters for the first generation; reloads keep the running set.
    pub fn with_engines(
        config: &Config,
        broker: Broker,
        previous: Option<&Self>,
        engines: crate::plugins::worker::Engines,
    ) -> io::Result<Self> {
        if let Some(previous) = previous {
            ExtensionScript::live_compatible(previous.script.as_ref(), config.extensions.as_ref())
                .map_err(invalid)?;
        }
        if let Some(script) = &config.extensions {
            script
                .check_environment()
                .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
        }
        let store = match previous {
            Some(previous) => previous.store.clone(),
            None => storage::Store::open(
                config
                    .extensions
                    .as_ref()
                    .and_then(|script| script.storage.as_ref()),
            )?,
        };
        let permissions = match previous {
            Some(previous) => previous.permissions.clone(),
            None => permissions::Permissions::open(store.clone())?,
        };
        let backends = previous.map_or_else(
            || {
                Arc::new(Mutex::new(
                    config
                        .backends
                        .keys()
                        .chain(config.service_groups.keys())
                        .cloned()
                        .collect(),
                ))
            },
            |previous| previous.backends.clone(),
        );
        let groups = previous.map_or_else(
            || Arc::new(Mutex::new(group_members(config))),
            |previous| previous.groups.clone(),
        );
        let players = previous.map_or_else(
            || Players::new(backends.clone(), groups.clone(), broker.clone()),
            |previous| previous.players.clone(),
        );
        let plugins = crate::plugins::Host::new(
            config,
            crate::plugins::services::Base {
                players: players.clone(),
                store: store.clone(),
                permissions: permissions.clone(),
                broker: broker.clone(),
                streams: Arc::default(),
            },
            previous.map(|previous| &previous.plugins),
            engines,
        )?;
        let runtime = Self {
            script: config.extensions.clone(),
            plugins,
            players,
            backends,
            groups,
            broker,
            store,
            permissions,
            job_gates: previous.map_or_else(
                || Arc::new(Mutex::new(BTreeMap::new())),
                |p| p.job_gates.clone(),
            ),
            slots: previous.map_or_else(
                || Arc::new(Semaphore::new(script::MAX_CONCURRENT)),
                |p| p.slots.clone(),
            ),
            job_slots: previous.map_or_else(
                || Arc::new(Semaphore::new(scheduler::MAX_CONCURRENT_JOBS)),
                |p| p.job_slots.clone(),
            ),
            host: previous.map_or_else(
                || Arc::new(Mutex::new(HostState::default())),
                |p| p.host.clone(),
            ),
        };
        if previous.is_none() {
            runtime.activate();
        }
        Ok(runtime)
    }
    /// Publish the live destination catalog after a runtime transaction commits.
    /// Existing sessions share this catalog with newly configured extensions.
    pub fn update_destinations(&self, config: &Config) {
        // Publish capacity membership before callbacks can select new names.
        *self.groups.lock().unwrap() = group_members(config);
        *self.backends.lock().unwrap_or_else(|e| e.into_inner()) = config
            .backends
            .keys()
            .chain(config.service_groups.keys())
            .cloned()
            .collect();
    }

    /// The plugin dispatch chain for this generation.
    pub fn plugins(&self) -> &crate::plugins::Host {
        &self.plugins
    }

    /// The process-wide directory that sessions join, with or without extensions.
    pub fn players(&self) -> &Players {
        &self.players
    }

    /// The directory for Lua outside extension callbacks; only API v2 exposes it.
    pub fn player_api(&self) -> Option<Players> {
        self.script
            .as_ref()
            .filter(|script| script.api_version == 2)
            .map(|_| self.players.clone())
    }

    /// Durable permission group memberships (UUID -> groups), or `None`
    /// without API v2 extensions.
    pub fn group_memberships(&self) -> Option<BTreeMap<String, BTreeSet<String>>> {
        self.player_api()?;
        Some(self.permissions.memberships())
    }

    /// Whether a player belongs to a permission group, directly, through
    /// `default` or by inheritance; `None` without API v2 extensions.
    pub fn in_group(&self, uuid: &str, group: &str) -> Option<bool> {
        self.player_api()?;
        Some(self.permissions.in_group(uuid, group))
    }

    /// Add a player to a permission group or remove them from it, as
    /// `rift.permissions.add_group` and `remove_group` do. This writes to the
    /// extension store, so call it off the async runtime.
    pub fn change_group_membership(&self, uuid: &str, group: &str, add: bool) -> io::Result<bool> {
        if self.player_api().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "permission groups require extensions with api_version = 2",
            ));
        }
        self.permissions
            .change_group(uuid, group, add, Instant::now() + Duration::from_secs(5))
    }

    /// Publish the permission baseline only after the configuration transaction commits.
    pub fn activate(&self) {
        if let Some(script) = &self.script
            && script.api_version == 2
        {
            self.permissions.replace(&script.permissions);
        }
    }
    /// API v1 checks its static grants; API v2 and persistent plugins
    /// without extensions use live permissions.
    fn permits(&self, uuid: &str, node: &str) -> bool {
        match &self.script {
            Some(script) if script.api_version == 1 => script.permissions.grants(uuid, node),
            Some(_) => self.permissions.has(uuid, node),
            None => self.plugins.has_plugins() && self.permissions.has(uuid, node),
        }
    }

    /// Whether authenticated sessions get player hooks: with extensions or
    /// with persistent plugins.
    pub fn enabled(&self) -> bool {
        self.script.is_some() || self.plugins.has_plugins()
    }

    pub fn session(&self, context: Context) -> Option<ExtensionSession> {
        if !self.enabled() {
            return None;
        }
        Some(ExtensionSession {
            runtime: self.clone(),
            context,
            pending: None,
            joined: false,
            jobs: Arc::new(Semaphore::new(1)),
        })
    }
    /// The plugin chain: the Lua hook first because it is cheap and local,
    /// then persistent plugins, then a configured bus decision. Script
    /// commands go to the script only; observers also reach every
    /// persistent plugin.
    async fn call(&self, hook: &str, input: &Context, jobs: &Arc<Semaphore>) -> io::Result<Action> {
        let script = self
            .script
            .as_ref()
            .filter(|script| hook == "command" || script.hooks.contains(hook))
            .map(|_| self.evaluate(hook, input, jobs));
        let action = match hook {
            "command" => match script {
                Some(script) => script.await?,
                None => return Err(invalid("unknown extension command")),
            },
            "join" | "after_transfer" | "disconnect" => {
                let result = match script {
                    Some(script) => script.await,
                    None => Ok(Action::Continue),
                };
                self.plugins.observe(hook, input);
                result?
            }
            _ => self.plugins.decide(hook, input, script).await?,
        };
        self.known_destination(action)
    }

    /// Plugins may only select configured backends and service groups.
    fn known_destination(&self, action: Action) -> io::Result<Action> {
        if let Action::Server(server) | Action::Queue(server) = &action
            && !self
                .backends
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(server)
        {
            return Err(invalid("extension selected an unknown backend"));
        }
        Ok(action)
    }
    async fn evaluate(
        &self,
        hook: &str,
        input: &Context,
        jobs: &Arc<Semaphore>,
    ) -> io::Result<Action> {
        let hook = hook.to_owned();
        self.spawn(input, jobs, move |script, input, deadline, runtime| {
            script.evaluate(&hook, input, deadline, runtime)
        })
        .await
    }

    /// Run `work` on a blocking worker under the session's single callback
    /// slot and the process-wide extension capacity, within the Lua deadline.
    /// A full pool is waited on briefly before the callback fails.
    async fn spawn<T: Send + 'static>(
        &self,
        input: &Context,
        jobs: &Arc<Semaphore>,
        work: impl FnOnce(&ExtensionScript, &Context, Instant, &Extensions) -> io::Result<T>
        + Send
        + 'static,
    ) -> io::Result<T> {
        let session_job = jobs
            .clone()
            .try_acquire_owned()
            .map_err(|_| invalid("previous session callback is still running"))?;
        let permit = script::acquire_slot(&self.slots)
            .await
            .ok_or_else(|| invalid("extension capacity exhausted"))?;
        let script = self.script.clone().unwrap();
        let input = input.clone();
        let runtime = self.clone();
        let deadline = Instant::now() + script::EXECUTION_TIMEOUT;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _session_job = session_job;
            work(&script, &input, deadline, &runtime)
        });
        tokio::time::timeout(script::EXECUTION_TIMEOUT, task)
            .await
            .map_err(invalid)?
            .map_err(invalid)?
    }
}
pub struct ExtensionSession {
    runtime: Extensions,
    pub context: Context,
    pending: Option<Context>,
    joined: bool,
    jobs: Arc<Semaphore>,
}
impl ExtensionSession {
    fn script_commands(&self) -> impl Iterator<Item = (&String, &Command)> {
        self.runtime
            .script
            .iter()
            .flat_map(|script| script.commands.iter())
    }

    /// Queue capacities by destination; none without extensions.
    fn queues(&self) -> &BTreeMap<String, usize> {
        static NONE: BTreeMap<String, usize> = BTreeMap::new();
        self.runtime
            .script
            .as_ref()
            .map_or(&NONE, |script| &script.queues)
    }

    /// Every command Rift answers: the script's, then persistent plugins'.
    pub fn commands(&self) -> Vec<String> {
        self.script_commands()
            .map(|(name, _)| name.clone())
            .chain(
                self.runtime
                    .plugins
                    .plugin_commands()
                    .into_iter()
                    .map(|command| command.name),
            )
            .collect()
    }
    /// Commands for this player's command tree with whether their arguments
    /// ask for suggestions. Commands the player lacks permission for are left
    /// out; the tree is rebuilt whenever a backend sends one.
    pub fn visible_commands(&self) -> Vec<(String, bool)> {
        let uuid = &self.context.uuid;
        self.script_commands()
            .filter(|(_, command)| self.runtime.permits(uuid, &command.permission))
            .map(|(name, command)| (name.clone(), command.suggest != Suggest::None))
            .chain(
                self.runtime
                    .plugins
                    .plugin_commands()
                    .into_iter()
                    .filter(|command| self.runtime.permits(uuid, &command.permission))
                    .map(|command| {
                        let suggests = command.suggest != crate::plugins::api::SuggestKind::None;
                        (command.name, suggests)
                    }),
            )
            .collect()
    }

    /// Suggestions for the last word of a command's `args` (the text after
    /// the command name and one space). Players without the command's
    /// permission, and commands without `suggest`, get none. `servers` lists
    /// the destinations this player may join.
    pub async fn suggest(
        &self,
        name: &str,
        args: &str,
        servers: impl FnOnce() -> Vec<String>,
    ) -> io::Result<Vec<Suggestion>> {
        use crate::plugins::{CommandOwner, api::SuggestKind};
        let Some(owner) = self.runtime.plugins.command_owner(name) else {
            return Ok(Vec::new());
        };
        let command = match owner {
            CommandOwner::Script => match self.script_commands().find(|(key, _)| *key == name) {
                Some((_, command)) => command,
                None => return Ok(Vec::new()),
            },
            CommandOwner::Plugin(instance, command) => {
                if !self
                    .runtime
                    .permits(&self.context.uuid, &command.permission)
                {
                    return Ok(Vec::new());
                }
                let word = args.rsplit(' ').next().unwrap_or_default();
                let suggestions = match command.suggest {
                    SuggestKind::None => Vec::new(),
                    SuggestKind::Fixed(names) => return Ok(complete_names(names, word)),
                    SuggestKind::Players => {
                        return Ok(complete_names(
                            self.runtime.players.names().into_values().collect(),
                            word,
                        ));
                    }
                    SuggestKind::Servers => return Ok(complete_names(servers(), word)),
                    SuggestKind::Dynamic => {
                        let mut input = self.context.clone();
                        input.command = Some(name.into());
                        input.args = args.into();
                        input.word = word.into();
                        input.position = args.split(' ').count();
                        self.runtime.plugins.suggest(&instance, input).await?
                    }
                };
                return Ok(filter_suggestions(suggestions, word));
            }
        };
        if !self
            .runtime
            .permits(&self.context.uuid, &command.permission)
        {
            return Ok(Vec::new());
        }
        let word = args.rsplit(' ').next().unwrap_or_default();
        let suggestions = match command.suggest {
            Suggest::None => Vec::new(),
            Suggest::Players => {
                return Ok(complete_names(
                    self.runtime.players.names().into_values().collect(),
                    word,
                ));
            }
            Suggest::Servers => return Ok(complete_names(servers(), word)),
            Suggest::Lua => {
                let mut input = self.context.clone();
                input.command = Some(name.into());
                input.args = args.into();
                input.word = word.into();
                input.position = args.split(' ').count();
                self.runtime
                    .spawn(&input, &self.jobs, |script, input, deadline, runtime| {
                        script.run(
                            "suggest",
                            input,
                            deadline,
                            runtime,
                            Vec::new,
                            parse_suggestions,
                        )
                    })
                    .await?
            }
        };
        Ok(filter_suggestions(suggestions, word))
    }

    pub async fn decision(&self, hook: &str) -> io::Result<Action> {
        self.runtime.call(hook, &self.context, &self.jobs).await
    }
    pub async fn command(&self, command: &str) -> io::Result<Action> {
        let (name, args) = command
            .split_once(char::is_whitespace)
            .unwrap_or((command, ""));
        use crate::plugins::CommandOwner;
        let owner = self
            .runtime
            .plugins
            .command_owner(name)
            .ok_or_else(|| invalid("unknown extension command"))?;
        let permission = match &owner {
            CommandOwner::Script => self
                .script_commands()
                .find(|(key, _)| *key == name)
                .map(|(_, command)| command.permission.clone())
                .ok_or_else(|| invalid("unknown extension command"))?,
            CommandOwner::Plugin(_, command) => command.permission.clone(),
        };
        if !self.runtime.permits(&self.context.uuid, &permission) {
            return Ok(Action::Message(
                "You do not have permission to use this command.".into(),
            ));
        }
        let mut input = self.context.clone();
        input.command = Some(name.into());
        input.args = args.trim().into();
        match owner {
            CommandOwner::Script => self.runtime.call("command", &input, &self.jobs).await,
            CommandOwner::Plugin(instance, _) => {
                let action = self.runtime.plugins.command(&instance, input).await?;
                self.runtime.known_destination(action)
            }
        }
    }
    async fn notify(&self, hook: &str, input: &Context) {
        if let Err(error) = self.runtime.call(hook, input, &self.jobs).await {
            eprintln!("rift: {error}");
        }
    }
    pub async fn before_transfer(&mut self, target: &str, reason: &str) -> io::Result<()> {
        let mut input = self.context.clone();
        input.target = Some(target.into());
        input.reason = Some(reason.into());
        match self
            .runtime
            .call("before_transfer", &input, &self.jobs)
            .await?
        {
            Action::Deny(reason) => {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
            }
            Action::Continue => {}
            _ => unreachable!(),
        }
        self.reserve(target)?;
        self.pending = Some(input);
        Ok(())
    }
    /// Reserve before any backend connection, including non-queued routes.
    pub fn reserve(&self, target: &str) -> io::Result<()> {
        let mut host = self.runtime.host.lock().unwrap();
        let groups = self.runtime.groups.lock().unwrap();
        for (destination, capacity) in self.queues() {
            let members = groups.get(destination);
            if destination != target && !members.is_some_and(|members| members.contains(target)) {
                continue;
            }
            let capacity = capacity * members.map_or(1, BTreeSet::len);
            // Moving between instances does not consume another group slot.
            if members.is_some()
                && host
                    .leases
                    .get(&self.context.id)
                    .is_some_and(|servers| contains_destination(servers, destination, members))
            {
                continue;
            }
            let used = host
                .leases
                .values()
                .filter(|servers| contains_destination(servers, destination, members))
                .count();
            let head = host
                .queues
                .get(destination)
                .and_then(|q| q.front())
                .map(|(id, _)| *id);
            if used >= capacity || head.is_some_and(|id| id != self.context.id) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Server is full; use the queue command.",
                ));
            }
        }
        host.leases
            .entry(self.context.id)
            .or_default()
            .insert(target.into());
        Ok(())
    }
    pub fn release(&self, target: &str) {
        let mut host = self.runtime.host.lock().unwrap();
        if let Some(leases) = host.leases.get_mut(&self.context.id) {
            leases.remove(target);
        }
    }
    pub async fn transfer_failed(&mut self) {
        if let Some(mut input) = self.pending.take() {
            self.release(input.target.as_deref().unwrap());
            input.success = Some(false);
            self.notify("after_transfer", &input).await;
        }
    }
    pub async fn ready(&mut self, server: &str) {
        self.runtime
            .host
            .lock()
            .unwrap()
            .leases
            .insert(self.context.id, BTreeSet::from([server.into()]));
        self.context.server = Some(server.into());
        if let Some(mut input) = self.pending.take() {
            input.success = Some(true);
            self.leave_queue();
            self.notify("after_transfer", &input).await;
        }
        if !self.joined {
            self.joined = true;
            self.notify("join", &self.context).await;
        }
    }
    pub fn enqueue(&self, target: &str) -> io::Result<usize> {
        if !self.queues().contains_key(target) {
            return Err(invalid("No queue is configured for that server."));
        }
        if self.context.server.as_deref().is_some_and(|current| {
            current == target
                || self
                    .runtime
                    .groups
                    .lock()
                    .unwrap()
                    .get(target)
                    .is_some_and(|members| members.contains(current))
        }) {
            return Err(invalid("You are already on that server."));
        }
        let mut host = self.runtime.host.lock().unwrap();
        if let Some(queue) = host.queues.get(target)
            && let Some(position) = queue.iter().position(|(id, _)| *id == self.context.id)
        {
            return Ok(position + 1);
        }
        if host.queues.get(target).is_some_and(|q| q.len() >= 1024) {
            return Err(invalid("Queue is full."));
        }
        for queue in host.queues.values_mut() {
            queue.retain(|(id, _)| *id != self.context.id);
        }
        let queue = host.queues.entry(target.into()).or_default();
        queue.push_back((self.context.id, Instant::now()));
        Ok(queue.len())
    }
    pub fn leave_queue(&self) {
        for queue in self.runtime.host.lock().unwrap().queues.values_mut() {
            queue.retain(|(id, _)| *id != self.context.id);
        }
    }
    pub fn queued_target(&self) -> io::Result<Option<String>> {
        let mut host = self.runtime.host.lock().unwrap();
        let mut head = None;
        for (server, queue) in &mut host.queues {
            if let Some((_, entered)) = queue.iter().find(|(id, _)| *id == self.context.id)
                && entered.elapsed() >= Duration::from_secs(300)
            {
                queue.retain(|(id, _)| *id != self.context.id);
                return Err(invalid("Queue expired after five minutes."));
            }
            if queue.front().is_some_and(|(id, _)| *id == self.context.id) {
                head = Some(server.clone());
            }
        }
        Ok(head.filter(|server| {
            let groups = self.runtime.groups.lock().unwrap();
            let members = groups.get(server);
            let used = host
                .leases
                .values()
                .filter(|servers| contains_destination(servers, server, members))
                .count();
            used < self.queues().get(server).copied().unwrap_or(0)
                * members.map_or(1, BTreeSet::len)
        }))
    }
}
fn group_members(config: &Config) -> BTreeMap<String, BTreeSet<String>> {
    config
        .service_groups
        .keys()
        .map(|group| {
            (
                group.clone(),
                config.instance_names(group).into_iter().collect(),
            )
        })
        .collect()
}

fn contains_destination(
    servers: &BTreeSet<String>,
    destination: &str,
    members: Option<&BTreeSet<String>>,
) -> bool {
    servers.contains(destination) || members.is_some_and(|members| !servers.is_disjoint(members))
}
impl Drop for ExtensionSession {
    fn drop(&mut self) {
        {
            let mut host = self.runtime.host.lock().unwrap();
            host.leases.remove(&self.context.id);
            for queue in host.queues.values_mut() {
                queue.retain(|(id, _)| *id != self.context.id);
            }
        }
        // Persistent plugins are told without waiting; their queues are bounded.
        let mut closing = self.context.clone();
        closing.reason = Some("session_closed".into());
        if let Some(mut transfer) = self.pending.clone() {
            transfer.success = Some(false);
            self.runtime.plugins.observe("after_transfer", &transfer);
        }
        self.runtime.plugins.observe("disconnect", &closing);
        // Cleanup is synchronous and unconditional. Notifications are bounded,
        // best effort on cancellation and shutdown; never queue unbounded work.
        let Some(script) = self.runtime.script.clone() else {
            return;
        };
        if !script.hooks.contains("disconnect")
            && !(self.pending.is_some() && script.hooks.contains("after_transfer"))
        {
            return;
        }
        // A cancelled callback may still be stopping. Never let teardown
        // overtake it; cleanup above is already complete without observers.
        let Ok(session_job) = self.jobs.clone().try_acquire_owned() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let runtime = self.runtime.clone();
        let mut input = self.context.clone();
        let pending = self.pending.take();
        input.reason = Some("session_closed".into());
        // Wait briefly for a worker, like other player callbacks, instead of
        // dropping the notification on the first busy moment.
        handle.clone().spawn(async move {
            let Some(permit) = script::acquire_slot(&runtime.slots).await else {
                eprintln!("rift: extension teardown notification dropped: capacity exhausted");
                return;
            };
            let deadline = Instant::now() + script::EXECUTION_TIMEOUT;
            handle.spawn_blocking(move || {
                let _permit = permit;
                let _session_job = session_job;
                if let Some(mut transfer) = pending {
                    transfer.success = Some(false);
                    if script.hooks.contains("after_transfer")
                        && let Err(error) =
                            script.evaluate("after_transfer", &transfer, deadline, &runtime)
                    {
                        eprintln!("rift: {error}");
                    }
                }
                if script.hooks.contains("disconnect")
                    && let Err(error) = script.evaluate("disconnect", &input, deadline, &runtime)
                {
                    eprintln!("rift: {error}");
                }
            });
        });
    }
}

#[cfg(test)]
mod decision_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod v2_tests;
