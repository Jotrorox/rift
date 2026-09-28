use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    net::SocketAddr,
    path::Path,
    time::{Duration, Instant},
};

use crate::routing::{Backend, Mode, Routes};
use mlua::{Table, Value};
use tokio::sync::Semaphore;

pub use crate::script::RouteScript;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listeners: BTreeMap<String, SocketAddr>,
    pub backends: BTreeMap<String, Backend>,
    pub routes: BTreeMap<String, Route>,
    pub limits: Limits,
    pub on_route: Option<RouteScript>,
    pub fallbacks: BTreeMap<String, Vec<String>>,
    pub rate_limit: Option<RateLimit>,
    pub health_check: Option<HealthCheck>,
    pub status_cache: Option<StatusCache>,
    pub metrics: Option<SocketAddr>,
    pub shutdown_timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    pub per_ip_per_second: usize,
    pub per_ip_burst: usize,
    pub global_per_second: usize,
    pub global_burst: usize,
    pub max_ips: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthCheck {
    pub interval: Duration,
    pub timeout: Duration,
    pub unhealthy_threshold: usize,
    pub healthy_threshold: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusCache {
    pub ttl: Duration,
    pub max_entries: usize,
    pub max_response_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Direct(String),
    Hostnames(BTreeMap<String, String>),
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
            backends: BTreeMap::from([("default".into(), Backend::parse(backend)?)]),
            routes: BTreeMap::from([("default".into(), Route::Direct("default".into()))]),
            limits: Limits::default(),
            on_route: None,
            fallbacks: BTreeMap::new(),
            rate_limit: None,
            health_check: None,
            status_cache: None,
            metrics: None,
            shutdown_timeout: Duration::from_secs(30),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let mut source = String::new();
        let mut read = || -> io::Result<()> {
            if !fs::metadata(path)?.is_file() {
                return Err(invalid("configuration must be a regular file"));
            }
            fs::File::open(path)?
                .take(crate::script::MAX_SOURCE_BYTES as u64 + 1)
                .read_to_string(&mut source)?;
            Ok(())
        };
        read().map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", path.display()))
        })?;
        Self::from_lua(&source, &path.display().to_string())
    }

    pub fn from_lua(source: &str, name: &str) -> io::Result<Self> {
        let parse = || -> Result<Self, String> {
            let (_lua, value) = crate::script::load(
                source,
                name,
                Instant::now() + crate::script::EXECUTION_TIMEOUT,
            )
            .map_err(|e| e.to_string())?;
            let root = table(value, "configuration (return a table)")?;
            fields(
                &root,
                &[
                    "listeners",
                    "backends",
                    "routes",
                    "limits",
                    "on_route",
                    "fallbacks",
                    "rate_limit",
                    "health_check",
                    "status_cache",
                    "metrics",
                    "shutdown_timeout_ms",
                ],
                "config",
            )?;
            let listeners = addresses(
                root.get("listeners").map_err(|e| e.to_string())?,
                "listeners",
            )?;
            let backends = strings(root.get("backends").map_err(|e| e.to_string())?, "backends")?
                .into_iter()
                .map(|(name, value)| {
                    Backend::parse(&value)
                        .map(|backend| (name.clone(), backend))
                        .map_err(|error| format!("backends.{name}: {error}"))
                })
                .collect::<Result<_, _>>()?;
            let routes = routes(root.get("routes").map_err(|e| e.to_string())?)?;
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
            let on_route = match root
                .raw_get::<Value>("on_route")
                .map_err(|e| e.to_string())?
            {
                Value::Nil => None,
                Value::Function(_) => Some(RouteScript::new(source, name)),
                _ => return Err("config.on_route: expected a function".into()),
            };
            let rate_limit = options(
                &root,
                "rate_limit",
                &[
                    "per_ip_per_second",
                    "per_ip_burst",
                    "global_per_second",
                    "global_burst",
                    "max_ips",
                ],
            )?
            .map(|t| {
                Ok::<_, String>(RateLimit {
                    per_ip_per_second: integer(
                        &t,
                        "rate_limit",
                        "per_ip_per_second",
                        20,
                        1_000_000,
                    )?,
                    per_ip_burst: integer(&t, "rate_limit", "per_ip_burst", 40, 1_000_000)?,
                    global_per_second: integer(
                        &t,
                        "rate_limit",
                        "global_per_second",
                        200,
                        1_000_000,
                    )?,
                    global_burst: integer(&t, "rate_limit", "global_burst", 400, 1_000_000)?,
                    max_ips: integer(&t, "rate_limit", "max_ips", 65536, 1_000_000)?,
                })
            })
            .transpose()?;
            let health_check = options(
                &root,
                "health_check",
                &[
                    "interval_ms",
                    "timeout_ms",
                    "unhealthy_threshold",
                    "healthy_threshold",
                ],
            )?
            .map(|t| {
                Ok::<_, String>(HealthCheck {
                    interval: Duration::from_millis(integer(
                        &t,
                        "health_check",
                        "interval_ms",
                        5000,
                        86_400_000,
                    )? as u64),
                    timeout: Duration::from_millis(integer(
                        &t,
                        "health_check",
                        "timeout_ms",
                        1000,
                        86_400_000,
                    )? as u64),
                    unhealthy_threshold: integer(
                        &t,
                        "health_check",
                        "unhealthy_threshold",
                        2,
                        1000,
                    )?,
                    healthy_threshold: integer(&t, "health_check", "healthy_threshold", 1, 1000)?,
                })
            })
            .transpose()?;
            let status_cache = options(
                &root,
                "status_cache",
                &["ttl_ms", "max_entries", "max_response_bytes"],
            )?
            .map(|t| {
                Ok::<_, String>(StatusCache {
                    ttl: Duration::from_millis(integer(
                        &t,
                        "status_cache",
                        "ttl_ms",
                        1000,
                        86_400_000,
                    )? as u64),
                    max_entries: integer(&t, "status_cache", "max_entries", 1024, 65536)?,
                    max_response_bytes: integer(
                        &t,
                        "status_cache",
                        "max_response_bytes",
                        65536,
                        1024 * 1024,
                    )?,
                })
            })
            .transpose()?;
            let metrics = match root.get::<Value>("metrics").map_err(|e| e.to_string())? {
                Value::Nil => None,
                value => Some(
                    address(&string(value, "metrics")?, "metrics").map_err(|e| e.to_string())?,
                ),
            };
            let shutdown_timeout = Duration::from_millis(integer(
                &root,
                "config",
                "shutdown_timeout_ms",
                30000,
                86_400_000,
            )? as u64);
            let mut fallbacks = BTreeMap::new();
            if let Some(t) = options_map(&root, "fallbacks")? {
                for pair in t.pairs::<Value, Value>() {
                    let (key, value) = pair.map_err(|e| e.to_string())?;
                    let key = string(key, "fallbacks key")?;
                    let path = format!("fallbacks.{key}");
                    let list = table(value, &path)?;
                    let mut ordered = BTreeMap::new();
                    for pair in list.pairs::<Value, Value>() {
                        let (index, value) = pair.map_err(|e| e.to_string())?;
                        let Value::Integer(index) = index else {
                            return Err(format!("{path}: expected a dense array"));
                        };
                        ordered.insert(index, string(value, &path)?);
                    }
                    if ordered.is_empty()
                        || ordered.len() > 16
                        || ordered.keys().copied().ne(1..=ordered.len() as i64)
                    {
                        return Err(format!(
                            "{path}: expected a dense array of 1..=16 backend names"
                        ));
                    }
                    fallbacks.insert(key, ordered.into_values().collect());
                }
            }
            Ok(Self {
                listeners,
                backends,
                routes,
                limits,
                on_route,
                fallbacks,
                rate_limit,
                health_check,
                status_cache,
                metrics,
                shutdown_timeout,
            })
        };
        let config = parse().map_err(|error| invalid(format!("{name}: {error}")))?;
        config
            .validate()
            .map_err(|error| invalid(format!("{name}: {error}")))?;
        Ok(config)
    }

    pub fn validate(&self) -> io::Result<()> {
        for (name, targets) in &self.fallbacks {
            if !self.backends.contains_key(name) {
                return Err(invalid(format!("fallbacks.{name}: unknown backend")));
            }
            let mut seen = std::collections::HashSet::new();
            for target in targets {
                if !self.backends.contains_key(target) || target == name || !seen.insert(target) {
                    return Err(invalid(format!(
                        "fallbacks.{name}: unknown, duplicate or self backend {target:?}"
                    )));
                }
            }
        }
        // Lists are deliberately flat: fallback targets' own lists are not followed.
        if let Some(metrics) = self.metrics {
            for listen in self.listeners.values() {
                if metrics.port() != 0 && metrics == *listen {
                    return Err(invalid("metrics: address duplicates a listener"));
                }
            }
            for backend in self.backends.values() {
                backend.check_loop(metrics)?;
            }
        }
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
                if backend.check_loop(*listen).is_err() {
                    return Err(invalid(format!(
                        "listeners.{name} and backends.{backend_name} must not point to the same socket"
                    )));
                }
            }
        }
        for listener in self.routes.keys() {
            if !self.listeners.contains_key(listener) {
                return Err(invalid(format!(
                    "routes.{listener}: unknown listener {listener:?}"
                )));
            }
            self.mode(listener)?;
        }
        Ok(())
    }

    pub fn mode(&self, listener: &str) -> io::Result<Mode> {
        let backend = |name: &str| {
            self.backends
                .get(name)
                .cloned()
                .ok_or_else(|| invalid(format!("routes.{listener}: unknown backend {name:?}")))
        };
        match &self.routes[listener] {
            Route::Direct(name) => Ok(Mode::Direct(backend(name)?)),
            Route::Hostnames(patterns) => {
                let mut routes = Routes::default();
                for (pattern, name) in patterns {
                    routes
                        .add_pattern(pattern, backend(name)?)
                        .map_err(|error| invalid(format!("routes.{listener}: {error}")))?;
                }
                Ok(Mode::Routed(routes))
            }
        }
    }
}

fn routes(value: Value) -> Result<BTreeMap<String, Route>, String> {
    let mut routes = BTreeMap::new();
    for pair in table(value, "routes")?.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|e| e.to_string())?;
        let key = string(key, "routes key")?;
        if key.trim().is_empty() {
            return Err("routes: names must not be empty".into());
        }
        let path = format!("routes.{key}");
        let route = match value {
            Value::Table(_) => Route::Hostnames(strings(value, &path)?),
            _ => Route::Direct(string(value, &path)?),
        };
        routes.insert(key, route);
    }
    if routes.is_empty() {
        return Err("routes: must not be empty".into());
    }
    Ok(routes)
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
    integer(table, "limits", key, default, max)
}

fn options_map(root: &Table, key: &str) -> Result<Option<Table>, String> {
    match root.get::<Value>(key).map_err(|e| e.to_string())? {
        Value::Nil => Ok(None),
        value => table(value, key).map(Some),
    }
}

fn options(root: &Table, key: &str, allowed: &[&str]) -> Result<Option<Table>, String> {
    let result = options_map(root, key)?;
    if let Some(t) = &result {
        fields(t, allowed, key)?;
    }
    Ok(result)
}

fn integer(
    table: &Table,
    path: &str,
    key: &str,
    default: usize,
    max: usize,
) -> Result<usize, String> {
    let value: Value = table.get(key).map_err(|e| e.to_string())?;
    // LuaJIT can represent integral arithmetic results as floating-point numbers.
    let number = match value {
        Value::Nil => return Ok(default),
        Value::Integer(value) if value > 0 && value as u64 <= max as u64 => {
            return Ok(value as usize);
        }
        Value::Number(value) => value,
        _ => return Err(format!("{path}.{key}: expected an integer in 1..={max}")),
    };
    if !number.is_finite() || number.fract() != 0.0 || number < 1.0 || number >= (max as f64 + 1.0)
    {
        return Err(format!("{path}.{key}: expected an integer in 1..={max}"));
    }
    Ok(number as usize)
}

#[cfg(test)]
mod tests;
