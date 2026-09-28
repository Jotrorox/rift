use crate::{
    admission::Admission,
    health::{self, Health},
    metrics::{Active, Metered, Metrics},
    status,
};
use rift::{
    config::{Config, Route},
    hooks::{ConnectionInfo, RouteDecision, Router},
    routing::Routes,
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
    io::{AsyncWriteExt, copy_bidirectional_with_sizes},
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
        })
    }
}

pub async fn handle(
    client: &mut TcpStream,
    listener: &str,
    snapshot: Arc<Snapshot>,
    addresses: &[SocketAddr],
    metrics: Arc<Metrics>,
) -> io::Result<(u64, u64)> {
    client.set_nodelay(true)?;
    let connection = ConnectionInfo {
        listener: listener.to_owned(),
        peer_addr: client.peer_addr()?,
        local_addr: client.local_addr()?,
        default_backend: snapshot.router.default_backend(listener).map(str::to_owned),
    };
    let selected = match snapshot.router.route(connection).await {
        Ok(RouteDecision::Default) => None,
        Ok(RouteDecision::Backend(name)) => Some(name),
        Ok(RouteDecision::Reject { .. }) => {
            metrics.route_rejected.inc();
            return Ok((0, 0));
        }
        Err(error) => {
            metrics.route_rejected.inc();
            return Err(io::Error::other(format!("on_route: {error}")));
        }
    };
    let mut client = Metered::new(client, metrics.clone());
    let mut packet = Vec::new();
    let mut is_status = false;
    let primary = match selected {
        Some(name) => name,
        None => match &snapshot.policies[listener] {
            Policy::Direct(name) => name.clone(),
            Policy::Hostnames(routes) => {
                let handshake = timeout(
                    HANDSHAKE_TIMEOUT,
                    crate::handshake::read_handshake(&mut client),
                )
                .await??;
                is_status = handshake.state == 1;
                packet = handshake.packet;
                routes.select(&handshake.host)?.clone()
            }
        },
    };
    if let Some(settings) = snapshot.config.status_cache.filter(|_| is_status) {
        // Cached status is a bounded protocol exchange. Login and transfer never
        // enter this path, and direct listeners remain byte-transparent.
        timeout(HANDSHAKE_TIMEOUT, status::request(&mut client)).await??;
        let deadline = Deadline::now() + snapshot.config.limits.connect_timeout;
        let response = timeout_at(deadline, async {
            if let Some(response) = snapshot.cache.get(listener, &packet) {
                metrics.cache_hits.inc();
                return Ok::<_, io::Error>(response);
            }
            let _fill = match snapshot
                .cache
                .fill_lock(listener, &packet, settings.max_entries)
            {
                Some(lock) => Some(lock.lock_owned().await),
                None => None,
            };
            if let Some(response) = snapshot.cache.get(listener, &packet) {
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
            )
            .await?;
            upstream.set_nodelay(true)?;
            upstream.write_all(&packet).await?;
            upstream.write_all(&[1, 0]).await?;
            let response = status::frame(&mut upstream, settings.max_response_bytes).await?;
            status::validate_response(&response)?;
            snapshot
                .cache
                .insert(listener, &packet, response.clone(), settings);
            Ok(Arc::new(response))
        })
        .await??;
        timeout(HANDSHAKE_TIMEOUT, status::respond(&mut client, &response)).await??;
    } else {
        let deadline = Deadline::now() + snapshot.config.limits.connect_timeout;
        let mut upstream = health::connect(
            &snapshot.config,
            &snapshot.health,
            &primary,
            addresses,
            &metrics,
            deadline,
        )
        .await?;
        upstream.set_nodelay(true)?;
        // Once any client bytes are sent, errors close this session. Never
        // replay a partially forwarded handshake or migrate an active relay.
        timeout_at(deadline, upstream.write_all(&packet)).await??;
        let buffer_size = snapshot.config.limits.buffer_size;
        // Active relays retain sockets and buffers, not old caches or scripts.
        drop(snapshot);
        copy_bidirectional_with_sizes(&mut client, &mut upstream, buffer_size, buffer_size).await?;
    }
    Ok((client.sent, client.received))
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
                metrics.accepted.inc();
                let snapshot = current.borrow().clone();
                if !admission.lock().unwrap().allow(peer.ip(), snapshot.config.rate_limit, Instant::now()) {
                    metrics.rate_rejected.inc();
                    continue;
                }
                let Some(active) = Active::new(metrics.clone(), snapshot.config.limits.max_connections) else {
                    metrics.capacity_rejected.inc();
                    continue;
                };
                let name = name.clone();
                let addresses = addresses.clone();
                let metrics = metrics.clone();
                sessions.spawn(async move {
                    let mut client = client;
                    let result = handle(&mut client, &name, snapshot, &addresses, metrics.clone()).await;
                    // Make capacity available before the peer can observe EOF and
                    // reconnect. The handler borrows the socket until it returns.
                    drop(active);
                    drop(client);
                    if let Err(error) = result {
                        metrics.errors.inc();
                        eprintln!("rift: {peer}: {error}");
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
