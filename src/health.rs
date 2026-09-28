use rift::config::{Config, HealthCheck};
use std::{collections::BTreeMap, io, net::SocketAddr, sync::Mutex};
use tokio::{
    net::TcpStream,
    time::{Instant, timeout},
};

#[derive(Clone, Copy)]
struct State {
    up: bool,
    successes: usize,
    failures: usize,
}

pub struct Health {
    settings: Option<HealthCheck>,
    states: Mutex<BTreeMap<String, State>>,
}

impl Health {
    pub fn new(config: &Config) -> Self {
        Self {
            settings: config.health_check,
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
        self.states.lock().unwrap()[name].up
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
        .filter(|name| health.available(name))
        .collect();
    let mut last_error =
        io::Error::new(io::ErrorKind::NotConnected, "no healthy backend available");
    for (index, name) in candidates.iter().enumerate() {
        // Reserve time for every fallback even when a primary silently drops SYNs.
        let budget =
            deadline.saturating_duration_since(Instant::now()) / (candidates.len() - index) as u32;
        match timeout(budget, config.backends[*name].connect(listeners)).await {
            Ok(Ok(stream)) => {
                health.record(name, true);
                if *name != primary {
                    metrics.fallbacks.inc();
                }
                return Ok(stream);
            }
            result => {
                health.record(name, false);
                metrics.backend_failures.inc();
                last_error = match result {
                    Ok(Err(error)) => error,
                    Err(error) => error.into(),
                    Ok(Ok(_)) => unreachable!(),
                };
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
}

// Linux's finite accept backlog gives us a local, deterministic stalled TCP
// connect without relying on an unroutable Internet address or firewall rules.
#[cfg(all(test, target_os = "linux"))]
mod timeout_tests {
    use super::*;
    use crate::metrics::Metrics;
    use std::time::Duration;
    use tokio::net::{TcpListener, TcpSocket};

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
        assert!(
            connect(
                &config,
                &health,
                "default",
                &[],
                &metrics,
                Instant::now() + Duration::from_millis(20)
            )
            .await
            .is_err()
        );
        assert_eq!(metrics.backend_failures.get(), 2);
        assert!(!health.available("default"));
    }
}
