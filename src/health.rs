use rift::{
    config::{Config, HealthCheck},
    routing::ConnectStage,
};
use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{net::TcpStream, time::Instant};

#[derive(Clone, Copy)]
struct State {
    up: bool,
    successes: usize,
    failures: usize,
}

pub struct Health {
    settings: Option<HealthCheck>,
    control: Arc<crate::admin::Control>,
    states: Mutex<BTreeMap<String, State>>,
    retired: AtomicBool,
}

impl Health {
    #[cfg(test)]
    pub fn new(config: &Config) -> Self {
        Self::with_control(config, Arc::default())
    }

    pub fn with_control(config: &Config, control: Arc<crate::admin::Control>) -> Self {
        Self {
            control,
            settings: config.health_check,
            retired: AtomicBool::new(false),
            states: Mutex::new(
                config
                    .backends
                    .keys()
                    .map(|name| {
                        (
                            name.clone(),
                            State {
                                up: true,
                                successes: 0,
                                failures: 0,
                            },
                        )
                    })
                    .collect(),
            ),
        }
    }

    pub fn available(&self, name: &str) -> bool {
        self.retired.load(Ordering::Relaxed) || self.states.lock().unwrap()[name].up
    }

    /// Established sessions retain their configuration after reload, but its
    /// health worker stops. Stale failures must not permanently exclude a
    /// recovered destination: those sessions now use bounded connection attempts.
    pub fn retire(&self) {
        self.retired.store(true, Ordering::Relaxed);
    }

    pub fn record(&self, name: &str, success: bool) {
        let Some(settings) = self.settings else {
            return;
        };
        let mut states = self.states.lock().unwrap();
        let state = states.get_mut(name).expect("configured backend");
        if success {
            state.failures = 0;
            state.successes = state.successes.saturating_add(1);
            if state.successes >= settings.healthy_threshold {
                state.up = true;
            }
        } else {
            state.successes = 0;
            state.failures = state.failures.saturating_add(1);
            if state.failures >= settings.unhealthy_threshold {
                state.up = false;
            }
        }
    }

    pub fn metrics(&self) -> Vec<(String, bool)> {
        self.states
            .lock()
            .unwrap()
            .iter()
            .map(|(n, s)| (n.clone(), s.up))
            .collect()
    }
}

pub async fn connect(
    config: &Config,
    health: &Health,
    primary: &str,
    listeners: &[SocketAddr],
    metrics: &crate::metrics::Metrics,
    deadline: Instant,
    event: &mut crate::events::Connection,
) -> io::Result<TcpStream> {
    let candidates: Vec<&str> = std::iter::once(primary)
        .chain(
            config
                .fallbacks
                .get(primary)
                .into_iter()
                .flatten()
                .map(String::as_str),
        )
        .collect();
    connect_candidates(
        config,
        health,
        &candidates,
        listeners,
        metrics,
        deadline,
        event,
    )
    .await
}

/// Connect only to the supplied, policy-checked candidates, in their given order.
/// In particular this must not expand another backend's fallback list.
pub async fn connect_candidates(
    config: &Config,
    health: &Health,
    candidates: &[&str],
    listeners: &[SocketAddr],
    metrics: &crate::metrics::Metrics,
    deadline: Instant,
    event: &mut crate::events::Connection,
) -> io::Result<TcpStream> {
    event.stage = "connect";
    event.failure = "no_eligible_backend";
    let primary = candidates.first().copied();
    let candidates: Vec<&str> = candidates
        .iter()
        .copied()
        .filter(|name| {
            (config.managed_servers.contains_key(*name) || health.available(name))
                && !health.control.draining(config, name)
        })
        .collect();
    let mut last_error = io::Error::new(
        io::ErrorKind::NotConnected,
        "no eligible backend available: all candidates are unhealthy or draining",
    );
    for (index, name) in candidates.iter().enumerate() {
        if health.control.draining(config, name) {
            continue;
        }
        // Reserve time for every fallback even when a primary silently drops SYNs.
        let budget =
            deadline.saturating_duration_since(Instant::now()) / (candidates.len() - index) as u32;
        event.backend = Some((*name).to_owned());
        event.backend_address = Some(config.backends[*name].address().to_owned());
        match config.backends[*name]
            .connect_until(listeners, Instant::now() + budget)
            .await
        {
            Ok(stream) => {
                if health.control.draining(config, name) {
                    continue;
                }
                health.record(name, true);
                if Some(*name) != primary {
                    metrics.fallbacks.inc();
                }
                return Ok(stream);
            }
            Err(error) => {
                health.record(name, false);
                metrics.backend_failures.inc();
                (event.stage, event.failure) = match error.stage {
                    ConnectStage::Dns => ("dns", "dns_error"),
                    ConnectStage::Connect => ("connect", "connect_error"),
                };
                event.emit(
                    "backend_attempt_failed",
                    &error.error,
                    Some(error.error.kind()),
                );
                last_error = error.error;
            }
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thresholds_and_recovery_are_consecutive() {
        let config = Config {
            health_check: Some(HealthCheck {
                interval: std::time::Duration::from_secs(1),
                timeout: std::time::Duration::from_secs(1),
                unhealthy_threshold: 2,
                healthy_threshold: 2,
            }),
            ..Config::default()
        };
        let health = Health::new(&config);
        health.record("default", false);
        assert!(health.available("default"));
        health.record("default", true);
        health.record("default", false);
        assert!(health.available("default"));
        health.record("default", false);
        assert!(!health.available("default"));
        health.record("default", true);
        assert!(!health.available("default"));
        health.record("default", true);
        assert!(health.available("default"));
    }

    #[test]
    fn retired_health_does_not_strand_players_with_stale_failures() {
        let config = Config {
            health_check: Some(HealthCheck {
                interval: std::time::Duration::from_secs(1),
                timeout: std::time::Duration::from_secs(1),
                unhealthy_threshold: 1,
                healthy_threshold: 1,
            }),
            ..Config::default()
        };
        let health = Health::new(&config);
        health.record("default", false);
        assert!(!health.available("default"));
        health.retire();
        assert!(health.available("default"));
        // Connection failures after the worker stops must remain retryable too.
        health.record("default", false);
        assert!(health.available("default"));
        health.record("default", true);
        assert!(health.available("default"));
        // A replacement configuration still uses ordinary health policy.
        let replacement = Health::new(&config);
        replacement.record("default", false);
        assert!(!replacement.available("default"));
    }
}

// Linux's finite accept backlog gives us a local, deterministic stalled TCP
// connect without relying on an unroutable Internet address or firewall rules.
#[cfg(all(test, target_os = "linux"))]
mod timeout_tests {
    use super::*;
    use crate::metrics::Metrics;
    use std::time::Duration;
    use tokio::{
        net::{TcpListener, TcpSocket},
        time::timeout,
    };

    #[tokio::test]
    async fn stalled_primary_reserves_remaining_deadline_for_fallback_and_records_timeouts() {
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let primary = socket.listen(1).unwrap();
        let address = primary.local_addr().unwrap();
        let mut queued = Vec::new();
        let mut stalled = false;
        for _ in 0..8 {
            match timeout(Duration::from_millis(30), TcpStream::connect(address)).await {
                Ok(stream) => queued.push(stream.unwrap()),
                Err(_) => {
                    stalled = true;
                    break;
                }
            }
        }
        assert!(stalled, "test must fill the primary's accept backlog");
        let backup = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = Config::from_addresses("127.0.0.1:0", &address.to_string()).unwrap();
        config.backends.insert(
            "backup".into(),
            backup.local_addr().unwrap().to_string().parse().unwrap(),
        );
        config
            .fallbacks
            .insert("default".into(), vec!["backup".into()]);
        config.health_check = Some(HealthCheck {
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(1),
            unhealthy_threshold: 1,
            healthy_threshold: 1,
        });
        let health = Health::new(&config);
        let metrics = Metrics::default();
        // The caller has only 200 ms left of its default five-second budget.
        let stream = timeout(
            Duration::from_millis(500),
            connect(
                &config,
                &health,
                "default",
                &[],
                &metrics,
                Instant::now() + Duration::from_millis(200),
                &mut crate::events::Connection::new("default", "127.0.0.1:0".parse().unwrap()),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), backup.local_addr().unwrap());
        assert_eq!(metrics.backend_failures.get(), 1);
        assert_eq!(metrics.fallbacks.get(), 1);
        assert!(!health.available("default"));
        // With no fallback, the final timed-out attempt is still recorded.
        config.fallbacks.clear();
        let health = Health::new(&config);
        let mut event = crate::events::Connection::new("default", "127.0.0.1:0".parse().unwrap());
        assert!(
            connect(
                &config,
                &health,
                "default",
                &[],
                &metrics,
                Instant::now() + Duration::from_millis(20),
                &mut event,
            )
            .await
            .is_err()
        );
        assert_eq!(metrics.backend_failures.get(), 2);
        assert_eq!(event.stage, "connect");
        assert_eq!(event.failure, "connect_error");
        assert_eq!(event.backend.as_deref(), Some("default"));
        assert!(!health.available("default"));
    }
}
