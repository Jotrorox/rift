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
        if !session.can_switch() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Server switching requires Minecraft 1.21.11 and a player in the world.",
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
        let (candidates, message) = match words.as_slice() {
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
        };
        if let Some(message) = message {
            session.send_system_message(&message).await?;
            return Ok(None);
        }
        match self.switch(session, current, &candidates, event).await {
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
