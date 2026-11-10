//! Policy and bounded attachment attempts shared by commands and outage recovery.
use crate::{events::Connection, health, metrics::Metrics, runtime::Snapshot};
use rift::session::{Session, SessionEvent};
use rift::text::{
    Args,
    catalog::{self, Notice},
};
use std::{collections::HashSet, future::Future, io, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    time::{Instant, sleep_until, timeout, timeout_at},
};

pub struct Network<'a> {
    pub snapshot: &'a Arc<Snapshot>,
    pub addresses: &'a [SocketAddr],
    pub metrics: &'a Metrics,
}

/// The UUID a backend will know the player by: the backend-confirmed one
/// after the first login, the Mojang one after online authentication, and
/// otherwise the offline UUID a backend computes from the name.
pub fn expected_uuid<C: AsyncRead + AsyncWrite + Unpin, B: AsyncRead + AsyncWrite + Unpin>(
    session: &Session<C, B>,
) -> Option<rift_permissions::Uuid> {
    let bytes = session
        .identity()
        .and_then(|identity| identity.uuid)
        .or_else(|| session.authenticated_profile().map(|profile| profile.uuid))
        .or_else(|| session.player_name().map(rift::players::offline_uuid))?;
    Some(rift_permissions::Uuid::from_bytes(bytes))
}

impl Network<'_> {
    /// Deliver a click on a dialog's plugin action to whoever showed the
    /// dialog. A failing handler is logged; the session carries on, as after
    /// a failing `join` observer.
    pub async fn dialog<C: AsyncRead + AsyncWrite + Unpin, B: AsyncRead + AsyncWrite + Unpin>(
        &self,
        session: &Session<C, B>,
        current: &str,
        event: &Connection,
        click: rift::session::DialogClick,
    ) {
        let result = match &session.extension {
            Some(extension) => extension.dialog(click.owner, click.response).await,
            // Without player hooks (persistent plugins in offline mode) there
            // is no extension session; plugins still get their clicks.
            None => {
                let mut context = rift::extensions::Context {
                    id: event.id(),
                    protocol: session.handshake.protocol,
                    server: Some(current.to_owned()),
                    ..Default::default()
                };
                if let Some(identity) = session.identity() {
                    context.name = identity.name;
                }
                if let Some(uuid) = expected_uuid(session) {
                    context.uuid = uuid.to_string();
                }
                let jobs = Arc::new(tokio::sync::Semaphore::new(1));
                self.snapshot
                    .extensions
                    .dialog(context, click.owner, click.response, &jobs)
                    .await
            }
        };
        if let Err(error) = result {
            eprintln!("rift: dialog: {error}");
        }
    }

    /// Publish the player's compiled subject for backend replicas before the
    /// connection to `backend` is forwarded.
    pub fn push_permissions<
        C: AsyncRead + AsyncWrite + Unpin,
        B: AsyncRead + AsyncWrite + Unpin,
    >(
        &self,
        session: &Session<C, B>,
        backend: &str,
    ) {
        if self.snapshot.perms.has_principals()
            && let Some(uuid) = expected_uuid(session)
        {
            self.snapshot.perms.push_before_connect(uuid, backend);
        }
    }

    /// Claim an attachment before waking the process so idle/manual shutdown
    /// cannot race a pending login. Health probes never call this path.
    pub async fn prepare_backend(
        &self,
        name: &str,
    ) -> io::Result<Option<rift::managed::ManagedLease>> {
        if !self.snapshot.config.backends.contains_key(name) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "backend is no longer registered",
            ));
        }
        if self.snapshot.control.draining(&self.snapshot.config, name) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "backend is draining",
            ));
        }
        let lease = self.snapshot.managed.reserve(name)?;
        self.snapshot.managed.ensure_running(name).await?;
        if self.snapshot.control.draining(&self.snapshot.config, name) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "backend is draining",
            ));
        }
        Ok(lease)
    }

    /// Keep the player's current world live while a cold destination starts.
    /// The startup future owns its reservation, so cancellation releases it.
    pub async fn prepare_transfer<C: AsyncRead + AsyncWrite + Unpin>(
        &self,
        session: &mut Session<C, TcpStream>,
        current: &str,
        name: &str,
        event: &mut Connection,
    ) -> io::Result<Option<rift::managed::ManagedLease>> {
        if !self.snapshot.managed.is_managed(name) || session.backend().is_none() {
            return self.prepare_backend(name).await;
        }
        self.forward_during_startup(session, current, event, self.prepare_backend(name))
            .await
    }

    async fn forward_during_startup<C: AsyncRead + AsyncWrite + Unpin>(
        &self,
        session: &mut Session<C, TcpStream>,
        current: &str,
        event: &mut Connection,
        startup: impl Future<Output = io::Result<Option<rift::managed::ManagedLease>>>,
    ) -> io::Result<Option<rift::managed::ManagedLease>> {
        tokio::pin!(startup);
        let mut ready = None;
        let mut transition_deadline = None;
        let mut command_notice_sent = false;
        loop {
            if session.can_switch() {
                if let Some(lease) = ready.take() {
                    return Ok(lease);
                }
                transition_deadline = None;
            } else {
                transition_deadline.get_or_insert(Instant::now() + Duration::from_secs(30));
            }
            tokio::select! {
                result = &mut startup, if ready.is_none() => {
                    ready = Some(result?);
                }
                result = session.forward() => match result? {
                    SessionEvent::Packet => {}
                    SessionEvent::ProxyCommand(_) => {
                        if !command_notice_sent {
                            timeout(Duration::from_secs(5), session.send_text(
                                &catalog::notice(Notice::SwitchStarting),
                            )).await??;
                            command_notice_sent = true;
                        }
                    }
                    SessionEvent::Suggestions(request) => {
                        let servers = self.joinable(session, current);
                        timeout(
                            Duration::from_secs(5),
                            session.answer_suggestions(&request, || servers),
                        ).await??;
                    }
                    SessionEvent::BungeeCord(request) => {
                        if let Some(identity) = session.identity()
                            && let Ok(Some(payload)) = crate::bungee_runtime::handle(
                                self.snapshot, current, &identity, event.peer_addr(), request,
                            ) {
                                timeout(Duration::from_secs(5), session.send_bungeecord(&payload)).await??;
                            }
                    }
                    SessionEvent::Dialog(click) => self.dialog(session, current, event, click).await,
                    SessionEvent::ClientClosed => return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof, "client closed while destination was starting",
                    )),
                    SessionEvent::Disconnected => return Err(rift::text::notice_error(
                        io::ErrorKind::PermissionDenied, Notice::ConnectionBackendClosed,
                    )),
                    SessionEvent::BackendClosed | SessionEvent::BackendFailed => return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted, "current backend closed while destination was starting",
                    )),
                },
                _ = sleep_until(transition_deadline.unwrap_or(Instant::now() + Duration::from_secs(30))),
                    if transition_deadline.is_some() => return Err(io::Error::new(
                        io::ErrorKind::TimedOut, "current backend configuration timed out during startup",
                    )),
            }
        }
    }

    /// Whether `name` runs the client's protocol or one Rift translates to.
    fn serves<C: AsyncRead + AsyncWrite + Unpin>(
        &self,
        name: &str,
        session: &Session<C, TcpStream>,
    ) -> bool {
        let Ok(client) = session.version() else {
            return false;
        };
        self.snapshot
            .config
            .backends
            .get(name)
            .and_then(|backend| backend.version())
            .is_none_or(|backend| rift::protocol::translatable(client, backend))
    }

    pub fn initial_candidates(
        &self,
        primary: &str,
        use_initial: bool,
        name: &str,
        uuid: Option<rift_permissions::Uuid>,
    ) -> io::Result<Vec<String>> {
        let config = &self.snapshot.config;
        if !use_initial && !config.can_access(primary, name) {
            return Err(rift::text::notice_error(
                io::ErrorKind::PermissionDenied,
                Notice::LoginNoAccess,
            ));
        }
        if !use_initial && !self.snapshot.dev_admits(primary, uuid) {
            return Err(rift::text::notice_error(
                io::ErrorKind::PermissionDenied,
                Notice::LoginDevMode,
            ));
        }
        let candidates = if use_initial {
            config.network.initial.clone()
        } else {
            std::iter::once(primary.to_owned())
                .chain(config.fallbacks.get(primary).into_iter().flatten().cloned())
                .chain(
                    config
                        .instances
                        .get(primary)
                        .and_then(|instance| config.fallbacks.get(&instance.group))
                        .into_iter()
                        .flatten()
                        .cloned(),
                )
                .collect()
        };
        Ok(self
            .snapshot
            .candidates(&candidates)
            .into_iter()
            .filter(|server| self.snapshot.admits(server, name, uuid))
            .collect())
    }

    pub fn recovery_candidates(&self, current: &str) -> Vec<String> {
        let config = &self.snapshot.config;
        let mut seen = HashSet::from([current.to_owned()]);
        let group_fallbacks = config
            .instances
            .get(current)
            .and_then(|instance| config.fallbacks.get(&instance.group));
        let destinations: Vec<_> = config
            .network
            .hubs
            .iter()
            .chain(config.fallbacks.get(current).into_iter().flatten())
            .chain(group_fallbacks.into_iter().flatten())
            .filter(|server| seen.insert((*server).clone()))
            .cloned()
            .collect();
        self.snapshot
            .candidates(&destinations)
            .into_iter()
            .filter(|name| name != current)
            .collect()
    }

    /// A TCP accept alone does not establish a usable attachment. A failed
    /// handshake/Login Start write can retry too: only the failed backend was
    /// written, and the client's cached login and codec remain owned by Session.
    pub async fn connect_initial<C: AsyncRead + AsyncWrite + Unpin>(
        &self,
        session: &mut Session<C, TcpStream>,
        candidates: &[String],
        deadline: &mut Instant,
        event: &mut Connection,
    ) -> io::Result<String> {
        let names: Vec<_> = candidates
            .iter()
            .filter(|name| self.serves(name, session))
            .map(String::as_str)
            .collect();
        let mut next = 0;
        if names.is_empty() && !candidates.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "No server on this network supports your Minecraft version.",
            ));
        }
        if names.is_empty() {
            return Err(io::Error::other("No initial server is available."));
        }
        loop {
            let selected = names[next];
            if let Some(extension) = &session.extension
                && let Err(error) = extension.reserve(selected)
            {
                next += 1;
                if next == names.len() {
                    return Err(error);
                }
                continue;
            }
            let wake_started = Instant::now();
            let prepared = self.prepare_backend(selected).await;
            // Cold starts have their own configured deadline; preserve the
            // existing shared TCP/login budget across fallback attempts.
            *deadline += wake_started.elapsed();
            let lease = match prepared {
                Ok(lease) => lease,
                Err(error) => {
                    if let Some(extension) = &session.extension {
                        extension.release(selected);
                    }
                    next += 1;
                    if next == names.len() {
                        return Err(error);
                    }
                    continue;
                }
            };
            let connected = health::connect_candidates(
                &self.snapshot.config,
                &self.snapshot.health,
                &names[next..next + 1],
                self.addresses,
                self.metrics,
                Instant::now()
                    + deadline.saturating_duration_since(Instant::now())
                        / (names.len() - next) as u32,
                event,
            )
            .await;
            let upstream = match connected {
                Ok(upstream) => upstream,
                Err(error) => {
                    if let Some(extension) = &session.extension {
                        extension.release(selected);
                    }
                    next += 1;
                    if next == names.len() {
                        return Err(error);
                    }
                    continue;
                }
            };
            if next > 0 {
                self.metrics.fallbacks.inc();
            }
            let current = event.backend.clone().expect("connected backend");
            let index = names.iter().position(|name| *name == current).unwrap();
            next = index + 1;
            let budget =
                deadline.saturating_duration_since(Instant::now()) / (names.len() - index) as u32;
            let result = async {
                event.stage = "backend_setup";
                event.failure = "io_error";
                upstream.set_nodelay(true)?;
                event.stage = "handshake_write";
                self.push_permissions(session, &current);
                let protocol = self
                    .snapshot
                    .config
                    .backends
                    .get(&current)
                    .and_then(|b| b.version());
                timeout_at(
                    Instant::now() + budget,
                    session.connect_backend_as(upstream, protocol),
                )
                .await?
            }
            .await;
            match result {
                Ok(()) => {
                    event.hold_managed_backend(&current, lease);
                    return Ok(current);
                }
                Err(error) => {
                    if let Some(extension) = &session.extension {
                        extension.release(&current);
                    }
                    event.emit("backend_attempt_failed", &error, Some(error.kind()));
                    if !session.reset_initial_backend() || next == names.len() {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Preflight each candidate independently. Once a client transition starts,
    /// any failure is terminal: retrying a possibly partial write is unsafe.
    /// Explicit backend rejections are never interpreted as outages.
    pub async fn switch<C: AsyncRead + AsyncWrite + Unpin>(
        &self,
        session: &mut Session<C, TcpStream>,
        current: &str,
        candidates: &[String],
        event: &mut Connection,
    ) -> io::Result<String> {
        self.switch_for(session, current, candidates, event, "recovery")
            .await
    }

    pub async fn switch_for<C: AsyncRead + AsyncWrite + Unpin>(
        &self,
        session: &mut Session<C, TcpStream>,
        current: &str,
        candidates: &[String],
        event: &mut Connection,
        reason: &str,
    ) -> io::Result<String> {
        if !session.can_switch() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Server switching requires a switchable Minecraft version and a player in the world.",
            ));
        }
        let identity = session
            .identity()
            .ok_or_else(|| io::Error::other("missing player identity"))?;
        let config = &self.snapshot.config;
        let uuid = expected_uuid(session);
        let expanded = self.snapshot.candidates(candidates);
        let candidates: Vec<_> = expanded
            .iter()
            .filter(|server| {
                server.as_str() != current
                    && self.snapshot.admits(server, &identity.name, uuid)
                    && self.snapshot.reachable(server)
                    && self.serves(server, session)
            })
            .collect();
        let mut deadline = Instant::now() + config.limits.connect_timeout;
        let mut last_error =
            rift::text::notice_error(io::ErrorKind::NotConnected, Notice::SwitchNoServer);
        for (index, name) in candidates.iter().enumerate() {
            if let Some(extension) = &mut session.extension
                && let Err(error) = extension.before_transfer(name, reason).await
            {
                last_error = error;
                continue;
            }
            let wake_started = Instant::now();
            let prepared = self.prepare_transfer(session, current, name, event).await;
            deadline += wake_started.elapsed();
            let lease = match prepared {
                Ok(lease) => lease,
                Err(error) => {
                    if let Some(extension) = &mut session.extension {
                        extension.transfer_failed().await;
                    }
                    if error.kind() == io::ErrorKind::PermissionDenied || !session.can_switch() {
                        return Err(error);
                    }
                    last_error = error;
                    continue;
                }
            };
            let budget = deadline.saturating_duration_since(Instant::now())
                / (candidates.len() - index) as u32;
            let attempt_deadline = Instant::now() + budget;
            let result = async {
                let upstream = health::connect_candidates(
                    config,
                    &self.snapshot.health,
                    &[name.as_str()],
                    self.addresses,
                    self.metrics,
                    attempt_deadline,
                    event,
                )
                .await?;
                upstream.set_nodelay(true)?;
                event.stage = "backend_switch";
                self.push_permissions(session, name);
                let protocol = config.backends.get(name.as_str()).and_then(|b| b.version());
                match timeout_at(
                    attempt_deadline,
                    session.connect_backend_as(upstream, protocol),
                )
                .await
                {
                    Ok(result) => result,
                    Err(error) => Err(error.into()),
                }
            }
            .await;
            match result {
                Ok(()) => {
                    event.hold_managed_backend(name, lease);
                    return Ok((*name).clone());
                }
                Err(error) => {
                    if let Some(extension) = &mut session.extension {
                        extension.transfer_failed().await;
                    }
                    event.emit("backend_switch_failed", &error, Some(error.kind()));
                    if error.kind() == io::ErrorKind::PermissionDenied
                        || session.switch_in_progress()
                    {
                        return Err(error);
                    }
                    last_error = error;
                }
            }
        }
        Err(last_error)
    }

    /// Destinations `/server` would accept from this player, other than the
    /// current one, for suggestions.
    pub fn joinable<C: AsyncRead + AsyncWrite + Unpin>(
        &self,
        session: &Session<C, TcpStream>,
        current: &str,
    ) -> Vec<String> {
        let config = &self.snapshot.config;
        let Some(identity) = session.identity() else {
            return Vec::new();
        };
        let uuid = expected_uuid(session);
        config
            .backends
            .keys()
            .chain(config.service_groups.keys())
            .filter(|name| *name != current && self.snapshot.admits(name, &identity.name, uuid))
            .cloned()
            .collect()
    }

    pub async fn command<C: AsyncRead + AsyncWrite + Unpin>(
        &self,
        session: &mut Session<C, TcpStream>,
        current: &str,
        command: &str,
        event: &mut Connection,
    ) -> io::Result<Option<String>> {
        let config = &self.snapshot.config;
        let identity = session
            .identity()
            .ok_or_else(|| io::Error::other("missing player identity"))?;
        let uuid = expected_uuid(session);
        let admits = |server: &str| self.snapshot.admits(server, &identity.name, uuid);
        let words: Vec<_> = command.split_whitespace().collect();
        let extension_action = if let Some(extension) = &session.extension
            && words.first().is_some_and(|name| extension.intercepts(name))
        {
            match extension.command(command).await {
                Ok(action) => Some(action),
                Err(error) => {
                    eprintln!("rift: {error}");
                    Some(rift::extensions::Action::Message(catalog::notice(
                        Notice::CommandFailed,
                    )))
                }
            }
        } else {
            None
        };
        use rift::extensions::Action;
        let (candidates, message) = if let Some(action) = extension_action {
            match action {
                Action::Server(target) => (vec![target], None),
                Action::Message(message) => (Vec::new(), Some(message)),
                Action::Queue(target) => {
                    let message = if !admits(&target) {
                        catalog::notice(Notice::QueueNoAccess)
                    } else {
                        match session.extension.as_ref().unwrap().enqueue(&target) {
                            Ok(position) => catalog::notice_with(
                                Notice::QueueJoined,
                                &Args::new()
                                    .text("server", target.as_str())
                                    .text("position", position.to_string()),
                            ),
                            Err(error) => rift::text::reason(&error),
                        }
                    };
                    (Vec::new(), Some(message))
                }
                Action::LeaveQueue => {
                    session.extension.as_ref().unwrap().leave_queue();
                    (Vec::new(), Some(catalog::notice(Notice::QueueLeft)))
                }
                Action::Continue => return Ok(None),
                Action::Deny(_) => unreachable!(),
            }
        } else {
            match words.as_slice() {
                ["server"] => {
                    let servers: Vec<_> = config
                        .backends
                        .keys()
                        .filter(|server| admits(server))
                        .map(String::as_str)
                        .collect();
                    (
                        Vec::new(),
                        Some(catalog::notice_with(
                            Notice::ServerList,
                            &Args::new()
                                .text("current", current)
                                .text("servers", servers.join(", ")),
                        )),
                    )
                }
                ["server", target] if *target == current => (
                    Vec::new(),
                    Some(catalog::notice_with(
                        Notice::ServerAlreadyOn,
                        &Args::new().text("current", current),
                    )),
                ),
                ["server", target] if !config.is_destination(target) || !admits(target) => {
                    (Vec::new(), Some(catalog::notice(Notice::ServerUnavailable)))
                }
                ["server", target] => (vec![(*target).to_owned()], None),
                ["hub"] if config.network.hubs.iter().any(|hub| hub == current) => {
                    (Vec::new(), Some(catalog::notice(Notice::HubAlready)))
                }
                ["hub"] => (config.network.hubs.clone(), None),
                _ => (Vec::new(), Some(catalog::notice(Notice::ServerUsage))),
            }
        };
        if let Some(message) = message {
            session.send_text(&message).await?;
            return Ok(None);
        }
        match self
            .switch_for(session, current, &candidates, event, "command")
            .await
        {
            Ok(target) => Ok(Some(target)),
            Err(error)
                if !session.switch_in_progress()
                    && session.backend().is_some()
                    && session.can_switch() =>
            {
                // The old attachment remains usable after preflight failures,
                // including a target's ban or whitelist rejection.
                let reason = rift::text::reason(&error);
                let reason = catalog::notice_with(
                    Notice::SwitchFailed,
                    &Args::new().component(
                        "reason",
                        reason.for_client(rift::text::Profile::UNKNOWN).into_owned(),
                    ),
                );
                session.send_text(&reason).await?;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests;
