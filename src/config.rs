use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Read},
    net::SocketAddr,
    path::Path,
    time::{Duration, Instant},
};

use crate::routing::{Backend, Mode, Routes};
use mlua::{Table, Value};
use tokio::sync::Semaphore;

pub use crate::message_script::MessageScript;
pub use crate::{http_script::HttpScript, script::RouteScript};
pub use managed::ManagedServer;
pub use messaging::{MessagingConfig, MessagingPrincipal, MessagingStream, MessagingSubscription};
pub use services::{InstanceStorage, ServiceGroup, ServiceInstance, ServiceScaling};
pub use templates::ServerTemplate;

mod managed;
mod messaging;
mod services;
mod templates;

// Configuration evaluation includes cold VM setup and can be descheduled on
// busy hosts. Keep it bounded without applying the latency-sensitive callback
// deadline; the shared instruction, memory and source limits still apply.
const CONFIGURATION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listeners: BTreeMap<String, SocketAddr>,
    pub backends: BTreeMap<String, Backend>,
    pub managed_servers: BTreeMap<String, ManagedServer>,
    pub templates: BTreeMap<String, ServerTemplate>,
    pub service_groups: BTreeMap<String, ServiceGroup>,
    pub instances: BTreeMap<String, ServiceInstance>,
    pub routes: BTreeMap<String, Route>,
    pub limits: Limits,
    pub authentication: Authentication,
    pub forwarding: Option<VelocityForwarding>,
    pub on_route: Option<RouteScript>,
    pub extensions: Option<crate::extensions::ExtensionScript>,
    pub on_http: Option<HttpScript>,
    pub on_message: Option<MessageScript>,
    pub messaging: Option<MessagingConfig>,
    pub fallbacks: BTreeMap<String, Vec<String>>,
    pub network: Network,
    pub rate_limit: Option<RateLimit>,
    pub login_rate_limit: Option<RateLimit>,
    pub health_check: Option<HealthCheck>,
    pub status_cache: Option<StatusCache>,
    pub metrics: Option<SocketAddr>,
    pub admin: Option<Admin>,
    pub maintenance: bool,
    pub draining: BTreeSet<String>,
    pub web: Option<WebConfig>,
    pub status: Option<StatusConfig>,
    pub shutdown_timeout: Duration,
}

/// Gameplay destinations and access rules. An empty initial list retains the
/// ordinary route and its fallbacks. `initial` replaces only a default direct
/// route; hostname and explicit script selections retain their own policy.
/// Hubs are explicit destinations for `/hub` and outage recovery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Network {
    /// Handle trusted backend BungeeCord transfer and query plugin messages.
    pub bungeecord: bool,
    pub initial: Vec<String>,
    pub hubs: Vec<String>,
    pub access: BTreeMap<String, ServerAccess>,
}

/// Names are matched without ASCII case. An absent allow list is public; an
/// explicitly empty allow list denies everyone. A deny entry always wins.
/// Names are authenticated only when online mode is enabled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerAccess {
    pub allow: Option<Vec<String>>,
    pub deny: Vec<String>,
}

/// Online authentication and Velocity forwarding are enabled together. Existing
/// configurations retain their explicitly documented offline behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authentication {
    pub online_mode: bool,
    pub timeout: Duration,
}

impl Default for Authentication {
    fn default() -> Self {
        Self {
            online_mode: false,
            timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VelocityForwarding {
    /// The configuration stores a variable name, never its secret value.
    pub secret_env: String,
}

impl ServerAccess {
    pub fn permits(&self, username: &str) -> bool {
        !self
            .deny
            .iter()
            .any(|name| name.eq_ignore_ascii_case(username))
            && self
                .allow
                .as_ref()
                .is_none_or(|names| names.iter().any(|name| name.eq_ignore_ascii_case(username)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admin {
    pub listen: SocketAddr,
    pub token_env: String,
    pub permissions: BTreeSet<String>,
}

pub const ADMIN_PERMISSIONS: &[&str] = &[
    "status",
    "maintenance",
    "drain",
    "transfer",
    "reload",
    "shutdown",
    "servers",
];

/// Optional administration service. Credentials are mandatory for non-loopback binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebConfig {
    pub listen: SocketAddr,
    pub token: Option<String>,
    pub api: bool,
    pub ui: bool,
    pub operators: BTreeMap<String, WebOperator>,
    pub records_directory: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebOperator {
    pub token: String,
    pub permissions: BTreeSet<String>,
    /// None grants access to every group and static managed server.
    pub groups: Option<BTreeSet<String>>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8080".parse().unwrap(),
            token: None,
            api: true,
            ui: true,
            operators: BTreeMap::new(),
            records_directory: None,
        }
    }
}

/// Optional read-only status service, independent of the administration service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusConfig {
    pub listen: SocketAddr,
    pub ui: bool,
    pub metrics: bool,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:9090".parse().unwrap(),
            ui: true,
            metrics: true,
        }
    }
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
            // Packet buffers grow as bytes arrive; budget for framed packets, sockets and runtime.
            max_connections: 1024,
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
            managed_servers: BTreeMap::new(),
            templates: BTreeMap::new(),
            service_groups: BTreeMap::new(),
            instances: BTreeMap::new(),
            routes: BTreeMap::from([("default".into(), Route::Direct("default".into()))]),
            limits: Limits::default(),
            authentication: Authentication::default(),
            forwarding: None,
            on_route: None,
            extensions: None,
            on_http: None,
            on_message: None,
            messaging: None,
            fallbacks: BTreeMap::new(),
            network: Network::default(),
            rate_limit: None,
            login_rate_limit: None,
            health_check: None,
            status_cache: None,
            metrics: None,
            admin: None,
            maintenance: false,
            draining: BTreeSet::new(),
            web: None,
            status: None,
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
        Self::from_lua_at(&source, path)
    }

    /// Evaluate an in-memory script without access to local modules or plugins.
    pub fn from_lua(source: &str, name: &str) -> io::Result<Self> {
        Self::from_source(
            crate::script::ScriptSource::new(source, name),
            Path::new("."),
            None,
        )
    }

    /// Evaluate edited source with modules relative to its configuration path.
    /// The entry file need not exist yet (for validation and atomic saves).
    pub fn from_lua_at(source: &str, path: &Path) -> io::Result<Self> {
        let source = crate::script::ScriptSource::from_path(source, path)
            .map_err(|error| invalid(format!("{}: {error}", path.display())))?;
        let directory = source.root.clone();
        Self::from_source(source, &directory, None)
    }

    /// Validate edited configuration against the current runtime instances.
    /// Definitions supplied by Lua cannot replace a live instance.
    pub fn from_lua_at_with_instances(
        source: &str,
        path: &Path,
        previous: &Self,
    ) -> io::Result<Self> {
        let source = crate::script::ScriptSource::from_path(source, path)
            .map_err(|error| invalid(format!("{}: {error}", path.display())))?;
        let directory = source.root.clone();
        Self::from_source(source, &directory, Some(previous))
    }

    /// Validate an in-memory script against the current runtime instances.
    pub fn from_lua_with_instances(source: &str, name: &str, previous: &Self) -> io::Result<Self> {
        Self::from_source(
            crate::script::ScriptSource::new(source, name),
            Path::new("."),
            Some(previous),
        )
    }

    fn from_source(
        source: crate::script::ScriptSource,
        directory: &Path,
        previous: Option<&Self>,
    ) -> io::Result<Self> {
        let name = source.entry.name.as_ref();
        let parse = || -> Result<Self, String> {
            let (_lua, value) =
                crate::script::load(&source, Instant::now() + CONFIGURATION_TIMEOUT)
                    .map_err(|e| e.to_string())?;
            let root = table(value, "configuration")?;
            fields(
                &root,
                &[
                    "listeners",
                    "backends",
                    "managed_servers",
                    "templates",
                    "service_groups",
                    "routes",
                    "limits",
                    "authentication",
                    "forwarding",
                    "on_route",
                    "extensions",
                    "on_http",
                    "on_message",
                    "messaging",
                    "fallbacks",
                    "network",
                    "rate_limit",
                    "login_rate_limit",
                    "health_check",
                    "status_cache",
                    "metrics",
                    "admin",
                    "maintenance",
                    "draining",
                    "web",
                    "status",
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
            let managed_servers = managed::parse(&root, directory)?;
            let templates = templates::parse(&root, directory)?;
            let service_groups = services::parse(&_lua, &root, directory)?;
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
            let extensions = crate::extensions::ExtensionScript::parse(&root, &source)
                .map_err(|e| e.to_string())?;
            let on_route = match root
                .raw_get::<Value>("on_route")
                .map_err(|e| e.to_string())?
            {
                Value::Nil => None,
                Value::Function(_) => Some(RouteScript::new(&source)),
                _ => return Err("config.on_route: expected a function".into()),
            };
            let on_http = match root
                .raw_get::<Value>("on_http")
                .map_err(|e| e.to_string())?
            {
                Value::Nil => None,
                Value::Function(_) => Some(HttpScript::new(&source)),
                _ => return Err("config.on_http: expected a function".into()),
            };
            let connection_rate_limit = rate_limit(&root, "rate_limit")?;
            let on_message = match root
                .raw_get::<Value>("on_message")
                .map_err(|e| e.to_string())?
            {
                Value::Nil => None,
                Value::Function(_) => Some(MessageScript::new(&source)),
                _ => return Err("config.on_message: expected a function".into()),
            };
            let messaging = messaging::parse(&root)?;
            let login_rate_limit = rate_limit(&root, "login_rate_limit")?;
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
                Value::Nil | Value::Boolean(false) => None,
                value => Some(
                    address(&string(value, "metrics")?, "metrics").map_err(|e| e.to_string())?,
                ),
            };
            let admin = options(&root, "admin", &["listen", "token_env", "permissions"])?
                .map(|t| {
                    let listen = address(
                        &string(t.get("listen").map_err(|e| e.to_string())?, "admin.listen")?,
                        "admin.listen",
                    )
                    .map_err(|e| e.to_string())?;
                    let token_env = match t.get::<Value>("token_env").map_err(|e| e.to_string())? {
                        Value::Nil => "RIFT_ADMIN_TOKEN".to_owned(),
                        value => string(value, "admin.token_env")?,
                    };
                    let permissions = string_set(
                        t.get("permissions").map_err(|e| e.to_string())?,
                        "admin.permissions",
                    )?;
                    Ok::<_, String>(Admin {
                        listen,
                        token_env,
                        permissions,
                    })
                })
                .transpose()?;
            let maintenance = match root
                .get::<Value>("maintenance")
                .map_err(|e| e.to_string())?
            {
                Value::Nil => false,
                Value::Boolean(value) => value,
                _ => return Err("maintenance: expected a boolean".into()),
            };
            let draining = match root.get::<Value>("draining").map_err(|e| e.to_string())? {
                Value::Nil => BTreeSet::new(),
                value => string_set(value, "draining")?,
            };
            let web = web_options(&root, directory)?;
            let status = status_options(&root)?;
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
            let network = network(&root)?;
            let authentication = authentication(&root)?;
            let forwarding = forwarding(&root)?;
            Ok(Self {
                listeners,
                backends,
                managed_servers,
                templates,
                service_groups,
                instances: BTreeMap::new(),
                routes,
                limits,
                authentication,
                forwarding,
                on_route,
                extensions,
                on_http,
                on_message,
                messaging,
                fallbacks,
                network,
                rate_limit: connection_rate_limit,
                login_rate_limit,
                health_check,
                status_cache,
                metrics,
                admin,
                maintenance,
                draining,
                web,
                status,
                shutdown_timeout,
            })
        };
        let mut config = parse().map_err(|error| invalid(format!("{name}: {error}")))?;
        if let Some(previous) = previous {
            services::restore_instances(&mut config, previous)
                .map_err(|error| invalid(format!("{name}: {error}")))?;
        }
        config
            .validate()
            .map_err(|error| invalid(format!("{name}: {error}")))?;
        Ok(config)
    }

    pub fn validate(&self) -> io::Result<()> {
        services::validate(self).map_err(invalid)?;
        managed::validate(&self.managed_servers, &self.backends).map_err(invalid)?;
        templates::validate(self).map_err(invalid)?;
        if let Some(extensions) = &self.extensions {
            if !self.authentication.online_mode {
                return Err(invalid("extensions requires authentication.online_mode"));
            }
            for (backend, capacity) in &extensions.queues {
                if !(1..=100_000).contains(capacity) {
                    return Err(invalid("extensions.queues: capacity must be 1..=100000"));
                }
                if !self.is_destination(backend) {
                    return Err(invalid("extensions.queues: unknown backend"));
                }
            }
        }
        if let Some(messaging) = &self.messaging {
            messaging.validate().map_err(invalid)?;
        }
        let subscriptions = self
            .messaging
            .as_ref()
            .is_some_and(|config| !config.subscriptions.is_empty());
        if self.on_message.is_some() != subscriptions {
            return Err(invalid(
                "on_message and messaging.subscriptions must be configured together",
            ));
        }
        if self.authentication.online_mode != self.forwarding.is_some() {
            return Err(invalid(
                "authentication.online_mode and forwarding.mode = 'velocity' must be enabled together",
            ));
        }
        if self.authentication.timeout < Duration::from_millis(1)
            || self.authentication.timeout > Duration::from_secs(60)
        {
            return Err(invalid("authentication.timeout_ms: expected 1..=60000"));
        }
        if let Some(forwarding) = &self.forwarding {
            let mut bytes = forwarding.secret_env.bytes();
            if !bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(invalid(
                    "forwarding.secret_env: expected an environment variable name such as RIFT_FORWARDING_SECRET",
                ));
            }
        }
        for (path, empty, blank_name) in [
            (
                "listeners",
                self.listeners.is_empty(),
                self.listeners.keys().any(|name| name.trim().is_empty()),
            ),
            (
                "backends",
                self.backends.is_empty() && self.service_groups.is_empty(),
                self.backends.keys().any(|name| name.trim().is_empty()),
            ),
        ] {
            if empty {
                return Err(invalid(format!("{path}: must not be empty")));
            }
            if blank_name {
                return Err(invalid(format!("{path}: names must not be empty")));
            }
        }
        if self.network.bungeecord {
            let mut names = BTreeSet::new();
            for name in self.backends.keys().chain(self.service_groups.keys()) {
                if !names.insert(name.to_ascii_lowercase()) {
                    return Err(invalid(
                        "network.bungeecord: backend names must be unique ignoring ASCII case",
                    ));
                }
            }
        }
        for name in &self.draining {
            if !self.backends.contains_key(name) {
                return Err(invalid(format!("draining: unknown backend {name:?}")));
            }
        }
        for (path, limits) in [
            ("rate_limit", self.rate_limit),
            ("login_rate_limit", self.login_rate_limit),
        ] {
            if let Some(limits) = limits {
                for (field, value) in [
                    ("per_ip_per_second", limits.per_ip_per_second),
                    ("per_ip_burst", limits.per_ip_burst),
                    ("global_per_second", limits.global_per_second),
                    ("global_burst", limits.global_burst),
                    ("max_ips", limits.max_ips),
                ] {
                    if !(1..=1_000_000).contains(&value) {
                        return Err(invalid(format!(
                            "{path}.{field}: expected an integer in 1..=1000000"
                        )));
                    }
                }
            }
        }
        if let Some(admin) = &self.admin {
            if !admin.listen.ip().is_loopback() {
                return Err(invalid(
                    "admin.listen: use a loopback IP address (127.0.0.1 or ::1)",
                ));
            }
            let mut bytes = admin.token_env.bytes();
            if !bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(invalid(
                    "admin.token_env: expected an environment variable name such as RIFT_ADMIN_TOKEN",
                ));
            }
            if admin.permissions.is_empty() {
                return Err(invalid(
                    "admin.permissions: explicitly grant at least one permission",
                ));
            }
            for permission in &admin.permissions {
                if !ADMIN_PERMISSIONS.contains(&permission.as_str()) {
                    return Err(invalid(format!(
                        "admin.permissions: unknown permission {permission:?} (expected {})",
                        ADMIN_PERMISSIONS.join(", ")
                    )));
                }
            }
            for (name, listen) in &self.listeners {
                if addresses_overlap(admin.listen, *listen) {
                    return Err(invalid(format!(
                        "admin.listen: address conflicts with listeners.{name}; choose another port"
                    )));
                }
            }
            for (name, listen) in [
                ("metrics", self.metrics),
                ("web.listen", self.web.as_ref().map(|web| web.listen)),
                ("status.listen", self.status.map(|status| status.listen)),
            ] {
                if listen.is_some_and(|listen| addresses_overlap(admin.listen, listen)) {
                    return Err(invalid(format!(
                        "admin.listen: address conflicts with {name}; choose another port"
                    )));
                }
            }
            for (name, backend) in &self.backends {
                if service_backend_loop(admin.listen, backend) {
                    return Err(invalid(format!(
                        "admin.listen and backends.{name} must not point to the same socket"
                    )));
                }
            }
        }
        for (label, names) in [
            ("network.initial", &self.network.initial),
            ("network.hubs", &self.network.hubs),
        ] {
            let mut seen = std::collections::HashSet::new();
            if names.len() > 16 {
                return Err(invalid(format!("{label}: at most 16 backend names")));
            }
            for name in names {
                if !self.is_destination(name) || !seen.insert(name) {
                    return Err(invalid(format!(
                        "{label}: unknown or duplicate backend {name:?}"
                    )));
                }
            }
        }
        for (backend, access) in &self.network.access {
            if !self.is_destination(backend) {
                return Err(invalid(format!(
                    "network.access.{backend}: unknown backend"
                )));
            }
            for (label, names) in [
                ("allow", access.allow.as_deref().unwrap_or_default()),
                ("deny", access.deny.as_slice()),
            ] {
                let mut seen = std::collections::HashSet::new();
                for name in names {
                    if !crate::players::valid_username(name)
                        || !seen.insert(name.to_ascii_lowercase())
                    {
                        return Err(invalid(format!(
                            "network.access.{backend}.{label}: invalid or duplicate player name {name:?}"
                        )));
                    }
                }
            }
        }
        for (name, targets) in &self.fallbacks {
            if !self.is_destination(name) {
                return Err(invalid(format!("fallbacks.{name}: unknown backend")));
            }
            let mut seen = std::collections::HashSet::new();
            for target in targets {
                if !self.is_destination(target) || target == name || !seen.insert(target) {
                    return Err(invalid(format!(
                        "fallbacks.{name}: unknown, duplicate or self backend {target:?}"
                    )));
                }
            }
        }
        // Lists are deliberately flat: fallback targets' own lists are not followed.
        if let Some(web) = &self.web {
            if web.operators.len() > 128 {
                return Err(invalid("web.operators: at most 128 operators"));
            }
            let mut tokens = BTreeSet::new();
            if let Some(token) = &web.token {
                tokens.insert(token.clone());
            }
            for (name, operator) in &web.operators {
                if name.is_empty()
                    || name.len() > 128
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                    || [
                        "admin",
                        "local",
                        "anonymous",
                        "system",
                        "signal",
                        "local-admin",
                        "scaler",
                    ]
                    .contains(&name.as_str())
                {
                    return Err(invalid("web.operators: invalid or reserved operator name"));
                }
                if !(16..=4096).contains(&operator.token.len())
                    || !operator.token.bytes().all(|b| b.is_ascii_graphic())
                    || !tokens.insert(operator.token.clone())
                {
                    return Err(invalid(
                        "web.operators: tokens must be unique printable secrets of 16..=4096 bytes",
                    ));
                }
                if operator.permissions.iter().any(|p| {
                    ![
                        "read",
                        "logs",
                        "console",
                        "servers",
                        "config",
                        "deploy",
                        "audit",
                        "extensions",
                    ]
                    .contains(&p.as_str())
                }) {
                    return Err(invalid("web.operators: unknown permission"));
                }
                if let Some(groups) = &operator.groups
                    && (groups.len() > 128
                        || operator
                            .permissions
                            .iter()
                            .any(|p| ["config", "deploy", "extensions"].contains(&p.as_str())))
                {
                    return Err(invalid(
                        "web.operators: config, deploy and extensions require unrestricted group access; at most 128 scopes",
                    ));
                }
            }
            if let Some(token) = &web.token {
                if !(16..=4096).contains(&token.len())
                    || !token.bytes().all(|byte| byte.is_ascii_graphic())
                {
                    return Err(invalid(
                        "web.token: expected 16..=4096 printable ASCII bytes without whitespace",
                    ));
                }
            } else if web.operators.is_empty() && !web.listen.ip().is_loopback() {
                return Err(invalid(
                    "web.token: required when web.listen is not loopback",
                ));
            }
        }
        for (name, listen) in &self.listeners {
            if !self.routes.contains_key(name) {
                return Err(invalid(format!(
                    "routes: missing route for listener {name:?}"
                )));
            }
            for (other_name, other) in self.listeners.range(..name.clone()) {
                if addresses_overlap(*listen, *other) {
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
        let services = [
            ("metrics", self.metrics),
            ("web.listen", self.web.as_ref().map(|web| web.listen)),
            ("status.listen", self.status.map(|status| status.listen)),
        ];
        for (index, (name, listen)) in services.iter().enumerate() {
            let Some(listen) = listen else { continue };
            for (listener_name, other) in &self.listeners {
                if addresses_overlap(*listen, *other) {
                    return Err(invalid(format!(
                        "{name}: duplicate address {listen} (also listeners.{listener_name})"
                    )));
                }
            }
            for (other_name, other) in &services[..index] {
                if other.is_some_and(|other| addresses_overlap(*listen, other)) {
                    return Err(invalid(format!(
                        "{name}: duplicate address {listen} (also {other_name})"
                    )));
                }
            }
            for (backend_name, backend) in &self.backends {
                if service_backend_loop(*listen, backend) {
                    return Err(invalid(format!(
                        "{name} and backends.{backend_name} must not point to the same socket"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Apply the same rule to initial login, commands and failure recovery.
    /// Unknown destinations are never permitted, even with no access entry.
    pub fn can_access(&self, backend: &str, username: &str) -> bool {
        self.is_destination(backend)
            && self
                .network
                .access
                .get(backend)
                .is_none_or(|access| access.permits(username))
            && self.instances.get(backend).is_none_or(|instance| {
                self.network
                    .access
                    .get(&instance.group)
                    .is_none_or(|access| access.permits(username))
            })
    }

    pub fn mode(&self, listener: &str) -> io::Result<Mode> {
        let backend = |name: &str| {
            self.backends
                .get(name)
                .cloned()
                .or_else(|| {
                    self.service_groups.get(name).and_then(|group| {
                        Backend::parse(&format!("127.0.0.1:{}", group.port_start)).ok()
                    })
                })
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

fn authentication(root: &Table) -> Result<Authentication, String> {
    let Some(values) = options(root, "authentication", &["online_mode", "timeout_ms"])? else {
        return Ok(Authentication::default());
    };
    Ok(Authentication {
        online_mode: boolean(&values, "authentication", "online_mode", false)?,
        timeout: Duration::from_millis(integer(
            &values,
            "authentication",
            "timeout_ms",
            10_000,
            60_000,
        )? as u64),
    })
}

fn forwarding(root: &Table) -> Result<Option<VelocityForwarding>, String> {
    let Some(values) = options(root, "forwarding", &["mode", "secret_env"])? else {
        return Ok(None);
    };
    let mode = string(
        values.get("mode").map_err(|e| e.to_string())?,
        "forwarding.mode",
    )?;
    if mode != "velocity" {
        return Err("forwarding.mode: expected 'velocity' (omit forwarding to disable)".into());
    }
    let secret_env = match values
        .get::<Value>("secret_env")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => "RIFT_FORWARDING_SECRET".into(),
        value => string(value, "forwarding.secret_env")?,
    };
    Ok(Some(VelocityForwarding { secret_env }))
}

fn network(root: &Table) -> Result<Network, String> {
    let Some(values) = options(
        root,
        "network",
        &["bungeecord", "initial", "hubs", "access"],
    )?
    else {
        return Ok(Network::default());
    };
    let mut network = Network {
        bungeecord: boolean(&values, "network", "bungeecord", false)?,
        ..Network::default()
    };
    for (key, destination) in [
        ("initial", &mut network.initial),
        ("hubs", &mut network.hubs),
    ] {
        let value: Value = values.get(key).map_err(|e| e.to_string())?;
        if !value.is_nil() {
            *destination = string_list(value, &format!("network.{key}"), 16)?;
        }
    }
    if let Some(entries) = options_map(&values, "access")? {
        for pair in entries.pairs::<Value, Value>() {
            let (backend, value) = pair.map_err(|e| e.to_string())?;
            let backend = string(backend, "network.access key")?;
            let path = format!("network.access.{backend}");
            let rule = table(value, &path)?;
            fields(&rule, &["allow", "deny"], &path)?;
            let allow: Value = rule.get("allow").map_err(|e| e.to_string())?;
            let deny: Value = rule.get("deny").map_err(|e| e.to_string())?;
            network.access.insert(
                backend,
                ServerAccess {
                    allow: if allow.is_nil() {
                        None
                    } else {
                        Some(string_list(allow, &format!("{path}.allow"), 65536)?)
                    },
                    deny: if deny.is_nil() {
                        Vec::new()
                    } else {
                        string_list(deny, &format!("{path}.deny"), 65536)?
                    },
                },
            );
        }
    }
    Ok(network)
}

fn string_list(value: Value, path: &str, maximum: usize) -> Result<Vec<String>, String> {
    let mut ordered = BTreeMap::new();
    for pair in table(value, path)?.pairs::<Value, Value>() {
        let (index, value) = pair.map_err(|e| e.to_string())?;
        let Value::Integer(index) = index else {
            return Err(format!("{path}: expected a dense array"));
        };
        ordered.insert(index, string(value, path)?);
    }
    if ordered.len() > maximum || ordered.keys().copied().ne(1..=ordered.len() as i64) {
        return Err(format!(
            "{path}: expected a dense array of at most {maximum} strings"
        ));
    }
    Ok(ordered.into_values().collect())
}

/// Port zero requests an independently assigned port, so it never overlaps.
fn addresses_overlap(a: SocketAddr, b: SocketAddr) -> bool {
    if a.port() == 0 || a.port() != b.port() {
        return false;
    }
    let a = a.ip().to_canonical();
    let b = b.ip().to_canonical();
    a == b
        || (a.is_ipv4() == b.is_ipv4() && (a.is_unspecified() || b.is_unspecified()))
        // Tokio's IPv6 wildcard may also accept IPv4 on dual-stack systems.
        || (a.is_ipv6() && a.is_unspecified())
        || (b.is_ipv6() && b.is_unspecified())
}

fn service_backend_loop(listen: SocketAddr, backend: &Backend) -> bool {
    if let Ok(target) = backend.address().parse::<SocketAddr>() {
        return addresses_overlap(listen, target);
    }
    // Reject the standard loopback DNS name without resolving arbitrary hosts
    // during configuration evaluation. Runtime loop checks still guard DNS.
    let Some((host, port)) = backend.address().rsplit_once(':') else {
        return false;
    };
    listen.port() != 0
        && port.parse::<u16>().ok() == Some(listen.port())
        && host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        && (listen.ip().is_loopback() || listen.ip().is_unspecified())
}

fn service_options(root: &Table, key: &str, allowed: &[&str]) -> Result<Option<Table>, String> {
    match root.raw_get::<Value>(key).map_err(|e| e.to_string())? {
        Value::Nil | Value::Boolean(false) => Ok(None),
        value => {
            let values = table(value, key)?;
            fields(&values, allowed, key)?;
            Ok(Some(values))
        }
    }
}

fn boolean(table: &Table, path: &str, key: &str, default: bool) -> Result<bool, String> {
    match table.raw_get::<Value>(key).map_err(|e| e.to_string())? {
        Value::Nil => Ok(default),
        Value::Boolean(value) => Ok(value),
        _ => Err(format!("{path}.{key}: expected a boolean")),
    }
}

fn service_listen(table: &Table, path: &str, default: SocketAddr) -> Result<SocketAddr, String> {
    match table
        .raw_get::<Value>("listen")
        .map_err(|e| e.to_string())?
    {
        Value::Nil => Ok(default),
        value => {
            let path = format!("{path}.listen");
            address(&string(value, &path)?, &path).map_err(|error| error.to_string())
        }
    }
}

fn web_options(root: &Table, base: &Path) -> Result<Option<WebConfig>, String> {
    let Some(table) = service_options(
        root,
        "web",
        &[
            "enabled",
            "listen",
            "token",
            "api",
            "ui",
            "operators",
            "records_directory",
        ],
    )?
    else {
        return Ok(None);
    };
    let enabled = boolean(&table, "web", "enabled", true)?;
    let token = match table.raw_get::<Value>("token").map_err(|e| e.to_string())? {
        Value::Nil => None,
        value => Some(string(value, "web.token")?),
    };
    let mut operators = BTreeMap::new();
    let mut tokens = BTreeSet::new();
    if let Some(token) = &token {
        tokens.insert(token.clone());
    }
    if let Some(values) = options_map(&table, "operators")? {
        for pair in values.pairs::<Value, Value>() {
            let (name, value) = pair.map_err(|e| e.to_string())?;
            let name = string(name, "web.operators key")?;
            if name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return Err("web.operators: invalid operator name".into());
            }
            let path = format!("web.operators.{name}");
            let value = self::table(value, &path)?;
            fields(&value, &["token", "permissions", "groups"], &path)?;
            let token = string(
                value.raw_get("token").map_err(|e| e.to_string())?,
                &format!("{path}.token"),
            )?;
            if !(16..=4096).contains(&token.len())
                || !token.bytes().all(|b| b.is_ascii_graphic())
                || !tokens.insert(token.clone())
            {
                return Err(format!(
                    "{path}.token: expected a unique secret of 16..=4096 printable bytes"
                ));
            }
            let permissions = string_set(
                value.raw_get("permissions").map_err(|e| e.to_string())?,
                &format!("{path}.permissions"),
            )?;
            if permissions.iter().any(|p| {
                ![
                    "read",
                    "logs",
                    "console",
                    "servers",
                    "config",
                    "deploy",
                    "audit",
                    "extensions",
                ]
                .contains(&p.as_str())
            }) {
                return Err(format!("{path}.permissions: unknown permission"));
            }
            let groups = match value
                .raw_get::<Value>("groups")
                .map_err(|e| e.to_string())?
            {
                Value::Nil => None,
                value => Some(string_set(value, &format!("{path}.groups"))?),
            };
            if groups.is_some()
                && permissions
                    .iter()
                    .any(|p| ["config", "deploy", "extensions"].contains(&p.as_str()))
            {
                return Err(format!(
                    "{path}: config, deploy and extensions require unrestricted group access"
                ));
            }
            operators.insert(
                name,
                WebOperator {
                    token,
                    permissions,
                    groups,
                },
            );
        }
    }
    if operators.len() > 128 {
        return Err("web.operators: at most 128 operators".into());
    }
    let config = WebConfig {
        listen: service_listen(&table, "web", WebConfig::default().listen)?,
        token,
        api: boolean(&table, "web", "api", true)?,
        ui: boolean(&table, "web", "ui", true)?,
        operators,
        records_directory: match table
            .raw_get::<Value>("records_directory")
            .map_err(|e| e.to_string())?
        {
            Value::Nil => None,
            value => {
                let value = string(value, "web.records_directory")?;
                if value.trim().is_empty()
                    || value.len() > 4096
                    || value.chars().any(char::is_control)
                {
                    return Err("web.records_directory: invalid path".into());
                }
                Some(base.join(value))
            }
        },
    };
    Ok(enabled.then_some(config))
}

fn status_options(root: &Table) -> Result<Option<StatusConfig>, String> {
    let Some(table) = service_options(root, "status", &["enabled", "listen", "ui", "metrics"])?
    else {
        return Ok(None);
    };
    let enabled = boolean(&table, "status", "enabled", true)?;
    let config = StatusConfig {
        listen: service_listen(&table, "status", StatusConfig::default().listen)?,
        ui: boolean(&table, "status", "ui", true)?,
        metrics: boolean(&table, "status", "metrics", true)?,
    };
    Ok(enabled.then_some(config))
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
    // Script-created diagnostics may be much larger than the source text.
    // Keep errors safe to return through HTTP and to print in server logs.
    let message = message.into();
    io::Error::new(
        io::ErrorKind::InvalidInput,
        message.chars().take(2048).collect::<String>(),
    )
}

fn string_set(value: Value, path: &str) -> Result<BTreeSet<String>, String> {
    let mut ordered = BTreeMap::new();
    for pair in table(value, path)?.pairs::<Value, Value>() {
        let (index, value) = pair.map_err(|e| e.to_string())?;
        let Value::Integer(index) = index else {
            return Err(format!("{path}: expected a dense array of distinct names"));
        };
        ordered.insert(index, string(value, path)?);
    }
    if ordered.keys().copied().ne(1..=ordered.len() as i64) {
        return Err(format!("{path}: expected a dense array of distinct names"));
    }
    let mut result = BTreeSet::new();
    for name in ordered.into_values() {
        if !result.insert(name.clone()) {
            return Err(format!("{path}: duplicate name {name:?}"));
        }
    }
    Ok(result)
}

fn rate_limit(root: &Table, path: &str) -> Result<Option<RateLimit>, String> {
    options(
        root,
        path,
        &[
            "per_ip_per_second",
            "per_ip_burst",
            "global_per_second",
            "global_burst",
            "max_ips",
        ],
    )?
    .map(|t| {
        Ok(RateLimit {
            per_ip_per_second: integer(&t, path, "per_ip_per_second", 20, 1_000_000)?,
            per_ip_burst: integer(&t, path, "per_ip_burst", 40, 1_000_000)?,
            global_per_second: integer(&t, path, "global_per_second", 200, 1_000_000)?,
            global_burst: integer(&t, path, "global_burst", 400, 1_000_000)?,
            max_ips: integer(&t, path, "max_ips", 65536, 1_000_000)?,
        })
    })
    .transpose()
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
    if result.is_empty() && path != "backends" {
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
