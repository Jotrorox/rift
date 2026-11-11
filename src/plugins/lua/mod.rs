//! The persistent Lua engine: a plugin folder `plugins/<name>/` with
//! `engine = "lua"` runs in its own long-lived LuaJIT state on the plugin's
//! thread. Handlers see the same `rift` API and the same `ctx` tables as in
//! `rift.lua`, and their results pass the same validation, so a handler body
//! moves between the two unchanged. What differs is the environment:
//!
//! - The whole safe standard library, including `io`, `os`, `package`,
//!   `coroutine`, `pcall` and `string.find`/`match`/`gsub`; `ffi` only with
//!   `lua.ffi = true`. `os.exit` is refused and `os.getenv` sees only the
//!   variables in `capabilities.env` when that list is set.
//! - `require` searches the plugin folder (`?.lua`, `?/init.lua`) and its
//!   `lua/` subfolder. Native modules load only with
//!   `lua.native_modules = true` (from the folder, its `lua/` subfolder and
//!   LuaJIT's default `cpath`).
//! - Globals, modules and upvalues persist between calls. The JIT compiler is
//!   on unless `lua.jit = false`; there is no instruction budget and memory
//!   is limited only when `lua.memory_limit_mb` is set.
//! - Each call runs under its manifest deadline (`init`, `reload` and
//!   `shutdown` under theirs; observers, timers and messages under
//!   [`OBSERVER_DEADLINE`]). An instruction hook and every Rift function
//!   raise shortly after the deadline passes (the host has applied
//!   `on_timeout` by then) or once Rift gives up on the plugin, and
//!   `pcall`, `xpcall` and `rift.try` rethrow those errors. LuaJIT runs hot
//!   loops as compiled traces that skip the instruction hook, so a compiled
//!   loop that never calls a Rift function cannot be interrupted: its calls
//!   time out under the manifest's policy and Rift abandons the thread on
//!   shutdown. With `lua.jit = false` everything is interpreted, the hook
//!   always fires and such a loop is interrupted too.
//!
//! The entry chunk (`entry`, default `init.lua`) receives the plugin name and
//! the decoded configuration table as `...`, like a script-mode plugin
//! folder, and registers with `rift.on`, `rift.command`, `rift.every` and
//! `rift.subscribe`; see [`bind`]. `rift.on("reload", fn)` receives the new
//! configuration table on a hot reload; without such a handler a hot reload
//! succeeds and the new configuration applies at the next restart.
//! `rift.on("shutdown", fn)` receives the reason before the state closes.
//!
//! A Lua error in a handler is a handler error; memory exhaustion, a panic
//! or an interrupt because Rift is stopping the plugin leave the state
//! unusable and fail the instance, which restarts per its manifest.
mod bind;
mod http;
#[cfg(test)]
mod tests;

use super::{
    PluginSpec,
    api::{Connection, Context, Event, Hook, Ping, Registration, Reply, StatusOverride},
    manifest::{LuaOptions, Manifest},
    services::Services,
    worker::{CallError, Engine},
};
use bind::{State, TimerAction};
use mlua::{Function, HookTriggers, Lua, LuaOptions as VmOptions, StdLib, Table, Value, VmState};
use std::{path::PathBuf, rc::Rc, time::Duration, time::Instant};

pub use super::manifest::OBSERVER_DEADLINE;
/// Guest code is interrupted this long after its deadline, so the host's own
/// timeout answers first and `on_timeout` (not `on_failure`) applies.
const GRACE: Duration = Duration::from_millis(100);
const HOOK_INTERVAL: u32 = 1000;
const MAX_ERROR: usize = 2048;

/// Why the engine interrupted running guest code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interrupt {
    Deadline,
    Stop,
}

impl std::fmt::Display for Interrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Deadline => "plugin call deadline exceeded",
            Self::Stop => "plugin is being stopped",
        })
    }
}

impl std::error::Error for Interrupt {}

/// The [`super::worker::EngineFactory`] for `engine = "lua"`.
pub fn factory(spec: &PluginSpec, services: Services) -> Result<Box<dyn Engine>, String> {
    LuaEngine::new(spec, services).map(|engine| Box::new(engine) as Box<dyn Engine>)
}

pub(crate) fn memory_error(error: &mlua::Error) -> bool {
    match error {
        mlua::Error::MemoryError(_) => true,
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => {
            memory_error(cause)
        }
        _ => false,
    }
}

fn interrupt(error: &mlua::Error) -> Option<Interrupt> {
    match error {
        mlua::Error::ExternalError(error) => error.downcast_ref::<Interrupt>().copied(),
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => {
            interrupt(cause)
        }
        _ => None,
    }
}

/// What Lua code sees of an error a Rift function raised: the root cause,
/// and for a runtime error just its message.
pub(crate) fn message(error: &mlua::Error) -> String {
    match error {
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => {
            message(cause)
        }
        mlua::Error::RuntimeError(message) => message.clone(),
        other => other.to_string(),
    }
}

/// The error text without Lua's stack traceback, bounded.
fn describe(error: &mlua::Error) -> String {
    let text = error.to_string();
    let text = text.split("\nstack traceback:").next().unwrap_or_default();
    text.chars().take(MAX_ERROR).collect()
}

pub struct LuaEngine {
    lua: Lua,
    state: Rc<State>,
    name: String,
    entry: PathBuf,
    chunk: String,
    manifest: Manifest,
    /// Set when `reload` or `shutdown` left the state unusable; the next call
    /// fails the instance.
    poisoned: Option<String>,
}

impl LuaEngine {
    pub fn new(spec: &PluginSpec, services: Services) -> Result<Self, String> {
        let options = spec.manifest.lua.clone().unwrap_or_default();
        let entry = spec
            .entry()
            .ok_or_else(|| "a Lua plugin needs an entry script".to_owned())?;
        let lua = vm(&options)?;
        let state = Rc::new(State::new(services));
        let setup = || -> mlua::Result<()> {
            environment(&lua, spec, &options)?;
            bind::install(&lua, &state, spec)?;
            let hook = state.clone();
            lua.set_hook(
                HookTriggers::new().every_nth_instruction(HOOK_INTERVAL),
                move |_, _| match hook.interrupted() {
                    Some(interrupt) => Err(mlua::Error::external(interrupt)),
                    None => Ok(VmState::Continue),
                },
            )
        };
        setup().map_err(|error| describe(&error))?;
        let relative = spec.manifest.entry.clone().unwrap_or_default();
        Ok(Self {
            lua,
            state,
            name: spec.name.clone(),
            chunk: format!("@{}/{}", spec.name, relative.display()),
            entry,
            manifest: spec.manifest.clone(),
            poisoned: None,
        })
    }

    /// Run guest code under `limit` and classify a failure.
    fn guard<T>(
        &self,
        limit: Duration,
        work: impl FnOnce() -> mlua::Result<T>,
    ) -> Result<T, CallError> {
        self.state
            .deadline
            .set(Some(Instant::now() + limit + GRACE));
        let result = work();
        self.state.deadline.set(None);
        result.map_err(|error| {
            let message = describe(&error);
            let fatal = memory_error(&error)
                || interrupt(&error) == Some(Interrupt::Stop)
                || self.state.services.stop_requested();
            if fatal {
                CallError::Fatal(message)
            } else {
                CallError::Handler(message)
            }
        })
    }

    fn registration(&self) -> Registration {
        let commands: Vec<_> = self
            .state
            .commands
            .borrow()
            .iter()
            .map(|command| command.spec.clone())
            .collect();
        let mut hooks = self.state.hooks.borrow_mut();
        if !commands.is_empty() {
            hooks.insert(Hook::Command);
        }
        if commands
            .iter()
            .any(|command| command.suggest == super::api::SuggestKind::Dynamic)
        {
            hooks.insert(Hook::Suggest);
        }
        Registration {
            commands,
            chat: hooks.contains(&Hook::Chat),
            hooks: hooks.iter().copied().collect(),
        }
    }

    /// The first non-nil result of `event`'s handlers, in registration order.
    fn first(&self, event: &str, input: impl mlua::IntoLua + Clone) -> mlua::Result<Value> {
        for handler in self.state.handlers(event) {
            let value: Value = handler.call(input.clone())?;
            if !value.is_nil() {
                return Ok(value);
            }
        }
        Ok(Value::Nil)
    }

    /// Every handler of an observer event; results are ignored.
    fn all(&self, event: &str, input: impl mlua::IntoLua + Clone) -> mlua::Result<()> {
        for handler in self.state.handlers(event) {
            handler.call::<()>(input.clone())?;
        }
        Ok(())
    }

    /// The `ctx` table script-mode extension callbacks receive.
    fn context(&self, input: &Context, hook: Hook) -> mlua::Result<Table> {
        let lua = &self.lua;
        let ctx = lua.create_table()?;
        ctx.raw_set("api_version", 2)?;
        ctx.raw_set("connection_id", input.id.to_string())?;
        ctx.raw_set("uuid", input.uuid.as_str())?;
        ctx.raw_set("name", input.name.as_str())?;
        ctx.raw_set("authenticated", true)?;
        ctx.raw_set("listener", input.listener.as_str())?;
        ctx.raw_set("hostname", input.hostname.as_str())?;
        ctx.raw_set("peer_ip", input.peer_ip.as_str())?;
        ctx.raw_set("protocol", input.protocol)?;
        ctx.raw_set("server", input.server.as_deref())?;
        ctx.raw_set("default_server", input.default_server.as_deref())?;
        ctx.raw_set("target", input.target.as_deref())?;
        ctx.raw_set("reason", input.reason.as_deref())?;
        ctx.raw_set("success", input.success)?;
        ctx.raw_set("command", input.command.as_deref())?;
        ctx.raw_set("args", input.args.as_str())?;
        if hook == Hook::Suggest {
            ctx.raw_set("word", input.word.as_str())?;
            ctx.raw_set("position", input.position)?;
        }
        if let Some(dialog) = &input.dialog {
            ctx.raw_set("action", dialog.action.as_str())?;
            let values = serde_json::Value::Object(dialog.values.clone());
            ctx.raw_set("values", bind::to_lua(lua, &values)?)?;
            if let Some(payload) = &dialog.payload {
                ctx.raw_set("payload", bind::to_lua(lua, payload)?)?;
            }
        }
        if let Some(chat) = &input.chat {
            ctx.raw_set("kind", chat.kind.name())?;
            ctx.raw_set("text", chat.text.as_str())?;
        }
        let services = self.state.services.clone();
        let uuid = input.uuid.clone();
        ctx.raw_set(
            "has_permission",
            bind::function(lua, &self.state, move |_, node: String| {
                Ok(services.permissions().has(&uuid, &node))
            })?,
        )?;
        Ok(ctx)
    }

    /// The `on_route` input table.
    fn connection(&self, input: &Connection) -> mlua::Result<Table> {
        let table = self.lua.create_table()?;
        table.raw_set("listener", input.listener.as_str())?;
        table.raw_set("peer_addr", input.peer_addr.as_str())?;
        table.raw_set("peer_ip", input.peer_ip.as_str())?;
        table.raw_set("peer_port", input.peer_port)?;
        table.raw_set("local_addr", input.local_addr.as_str())?;
        table.raw_set("local_ip", input.local_ip.as_str())?;
        table.raw_set("local_port", input.local_port)?;
        table.raw_set("default_backend", input.default_backend.as_deref())?;
        Ok(table)
    }

    /// The `on_status` input table (without `peer_addr`, which the plugin
    /// contract does not carry).
    fn ping(&self, input: &Ping) -> mlua::Result<Table> {
        let table = self.lua.create_table()?;
        table.raw_set("listener", input.listener.as_str())?;
        table.raw_set("hostname", input.hostname.as_str())?;
        table.raw_set("protocol", input.protocol)?;
        table.raw_set("peer_ip", input.peer_ip.as_str())?;
        table.raw_set("backend", input.backend.as_deref())?;
        table.raw_set("maintenance", input.maintenance)?;
        table.raw_set("online", input.online)?;
        table.raw_set("max_players", input.max_players)?;
        Ok(table)
    }

    /// The `on_http` input table.
    fn request(&self, input: &super::api::HttpRequest) -> mlua::Result<Table> {
        let table = self.lua.create_table()?;
        table.raw_set("method", input.method.as_str())?;
        table.raw_set("path", input.path.as_str())?;
        table.raw_set("query", input.query.as_str())?;
        table.raw_set("body", input.body.as_str())?;
        let headers = self.lua.create_table()?;
        for (key, value) in &input.headers {
            headers.raw_set(key.as_str(), value.as_str())?;
        }
        table.raw_set("headers", headers)?;
        table.raw_set("context", bind::to_lua(&self.lua, &input.context)?)?;
        Ok(table)
    }

    fn dispatch(&self, event: Event) -> mlua::Result<Reply> {
        let hook = event.hook();
        let name = hook.name();
        Ok(match event {
            Event::Route(connection) => Reply::Route(crate::script::decision(
                self.first(name, self.connection(&connection)?)?,
            )?),
            Event::Login(ctx)
            | Event::InitialServer(ctx)
            | Event::BeforeTransfer(ctx)
            | Event::Chat(ctx) => {
                let value = self.first(name, self.context(&ctx, hook)?)?;
                Reply::Action(crate::extensions::parse_action(name, value)?)
            }
            Event::Command(ctx) => {
                let command = ctx.command.clone().unwrap_or_default();
                let run = self
                    .state
                    .commands
                    .borrow()
                    .iter()
                    .find(|entry| entry.spec.name == command)
                    .map(|entry| entry.run.clone())
                    .ok_or_else(|| mlua::Error::runtime(format!("unknown command /{command}")))?;
                let value: Value = run.call(self.context(&ctx, hook)?)?;
                Reply::Action(crate::extensions::parse_action(name, value)?)
            }
            Event::Suggest(ctx) => {
                let command = ctx.command.clone().unwrap_or_default();
                let suggest = self
                    .state
                    .commands
                    .borrow()
                    .iter()
                    .find(|entry| entry.spec.name == command)
                    .and_then(|entry| entry.suggest.clone());
                match suggest {
                    None => Reply::Suggestions(Vec::new()),
                    Some(suggest) => {
                        let value: Value = suggest.call(self.context(&ctx, hook)?)?;
                        Reply::Suggestions(crate::extensions::parse_suggestions(name, value)?)
                    }
                }
            }
            Event::Status(ping) => {
                let value = self.first(name, self.ping(&ping)?)?;
                let changes =
                    crate::server_list::status_override(value).map_err(mlua::Error::runtime)?;
                Reply::Status(changes.map(|changes| StatusOverride {
                    motd_json: changes.motd.map(|motd| motd.to_string()),
                    version: changes.version,
                    protocol: changes.protocol,
                    online: changes.online,
                    max_players: changes.max_players,
                    sample: changes.sample,
                    favicon: changes.favicon.map(|favicon| favicon.to_string()),
                }))
            }
            Event::Http(request) => {
                let value = self.first(name, self.request(&request)?)?;
                Reply::Http(crate::http_script::response(value)?)
            }
            Event::Join(ctx)
            | Event::AfterTransfer(ctx)
            | Event::Disconnect(ctx)
            | Event::Dialog(ctx) => {
                // As in script mode, observers must return nil.
                let input = self.context(&ctx, hook)?;
                for handler in self.state.handlers(name) {
                    let value: Value = handler.call(input.clone())?;
                    crate::extensions::parse_action(name, value)?;
                }
                Reply::None
            }
            Event::Message(message) => {
                let callback =
                    self.state
                        .subscriptions
                        .borrow()
                        .get(&message.handle)
                        .map(|subscription| {
                            (subscription.subject.clone(), subscription.callback.clone())
                        });
                // A message for a subscription that ended is dropped.
                if let Some((subject, callback)) = callback {
                    let input = self.lua.create_table()?;
                    input.raw_set("subject", message.subject.as_str())?;
                    input.raw_set("payload", self.lua.create_string(&message.payload)?)?;
                    input.raw_set("reply", message.reply.as_deref())?;
                    input.raw_set("subscription", subject)?;
                    input.raw_set("handle", message.handle)?;
                    match callback {
                        Some(callback) => callback.call::<()>(input)?,
                        None => self.all(name, input)?,
                    }
                }
                Reply::None
            }
            Event::Timer(handle) => {
                let action = {
                    let mut timers = self.state.timers.borrow_mut();
                    let action = timers.get(&handle).map(|timer| timer.action.clone());
                    if timers.get(&handle).is_some_and(|timer| timer.once) {
                        timers.remove(&handle);
                    }
                    action
                };
                match action {
                    Some(TimerAction::Job(job, callback)) => {
                        let ctx = self.lua.create_table()?;
                        ctx.raw_set("api_version", 2)?;
                        ctx.raw_set("job", job)?;
                        callback.call::<()>(ctx)?;
                    }
                    Some(TimerAction::Callback(callback)) => callback.call::<()>(handle)?,
                    Some(TimerAction::Event) => self.all(name, handle)?,
                    // A tick of a cancelled timer.
                    None => {}
                }
                Reply::None
            }
            Event::Event(event) => {
                let input = self.lua.create_table()?;
                input.raw_set("subject", event.subject.as_str())?;
                input.raw_set("payload_json", event.payload_json.as_str())?;
                if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&event.payload_json)
                {
                    input.raw_set("payload", bind::to_lua(&self.lua, &payload)?)?;
                }
                self.all(name, input)?;
                Reply::None
            }
        })
    }

    fn decode(&self, config_json: &str) -> Result<serde_json::Value, String> {
        serde_json::from_str(config_json)
            .map_err(|error| format!("invalid configuration JSON: {error}"))
    }
}

impl Engine for LuaEngine {
    fn init(&mut self, config_json: &str) -> Result<Registration, String> {
        let config = self.decode(config_json)?;
        let source = std::fs::read(&self.entry)
            .map_err(|error| format!("cannot read {}: {error}", self.entry.display()))?;
        self.state.initializing.set(true);
        let result = self.guard(self.manifest.init_deadline(), || {
            let config = bind::to_lua(&self.lua, &config)?;
            self.lua
                .load(&source)
                .set_name(&self.chunk)
                .call::<()>((self.name.as_str(), config))
        });
        self.state.initializing.set(false);
        result.map_err(|error| error.to_string())?;
        Ok(self.registration())
    }

    fn call(&mut self, event: Event) -> Result<Reply, CallError> {
        if let Some(error) = &self.poisoned {
            return Err(CallError::Fatal(error.clone()));
        }
        let limit = self.manifest.run_limit(event.hook());
        self.guard(limit, || self.dispatch(event))
    }

    fn reload(&mut self, config_json: &str) -> Result<(), String> {
        let handlers = self.state.handlers("reload");
        if handlers.is_empty() {
            return Ok(());
        }
        let config = self.decode(config_json)?;
        let result = self.guard(self.manifest.reload_deadline(), || {
            let config = bind::to_lua(&self.lua, &config)?;
            for handler in handlers {
                handler.call::<()>(config.clone())?;
            }
            Ok(())
        });
        match result {
            Ok(()) => Ok(()),
            Err(CallError::Fatal(error)) => {
                self.poisoned = Some(format!("reload: {error}"));
                Err(error)
            }
            Err(error) => Err(error.to_string()),
        }
    }

    fn shutdown(&mut self, reason: &str) {
        if self.poisoned.is_some() {
            return;
        }
        if let Err(error) = self.guard(self.manifest.shutdown_grace(), || {
            self.all("shutdown", reason)
        }) {
            self.state.services.log(
                super::api::Level::Error,
                &format!("shutdown handler: {error}"),
            );
        }
    }
}

fn vm(options: &LuaOptions) -> Result<Lua, String> {
    let libs = StdLib::ALL_SAFE | StdLib::IO | StdLib::OS;
    let lua = if options.ffi {
        // SAFETY: `lua.ffi = true` is the plugin author's explicit request
        // for LuaJIT's FFI. Persistent plugins are trusted code installed on
        // disk; the FFI can do anything native code can.
        unsafe { Lua::unsafe_new_with(libs | StdLib::FFI, VmOptions::default()) }
    } else {
        Lua::new_with(libs, VmOptions::default()).map_err(|error| describe(&error))?
    };
    if options.memory_limit_mb > 0 {
        lua.set_memory_limit(options.memory_limit_mb as usize * 1024 * 1024)
            .map_err(|error| describe(&error))?;
    }
    Ok(lua)
}

/// `package.path`/`cpath`, `os.exit`, `os.getenv` and the JIT. With
/// `lua.jit = false` the compiler is off before the entry script loads and
/// `jit.on` refuses, so the deadline hook fires in every loop.
fn environment(lua: &Lua, spec: &PluginSpec, options: &LuaOptions) -> mlua::Result<()> {
    let globals = lua.globals();
    let folder = spec.folder.display().to_string();
    let package: Table = globals.raw_get("package")?;
    package.raw_set(
        "path",
        format!("{folder}/?.lua;{folder}/?/init.lua;{folder}/lua/?.lua;{folder}/lua/?/init.lua"),
    )?;
    if options.native_modules {
        let default: String = package.raw_get("cpath")?;
        package.raw_set(
            "cpath",
            format!("{folder}/?.so;{folder}/lua/?.so;{default}"),
        )?;
    } else {
        package.raw_set("cpath", "")?;
        package.raw_set("loadlib", Value::Nil)?;
        // Keep the preload and Lua loaders; drop the two C loaders.
        let loaders: Table = package.raw_get("loaders")?;
        loaders.raw_set(4, Value::Nil)?;
        loaders.raw_set(3, Value::Nil)?;
    }
    let os: Table = globals.raw_get("os")?;
    os.raw_set(
        "exit",
        lua.create_function(|_, _: mlua::MultiValue| -> mlua::Result<()> {
            Err(mlua::Error::runtime(
                "os.exit would stop Rift; return from the handler instead",
            ))
        })?,
    )?;
    if let Some(allowed) = spec.manifest.capabilities.env.clone() {
        let getenv: Function = os.raw_get("getenv")?;
        os.raw_set(
            "getenv",
            lua.create_function(move |_, name: String| {
                if allowed.contains(&name) {
                    getenv.call::<Value>(name)
                } else {
                    Ok(Value::Nil)
                }
            })?,
        )?;
    }
    let jit: Table = globals.raw_get("jit")?;
    if options.jit {
        return jit.raw_get::<Function>("on")?.call::<()>(());
    }
    jit.raw_get::<Function>("off")?.call::<()>(())?;
    // Flush traces compiled during setup, then keep the compiler off.
    jit.raw_get::<Function>("flush")?.call::<()>(())?;
    jit.raw_set(
        "on",
        lua.create_function(|_, _: mlua::MultiValue| -> mlua::Result<()> {
            Err(mlua::Error::runtime(
                "jit.on: the manifest sets lua.jit = false so that deadlines can interrupt every loop",
            ))
        })?,
    )
}
