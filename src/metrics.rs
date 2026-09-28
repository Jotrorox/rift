use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
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
pub struct Player(Arc<Metrics>);
impl Player {
    pub fn new(metrics: Arc<Metrics>) -> Self {
        metrics.players.inc();
        Self(metrics)
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.0.players.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Metrics {
    pub fn values(&self) -> serde_json::Value {
        serde_json::json!({
            "accepted": self.accepted.get(),
            "active": self.active.get(),
            "players": self.players.get(),
            "completed": self.completed.get(),
            "rate_rejected": self.rate_rejected.get(),
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
        ] {
            text.push_str(&format!("# TYPE rift_{name} {kind}\nrift_{name} {value}\n"));
        }
        text.push_str("# TYPE rift_backend_up gauge\n");
        for (name, up) in snapshot.health.metrics() {
            let name = name
                .replace('\\', "\\\\")
                .replace('\n', "\\n")
                .replace('"', "\\\"");
            text.push_str(&format!(
                "rift_backend_up{{backend=\"{name}\"}} {}\n",
                u8::from(up)
            ));
        }
        text
    }
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
