use std::{
    collections::BTreeMap,
    io,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

#[derive(Default)]
pub struct Counter(AtomicU64);
impl Counter {
    pub fn inc(&self) {
        self.add(1);
    }
    pub fn add(&self, value: u64) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
pub struct Metrics {
    pub accepted: Counter,
    pub active: Counter,
    pub players: Counter,
    pub completed: Counter,
    pub rate_rejected: Counter,
    pub login_rejected: Counter,
    pub access_rejected: Counter,
    pub capacity_rejected: Counter,
    pub route_rejected: Counter,
    pub errors: Counter,
    pub backend_failures: Counter,
    pub fallbacks: Counter,
    pub cache_hits: Counter,
    pub cache_misses: Counter,
    pub reloads: Counter,
    pub reload_failures: Counter,
    pub health_checks: Counter,
    pub health_failures: Counter,
    pub sent: Counter,
    pub received: Counter,
    pub forced_shutdowns: Counter,
    pub transfers: Counter,
    pub transfer_failures: Counter,
    login_latency: Mutex<LoginLatency>,
    backend_players: Mutex<BTreeMap<String, u64>>,
}

const LOGIN_BUCKETS_MS: [u64; 12] = [5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000];

#[derive(Default)]
struct LoginLatency {
    buckets: [u64; LOGIN_BUCKETS_MS.len()],
    count: u64,
    seconds: f64,
}

pub struct Active(Arc<Metrics>);
impl Active {
    pub fn new(metrics: Arc<Metrics>, maximum: usize) -> Option<Self> {
        metrics
            .active
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < maximum as u64).then_some(n + 1)
            })
            .ok()?;
        Some(Self(metrics))
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.0.active.0.fetch_sub(1, Ordering::Relaxed);
        self.0.completed.inc();
    }
}

/// Counts logged-in sessions, excluding status probes and pending handshakes.
pub struct Player {
    metrics: Arc<Metrics>,
    backend: Option<String>,
}
impl Player {
    pub fn new(metrics: Arc<Metrics>) -> Self {
        metrics.players.inc();
        Self {
            metrics,
            backend: None,
        }
    }

    pub fn new_on_backend(metrics: Arc<Metrics>, backend: String) -> Self {
        let mut player = Self::new(metrics);
        player.move_backend(backend);
        player
    }

    /// Update only after a transfer succeeds. The total player count is unchanged.
    pub fn move_backend(&mut self, backend: String) {
        if self.backend.as_ref() == Some(&backend) {
            return;
        }
        let mut players = self.metrics.backend_players.lock().unwrap();
        if let Some(previous) = &self.backend {
            remove_backend_player(&mut players, previous);
        }
        *players.entry(backend.clone()).or_default() += 1;
        self.backend = Some(backend);
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.metrics.players.0.fetch_sub(1, Ordering::Relaxed);
        if let Some(backend) = &self.backend {
            remove_backend_player(&mut self.metrics.backend_players.lock().unwrap(), backend);
        }
    }
}

fn remove_backend_player(players: &mut BTreeMap<String, u64>, backend: &str) {
    if let Some(count) = players.get_mut(backend) {
        *count -= 1;
        if *count == 0 {
            // Old backend labels survive reload only while a session uses them.
            players.remove(backend);
        }
    }
}

impl Metrics {
    /// Record successful logins once, when the session first reaches play.
    pub fn observe_login(&self, duration: Duration) {
        let mut latency = self.login_latency.lock().unwrap();
        latency.count += 1;
        latency.seconds += duration.as_secs_f64();
        for (index, upper) in LOGIN_BUCKETS_MS.iter().enumerate() {
            if duration <= Duration::from_millis(*upper) {
                latency.buckets[index] += 1;
            }
        }
    }

    pub fn values(&self) -> serde_json::Value {
        serde_json::json!({
            "accepted": self.accepted.get(),
            "active": self.active.get(),
            "players": self.players.get(),
            "completed": self.completed.get(),
            "rate_rejected": self.rate_rejected.get(),
            "login_rejected": self.login_rejected.get(),
            "access_rejected": self.access_rejected.get(),
            "capacity_rejected": self.capacity_rejected.get(),
            "route_rejected": self.route_rejected.get(),
            "errors": self.errors.get(),
            "backend_failures": self.backend_failures.get(),
            "fallbacks": self.fallbacks.get(),
            "cache_hits": self.cache_hits.get(),
            "cache_misses": self.cache_misses.get(),
            "reloads": self.reloads.get(),
            "reload_failures": self.reload_failures.get(),
            "health_checks": self.health_checks.get(),
            "health_failures": self.health_failures.get(),
            "sent": self.sent.get(),
            "received": self.received.get(),
            "forced_shutdowns": self.forced_shutdowns.get(),
            "transfers": self.transfers.get(),
            "transfer_failures": self.transfer_failures.get(),
        })
    }

    pub fn render(&self, snapshot: &crate::runtime::Snapshot) -> String {
        let mut text = String::new();
        for (name, kind, value) in [
            ("connections_accepted_total", "counter", self.accepted.get()),
            ("connections_active", "gauge", self.active.get()),
            ("players_online", "gauge", self.players.get()),
            (
                "connections_completed_total",
                "counter",
                self.completed.get(),
            ),
            (
                "connections_rate_limited_total",
                "counter",
                self.rate_rejected.get(),
            ),
            (
                "logins_rate_limited_total",
                "counter",
                self.login_rejected.get(),
            ),
            (
                "connections_access_rejected_total",
                "counter",
                self.access_rejected.get(),
            ),
            (
                "connections_capacity_rejected_total",
                "counter",
                self.capacity_rejected.get(),
            ),
            (
                "connections_route_rejected_total",
                "counter",
                self.route_rejected.get(),
            ),
            ("connection_errors_total", "counter", self.errors.get()),
            (
                "backend_connect_failures_total",
                "counter",
                self.backend_failures.get(),
            ),
            ("fallbacks_total", "counter", self.fallbacks.get()),
            ("status_cache_hits_total", "counter", self.cache_hits.get()),
            (
                "status_cache_misses_total",
                "counter",
                self.cache_misses.get(),
            ),
            ("reloads_total", "counter", self.reloads.get()),
            (
                "reload_failures_total",
                "counter",
                self.reload_failures.get(),
            ),
            ("health_checks_total", "counter", self.health_checks.get()),
            (
                "health_check_failures_total",
                "counter",
                self.health_failures.get(),
            ),
            ("client_bytes_read_total", "counter", self.sent.get()),
            ("client_bytes_written_total", "counter", self.received.get()),
            (
                "forced_shutdowns_total",
                "counter",
                self.forced_shutdowns.get(),
            ),
            ("player_transfers_total", "counter", self.transfers.get()),
            (
                "player_transfer_failures_total",
                "counter",
                self.transfer_failures.get(),
            ),
        ] {
            text.push_str(&format!("# TYPE rift_{name} {kind}\nrift_{name} {value}\n"));
        }
        text.push_str("# TYPE rift_backend_up gauge\n");
        for (name, up) in snapshot.health.metrics() {
            let name = escape_label(&name);
            text.push_str(&format!(
                "rift_backend_up{{backend=\"{name}\"}} {}\n",
                u8::from(up)
            ));
        }
        text.push_str(&format!(
            "# TYPE rift_maintenance_mode gauge\nrift_maintenance_mode {}\n",
            u8::from(snapshot.control.maintenance(&snapshot.config))
        ));
        text.push_str("# TYPE rift_backend_draining gauge\n");
        for backend in snapshot.config.backends.keys() {
            text.push_str(&format!(
                "rift_backend_draining{{backend=\"{}\"}} {}\n",
                escape_label(backend),
                u8::from(snapshot.control.draining(&snapshot.config, backend))
            ));
        }
        text.push_str("# TYPE rift_backend_players_online gauge\n");
        let mut players: BTreeMap<_, _> = snapshot
            .config
            .backends
            .keys()
            .map(|name| (name.clone(), 0))
            .collect();
        players.extend(self.backend_players.lock().unwrap().clone());
        for (backend, count) in players {
            text.push_str(&format!(
                "rift_backend_players_online{{backend=\"{}\"}} {count}\n",
                escape_label(&backend)
            ));
        }
        {
            let latency = self.login_latency.lock().unwrap();
            text.push_str("# TYPE rift_login_duration_seconds histogram\n");
            for (upper, count) in LOGIN_BUCKETS_MS.iter().zip(latency.buckets) {
                text.push_str(&format!(
                    "rift_login_duration_seconds_bucket{{le=\"{}\"}} {count}\n",
                    *upper as f64 / 1000.0
                ));
            }
            text.push_str(&format!("rift_login_duration_seconds_bucket{{le=\"+Inf\"}} {}\nrift_login_duration_seconds_sum {}\nrift_login_duration_seconds_count {}\n", latency.count, latency.seconds, latency.count));
        }
        let resident = resident_memory_bytes();
        text.push_str(&format!("# TYPE rift_process_resident_memory_available gauge\nrift_process_resident_memory_available {}\n", u8::from(resident.is_some())));
        if let Some(bytes) = resident {
            text.push_str(&format!("# TYPE rift_process_resident_memory_bytes gauge\nrift_process_resident_memory_bytes {bytes}\n"));
        }
        text
    }
}

fn escape_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

/// Linux reports the process resident set in KiB. Other systems, or Linux
/// without a readable procfs, expose availability=0 and omit the byte gauge.
fn resident_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        parse_resident_memory(&std::fs::read_to_string("/proc/self/status").ok()?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_resident_memory(status: &str) -> Option<u64> {
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))?;
    let mut fields = line.split_whitespace();
    let value = fields.next()?.parse::<u64>().ok()?;
    if fields.next()? != "kB" || fields.next().is_some() {
        return None;
    }
    value.checked_mul(1024)
}

// Count bytes as traffic flows, including partial sessions that later fail.
// The only hot-path shared operations are relaxed atomic additions per I/O.
pub struct Metered<'a> {
    pub stream: &'a mut TcpStream,
    metrics: Arc<Metrics>,
    pub sent: u64,
    pub received: u64,
}
impl<'a> Metered<'a> {
    pub fn new(stream: &'a mut TcpStream, metrics: Arc<Metrics>) -> Self {
        Self {
            stream,
            metrics,
            sent: 0,
            received: 0,
        }
    }
}
impl AsyncRead for Metered<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buffer);
        let count = (buffer.filled().len() - before) as u64;
        self.sent += count;
        self.metrics.sent.add(count);
        result
    }
}
impl AsyncWrite for Metered<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buffer);
        if let Poll::Ready(Ok(count)) = result {
            self.received += count as u64;
            self.metrics.received.add(count as u64);
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Snapshot;
    use rift::config::Config;

    #[test]
    fn login_histogram_is_cumulative_and_survives_config_generations() {
        let metrics = Metrics::default();
        for duration in [
            Duration::ZERO,
            Duration::from_millis(5),
            Duration::from_millis(6),
            Duration::from_secs(60),
        ] {
            metrics.observe_login(duration);
        }
        metrics.accepted.add(7);
        let original = Snapshot::new(Config::default(), None).unwrap();
        let replacement = Snapshot::new(Config::default(), Some(&original)).unwrap();
        for snapshot in [&original, &replacement] {
            let text = metrics.render(snapshot);
            assert!(text.contains("# TYPE rift_login_duration_seconds histogram\n"));
            assert!(text.contains("rift_login_duration_seconds_bucket{le=\"0.005\"} 2\n"));
            assert!(text.contains("rift_login_duration_seconds_bucket{le=\"0.01\"} 3\n"));
            assert!(text.contains("rift_login_duration_seconds_bucket{le=\"30\"} 3\n"));
            assert!(text.contains("rift_login_duration_seconds_bucket{le=\"+Inf\"} 4\n"));
            assert!(text.contains("rift_login_duration_seconds_count 4\n"));
            assert!(text.contains("rift_connections_accepted_total 7\n"));
            let sum: f64 = text
                .lines()
                .find_map(|line| line.strip_prefix("rift_login_duration_seconds_sum "))
                .unwrap()
                .parse()
                .unwrap();
            assert!((sum - 60.011).abs() < 1e-10);
        }
    }

    #[test]
    fn player_guards_track_transfers_and_release_removed_backend_labels() {
        let metrics = Arc::new(Metrics::default());
        let snapshot = Snapshot::new(Config::default(), None).unwrap();
        let mut first = Player::new_on_backend(metrics.clone(), "retired".into());
        let second = Player::new_on_backend(metrics.clone(), "retired".into());
        assert_eq!(metrics.players.get(), 2);
        assert!(
            metrics
                .render(&snapshot)
                .contains("rift_backend_players_online{backend=\"retired\"} 2\n")
        );
        first.move_backend("default".into());
        first.move_backend("default".into());
        assert_eq!(metrics.players.get(), 2);
        let text = metrics.render(&snapshot);
        assert!(text.contains("rift_backend_players_online{backend=\"retired\"} 1\n"));
        assert!(text.contains("rift_backend_players_online{backend=\"default\"} 1\n"));
        drop(second);
        assert!(!metrics.render(&snapshot).contains("backend=\"retired\""));
        drop(first);
        assert_eq!(metrics.players.get(), 0);
        assert!(metrics.backend_players.lock().unwrap().is_empty());
        assert!(
            metrics
                .render(&snapshot)
                .contains("rift_backend_players_online{backend=\"default\"} 0\n")
        );
    }

    #[test]
    fn player_labels_escape_prometheus_control_characters() {
        let metrics = Arc::new(Metrics::default());
        let snapshot = Snapshot::new(Config::default(), None).unwrap();
        let _player = Player::new_on_backend(metrics.clone(), "quote\"slash\\newline\n".into());
        let text = metrics.render(&snapshot);
        assert!(
            text.contains(
                "rift_backend_players_online{backend=\"quote\\\"slash\\\\newline\\n\"} 1\n"
            )
        );
    }

    #[test]
    fn maintenance_and_draining_are_distinct_from_backend_health() {
        let metrics = Metrics::default();
        let config = Config {
            maintenance: true,
            draining: ["default".into()].into(),
            ..Config::default()
        };
        let snapshot = Snapshot::new(config, None).unwrap();
        let text = metrics.render(&snapshot);
        assert!(text.contains("rift_maintenance_mode 1\n"));
        assert!(text.contains("rift_backend_draining{backend=\"default\"} 1\n"));
        assert!(text.contains("rift_backend_up{backend=\"default\"} 1\n"));
    }

    #[test]
    fn connection_guard_enforces_capacity_and_counts_completion() {
        let metrics = Arc::new(Metrics::default());
        let active = Active::new(metrics.clone(), 1).unwrap();
        assert_eq!(metrics.active.get(), 1);
        assert!(Active::new(metrics.clone(), 1).is_none());
        assert_eq!(metrics.completed.get(), 0);
        drop(active);
        assert_eq!(metrics.active.get(), 0);
        assert_eq!(metrics.completed.get(), 1);
        assert!(Active::new(metrics.clone(), 1).is_some());
    }

    #[test]
    fn procfs_memory_parser_checks_units_and_overflow() {
        assert_eq!(
            parse_resident_memory("Name:\trift\nVmRSS:\t 1234 kB\nVmSwap:\t0 kB\n"),
            Some(1234 * 1024)
        );
        for invalid in [
            "VmSwap: 1234 kB\n",
            "VmRSS: unknown kB\n",
            "VmRSS: 1234 MB\n",
            "VmRSS: 1234 kB extra\n",
            "VmRSS: 18446744073709551615 kB\n",
        ] {
            assert_eq!(parse_resident_memory(invalid), None);
        }
    }
}
