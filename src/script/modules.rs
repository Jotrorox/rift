use std::{cell::RefCell, collections::BTreeSet, rc::Rc, time::Instant};

use mlua::{Function, Lua, Table, Value, chunk::ChunkMode};

use super::{ScriptSource, check_deadline};

/// A bounded, text-only require implementation over an immutable source bundle.
/// Neither package.path nor a native/dynamic loader is exposed to Lua.
pub(super) fn install(
    lua: &Lua,
    source: &ScriptSource,
    deadline: Instant,
) -> mlua::Result<Function> {
    let api = lua.create_table()?;
    lua.globals().raw_set("rift", api.clone())?;
    lua.set_named_registry_value("rift.api", api.clone())?;
    let runtime = lua.create_table()?;
    runtime.raw_set(
        "publish",
        lua.create_function(|_, _: mlua::MultiValue| -> mlua::Result<()> {
            Err(mlua::Error::runtime(
                "rift.publish is only available inside runtime callbacks",
            ))
        })?,
    )?;
    lua.set_named_registry_value("rift.runtime", runtime.clone())?;
    let finalize: Function = lua
        .load(include_str!("../lua/api.lua"))
        .set_name("=rift API")
        .call(runtime)?;
    let cache = lua.create_table()?;
    cache.raw_set("rift", api.clone())?;
    cache.raw_set("rift.config", api.raw_get::<Table>("config")?)?;
    for namespace in ["store", "permissions", "http"] {
        cache.raw_set(
            format!("rift.{namespace}"),
            api.raw_get::<Table>(namespace)?,
        )?;
    }
    let plugins = lua.create_table()?;
    let roots = Rc::new(RefCell::new(vec!["lua".to_owned()]));
    let loading = Rc::new(RefCell::new(BTreeSet::new()));

    let files = source.files.clone();
    let module_roots = roots.clone();
    let module_loading = loading.clone();
    lua.globals().raw_set(
        "require",
        lua.create_function(move |lua, name: String| {
            check_deadline(deadline)?;
            if !valid_name(&name, true) {
                return Err(mlua::Error::runtime(
                    "require expects a dotted module name (for example 'network.routes')",
                ));
            }
            let cached: Value = cache.raw_get(name.as_str())?;
            if !cached.is_nil() && cached != Value::Boolean(false) {
                return Ok(cached);
            }
            let module = name.replace('.', "/");
            let file = module_roots
                .borrow()
                .iter()
                .find_map(|root| {
                    files
                        .get(&format!("{root}/{module}.lua"))
                        .or_else(|| files.get(&format!("{root}/{module}/init.lua")))
                        .cloned()
                })
                .ok_or_else(|| {
                    mlua::Error::runtime(format!(
                        "module '{name}' not found in lua/ or loaded plugins' lua/ directories"
                    ))
                })?;
            let key = format!("module:{name}");
            if !module_loading.borrow_mut().insert(key.clone()) {
                return Err(mlua::Error::runtime(format!(
                    "circular require of module '{name}'"
                )));
            }
            let result = lua
                .load(file.source.as_ref())
                .set_name(file.name.as_ref())
                .set_mode(ChunkMode::Text)
                .call::<Value>(name.as_str());
            module_loading.borrow_mut().remove(&key);
            let value = result?;
            check_deadline(deadline)?;
            let value = if value.is_nil() {
                Value::Boolean(true)
            } else {
                value
            };
            cache.raw_set(name, value.clone())?;
            Ok(value)
        })?,
    )?;

    let files = source.files.clone();
    api.raw_set(
        "plugin",
        lua.create_function(move |lua, name: String| {
            check_deadline(deadline)?;
            if !valid_name(&name, false) {
                return Err(mlua::Error::runtime(
                    "rift.plugin expects a folder name using letters, digits, '_' or '-'",
                ));
            }
            let cached: Value = plugins.raw_get(name.as_str())?;
            if !cached.is_nil() {
                return Ok(cached);
            }
            let file = files
                .get(&format!("plugins/{name}/init.lua"))
                .ok_or_else(|| {
                    mlua::Error::runtime(format!(
                        "plugin '{name}' not found: expected plugins/{name}/init.lua"
                    ))
                })?;
            let key = format!("plugin:{name}");
            if !loading.borrow_mut().insert(key.clone()) {
                return Err(mlua::Error::runtime(format!(
                    "circular load of plugin '{name}'"
                )));
            }
            roots.borrow_mut().push(format!("plugins/{name}/lua"));
            let result = lua
                .load(file.source.as_ref())
                .set_name(file.name.as_ref())
                .set_mode(ChunkMode::Text)
                .call::<Value>(name.as_str());
            loading.borrow_mut().remove(&key);
            let value = result?;
            check_deadline(deadline)?;
            let value = if value.is_nil() {
                Value::Boolean(true)
            } else {
                value
            };
            plugins.raw_set(name, value.clone())?;
            Ok(value)
        })?,
    )?;
    Ok(finalize)
}

fn valid_name(name: &str, dotted: bool) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
        && (dotted || !name.contains('.'))
}
