//! The Lua `rift.permissions` module of extensions API v3. Calls are
//! synchronous and stay in the process; writes go through
//! [`Authority::apply_with`] and have committed when they return, under the
//! callback's deadline, as the extension store's writes do.
//!
//! `on_change` and `contexts.provide` are registrations made while the
//! configuration is evaluated; `src/lua/api.lua` records them and the
//! extension runtime calls them (see `src/extensions.rs`).
use super::{Authority, PermError};
use crate::http_script::json_value;
use mlua::{Lua, Table, Value};
use rift_permissions::{
    Actor, Applied, Change, ChangeEvent, ContextSet, Node, NodeKind, SubjectRef, Tristate, Uuid,
    change::Op, node::NodeKey, unix_now,
};
use serde::Serialize;
use std::{cell::Cell, rc::Rc, sync::Arc, time::Instant};

/// Writes one callback may make.
const MAX_WRITES: usize = 64;
/// Longest audit reason in bytes.
const MAX_REASON: usize = 256;
/// Ops passed to `on_change` handlers per change; the rest are dropped.
const MAX_HANDLED_OPS: usize = 64;

fn error(message: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(format!("rift.permissions: {message}"))
}

fn failure(error: PermError) -> mlua::Error {
    match error {
        PermError::Storage(_) => mlua::Error::external(error),
        error => self::error(error),
    }
}

fn uuid(value: &Value) -> mlua::Result<Uuid> {
    match value {
        Value::String(text) => Uuid::parse(&text.to_str()?).map_err(error),
        _ => Err(error("expected a player UUID string")),
    }
}

fn string(value: &Value, what: &str) -> mlua::Result<String> {
    match value {
        Value::String(text) => Ok(text.to_str()?.to_owned()),
        _ => Err(error(format!("expected {what}"))),
    }
}

fn integer(value: &Value, what: &str) -> mlua::Result<i64> {
    match value {
        Value::Integer(value) => Ok(*value),
        Value::Number(value) if value.fract() == 0.0 && value.abs() < 9.0e15 => Ok(*value as i64),
        _ => Err(error(format!("expected {what}"))),
    }
}

fn priority(value: &Value) -> mlua::Result<i32> {
    i32::try_from(integer(value, "an integer priority")?)
        .map_err(|_| error("priorities are 32-bit integers"))
}

/// A context table: keys to a string or a list of strings.
fn contexts(value: &Value) -> mlua::Result<ContextSet> {
    let mut set = ContextSet::new();
    match value {
        Value::Nil => {}
        Value::Table(table) => {
            for pair in table.pairs::<Value, Value>() {
                let (key, values) = pair?;
                let key = string(&key, "context keys to be strings")?;
                add_values(&mut set, &key, values)?;
            }
        }
        _ => return Err(error("contexts must be a table")),
    }
    Ok(set)
}

/// Add a context value or a list of values, as a provider returns them.
/// `nil` and `false` add nothing.
pub(crate) fn add_values(set: &mut ContextSet, key: &str, values: Value) -> mlua::Result<()> {
    match values {
        Value::Nil | Value::Boolean(false) => Ok(()),
        Value::String(value) => set.insert(key, &value.to_str()?).map(drop).map_err(error),
        Value::Table(list) => {
            for value in list.sequence_values::<Value>() {
                let value = string(&value?, "context values to be strings")?;
                set.insert(key, &value).map_err(error)?;
            }
            Ok(())
        }
        _ => Err(error("context values must be strings or lists of strings")),
    }
}

/// Write options: `{ contexts = {...}, expires_in = s, expires_at = t, reason = "..." }`.
#[derive(Default)]
struct Options {
    contexts: ContextSet,
    expires_at: Option<u64>,
    reason: Option<String>,
}

const OPTION_FIELDS: [&str; 4] = ["contexts", "expires_in", "expires_at", "reason"];

impl Options {
    fn parse(value: &Value) -> mlua::Result<Self> {
        let Value::Table(table) = value else {
            return match value {
                Value::Nil => Ok(Self::default()),
                _ => Err(error("options must be a table")),
            };
        };
        let mut options = Self::default();
        for pair in table.pairs::<Value, Value>() {
            let (key, value) = pair?;
            let key = string(&key, "option names to be strings")?;
            match key.as_str() {
                "contexts" => options.contexts = contexts(&value)?,
                "expires_in" => {
                    let seconds = integer(&value, "expires_in in whole seconds")?;
                    if seconds <= 0 {
                        return Err(error("expires_in must be positive"));
                    }
                    options.expires_at = Some(unix_now().saturating_add(seconds as u64));
                }
                "expires_at" => {
                    let at = integer(&value, "expires_at in Unix seconds")?;
                    options.expires_at =
                        Some(u64::try_from(at).map_err(|_| error("expires_at must be positive"))?);
                }
                "reason" => {
                    let reason = string(&value, "a reason string")?;
                    if reason.len() > MAX_REASON || reason.chars().any(char::is_control) {
                        return Err(error(format!(
                            "reasons have at most {MAX_REASON} bytes without control characters"
                        )));
                    }
                    options.reason = Some(reason);
                }
                _ => return Err(error(format!("unknown option {key}"))),
            }
        }
        Ok(options)
    }

    /// The second argument of `unset`: options, or a bare context table.
    fn or_contexts(value: &Value) -> mlua::Result<Self> {
        if let Value::Table(table) = value {
            let options = OPTION_FIELDS
                .iter()
                .any(|field| !matches!(table.raw_get::<Value>(*field), Ok(Value::Nil)));
            if !options {
                return Ok(Self {
                    contexts: contexts(value)?,
                    ..Self::default()
                });
            }
        }
        Self::parse(value)
    }

    fn node(&self, node: Node) -> mlua::Result<Node> {
        let mut node = node.with_contexts(self.contexts.clone()).map_err(error)?;
        node.expires_at = self.expires_at;
        Ok(node)
    }

    fn key(&self, kind: NodeKind, key: &str) -> mlua::Result<NodeKey> {
        NodeKey::new(kind, key, self.contexts.clone()).map_err(error)
    }
}

/// Everything one callback's module shares.
#[derive(Clone)]
struct Module {
    authority: Arc<Authority>,
    actor: Actor,
    deadline: Instant,
    writes: Rc<Cell<usize>>,
}

impl Module {
    fn json(&self, lua: &Lua, value: &impl Serialize) -> mlua::Result<Value> {
        let value = serde_json::to_value(value).map_err(mlua::Error::external)?;
        json_value(lua, &value, 0, self.deadline)
    }

    fn write(&self, lua: &Lua, change: Change, reason: Option<&str>) -> mlua::Result<Table> {
        crate::script::check_deadline(self.deadline)?;
        let count = self.writes.get() + 1;
        if count > MAX_WRITES {
            return Err(error(format!("at most {MAX_WRITES} writes per callback")));
        }
        self.writes.set(count);
        let Applied { revision, ops, .. } = self
            .authority
            .apply_with(self.actor.clone(), change, reason, Some(self.deadline))
            .map_err(failure)?;
        let result = lua.create_table()?;
        result.raw_set("revision", revision)?;
        result.raw_set("changed", ops > 0)?;
        Ok(result)
    }

    /// `true`, `false` or `nil` for undefined.
    fn tristate(value: Tristate) -> Value {
        value.as_option().map_or(Value::Nil, Value::Boolean)
    }

    /// Explicit contexts, or the ones the proxy knows for the player.
    fn active(&self, uuid: Uuid, value: &Value) -> mlua::Result<ContextSet> {
        match value {
            Value::Nil => Ok(self.authority.contexts(uuid)),
            value => contexts(value),
        }
    }
}

/// Install `rift.permissions` for one callback. Writes are recorded with
/// `actor`, typically `lua:<hook>`.
pub(crate) fn install(
    lua: &Lua,
    runtime: &Table,
    authority: &Arc<Authority>,
    actor: Actor,
    deadline: Instant,
) -> mlua::Result<()> {
    let module = Module {
        authority: authority.clone(),
        actor,
        deadline,
        writes: Rc::new(Cell::new(0)),
    };
    let api = lua.create_table()?;
    let m = module.clone();
    api.raw_set(
        "check",
        lua.create_function(move |_, (player, node, active): (Value, Value, Value)| {
            crate::script::check_deadline(m.deadline)?;
            let uuid = uuid(&player)?;
            let node = string(&node, "a permission node")?;
            let active = m.active(uuid, &active)?;
            Ok(Module::tristate(m.authority.check(uuid, &node, &active)))
        })?,
    )?;
    let m = module.clone();
    api.raw_set(
        "compiled",
        lua.create_function(move |lua, player: Value| {
            crate::script::check_deadline(m.deadline)?;
            m.json(lua, &*m.authority.compiled(uuid(&player)?))
        })?,
    )?;
    let m = module.clone();
    api.raw_set(
        "meta",
        lua.create_function(move |lua, (player, active): (Value, Value)| {
            crate::script::check_deadline(m.deadline)?;
            let uuid = uuid(&player)?;
            let active = m.active(uuid, &active)?;
            m.json(lua, &m.authority.meta(uuid, &active))
        })?,
    )?;
    let m = module.clone();
    api.raw_set(
        "groups",
        lua.create_function(move |lua, ()| {
            crate::script::check_deadline(m.deadline)?;
            let list = lua.create_table()?;
            for group in m.authority.groups().values() {
                let entry = lua.create_table()?;
                entry.raw_set("name", group.name.as_str())?;
                entry.raw_set("weight", group.weight)?;
                entry.raw_set("display_name", group.display_name.as_deref())?;
                list.raw_push(entry)?;
            }
            Ok(list)
        })?,
    )?;
    let m = module.clone();
    api.raw_set(
        "user",
        lua.create_function(move |lua, player: Value| {
            crate::script::check_deadline(m.deadline)?;
            subject(lua, &m, SubjectRef::User(uuid(&player)?))
        })?,
    )?;
    let m = module.clone();
    api.raw_set(
        "group",
        lua.create_function(move |lua, name: Value| {
            crate::script::check_deadline(m.deadline)?;
            let name = rift_permissions::subject::normalize_group(&string(&name, "a group name")?)
                .map_err(error)?;
            subject(lua, &m, SubjectRef::Group(name))
        })?,
    )?;
    let m = module;
    api.raw_set(
        "track",
        lua.create_function(move |lua, name: Value| {
            crate::script::check_deadline(m.deadline)?;
            let name = rift_permissions::subject::normalize_track(&string(&name, "a track name")?)
                .map_err(error)?;
            track(lua, &m, name)
        })?,
    )?;
    runtime.raw_set("permissions", api)
}

/// Methods are called with a colon; the first argument is the object.
type Args = (Value, Value, Value, Value);

fn method(
    lua: &Lua,
    object: &Table,
    name: &str,
    module: &Module,
    body: impl Fn(&Lua, &Module, Value, Value, Value) -> mlua::Result<Value> + 'static,
) -> mlua::Result<()> {
    let module = module.clone();
    object.raw_set(
        name,
        lua.create_function(move |lua, (_this, a, b, c): Args| {
            crate::script::check_deadline(module.deadline)?;
            body(lua, &module, a, b, c)
        })?,
    )
}

/// A user or group object.
fn subject(lua: &Lua, module: &Module, subject: SubjectRef) -> mlua::Result<Table> {
    let object = lua.create_table()?;
    let s = subject.clone();
    method(
        lua,
        &object,
        "set",
        module,
        move |lua, m, node, value, options| {
            let Value::Boolean(value) = value else {
                return Err(error("set expects a node, true or false, and options"));
            };
            let options = Options::parse(&options)?;
            let node = options.node(
                Node::permission(&string(&node, "a permission node")?, value).map_err(error)?,
            )?;
            let change = Change::SetNode {
                subject: s.clone(),
                node,
            };
            m.write(lua, change, options.reason.as_deref())
                .map(Value::Table)
        },
    )?;
    let unset = |kind: NodeKind| {
        let s = subject.clone();
        move |lua: &Lua, m: &Module, key: Value, options: Value, _: Value| {
            let options = Options::or_contexts(&options)?;
            let key = match &key {
                Value::Integer(_) | Value::Number(_) => priority(&key)?.to_string(),
                key => string(key, "a node key")?,
            };
            let change = Change::UnsetNode {
                subject: s.clone(),
                key: options.key(kind, &key)?,
            };
            m.write(lua, change, options.reason.as_deref())
                .map(Value::Table)
        }
    };
    method(lua, &object, "unset", module, unset(NodeKind::Permission))?;
    method(
        lua,
        &object,
        "remove_parent",
        module,
        unset(NodeKind::Inherit),
    )?;
    method(lua, &object, "unset_meta", module, unset(NodeKind::Meta))?;
    method(
        lua,
        &object,
        "unset_prefix",
        module,
        unset(NodeKind::Prefix),
    )?;
    method(
        lua,
        &object,
        "unset_suffix",
        module,
        unset(NodeKind::Suffix),
    )?;
    let s = subject.clone();
    method(
        lua,
        &object,
        "add_parent",
        module,
        move |lua, m, group, options, _| {
            let options = Options::parse(&options)?;
            let node = options
                .node(Node::inherit(&string(&group, "a group name")?, true).map_err(error)?)?;
            let change = Change::SetNode {
                subject: s.clone(),
                node,
            };
            m.write(lua, change, options.reason.as_deref())
                .map(Value::Table)
        },
    )?;
    for (name, prefix) in [("set_prefix", true), ("set_suffix", false)] {
        let s = subject.clone();
        method(
            lua,
            &object,
            name,
            module,
            move |lua, m, level, text, options| {
                let (level, text) = (priority(&level)?, string(&text, "decoration text")?);
                let options = Options::parse(&options)?;
                let node = if prefix {
                    Node::prefix(level, &text)
                } else {
                    Node::suffix(level, &text)
                };
                let node = options.node(node.map_err(error)?)?;
                let change = Change::SetNode {
                    subject: s.clone(),
                    node,
                };
                m.write(lua, change, options.reason.as_deref())
                    .map(Value::Table)
            },
        )?;
    }
    let s = subject.clone();
    method(
        lua,
        &object,
        "set_meta",
        module,
        move |lua, m, key, value, options| {
            let value = match &value {
                Value::Integer(value) => value.to_string(),
                value => string(value, "a meta value string")?,
            };
            let options = Options::parse(&options)?;
            let node =
                options.node(Node::meta(&string(&key, "a meta key")?, &value).map_err(error)?)?;
            let change = Change::SetNode {
                subject: s.clone(),
                node,
            };
            m.write(lua, change, options.reason.as_deref())
                .map(Value::Table)
        },
    )?;
    let s = subject.clone();
    method(
        lua,
        &object,
        "clear",
        module,
        move |lua, m, active, _, _| {
            let contexts = match active {
                Value::Nil => None,
                active => Some(contexts(&active)?),
            };
            let change = Change::ClearSubject {
                subject: s.clone(),
                contexts,
            };
            m.write(lua, change, None).map(Value::Table)
        },
    )?;
    let s = subject.clone();
    method(lua, &object, "nodes", module, move |lua, m, _, _, _| {
        let nodes = match &s {
            SubjectRef::User(uuid) => m.authority.user(*uuid).map_err(failure)?.nodes,
            SubjectRef::Group(name) => m.authority.group(name).map(|g| g.nodes).unwrap_or_default(),
        };
        m.json(lua, &nodes)
    })?;
    match subject {
        SubjectRef::User(uuid) => {
            method(
                lua,
                &object,
                "set_primary_group",
                module,
                move |lua, m, group, _, _| {
                    let group = match group {
                        Value::Nil => None,
                        group => Some(string(&group, "a group name or nil")?),
                    };
                    m.write(lua, Change::SetPrimaryGroup { user: uuid, group }, None)
                        .map(Value::Table)
                },
            )?;
        }
        SubjectRef::Group(name) => group_methods(lua, &object, module, name)?,
    }
    Ok(object)
}

fn group_methods(lua: &Lua, object: &Table, module: &Module, name: String) -> mlua::Result<()> {
    let n = name.clone();
    method(lua, object, "exists", module, move |_, m, _, _, _| {
        Ok(Value::Boolean(m.authority.group(&n).is_some()))
    })?;
    let n = name.clone();
    method(
        lua,
        object,
        "create",
        module,
        move |lua, m, options, _, _| {
            let (mut weight, mut display_name) = (0, None);
            match &options {
                Value::Nil => {}
                Value::Table(table) => {
                    for pair in table.pairs::<String, Value>() {
                        let (key, value) = pair?;
                        match key.as_str() {
                            "weight" => {
                                weight = i32::try_from(integer(&value, "an integer weight")?)
                                    .map_err(|_| error("weights are 32-bit integers"))?;
                            }
                            "display_name" => {
                                display_name = Some(string(&value, "a display name")?)
                            }
                            _ => return Err(error(format!("unknown group field {key}"))),
                        }
                    }
                }
                _ => return Err(error("create expects { weight = ..., display_name = ... }")),
            }
            let change = Change::CreateGroup {
                name: n.clone(),
                weight,
                display_name,
            };
            m.write(lua, change, None).map(Value::Table)
        },
    )?;
    let n = name.clone();
    method(lua, object, "delete", module, move |lua, m, _, _, _| {
        m.write(lua, Change::DeleteGroup { name: n.clone() }, None)
            .map(Value::Table)
    })?;
    let n = name.clone();
    method(
        lua,
        object,
        "set_weight",
        module,
        move |lua, m, weight, _, _| {
            let weight = i32::try_from(integer(&weight, "an integer weight")?)
                .map_err(|_| error("weights are 32-bit integers"))?;
            m.write(
                lua,
                Change::SetWeight {
                    group: n.clone(),
                    weight,
                },
                None,
            )
            .map(Value::Table)
        },
    )?;
    method(
        lua,
        object,
        "set_display_name",
        module,
        move |lua, m, display, _, _| {
            let display_name = match display {
                Value::Nil => None,
                display => Some(string(&display, "a display name or nil")?),
            };
            let change = Change::SetDisplayName {
                group: name.clone(),
                display_name,
            };
            m.write(lua, change, None).map(Value::Table)
        },
    )
}

fn track(lua: &Lua, module: &Module, name: String) -> mlua::Result<Table> {
    let object = lua.create_table()?;
    for (method_name, up) in [("promote", true), ("demote", false)] {
        let n = name.clone();
        method(
            lua,
            &object,
            method_name,
            module,
            move |lua, m, player, active, _| {
                let options = Options::or_contexts(&active)?;
                let (user, track, contexts) = (uuid(&player)?, n.clone(), options.contexts);
                let change = if up {
                    Change::Promote {
                        user,
                        track,
                        contexts,
                    }
                } else {
                    Change::Demote {
                        user,
                        track,
                        contexts,
                    }
                };
                m.write(lua, change, options.reason.as_deref())
                    .map(Value::Table)
            },
        )?;
    }
    let n = name.clone();
    method(
        lua,
        &object,
        "groups",
        module,
        move |lua, m, _, _, _| match m.authority.tracks().get(&n) {
            Some(track) => m.json(lua, &track.groups),
            None => Ok(Value::Nil),
        },
    )?;
    let n = name.clone();
    method(lua, &object, "set", module, move |lua, m, groups, _, _| {
        let Value::Table(list) = groups else {
            return Err(error("set expects a list of group names"));
        };
        let groups = list
            .sequence_values::<Value>()
            .map(|group| string(&group?, "group names"))
            .collect::<mlua::Result<_>>()?;
        m.write(
            lua,
            Change::SetTrack {
                name: n.clone(),
                groups,
            },
            None,
        )
        .map(Value::Table)
    })?;
    method(lua, &object, "delete", module, move |lua, m, _, _, _| {
        m.write(lua, Change::DeleteTrack { name: name.clone() }, None)
            .map(Value::Table)
    })?;
    Ok(object)
}

/// One `on_change` event table per op of `event`, at most
/// [`MAX_HANDLED_OPS`]: `{ revision, actor, reason, subject, kind, key,
/// before, after }`.
pub(crate) fn events(
    lua: &Lua,
    event: &ChangeEvent,
    deadline: Instant,
) -> mlua::Result<Vec<Table>> {
    fn json(lua: &Lua, value: &impl Serialize, deadline: Instant) -> mlua::Result<Value> {
        let value = serde_json::to_value(value).map_err(mlua::Error::external)?;
        json_value(lua, &value, 0, deadline)
    }
    let mut tables = Vec::new();
    for op in event.ops.iter().take(MAX_HANDLED_OPS) {
        let table = lua.create_table()?;
        table.raw_set("revision", event.revision)?;
        table.raw_set("actor", event.actor.to_string())?;
        table.raw_set("reason", event.reason.as_deref())?;
        let (subject, kind, key, before, after) = match op {
            Op::Node {
                subject,
                before,
                after,
            } => {
                let node = after
                    .as_ref()
                    .or(before.as_ref())
                    .expect("a node op names a node");
                (
                    subject.to_string(),
                    node.kind().as_str().to_owned(),
                    node.payload.key(),
                    json(lua, before, deadline)?,
                    json(lua, after, deadline)?,
                )
            }
            Op::Group {
                name,
                before,
                after,
            } => (
                format!("group:{name}"),
                "group".into(),
                name.clone(),
                json(lua, before, deadline)?,
                json(lua, after, deadline)?,
            ),
            Op::Track {
                name,
                before,
                after,
            } => (
                format!("track:{name}"),
                "track".into(),
                name.clone(),
                json(lua, before, deadline)?,
                json(lua, after, deadline)?,
            ),
            Op::PrimaryGroup {
                user,
                before,
                after,
            } => (
                SubjectRef::User(*user).to_string(),
                "primary_group".into(),
                "primary_group".into(),
                json(lua, before, deadline)?,
                json(lua, after, deadline)?,
            ),
        };
        table.raw_set("subject", subject)?;
        table.raw_set("kind", kind)?;
        table.raw_set("key", key)?;
        table.raw_set("before", before)?;
        table.raw_set("after", after)?;
        tables.push(table);
    }
    Ok(tables)
}
