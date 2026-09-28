//! One JSON line per failed connection or backend attempt; no traffic logging.
use rift::hooks::RouteError;
use serde_json::{Value, json};
use std::{
    fmt::Display,
    io::{self, Write},
    net::SocketAddr,
    sync::atomic::{AtomicU64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub struct Connection {
    id: u64,
    listener: String,
    peer: SocketAddr,
    started: Instant,
    pub backend: Option<String>,
    pub backend_address: Option<String>,
    pub stage: &'static str,
    pub failure: &'static str,
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
        }
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
        self.failure = failure;
        self.emit("connection_rejected", &message, None);
    }
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
}
