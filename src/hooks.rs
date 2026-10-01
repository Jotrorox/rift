//! Owned, language-independent inputs and results for connection routing.
//!
//! No sockets, Lua values or borrowed VM data cross this boundary. A future C
//! adapter can translate addresses and UTF-8 strings and use a tagged decision;
//! these Rust types themselves are not a stable C layout.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    net::SocketAddr,
    sync::Arc,
    time::Instant,
};

use tokio::{sync::Semaphore, task::spawn_blocking, time::timeout};

use crate::{
    config::{Config, Route},
    routing::Backend,
    script::RouteScript,
};

/// Metadata available immediately after TCP accept, before reading any bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub listener: String,
    pub peer_addr: SocketAddr,
    pub local_addr: SocketAddr,
    pub default_backend: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteDecision {
    /// Continue the configured direct or hostname routing policy.
    Default,
    /// A configured backend name, never a script-supplied socket address.
    Backend(String),
    /// Close TCP without sending a protocol-specific response.
    Reject { reason: Option<String> },
}

/// Every error closes the affected connection; there is no implicit fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    Busy,
    TimedOut,
    Script(String),
    UnknownBackend(String),
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("script capacity exhausted"),
            Self::TimedOut => f.write_str("script deadline exceeded"),
            // Script-controlled diagnostics must not produce megabytes of
            // synchronous stderr output on a traffic thread.
            Self::Script(message) => write!(f, "{message:.2048}"),
            Self::UnknownBackend(name) => {
                let display: String = name.chars().take(256).collect();
                write!(f, "unknown backend {display:?}")
            }
        }
    }
}

impl std::error::Error for RouteError {}

/// Shared admission control, with an independent Lua VM for every invocation.
#[derive(Clone)]
pub struct Router {
    script: Option<RouteScript>,
    backends: Arc<BTreeMap<String, Backend>>,
    groups: Arc<BTreeSet<String>>,
    routes: Arc<BTreeMap<String, Route>>,
    slots: Arc<Semaphore>,
    messaging: Option<crate::messaging::Broker>,
}

impl Router {
    /// Preserve the process-wide script job limit across configuration generations.
    pub fn reconfigured(&self, config: &Config) -> Self {
        let mut router = Self::new(config);
        router.slots = self.slots.clone();
        router.messaging = self.messaging.clone();
        router
    }

    pub fn new(config: &Config) -> Self {
        Self {
            script: config.on_route.clone(),
            backends: Arc::new(config.backends.clone()),
            groups: Arc::new(config.service_groups.keys().cloned().collect()),
            routes: Arc::new(config.routes.clone()),
            slots: Arc::new(Semaphore::new(crate::script::MAX_CONCURRENT)),
            messaging: None,
        }
    }

    /// Give connection hooks access to the shared, nonblocking message broker.
    pub fn with_messaging(mut self, broker: crate::messaging::Broker) -> Self {
        self.messaging = Some(broker);
        self
    }

    /// Run once per accepted connection. Overload is rejected without waiting.
    pub async fn route(&self, connection: ConnectionInfo) -> Result<RouteDecision, RouteError> {
        let decision = if let Some(script) = &self.script {
            let permit = self
                .slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| RouteError::Busy)?;
            let script = script.clone();
            let messaging = self.messaging.clone();
            let deadline = Instant::now() + crate::script::EXECUTION_TIMEOUT;
            let task = spawn_blocking(move || {
                // Keep the permit until execution actually stops, including if
                // the async caller times out or is cancelled. Never share a VM
                // or hold a lock on Tokio's traffic threads.
                let _permit = permit;
                script.evaluate(&connection, deadline, messaging)
            });
            timeout(crate::script::EXECUTION_TIMEOUT, task)
                .await
                .map_err(|_| RouteError::TimedOut)?
                .map_err(|error| RouteError::Script(format!("script worker: {error}")))??
        } else {
            RouteDecision::Default
        };
        if let RouteDecision::Backend(name) = &decision
            && !self.backends.contains_key(name)
            && !self.groups.contains(name)
        {
            return Err(RouteError::UnknownBackend(name.clone()));
        }
        Ok(decision)
    }

    pub fn backend(&self, name: &str) -> Option<&Backend> {
        self.backends.get(name)
    }

    pub fn default_backend(&self, listener: &str) -> Option<&str> {
        match self.routes.get(listener)? {
            Route::Direct(name) => Some(name.as_str()),
            Route::Hostnames(patterns) => patterns.get("*").map(String::as_str),
        }
    }
}

#[cfg(test)]
mod tests;
