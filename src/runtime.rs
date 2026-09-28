use crate::{
    admission::Admission,
    events::Connection,
    health::{self, Health},
    metrics::{Active, Metered, Metrics, Player},
    status,
};
use rift::{
    config::{Config, Route},
    hooks::{ConnectionInfo, RouteDecision, Router},
    protocol::{Codec, NextState, status_response},
    routing::Routes,
    session::{Session, SessionEvent},
};
use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::watch,
    task::{JoinHandle, JoinSet},
    time::{Instant as Deadline, sleep, timeout, timeout_at},
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

enum Policy {
    Direct(String),
    Hostnames(Routes<String>),
}

pub struct Snapshot {
    pub config: Config,
    pub source: Option<Arc<str>>,
    pub revision: String,
    pub listener_addresses: BTreeMap<String, SocketAddr>,
    pub service_addresses: BTreeMap<String, SocketAddr>,
    pub addresses: Arc<Vec<SocketAddr>>,
    policies: BTreeMap<String, Policy>,
    router: Router,
    pub health: Health,
    cache: status::Cache,
}

impl Snapshot {
    pub fn new(config: Config, previous: Option<&Self>) -> io::Result<Self> {
        config.validate()?;
        let mut policies = BTreeMap::new();
        for (listener, route) in &config.routes {
            let policy = match route {
                Route::Direct(name) => Policy::Direct(name.clone()),
                Route::Hostnames(patterns) => {
                    let mut routes = Routes::default();
                    for (pattern, name) in patterns {
                        routes.add_pattern(pattern, name.clone())?;
                    }
                    Policy::Hostnames(routes)
                }
            };
            policies.insert(listener.clone(), policy);
        }
        let router = previous.map_or_else(
            || Router::new(&config),
            |old| old.router.reconfigured(&config),
        );
        let health = Health::new(&config);
        Ok(Self {
            source: None,
            revision: "runtime".into(),
            listener_addresses: config.listeners.clone(),
            service_addresses: BTreeMap::new(),
            addresses: Arc::new(config.listeners.values().copied().collect()),
            config,
            policies,
            router,
            health,
            cache: status::Cache::default(),
        })
    }
}

pub async fn handle(
    client: &mut TcpStream,
    listener: &str,
    snapshot: Arc<Snapshot>,
    addresses: &[SocketAddr],
    metrics: Arc<Metrics>,
    event: &mut Connection,
) -> io::Result<(u64, u64)> {
    event.stage = "client_setup";
    client.set_nodelay(true)?;
    let connection = ConnectionInfo {
        listener: listener.to_owned(),
        peer_addr: client.peer_addr()?,
        local_addr: client.local_addr()?,
        default_backend: snapshot.router.default_backend(listener).map(str::to_owned),
    };
    event.stage = "on_route";
    let selected = match snapshot.router.route(connection).await {
        Ok(RouteDecision::Default) => None,
        Ok(RouteDecision::Backend(name)) => Some(name),
        Ok(RouteDecision::Reject { reason }) => {
            metrics.route_rejected.inc();
            event.reject(
                "route_rejected",
                reason.as_deref().unwrap_or("rejected by on_route"),
            );
            return Ok((0, 0));
        }
        Err(error) => {
            metrics.route_rejected.inc();
            event.route_error(&error);
            return Err(io::Error::other(format!("on_route: {error}")));
        }
    };
    event.stage = "handshake";
    event.failure = "handshake_error";
    let mut session: Session<_, TcpStream> = timeout(
        HANDSHAKE_TIMEOUT,
        Session::accept(Metered::new(client, metrics.clone())),
    )
    .await??;
    session.set_read_chunk_size(snapshot.config.limits.buffer_size);
    let is_status = session.handshake.next_state == NextState::Status;
    event.stage = "route";
    event.failure = "no_route";
    let primary = match selected {
        Some(name) => Ok(name),
        None => match &snapshot.policies[listener] {
            Policy::Direct(name) => Ok(name.clone()),
            Policy::Hostnames(routes) => routes.select(&session.handshake.hostname()).cloned(),
        },
    };
    let primary = match primary {
        Ok(primary) => primary,
        Err(error) => {
            if is_status {
                timeout(HANDSHAKE_TIMEOUT, session.status_request()).await??;
                let response = status_response(
                    session.handshake.protocol,
                    0,
                    snapshot.config.limits.max_connections,
                    "No server is configured for this hostname.",
                );
                timeout(HANDSHAKE_TIMEOUT, session.respond_status(&response)).await??;
            } else {
                let _ = timeout(
                    HANDSHAKE_TIMEOUT,
                    session.disconnect("No server is configured for this hostname."),
                )
                .await;
            }
            return Err(error);
        }
    };
    event.backend = Some(primary.clone());
    event.backend_address = Some(snapshot.config.backends[&primary].address().to_owned());
    event.failure = "io_error";
    if is_status {
        event.stage = "status_request";
        timeout(HANDSHAKE_TIMEOUT, session.status_request()).await??;
        let mut response = status_response(
            session.handshake.protocol,
            metrics.players.get(),
            snapshot.config.limits.max_connections,
            "Rift",
        );
        if let Some(settings) = snapshot.config.status_cache {
            let packet = Codec::default().encode(&session.handshake.packet())?;
            let deadline = Deadline::now() + snapshot.config.limits.connect_timeout;
            event.stage = "status_cache_wait";
            let cached = async {
                if let Some(response) = snapshot.cache.get(listener, &primary, &packet) {
                    metrics.cache_hits.inc();
                    return Ok::<_, io::Error>(response);
                }
                let _fill = match snapshot.cache.fill_lock(
                    listener,
                    &primary,
                    &packet,
                    settings.max_entries,
                ) {
                    Some(lock) => Some(timeout_at(deadline, lock.lock_owned()).await?),
                    None => None,
                };
                if let Some(response) = snapshot.cache.get(listener, &primary, &packet) {
                    metrics.cache_hits.inc();
                    return Ok(response);
                }
                metrics.cache_misses.inc();
                let mut upstream = health::connect(
                    &snapshot.config,
                    &snapshot.health,
                    &primary,
                    addresses,
                    &metrics,
                    deadline,
                    event,
                )
                .await?;
                event.stage = "backend_setup";
                event.failure = "io_error";
                upstream.set_nodelay(true)?;
                let response = timeout_at(deadline, async {
                    event.stage = "handshake_write";
                    upstream.write_all(&packet).await?;
                    event.stage = "status_upstream";
                    upstream.write_all(&[1, 0]).await?;
                    status::frame(&mut upstream, settings.max_response_bytes).await
                })
                .await??;
                status::validate_response(&response)?;
                snapshot
                    .cache
                    .insert(listener, &primary, &packet, response.clone(), settings);
                Ok(Arc::new(response))
            }
            .await;
            match cached {
                Ok(cached) => response = status::response_packet(&cached)?,
                Err(error) if error.kind() != io::ErrorKind::InvalidData => {
                    response = status_response(
                        session.handshake.protocol,
                        0,
                        snapshot.config.limits.max_connections,
                        "The server is currently unavailable. Please try again later.",
                    );
                    // Connection failures have per-attempt diagnostics. Reads and
                    // timeouts are also reported, while the client gets a pong.
                    if !matches!(event.stage, "connect" | "dns") {
                        event.emit("backend_status_failed", &error, Some(error.kind()));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        event.stage = "status_response";
        timeout(HANDSHAKE_TIMEOUT, session.respond_status(&response)).await??;
    } else {
        if let Err(error) = session.version() {
            let _ = timeout(HANDSHAKE_TIMEOUT, session.disconnect(&error.to_string())).await;
            return Err(error);
        }
        let deadline = Deadline::now() + snapshot.config.limits.connect_timeout;
        let upstream = match health::connect(
            &snapshot.config,
            &snapshot.health,
            &primary,
            addresses,
            &metrics,
            deadline,
            event,
        )
        .await
        {
            Ok(upstream) => upstream,
            Err(error) => {
                let _ = timeout(
                    HANDSHAKE_TIMEOUT,
                    session
                        .disconnect("The server is currently unavailable. Please try again later."),
                )
                .await;
                return Err(error);
            }
        };
        event.stage = "backend_setup";
        event.failure = "io_error";
        upstream.set_nodelay(true)?;
        event.stage = "handshake_write";
        if let Err(error) =
            async { timeout_at(deadline, session.connect_backend(upstream)).await? }.await
        {
            let _ = timeout(
                HANDSHAKE_TIMEOUT,
                session.disconnect("Unable to connect to the server."),
            )
            .await;
            return Err(error);
        }
        drop(snapshot);
        event.stage = "session";
        // Login/configuration deadlines also bound stalled encryption and login
        // exchanges. Play itself has no idle timeout.
        let mut phase_deadline = Deadline::now() + Duration::from_secs(30);
        let mut client_closed = false;
        let mut player = None;
        loop {
            let playing =
                !client_closed && session.client.state.settled(rift::protocol::State::Play);
            let result = if playing {
                session.forward().await
            } else {
                match timeout_at(phase_deadline, session.forward()).await {
                    Ok(result) => result,
                    Err(error) => Err(error.into()),
                }
            };
            match result {
                Ok(SessionEvent::Packet) => {
                    if player.is_none() && session.client.state.settled(rift::protocol::State::Play)
                    {
                        player = Some(Player::new(metrics.clone()));
                    }
                    if playing && !session.client.state.settled(rift::protocol::State::Play) {
                        phase_deadline = Deadline::now() + Duration::from_secs(30);
                    }
                }
                Ok(SessionEvent::Disconnected) => break,
                Ok(SessionEvent::ClientClosed) => {
                    client_closed = true;
                    phase_deadline = Deadline::now() + Duration::from_secs(30);
                }
                Ok(SessionEvent::BackendClosed) => {
                    if client_closed {
                        break;
                    }
                    timeout(
                        HANDSHAKE_TIMEOUT,
                        session.disconnect("The server connection was lost. Please reconnect."),
                    )
                    .await??;
                    break;
                }
                Err(error) => {
                    let reason = if error.kind() == io::ErrorKind::Unsupported {
                        error.to_string()
                    } else {
                        "The server connection failed. Please reconnect.".to_owned()
                    };
                    let _ = timeout(HANDSHAKE_TIMEOUT, session.disconnect(&reason)).await;
                    return Err(error);
                }
            }
        }
    }
    Ok((session.client.io.sent, session.client.io.received))
}

pub(crate) async fn accept(
    listener: TcpListener,
    name: String,
    current: watch::Receiver<Arc<Snapshot>>,
    mut stop: watch::Receiver<bool>,
    metrics: Arc<Metrics>,
    admission: Arc<Mutex<Admission>>,
) {
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            result = sessions.join_next(), if !sessions.is_empty() => {
                if let Some(Err(error)) = result { eprintln!("rift: connection task: {error}"); }
            }
            accepted = listener.accept() => {
                let (client, peer) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => {
                        eprintln!("rift: accept: {error}");
                        sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let mut event = Connection::new(&name, peer);
                metrics.accepted.inc();
                let snapshot = current.borrow().clone();
                if !admission.lock().unwrap().allow(peer.ip(), snapshot.config.rate_limit, Instant::now()) {
                    metrics.rate_rejected.inc();
                    event.reject("rate_limited", "connection rate limit exceeded");
                    continue;
                }
                let Some(active) = Active::new(metrics.clone(), snapshot.config.limits.max_connections) else {
                    metrics.capacity_rejected.inc();
                    event.reject("capacity_exhausted", "connection capacity exhausted");
                    continue;
                };
                let name = name.clone();
                let addresses = snapshot.addresses.clone();
                let metrics = metrics.clone();
                sessions.spawn(async move {
                    let mut client = client;
                    let result = handle(&mut client, &name, snapshot, &addresses, metrics.clone(), &mut event).await;
                    // Make capacity available before the peer can observe EOF and
                    // reconnect. The handler borrows the socket until it returns.
                    drop(active);
                    drop(client);
                    if let Err(error) = result {
                        metrics.errors.inc();
                        event.failed(&error);
                    }
                });
            }
        }
    }
    // Close listening sockets immediately, then keep all accepted sessions alive.
    drop(listener);
    while sessions.join_next().await.is_some() {}
}

fn health_worker(
    snapshot: Arc<Snapshot>,
    addresses: Arc<Vec<SocketAddr>>,
    metrics: Arc<Metrics>,
) -> Option<JoinHandle<()>> {
    let settings = snapshot.config.health_check?;
    Some(tokio::spawn(async move {
        loop {
            let mut probes = JoinSet::new();
            for (name, backend) in &snapshot.config.backends {
                if probes.len() >= 16 {
                    let _ = probes.join_next().await;
                }
                let name = name.clone();
                let backend = backend.clone();
                let snapshot = snapshot.clone();
                let addresses = addresses.clone();
                let metrics = metrics.clone();
                probes.spawn(async move {
                    let success = matches!(
                        timeout(settings.timeout, backend.connect(&addresses)).await,
                        Ok(Ok(_))
                    );
                    snapshot.health.record(&name, success);
                    metrics.health_checks.inc();
                    if !success {
                        metrics.health_failures.inc();
                    }
                });
            }
            while probes.join_next().await.is_some() {}
            // Wait after completion: probes never overlap or queue missed ticks.
            sleep(settings.interval).await;
        }
    }))
}

/// Every accepted configuration follows the same transaction, regardless of
/// whether it came from a signal, the website, or an API client.
async fn reconfigure(
    operation: crate::control::Operation,
    app: &crate::web::App,
    snapshot: &Arc<Snapshot>,
    services: &crate::web::Services,
) -> Result<(Arc<Snapshot>, crate::web::Prepared), crate::control::Error> {
    use crate::control::{self, Error};
    let path = app
        .path
        .clone()
        .ok_or_else(|| Error::conflict("no configuration file selected"))?;
    let active = snapshot
        .source
        .clone()
        .ok_or_else(|| Error::conflict("no configuration source available"))?;
    let path_for_prepare = path.clone();
    let candidate = tokio::task::spawn_blocking(move || {
        control::prepare(&path_for_prepare, &active, operation)
    })
    .await
    .map_err(Error::invalid)??;
    if candidate.config.listeners != snapshot.config.listeners {
        return Err(Error::invalid("listener names/addresses require a restart"));
    }
    let prepared = services.prepare(&candidate.config).await?;
    let service_addresses = services.addresses(&candidate.config, &prepared)?;
    let addresses: Vec<_> = snapshot
        .listener_addresses
        .values()
        .chain(service_addresses.values())
        .copied()
        .collect();
    check_bound_loops(&candidate.config, &addresses)?;
    let mut next = Snapshot::new(candidate.config, Some(snapshot))?;
    next.revision = control::revision(&candidate.source);
    next.source = Some(Arc::from(candidate.source.as_str()));
    next.listener_addresses = snapshot.listener_addresses.clone();
    next.service_addresses = service_addresses;
    next.addresses = Arc::new(addresses);
    if candidate.save {
        tokio::task::spawn_blocking(move || {
            control::persist(&path, &candidate.source, &candidate.previous_disk)
        })
        .await
        .map_err(Error::invalid)??;
    }
    Ok((Arc::new(next), prepared))
}

pub async fn serve(config: Config, source: Option<PathBuf>) -> io::Result<()> {
    use crate::{control, web};
    // Install handlers before advertising readiness and resolve the selected
    // file once, so saving through a symlink updates its target atomically.
    let mut signals = Signals::new()?;
    let source = source.map(std::fs::canonicalize).transpose()?;
    let (config, document) = if let Some(path) = source.clone() {
        tokio::task::spawn_blocking(move || -> io::Result<_> {
            let text = control::read_source(&path)?;
            let config = Config::from_lua(&text, &path.display().to_string())?;
            Ok((config, Some(text)))
        })
        .await
        .map_err(io::Error::other)??
    } else {
        (config, None)
    };
    let mut initial = Snapshot::new(config, None)?;
    if let Some(document) = document {
        initial.revision = control::revision(&document);
        initial.source = Some(Arc::from(document));
    }
    let mut listeners = Vec::new();
    for (name, address) in &initial.config.listeners {
        let listener = TcpListener::bind(address).await.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("listeners.{name} ({address}): {error}"),
            )
        })?;
        initial
            .listener_addresses
            .insert(name.clone(), listener.local_addr()?);
        listeners.push((name.clone(), listener));
    }
    let mut services = web::Services::default();
    let prepared = services.prepare(&initial.config).await?;
    initial.service_addresses = services.addresses(&initial.config, &prepared)?;
    initial.addresses = Arc::new(
        initial
            .listener_addresses
            .values()
            .chain(initial.service_addresses.values())
            .copied()
            .collect(),
    );
    check_bound_loops(&initial.config, &initial.addresses)?;
    let mut snapshot = Arc::new(initial);
    let metrics = Arc::new(Metrics::default());
    let admission = Arc::new(Mutex::new(Admission::default()));
    let (current, receiver) = watch::channel(snapshot.clone());
    let (stop, stopping) = watch::channel(false);
    let (commands, mut requests) = tokio::sync::mpsc::channel::<control::Command>(8);
    let app = web::App::new(receiver.clone(), metrics.clone(), commands, source);
    let mut tasks = JoinSet::new();
    for (name, listener) in listeners {
        eprintln!("rift: listening on {}", listener.local_addr()?);
        tasks.spawn(accept(
            listener,
            name,
            receiver.clone(),
            stopping.clone(),
            metrics.clone(),
            admission.clone(),
        ));
    }
    services.commit(&snapshot.config, prepared, &app);
    let mut health = health_worker(
        snapshot.clone(),
        snapshot.addresses.clone(),
        metrics.clone(),
    );
    let mut failure = None;
    let mut service_check = tokio::time::interval(Duration::from_secs(1));
    loop {
        let (operation, reply) = tokio::select! {
            event = signals.next() => match event {
                Event::Shutdown => break,
                Event::Reload => {
                    if app.path.is_none() { eprintln!("rift: reload ignored: no configuration file selected"); continue; }
                    (control::Operation::Reload, None)
                }
            },
            Some(command) = requests.recv() => (command.operation, Some(command.reply)),
            result = tasks.join_next() => {
                failure = Some(io::Error::other(format!("listener stopped unexpectedly: {result:?}")));
                break;
            }
            _ = service_check.tick() => {
                if let Some(name) = services.failed() {
                    failure = Some(io::Error::other(format!("{name} listener stopped unexpectedly")));
                    break;
                }
                continue;
            }
        };
        let result = reconfigure(operation, &app, &snapshot, &services).await;
        let response = match result {
            Ok((next, prepared)) => {
                snapshot = next;
                current.send_replace(snapshot.clone());
                services.commit(&snapshot.config, prepared, &app);
                if let Some(task) = health.take() {
                    task.abort();
                    let _ = task.await;
                }
                health = health_worker(
                    snapshot.clone(),
                    snapshot.addresses.clone(),
                    metrics.clone(),
                );
                metrics.reloads.inc();
                eprintln!("rift: configuration reloaded");
                Ok(snapshot.revision.clone())
            }
            Err(error) => {
                metrics.reload_failures.inc();
                eprintln!("rift: reload rejected: {}", error.message);
                Err(error)
            }
        };
        if let Some(reply) = reply {
            let _ = reply.send(response);
        }
    }
    // Stop accepting configuration commands before draining proxy sessions.
    requests.close();
    while let Ok(command) = requests.try_recv() {
        let _ = command.reply.send(Err(control::Error {
            status: 503,
            message: "proxy shutting down".into(),
        }));
    }
    stop.send_replace(true);
    if let Some(task) = health {
        task.abort();
        let _ = task.await;
    }
    eprintln!("rift: draining {} connections", metrics.active.get());
    let drained = async { while tasks.join_next().await.is_some() {} };
    tokio::select! {
        result = timeout(snapshot.config.shutdown_timeout, drained) => {
            if result.is_err() {
                metrics.forced_shutdowns.inc();
                eprintln!("rift: shutdown deadline reached; closing {} connections", metrics.active.get());
            }
        }
        _ = signals.shutdown() => {
            metrics.forced_shutdowns.inc();
            eprintln!("rift: second shutdown signal; closing remaining connections");
        }
    }
    tasks.shutdown().await;
    services.shutdown().await;
    eprintln!("rift: shutdown complete");
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn check_bound_loops(config: &Config, addresses: &[SocketAddr]) -> io::Result<()> {
    for backend in config.backends.values() {
        for address in addresses {
            backend.check_loop(*address)?;
        }
    }
    Ok(())
}

enum Event {
    Reload,
    Shutdown,
}

#[cfg(unix)]
struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    reload: tokio::signal::unix::Signal,
}
#[cfg(unix)]
impl Signals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            reload: signal(SignalKind::hangup())?,
        })
    }
    async fn next(&mut self) -> Event {
        tokio::select! { _ = self.interrupt.recv() => Event::Shutdown, _ = self.terminate.recv() => Event::Shutdown, _ = self.reload.recv() => Event::Reload }
    }
    async fn shutdown(&mut self) {
        tokio::select! { _ = self.interrupt.recv() => {}, _ = self.terminate.recv() => {} }
    }
}
#[cfg(windows)]
struct Signals {
    interrupt: tokio::signal::windows::CtrlC,
    reload: tokio::signal::windows::CtrlBreak,
}
#[cfg(windows)]
impl Signals {
    fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::windows::ctrl_c()?,
            reload: tokio::signal::windows::ctrl_break()?,
        })
    }
    async fn next(&mut self) -> Event {
        tokio::select! { _ = self.interrupt.recv() => Event::Shutdown, _ = self.reload.recv() => Event::Reload }
    }
    async fn shutdown(&mut self) {
        self.interrupt.recv().await;
    }
}
