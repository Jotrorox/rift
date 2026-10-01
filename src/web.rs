//! Axum control and observability services. Assets are embedded in the binary.
use crate::{admin, control, metrics::Metrics, operator, runtime::Snapshot};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Extension, Path, RawQuery, Request, State, rejection::JsonRejection,
    },
    http::{HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{any, delete, get, post},
};
use rift::{
    config::Config,
    http_script::{HttpError, HttpRequest},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    collections::BTreeSet,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};

#[derive(Clone)]
pub struct App {
    pub current: watch::Receiver<Arc<Snapshot>>,
    pub metrics: Arc<Metrics>,
    pub commands: control::Sender,
    pub path: Option<PathBuf>,
    pub started: Instant,
    pub workflows: Arc<operator::Store>,
    records_directory: Option<PathBuf>,
    binding: Option<(Kind, SocketAddr)>,
    requests: Arc<Semaphore>,
    validations: Arc<Semaphore>,
}
impl App {
    pub fn new(
        current: watch::Receiver<Arc<Snapshot>>,
        metrics: Arc<Metrics>,
        commands: control::Sender,
        path: Option<PathBuf>,
    ) -> io::Result<Self> {
        let snapshot = current.borrow().clone();
        let configured_directory = snapshot
            .config
            .web
            .as_ref()
            .and_then(|web| web.records_directory.clone());
        let directory = configured_directory.clone().or_else(|| {
            (snapshot.config.web.is_some() || snapshot.config.admin.is_some())
                .then(|| {
                    path.as_ref().map(|path| {
                        path.with_file_name(format!(
                            "{}.operators",
                            path.file_name().unwrap().to_string_lossy()
                        ))
                    })
                })
                .flatten()
        });
        let store = match operator::Store::open(directory) {
            Ok(store) => store,
            Err(e)
                if configured_directory.is_none()
                    && matches!(
                        e.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
                    ) =>
            {
                eprintln!(
                    "rift: operator records are in memory; configure web.records_directory on a writable filesystem for retention"
                );
                operator::Store::open(None)?
            }
            Err(e) => return Err(e),
        };
        let workflows = Arc::new(store);
        if let Some(source) = current.borrow().source.as_deref() {
            workflows.deploy(source, "system", "startup")?;
        }
        Ok(Self {
            current,
            metrics,
            commands,
            path,
            started: Instant::now(),
            workflows,
            records_directory: configured_directory,
            binding: None,
            requests: Arc::new(Semaphore::new(64)),
            validations: Arc::new(Semaphore::new(4)),
        })
    }
    fn snapshot(&self) -> Arc<Snapshot> {
        self.current.borrow().clone()
    }
    pub(crate) fn record_settings_compatible(&self, config: &Config) -> bool {
        config
            .web
            .as_ref()
            .is_none_or(|web| web.records_directory == self.records_directory)
    }
    fn active_binding(&self, snapshot: &Snapshot) -> bool {
        self.binding.is_none_or(|(kind, address)| {
            snapshot.service_addresses.get(kind.name()) == Some(&address)
        })
    }
    fn status(&self, private: bool) -> Value {
        self.status_snapshot(&self.snapshot(), private)
    }
    fn status_snapshot(&self, snapshot: &Snapshot, private: bool) -> Value {
        let config = &snapshot.config;
        let listeners: Vec<_> = config.listeners.iter().map(|(name, address)| json!({"name":name,"address":snapshot.listener_addresses.get(name).unwrap_or(address).to_string()})).collect();
        let backends: Vec<_> = config.backends.iter().map(|(name, address)| json!({"name":name,"address":address.address(),"healthy":snapshot.health.available(name)})).collect();
        let mut value = json!({
            "version":env!("CARGO_PKG_VERSION"), "uptime_seconds":self.started.elapsed().as_secs(),
            "revision":snapshot.revision, "listeners":listeners, "backends":backends,
            "metrics":self.metrics.values(), "health_checks_enabled":config.health_check.is_some(),
            "http_hook":config.on_http.is_some(),
            "services":{
                "web":config.web.as_ref().map(|c| json!({"listen":snapshot.service_addresses.get("web").unwrap_or(&c.listen).to_string(),"api":c.api,"ui":c.ui})),
                "status":config.status.as_ref().map(|c| json!({"listen":snapshot.service_addresses.get("status").unwrap_or(&c.listen).to_string(),"ui":c.ui,"metrics":c.metrics})),
                "metrics":config.metrics.map(|a| snapshot.service_addresses.get("metrics").unwrap_or(&a).to_string())
            }
        });
        if private {
            value["managed_servers"] = admin::managed_servers(snapshot);
            value["service_groups"] = admin::service_groups(snapshot)["groups"].clone();
            value["config_path"] = json!(self.path.as_ref().map(|p| p.display().to_string()));
            value["writable"] = json!(self.path.is_some());
            value["routes"] = Value::Object(
                config
                    .routes
                    .iter()
                    .map(|(name, route)| {
                        (
                            name.clone(),
                            match route {
                                rift::config::Route::Direct(target) => json!(target),
                                rift::config::Route::Hostnames(patterns) => json!(patterns),
                            },
                        )
                    })
                    .collect(),
            );
            value["fallbacks"] = json!(config.fallbacks);
            value["limits"] = json!({"max_connections":config.limits.max_connections,"connect_timeout_ms":config.limits.connect_timeout.as_millis(),"buffer_size":config.limits.buffer_size});
        }
        value
    }
}

fn error(status: StatusCode, message: impl ToString) -> Response {
    (
        status,
        Json(json!({"error": message.to_string().chars().take(2048).collect::<String>()})),
    )
        .into_response()
}
impl IntoResponse for control::Error {
    fn into_response(self) -> Response {
        error(
            StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            self.message,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Web,
    Status,
    Metrics,
}
impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Status => "status",
            Self::Metrics => "metrics",
        }
    }
    fn address(self, config: &Config) -> Option<SocketAddr> {
        match self {
            Self::Web => config.web.as_ref().map(|c| c.listen),
            Self::Status => config.status.as_ref().map(|c| c.listen),
            Self::Metrics => config.metrics,
        }
    }
}
struct Server {
    configured: SocketAddr,
    actual: SocketAddr,
    stop: watch::Sender<bool>,
    task: JoinHandle<io::Result<()>>,
}
#[derive(Default)]
pub struct Services {
    servers: BTreeMap<Kind, Server>,
}
pub type Prepared = BTreeMap<Kind, TcpListener>;
impl Services {
    /// Reserve every new socket before changing configuration or stopping a server.
    pub async fn prepare(&self, config: &Config) -> io::Result<Prepared> {
        let mut sockets = BTreeMap::new();
        for kind in [Kind::Web, Kind::Status, Kind::Metrics] {
            if let Some(address) = kind.address(config) {
                if self
                    .servers
                    .get(&kind)
                    .is_some_and(|s| s.configured == address)
                {
                    continue;
                }
                let socket = TcpListener::bind(address).await.map_err(|e| {
                    io::Error::new(e.kind(), format!("{} ({address}): {e}", kind.name()))
                })?;
                sockets.insert(kind, socket);
            }
        }
        Ok(sockets)
    }
    pub fn addresses(
        &self,
        config: &Config,
        prepared: &Prepared,
    ) -> io::Result<BTreeMap<String, SocketAddr>> {
        let mut addresses = BTreeMap::new();
        for kind in [Kind::Web, Kind::Status, Kind::Metrics] {
            if kind.address(config).is_none() {
                continue;
            }
            let address = match prepared.get(&kind) {
                Some(s) => s.local_addr()?,
                None => self.servers[&kind].actual,
            };
            addresses.insert(kind.name().into(), address);
        }
        Ok(addresses)
    }
    pub fn commit(&mut self, config: &Config, mut prepared: Prepared, app: &App) {
        for kind in [Kind::Web, Kind::Status, Kind::Metrics] {
            let desired = kind.address(config);
            if self
                .servers
                .get(&kind)
                .is_some_and(|s| Some(s.configured) != desired)
            {
                let old = self.servers.remove(&kind).unwrap();
                old.stop.send_replace(true);
                // The bounded graceful shutdown task owns the old listener and
                // lets an HTTP save that disables itself finish its response.
            }
            if let Some(socket) = prepared.remove(&kind) {
                let actual = socket.local_addr().expect("bound socket address");
                let (stop, mut stopped) = watch::channel(false);
                let mut deadline_signal = stopped.clone();
                let router = router(kind, actual, app.clone());
                let task = tokio::spawn(async move {
                    let (force, forced) = watch::channel(false);
                    let socket = crate::web_transport::Listener::new(socket, forced);
                    let serve = axum::serve(socket, router).with_graceful_shutdown(async move {
                        let _ = stopped.changed().await;
                    });
                    tokio::select! {
                        result = serve => result,
                        _ = async move { let _ = deadline_signal.changed().await; tokio::time::sleep(Duration::from_secs(3)).await; } => { force.send_replace(true); Ok(()) }
                    }
                });
                eprintln!("rift: {} on {actual}", kind.name());
                self.servers.insert(
                    kind,
                    Server {
                        configured: desired.unwrap(),
                        actual,
                        stop,
                        task,
                    },
                );
            }
        }
    }
    pub fn failed(&self) -> Option<&'static str> {
        self.servers
            .iter()
            .find(|(_, s)| s.task.is_finished())
            .map(|(k, _)| k.name())
    }
    pub async fn shutdown(self) {
        for server in self.servers.values() {
            server.stop.send_replace(true);
        }
        for (_, server) in self.servers {
            let _ = server.task.await;
        }
    }
}

fn router(kind: Kind, address: SocketAddr, mut app: App) -> Router {
    app.binding = Some((kind, address));
    let routes = match kind {
        Kind::Web => Router::new()
            .route("/", get(admin_html))
            .route("/assets/app.css", get(css))
            .route("/assets/app.js", get(app_js))
            .route("/api", get(discovery))
            .route("/api/status", get(admin_status))
            .route("/api/servers", get(managed_servers))
            .route("/api/servers/{name}/start", post(start_server))
            .route("/api/servers/{name}/stop", post(stop_server))
            .route("/api/servers/{name}/logs", get(server_logs))
            .route("/api/servers/{name}/console", post(server_console))
            .route("/api/groups", get(service_groups))
            .route("/api/definitions", get(definitions))
            .route(
                "/api/definitions/{section}/{name}",
                axum::routing::put(edit_definition),
            )
            .route("/api/access", get(operator_access))
            .route("/api/deployments", get(deployments))
            .route("/api/deployments/{id}/rollback", post(rollback))
            .route("/api/audit", get(audit_records))
            .route("/api/groups/{name}/instances", post(create_instance))
            .route("/api/instances/{name}", delete(remove_instance))
            .route("/api/metrics", get(json_metrics))
            .route("/api/config", get(config_source).put(save_config))
            .route("/api/config/validate", post(validate_config))
            .route("/api/reload", post(reload))
            .route("/ext", any(extension))
            .route("/ext/{*path}", any(extension))
            .layer(middleware::from_fn_with_state(app.clone(), admin_guard)),
        Kind::Status => Router::new()
            .route("/", get(status_html))
            .route("/assets/app.css", get(css))
            .route("/assets/status.js", get(status_js))
            .route("/status", get(public_status))
            .route("/metrics", get(status_metrics))
            .layer(middleware::from_fn_with_state(app.clone(), status_guard)),
        Kind::Metrics => Router::new().route("/metrics", get(standalone_metrics)),
    };
    routes
        .fallback(|| async { error(StatusCode::NOT_FOUND, "endpoint not found") })
        .layer(DefaultBodyLimit::max(control::MAX_SOURCE * 6 + 1024))
        .layer(middleware::from_fn_with_state(app.clone(), boundary))
        .with_state(app)
}

async fn boundary(State(app): State<App>, request: Request, next: Next) -> Response {
    let Ok(_permit) = app.requests.try_acquire() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "HTTP request capacity exhausted",
        );
    };
    let mut deadline = Duration::from_secs(10);
    if request.method() == axum::http::Method::DELETE
        && let Some(name) = request.uri().path().strip_prefix("/api/instances/")
        && let Some(server) = app.snapshot().config.managed_servers.get(name)
    {
        deadline += server.stop_timeout;
    }
    let mut response = match timeout(deadline, next.run(request)).await {
        Ok(response) => response,
        Err(_) => error(
            StatusCode::REQUEST_TIMEOUT,
            "HTTP request deadline exceeded",
        ),
    };
    let headers = response.headers_mut();
    for (name, value) in [
        ("cache-control", "no-store"),
        ("connection", "close"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

#[derive(Clone)]
struct Access {
    name: String,
    permissions: Option<BTreeSet<String>>,
    groups: Option<BTreeSet<String>>,
}
impl Access {
    fn allows(&self, permission: &str) -> bool {
        self.permissions
            .as_ref()
            .is_none_or(|p| p.contains(permission))
    }
    fn group(&self, group: &str) -> bool {
        self.groups
            .as_ref()
            .is_none_or(|groups| groups.contains(group))
    }
    fn server(&self, snapshot: &Snapshot, name: &str) -> bool {
        self.groups.is_none()
            || snapshot
                .config
                .instances
                .get(name)
                .is_some_and(|instance| self.group(&instance.group))
    }
}
fn decode_name(name: &str) -> Option<String> {
    let mut result = Vec::new();
    let mut bytes = name.bytes();
    while let Some(byte) = bytes.next() {
        result.push(if byte == b'%' {
            let a = char::from(bytes.next()?).to_digit(16)?;
            let b = char::from(bytes.next()?).to_digit(16)?;
            (a * 16 + b) as u8
        } else {
            byte
        });
    }
    String::from_utf8(result).ok()
}
async fn record_audit(
    app: &App,
    access: &Access,
    action: &str,
    target: &str,
    outcome: &str,
    status: u16,
) -> io::Result<()> {
    let store = app.workflows.clone();
    let actor = access.name.clone();
    let (action, target, outcome) = (action.to_owned(), target.to_owned(), outcome.to_owned());
    tokio::task::spawn_blocking(move || store.audit(&actor, &action, &target, &outcome, status))
        .await
        .map_err(io::Error::other)?
}
async fn admin_guard(State(app): State<App>, mut request: Request, next: Next) -> Response {
    let snapshot = app.snapshot();
    // A socket retained only to finish an earlier response must never acquire
    // the replacement listener's authentication policy (for example when a
    // public authenticated listener moves to a tokenless loopback address).
    if !app.active_binding(&snapshot) {
        return error(StatusCode::NOT_FOUND, "listener retired");
    }
    let Some(settings) = &snapshot.config.web else {
        return error(StatusCode::NOT_FOUND, "web service disabled");
    };
    let path = request.uri().path().to_owned();
    let api = path == "/api" || path.starts_with("/api/");
    let extension = path == "/ext" || path.starts_with("/ext/");
    if (api && !settings.api) || (!api && !extension && !settings.ui) {
        return error(StatusCode::NOT_FOUND, "interface disabled");
    }
    if api || extension {
        let headers = request.headers();
        let host = headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if let Some(origin) = headers.get(header::ORIGIN) {
            let valid = origin
                .to_str()
                .ok()
                .and_then(|s| s.parse::<Uri>().ok())
                .is_some_and(|uri| {
                    matches!(uri.scheme_str(), Some("http" | "https"))
                        && uri
                            .authority()
                            .is_some_and(|a| a.as_str().eq_ignore_ascii_case(host))
                });
            if !valid {
                return error(
                    StatusCode::FORBIDDEN,
                    "cross-origin requests are not allowed",
                );
            }
        }
        if headers
            .get("sec-fetch-site")
            .is_some_and(|h| h == "cross-site")
        {
            return error(StatusCode::FORBIDDEN, "cross-site requests are not allowed");
        }
        let supplied = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("");
        let mut access = None;
        if let Some(token) = &settings.token
            && equal_token(token.as_bytes(), supplied.as_bytes())
        {
            access = Some(Access {
                name: "admin".into(),
                permissions: None,
                groups: None,
            });
        }
        for (name, operator) in &settings.operators {
            if equal_token(operator.token.as_bytes(), supplied.as_bytes()) {
                access = Some(Access {
                    name: name.clone(),
                    permissions: Some(operator.permissions.clone()),
                    groups: operator.groups.clone(),
                });
            }
        }
        if settings.token.is_some() || !settings.operators.is_empty() {
            if access.is_none() {
                drop(snapshot);
                let anonymous = Access {
                    name: "anonymous".into(),
                    permissions: Some(BTreeSet::new()),
                    groups: None,
                };
                let _ = record_audit(&app, &anonymous, "authentication", "", "denied", 401).await;
                let mut response =
                    error(StatusCode::UNAUTHORIZED, "a valid bearer token is required");
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                return response;
            }
        } else {
            let local_host = host
                .parse::<axum::http::uri::Authority>()
                .ok()
                .is_some_and(|a| {
                    a.host().eq_ignore_ascii_case("localhost")
                        || a.host()
                            .trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                });
            if !local_host {
                return error(
                    StatusCode::FORBIDDEN,
                    "tokenless administration requires a loopback Host header",
                );
            }
            access = Some(Access {
                name: "local".into(),
                permissions: None,
                groups: None,
            });
        }
        let access = access.unwrap();
        let segments: Vec<_> = path.trim_start_matches('/').split('/').collect();
        let mutation = request.method() != axum::http::Method::GET
            && request.method() != axum::http::Method::HEAD;
        let permission = if extension {
            "extensions"
        } else {
            match segments.as_slice() {
                ["api", "access"] | ["api"] => "",
                ["api", "servers", _, "logs"] => "logs",
                ["api", "servers", _, "console"] => "console",
                ["api", "servers", _, _]
                | ["api", "groups", _, "instances"]
                | ["api", "instances", _] => "servers",
                ["api", "definitions", ..] | ["api", "config", ..] | ["api", "reload"] => "config",
                ["api", "deployments", ..] => "deploy",
                ["api", "audit"] => "audit",
                _ => "read",
            }
        };
        let (in_scope, target) = match segments.as_slice() {
            ["api", "servers" | "instances", name, ..] => {
                let name = decode_name(name).unwrap_or_default();
                (
                    access.server(&snapshot, &name),
                    if snapshot.config.managed_servers.contains_key(&name) {
                        name
                    } else {
                        String::new()
                    },
                )
            }
            ["api", "groups", name, ..] => {
                let name = decode_name(name).unwrap_or_default();
                (
                    access.group(&name),
                    if snapshot.config.service_groups.contains_key(&name) {
                        name
                    } else {
                        String::new()
                    },
                )
            }
            _ => (true, String::new()),
        };
        let authorized = (permission.is_empty() || access.allows(permission)) && in_scope;
        let operation = match segments.as_slice() {
            ["api", "servers", _, "start"] => "start",
            ["api", "servers", _, "stop"] => "stop",
            ["api", "servers", _, "console"] => "console",
            ["api", "groups", _, "instances"] => "create_instance",
            ["api", "instances", _] => "remove_instance",
            ["api", "definitions", "templates", _] => "edit_template",
            ["api", "definitions", "service_groups", _] => "edit_group",
            ["api", "deployments", _, "rollback"] => "rollback",
            ["api", "config"] => "configuration",
            ["api", "config", "validate"] => "validate",
            ["api", "reload"] => "reload",
            _ => permission,
        };
        let action = format!("{} {}", request.method(), operation);
        drop(snapshot);
        if !authorized {
            let _ = record_audit(&app, &access, &action, &target, "denied", 403).await;
            return error(
                StatusCode::FORBIDDEN,
                "operator permission or group scope denied",
            );
        }
        if mutation
            && record_audit(&app, &access, &action, &target, "requested", 0)
                .await
                .is_err()
        {
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "audit record could not be written; operation was not submitted",
            );
        }
        request.extensions_mut().insert(access.clone());
        let response = next.run(request).await;
        if mutation {
            let status = response.status().as_u16();
            let outcome = if response.status().is_success() {
                "accepted"
            } else {
                "rejected"
            };
            if let Err(error) = record_audit(&app, &access, &action, &target, outcome, status).await
            {
                eprintln!("rift: operator result audit failed: {error}");
            }
        }
        return response;
    }
    drop(snapshot);
    next.run(request).await
}
fn equal_token(expected: &[u8], actual: &[u8]) -> bool {
    let mut difference = expected.len() ^ actual.len();
    for (i, byte) in expected.iter().enumerate() {
        difference |= usize::from(*byte ^ actual.get(i).copied().unwrap_or(0));
    }
    difference == 0
}
async fn status_guard(State(app): State<App>, request: Request, next: Next) -> Response {
    let snapshot = app.snapshot();
    if !app.active_binding(&snapshot) {
        return error(StatusCode::NOT_FOUND, "listener retired");
    }
    let Some(settings) = &snapshot.config.status else {
        return error(StatusCode::NOT_FOUND, "status service disabled");
    };
    let path = request.uri().path();
    if (path == "/metrics" && !settings.metrics)
        || ((path == "/" || path.starts_with("/assets/")) && !settings.ui)
    {
        return error(StatusCode::NOT_FOUND, "endpoint disabled");
    }
    drop(snapshot);
    next.run(request).await
}

async fn admin_html() -> Html<&'static str> {
    Html(include_str!("web_assets/index.html"))
}
async fn status_html() -> Html<&'static str> {
    Html(include_str!("web_assets/status.html"))
}
async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("web_assets/app.css"),
    )
}
async fn app_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("web_assets/app.js"),
    )
}
async fn status_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("web_assets/status.js"),
    )
}
fn visible_servers(snapshot: &Snapshot, access: &Access) -> Value {
    let mut value = admin::managed_servers(snapshot);
    value
        .as_array_mut()
        .unwrap()
        .retain(|server| access.server(snapshot, server["name"].as_str().unwrap()));
    value
}
fn visible_groups(snapshot: &Snapshot, access: &Access) -> Value {
    let mut value = admin::service_groups(snapshot);
    value["groups"]
        .as_array_mut()
        .unwrap()
        .retain(|group| access.group(group["name"].as_str().unwrap()));
    value
}
async fn admin_status(State(app): State<App>, Extension(access): Extension<Access>) -> Json<Value> {
    let snapshot = app.snapshot();
    let mut value = app.status_snapshot(&snapshot, true);
    value["managed_servers"] = visible_servers(&snapshot, &access);
    value["service_groups"] = visible_groups(&snapshot, &access)["groups"].clone();
    if access.groups.is_some() {
        let names: BTreeSet<_> = snapshot
            .config
            .instances
            .iter()
            .filter(|(_, instance)| access.group(&instance.group))
            .map(|(name, _)| name.as_str())
            .collect();
        value["backends"]
            .as_array_mut()
            .unwrap()
            .retain(|backend| names.contains(backend["name"].as_str().unwrap()));
        value.as_object_mut().unwrap().remove("routes");
        value.as_object_mut().unwrap().remove("fallbacks");
    }
    if !access.allows("config") {
        value.as_object_mut().unwrap().remove("config_path");
    }
    Json(value)
}
async fn managed_servers(
    State(app): State<App>,
    Extension(access): Extension<Access>,
) -> Json<Value> {
    Json(json!({"servers":visible_servers(&app.snapshot(), &access)}))
}
async fn service_groups(
    State(app): State<App>,
    Extension(access): Extension<Access>,
) -> Json<Value> {
    Json(visible_groups(&app.snapshot(), &access))
}

async fn operator_access(Extension(access): Extension<Access>) -> Json<Value> {
    Json(json!({"name":access.name,"permissions":access.permissions,"groups":access.groups}))
}
async fn server_logs(
    State(app): State<App>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    let cursor = match query.as_deref() {
        None | Some("") => None,
        Some(query) => match query
            .strip_prefix("cursor=")
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(value) => Some(value),
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "expected cursor=<nonnegative integer>",
                );
            }
        },
    };
    match app.snapshot().managed.logs(&name, cursor).await {
        Ok(value) => Json(value).into_response(),
        Err(e) => managed_error(e),
    }
}
fn managed_error(failure: io::Error) -> Response {
    error(
        match failure.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
            io::ErrorKind::NotConnected | io::ErrorKind::ResourceBusy => StatusCode::CONFLICT,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        },
        failure,
    )
}
async fn server_console(
    State(app): State<App>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let value = match payload(body, &["command"]) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let Some(command) = value["command"].as_str() else {
        return error(StatusCode::BAD_REQUEST, "command must be a string");
    };
    match app.snapshot().managed.console(&name, command.to_owned()).await {
        Ok(()) => Json(json!({"sent":true, "message":"Command written to the server console; check logs for its result"})).into_response(),
        Err(e) => managed_error(e),
    }
}
async fn deployments(State(app): State<App>) -> Json<Value> {
    let mut value = app.workflows.deployments();
    value["revision"] = json!(app.snapshot().revision);
    Json(value)
}
async fn audit_records(
    State(app): State<App>,
    Extension(access): Extension<Access>,
) -> Json<Value> {
    let mut value = app.workflows.audits();
    if access.groups.is_some() {
        value["records"]
            .as_array_mut()
            .unwrap()
            .retain(|record| record["actor"] == access.name);
    }
    Json(value)
}
async fn rollback(
    State(app): State<App>,
    Extension(access): Extension<Access>,
    Path(id): Path<u64>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let value = match payload(body, &["revision"]) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let Some(revision) = value["revision"].as_str() else {
        return error(StatusCode::BAD_REQUEST, "revision is required");
    };
    let Some(source) = app.workflows.source(id) else {
        return error(StatusCode::NOT_FOUND, "deployment is no longer retained");
    };
    submit(
        app,
        control::Operation::Rollback {
            source,
            revision: revision.to_owned(),
        },
        access.name,
    )
    .await
}
fn definition_values(config: &Config) -> Value {
    let server = |s: &rift::config::ManagedServer| json!({"directory":s.directory,"command":s.command,"autostart":s.autostart,"start_on_connect":s.start_on_connect,"idle_timeout_ms":s.idle_timeout.map_or(0, |d| d.as_millis()),"start_timeout_ms":s.start_timeout.as_millis(),"stop_timeout_ms":s.stop_timeout.as_millis(),"restart_delay_ms":s.restart_delay.as_millis(),"restart_retries":s.restart_retries});
    let groups: BTreeMap<_,_> = config.service_groups.iter().map(|(name, group)| {
        let mut value = server(&group.server);
        value["port_range"] = json!([group.port_start,group.port_end]); value["storage"] = json!(group.storage.as_str());
        if let Some(template) = &group.template { value["template"] = json!(template); }
        if let Some(s) = &group.scaling { value["scaling"] = json!({"min_instances":s.min_instances,"max_instances":s.max_instances,"spare_instances":s.spare_instances,"capacity_per_instance":s.capacity_per_instance,"target_occupancy_percent":s.target_occupancy_percent,"queue_threshold":s.queue_threshold,"cooldown_ms":s.cooldown.as_millis()}); }
        (name, value)
    }).collect();
    let templates: BTreeMap<_, _> = config
        .templates
        .iter()
        .map(|(name, t)| {
            let mut value = json!({"server_jar":t.server_jar,"plugins":t.plugins});
            if let Some(v) = &t.configs {
                value["configs"] = json!(v);
            }
            if let Some(v) = &t.map {
                value["map"] = json!(v);
            }
            (name, value)
        })
        .collect();
    json!({"service_groups":groups,"templates":templates})
}
async fn definitions(State(app): State<App>) -> Json<Value> {
    let snapshot = app.snapshot();
    let mut value = definition_values(&snapshot.config);
    value["revision"] = json!(snapshot.revision);
    Json(value)
}
async fn edit_definition(
    State(app): State<App>,
    Extension(access): Extension<Access>,
    Path((section, name)): Path<(String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let value = match payload(body, &["revision", "definition"]) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let Some(revision) = value["revision"].as_str() else {
        return error(StatusCode::BAD_REQUEST, "revision is required");
    };
    let Some(definition) = value.get("definition") else {
        return error(StatusCode::BAD_REQUEST, "definition is required");
    };
    let snapshot = app.snapshot();
    if revision != snapshot.revision {
        return error(
            StatusCode::CONFLICT,
            "configuration changed; refresh definitions before editing",
        );
    }
    let Some(source) = snapshot.source.as_deref() else {
        return error(StatusCode::CONFLICT, "no configuration source available");
    };
    let source = match operator::edited_source(source, &section, &name, definition) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    submit(
        app,
        control::Operation::Save {
            source,
            revision: revision.to_owned(),
        },
        access.name,
    )
    .await
}
async fn create_instance(
    State(app): State<App>,
    Path(group): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    if let Err(error) = payload(body, &[]) {
        return error.into_response();
    }
    instance_operation(
        app,
        control::Operation::CreateInstance { group },
        StatusCode::CREATED,
    )
    .await
}
async fn remove_instance(
    State(app): State<App>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    if let Err(error) = payload(body, &[]) {
        return error.into_response();
    }
    instance_operation(
        app,
        control::Operation::RemoveInstance { name },
        StatusCode::OK,
    )
    .await
}
async fn instance_operation(
    app: App,
    operation: control::Operation,
    status: StatusCode,
) -> Response {
    let (reply, result) = oneshot::channel();
    if app
        .commands
        .try_send(control::Command {
            operation,
            reply,
            actor: "operator".into(),
        })
        .is_err()
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "instance operation queue unavailable or busy",
        );
    }
    match result.await {
        Ok(Ok(value)) => match serde_json::from_str::<Value>(&value) {
            Ok(value) => (status, Json(value)).into_response(),
            Err(_) => error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid instance operation response",
            ),
        },
        Ok(Err(failure)) => failure.into_response(),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "instance operation service stopped",
        ),
    }
}
async fn start_server(
    State(app): State<App>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    server_operation(app, name, body, true)
}
async fn stop_server(
    State(app): State<App>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    server_operation(app, name, body, false)
}
fn server_operation(
    app: App,
    name: String,
    body: Result<Json<Value>, JsonRejection>,
    start: bool,
) -> Response {
    if let Err(error) = payload(body, &[]) {
        return error.into_response();
    }
    let snapshot = app.snapshot();
    let result = admin::managed_backend(&snapshot, &name).and_then(|()| {
        if start {
            snapshot.managed.request_start(&name)
        } else {
            snapshot.managed.request_stop(&name)
        }
    });
    match result {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "backend":name,"operation":if start {"start"} else {"stop"},
                "accepted":true,"message":"Request accepted; poll /api/servers for completion",
            })),
        )
            .into_response(),
        Err(failure) => error(
            match failure.kind() {
                io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
                io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
                io::ErrorKind::WouldBlock
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::NotConnected => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::CONFLICT,
            },
            failure,
        ),
    }
}
async fn public_status(State(app): State<App>) -> Json<Value> {
    Json(app.status(false))
}
async fn json_metrics(State(app): State<App>) -> Json<Value> {
    Json(app.metrics.values())
}
async fn prometheus(State(app): State<App>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        app.metrics.render(&app.snapshot()),
    )
}
async fn status_metrics(state: State<App>) -> impl IntoResponse {
    prometheus(state).await
}
async fn standalone_metrics(state: State<App>) -> Response {
    let snapshot = state.snapshot();
    if !state.active_binding(&snapshot) || snapshot.config.metrics.is_none() {
        return error(StatusCode::NOT_FOUND, "metrics service disabled");
    }
    prometheus(state).await.into_response()
}
async fn discovery() -> Json<Value> {
    Json(
        json!({"version":1,"endpoints":["GET /api/access","GET /api/status","GET /api/servers","POST /api/servers/{name}/start","POST /api/servers/{name}/stop","GET /api/servers/{name}/logs","POST /api/servers/{name}/console","GET /api/groups","POST /api/groups/{name}/instances","DELETE /api/instances/{name}","GET /api/definitions","PUT /api/definitions/{section}/{name}","GET /api/deployments","POST /api/deployments/{id}/rollback","GET /api/audit","GET /api/metrics","GET /api/config","POST /api/config/validate","PUT /api/config","POST /api/reload"],"extensions":"/ext/* -> on_http(request)","source_limit_bytes":control::MAX_SOURCE}),
    )
}
async fn config_source(State(app): State<App>) -> Json<Value> {
    let snapshot = app.snapshot();
    Json(
        json!({"source":snapshot.source.as_deref(),"revision":snapshot.revision,"writable":app.path.is_some()}),
    )
}
fn payload(
    body: Result<Json<Value>, JsonRejection>,
    allowed: &[&str],
) -> Result<Value, control::Error> {
    let Json(value) = body.map_err(|e| control::Error {
        status: if e.status() == StatusCode::UNSUPPORTED_MEDIA_TYPE
            || e.status() == StatusCode::PAYLOAD_TOO_LARGE
        {
            e.status().as_u16()
        } else {
            400
        },
        message: e.body_text(),
    })?;
    let Some(object) = value.as_object() else {
        return Err(control::Error::invalid("expected a JSON object"));
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(control::Error::invalid("unknown request field"));
    }
    Ok(value)
}
fn source_field(value: &Value) -> Result<String, control::Error> {
    let Some(source) = value.get("source").and_then(Value::as_str) else {
        return Err(control::Error::invalid("source must be a Lua string"));
    };
    if source.len() > control::MAX_SOURCE {
        return Err(control::Error {
            status: 413,
            message: "source exceeds 256 KiB limit".into(),
        });
    }
    Ok(source.to_owned())
}
async fn validate_config(
    State(app): State<App>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let value = match payload(body, &["source"]) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let source = match source_field(&value) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let Ok(permit) = app.validations.clone().try_acquire_owned() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "configuration validation busy",
        );
    };
    let snapshot = app.snapshot();
    let path = app.path.clone();
    let previous_config = snapshot.config.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        match path {
            Some(path) => Config::from_lua_at_with_instances(&source, &path, &previous_config),
            None => Config::from_lua_with_instances(&source, "HTTP validation", &previous_config),
        }
    })
    .await
    {
        Ok(Ok(config)) => {
            if !control::live_compatible(&config, &snapshot.config)
                || !app.record_settings_compatible(&config)
            {
                return error(
                    StatusCode::BAD_REQUEST,
                    "listener, admin, messaging, existing managed process definitions/addresses, instance storage and extension queue changes require a restart or removal of affected instances",
                );
            }
            Json(json!({"valid":true,"message":"Configuration valid; socket availability is checked when applying."})).into_response()
        }
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, e),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
async fn submit(app: App, operation: control::Operation, actor: String) -> Response {
    let (reply, result) = oneshot::channel();
    if app
        .commands
        .try_send(control::Command {
            operation,
            reply,
            actor,
        })
        .is_err()
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "configuration queue unavailable or busy",
        );
    }
    match result.await {
        Ok(Ok(revision)) => {
            Json(json!({"revision":revision,"message":"Configuration applied"})).into_response()
        }
        Ok(Err(e)) => e.into_response(),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "configuration service stopped",
        ),
    }
}
async fn save_config(
    State(app): State<App>,
    Extension(operator): Extension<Access>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let value = match payload(body, &["source", "revision"]) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let source = match source_field(&value) {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let Some(revision) = value.get("revision").and_then(Value::as_str) else {
        return error(
            StatusCode::BAD_REQUEST,
            "revision is required; fetch /api/config first",
        );
    };
    submit(
        app,
        control::Operation::Save {
            source,
            revision: revision.to_owned(),
        },
        operator.name,
    )
    .await
}
async fn reload(
    State(app): State<App>,
    Extension(operator): Extension<Access>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    if let Err(e) = payload(body, &[]) {
        return e.into_response();
    }
    submit(app, control::Operation::Reload, operator.name).await
}
async fn extension(State(app): State<App>, request: Request) -> Response {
    let snapshot = app.snapshot();
    let Some(script) = snapshot.config.on_http.clone() else {
        return error(StatusCode::NOT_FOUND, "no on_http handler configured");
    };
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, control::MAX_SOURCE).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "HTTP body exceeds 256 KiB limit",
            );
        }
    };
    let body = match String::from_utf8(bytes.to_vec()) {
        Ok(body) => body,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Lua HTTP bodies must be UTF-8"),
    };
    let headers = parts
        .headers
        .iter()
        .filter(|(name, _)| {
            !matches!(
                name.as_str(),
                "authorization" | "cookie" | "proxy-authorization"
            )
        })
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.to_string(), v.to_owned()))
        })
        .collect();
    let request = HttpRequest {
        method: parts.method.to_string(),
        path: parts.uri.path().into(),
        query: parts.uri.query().unwrap_or("").into(),
        body,
        headers,
        context: app.status_snapshot(&snapshot, false),
    };
    match script
        .execute_with_messaging(request, snapshot.messaging.clone())
        .await
    {
        Ok(Some(result)) => (
            StatusCode::from_u16(result.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            [(header::CONTENT_TYPE, result.content_type)],
            result.body,
        )
            .into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "Lua endpoint not found"),
        Err(HttpError::Busy) => error(StatusCode::SERVICE_UNAVAILABLE, "Lua HTTP workers busy"),
        Err(HttpError::TimedOut) => {
            error(StatusCode::GATEWAY_TIMEOUT, "Lua HTTP deadline exceeded")
        }
        Err(HttpError::Script(message)) => error(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}
