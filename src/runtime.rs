use crate::{
    admin::{self, Control},
    admission::Admission,
    events::Connection,
    health::{self, Health},
    metrics::{Active, Metered, Metrics, Player},
    network::Network,
    status,
};
use rift::{
    auth::Authenticator,
    config::{Config, Route},
    hooks::{ConnectionInfo, RouteDecision, Router},
    messaging::{Broker, BrokerConfig, Stream},
    players::PlayerRegistry,
    protocol::{Codec, NextState, status_response},
    routing::Routes,
    session::{Session, SessionEvent},
};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
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
    generation: u64,
    pub control: Arc<Control>,
    pub source: Option<Arc<str>>,
    pub revision: String,
    pub listener_addresses: BTreeMap<String, SocketAddr>,
    pub service_addresses: BTreeMap<String, SocketAddr>,
    pub addresses: Arc<Vec<SocketAddr>>,
    policies: BTreeMap<String, Policy>,
    router: Router,
    pub health: Health,
    cache: status::Cache,
    pub players: Arc<PlayerRegistry>,
    pub messaging: Broker,
    pub messaging_streams: Arc<HashMap<String, Stream>>,
    authenticator: Option<Arc<Authenticator>>,
    forwarding_secret: Option<Arc<[u8]>>,
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
        let messaging = match previous {
            Some(old) => old.messaging.clone(),
            None => Broker::new(config.messaging.as_ref().map_or_else(
                BrokerConfig::default,
                |settings| BrokerConfig {
                    subscription_capacity: settings.subscription_capacity,
                    max_payload_bytes: settings.max_payload_bytes,
                    max_subject_bytes: settings.max_subject_bytes,
                    max_subscriptions: settings.max_subscriptions,
                },
            ))
            .map_err(io::Error::other)?,
        };
        let router = previous
            .map_or_else(
                || Router::new(&config),
                |old| old.router.reconfigured(&config),
            )
            .with_messaging(messaging.clone());
        let control =
            previous.map_or_else(|| Arc::new(Control::default()), |old| old.control.clone());
        let health = Health::with_control(&config, control.clone());
        let forwarding_secret = config.forwarding.as_ref().map(|settings| {
            let secret = std::env::var(&settings.secret_env).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, format!(
                    "forwarding.secret_env: set {} to the Paper Velocity secret before starting Rift",
                    settings.secret_env,
                ))
            })?;
            if secret.is_empty() || secret.len() > 1024 || secret.chars().any(char::is_control) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput,
                    "forwarding.secret_env: secret must contain 1..=1024 bytes without control characters"));
            }
            Ok(Arc::<[u8]>::from(secret.into_bytes()))
        }).transpose()?;
        let authenticator = if config.authentication.online_mode {
            match previous
                .filter(|old| old.config.authentication == config.authentication)
                .and_then(|old| old.authenticator.clone())
            {
                Some(authenticator) => Some(authenticator),
                None => Some(Arc::new(Authenticator::new(config.authentication.timeout)?)),
            }
        } else {
            None
        };
        Ok(Self {
            source: None,
            revision: "runtime".into(),
            listener_addresses: config.listeners.clone(),
            service_addresses: BTreeMap::new(),
            addresses: Arc::new(config.listeners.values().copied().collect()),
            config,
            authenticator,
            forwarding_secret,
            generation: previous.map_or(1, |old| old.generation + 1),
            control,
            policies,
            router,
            messaging,
            messaging_streams: previous.map_or_else(
                || Arc::new(HashMap::new()),
                |old| old.messaging_streams.clone(),
            ),
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
    let peer_ip = client.peer_addr()?.ip().to_canonical();
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
    if !is_status {
        event.stage = "login_admission";
        let maintenance = snapshot.control.maintenance(&snapshot.config);
        let login_allowed = snapshot.control.logins.lock().unwrap().allow_generation(
            session.client.io.stream.peer_addr()?.ip(),
            snapshot.config.login_rate_limit,
            snapshot.generation,
            Instant::now(),
        );
        if maintenance || !login_allowed {
            let reason = if maintenance {
                metrics.access_rejected.inc();
                event.reject("maintenance", "network is in maintenance mode");
                "The network is under maintenance. Please try again later."
            } else {
                metrics.login_rejected.inc();
                event.reject("login_rate_limited", "login rate limit exceeded");
                "Too many login attempts. Please wait before reconnecting."
            };
            let _ = timeout(HANDSHAKE_TIMEOUT, session.disconnect(reason)).await;
            return Ok((session.client.io.sent, session.client.io.received));
        }
    }
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
            if snapshot.control.maintenance(&snapshot.config) {
                "The network is under maintenance."
            } else {
                "Rift"
            },
        );
        if let Some(settings) = snapshot
            .config
            .status_cache
            .filter(|_| !snapshot.control.maintenance(&snapshot.config))
        {
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
        if snapshot.authenticator.is_some() && session.handshake.protocol < 761 {
            let reason = "Online authentication with Velocity forwarding requires Minecraft 1.19.3 or newer.";
            let _ = timeout(HANDSHAKE_TIMEOUT, session.disconnect(reason)).await;
            return Err(io::Error::new(io::ErrorKind::Unsupported, reason));
        }
        let network_configured = snapshot.config.network != Default::default();
        let network_enabled = session.switch_supported() && network_configured;
        if network_enabled {
            session.enable_network()?;
        }
        let mut login_name = if network_configured || snapshot.authenticator.is_some() {
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
        if let Some(authenticator) = &snapshot.authenticator {
            // Authentication finishes before access rules, duplicate reservations,
            // DNS or backend connection attempts can act on a claimed identity.
            event.stage = "authentication";
            event.failure = "authentication_failed";
            let result = timeout(
                snapshot.config.authentication.timeout,
                session.authenticate(authenticator),
            )
            .await;
            if let Err(error) = result.map_err(io::Error::from).and_then(|result| result) {
                let reason = if error.kind() == io::ErrorKind::PermissionDenied {
                    "Unable to verify your Minecraft account. Please sign in again."
                } else if error.kind() == io::ErrorKind::InvalidData {
                    "Invalid authentication response. Please reconnect."
                } else {
                    "Minecraft authentication is unavailable or timed out. Please try again later."
                };
                let _ = timeout(HANDSHAKE_TIMEOUT, session.disconnect(reason)).await;
                return Err(error);
            }
            login_name = session.authenticated_profile().unwrap().name.clone();
            session.set_forwarding(snapshot.forwarding_secret.clone().unwrap(), peer_ip)?;
            event.failure = "io_error";
        }
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
        // Reserve the name before contacting a backend, which might
        // otherwise evict the existing player as soon as a duplicate logs in.
        // The client's claimed UUID is never used as the confirmed identity.
        let mut name_reservation = if network_configured || snapshot.authenticator.is_some() {
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
        let mut player: Option<Player> = None;
        let mut registration = None;
        let mut registered_backend = String::new();
        let control = snapshot.control.clone();
        let mut admin_registration = None;
        let mut transfers: Option<mpsc::Receiver<admin::Transfer>> = None;
        let mut transfer_reply: Option<admin::Reply> = None;
        // An administrator may select a backend added after this session's
        // snapshot. Keep its address without indexing the older configuration.
        let mut current_address = snapshot.config.backends[&current_backend]
            .address()
            .to_owned();
        loop {
            event.stage = if transfer_reply.is_some() {
                "transfer"
            } else {
                match session
                    .client
                    .state
                    .phase(rift::protocol::Direction::Clientbound)
                {
                    rift::protocol::State::Login => "login",
                    rift::protocol::State::Configuration => "configuration",
                    _ => "play",
                }
            };
            event.failure = "io_error";
            event.backend = Some(current_backend.clone());
            event.backend_address = Some(current_address.clone());
            // Reaching configuration's finish acknowledgement alone is not enough:
            // a network player must receive the new world's Join Game as well.
            let playing = !client_closed
                && session.client.state.settled(rift::protocol::State::Play)
                && (!network_enabled || session.can_switch())
                && !session.switch_in_progress();
            let forward = async {
                if playing {
                    session.forward().await
                } else {
                    match timeout_at(phase_deadline, session.forward()).await {
                        Ok(result) => result,
                        Err(_) if network_configured && !login_complete => {
                            Ok(SessionEvent::BackendFailed)
                        }
                        Err(error) => Err(error.into()),
                    }
                }
            };
            let result = tokio::select! {
                result = forward => result,
                request = async { transfers.as_mut().expect("registered player").recv().await },
                    if playing && transfers.is_some() && transfer_reply.is_none() => {
                    let Some(request) = request else { transfers = None; continue; };
                    // The cancelled forward poll may already have queued the
                    // backend's Start Configuration behind client backpressure.
                    if !session.client.state.settled(rift::protocol::State::Play) {
                        phase_deadline = Deadline::now() + Duration::from_secs(30);
                    }
                    if request.reply.is_closed() { continue; }
                    let target = &request.backend;
                    if !session.can_switch() || current_backend == *target {
                        let _ = request.reply.send(Err("transfer requires a joined Minecraft 1.21.11 player and a different backend".into()));
                        continue;
                    }
                    let name = session.player_name().unwrap_or_default();
                    if snapshot.config.network.access.get(target).is_some_and(|access| !access.permits(name))
                        || !request.snapshot.config.can_access(target, name)
                        || control.draining(&request.snapshot.config, target)
                        || !request.snapshot.health.available(target)
                    {
                        metrics.transfer_failures.inc();
                        let _ = request.reply.send(Err("target backend is unavailable or the player does not have access".into()));
                        continue;
                    }
                    let deadline = Deadline::now() + Duration::from_secs(30);
                    let connect_deadline = deadline.min(Deadline::now() + request.snapshot.config.limits.connect_timeout);
                    let upstream = match request.snapshot.config.backends[target].connect_until(&request.snapshot.addresses, connect_deadline).await {
                        Ok(upstream) => upstream,
                        Err(error) => {
                            metrics.transfer_failures.inc();
                            metrics.backend_failures.inc();
                            request.snapshot.health.record(target, false);
                            event.backend = Some(target.clone());
                            event.backend_address = Some(request.snapshot.config.backends[target].address().to_owned());
                            (event.stage, event.failure) = match error.stage {
                                rift::routing::ConnectStage::Dns => ("dns", "dns_error"),
                                rift::routing::ConnectStage::Connect => ("connect", "connect_error"),
                            };
                            event.emit("backend_attempt_failed", &error.error, Some(error.error.kind()));
                            let _ = request.reply.send(Err(format!("target connection failed: {}; player remains on the original backend", error.error)));
                            continue;
                        }
                    };
                    if control.draining(&request.snapshot.config, target) {
                        metrics.transfer_failures.inc();
                        let _ = request.reply.send(Err("target backend started draining; player remains on the original backend".into()));
                        continue;
                    }
                    request.snapshot.health.record(target, true);
                    if let Err(error) = upstream.set_nodelay(true) {
                        metrics.transfer_failures.inc();
                        let _ = request.reply.send(Err(format!("target socket setup failed; player remains on the original backend: {error}")));
                        continue;
                    }
                    event.stage = "transfer";
                    event.failure = "transfer_failed";
                    let result = timeout_at(deadline, session.connect_backend(upstream)).await;
                    match result {
                        Ok(Ok(())) => {
                            current_backend = target.clone();
                            current_address = request.snapshot.config.backends[target].address().to_owned();
                            transfer_reply = Some(request.reply);
                            phase_deadline = deadline;
                            continue;
                        }
                        result => {
                            let error = match result { Ok(Err(error)) => error, Err(error) => error.into(), _ => unreachable!() };
                            metrics.transfer_failures.inc();
                            if !session.switch_in_progress() && session.can_switch() {
                                let _ = request.reply.send(Err(format!("transfer failed: {error}")));
                                if session.backend().is_some() { continue; }
                                if error.kind() != io::ErrorKind::PermissionDenied {
                                    // The old backend failed during preflight. Let
                                    // the ordinary recovery path choose an allowed hub.
                                    Ok(SessionEvent::BackendFailed)
                                } else {
                                    return Err(error);
                                }
                            } else {
                                let _ = request.reply.send(Err(format!("transfer failed; session closed: {error}")));
                                // Transition writes may be partial; close directly.
                                return Err(error);
                            }
                        }
                    }
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
                        && (!network_enabled || session.can_switch())
                        && !session.switch_in_progress();
                    if ready {
                        if let Some(player) = &mut player {
                            if registered_backend != current_backend {
                                player.move_backend(current_backend.clone());
                                control.moved(event.id(), &current_backend);
                                metrics.transfers.inc();
                            }
                        } else {
                            player = Some(Player::new_on_backend(
                                metrics.clone(),
                                current_backend.clone(),
                            ));
                            metrics.observe_login(event.elapsed());
                            let (guard, receiver) = control.register(
                                event.id(),
                                current_backend.clone(),
                                session.handshake.protocol,
                                session.player_name().unwrap_or_default().to_owned(),
                            );
                            admin_registration = Some(guard);
                            transfers = Some(receiver);
                        }
                        if registered_backend != current_backend {
                            if let Some(registration) = &registration {
                                registration.set_server(&current_backend);
                            }
                            registered_backend.clone_from(&current_backend);
                        }
                        if let Some(reply) = transfer_reply.take() {
                            let _ = reply.send(Ok(serde_json::json!({
                                "connection_id": event.id(), "backend": current_backend,
                            })));
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
                            current_address =
                                snapshot.config.backends[&target].address().to_owned();
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
                            current_address =
                                snapshot.config.backends[&target].address().to_owned();
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
                                current_address =
                                    snapshot.config.backends[&target].address().to_owned();
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
                    if let Some(reply) = transfer_reply.take() {
                        metrics.transfer_failures.inc();
                        let _ =
                            reply.send(Err(format!("transfer failed; session closed: {error}")));
                    }
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
        drop(admin_registration);
        if let Some(reply) = transfer_reply {
            metrics.transfer_failures.inc();
            let _ = reply.send(Err("player disconnected during transfer".into()));
        }
        drop(registration);
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
    if candidate.config.listeners != snapshot.config.listeners
        || candidate.config.admin != snapshot.config.admin
        || candidate.config.messaging != snapshot.config.messaging
    {
        return Err(Error::invalid(
            "listener names/addresses, admin and messaging settings require a restart",
        ));
    }
    let prepared = services.prepare(&candidate.config).await?;
    let mut service_addresses = services.addresses(&candidate.config, &prepared)?;
    if let Some(address) = snapshot.service_addresses.get("admin") {
        service_addresses.insert("admin".into(), *address);
    }
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
    let admin_secret = initial
        .config
        .admin
        .as_ref()
        .map(admin::token)
        .transpose()?;
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
    let admin_listener = match &initial.config.admin {
        Some(settings) => Some(TcpListener::bind(settings.listen).await.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "admin.listen ({}): {error}; choose an unused loopback port",
                    settings.listen
                ),
            )
        })?),
        None => None,
    };
    let mut services = web::Services::default();
    let prepared = services.prepare(&initial.config).await?;
    initial.service_addresses = services.addresses(&initial.config, &prepared)?;
    if let Some(listener) = &admin_listener {
        initial
            .service_addresses
            .insert("admin".into(), listener.local_addr()?);
    }
    initial.messaging_streams =
        Arc::new(crate::messaging_runtime::open_streams(&initial.config).await?);
    let messaging_server = crate::messaging_runtime::listen(&initial).await?;
    let message_handler =
        rift::message_script::MessageHandler::new(&initial.config, initial.messaging.clone())
            .map_err(io::Error::other)?;
    let messaging_controls = crate::messaging_runtime::Controls::new(initial.messaging.clone())?;
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
    let mut background = JoinSet::new();
    let (admin_commands, mut admin_requests) = mpsc::channel(16);
    background.spawn(messaging_controls.run(
        receiver.clone(),
        metrics.clone(),
        admin_commands.clone(),
    ));
    let message_updates = message_handler
        .as_ref()
        .map(rift::message_script::MessageHandler::script_updates);
    let message_task = message_handler.map(|handler| tokio::spawn(handler.run()));
    if let Some(server) = &messaging_server {
        eprintln!(
            "rift: messaging QUIC on {}",
            server.local_addr().map_err(io::Error::other)?
        );
    }
    if let Some(listener) = admin_listener {
        eprintln!("rift: admin on {}", listener.local_addr()?);
        background.spawn(admin::serve(
            listener,
            snapshot.config.admin.clone().unwrap(),
            admin_secret.unwrap(),
            receiver.clone(),
            metrics.clone(),
            admin_commands,
        ));
    }
    services.commit(&snapshot.config, prepared, &app);
    crate::messaging_runtime::emit(
        &snapshot.messaging,
        "rift.events.lifecycle",
        serde_json::json!({"state":"ready"}),
    );
    let mut health = health_worker(
        snapshot.clone(),
        snapshot.addresses.clone(),
        metrics.clone(),
    );
    let mut failure = None;
    let mut service_check = tokio::time::interval(Duration::from_secs(1));
    loop {
        let (operation, reply, admin_reply) = tokio::select! {
            event = signals.next() => match event {
                Event::Shutdown => break,
                Event::Reload => {
                    if app.path.is_none() { eprintln!("rift: reload ignored: no configuration file selected"); continue; }
                    (control::Operation::Reload, None, None)
                }
            },
            Some(command) = requests.recv() => (command.operation, Some(command.reply), None),
            Some(command) = admin_requests.recv() => match command {
                admin::Command::Reload(reply) => (control::Operation::Reload, None, Some(reply)),
                admin::Command::Shutdown(ack) => { let _ = timeout(Duration::from_secs(2), ack).await; break; }
            },
            result = background.join_next(), if !background.is_empty() => {
                failure = Some(io::Error::other(format!("control service stopped unexpectedly: {result:?}")));
                break;
            }
            result = tasks.join_next() => {
                failure = Some(io::Error::other(format!("listener stopped unexpectedly: {result:?}")));
                break;
            }
            _ = service_check.tick() => {
                if messaging_server.as_ref().is_some_and(rift::messaging::quic::Server::is_finished) {
                    failure = Some(io::Error::other("QUIC messaging listener stopped unexpectedly"));
                    break;
                }
                if message_task.as_ref().is_some_and(JoinHandle::is_finished) {
                    failure = Some(io::Error::other("Lua message handler stopped unexpectedly"));
                    break;
                }
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
                // Keep subscriptions and queued messages across script reloads.
                if let (Some(updates), Some(script)) = (&message_updates, &next.config.on_message) {
                    updates.send_replace(script.clone());
                }
                snapshot.health.retire();
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
                crate::messaging_runtime::emit(
                    &snapshot.messaging,
                    "rift.events.reload",
                    serde_json::json!({"revision":snapshot.revision}),
                );
                Ok(snapshot.revision.clone())
            }
            Err(error) => {
                metrics.reload_failures.inc();
                eprintln!("rift: reload rejected: {}", error.message);
                Err(error)
            }
        };
        if let Some(reply) = admin_reply {
            let response = response.as_ref()
                .map(|revision| serde_json::json!({"reloaded":true,"revision":revision,"existing_sessions":"preserved"}))
                .map_err(|error| format!("reload rejected; previous configuration retained: {}", error.message));
            let _ = reply.send(response);
        }
        if let Some(reply) = reply {
            let _ = reply.send(response);
        }
    }
    // Stop accepting configuration commands before draining proxy sessions.
    admin_requests.close();
    while let Ok(command) = admin_requests.try_recv() {
        if let admin::Command::Reload(reply) = command {
            let _ = reply.send(Err("proxy is shutting down".into()));
        }
    }
    requests.close();
    while let Ok(command) = requests.try_recv() {
        let _ = command.reply.send(Err(control::Error {
            status: 503,
            message: "proxy shutting down".into(),
        }));
    }
    stop.send_replace(true);
    crate::messaging_runtime::emit(
        &snapshot.messaging,
        "rift.events.lifecycle",
        serde_json::json!({"state":"draining"}),
    );
    if let Some(task) = message_task {
        task.abort();
        let _ = task.await;
    }
    if let Some(server) = messaging_server {
        server.shutdown().await;
    }
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
