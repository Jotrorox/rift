use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use mlua::{
    Function, HookTriggers, Lua, LuaOptions, StdLib, Table, Value, VmState, chunk::ChunkMode,
};

use crate::hooks::{ConnectionInfo, RouteDecision, RouteError};

mod modules;
mod source;
pub(crate) use source::ScriptSource;

pub(crate) const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub(crate) const MAX_CONCURRENT: usize = 4;
pub(crate) const EXECUTION_TIMEOUT: Duration = Duration::from_millis(50);
const MEMORY_LIMIT: usize = 8 * 1024 * 1024;
const INSTRUCTION_LIMIT: u32 = 100_000;
const HOOK_INTERVAL: u32 = 1000;
const MAX_REASON_BYTES: usize = 1024;

/// Immutable source snapshot. No Lua handles escape into the async runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteScript {
    source: ScriptSource,
}

impl RouteScript {
    pub(crate) fn new(source: &ScriptSource) -> Self {
        Self {
            source: source.clone(),
        }
    }

    pub(crate) fn evaluate(
        &self,
        connection: &ConnectionInfo,
        deadline: Instant,
        messaging: Option<crate::messaging::Broker>,
    ) -> Result<RouteDecision, RouteError> {
        let run = || -> mlua::Result<RouteDecision> {
            let (lua, root) = load(&self.source, deadline)?;
            install_messaging(&lua, messaging, deadline)?;
            let Value::Table(root) = root else {
                return Err(mlua::Error::runtime("configuration must return a table"));
            };
            let hook: Function = root.raw_get("on_route")?;
            let input = lua.create_table()?;
            input.raw_set("listener", connection.listener.as_str())?;
            input.raw_set("peer_addr", connection.peer_addr.to_string())?;
            input.raw_set("peer_ip", connection.peer_addr.ip().to_string())?;
            input.raw_set("peer_port", connection.peer_addr.port())?;
            input.raw_set("local_addr", connection.local_addr.to_string())?;
            input.raw_set("local_ip", connection.local_addr.ip().to_string())?;
            input.raw_set("local_port", connection.local_addr.port())?;
            input.raw_set("default_backend", connection.default_backend.as_deref())?;
            let result = decision(hook.call::<Value>(input)?)?;
            check_deadline(deadline)?;
            Ok(result)
        };
        run().map_err(|error| {
            RouteError::Script(format!("{}: on_route: {error}", self.source.entry.name))
        })
    }
}

/// Install only after configuration evaluation: loading/validating a config
/// must never send messages. Callback functions resolve this global at call time.
pub(crate) fn install_messaging(
    lua: &Lua,
    broker: Option<crate::messaging::Broker>,
    deadline: Instant,
) -> mlua::Result<()> {
    let api: Table = lua.named_registry_value("rift.api")?;
    api.raw_set("enabled", broker.is_some())?;
    let runtime: Table = lua.named_registry_value("rift.runtime")?;
    let publications = Rc::new(Cell::new(0usize));
    let bytes = Rc::new(Cell::new(0usize));
    // Every publication, including `rift.servers` requests, shares one budget.
    let send = move |subject: &str, reply: Option<&str>, payload: &[u8]| {
        check_deadline(deadline)?;
        let broker = broker
            .as_ref()
            .ok_or_else(|| mlua::Error::runtime("messaging broker unavailable"))?;
        let count = publications.get() + 1;
        let total = bytes.get().saturating_add(payload.len());
        if count > 256 || total > 1024 * 1024 {
            return Err(mlua::Error::runtime(
                "script messaging budget exceeded (256 messages / 1 MiB)",
            ));
        }
        publications.set(count);
        bytes.set(total);
        broker
            .publish_with_reply(subject, reply, bytes::Bytes::copy_from_slice(payload))
            .map_err(mlua::Error::external)
    };
    let publish = send.clone();
    runtime.raw_set(
        "publish",
        lua.create_function(
            move |lua,
                  (subject, payload, reply): (
                mlua::LuaString,
                mlua::LuaString,
                Option<mlua::LuaString>,
            )| {
                let subject = subject.to_str()?;
                let reply = reply.as_ref().map(mlua::LuaString::to_str).transpose()?;
                let report = publish(
                    subject.as_ref(),
                    reply.as_ref().map(|value| value.as_ref()),
                    payload.as_bytes().as_ref(),
                )?;
                let result = lua.create_table()?;
                result.raw_set("delivered", report.delivered)?;
                result.raw_set("slow_consumers", report.slow_consumers)?;
                Ok(result)
            },
        )?,
    )?;
    install_servers(lua, &runtime, send)
}

/// `rift.servers` submits the same `rift.control.*` requests as server
/// plugins on the message bus. Requests are asynchronous: the result is
/// published to `reply` (an `_INBOX.` subject) once the operation finishes,
/// and creations and removals are announced on `rift.events.instances`.
fn install_servers(
    lua: &Lua,
    runtime: &Table,
    send: impl Fn(&str, Option<&str>, &[u8]) -> mlua::Result<crate::messaging::PublishReport>
    + Clone
    + 'static,
) -> mlua::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let servers = lua.create_table()?;
    for (method, field) in [
        ("create", "group"),
        ("remove", "name"),
        ("start", "name"),
        ("stop", "name"),
        ("download", "template"),
    ] {
        let send = send.clone();
        servers.raw_set(
            method,
            lua.create_function(move |_, (target, reply): (String, Option<String>)| {
                if target.is_empty() || target.len() > 256 {
                    return Err(mlua::Error::runtime(format!(
                        "rift.servers.{method}: expected a {field} name of 1..=256 bytes"
                    )));
                }
                let reply = match reply {
                    Some(reply) if reply.starts_with("_INBOX.") && reply.len() <= 256 => reply,
                    Some(_) => {
                        return Err(mlua::Error::runtime(format!(
                            "rift.servers.{method}: reply subjects must start with _INBOX."
                        )));
                    }
                    None => format!(
                        "_INBOX.rift.lua.{}",
                        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    ),
                };
                let payload = serde_json::json!({ field: target }).to_string();
                let report = send(
                    &format!("rift.control.{method}"),
                    Some(&reply),
                    payload.as_bytes(),
                )?;
                if report.delivered == 0 {
                    return Err(mlua::Error::runtime(
                        "rift.servers: the control service is unavailable",
                    ));
                }
                Ok(reply)
            })?,
        )?;
    }
    runtime.raw_set("servers", servers)
}

pub(crate) fn check_deadline(deadline: Instant) -> mlua::Result<()> {
    if Instant::now() >= deadline {
        return Err(mlua::Error::runtime("script deadline exceeded"));
    }
    Ok(())
}

/// Configuration evaluation and routing share the same restricted environment.
pub(crate) fn load(source: &ScriptSource, deadline: Instant) -> mlua::Result<(Lua, Value)> {
    if source.entry.source.len() > MAX_SOURCE_BYTES {
        return Err(mlua::Error::runtime("script exceeds 256 KiB source limit"));
    }
    check_deadline(deadline)?;
    let lua = Lua::new_with(
        StdLib::MATH | StdLib::STRING | StdLib::TABLE | StdLib::JIT,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(MEMORY_LIMIT)?;
    let globals = lua.globals();
    // LuaJIT traces can bypass instruction hooks. Disable compilation before
    // loading user code, then remove every route back to the JIT library.
    globals
        .raw_get::<Table>("jit")?
        .raw_get::<Function>("off")?
        .call::<()>(())?;
    retain(
        &globals,
        &[
            "_G", "_VERSION", "assert", "error", "ipairs", "next", "pairs", "select", "tonumber",
            "tostring", "type", "unpack", "math", "string", "table",
        ],
    )?;
    // Pattern matching can run for unbounded time in native code without
    // visiting a Lua hook. Only expose simple, memory-bounded operations.
    retain(
        &globals.raw_get::<Table>("string")?,
        &[
            "byte", "char", "len", "lower", "rep", "reverse", "sub", "upper",
        ],
    )?;
    retain(
        &globals.raw_get::<Table>("table")?,
        &["concat", "insert", "remove", "maxn"],
    )?;
    // Native LuaJIT table.insert can shift billions of entries for negative
    // positions or sparse tables with a huge length. Keep this loop in Lua so
    // it shares the instruction budget. Capture primitives before user code
    // can replace them.
    let insert: Function = lua
        .load(
            r#"
        local type, error, select = type, error, select
        return function(t, ...)
            if type(t) ~= 'table' then error('table.insert expects a table') end
            local count, length = select('#', ...), #t
            local position, value
            if count == 1 then
                position, value = length + 1, ...
            elseif count == 2 then
                position, value = ...
            else
                error('table.insert expects two or three arguments')
            end
            if type(position) ~= 'number' or position % 1 ~= 0
                or position < 1 or position > length + 1 then
                error('table.insert position out of bounds')
            end
            for i = length, position, -1 do t[i + 1] = t[i] end
            t[position] = value
        end
    "#,
        )
        .set_name("=rift table.insert")
        .eval()?;
    globals
        .raw_get::<Table>("table")?
        .raw_set("insert", insert)?;
    // pcall/xpcall, coroutines, dynamic loaders, metatables/finalizers, I/O, native
    // modules and debug access are deliberately absent. In particular a script
    // cannot catch a budget error or escape to a thread without an active hook.
    let instructions = Cell::new(0);
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_INTERVAL),
        move |_, _| {
            check_deadline(deadline)?;
            let count = instructions.get() + HOOK_INTERVAL;
            instructions.set(count);
            if count >= INSTRUCTION_LIMIT {
                return Err(mlua::Error::runtime("script instruction limit exceeded"));
            }
            Ok(VmState::Continue)
        },
    )?;
    let finalize = modules::install(&lua, source, deadline)?;
    let root: Value = lua
        .load(source.entry.source.as_ref())
        .set_name(source.entry.name.as_ref())
        .set_mode(ChunkMode::Text)
        .eval()?;
    let root = finalize.call(root)?;
    check_deadline(deadline)?;
    Ok((lua, root))
}

fn retain(table: &Table, allowed: &[&str]) -> mlua::Result<()> {
    let keys = table
        .clone()
        .pairs::<String, Value>()
        .map(|pair| pair.map(|(key, _)| key))
        .collect::<mlua::Result<Vec<_>>>()?;
    for key in keys {
        if !allowed.contains(&key.as_str()) {
            table.raw_remove(key)?;
        }
    }
    Ok(())
}

fn decision(value: Value) -> mlua::Result<RouteDecision> {
    if value.is_nil() {
        return Ok(RouteDecision::Default);
    }
    let Value::Table(table) = value else {
        return Err(mlua::Error::runtime(
            "expected nil, { backend = name }, or { reject = true, reason = optional_string }",
        ));
    };
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        match key {
            Value::String(key)
                if matches!(key.to_str()?.as_ref(), "backend" | "reject" | "reason") => {}
            _ => return Err(mlua::Error::runtime("unknown routing decision field")),
        }
    }
    let backend: Value = table.raw_get("backend")?;
    let reject: Value = table.raw_get("reject")?;
    let reason: Value = table.raw_get("reason")?;
    match (backend, reject, reason) {
        (Value::String(backend), Value::Nil, Value::Nil) => {
            let backend = backend.to_str()?.to_owned();
            if backend.trim().is_empty() {
                return Err(mlua::Error::runtime("backend name must not be empty"));
            }
            Ok(RouteDecision::Backend(backend))
        }
        (Value::Nil, Value::Boolean(true), reason) => {
            let reason = match reason {
                Value::Nil => None,
                Value::String(reason) if reason.as_bytes().len() <= MAX_REASON_BYTES => {
                    Some(reason.to_str()?.to_owned())
                }
                _ => {
                    return Err(mlua::Error::runtime(
                        "reject reason must be a UTF-8 string of at most 1024 bytes",
                    ));
                }
            };
            Ok(RouteDecision::Reject { reason })
        }
        _ => Err(mlua::Error::runtime(
            "expected exactly one of backend or reject = true; reason is only valid with reject",
        )),
    }
}
