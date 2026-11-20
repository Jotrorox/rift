//! The `rift` global of a persistent Lua plugin: the script-mode API backed
//! by [`Services`], without per-call budgets.
//!
//! Registration verbs (`rift.on`, `rift.command`) only work while the entry
//! chunk runs. `rift.every`, `rift.subscribe` and `rift.timers.*` also work
//! later, from handlers, as long as the plugin registered the matching hook
//! (`timer` or `message`) during initialization: the registration is fixed
//! once `init` returns, and the host does not deliver hooks outside it.
use super::Interrupt;
use crate::plugins::{
    PluginSpec,
    api::{
        BossBarSpec, CommandSpec, Hook, Level, PlayerInfo, PlayerRecord, SuggestKind, TitleSpec,
    },
    services::Services,
};
use crate::{cluster::queues::jobs::Scope, messaging::SubscriptionScope, text::Text};
use mlua::{Function, Lua, LuaString, MultiValue, Table, Value};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    time::Instant,
};

/// Events `rift.on` accepts and the hook each one registers. `reload` and
/// `shutdown` are lifecycle callbacks, not hooks.
const EVENTS: &[(&str, Option<Hook>)] = &[
    ("route", Some(Hook::Route)),
    ("http", Some(Hook::Http)),
    ("status", Some(Hook::Status)),
    ("login", Some(Hook::Login)),
    ("initial_server", Some(Hook::InitialServer)),
    ("before_transfer", Some(Hook::BeforeTransfer)),
    ("join", Some(Hook::Join)),
    ("after_transfer", Some(Hook::AfterTransfer)),
    ("disconnect", Some(Hook::Disconnect)),
    ("message", Some(Hook::Message)),
    ("timer", Some(Hook::Timer)),
    ("event", Some(Hook::Event)),
    ("dialog", Some(Hook::Dialog)),
    ("menu", Some(Hook::Menu)),
    ("chat", Some(Hook::Chat)),
    ("kick", Some(Hook::Kick)),
    ("job_done", Some(Hook::Job)),
    // Background workers only: see `background`.
    ("work", None),
    ("reload", None),
    ("shutdown", None),
];

/// `extensions.jobs` limits, which `rift.every` shares with script mode.
const MIN_JOB_MS: i64 = 100;
const MAX_JOB_MS: i64 = 86_400_000;
const MAX_JOBS: usize = 32;
const MAX_JSON_DEPTH: usize = 64;
const DEFAULT_REQUEST_MS: u32 = 5000;

pub(super) struct Command {
    pub spec: CommandSpec,
    pub run: Function,
    pub suggest: Option<Function>,
}

/// What a timer handle runs when it fires.
#[derive(Clone)]
pub(super) enum TimerAction {
    /// `rift.every(name, ms, fn)`: called with `{ api_version = 2, job = name }`.
    Job(String, Function),
    /// `rift.timers.every/after(ms, fn)`: called with the handle.
    Callback(Function),
    /// No function: `rift.on("timer", fn)` handlers get the handle.
    Event,
}

pub(super) struct Timer {
    pub action: TimerAction,
    pub once: bool,
}

pub(super) struct Subscription {
    pub subject: String,
    /// `None` delivers to `rift.on("message", fn)` handlers.
    pub callback: Option<Function>,
}

/// Plugin state shared by the engine and the functions it installs.
pub(super) struct State {
    pub services: Services,
    /// The running call's wall deadline; `None` between calls.
    pub deadline: Cell<Option<Instant>>,
    /// True while the entry chunk runs.
    pub initializing: Cell<bool>,
    pub handlers: RefCell<BTreeMap<&'static str, Vec<Function>>>,
    pub commands: RefCell<Vec<Command>>,
    pub timers: RefCell<BTreeMap<u64, Timer>>,
    pub jobs: RefCell<BTreeSet<String>>,
    pub subscriptions: RefCell<BTreeMap<u64, Subscription>>,
    /// Hooks registered so far; fixed once `init` returns.
    pub hooks: RefCell<BTreeSet<Hook>>,
    /// `rift.background.run(payload, fn)`: what to call with the result.
    pub job_callbacks: RefCell<BTreeMap<u64, Function>>,
}

impl State {
    pub fn new(services: Services) -> Self {
        Self {
            services,
            deadline: Cell::new(None),
            initializing: Cell::new(false),
            handlers: RefCell::default(),
            commands: RefCell::default(),
            timers: RefCell::default(),
            jobs: RefCell::default(),
            subscriptions: RefCell::default(),
            hooks: RefCell::default(),
            job_callbacks: RefCell::default(),
        }
    }

    /// Why running guest code must stop now, if it must.
    pub fn interrupted(&self) -> Option<Interrupt> {
        if self.services.stop_requested() {
            Some(Interrupt::Stop)
        } else if self
            .deadline
            .get()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Some(Interrupt::Deadline)
        } else {
            None
        }
    }

    pub fn check(&self) -> mlua::Result<()> {
        match self.interrupted() {
            Some(interrupt) => Err(mlua::Error::external(interrupt)),
            None => Ok(()),
        }
    }

    pub fn handlers(&self, event: &str) -> Vec<Function> {
        self.handlers
            .borrow()
            .get(event)
            .cloned()
            .unwrap_or_default()
    }

    /// During init, register `hook`; afterwards require that it was.
    fn require_hook(&self, hook: Hook, verb: &str) -> mlua::Result<()> {
        if self.initializing.get() {
            self.hooks.borrow_mut().insert(hook);
            return Ok(());
        }
        if self.hooks.borrow().contains(&hook) {
            return Ok(());
        }
        Err(runtime(format!(
            "{verb}: this plugin registered no {} hook during initialization, so these events would never arrive; create one {} (or call rift.on(\"{}\", ...)) in the entry script",
            hook.name(),
            if hook == Hook::Timer {
                "timer"
            } else {
                "subscription"
            },
            hook.name()
        )))
    }

    fn registering(&self, verb: &str) -> mlua::Result<()> {
        if self.initializing.get() {
            Ok(())
        } else {
            Err(runtime(format!(
                "{verb} is only available while the plugin initializes"
            )))
        }
    }
}

fn runtime(message: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(message)
}

/// A Rust function for Lua that first checks the deadline and the stop flag.
/// LuaJIT runs hot loops as compiled traces that never call the instruction
/// hook, but a loop that calls into Rift still passes through here.
pub(super) fn function<A, R>(
    lua: &Lua,
    state: &Rc<State>,
    work: impl Fn(&Lua, A) -> mlua::Result<R> + 'static,
) -> mlua::Result<Function>
where
    A: mlua::FromLuaMulti,
    R: mlua::IntoLuaMulti,
{
    let state = state.clone();
    lua.create_function(move |lua, args: A| {
        state.check()?;
        work(lua, args)
    })
}

/// A JSON value as Lua: `null` is nil, arrays are sequences, objects tables.
pub(super) fn to_lua(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    fn convert(lua: &Lua, value: &serde_json::Value, depth: usize) -> mlua::Result<Value> {
        if depth > MAX_JSON_DEPTH {
            return Err(runtime("JSON value nests too deeply"));
        }
        Ok(match value {
            serde_json::Value::Null => Value::Nil,
            serde_json::Value::Bool(value) => Value::Boolean(*value),
            serde_json::Value::Number(number) => match number.as_i64() {
                Some(value) => Value::Integer(value),
                None => Value::Number(number.as_f64().unwrap_or(f64::NAN)),
            },
            serde_json::Value::String(text) => Value::String(lua.create_string(text)?),
            serde_json::Value::Array(values) => {
                let table = lua.create_table_with_capacity(values.len(), 0)?;
                for (index, value) in values.iter().enumerate() {
                    table.raw_set(index + 1, convert(lua, value, depth + 1)?)?;
                }
                Value::Table(table)
            }
            serde_json::Value::Object(values) => {
                let table = lua.create_table_with_capacity(0, values.len())?;
                for (key, value) in values {
                    table.raw_set(key.as_str(), convert(lua, value, depth + 1)?)?;
                }
                Value::Table(table)
            }
        })
    }
    convert(lua, value, 0)
}

/// A Lua value as JSON with the configuration table rules: string keys make
/// objects, dense integer keys from 1 make arrays, an empty table is `{}`.
pub(super) fn from_lua(value: Value) -> mlua::Result<serde_json::Value> {
    fn convert(value: Value, depth: usize) -> mlua::Result<serde_json::Value> {
        if depth > MAX_JSON_DEPTH {
            return Err(runtime("table nests too deeply for JSON"));
        }
        Ok(match value {
            Value::Nil => serde_json::Value::Null,
            Value::Boolean(value) => value.into(),
            Value::Integer(value) => value.into(),
            Value::Number(value) => serde_json::Number::from_f64(value)
                .map(serde_json::Value::Number)
                .ok_or_else(|| runtime("JSON numbers must be finite"))?,
            Value::String(text) => text.to_str()?.to_owned().into(),
            Value::Table(table) => {
                let mut object = serde_json::Map::new();
                let mut array = BTreeMap::new();
                for pair in table.pairs::<Value, Value>() {
                    let (key, value) = pair?;
                    let value = convert(value, depth + 1)?;
                    match key {
                        Value::String(key) => {
                            object.insert(key.to_str()?.to_owned(), value);
                        }
                        Value::Integer(index) if index >= 1 => {
                            array.insert(index, value);
                        }
                        _ => return Err(runtime("JSON keys must be strings or array indices")),
                    }
                }
                match (object.is_empty(), array.is_empty()) {
                    (_, true) => serde_json::Value::Object(object),
                    (true, false) if array.keys().copied().eq(1..=array.len() as i64) => {
                        serde_json::Value::Array(array.into_values().collect())
                    }
                    _ => {
                        return Err(runtime(
                            "tables must be either a dense list or have only string keys",
                        ));
                    }
                }
            }
            other => {
                return Err(runtime(format!(
                    "a {} value cannot be encoded as JSON",
                    other.type_name()
                )));
            }
        })
    }
    convert(value, 0)
}

/// A JSON text decoded into Lua, or the text itself when it is not JSON.
fn decoded(lua: &Lua, text: &str) -> mlua::Result<Value> {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => to_lua(lua, &value),
        Err(_) => Ok(Value::String(lua.create_string(text)?)),
    }
}

/// The same player table script mode builds (`connection_id` is a string).
fn player(lua: &Lua, info: &PlayerInfo) -> mlua::Result<Table> {
    let player = lua.create_table()?;
    player.raw_set("uuid", info.uuid.as_str())?;
    player.raw_set("name", info.name.as_str())?;
    player.raw_set("server", info.server.as_str())?;
    player.raw_set("connection_id", info.connection_id.to_string())?;
    player.raw_set("protocol", info.protocol)?;
    // nil when everyone sees the player.
    player.raw_set("visible_to", info.visible_to.as_deref())?;
    // nil until the client's Client Information arrives.
    player.raw_set("locale", info.locale.as_deref())?;
    // What a plugin set, else the client's locale.
    player.raw_set("language", info.language.as_deref())?;
    player.raw_set("ip", info.ip.as_str())?;
    let properties = lua.create_table_with_capacity(info.properties.len(), 0)?;
    for property in &info.properties {
        let entry = lua.create_table()?;
        entry.raw_set("name", property.name.as_str())?;
        entry.raw_set("value", property.value.as_str())?;
        entry.raw_set("signature", property.signature.as_deref())?;
        properties.raw_push(entry)?;
    }
    player.raw_set("properties", properties)?;
    // The cluster peer the player is on; absent without a cluster.
    if let Some(peer) = &info.peer {
        player.raw_set("peer", peer.as_str())?;
    }
    Ok(player)
}

/// What `rift.players.lookup` and `search` return for one player.
fn record(lua: &Lua, record: &PlayerRecord) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.raw_set("uuid", record.uuid.as_str())?;
    table.raw_set("name", record.name.as_str())?;
    table.raw_set("first_seen", record.first_seen)?;
    table.raw_set("last_seen", record.last_seen)?;
    table.raw_set("last_ip", record.last_ip.as_str())?;
    table.raw_set("last_server", record.last_server.as_str())?;
    table.raw_set("online", record.online)?;
    table.raw_set("language", record.language.as_deref())?;
    Ok(table)
}

fn level(name: &str) -> mlua::Result<Level> {
    Ok(match name {
        "trace" => Level::Trace,
        "debug" => Level::Debug,
        "info" => Level::Info,
        "warn" => Level::Warn,
        "error" => Level::Error,
        _ => {
            return Err(runtime(
                "rift.log level must be trace, debug, info, warn or error",
            ));
        }
    })
}

/// Install `rift`, `print`, `pcall`/`xpcall` and the `rift.*` modules for
/// `require`.
pub(super) fn install(lua: &Lua, state: &Rc<State>, spec: &PluginSpec) -> mlua::Result<()> {
    let globals = lua.globals();
    let rift = lua.create_table()?;
    rift.raw_set("enabled", true)?;
    rift.raw_set("api_version", crate::extensions::API_VERSION)?;
    rift.raw_set("plugin_name", spec.name.as_str())?;
    let meta = lua.create_table()?;
    meta.raw_set("name", spec.name.as_str())?;
    meta.raw_set("version", spec.manifest.version.as_str())?;
    meta.raw_set(
        "data_dir",
        state.services.meta().data_dir().display().to_string(),
    )?;
    meta.raw_set(
        "config_dir",
        state
            .services
            .meta()
            .config_dir()
            .map(|path| path.display().to_string()),
    )?;
    // The exact `rift:plugin` version, such as "0.2.1".
    meta.raw_set("api", state.services.meta().api())?;
    rift.raw_set("meta", meta)?;

    registration(lua, &rift, state)?;
    lua.load(include_str!("../../lua/menus.lua"))
        .set_name("=rift menus")
        .call::<()>(rift.clone())?;
    helpers(lua, &rift, state)?;
    crate::text::lua::install(lua, &rift, Some(spec.name.as_str()))?;
    rift.raw_set("players", players(lua, state)?)?;
    rift.raw_set("store", store(lua, state)?)?;
    rift.raw_set("permissions", permissions(lua, state)?)?;
    rift.raw_set("servers", servers(lua, state)?)?;
    rift.raw_set("timers", timers(lua, state)?)?;
    rift.raw_set("background", background(lua, state)?)?;
    rift.raw_set("cluster", cluster(lua, state)?)?;
    messaging(lua, &rift, state)?;
    super::http::install(lua, &rift, state, spec.manifest.capabilities.network)?;

    globals.raw_set("rift", rift.clone())?;
    let loaded: Table = globals
        .raw_get::<Table>("package")?
        .raw_get::<Table>("loaded")?;
    loaded.raw_set("rift", rift.clone())?;
    for module in [
        "store",
        "permissions",
        "http",
        "players",
        "servers",
        "timers",
        "background",
        "text",
    ] {
        loaded.raw_set(format!("rift.{module}"), rift.raw_get::<Table>(module)?)?;
    }
    Ok(())
}

/// `rift.on`, `rift.command`, `rift.every`, `rift.subscribe` and the
/// configuration-only verbs, which fail with a pointer to `rift.lua`.
fn registration(lua: &Lua, rift: &Table, state: &Rc<State>) -> mlua::Result<()> {
    let on = state.clone();
    rift.raw_set(
        "on",
        function(lua, state, move |_, (event, callback): (Value, Value)| {
            on.registering("rift.on")?;
            let event = match &event {
                Value::String(name) => name.to_str()?.to_owned(),
                _ => String::new(),
            };
            let Some((name, hook)) = EVENTS.iter().find(|(name, _)| *name == event) else {
                return Err(runtime(format!("unknown Rift event: {event}")));
            };
            let Value::Function(callback) = callback else {
                return Err(runtime("rift.on expects a function"));
            };
            let worker = on.services.is_worker();
            if worker != (*name == "work") {
                return Err(runtime(if worker {
                    format!(
                        "rift.on(\"{name}\"): a background worker receives no events; it handles only \"work\""
                    )
                } else {
                    "rift.on(\"work\") belongs in the worker script ([background] entry), which runs jobs queued with rift.background.run".to_owned()
                }));
            }
            if let Some(hook) = hook {
                on.hooks.borrow_mut().insert(*hook);
            }
            on.handlers
                .borrow_mut()
                .entry(name)
                .or_default()
                .push(callback);
            Ok(())
        })?,
    )?;

    let commands = state.clone();
    rift.raw_set(
        "command",
        function(lua, state, move |_, (name, definition): (Value, Value)| {
            commands.registering("rift.command")?;
            let (Value::String(name), Value::Table(definition)) = (name, definition) else {
                return Err(runtime(
                    "rift.command expects a name and { permission = ..., run = function(ctx) ... end, aliases = { ... }, console = false }",
                ));
            };
            let name = name.to_str()?.to_owned();
            crate::extensions::fields(
                &definition,
                &["permission", "run", "suggest", "aliases", "console"],
            )?;
            if commands
                .commands
                .borrow()
                .iter()
                .any(|command| command.spec.name == name)
            {
                return Err(runtime(format!("duplicate Rift command: {name}")));
            }
            let permission = match definition.raw_get::<Value>("permission")? {
                Value::String(permission) => permission.to_str()?.to_owned(),
                _ => {
                    return Err(runtime(format!(
                        "commands.{name}.permission must be a permission node"
                    )));
                }
            };
            let Value::Function(run) = definition.raw_get::<Value>("run")? else {
                return Err(runtime(format!("commands.{name}.run must be a function")));
            };
            let (suggest, function) = match definition.raw_get::<Value>("suggest")? {
                Value::Nil => (SuggestKind::None, None),
                Value::Function(function) => (SuggestKind::Dynamic, Some(function)),
                Value::String(kind) if kind.as_bytes().as_ref() == b"players" => {
                    (SuggestKind::Players, None)
                }
                Value::String(kind) if kind.as_bytes().as_ref() == b"servers" => {
                    (SuggestKind::Servers, None)
                }
                Value::Table(entries) => {
                    let fixed = entries
                        .sequence_values::<String>()
                        .collect::<mlua::Result<Vec<_>>>()
                        .ok()
                        .filter(|fixed| fixed.len() == entries.clone().pairs::<Value, Value>().count())
                        .ok_or_else(|| {
                            runtime(format!("commands.{name}.suggest lists must contain only strings"))
                        })?;
                    (SuggestKind::Fixed(fixed), None)
                }
                _ => {
                    return Err(runtime(format!(
                        "commands.{name}.suggest must be a function, \"players\", \"servers\" or a list of strings"
                    )));
                }
            };
            let aliases = match definition.raw_get::<Value>("aliases")? {
                Value::Nil => Vec::new(),
                Value::Table(entries) => entries
                    .sequence_values::<String>()
                    .collect::<mlua::Result<Vec<_>>>()
                    .ok()
                    .filter(|aliases| {
                        aliases.len() == entries.clone().pairs::<Value, Value>().count()
                    })
                    .ok_or_else(|| {
                        runtime(format!("commands.{name}.aliases must be a list of names"))
                    })?,
                _ => {
                    return Err(runtime(format!(
                        "commands.{name}.aliases must be a list of names"
                    )));
                }
            };
            let console = match definition.raw_get::<Value>("console")? {
                Value::Nil => false,
                Value::Boolean(console) => console,
                _ => {
                    return Err(runtime(format!(
                        "commands.{name}.console must be true or false"
                    )));
                }
            };
            commands.commands.borrow_mut().push(Command {
                spec: CommandSpec {
                    name,
                    aliases,
                    permission,
                    suggest,
                    console,
                },
                run,
                suggest: function,
            });
            Ok(())
        })?,
    )?;

    let every = state.clone();
    rift.raw_set(
        "every",
        function(
            lua,
            state,
            move |_, (name, every_ms, options, callback): (Value, Value, Value, Value)| {
                // `rift.every(name, ms, fn)` or `rift.every(name, ms, { scope = ... }, fn)`.
                let (options, callback) = match (options, callback) {
                    (Value::Function(callback), Value::Nil) => (None, callback),
                    (Value::Table(options), Value::Function(callback)) => (Some(options), callback),
                    _ => {
                        return Err(runtime(
                            "rift.every expects a job name, an interval in milliseconds, optional options and a function",
                        ));
                    }
                };
                let Value::String(name) = name else {
                    return Err(runtime(
                        "rift.every expects a job name, an interval in milliseconds and a function",
                    ));
                };
                let scope = match options {
                    None => Scope::default(),
                    Some(options) => {
                        for pair in options.pairs::<Value, Value>() {
                            let (key, _) = pair?;
                            if !matches!(&key, Value::String(key) if key.as_bytes().as_ref() == b"scope") {
                                return Err(runtime("rift.every options take scope only"));
                            }
                        }
                        let scope: Option<String> = options.get("scope")?;
                        Scope::parse(scope.as_deref()).map_err(runtime)?
                    }
                };
                let name = name.to_str()?.to_owned();
                if !crate::extensions::token(&name) {
                    return Err(runtime("invalid job name"));
                }
                let ms = match every_ms {
                    Value::Integer(ms) if (MIN_JOB_MS..=MAX_JOB_MS).contains(&ms) => ms as u32,
                    _ => {
                        return Err(runtime(format!(
                            "job every_ms must be an integer in {MIN_JOB_MS}..={MAX_JOB_MS}"
                        )));
                    }
                };
                every.require_hook(Hook::Timer, "rift.every")?;
                let mut jobs = every.jobs.borrow_mut();
                if jobs.contains(&name) {
                    return Err(runtime(format!("duplicate rift.every job: {name}")));
                }
                if jobs.len() >= MAX_JOBS {
                    return Err(runtime(format!(
                        "a plugin may define at most {MAX_JOBS} jobs"
                    )));
                }
                jobs.insert(name.clone());
                let handle = every.services.timers().every_scoped(ms, scope);
                every.timers.borrow_mut().insert(
                    handle,
                    Timer {
                        action: TimerAction::Job(name, callback),
                        once: false,
                    },
                );
                Ok(handle)
            },
        )?,
    )?;

    let subscribe = state.clone();
    rift.raw_set(
        "subscribe",
        function(lua, state, move |_, args: MultiValue| {
            let mut args = args.into_iter();
            let subject = args.next().unwrap_or(Value::Nil);
            // The second argument is a queue group or `{ queue = ..., scope = ... }`.
            let options = |table: &Table| -> mlua::Result<(Option<String>, Option<String>)> {
                Ok((table.get("queue")?, table.get("scope")?))
            };
            let (queue, scope, callback) = match (
                args.next().unwrap_or(Value::Nil),
                args.next().unwrap_or(Value::Nil),
            ) {
                (Value::Function(callback), Value::Nil) => (None, None, Some(callback)),
                (Value::String(queue), Value::Function(callback)) => {
                    (Some(queue.to_str()?.to_owned()), None, Some(callback))
                }
                (Value::String(queue), Value::Nil) => {
                    (Some(queue.to_str()?.to_owned()), None, None)
                }
                (Value::Table(table), Value::Function(callback)) => {
                    let (queue, scope) = options(&table)?;
                    (queue, scope, Some(callback))
                }
                (Value::Table(table), Value::Nil) => {
                    let (queue, scope) = options(&table)?;
                    (queue, scope, None)
                }
                (Value::Nil, Value::Nil) => (None, None, None),
                _ => {
                    return Err(runtime(
                        "rift.subscribe expects a subject, an optional queue group or options table and an optional function",
                    ));
                }
            };
            let scope = match scope {
                None => SubscriptionScope::Cluster,
                Some(scope) => scope
                    .parse::<SubscriptionScope>()
                    .map_err(|error| runtime(format!("rift.subscribe: {error}")))?,
            };
            let Value::String(subject) = subject else {
                return Err(runtime("rift.subscribe expects a subject"));
            };
            let subject = subject.to_str()?.to_owned();
            subscribe.require_hook(Hook::Message, "rift.subscribe")?;
            let handle = subscribe
                .services
                .bus()
                .subscribe_scoped(&subject, queue.as_deref(), scope)
                .map_err(runtime)?;
            subscribe
                .subscriptions
                .borrow_mut()
                .insert(handle, Subscription { subject, callback });
            Ok(handle)
        })?,
    )?;

    let unsubscribe = state.clone();
    rift.raw_set(
        "unsubscribe",
        function(lua, state, move |_, handle: u64| {
            unsubscribe.subscriptions.borrow_mut().remove(&handle);
            Ok(unsubscribe.services.bus().unsubscribe(handle))
        })?,
    )?;

    for verb in ["setup", "plugin"] {
        rift.raw_set(
            verb,
            function(lua, state, move |_, _: MultiValue| -> mlua::Result<()> {
                Err(runtime(format!(
                    "rift.{verb} configures Rift and is only available in rift.lua"
                )))
            })?,
        )?;
    }
    Ok(())
}

/// `rift.log`, `print`, `rift.clock`, `rift.time`, `rift.try` and the
/// interrupt-aware `pcall`/`xpcall`.
fn helpers(lua: &Lua, rift: &Table, state: &Rc<State>) -> mlua::Result<()> {
    let globals = lua.globals();
    let log = state.clone();
    rift.raw_set(
        "log",
        function(lua, state, move |_, (name, message): (String, String)| {
            log.services.log(level(&name)?, &message);
            Ok(())
        })?,
    )?;
    let tostring: Function = globals.raw_get("tostring")?;
    let print = state.clone();
    globals.raw_set(
        "print",
        function(lua, state, move |_, args: MultiValue| {
            let mut parts = Vec::with_capacity(args.len());
            for value in args {
                parts.push(tostring.call::<String>(value)?);
            }
            print.services.log(Level::Info, &parts.join("\t"));
            Ok(())
        })?,
    )?;
    let clock = state.clone();
    rift.raw_set(
        "clock",
        function(lua, state, move |_, ()| Ok(clock.services.meta().clock()))?,
    )?;
    let time = state.clone();
    rift.raw_set(
        "time",
        function(lua, state, move |_, ()| Ok(time.services.meta().time()))?,
    )?;
    // A caught error is rethrown while the call must stop: its deadline
    // passed, Rift is stopping the plugin, or memory ran out. Guest code can
    // handle every other error.
    // Errors raised by Rift functions reach Lua as their message, like an
    // `error("...")` from Lua code.
    let check = state.clone();
    let inspect = lua.create_function(move |lua, error: Value| {
        let (memory, error) = match error {
            Value::Error(error) => (
                super::memory_error(&error),
                Value::String(lua.create_string(super::message(&error))?),
            ),
            Value::String(message) => (
                message.as_bytes().as_ref() == b"not enough memory",
                Value::String(message),
            ),
            other => (false, other),
        };
        Ok((memory || check.interrupted().is_some(), error))
    })?;
    let (pcall, xpcall): (Function, Function) = lua
        .load(
            r#"
        local pcall, xpcall, error, inspect = ...
        local function finish(ok, ...)
            if ok then return ok, ... end
            local stop, message = inspect((...))
            if stop then error((...), 0) end
            return false, message
        end
        return function(f, ...)
            return finish(pcall(f, ...))
        end, function(f, handler, ...)
            return finish(xpcall(f, handler, ...))
        end
    "#,
        )
        .set_name("=rift pcall")
        .call((
            globals.raw_get::<Function>("pcall")?,
            globals.raw_get::<Function>("xpcall")?,
            globals.raw_get::<Function>("error")?,
            inspect,
        ))?;
    globals.raw_set("pcall", pcall.clone())?;
    globals.raw_set("xpcall", xpcall)?;
    rift.raw_set("try", pcall)
}

fn players(lua: &Lua, state: &Rc<State>) -> mlua::Result<Table> {
    let api = lua.create_table()?;
    let services = state.services.clone();
    api.raw_set(
        "list",
        function(lua, state, move |lua, server: Option<String>| {
            let list = services
                .players()
                .list(server.as_deref())
                .map_err(runtime)?;
            let table = lua.create_table_with_capacity(list.len(), 0)?;
            for info in &list {
                table.raw_push(player(lua, info)?)?;
            }
            Ok(table)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "get",
        function(lua, state, move |lua, target: String| {
            services
                .players()
                .get(&target)
                .map_err(runtime)?
                .map(|info| player(lua, &info))
                .transpose()
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "names",
        function(lua, state, move |lua, prefix: Option<String>| {
            lua.create_sequence_from(services.players().names(prefix.as_deref()))
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "lookup",
        function(lua, state, move |lua, target: String| {
            services
                .players()
                .lookup(&target)
                .map_err(runtime)?
                .map(|found| record(lua, &found))
                .transpose()
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "search",
        function(
            lua,
            state,
            move |lua, (prefix, limit): (String, Option<u32>)| {
                let found = services
                    .players()
                    .search(&prefix, limit.unwrap_or(20))
                    .map_err(runtime)?;
                let table = lua.create_table_with_capacity(found.len(), 0)?;
                for found in &found {
                    table.raw_push(record(lua, found)?)?;
                }
                Ok(table)
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "set_language",
        function(
            lua,
            state,
            move |_, (target, language): (String, Option<String>)| {
                services
                    .players()
                    .set_language(&target, language.as_deref())
                    .map_err(runtime)
            },
        )?,
    )?;
    // A string is plain text and a table a chat component, whose messages
    // are this plugin's (when unqualified) in the target's language.
    type Send = fn(&Services, &str, Text) -> Result<bool, String>;
    let texts: [(&str, &str, Send); 3] = [
        ("message", "message", |services, target, text| {
            services.players().message_text(target, text)
        }),
        ("action_bar", "action bar text", |services, target, text| {
            services.players().action_bar_text(target, text)
        }),
        ("kick", "kick reason", |services, target, text| {
            services.players().kick_text(target, text)
        }),
    ];
    for (name, label, send) in texts {
        let services = state.services.clone();
        api.raw_set(
            name,
            function(lua, state, move |_, (target, text): (String, Value)| {
                let text = services
                    .players()
                    .for_target(&target, || Text::lua(text, label))
                    .map_err(runtime)?;
                send(&services, &target, text).map_err(runtime)
            })?,
        )?;
    }
    let services = state.services.clone();
    api.raw_set(
        "title",
        function(lua, state, move |_, (target, options): (String, Value)| {
            let crate::extensions::PlayerAction::Title {
                title,
                subtitle,
                times,
            } = services
                .players()
                .for_target(&target, || crate::extensions::players::title(options))?
            else {
                unreachable!("title builds a title action");
            };
            services
                .players()
                .title(
                    &target,
                    &TitleSpec {
                        title: title.unwrap_or_else(|| Text::Plain(String::new())),
                        subtitle,
                        fade_in: times[0],
                        stay: times[1],
                        fade_out: times[2],
                    },
                )
                .map_err(runtime)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "tab_list",
        function(
            lua,
            state,
            move |_, (target, options): (String, Option<Table>)| {
                let crate::extensions::PlayerAction::TabList(changes) = services
                    .players()
                    .for_target(&target, || crate::extensions::players::tab_list(options))?
                else {
                    unreachable!("tab_list builds a tab list action");
                };
                let (header, footer) = changes.map_or((None, None), |changes| {
                    (
                        changes.header.map(|header| header.to_string()),
                        changes.footer.map(|footer| footer.to_string()),
                    )
                });
                services
                    .players()
                    .tab_list(&target, header.as_deref(), footer.as_deref())
                    .map_err(runtime)
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "boss_bar",
        function(
            lua,
            state,
            move |_, (target, name, options): (String, String, Option<Table>)| {
                let crate::extensions::PlayerAction::BossBar { name, bar } =
                    services.players().for_target(&target, || {
                        crate::extensions::players::boss_bar(name, options)
                    })?
                else {
                    unreachable!("boss_bar builds a boss bar action");
                };
                let bar = bar.map(|bar| BossBarSpec {
                    text_json: bar.text.to_string(),
                    progress: bar.progress,
                    color: crate::protocol::COLORS[usize::from(bar.color)].to_owned(),
                    style: crate::protocol::STYLES[usize::from(bar.style)].to_owned(),
                    darken_sky: bar.flags & 1 != 0,
                    play_music: bar.flags & 2 != 0,
                    create_fog: bar.flags & 4 != 0,
                });
                services
                    .players()
                    .boss_bar(&target, &name, bar.as_ref())
                    .map_err(runtime)
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "dialog",
        function(lua, state, move |_, (target, spec): (String, Value)| {
            let players = services.players();
            let dialog =
                players.for_target(&target, || crate::extensions::players::dialog(spec))?;
            players.show_dialog(&target, dialog).map_err(runtime)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "clear_dialog",
        function(lua, state, move |_, target: String| {
            services.players().clear_dialog(&target).map_err(runtime)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "menu",
        function(lua, state, move |_, (target, spec): (String, Value)| {
            let players = services.players();
            let menu = players.for_target(&target, || crate::extensions::players::menu(spec))?;
            players.show_menu(&target, menu).map_err(runtime)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "close_menu",
        function(
            lua,
            state,
            move |_, (target, id): (String, Option<String>)| {
                services
                    .players()
                    .close_menu(&target, id.as_deref())
                    .map_err(runtime)
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "transfer",
        function(lua, state, move |_, (target, server): (String, String)| {
            services
                .players()
                .transfer(&target, &server)
                .map_err(runtime)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "broadcast",
        function(
            lua,
            state,
            move |_, (message, server): (Value, Option<String>)| {
                // Rendered for each recipient by `broadcast_text`.
                let message = Text::lua_as_written(message, "message").map_err(runtime)?;
                services
                    .players()
                    .broadcast_text(message, server.as_deref())
                    .map_err(runtime)
            },
        )?,
    )?;
    Ok(api)
}

fn store(lua: &Lua, state: &Rc<State>) -> mlua::Result<Table> {
    let api = lua.create_table()?;
    let services = state.services.clone();
    api.raw_set(
        "get",
        function(
            lua,
            state,
            move |lua, (namespace, key): (String, String)| {
                services
                    .store()
                    .get(&namespace, &key)
                    .map_err(runtime)?
                    .map(|value| lua.create_string(value))
                    .transpose()
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "set",
        function(
            lua,
            state,
            move |_, (namespace, key, value): (String, String, Value)| {
                let Value::String(value) = value else {
                    return Err(runtime("store value must be a string"));
                };
                services
                    .store()
                    .set(&namespace, &key, &value.as_bytes())
                    .map_err(runtime)
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "delete",
        function(lua, state, move |_, (namespace, key): (String, String)| {
            services.store().delete(&namespace, &key).map_err(runtime)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "increment",
        function(
            lua,
            state,
            move |_, (namespace, key, delta): (String, String, Value)| {
                let delta = match delta {
                    Value::Nil => 1,
                    Value::Integer(delta) => delta,
                    _ => return Err(runtime("store increment delta must be an integer")),
                };
                services
                    .store()
                    .increment(&namespace, &key, delta)
                    .map_err(runtime)
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "list",
        function(
            lua,
            state,
            move |lua, (namespace, prefix): (String, Option<String>)| {
                let entries = services
                    .store()
                    .list(&namespace, prefix.as_deref().unwrap_or_default())
                    .map_err(runtime)?;
                let table = lua.create_table_with_capacity(entries.len(), 0)?;
                for (key, value) in entries {
                    let entry = lua.create_table()?;
                    entry.raw_set("key", key)?;
                    entry.raw_set("value", lua.create_string(value)?)?;
                    table.raw_push(entry)?;
                }
                Ok(table)
            },
        )?,
    )?;
    Ok(api)
}

fn permissions(lua: &Lua, state: &Rc<State>) -> mlua::Result<Table> {
    let api = lua.create_table()?;
    let services = state.services.clone();
    api.raw_set(
        "has",
        function(lua, state, move |_, (uuid, node): (String, String)| {
            Ok(services.permissions().has(&uuid, &node))
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "set",
        function(
            lua,
            state,
            move |_, (uuid, node, value): (String, String, Value)| {
                let value = match value {
                    Value::Nil => None,
                    Value::Boolean(value) => Some(value),
                    _ => return Err(runtime("permission override must be true, false, or nil")),
                };
                services
                    .permissions()
                    .set(&uuid, &node, value)
                    .map_err(runtime)
            },
        )?,
    )?;
    // `add_group(uuid, group, options?)`: `expires_at` (Unix seconds) or
    // `expires_in` (seconds from now) end the membership then.
    let services = state.services.clone();
    api.raw_set(
        "add_group",
        function(
            lua,
            state,
            move |_, (uuid, group, options): (String, String, Value)| {
                let expires_at = expiry(&options, "add_group", &[])?;
                services
                    .permissions()
                    .add_group_until(&uuid, &group, expires_at)
                    .map_err(runtime)
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "remove_group",
        function(lua, state, move |_, (uuid, group): (String, String)| {
            services
                .permissions()
                .remove_group(&uuid, &group)
                .map_err(runtime)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "groups",
        function(lua, state, move |lua, uuid: String| {
            lua.create_sequence_from(services.permissions().groups(&uuid))
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "list_groups",
        function(lua, state, move |lua, ()| {
            decoded(lua, &services.permissions().list_groups())
        })?,
    )?;
    // Lua only, as in `rift.permissions` of extensions API v3: the node as
    // true, false or nil (undefined), and the player's prefix, suffix and
    // meta, in explicit contexts or the player's current ones.
    let services = state.services.clone();
    api.raw_set(
        "check",
        function(
            lua,
            state,
            move |_, (uuid, node, contexts): (String, String, Value)| {
                let contexts = optional_contexts(&contexts)?;
                services
                    .permissions()
                    .check(&uuid, &node, contexts.as_ref())
                    .map_err(runtime)
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "meta",
        function(lua, state, move |lua, (uuid, contexts): (String, Value)| {
            let contexts = optional_contexts(&contexts)?;
            let meta = services
                .permissions()
                .meta(&uuid, contexts.as_ref())
                .map_err(runtime)?;
            decoded(lua, &meta)
        })?,
    )?;
    // `set_meta(uuid, key, value_or_nil, options?)`, with the write options
    // of extensions API v3: `contexts`, `expires_in` (seconds from now) and
    // `expires_at` (Unix seconds). `nil` removes the node in those contexts.
    let services = state.services.clone();
    api.raw_set(
        "set_meta",
        function(
            lua,
            state,
            move |_, (uuid, key, value, options): (String, String, Option<String>, Value)| {
                let (contexts, expires_at) = meta_options(&options)?;
                services
                    .permissions()
                    .set_meta(&uuid, &key, value.as_deref(), contexts, expires_at)
                    .map_err(runtime)
            },
        )?,
    )?;
    Ok(api)
}

/// `expires_at` or `expires_in` of an options table whose other keys are
/// `also`; `nil` options mean no expiry.
fn expiry(options: &Value, verb: &str, also: &[&str]) -> mlua::Result<Option<u64>> {
    let options = match options {
        Value::Nil => return Ok(None),
        Value::Table(options) => options,
        _ => return Err(runtime(format!("{verb} options must be a table"))),
    };
    for pair in options.pairs::<String, Value>() {
        let (key, _) = pair?;
        if !matches!(key.as_str(), "expires_in" | "expires_at") && !also.contains(&key.as_str()) {
            return Err(runtime(format!("unknown {verb} option {key:?}")));
        }
    }
    match (
        options.raw_get::<Option<u64>>("expires_at")?,
        options.raw_get::<Option<u64>>("expires_in")?,
    ) {
        (Some(_), Some(_)) => Err(runtime("give expires_at or expires_in, not both")),
        (Some(at), None) => Ok(Some(at)),
        (None, Some(seconds)) => Ok(Some(rift_permissions::unix_now().saturating_add(seconds))),
        (None, None) => Ok(None),
    }
}

/// The options table of `set_meta`.
fn meta_options(options: &Value) -> mlua::Result<(rift_permissions::ContextSet, Option<u64>)> {
    let expires_at = expiry(options, "set_meta", &["contexts"])?;
    let contexts = match options {
        Value::Table(options) => match options.raw_get::<Value>("contexts")? {
            Value::Nil => rift_permissions::ContextSet::new(),
            value => crate::permissions::lua::contexts(&value)?,
        },
        _ => rift_permissions::ContextSet::new(),
    };
    Ok((contexts, expires_at))
}

/// `nil` for the player's current contexts, or a context table.
fn optional_contexts(value: &Value) -> mlua::Result<Option<rift_permissions::ContextSet>> {
    match value {
        Value::Nil => Ok(None),
        value => crate::permissions::lua::contexts(value).map(Some),
    }
}

/// `rift.servers.*` wait for the control service and return its decoded
/// reply, instead of publishing a request like script mode does.
fn servers(lua: &Lua, state: &Rc<State>) -> mlua::Result<Table> {
    let api = lua.create_table()?;
    type Operation = fn(&Services, &str) -> Result<String, String>;
    let operations: [(&str, Operation); 5] = [
        ("create", |services, target| {
            services.servers().create(target)
        }),
        ("remove", |services, target| {
            services.servers().remove(target)
        }),
        ("start", |services, target| services.servers().start(target)),
        ("stop", |services, target| services.servers().stop(target)),
        ("download", |services, target| {
            services.servers().download(target)
        }),
    ];
    for (name, operation) in operations {
        let services = state.services.clone();
        api.raw_set(
            name,
            function(lua, state, move |lua, target: String| {
                decoded(lua, &operation(&services, &target).map_err(runtime)?)
            })?,
        )?;
    }
    let services = state.services.clone();
    api.raw_set(
        "control",
        function(
            lua,
            state,
            move |lua, (operation, payload): (String, Value)| {
                let payload = match payload {
                    Value::Nil => "{}".to_owned(),
                    payload => from_lua(payload)?.to_string(),
                };
                decoded(
                    lua,
                    &services
                        .servers()
                        .control(&operation, &payload)
                        .map_err(runtime)?,
                )
            },
        )?,
    )?;
    Ok(api)
}

/// `rift.timers.every(ms, fn?)`, `rift.timers.after(ms, fn?)` and
/// `rift.timers.cancel(handle)`. A function is called with the handle;
/// without one, `rift.on("timer", fn)` handlers are.
fn timers(lua: &Lua, state: &Rc<State>) -> mlua::Result<Table> {
    let api = lua.create_table()?;
    for (name, once) in [("every", false), ("after", true)] {
        let timers = state.clone();
        api.raw_set(
            name,
            function(
                lua,
                state,
                move |_, (ms, callback): (u32, Option<Function>)| {
                    timers.require_hook(Hook::Timer, &format!("rift.timers.{name}"))?;
                    let handle = if once {
                        timers.services.timers().after(ms)
                    } else {
                        timers.services.timers().every(ms)
                    };
                    timers.timers.borrow_mut().insert(
                        handle,
                        Timer {
                            action: callback.map_or(TimerAction::Event, TimerAction::Callback),
                            once,
                        },
                    );
                    Ok(handle)
                },
            )?,
        )?;
    }
    let timers = state.clone();
    api.raw_set(
        "cancel",
        function(lua, state, move |_, handle: u64| {
            if let Some(Timer {
                action: TimerAction::Job(name, _),
                ..
            }) = timers.timers.borrow_mut().remove(&handle)
            {
                timers.jobs.borrow_mut().remove(&name);
            }
            Ok(timers.services.timers().cancel(handle))
        })?,
    )?;
    Ok(api)
}

/// `rift.cluster`: `lease(name, ttl_ms)` returns a fencing token or nil while
/// another peer holds the lease, `renew(name, token, ttl_ms)` and
/// `release(name, token)` return whether they took effect, `is_leader()`
/// (advisory) and `peer()` (nil without a cluster). The lease calls wait for
/// the cluster's answer.
fn cluster(lua: &Lua, state: &Rc<State>) -> mlua::Result<Table> {
    fn ttl(ttl_ms: i64) -> mlua::Result<u64> {
        u64::try_from(ttl_ms)
            .map_err(|_| runtime("lease ttl must be a positive number of milliseconds"))
    }
    let api = lua.create_table()?;
    let services = state.services.clone();
    api.raw_set(
        "lease",
        function(lua, state, move |_, (name, ttl_ms): (String, i64)| {
            services
                .cluster()
                .lease(&name, ttl(ttl_ms)?)
                .map(|token| token.map(|token| i64::try_from(token).unwrap_or(i64::MAX)))
                .map_err(runtime)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "renew",
        function(
            lua,
            state,
            move |_, (name, token, ttl_ms): (String, i64, i64)| {
                let Ok(token) = u64::try_from(token) else {
                    return Ok(false);
                };
                services
                    .cluster()
                    .renew(&name, token, ttl(ttl_ms)?)
                    .map_err(runtime)
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "release",
        function(lua, state, move |_, (name, token): (String, i64)| {
            let Ok(token) = u64::try_from(token) else {
                return Ok(false);
            };
            services.cluster().release(&name, token).map_err(runtime)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "is_leader",
        function(lua, state, move |_, ()| Ok(services.cluster().is_leader()))?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "peer",
        function(lua, state, move |_, ()| Ok(services.cluster().peer()))?,
    )?;
    Ok(api)
}

/// `rift.background`: answers that come later and jobs for the worker
/// script (`[background]` in the manifest).
///
/// - `defer()` inside a decision hook returns a ticket; the hook's own
///   result is ignored and `resolve(ticket, decision)` answers it, with the
///   table the hook would have returned (`nil` continues).
/// - `run(payload, fn?)` queues a string for a worker and returns a handle.
///   `fn(result, error)` runs in the plugin when the job is done; without
///   it `rift.on("job_done", fn(handle, result, error))` handlers do.
/// - `is_worker()` tells the worker script from the plugin.
fn background(lua: &Lua, state: &Rc<State>) -> mlua::Result<Table> {
    let api = lua.create_table()?;
    let services = state.services.clone();
    api.raw_set(
        "defer",
        function(lua, state, move |_, ()| {
            services.background().defer().map_err(runtime)
        })?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "resolve",
        function(lua, state, move |_, (ticket, decision): (u64, Value)| {
            // Which decisions a hook accepts depends on the hook, which the
            // host checks when it applies the answer; here only the shape.
            let hook = match &decision {
                Value::Table(table) if table.contains_key("deny")? => "login",
                Value::Table(table) if table.contains_key("server")? => "initial_server",
                _ => "command",
            };
            let background = services.background();
            let action = background
                .for_ticket(ticket, || crate::extensions::parse_action(hook, decision))?;
            background.resolve(ticket, action).map_err(runtime)
        })?,
    )?;
    let run = state.clone();
    api.raw_set(
        "run",
        function(
            lua,
            state,
            move |_, (payload, callback): (LuaString, Option<Function>)| {
                let handle = run
                    .services
                    .background()
                    .run(payload.as_bytes().to_vec())
                    .map_err(runtime)?;
                if let Some(callback) = callback {
                    run.job_callbacks.borrow_mut().insert(handle, callback);
                }
                Ok(handle)
            },
        )?,
    )?;
    let services = state.services.clone();
    api.raw_set(
        "is_worker",
        function(lua, state, move |_, ()| Ok(services.is_worker()))?,
    )?;
    Ok(api)
}

/// `rift.publish(subject, payload, reply?)` and
/// `rift.request(subject, payload, timeout_ms?)`.
fn messaging(lua: &Lua, rift: &Table, state: &Rc<State>) -> mlua::Result<()> {
    let services = state.services.clone();
    rift.raw_set(
        "publish",
        function(
            lua,
            state,
            move |lua, (subject, payload, reply): (String, LuaString, Option<String>)| {
                let delivered = services
                    .bus()
                    .publish(&subject, &payload.as_bytes(), reply.as_deref())
                    .map_err(runtime)?;
                let result = lua.create_table()?;
                result.raw_set("delivered", delivered)?;
                Ok(result)
            },
        )?,
    )?;
    let services = state.services.clone();
    rift.raw_set(
        "request",
        function(
            lua,
            state,
            move |lua, (subject, payload, timeout_ms): (String, LuaString, Option<u32>)| {
                let reply = services
                    .bus()
                    .request(
                        &subject,
                        &payload.as_bytes(),
                        timeout_ms.unwrap_or(DEFAULT_REQUEST_MS),
                    )
                    .map_err(runtime)?;
                lua.create_string(reply)
            },
        )?,
    )?;
    Ok(())
}
