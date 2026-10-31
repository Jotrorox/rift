//! The controller's view of its node agents, fed by their state snapshots.
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use bytes::Bytes;
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::protocol::{self, Command, NodeState};
use crate::{
    config::{Config, NodeConfig},
    messaging::Broker,
};

/// Node agents known to this controller. Cheap to clone; every clone shares
/// the same observations.
#[derive(Clone)]
pub struct Nodes(Arc<Inner>);

impl std::fmt::Debug for Nodes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Nodes")
            .field("config", &self.0.config)
            .finish_non_exhaustive()
    }
}

struct Inner {
    broker: Broker,
    config: BTreeMap<String, NodeConfig>,
    unreachable_after: Duration,
    observed: Mutex<BTreeMap<String, Observed>>,
    reconnected: Mutex<BTreeSet<String>>,
    wake: Notify,
}

#[derive(Debug)]
struct Observed {
    state: NodeState,
    seen: Instant,
    connected: Instant,
}

/// One server as the latest snapshot of its node reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub agent: String,
    pub revision: u64,
    /// `Some(pid)` while the agent runs the server.
    pub running: Option<Option<u32>>,
    pub exit: Option<String>,
}

/// Reachability as shown by `/api/nodes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    Online,
    Unreachable,
    NeverConnected,
}

impl Reachability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Unreachable => "unreachable",
            Self::NeverConnected => "never_connected",
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

impl Nodes {
    pub fn new(broker: Broker, config: &Config) -> Self {
        Self::with_timeout(broker, config, protocol::UNREACHABLE_AFTER)
    }

    /// Like [`Nodes::new`], with a custom silence before a node is unreachable.
    pub fn with_timeout(broker: Broker, config: &Config, unreachable_after: Duration) -> Self {
        Self(Arc::new(Inner {
            broker,
            config: config.nodes.clone(),
            unreachable_after,
            observed: Mutex::new(BTreeMap::new()),
            reconnected: Mutex::new(BTreeSet::new()),
            wake: Notify::new(),
        }))
    }

    pub fn is_empty(&self) -> bool {
        self.0.config.is_empty()
    }

    pub fn config(&self) -> &BTreeMap<String, NodeConfig> {
        &self.0.config
    }

    /// Subscribe before returning, so no snapshot published after startup is
    /// missed; run the returned future for the controller's lifetime.
    pub fn listen(&self) -> io::Result<impl Future<Output = ()> + Send + 'static> {
        let mut subscription = self
            .0
            .broker
            .subscribe(protocol::STATE_PATTERN, None)
            .map_err(io::Error::other)?;
        let nodes = self.clone();
        Ok(async move {
            loop {
                match subscription.recv().await {
                    Ok(message) => {
                        if let Some(node) = protocol::state_node(&message.subject)
                            && let Err(error) = nodes.record(node, &message.payload)
                        {
                            eprintln!("rift: ignored state from node {node}: {error}");
                        }
                    }
                    // A burst closed this subscriber; the next snapshot repairs state.
                    Err(_) => match nodes.0.broker.subscribe(protocol::STATE_PATTERN, None) {
                        Ok(next) => subscription = next,
                        Err(_) => return,
                    },
                }
            }
        })
    }

    /// Apply one snapshot. A first snapshot, one after an outage, or one from a
    /// restarted agent marks the node for reconciliation.
    pub fn record(&self, node: &str, payload: &[u8]) -> io::Result<()> {
        if !self.0.config.contains_key(node) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "node is not configured",
            ));
        }
        let state: NodeState = serde_json::from_slice(payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let now = Instant::now();
        let mut observed = lock(&self.0.observed);
        let reconnect = match observed.get(node) {
            None => true,
            Some(previous) => {
                previous.state.agent != state.agent
                    || now.duration_since(previous.seen) > self.0.unreachable_after
            }
        };
        if !reconnect
            && let Some(previous) = observed.get(node)
            && state.revision < previous.state.revision
        {
            // Reordered delivery; never step back to an older process list.
            observed.get_mut(node).unwrap().seen = now;
            return Ok(());
        }
        let connected = match observed.get(node) {
            Some(previous) if !reconnect => previous.connected,
            _ => now,
        };
        observed.insert(
            node.to_owned(),
            Observed {
                state,
                seen: now,
                connected,
            },
        );
        drop(observed);
        if reconnect {
            eprintln!("rift: node {node} connected");
            lock(&self.0.reconnected).insert(node.to_owned());
            self.0.wake.notify_one();
        }
        Ok(())
    }

    /// Wait until at least one node connected or reconnected and return them.
    pub async fn reconnected(&self) -> Vec<String> {
        loop {
            let notified = self.0.wake.notified();
            let nodes: Vec<_> = std::mem::take(&mut *lock(&self.0.reconnected))
                .into_iter()
                .collect();
            if !nodes.is_empty() {
                return nodes;
            }
            notified.await;
        }
    }

    pub fn reachability(&self, node: &str) -> Reachability {
        match lock(&self.0.observed).get(node) {
            None => Reachability::NeverConnected,
            Some(observed) if observed.seen.elapsed() <= self.0.unreachable_after => {
                Reachability::Online
            }
            Some(_) => Reachability::Unreachable,
        }
    }

    pub fn online(&self, node: &str) -> bool {
        self.reachability(node) == Reachability::Online
    }

    /// The latest report about `server`, or `None` while the node is unreachable.
    pub fn observe(&self, node: &str, server: &str) -> Option<Observation> {
        let observed = lock(&self.0.observed);
        let observed = observed
            .get(node)
            .filter(|observed| observed.seen.elapsed() <= self.0.unreachable_after)?;
        Some(Observation {
            agent: observed.state.agent.clone(),
            revision: observed.state.revision,
            running: observed
                .state
                .servers
                .iter()
                .find(|process| process.name == server)
                .map(|process| process.pid),
            exit: observed
                .state
                .exits
                .iter()
                .rev()
                .find(|exit| exit.name == server)
                .map(|exit| exit.status.clone()),
        })
    }

    /// Send one command and wait for the agent's reply.
    pub async fn request(
        &self,
        node: &str,
        command: &Command,
        wait: Duration,
    ) -> io::Result<Value> {
        if !self.0.config.contains_key(node) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("unknown node {node:?}"),
            ));
        }
        if !self.online(node) {
            return Err(unreachable(node));
        }
        let payload = serde_json::to_vec(command)?;
        if payload.len() > protocol::MAX_MESSAGE {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "node command exceeds the 1 MiB message limit",
            ));
        }
        let reply = self
            .0
            .broker
            .request(&protocol::command_subject(node), Bytes::from(payload), wait)
            .await
            .map_err(|error| match error {
                crate::messaging::Error::NoResponders => unreachable(node),
                crate::messaging::Error::Timeout => io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("node {node} did not answer in time"),
                ),
                error => io::Error::other(error),
            })?;
        protocol::parse_reply(&reply.payload)
    }

    /// Operational state for the HTTP API and control subjects.
    pub fn describe(&self, config: &Config) -> Value {
        let usage = usage(config);
        let observed = lock(&self.0.observed);
        Value::Array(
            self.0
                .config
                .iter()
                .map(|(name, node)| {
                    let seen = observed.get(name);
                    let (instances, memory) = usage.get(name).copied().unwrap_or_default();
                    let reachability = match seen {
                        None => Reachability::NeverConnected,
                        Some(seen) if seen.seen.elapsed() <= self.0.unreachable_after => {
                            Reachability::Online
                        }
                        Some(_) => Reachability::Unreachable,
                    };
                    json!({
                        "name": name,
                        "address": node.address,
                        "labels": node.labels,
                        "state": reachability.as_str(),
                        "memory_mb": node.memory_mb,
                        "allocated_memory_mb": memory,
                        "max_instances": node.max_instances,
                        "instances": instances,
                        "running": seen.map(|seen| seen.state.servers.len()),
                        "version": seen.map(|seen| seen.state.version.clone()),
                        "last_seen_ms": seen.map(|seen| seen.seen.elapsed().as_millis() as u64),
                        "connected_ms": seen
                            .filter(|_| reachability == Reachability::Online)
                            .map(|seen| seen.connected.elapsed().as_millis() as u64),
                    })
                })
                .collect(),
        )
    }
}

fn unreachable(node: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        format!("node {node} is unreachable"),
    )
}

/// The default heap size of template groups, used for placement when a
/// group has an explicit command.
pub const DEFAULT_INSTANCE_MEMORY_MB: usize = 1024;

/// Instances and allocated memory per node. Named servers count as an
/// instance without memory; group instances use their group's `memory_mb`.
pub fn usage(config: &Config) -> BTreeMap<String, (usize, usize)> {
    let mut usage: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (name, server) in &config.managed_servers {
        let Some(node) = &server.node else {
            continue;
        };
        let memory = config
            .instances
            .get(name)
            .and_then(|instance| config.service_groups.get(&instance.group))
            .map_or(0, |group| {
                group.memory_mb.unwrap_or(DEFAULT_INSTANCE_MEMORY_MB)
            });
        let entry = usage.entry(node.clone()).or_default();
        entry.0 += 1;
        entry.1 += memory;
    }
    usage
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::protocol::{ExitState, ProcessState};

    fn nodes() -> Nodes {
        let config = Config::from_lua(
            "rift.config.listeners.public='127.0.0.1:25565'
rift.config.backends.lobby='127.0.0.1:25566'
rift.config.routes.public='lobby'
rift.config.messaging={listen='0.0.0.0:7443',certificate='c.pem',private_key='k.pem'}
rift.config.nodes.node1={address='10.0.0.11'}",
            "registry.lua",
        )
        .unwrap();
        Nodes::new(Broker::default(), &config)
    }

    fn state(agent: &str, revision: u64, servers: &[&str]) -> Vec<u8> {
        serde_json::to_vec(&NodeState {
            agent: agent.into(),
            version: "test".into(),
            revision,
            servers: servers
                .iter()
                .map(|name| ProcessState {
                    name: (*name).into(),
                    pid: Some(1),
                })
                .collect(),
            exits: vec![ExitState {
                name: "gone".into(),
                status: "exit status: 1".into(),
            }],
        })
        .unwrap()
    }

    #[tokio::test]
    async fn snapshots_track_reachability_reconnects_and_revisions() {
        let nodes = nodes();
        assert_eq!(nodes.reachability("node1"), Reachability::NeverConnected);
        assert!(nodes.record("other", &state("a", 1, &[])).is_err());
        nodes.record("node1", &state("a", 2, &["game-1"])).unwrap();
        assert!(nodes.online("node1"));
        assert_eq!(nodes.reconnected().await, ["node1"]);
        let observation = nodes.observe("node1", "game-1").unwrap();
        assert_eq!(observation.running, Some(Some(1)));
        assert_eq!(
            nodes.observe("node1", "gone").unwrap().exit.as_deref(),
            Some("exit status: 1")
        );
        // An older, reordered snapshot does not resurrect stale processes.
        nodes.record("node1", &state("a", 1, &[])).unwrap();
        assert_eq!(nodes.observe("node1", "game-1").unwrap().revision, 2);
        // A restarted agent is a reconnection.
        nodes.record("node1", &state("b", 1, &[])).unwrap();
        assert_eq!(nodes.reconnected().await, ["node1"]);
        assert_eq!(nodes.observe("node1", "game-1").unwrap().running, None);
        let error = nodes
            .request(
                "node1",
                &Command::Reconcile { servers: vec![] },
                Duration::from_millis(50),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    }
}
