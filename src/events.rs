//! JSON diagnostics for failed connections, backend attempts and administration.
use rift::hooks::RouteError;
use serde_json::{Value, json};
use std::{
    fmt::Display,
    io::{self, Write},
    net::SocketAddr,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// `connection_rejected` lines written per second. A flood of rejected
/// connections (rate limited, over capacity, untrusted proxy) would otherwise
/// write one line each and fill the log; counters still count every one.
const REJECTIONS_PER_SECOND: u32 = 20;

#[derive(Default)]
struct RejectionLog {
    second: Option<Instant>,
    written: u32,
    suppressed: u64,
}

impl RejectionLog {
    /// Whether to write a line now: `Some` with the number of rejections left
    /// out since the last written line.
    fn admit(&mut self, now: Instant) -> Option<u64> {
        if self
            .second
            .is_none_or(|second| now.duration_since(second) >= Duration::from_secs(1))
        {
            self.second = Some(now);
            self.written = 0;
        }
        if self.written >= REJECTIONS_PER_SECOND {
            self.suppressed += 1;
            return None;
        }
        self.written += 1;
        Some(std::mem::take(&mut self.suppressed))
    }
}

pub struct Connection {
    id: u64,
    listener: String,
    peer: SocketAddr,
    started: Instant,
    pub backend: Option<String>,
    pub backend_address: Option<String>,
    pub stage: &'static str,
    pub failure: &'static str,
    // Keep both attachments alive during a transfer until the new world is
    // committed. Dropping the connection releases every outstanding lease.
    managed_leases: std::collections::BTreeMap<String, rift::managed::ManagedLease>,
}

impl Connection {
    pub fn new(listener: &str, peer: SocketAddr) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            listener: listener.to_owned(),
            peer,
            started: Instant::now(),
            backend: None,
            backend_address: None,
            stage: "admission",
            failure: "io_error",
            managed_leases: Default::default(),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// Record the client address reported by a trusted PROXY protocol header.
    pub fn set_peer_addr(&mut self, peer: SocketAddr) {
        self.peer = peer;
    }

    pub fn hold_managed_backend(&mut self, name: &str, lease: Option<rift::managed::ManagedLease>) {
        if let Some(lease) = lease {
            self.managed_leases.insert(name.to_owned(), lease);
        }
    }

    pub fn retain_managed_backend(&mut self, name: &str) {
        self.managed_leases.retain(|backend, _| backend == name);
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn route_error(&mut self, error: &RouteError) {
        self.failure = match error {
            RouteError::Busy => "lua_overload",
            RouteError::TimedOut => "script_timeout",
            RouteError::Script(_) => "script_error",
            RouteError::UnknownBackend(_) => "unknown_backend",
        };
    }

    fn value(&self, event: &str, message: &dyn Display, kind: Option<io::ErrorKind>) -> Value {
        // Bound diagnostics and let JSON escape newlines/control characters,
        // including script-controlled messages, into a single physical line.
        let message: String = message.to_string().chars().take(2048).collect();
        json!({
            "event": event,
            "timestamp_unix_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
            "connection_id": self.id,
            "listener": self.listener,
            "peer": self.peer.to_string(),
            "backend": self.backend,
            "backend_address": self.backend_address,
            "stage": self.stage,
            "failure": self.failure,
            "duration_ms": self.started.elapsed().as_micros() as f64 / 1000.0,
            "error_kind": kind.map(|kind| format!("{kind:?}")),
            "message": message,
        })
    }

    pub fn emit(&self, event: &str, message: &dyn Display, kind: Option<io::ErrorKind>) {
        let line = self.value(event, message, kind).to_string();
        // Serialize before locking; keep concurrent events intact. A closed
        // stderr must not panic a connection task or change its outcome.
        let _ = writeln!(io::stderr().lock(), "{line}");
    }

    pub fn failed(&self, error: &io::Error) {
        self.emit("connection_failed", error, Some(error.kind()));
    }

    pub fn reject(&mut self, failure: &'static str, message: &str) {
        static LOG: Mutex<RejectionLog> = Mutex::new(RejectionLog {
            second: None,
            written: 0,
            suppressed: 0,
        });
        self.failure = failure;
        let admitted = LOG
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admit(Instant::now());
        let Some(suppressed) = admitted else {
            return;
        };
        let mut value = self.value("connection_rejected", &message, None);
        if suppressed > 0 {
            value["suppressed"] = json!(suppressed);
        }
        let _ = writeln!(io::stderr().lock(), "{value}");
    }
}

fn admin_value(event: &str, outcome: &str, permission: &str, message: &dyn Display) -> Value {
    json!({
        "event": event,
        "timestamp_unix_ms": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
        "stage": "administration",
        "outcome": outcome,
        "permission": permission,
        "message": message.to_string().chars().take(2048).collect::<String>(),
    })
}

/// Emit an administration result without recording request headers or credentials.
/// Callers provide a public diagnostic message, never the submitted authorization.
pub fn admin(event: &str, outcome: &str, permission: &str, message: &dyn Display) {
    let line = admin_value(event, outcome, permission, message).to_string();
    let _ = writeln!(io::stderr().lock(), "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_categories_are_typed_and_diagnostics_are_bounded_json() {
        let mut connection = Connection::new("public", "127.0.0.1:1234".parse().unwrap());
        connection.stage = "on_route";
        for (error, expected) in [
            (RouteError::Busy, "lua_overload"),
            (RouteError::TimedOut, "script_timeout"),
            (
                RouteError::Script("script capacity exhausted".into()),
                "script_error",
            ),
            (
                RouteError::UnknownBackend("missing".into()),
                "unknown_backend",
            ),
        ] {
            connection.route_error(&error);
            let value = connection.value("connection_failed", &error, None);
            assert_eq!(value["failure"], expected);
            assert!(value["backend"].is_null());
            assert_eq!(value["stage"], "on_route");
        }
        let message = format!("quote\"\n\r\0{}", "é".repeat(4096));
        let value = connection.value("connection_failed", &message, None);
        assert_eq!(value["message"].as_str().unwrap().chars().count(), 2048);
        let line = value.to_string();
        assert_eq!(line.lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["message"],
            value["message"]
        );
    }

    #[test]
    fn rejection_lines_are_capped_per_second_and_report_what_was_left_out() {
        let mut log = RejectionLog::default();
        let start = Instant::now();
        for _ in 0..REJECTIONS_PER_SECOND {
            assert_eq!(log.admit(start), Some(0));
        }
        for _ in 0..500 {
            assert_eq!(log.admit(start + Duration::from_millis(900)), None);
        }
        assert_eq!(log.admit(start + Duration::from_secs(1)), Some(500));
        assert_eq!(log.admit(start + Duration::from_secs(1)), Some(0));
    }

    #[test]
    fn admin_diagnostics_are_bounded_single_line_json() {
        let message = format!("invalid \"backend\"\n{}", "é".repeat(4096));
        let value = admin_value("reload", "rejected", "reload", &message);
        assert_eq!(value["stage"], "administration");
        assert_eq!(value["outcome"], "rejected");
        assert_eq!(value["permission"], "reload");
        assert_eq!(value["message"].as_str().unwrap().chars().count(), 2048);
        assert_eq!(value.to_string().lines().count(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&value.to_string()).unwrap(),
            value
        );
        assert!(value.get("authorization").is_none());
    }
}
