//! `extensions.groups`: permission groups as configuration. Each committed
//! reload mirrors them into the permission store as `source = 'config'`
//! nodes in one revision; groups and nodes edited at runtime are kept.
//!
//! Also reads the removed `extensions.permissions` table, only so its grants
//! can be copied into the store once on upgrade.
use super::fields;
use mlua::{Table, Value};
use rift_permissions::{
    DEFAULT_GROUP, Group, Node, Source, SubjectRef, Uuid,
    node::{Payload, normalize_node},
    subject::valid_name,
};
use std::collections::BTreeMap;

fn error(message: String) -> mlua::Error {
    mlua::Error::runtime(message)
}

fn config(node: Result<Node, rift_permissions::Error>, path: &str) -> mlua::Result<Node> {
    node.map(|node| node.with_source(Source::Config))
        .map_err(|e| error(format!("{path}: {e}")))
}

fn integer(value: Value, path: &str) -> mlua::Result<i32> {
    let number = match value {
        Value::Integer(value) => value as f64,
        Value::Number(value) => value,
        _ => return Err(error(format!("{path} must be an integer"))),
    };
    if number.fract() != 0.0 || number < f64::from(i32::MIN) || number > f64::from(i32::MAX) {
        return Err(error(format!("{path} must be a 32-bit integer")));
    }
    Ok(number as i32)
}

/// Node patterns mapped to grant (`true`) or deny (`false`).
fn permission_table(table: Table, path: &str) -> mlua::Result<BTreeMap<String, bool>> {
    let mut nodes = BTreeMap::new();
    for entry in table.pairs::<String, Value>() {
        let (node, value) = entry?;
        // Converting to bool would accept any truthy value, including 'false'.
        let Value::Boolean(granted) = value else {
            return Err(error(format!("{path}: {node:?} must be true or false")));
        };
        let normalized = normalize_node(&node).map_err(|e| error(format!("{path}: {e}")))?;
        if nodes.insert(normalized, granted).is_some() {
            return Err(error(format!("{path}: {node:?} is listed twice")));
        }
    }
    Ok(nodes)
}

/// A prefix or suffix: text with the group's weight as priority, or
/// `{ priority = n, text = "..." }`.
fn decoration(value: Value, weight: i32, path: &str) -> mlua::Result<Option<(i32, String)>> {
    match value {
        Value::Nil => Ok(None),
        Value::String(text) => Ok(Some((weight, text.to_str()?.to_owned()))),
        Value::Table(table) => {
            fields(&table, &["priority", "text"])?;
            let priority = integer(table.raw_get("priority")?, &format!("{path}.priority"))?;
            let text: String = table
                .raw_get::<Option<String>>("text")?
                .ok_or_else(|| error(format!("{path}.text must be a string")))?;
            Ok(Some((priority, text)))
        }
        _ => Err(error(format!(
            "{path} must be text or {{ priority = ..., text = ... }}"
        ))),
    }
}

/// Parse `extensions.groups` into groups whose nodes all have source
/// `config`. Store limits are checked with the rest of the configuration.
pub(super) fn parse(value: Value) -> mlua::Result<BTreeMap<String, Group>> {
    let mut groups = BTreeMap::new();
    let entries = match value {
        Value::Nil => return Ok(groups),
        Value::Table(entries) => entries,
        _ => return Err(error("extensions.groups must be a table".into())),
    };
    for entry in entries.pairs::<String, Table>() {
        let (name, definition) = entry?;
        let path = format!("extensions.groups.{name}");
        if !valid_name(&name) {
            return Err(error(format!(
                "invalid permission group name {name:?}: use 1 to 64 bytes of [a-z0-9_.-]"
            )));
        }
        fields(
            &definition,
            &[
                "inherits",
                "permissions",
                "weight",
                "prefix",
                "suffix",
                "meta",
            ],
        )?;
        let mut group = Group::new(&name).map_err(|e| error(format!("{path}: {e}")))?;
        if let Some(list) = definition.raw_get::<Option<Table>>("inherits")? {
            let length = list.raw_len();
            for (index, entry) in list.pairs::<Value, Value>().enumerate() {
                let (Value::Integer(1..), Value::String(parent)) = entry? else {
                    return Err(error(format!(
                        "{path}.inherits must be a list of group names"
                    )));
                };
                if index >= length {
                    return Err(error(format!(
                        "{path}.inherits must be a list of group names"
                    )));
                }
                let node = Node::inherit(&parent.to_str()?, true);
                group.nodes.push(config(node, &format!("{path}.inherits"))?);
            }
        }
        if let Some(table) = definition.raw_get::<Option<Table>>("permissions")? {
            let path = format!("{path}.permissions");
            for (node, granted) in permission_table(table, &path)? {
                group
                    .nodes
                    .push(config(Node::permission(&node, granted), &path)?);
            }
        }
        match definition.raw_get::<Value>("weight")? {
            Value::Nil => {}
            value => group.weight = integer(value, &format!("{path}.weight"))?,
        }
        if group.weight != 0 {
            group
                .nodes
                .push(Node::weight(group.weight).with_source(Source::Config));
        }
        for (field, prefix) in [("prefix", true), ("suffix", false)] {
            let path = format!("{path}.{field}");
            if let Some((priority, text)) =
                decoration(definition.raw_get(field)?, group.weight, &path)?
            {
                let node = if prefix {
                    Node::prefix(priority, &text)
                } else {
                    Node::suffix(priority, &text)
                };
                group.nodes.push(config(node, &path)?);
            }
        }
        if let Some(table) = definition.raw_get::<Option<Table>>("meta")? {
            let mut meta = BTreeMap::new();
            for entry in table.pairs::<String, Value>() {
                let (key, value) = entry?;
                let value = match value {
                    Value::String(value) => value.to_str()?.to_owned(),
                    Value::Integer(value) => value.to_string(),
                    _ => {
                        return Err(error(format!(
                            "{path}.meta.{key} must be a string or an integer"
                        )));
                    }
                };
                meta.insert(key, value);
            }
            for (key, value) in meta {
                group
                    .nodes
                    .push(config(Node::meta(&key, &value), &format!("{path}.meta"))?);
            }
        }
        groups.insert(name, group);
    }
    Ok(groups)
}

/// Groups with the field names of `extensions.groups`, for the dashboard.
pub(super) fn value(groups: &BTreeMap<String, Group>) -> serde_json::Value {
    let group = |group: &Group| {
        let mut inherits = Vec::new();
        let mut permissions = serde_json::Map::new();
        let mut meta = serde_json::Map::new();
        let mut value = serde_json::Map::new();
        for node in &group.nodes {
            match &node.payload {
                Payload::Inherit { group, .. } => inherits.push(group.clone()),
                Payload::Permission { key, value } => {
                    permissions.insert(key.clone(), (*value).into());
                }
                Payload::Prefix { priority, text } | Payload::Suffix { priority, text } => {
                    let field = if node.kind() == rift_permissions::NodeKind::Prefix {
                        "prefix"
                    } else {
                        "suffix"
                    };
                    let decoration = if *priority == group.weight {
                        text.clone().into()
                    } else {
                        serde_json::json!({"priority": priority, "text": text})
                    };
                    value.insert(field.into(), decoration);
                }
                Payload::Meta { key, value } => {
                    meta.insert(key.clone(), value.clone().into());
                }
                Payload::Weight { weight } => {
                    value.insert("weight".into(), (*weight).into());
                }
                Payload::DisplayName { .. } => {}
            }
        }
        value.insert("inherits".into(), inherits.into());
        value.insert("permissions".into(), permissions.into());
        if !meta.is_empty() {
            value.insert("meta".into(), meta.into());
        }
        serde_json::Value::Object(value)
    };
    groups
        .iter()
        .map(|(name, g)| (name.clone(), group(g)))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// The grants of the removed `extensions.permissions`, `None` when absent:
/// player UUID (32 lowercase hex digits) or `*` to node patterns. `*` grants
/// become nodes of the `default` group.
pub(super) fn legacy(value: Value) -> mlua::Result<Option<Vec<(SubjectRef, Node)>>> {
    let mut grants = Vec::new();
    let entries = match value {
        Value::Nil => return Ok(None),
        Value::Table(entries) => entries,
        _ => return Err(error("extensions.permissions must be a table".into())),
    };
    for entry in entries.pairs::<String, Table>() {
        let (key, nodes) = entry?;
        let subject = if key == "*" {
            SubjectRef::Group(DEFAULT_GROUP.into())
        } else {
            match Uuid::parse(&key) {
                Ok(uuid) if key.len() == 32 && key == uuid.to_string() => SubjectRef::User(uuid),
                _ => {
                    return Err(error(
                        "permission keys must be lowercase UUID hex (32 digits) or *".into(),
                    ));
                }
            }
        };
        for (node, granted) in permission_table(nodes, "extensions.permissions")? {
            let node = Node::permission(&node, granted)
                .map_err(|e| error(format!("extensions.permissions: {e}")))?;
            grants.push((subject.clone(), node));
        }
    }
    Ok(Some(grants))
}
