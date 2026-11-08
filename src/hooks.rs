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
    server_list::{Ping, StatusOverride, StatusScript},
};

/// Metadata available immediately after TCP accept, before reading any bytes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConnectionInfo {
    pub listener: String,
    pub peer_addr: SocketAddr,
    pub local_addr: SocketAddr,
    pub default_backend: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
    status: Option<StatusScript>,
    backends: Arc<BTreeMap<String, Backend>>,
    groups: Arc<BTreeSet<String>>,
    routes: Arc<BTreeMap<String, Route>>,
    /// Workers for `on_route`. Server-list pings use `status_slots`, so they
    /// cannot crowd out connection routing.
    slots: Arc<Semaphore>,
    status_slots: Arc<Semaphore>,
    messaging: Option<crate::messaging::Broker>,
    /// Persistent plugins asked after the script keeps the default.
    plugins: Option<crate::plugins::Host>,
}

impl Router {
    /// Preserve the process-wide script job limit across configuration generations.
    pub fn reconfigured(&self, config: &Config) -> Self {
        let mut router = Self::new(config);
        router.slots = self.slots.clone();
        router.status_slots = self.status_slots.clone();
        router.messaging = self.messaging.clone();
        router.plugins = self.plugins.clone();
        router
    }

    pub fn new(config: &Config) -> Self {
        Self {
            script: config.on_route.clone(),
            status: config.on_status.clone(),
            backends: Arc::new(config.backends.clone()),
            groups: Arc::new(config.service_groups.keys().cloned().collect()),
            routes: Arc::new(config.routes.clone()),
            slots: Arc::new(Semaphore::new(crate::script::MAX_CONCURRENT)),
            status_slots: Arc::new(Semaphore::new(crate::script::MAX_CONCURRENT_STATUS)),
            messaging: None,
            plugins: None,
        }
    }

    /// Ask persistent plugins after `on_route`/`on_status` keep the default.
    pub fn with_plugins(mut self, plugins: crate::plugins::Host) -> Self {
        self.plugins = Some(plugins);
        self
    }

    /// Give connection hooks access to the shared, nonblocking message broker.
    pub fn with_messaging(mut self, broker: crate::messaging::Broker) -> Self {
        self.messaging = Some(broker);
        self
    }

    /// Run once per accepted connection: `on_route` first, then persistent
    /// plugins in order. A short burst waits briefly for a script worker;
    /// sustained overload is rejected.
    pub async fn route(&self, connection: ConnectionInfo) -> Result<RouteDecision, RouteError> {
        let plugins = self
            .plugins
            .as_ref()
            .filter(|plugins| plugins.may_handle(crate::plugins::api::Hook::Route));
        let mut decision = if let Some(script) = self.script.clone() {
            let connection = connection.clone();
            self.run(&self.slots, move |deadline, messaging| {
                script.evaluate(&connection, deadline, messaging)
            })
            .await?
        } else {
            RouteDecision::Default
        };
        if decision == RouteDecision::Default
            && let Some(plugins) = plugins
        {
            decision = plugins.route(&connection).await?;
        }
        if let RouteDecision::Backend(name) = &decision
            && !self.backends.contains_key(name)
            && !self.groups.contains(name)
        {
            return Err(RouteError::UnknownBackend(name.clone()));
        }
        Ok(decision)
    }

    /// Run `on_status` for a server-list ping on its own workers, separate
    /// from `on_route`; `None` keeps the default response.
    pub async fn status(&self, ping: Ping) -> Result<Option<StatusOverride>, RouteError> {
        if let Some(script) = self.status.clone() {
            let input = ping.clone();
            let changes = self
                .run(&self.status_slots, move |deadline, messaging| {
                    script.evaluate(&input, deadline, messaging)
                })
                .await?;
            if changes.is_some() {
                return Ok(changes);
            }
        }
        match &self.plugins {
            Some(plugins) if plugins.may_handle(crate::plugins::api::Hook::Status) => {
                plugins.status(&ping).await
            }
            _ => Ok(None),
        }
    }

    /// `on_status` or a persistent plugin that may answer pings.
    pub fn has_status_hook(&self) -> bool {
        self.status.is_some()
            || self
                .plugins
                .as_ref()
                .is_some_and(|plugins| plugins.may_handle(crate::plugins::api::Hook::Status))
    }

    /// Waits at most [`crate::script::SLOT_WAIT`] for a worker. The execution
    /// deadline starts once a worker is assigned.
    async fn run<T: Send + 'static>(
        &self,
        slots: &Arc<Semaphore>,
        evaluate: impl FnOnce(Instant, Option<crate::messaging::Broker>) -> Result<T, RouteError>
        + Send
        + 'static,
    ) -> Result<T, RouteError> {
        let permit = crate::script::acquire_slot(slots)
            .await
            .ok_or(RouteError::Busy)?;
        let messaging = self.messaging.clone();
        let deadline = Instant::now() + crate::script::EXECUTION_TIMEOUT;
        let task = spawn_blocking(move || {
            // Keep the permit until execution actually stops, including if
            // the async caller times out or is cancelled. Never share a VM
            // or hold a lock on Tokio's traffic threads.
            let _permit = permit;
            evaluate(deadline, messaging)
        });
        timeout(crate::script::EXECUTION_TIMEOUT, task)
            .await
            .map_err(|_| RouteError::TimedOut)?
            .map_err(|error| RouteError::Script(format!("script worker: {error}")))?
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
