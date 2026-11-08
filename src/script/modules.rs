use std::{cell::RefCell, collections::BTreeSet, rc::Rc, time::Instant};

use mlua::{Function, Lua, Table, Value, chunk::ChunkMode};

use super::{ScriptSource, check_deadline};

/// `rift.plugin(name, config)` calls that named a persistent plugin, in call
/// order: `(name, configuration JSON)`. Read by configuration evaluation;
/// callbacks re-evaluate the configuration and ignore it.
#[derive(Debug, Default)]
pub(crate) struct PersistentPlugins(pub Vec<(String, String)>);

/// The configuration table handed to a persistent plugin's `init`.
const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_CONFIG_DEPTH: usize = 32;

/// Encode a configuration table as JSON: string keys make objects, dense
/// integer keys from 1 make arrays, an empty table is an object.
fn json(value: Value, depth: usize) -> mlua::Result<serde_json::Value> {
    let fail = |message: &str| mlua::Error::runtime(format!("rift.plugin: config {message}"));
    if depth > MAX_CONFIG_DEPTH {
        return Err(fail(&format!(
            "nests deeper than {MAX_CONFIG_DEPTH} levels"
        )));
    }
    Ok(match value {
        Value::Nil => serde_json::Value::Null,
        Value::Boolean(value) => value.into(),
        Value::Integer(value) => value.into(),
        Value::Number(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| fail("numbers must be finite"))?,
        Value::String(value) => value
            .to_str()
            .map_err(|_| fail("strings must be UTF-8"))?
            .to_owned()
            .into(),
        Value::Table(table) => {
            let mut object = serde_json::Map::new();
            let mut array = std::collections::BTreeMap::new();
            for pair in table.pairs::<Value, Value>() {
                let (key, value) = pair?;
                let value = json(value, depth + 1)?;
                match key {
                    Value::String(key) => {
                        let key = key.to_str().map_err(|_| fail("keys must be UTF-8"))?;
                        object.insert(key.to_owned(), value);
                    }
                    Value::Integer(index) if index >= 1 => {
                        array.insert(index, value);
                    }
                    _ => return Err(fail("keys must be strings or array indices")),
                }
            }
            match (object.is_empty(), array.is_empty()) {
                (_, true) => serde_json::Value::Object(object),
                (true, false) if array.keys().copied().eq(1..=array.len() as i64) => {
                    serde_json::Value::Array(array.into_values().collect())
                }
                _ => {
                    return Err(fail(
                        "tables must be either a dense list or have only string keys",
                    ));
                }
            }
        }
        other => {
            return Err(fail(&format!(
                "cannot contain a {} value",
                other.type_name()
            )));
        }
    })
}

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
    for namespace in ["store", "permissions", "http", "players"] {
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
    let installed = source.installed.clone();
    lua.set_app_data(PersistentPlugins::default());
    api.raw_set(
        "plugin",
        lua.create_function(move |lua, (name, config): (String, Value)| {
            check_deadline(deadline)?;
            if !valid_name(&name, false) {
                return Err(mlua::Error::runtime(
                    "rift.plugin expects a folder name using letters, digits, '_' or '-'",
                ));
            }
            if !matches!(config, Value::Nil | Value::Table(_)) {
                return Err(mlua::Error::runtime(
                    "rift.plugin expects an optional configuration table",
                ));
            }
            // A persistent plugin is recorded, never evaluated here: trusted
            // code does not run in the configuration sandbox.
            if let Some(plugin) = installed.get(&name).filter(|plugin| plugin.persistent()) {
                if let Err(error) = &plugin.manifest {
                    return Err(mlua::Error::runtime(format!("plugin '{name}': {error}")));
                }
                if let Err(error) = &plugin.fingerprint {
                    return Err(mlua::Error::runtime(format!("plugin '{name}': {error}")));
                }
                let config = match config {
                    Value::Nil => "{}".to_owned(),
                    table => json(table, 0)?.to_string(),
                };
                if config.len() > MAX_CONFIG_BYTES {
                    return Err(mlua::Error::runtime(format!(
                        "rift.plugin: config for '{name}' exceeds 64 KiB as JSON"
                    )));
                }
                let mut records = lua
                    .app_data_mut::<PersistentPlugins>()
                    .ok_or_else(|| mlua::Error::runtime("rift.plugin registry unavailable"))?;
                if records.0.iter().any(|(existing, _)| *existing == name) {
                    return Err(mlua::Error::runtime(format!(
                        "plugin '{name}' is already registered"
                    )));
                }
                records.0.push((name, config));
                return Ok(Value::Boolean(true));
            }
            let cached: Value = plugins.raw_get(name.as_str())?;
            if !cached.is_nil() {
                return Ok(cached);
            }
            let file = files
                .get(&format!("plugins/{name}/init.lua"))
                .ok_or_else(|| {
                    mlua::Error::runtime(format!(
                        "plugin '{name}' not found: expected plugins/{name}/init.lua or plugins/{name}/plugin.toml"
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
                .call::<Value>((name.as_str(), config));
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
