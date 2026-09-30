//! Live v2 grants. Configuration grants and runtime overrides have separate lifetimes.
use super::token;
use mlua::{Lua, Table, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Instant,
};

type Grants = BTreeMap<String, BTreeSet<String>>;

#[derive(Default)]
struct State {
    baseline: Grants,
    overrides: BTreeMap<(String, String), bool>,
}

#[derive(Clone, Default)]
pub(super) struct Permissions(Arc<Mutex<State>>);

pub(super) fn valid_uuid(uuid: &str) -> bool {
    uuid == "*"
        || (uuid.len() == 32
            && uuid
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

impl Permissions {
    pub(super) fn replace(&self, baseline: &Grants) {
        self.0.lock().unwrap().baseline.clone_from(baseline);
    }

    pub(super) fn has(&self, uuid: &str, node: &str) -> bool {
        let state = self.0.lock().unwrap();
        for target in [uuid, "*"] {
            if let Some(granted) = state.overrides.get(&(target.to_owned(), node.to_owned())) {
                return *granted;
            }
        }
        [uuid, "*"].iter().any(|target| {
            state
                .baseline
                .get(*target)
                .is_some_and(|nodes| nodes.contains(node))
        })
    }

    pub(super) fn install(
        &self,
        lua: &Lua,
        runtime: &Table,
        deadline: Instant,
    ) -> mlua::Result<()> {
        let api = lua.create_table()?;
        let permissions = self.clone();
        api.raw_set(
            "has",
            lua.create_function(move |_, (uuid, node): (String, String)| {
                crate::script::check_deadline(deadline)?;
                validate(&uuid, &node)?;
                Ok(permissions.has(&uuid, &node))
            })?,
        )?;
        let permissions = self.clone();
        api.raw_set(
            "set",
            lua.create_function(move |_, (uuid, node, value): (String, String, Value)| {
                crate::script::check_deadline(deadline)?;
                validate(&uuid, &node)?;
                let value = match value {
                    Value::Nil => None,
                    Value::Boolean(value) => Some(value),
                    _ => {
                        return Err(mlua::Error::runtime(
                            "permission override must be true, false, or nil",
                        ));
                    }
                };
                let mut state = permissions.0.lock().unwrap();
                crate::script::check_deadline(deadline)?;
                let key = (uuid, node);
                if let Some(value) = value {
                    if state.overrides.len() >= 4096 && !state.overrides.contains_key(&key) {
                        return Err(mlua::Error::runtime(
                            "permission override limit exceeded (4096)",
                        ));
                    }
                    state.overrides.insert(key, value);
                } else {
                    state.overrides.remove(&key);
                }
                Ok(())
            })?,
        )?;
        runtime.raw_set("permissions", api)
    }
}

fn validate(uuid: &str, node: &str) -> mlua::Result<()> {
    if !valid_uuid(uuid) || !token(node) {
        return Err(mlua::Error::runtime(
            "permissions require a lowercase UUID (32 hex digits) or * and a permission node",
        ));
    }
    Ok(())
}
