//! Policy and bounded attachment attempts shared by commands and outage recovery.
use crate::{events::Connection, health, metrics::Metrics, runtime::Snapshot};
use rift::{protocol::State, session::Session};
use std::{collections::HashSet, io, net::SocketAddr};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    time::{Instant, timeout_at},
};

pub struct Network<'a> {
    pub snapshot: &'a Snapshot,
    pub addresses: &'a [SocketAddr],
    pub metrics: &'a Metrics,
}

impl Network<'_> {
    pub fn initial_candidates(
        &self,
        primary: &str,
        use_initial: bool,
        name: &str,
    ) -> io::Result<Vec<String>> {
        let config = &self.snapshot.config;
        if !use_initial && !config.can_access(primary, name) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "You do not have access to this server.",
            ));
        }
        let candidates = if use_initial {
            config.network.initial.clone()
        } else {
            std::iter::once(primary.to_owned())
                .chain(config.fallbacks.get(primary).into_iter().flatten().cloned())
                .collect()
        };
        Ok(candidates
            .into_iter()
            .filter(|server| config.can_access(server, name))
            .collect())
    }

    pub fn recovery_candidates(&self, current: &str) -> Vec<String> {
        let config = &self.snapshot.config;
        let mut seen = HashSet::from([current.to_owned()]);
        config
            .network
            .hubs
            .iter()
            .chain(config.fallbacks.get(current).into_iter().flatten())
            .filter(|server| seen.insert((*server).clone()))
            .cloned()
            .collect()
    }

    /// A TCP accept alone does not establish a usable attachment. A failed
    /// handshake/Login Start write can retry too: only the failed backend was
    /// written, and the client's cached login and codec remain owned by Session.
    pub async fn connect_initial<C: AsyncRead + AsyncWrite + Unpin>(
        &self,
        session: &mut Session<C, TcpStream>,
        candidates: &[String],
        deadline: Instant,
        event: &mut Connection,
    ) -> io::Result<String> {
        let names: Vec<_> = candidates.iter().map(String::as_str).collect();
        let mut next = 0;
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
                timeout_at(Instant::now() + budget, session.connect_backend(upstream)).await?
            }
            .await;
            match result {
                Ok(()) => return Ok(current),
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
        let candidates: Vec<_> = candidates
            .iter()
            .filter(|server| {
                server.as_str() != current
                    && config.can_access(server, &identity.name)
                    && self.snapshot.health.available(server)
            })
            .collect();
        let deadline = Instant::now() + config.limits.connect_timeout;
        let mut last_error = io::Error::new(
            io::ErrorKind::NotConnected,
            "No available server could be reached.",
        );
        for (index, name) in candidates.iter().enumerate() {
            if let Some(extension) = &mut session.extension
                && let Err(error) = extension.before_transfer(name, reason).await
            {
                last_error = error;
                continue;
            }
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
                match timeout_at(attempt_deadline, session.connect_backend(upstream)).await {
                    Ok(result) => result,
                    Err(error) => Err(error.into()),
                }
            }
            .await;
            match result {
                Ok(()) => return Ok((*name).clone()),
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
        let words: Vec<_> = command.split_whitespace().collect();
        let extension_action = if let Some(extension) = &session.extension
            && words.first().is_some_and(|name| {
                extension
                    .commands()
                    .iter()
                    .any(|registered| registered == name)
            }) {
            match extension.command(command).await {
                Ok(action) => Some(action),
                Err(error) => {
                    eprintln!("rift: {error}");
                    Some(rift::extensions::Action::Message(
                        "Extension command failed.".into(),
                    ))
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
                    let message = if !config.can_access(&target, &identity.name) {
                        "You do not have access to that server.".into()
                    } else {
                        match session.extension.as_ref().unwrap().enqueue(&target) {
                            Ok(position) => format!("Queued for {target}. Position: {position}."),
                            Err(error) => error.to_string(),
                        }
                    };
                    (Vec::new(), Some(message))
                }
                Action::LeaveQueue => {
                    session.extension.as_ref().unwrap().leave_queue();
                    (Vec::new(), Some("Left the queue.".into()))
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
                        .filter(|server| config.can_access(server, &identity.name))
                        .map(String::as_str)
                        .collect();
                    (
                        Vec::new(),
                        Some(format!(
                            "Current server: {current}. Available servers: {}",
                            servers.join(", ")
                        )),
                    )
                }
                ["server", target] if *target == current => {
                    (Vec::new(), Some(format!("You are already on {current}.")))
                }
                ["server", target]
                    if !config.backends.contains_key(*target)
                        || !config.can_access(target, &identity.name) =>
                {
                    (
                        Vec::new(),
                        Some("Server unavailable or you do not have access to it.".into()),
                    )
                }
                ["server", target] => (vec![(*target).to_owned()], None),
                ["hub"] if config.network.hubs.iter().any(|hub| hub == current) => {
                    (Vec::new(), Some("You are already in a hub.".into()))
                }
                ["hub"] => (config.network.hubs.clone(), None),
                _ => (Vec::new(), Some("Usage: /server [name] or /hub".into())),
            }
        };
        if let Some(message) = message {
            session.send_system_message(&message).await?;
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
                    && session.client.state.settled(State::Play) =>
            {
                // The old attachment remains usable after preflight failures,
                // including a target's ban or whitelist rejection.
                session
                    .send_system_message(&format!("Unable to switch servers: {error}"))
                    .await?;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests;
