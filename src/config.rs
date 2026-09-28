use std::{collections::BTreeMap, fs, io, net::SocketAddr, path::Path, time::Duration};

use mlua::{Lua, Table, Value};
use tokio::sync::Semaphore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listeners: BTreeMap<String, SocketAddr>,
    pub backends: BTreeMap<String, SocketAddr>,
    // Each listener has exactly one backend; traffic remains transparent TCP.
    pub routes: BTreeMap<String, String>,
    pub limits: Limits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_connections: usize,
    pub connect_timeout: Duration,
    pub buffer_size: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 4096,
            connect_timeout: Duration::from_secs(5),
            buffer_size: 32 * 1024,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::from_addresses("0.0.0.0:25565", "127.0.0.1:25566")
            .expect("built-in configuration is valid")
    }
}

impl Config {
    pub fn from_addresses(listen: &str, backend: &str) -> io::Result<Self> {
        let config = Self {
            listeners: BTreeMap::from([("default".into(), address(listen, "listen")?)]),
            backends: BTreeMap::from([("default".into(), address(backend, "backend")?)]),
            routes: BTreeMap::from([("default".into(), "default".into())]),
            limits: Limits::default(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let source = fs::read_to_string(path).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        Self::from_lua(&source, &path.display().to_string())
    }

    pub fn from_lua(source: &str, name: &str) -> io::Result<Self> {
        // Lua exists only during startup. The runtime receives owned Rust data.
        let lua = Lua::new();
        let parse = || -> Result<Self, String> {
            let value = lua
                .load(source)
                .set_name(name)
                .eval::<Value>()
                .map_err(|e| e.to_string())?;
            let root = table(value, "configuration (return a table)")?;
            fields(
                &root,
                &["listeners", "backends", "routes", "limits"],
                "config",
            )?;
            let listeners = addresses(
                root.get("listeners").map_err(|e| e.to_string())?,
                "listeners",
            )?;
            let backends = addresses(root.get("backends").map_err(|e| e.to_string())?, "backends")?;
            let routes = strings(root.get("routes").map_err(|e| e.to_string())?, "routes")?;
            let mut limits = Limits::default();
            let value: Value = root.get("limits").map_err(|e| e.to_string())?;
            if !value.is_nil() {
                let values = table(value, "limits")?;
                fields(
                    &values,
                    &["max_connections", "connect_timeout_ms", "buffer_size"],
                    "limits",
                )?;
                limits.max_connections = positive_integer(
                    &values,
                    "max_connections",
                    limits.max_connections,
                    Semaphore::MAX_PERMITS,
                )?;
                limits.buffer_size =
                    positive_integer(&values, "buffer_size", limits.buffer_size, 16 * 1024 * 1024)?;
                limits.connect_timeout = Duration::from_millis(positive_integer(
                    &values,
                    "connect_timeout_ms",
                    5000,
                    86_400_000,
                )? as u64);
            }
            Ok(Self {
                listeners,
                backends,
                routes,
                limits,
            })
        };
        let config = parse().map_err(|error| invalid(format!("{name}: {error}")))?;
        config
            .validate()
            .map_err(|error| invalid(format!("{name}: {error}")))?;
        Ok(config)
    }

    fn validate(&self) -> io::Result<()> {
        for (name, listen) in &self.listeners {
            if !self.routes.contains_key(name) {
                return Err(invalid(format!(
                    "routes: missing route for listener {name:?}"
                )));
            }
            for (other_name, other) in self.listeners.range(..name.clone()) {
                if listen.port() != 0 && listen == other {
                    return Err(invalid(format!(
                        "listeners.{name}: duplicate address {listen} (also listeners.{other_name})"
                    )));
                }
            }
            // Check every backend against every listener, including cross-listener loops.
            for (backend_name, backend) in &self.backends {
                if same_socket(*listen, *backend) {
                    return Err(invalid(format!(
                        "listeners.{name} and backends.{backend_name} must not point to the same socket ({backend})"
                    )));
                }
            }
        }
        for (listener, backend) in &self.routes {
            if !self.listeners.contains_key(listener) {
                return Err(invalid(format!(
                    "routes.{listener}: unknown listener {listener:?}"
                )));
            }
            if !self.backends.contains_key(backend) {
                return Err(invalid(format!(
                    "routes.{listener}: unknown backend {backend:?}"
                )));
            }
        }
        Ok(())
    }
}

fn same_socket(listen: SocketAddr, backend: SocketAddr) -> bool {
    listen.port() != 0
        && listen.port() == backend.port()
        && (listen.ip() == backend.ip()
            || (listen.ip().is_unspecified() && backend.ip().is_loopback()))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn address(value: &str, path: &str) -> io::Result<SocketAddr> {
    value
        .parse()
        .map_err(|error| invalid(format!("{path}: invalid socket address {value:?}: {error}")))
}

fn table(value: Value, path: &str) -> Result<Table, String> {
    match value {
        Value::Table(table) => Ok(table),
        _ => Err(format!(
            "{path}: expected a table, got {}",
            value.type_name()
        )),
    }
}

fn string(value: Value, path: &str) -> Result<String, String> {
    match value {
        Value::String(value) => value
            .to_str()
            .map(|s| s.to_owned())
            .map_err(|e| format!("{path}: {e}")),
        _ => Err(format!(
            "{path}: expected a string, got {}",
            value.type_name()
        )),
    }
}

fn fields(table: &Table, allowed: &[&str], path: &str) -> Result<(), String> {
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|e| e.to_string())?;
        let key = string(key, path)?;
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "{path}.{key}: unknown field (expected {})",
                allowed.join(", ")
            ));
        }
    }
    Ok(())
}

fn strings(value: Value, path: &str) -> Result<BTreeMap<String, String>, String> {
    let table = table(value, path)?;
    let mut result = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| e.to_string())?;
        let key = string(key, &format!("{path} key"))?;
        if key.trim().is_empty() {
            return Err(format!("{path}: names must not be empty"));
        }
        result.insert(key.clone(), string(value, &format!("{path}.{key}"))?);
    }
    if result.is_empty() {
        return Err(format!("{path}: must not be empty"));
    }
    Ok(result)
}

fn addresses(value: Value, path: &str) -> Result<BTreeMap<String, SocketAddr>, String> {
    strings(value, path)?
        .into_iter()
        .map(|(name, value)| {
            address(&value, &format!("{path}.{name}"))
                .map(|address| (name, address))
                .map_err(|e| e.to_string())
        })
        .collect()
}

fn positive_integer(table: &Table, key: &str, default: usize, max: usize) -> Result<usize, String> {
    let value: Value = table.get(key).map_err(|e| e.to_string())?;
    // LuaJIT can represent integral arithmetic results as floating-point numbers.
    let number = match value {
        Value::Nil => return Ok(default),
        Value::Integer(value) if value > 0 && value as u64 <= max as u64 => {
            return Ok(value as usize);
        }
        Value::Number(value) => value,
        _ => return Err(format!("limits.{key}: expected an integer in 1..={max}")),
    };
    if !number.is_finite() || number.fract() != 0.0 || number < 1.0 || number >= (max as f64 + 1.0)
    {
        return Err(format!("limits.{key}: expected an integer in 1..={max}"));
    }
    Ok(number as usize)
}

#[cfg(test)]
mod tests;
