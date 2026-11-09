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
    api::{BossBarSpec, CommandSpec, Hook, Level, PlayerInfo, SuggestKind, TitleSpec},
    services::Services,
};
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
    Ok(player)
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
    rift.raw_set("api_version", 2)?;
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
    rift.raw_set("meta", meta)?;

    registration(lua, &rift, state)?;
    helpers(lua, &rift, state)?;
    rift.raw_set("players", players(lua, state)?)?;
    rift.raw_set("store", store(lua, state)?)?;
    rift.raw_set("permissions", permissions(lua, state)?)?;
    rift.raw_set("servers", servers(lua, state)?)?;
    rift.raw_set("timers", timers(lua, state)?)?;
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
                    "rift.command expects a name and { permission = ..., run = function(ctx) ... end }",
                ));
            };
            let name = name.to_str()?.to_owned();
            crate::extensions::fields(&definition, &["permission", "run", "suggest"])?;
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
            commands.commands.borrow_mut().push(Command {
                spec: CommandSpec {
                    name,
                    permission,
                    suggest,
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
            move |_, (name, every_ms, callback): (Value, Value, Value)| {
                let (Value::String(name), Value::Function(callback)) = (name, callback) else {
                    return Err(runtime(
                        "rift.every expects a job name, an interval in milliseconds and a function",
                    ));
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
                let handle = every.services.timers().every(ms);
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
            let (queue, callback) = match (
                args.next().unwrap_or(Value::Nil),
                args.next().unwrap_or(Value::Nil),
            ) {
                (Value::Function(callback), Value::Nil) => (None, Some(callback)),
                (Value::String(queue), Value::Function(callback)) => {
                    (Some(queue.to_str()?.to_owned()), Some(callback))
                }
                (Value::String(queue), Value::Nil) => (Some(queue.to_str()?.to_owned()), None),
                (Value::Nil, Value::Nil) => (None, None),
                _ => {
                    return Err(runtime(
                        "rift.subscribe expects a subject, an optional queue group and an optional function",
                    ));
                }
            };
            let Value::String(subject) = subject else {
                return Err(runtime("rift.subscribe expects a subject"));
            };
            let subject = subject.to_str()?.to_owned();
            subscribe.require_hook(Hook::Message, "rift.subscribe")?;
            let handle = subscribe
                .services
                .bus()
                .subscribe(&subject, queue.as_deref())
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
    type Send = fn(&Services, &str, &str) -> Result<bool, String>;
    let texts: [(&str, Send); 3] = [
        ("message", |services, target, text| {
            services.players().message(target, text)
        }),
        ("action_bar", |services, target, text| {
            services.players().action_bar(target, text)
        }),
        ("kick", |services, target, text| {
            services.players().kick(target, text)
        }),
    ];
    for (name, send) in texts {
        let services = state.services.clone();
        api.raw_set(
            name,
            function(lua, state, move |_, (target, text): (String, String)| {
                send(&services, &target, &text).map_err(runtime)
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
            } = crate::extensions::players::title(options)?
            else {
                unreachable!("title builds a title action");
            };
            services
                .players()
                .title(
                    &target,
                    &TitleSpec {
                        title,
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
                let crate::extensions::PlayerAction::TabList(changes) =
                    crate::extensions::players::tab_list(options)?
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
                    crate::extensions::players::boss_bar(name, options)?
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
            move |_, (message, server): (String, Option<String>)| {
                services
                    .players()
                    .broadcast_to(&message, server.as_deref())
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
    for (name, add) in [("add_group", true), ("remove_group", false)] {
        let services = state.services.clone();
        api.raw_set(
            name,
            function(lua, state, move |_, (uuid, group): (String, String)| {
                let permissions = services.permissions();
                if add {
                    permissions.add_group(&uuid, &group)
                } else {
                    permissions.remove_group(&uuid, &group)
                }
                .map_err(runtime)
            })?,
        )?;
    }
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
    Ok(api)
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
