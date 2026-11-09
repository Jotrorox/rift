//! `engine = "wasm"`: WebAssembly components on Wasmtime.
//!
//! A plugin is a component built against the `plugin` world of
//! `rift:plugin@0.2.0` (`wit/rift-plugin.wit`). Each instance owns one
//! [`Store`] and one component instance, created on the plugin's own thread
//! and called synchronously, one call at a time, by the worker.
//!
//! - [`bindings`]: the `core` world's host traits, forwarding to
//!   [`Services`], and conversions to the neutral [`api`] types.
//! - [`wasi`]: the WASI 0.2 command imports (TinyGo needs the whole set) and
//!   `wasi:http`, restricted by the manifest's `[capabilities]`.
//! - [`limits`]: memory limits and epoch-based deadlines and stops.
//! - [`cache`]: compiled components in `local/plugincache`.
//!
//! One process-wide [`wasmtime::Engine`] compiles with Cranelift; compiled
//! components are kept in memory per plugin, so restarts and reloads of an
//! unchanged artifact instantiate without compiling again.
//!
//! Errors map onto the worker's contract: a trap, including a deadline or
//! stop interruption, is [`CallError::Fatal`] (the instance is discarded and
//! restarted per the manifest); `init` or `reload` returning `err` is a
//! handler error that leaves the instance as it was.
mod bindings;
mod cache;
mod limits;
#[cfg(test)]
mod tests;
mod wasi;

use super::{
    PluginSpec,
    api::{self, Event, Level, Registration, Reply},
    manifest::Manifest,
    services::Services,
    worker::{CallError, Engine as PluginEngine},
};
use bindings::{Core, CorePre};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, OnceLock},
    time::Instant,
};
use wasmtime::{
    Config, Engine, OptLevel, Store, StoreLimits, Trap,
    component::{Component, HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

/// Everything one store holds: the host API and the WASI state.
pub(crate) struct State {
    services: Services,
    wasi: WasiCtx,
    http: WasiHttpCtx,
    hooks: wasi::Http,
    table: ResourceTable,
    limits: StoreLimits,
    watch: limits::Watch,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            hooks: &mut self.hooks,
            table: &mut self.table,
            ctx: &mut self.http,
        }
    }
}

/// The process-wide engine.
fn engine() -> Result<&'static Engine, String> {
    static ENGINE: OnceLock<Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config
                .wasm_component_model(true)
                .epoch_interruption(true)
                .cranelift_opt_level(OptLevel::Speed);
            Engine::new(&config).map_err(|error| format!("cannot start Wasmtime: {error}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Rift's imports and all of WASI 0.2 with `wasi:http`, shared by every
/// plugin; capabilities are enforced per store.
fn linker() -> Result<&'static Linker<State>, String> {
    static LINKER: OnceLock<Result<Linker<State>, String>> = OnceLock::new();
    LINKER
        .get_or_init(|| {
            let mut linker = Linker::new(engine()?);
            wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
                .and_then(|()| wasmtime_wasi_http::p2::add_only_http_to_linker_sync(&mut linker))
                .and_then(|()| Core::add_to_linker::<_, HasSelf<State>>(&mut linker, |state| state))
                .map_err(|error| format!("cannot link the plugin API: {error}"))?;
            Ok(linker)
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn entry_name(spec: &PluginSpec) -> std::path::Display<'_> {
    spec.manifest
        .entry
        .as_deref()
        .unwrap_or(Path::new("entry"))
        .display()
}

/// The entry's bytes and their cache key.
fn read(spec: &PluginSpec) -> Result<(Vec<u8>, String), String> {
    let entry = spec
        .entry()
        .ok_or_else(|| "engine = \"wasm\" requires an entry".to_owned())?;
    let wasm = std::fs::read(&entry)
        .map_err(|error| format!("cannot read {}: {error}", entry.display()))?;
    let key = cache::key(engine()?, &wasm);
    Ok((wasm, key))
}

fn compile(spec: &PluginSpec, wasm: &[u8]) -> Result<Component, String> {
    Component::new(engine()?, wasm).map_err(|error| {
        format!(
            "{} is not a WebAssembly component (build it for wasm32-wasip2): {error:#}",
            entry_name(spec)
        )
    })
}

/// Resolve every import against the linker and find the handler exports.
fn prepare(spec: &PluginSpec, component: &Component) -> Result<CorePre<State>, String> {
    linker()?
        .instantiate_pre(component)
        .and_then(CorePre::new)
        .map_err(|error| {
            format!(
                "{} does not fit this Rift's rift:plugin {} host: {error:#}",
                entry_name(spec),
                api::API_VERSION
            )
        })
}

/// The plugin's component, ready to instantiate: from memory, from the disk
/// cache or freshly compiled (and then cached).
fn load(spec: &PluginSpec) -> Result<CorePre<State>, String> {
    type Loaded = HashMap<String, (String, CorePre<State>)>;
    static LOADED: OnceLock<Mutex<Loaded>> = OnceLock::new();
    let loaded = LOADED.get_or_init(Mutex::default);
    let (wasm, key) = read(spec)?;
    if let Some((cached, pre)) = loaded.lock().unwrap().get(&spec.name)
        && *cached == key
    {
        return Ok(pre.clone());
    }
    let path = cache::path(&spec.config_root, &spec.name, &key);
    let component = match cache::load(engine()?, &path, &key) {
        Some(component) => component,
        None => {
            let component = compile(spec, &wasm)?;
            if let Ok(serialized) = component.serialize() {
                // Best effort: a read-only configuration directory only costs
                // the next start a compile.
                let _ = cache::store(&path, &spec.name, &key, &serialized);
            }
            component
        }
    };
    let pre = prepare(spec, &component)?;
    loaded
        .lock()
        .unwrap()
        .insert(spec.name.clone(), (key, pre.clone()));
    Ok(pre)
}

/// `rift check --strict`: compile the plugin, check that it links against
/// this Rift, and store it in the cache that startup reads.
pub fn precompile(spec: &PluginSpec) -> Result<String, String> {
    let started = Instant::now();
    let (wasm, key) = read(spec)?;
    let component = compile(spec, &wasm)?;
    prepare(spec, &component)?;
    let serialized = component
        .serialize()
        .map_err(|error| format!("cannot serialize the compiled component: {error}"))?;
    let path = cache::path(&spec.config_root, &spec.name, &key);
    cache::store(&path, &spec.name, &key, &serialized)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(format!(
        "plugin {}: compiled {} in {} ms into {}",
        spec.name,
        entry_name(spec),
        started.elapsed().as_millis(),
        path.strip_prefix(&spec.config_root)
            .unwrap_or(&path)
            .display()
    ))
}

/// The [`EngineFactory`](super::worker::EngineFactory) for `engine = "wasm"`.
pub(crate) fn create(
    spec: &PluginSpec,
    services: Services,
) -> Result<Box<dyn PluginEngine>, String> {
    let pre = load(spec)?;
    let engine = engine()?;
    limits::start_ticker(engine);
    let stdout = wasi::LogPipe::new(services.clone(), "stdout", Level::Info);
    let stderr = wasi::LogPipe::new(services.clone(), "stderr", Level::Warn);
    let state = State {
        wasi: wasi::context(spec, stdout.clone(), stderr.clone())?,
        http: WasiHttpCtx::new(),
        hooks: wasi::Http {
            network: spec.manifest.capabilities.network,
        },
        table: ResourceTable::new(),
        limits: limits::store_limits(&spec.manifest),
        watch: limits::Watch::default(),
        services,
    };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limits);
    limits::watch(&mut store);
    limits::arm(
        &mut store,
        "instantiation",
        Some(spec.manifest.init_deadline()),
    );
    let core = match pre.instantiate(&mut store) {
        Ok(core) => core,
        Err(error) => {
            let reason = describe(&mut store, error);
            stdout.finish();
            stderr.finish();
            return Err(reason);
        }
    };
    Ok(Box::new(WasmEngine {
        store,
        core,
        manifest: spec.manifest.clone(),
        registration: None,
        broken: None,
        pipes: [stdout, stderr],
    }))
}

/// A readable reason for a failed guest call.
fn describe(store: &mut Store<State>, error: wasmtime::Error) -> String {
    if let Some(reason) = store.data_mut().watch.take_interrupted() {
        return reason;
    }
    match error.downcast_ref::<Trap>() {
        Some(trap) => format!("trap: {trap}"),
        None => format!("{error:#}"),
    }
}

struct WasmEngine {
    store: Store<State>,
    core: Core,
    manifest: Manifest,
    registration: Option<Registration>,
    /// Set by the first trap: the instance's state is suspect from then on.
    broken: Option<String>,
    pipes: [wasi::LogPipe; 2],
}

impl WasmEngine {
    fn fail(&mut self, error: wasmtime::Error) -> String {
        let reason = describe(&mut self.store, error);
        self.broken = Some(reason.clone());
        reason
    }
}

impl Drop for WasmEngine {
    fn drop(&mut self) {
        for pipe in &self.pipes {
            pipe.finish();
        }
    }
}

impl PluginEngine for WasmEngine {
    fn init(&mut self, config_json: &str) -> Result<Registration, String> {
        limits::arm(&mut self.store, "init", Some(self.manifest.init_deadline()));
        let guest = self.core.rift_plugin_handler();
        match guest.call_init(&mut self.store, config_json) {
            Ok(Ok(registration)) => {
                let registration = bindings::registration(registration);
                self.registration = Some(registration.clone());
                Ok(registration)
            }
            Ok(Err(message)) => Err(message),
            Err(error) => Err(self.fail(error)),
        }
    }

    fn call(&mut self, event: Event) -> Result<Reply, CallError> {
        if let Some(reason) = &self.broken {
            return Err(CallError::Fatal(reason.clone()));
        }
        let hook = event.hook();
        // The worker filters too; an export the plugin did not list is never
        // entered.
        if self
            .registration
            .as_ref()
            .is_some_and(|registration| !registration.wants(hook))
        {
            return Ok(Reply::default_for(hook));
        }
        // Observers are not awaited, but a looping one would wedge the
        // queue behind it: they run under the same limit as in Lua.
        limits::arm(
            &mut self.store,
            hook.name(),
            Some(self.manifest.run_limit(hook)),
        );
        let guest = self.core.rift_plugin_handler();
        let store = &mut self.store;
        let mut invalid = None;
        let mut decide = |action| match bindings::action(action) {
            Ok(action) => Reply::Action(action),
            Err(error) => {
                invalid = Some(error);
                Reply::Action(crate::extensions::Action::Continue)
            }
        };
        let result = match event {
            Event::Route(connection) => guest
                .call_route(store, &bindings::connection(connection))
                .map(|decision| Reply::Route(bindings::route_decision(decision))),
            Event::Login(context) => guest
                .call_login(store, &bindings::context(context))
                .map(&mut decide),
            Event::InitialServer(context) => guest
                .call_initial_server(store, &bindings::context(context))
                .map(&mut decide),
            Event::BeforeTransfer(context) => guest
                .call_before_transfer(store, &bindings::context(context))
                .map(&mut decide),
            Event::Command(context) => guest
                .call_command(store, &bindings::context(context))
                .map(&mut decide),
            Event::Suggest(context) => guest
                .call_suggest(store, &bindings::context(context))
                .map(|list| Reply::Suggestions(bindings::suggestions(list))),
            Event::Status(ping) => guest
                .call_status(store, &bindings::ping(ping))
                .map(|status| Reply::Status(status.map(bindings::status_override))),
            Event::Http(request) => guest
                .call_http(store, &bindings::http_request(request))
                .map(|response| Reply::Http(response.map(bindings::http_response))),
            Event::Join(context) => guest
                .call_join(store, &bindings::context(context))
                .map(|()| Reply::None),
            Event::AfterTransfer(context) => guest
                .call_after_transfer(store, &bindings::context(context))
                .map(|()| Reply::None),
            Event::Disconnect(context) => guest
                .call_disconnect(store, &bindings::context(context))
                .map(|()| Reply::None),
            Event::Message(message) => guest
                .call_message(store, &bindings::bus_message(message))
                .map(|()| Reply::None),
            Event::Timer(handle) => guest.call_timer(store, handle).map(|()| Reply::None),
            Event::Event(event) => guest
                .call_event(store, &bindings::rift_event(event))
                .map(|()| Reply::None),
        };
        let reply = result.map_err(|error| CallError::Fatal(self.fail(error)))?;
        match invalid {
            Some(error) => Err(CallError::Handler(format!("invalid decision: {error}"))),
            None => Ok(reply),
        }
    }

    fn reload(&mut self, config_json: &str) -> Result<(), String> {
        if let Some(reason) = &self.broken {
            return Err(reason.clone());
        }
        limits::arm(
            &mut self.store,
            "reload",
            Some(self.manifest.reload_deadline()),
        );
        let guest = self.core.rift_plugin_handler();
        match guest.call_reload(&mut self.store, config_json) {
            Ok(result) => result,
            Err(error) => Err(self.fail(error)),
        }
    }

    fn shutdown(&mut self, reason: &str) {
        if self.broken.is_some() {
            return;
        }
        limits::arm(
            &mut self.store,
            "shutdown",
            Some(self.manifest.shutdown_grace()),
        );
        let guest = self.core.rift_plugin_handler();
        if let Err(error) = guest.call_shutdown(&mut self.store, reason) {
            let reason = self.fail(error);
            self.store
                .data()
                .services
                .log(Level::Warn, &format!("shutdown: {reason}"));
        }
    }
}
