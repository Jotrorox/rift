use crate::{
    admission::Admission,
    events::Connection,
    health::{self, Health},
    metrics::{Active, Metered, Metrics, Player},
    network::Network,
    status,
};
use rift::{
    config::{Config, Route},
    hooks::{ConnectionInfo, RouteDecision, Router},
    players::PlayerRegistry,
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
    policies: BTreeMap<String, Policy>,
    router: Router,
    pub health: Health,
    cache: status::Cache,
    pub players: Arc<PlayerRegistry>,
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
            config,
            policies,
            router,
            health,
            cache: status::Cache::default(),
            players: previous.map_or_else(
                || Arc::new(PlayerRegistry::default()),
                |old| old.players.clone(),
            ),
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
    let use_initial = selected.is_none()
        && matches!(&snapshot.policies[listener], Policy::Direct(_))
        && !snapshot.config.network.initial.is_empty();
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
        let network_configured = snapshot.config.network != Default::default();
        let network_enabled = session.switch_supported() && network_configured;
        if network_enabled {
            session.enable_network()?;
        }
        let login_name = if network_configured {
            match timeout(HANDSHAKE_TIMEOUT, session.read_login_start()).await {
                Ok(Ok(login)) => login.name,
                result => {
                    let error = match result {
                        Ok(Err(error)) => error,
                        Err(error) => error.into(),
                        _ => unreachable!(),
                    };
                    let _ = timeout(
                        HANDSHAKE_TIMEOUT,
                        session.disconnect("Invalid or missing login start."),
                    )
                    .await;
                    return Err(error);
                }
            }
        } else {
            String::new()
        };
        let network = Network {
            snapshot: &snapshot,
            addresses,
            metrics: &metrics,
        };
        let candidates = match network.initial_candidates(&primary, use_initial, &login_name) {
            Ok(candidates) => candidates,
            Err(error) => {
                event.reject("access_denied", &error.to_string());
                let _ = timeout(HANDSHAKE_TIMEOUT, session.disconnect(&error.to_string())).await;
                return Err(error);
            }
        };
        // Reserve the name before contacting an offline backend, which might
        // otherwise evict the existing player as soon as a duplicate logs in.
        // The client's claimed UUID is never used as the confirmed identity.
        let mut name_reservation = if network_configured {
            match snapshot.players.reserve_name(&login_name) {
                Ok(reservation) => Some(reservation),
                Err(error) => {
                    let _ =
                        timeout(HANDSHAKE_TIMEOUT, session.disconnect(&error.to_string())).await;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let deadline = Deadline::now() + snapshot.config.limits.connect_timeout;
        let mut current_backend = match network
            .connect_initial(&mut session, &candidates, deadline, event)
            .await
        {
            Ok(name) => name,
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
        let entry_deadline = |current: &str| {
            let remaining =
                candidates.len() - candidates.iter().position(|name| name == current).unwrap();
            Deadline::now() + deadline.saturating_duration_since(Deadline::now()) / remaining as u32
        };
        let mut phase_deadline = if network_configured {
            entry_deadline(&current_backend)
        } else {
            Deadline::now() + Duration::from_secs(30)
        };
        let mut login_complete = false;
        let mut client_closed = false;
        let mut player = None;
        let mut registration = None;
        let mut registered_backend = String::new();
        loop {
            event.stage = "session";
            event.failure = "io_error";
            event.backend = Some(current_backend.clone());
            event.backend_address = Some(
                snapshot.config.backends[&current_backend]
                    .address()
                    .to_owned(),
            );
            // Reaching configuration's finish acknowledgement alone is not enough:
            // a network player must receive the new world's Join Game as well.
            let playing = !client_closed
                && session.client.state.settled(rift::protocol::State::Play)
                && (!network_enabled || session.can_switch());
            let result = if playing {
                session.forward().await
            } else {
                match timeout_at(phase_deadline, session.forward()).await {
                    Ok(result) => result,
                    Err(_) if network_configured && !login_complete => {
                        Ok(SessionEvent::BackendFailed)
                    }
                    Err(error) => Err(error.into()),
                }
            };
            match result {
                Ok(SessionEvent::Packet) => {
                    if !login_complete && session.identity().is_some() {
                        login_complete = true;
                        phase_deadline = Deadline::now() + Duration::from_secs(30);
                    }
                    if registration.is_none()
                        && let Some(identity) = session.identity()
                        && let Some(uuid) = identity.uuid
                    {
                        let registered = if let Some(reservation) = name_reservation.take() {
                            reservation.register(uuid, &current_backend)
                        } else {
                            snapshot
                                .players
                                .register(uuid, identity.name, &current_backend)
                        };
                        match registered {
                            Ok(guard) => registration = Some(guard),
                            Err(error) => {
                                let _ = timeout(
                                    HANDSHAKE_TIMEOUT,
                                    session.disconnect("This player is already connected."),
                                )
                                .await;
                                return Err(error);
                            }
                        }
                    }
                    let ready = session.client.state.settled(rift::protocol::State::Play)
                        && (!network_enabled || session.can_switch());
                    if ready {
                        if player.is_none() {
                            player = Some(Player::new(metrics.clone()));
                        }
                        if registered_backend != current_backend
                            && let Some(registration) = &registration
                        {
                            registration.set_server(&current_backend);
                            registered_backend.clone_from(&current_backend);
                        }
                    }
                    if playing && !ready {
                        phase_deadline = Deadline::now() + Duration::from_secs(30);
                    }
                }
                Ok(SessionEvent::ProxyCommand(command)) => {
                    let mut result = match timeout(
                        snapshot.config.limits.connect_timeout + HANDSHAKE_TIMEOUT,
                        network.command(&mut session, &current_backend, &command, event),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(error) => Err(error.into()),
                    };
                    // The old server can fail while a target is being preflighted.
                    // A failed target must not strand that player without recovery.
                    if result
                        .as_ref()
                        .is_err_and(|error| error.kind() != io::ErrorKind::PermissionDenied)
                        && session.backend().is_none()
                        && session.can_switch()
                    {
                        result = network
                            .switch(
                                &mut session,
                                &current_backend,
                                &network.recovery_candidates(&current_backend),
                                event,
                            )
                            .await
                            .map(Some);
                        if result.is_ok() {
                            metrics.fallbacks.inc();
                        }
                    }
                    match result {
                        Ok(Some(target)) => {
                            current_backend = target;
                            phase_deadline = Deadline::now() + Duration::from_secs(30);
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let _ = timeout(
                                HANDSHAKE_TIMEOUT,
                                session.disconnect("The server switch could not be completed."),
                            )
                            .await;
                            return Err(error);
                        }
                    }
                }
                Ok(SessionEvent::Disconnected) => break,
                Ok(SessionEvent::ClientClosed) => {
                    client_closed = true;
                    phase_deadline = Deadline::now() + Duration::from_secs(30);
                }
                Ok(SessionEvent::BackendClosed | SessionEvent::BackendFailed) => {
                    if client_closed {
                        break;
                    }
                    if network_configured && !login_complete && session.reset_initial_backend() {
                        let next = candidates
                            .iter()
                            .position(|name| name == &current_backend)
                            .unwrap()
                            + 1;
                        if next < candidates.len()
                            && let Ok(target) = network
                                .connect_initial(&mut session, &candidates[next..], deadline, event)
                                .await
                        {
                            metrics.fallbacks.inc();
                            current_backend = target;
                            phase_deadline = entry_deadline(&current_backend);
                            continue;
                        }
                    }
                    let candidates = network.recovery_candidates(&current_backend);
                    if network_enabled && session.can_switch() && !candidates.is_empty() {
                        match network
                            .switch(&mut session, &current_backend, &candidates, event)
                            .await
                        {
                            Ok(target) => {
                                metrics.fallbacks.inc();
                                current_backend = target;
                                phase_deadline = Deadline::now() + Duration::from_secs(30);
                                continue;
                            }
                            Err(error) => {
                                let reason = if error.kind() == io::ErrorKind::PermissionDenied {
                                    error.to_string()
                                } else {
                                    "The server connection was lost and no fallback was available."
                                        .into()
                                };
                                let _ =
                                    timeout(HANDSHAKE_TIMEOUT, session.disconnect(&reason)).await;
                                return Err(error);
                            }
                        }
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
    addresses: Arc<Vec<SocketAddr>>,
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
                let addresses = addresses.clone();
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

pub async fn serve(config: Config, source: Option<PathBuf>) -> io::Result<()> {
    // Install handlers before advertising readiness.
    let mut signals = Signals::new()?;
    let mut snapshot = Arc::new(Snapshot::new(config, None)?);
    let mut listeners = Vec::new();
    for (name, address) in &snapshot.config.listeners {
        let listener = TcpListener::bind(address).await.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("listeners.{name} ({address}): {error}"),
            )
        })?;
        listeners.push((name.clone(), listener));
    }
    let metrics_listener = match snapshot.config.metrics {
        Some(address) => Some(TcpListener::bind(address).await.map_err(|error| {
            io::Error::new(error.kind(), format!("metrics ({address}): {error}"))
        })?),
        None => None,
    };
    let mut addresses = listeners
        .iter()
        .map(|(_, listener)| listener.local_addr())
        .collect::<io::Result<Vec<_>>>()?;
    if let Some(listener) = &metrics_listener {
        addresses.push(listener.local_addr()?);
    }
    check_bound_loops(&snapshot.config, &addresses)?;
    let addresses = Arc::new(addresses);
    let metrics = Arc::new(Metrics::default());
    let admission = Arc::new(Mutex::new(Admission::default()));
    let (current, receiver) = watch::channel(snapshot.clone());
    let (stop, stopping) = watch::channel(false);
    let mut tasks = JoinSet::new();
    for (name, listener) in listeners {
        eprintln!("rift: listening on {}", listener.local_addr()?);
        tasks.spawn(accept(
            listener,
            name,
            receiver.clone(),
            stopping.clone(),
            addresses.clone(),
            metrics.clone(),
            admission.clone(),
        ));
    }
    let mut background = JoinSet::new();
    if let Some(listener) = metrics_listener {
        eprintln!("rift: metrics on {}", listener.local_addr()?);
        background.spawn(crate::metrics::serve(listener, receiver, metrics.clone()));
    }
    let mut health = health_worker(snapshot.clone(), addresses.clone(), metrics.clone());
    let mut reloads = JoinSet::new();
    let mut failure = None;
    loop {
        tokio::select! {
            event = signals.next() => match event {
                Event::Shutdown => break,
                Event::Reload => {
                    if let Some(path) = source.clone() {
                        if reloads.is_empty() { reloads.spawn_blocking(move || Config::load(&path)); }
                    } else { eprintln!("rift: reload ignored: no configuration file selected"); }
                }
            },
            result = reloads.join_next(), if !reloads.is_empty() => {
                let candidate = result.expect("reload task").map_err(io::Error::other).and_then(|r| r)
                    .and_then(|config| {
                        if config.listeners != snapshot.config.listeners || config.metrics != snapshot.config.metrics {
                            return Err(io::Error::new(io::ErrorKind::InvalidInput, "listener names/addresses and metrics address require a restart"));
                        }
                        check_bound_loops(&config, &addresses)?;
                        Snapshot::new(config, Some(&snapshot))
                    });
                match candidate {
                    Ok(candidate) => {
                        snapshot.health.retire();
                        snapshot = Arc::new(candidate);
                        current.send_replace(snapshot.clone());
                        if let Some(task) = health.take() { task.abort(); let _ = task.await; }
                        health = health_worker(snapshot.clone(), addresses.clone(), metrics.clone());
                        metrics.reloads.inc();
                        eprintln!("rift: configuration reloaded");
                    }
                    Err(error) => { metrics.reload_failures.inc(); eprintln!("rift: reload rejected: {error}"); }
                }
            }
            result = tasks.join_next() => {
                failure = Some(io::Error::other(format!("listener stopped unexpectedly: {result:?}")));
                break;
            }
            result = background.join_next(), if !background.is_empty() => {
                failure = Some(io::Error::other(format!("metrics listener stopped unexpectedly: {result:?}")));
                break;
            }
        }
    }
    stop.send_replace(true);
    snapshot.health.retire();
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
    background.shutdown().await;
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
