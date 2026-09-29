//! Version 1 authenticated extensions. Lua owns no shared mutable state; Rust
//! owns admission leases and FIFO tickets, whose lifetime is the client session.
use crate::{auth::AuthenticatedProfile, config::Config, messaging::Broker, script};
use mlua::{Function, Table, Value};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

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
    hooks: BTreeSet<String>,
    commands: BTreeMap<String, String>,
    permissions: BTreeMap<String, BTreeSet<String>>,
    pub queues: BTreeMap<String, usize>,
}

fn invalid(message: impl ToString) -> io::Error {
    io::Error::other(message.to_string())
}
fn fields(table: &Table, allowed: &[&str]) -> mlua::Result<()> {
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
fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.' || b == b'-'
        })
}
impl ExtensionScript {
    pub(crate) fn parse(root: &Table, source: &script::ScriptSource) -> mlua::Result<Option<Self>> {
        let value: Value = root.raw_get("extensions")?;
        if value.is_nil() {
            return Ok(None);
        }
        let Value::Table(table) = value else {
            return Err(mlua::Error::runtime("extensions must be a table"));
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
                "queues",
            ],
        )?;
        if table.raw_get::<Value>("api_version")? != Value::Integer(1) {
            return Err(mlua::Error::runtime("extensions.api_version must be 1"));
        }
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
                fields(&command, &["permission", "run"])?;
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
                commands.insert(name, permission);
            }
        }
        let mut permissions = BTreeMap::new();
        if let Some(entries) = table.raw_get::<Option<Table>>("permissions")? {
            for entry in entries.pairs::<String, Table>() {
                let (uuid, grants) = entry?;
                if uuid != "*"
                    && (uuid.len() != 32
                        || !uuid
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
                {
                    return Err(mlua::Error::runtime(
                        "permission keys must be lowercase UUID hex (32 digits) or *",
                    ));
                }
                let mut nodes = BTreeSet::new();
                for grant in grants.pairs::<String, bool>() {
                    let (node, granted) = grant?;
                    if !token(&node) || !granted {
                        return Err(mlua::Error::runtime(
                            "permission grants must be node = true",
                        ));
                    }
                    nodes.insert(node);
                }
                permissions.insert(uuid, nodes);
            }
        }
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
            hooks,
            commands,
            permissions,
            queues,
        }))
    }
    pub fn permits(&self, uuid: &str, permission: &str) -> bool {
        ["*", uuid].iter().any(|key| {
            self.permissions
                .get(*key)
                .is_some_and(|nodes| nodes.contains(permission))
        })
    }
    fn evaluate(
        &self,
        hook: &str,
        input: &Context,
        deadline: Instant,
        broker: Broker,
    ) -> io::Result<Action> {
        let run = || -> mlua::Result<Action> {
            let (lua, root) = script::load(&self.source, deadline)?;
            script::install_messaging(&lua, Some(broker), deadline)?;
            let Value::Table(root) = root else {
                return Err(mlua::Error::runtime("configuration must return a table"));
            };
            let extensions: Table = root.raw_get("extensions")?;
            let callback: Function = if hook == "command" {
                extensions
                    .raw_get::<Table>("commands")?
                    .raw_get::<Table>(input.command.as_deref().unwrap())?
                    .raw_get("run")?
            } else {
                extensions.raw_get(hook)?
            };
            let ctx = lua.create_table()?;
            ctx.set("api_version", 1)?;
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
            let grants: BTreeSet<String> = ["*", input.uuid.as_str()]
                .iter()
                .filter_map(|key| self.permissions.get(*key))
                .flatten()
                .cloned()
                .collect();
            ctx.set(
                "has_permission",
                lua.create_function(move |_, node: String| Ok(grants.contains(&node)))?,
            )?;
            let value: Value = callback.call(ctx)?;
            script::check_deadline(deadline)?;
            parse_action(hook, value)
        };
        run().map_err(|error| invalid(format!("extensions.{hook}: {error:.2048}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Continue,
    Deny(String),
    Server(String),
    Message(String),
    Queue(String),
    LeaveQueue,
}
fn parse_action(hook: &str, value: Value) -> mlua::Result<Action> {
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

#[derive(Debug, Clone, Default)]
pub struct Context {
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
    backends: Arc<BTreeSet<String>>,
    slots: Arc<Semaphore>,
    host: Arc<Mutex<HostState>>,
    broker: Broker,
}
impl Extensions {
    pub fn new(config: &Config, broker: Broker, previous: Option<&Self>) -> Self {
        Self {
            script: config.extensions.clone(),
            backends: Arc::new(config.backends.keys().cloned().collect()),
            broker,
            slots: previous.map_or_else(
                || Arc::new(Semaphore::new(script::MAX_CONCURRENT)),
                |p| p.slots.clone(),
            ),
            host: previous.map_or_else(
                || Arc::new(Mutex::new(HostState::default())),
                |p| p.host.clone(),
            ),
        }
    }
    pub fn session(&self, context: Context) -> Option<ExtensionSession> {
        self.script.as_ref()?;
        Some(ExtensionSession {
            runtime: self.clone(),
            context,
            pending: None,
            joined: false,
            jobs: Arc::new(Semaphore::new(1)),
        })
    }
    async fn call(&self, hook: &str, input: &Context, jobs: &Arc<Semaphore>) -> io::Result<Action> {
        let script = self.script.as_ref().unwrap();
        if hook != "command" && !script.hooks.contains(hook) {
            return Ok(Action::Continue);
        }
        let session_job = jobs
            .clone()
            .try_acquire_owned()
            .map_err(|_| invalid("previous session callback is still running"))?;
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| invalid("extension capacity exhausted"))?;
        let script = script.clone();
        let input = input.clone();
        let hook = hook.to_owned();
        let broker = self.broker.clone();
        let deadline = Instant::now() + script::EXECUTION_TIMEOUT;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _session_job = session_job;
            script.evaluate(&hook, &input, deadline, broker)
        });
        let action = tokio::time::timeout(script::EXECUTION_TIMEOUT, task)
            .await
            .map_err(invalid)?
            .map_err(invalid)??;
        if let Action::Server(server) | Action::Queue(server) = &action
            && !self.backends.contains(server)
        {
            return Err(invalid("extension selected an unknown backend"));
        }
        Ok(action)
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
    pub fn commands(&self) -> Vec<String> {
        self.runtime
            .script
            .as_ref()
            .unwrap()
            .commands
            .keys()
            .cloned()
            .collect()
    }
    pub async fn decision(&self, hook: &str) -> io::Result<Action> {
        self.runtime.call(hook, &self.context, &self.jobs).await
    }
    pub async fn command(&self, command: &str) -> io::Result<Action> {
        let (name, args) = command
            .split_once(char::is_whitespace)
            .unwrap_or((command, ""));
        let script = self.runtime.script.as_ref().unwrap();
        let permission = script
            .commands
            .get(name)
            .ok_or_else(|| invalid("unknown extension command"))?;
        if !script.permits(&self.context.uuid, permission) {
            return Ok(Action::Message(
                "You do not have permission to use this command.".into(),
            ));
        }
        let mut input = self.context.clone();
        input.command = Some(name.into());
        input.args = args.trim().into();
        self.runtime.call("command", &input, &self.jobs).await
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
        if let Some(capacity) = self.runtime.script.as_ref().unwrap().queues.get(target) {
            let used = host
                .leases
                .values()
                .filter(|servers| servers.contains(target))
                .count();
            let head = host
                .queues
                .get(target)
                .and_then(|q| q.front())
                .map(|(id, _)| *id);
            if used >= *capacity || head.is_some_and(|id| id != self.context.id) {
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
        if !self
            .runtime
            .script
            .as_ref()
            .unwrap()
            .queues
            .contains_key(target)
        {
            return Err(invalid("No queue is configured for that server."));
        }
        if self.context.server.as_deref() == Some(target) {
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
            let used = host
                .leases
                .values()
                .filter(|servers| servers.contains(server))
                .count();
            used < self.runtime.script.as_ref().unwrap().queues[server]
        }))
    }
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
        let Ok(permit) = self.runtime.slots.clone().try_acquire_owned() else {
            eprintln!("rift: extension teardown notification dropped: capacity exhausted");
            return;
        };
        let deadline = Instant::now() + script::EXECUTION_TIMEOUT;
        let broker = self.runtime.broker.clone();
        let mut input = self.context.clone();
        let pending = self.pending.take();
        input.reason = Some("session_closed".into());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                let _permit = permit;
                let _session_job = session_job;
                if let Some(mut transfer) = pending {
                    transfer.success = Some(false);
                    if script.hooks.contains("after_transfer")
                        && let Err(error) =
                            script.evaluate("after_transfer", &transfer, deadline, broker.clone())
                    {
                        eprintln!("rift: {error}");
                    }
                }
                if script.hooks.contains("disconnect")
                    && let Err(error) = script.evaluate("disconnect", &input, deadline, broker)
                {
                    eprintln!("rift: {error}");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests;
