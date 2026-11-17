//! The controller's view of its node agents, fed by their state snapshots.
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex, MutexGuard, RwLock},
    time::{Duration, Instant},
};

use bytes::Bytes;
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::protocol::{self, Command, NodeState, Requirement, Resources};
use crate::{
    config::{Config, ManagedServer, NodeConfig},
    messaging::{Broker, quic},
};

/// The controller's own version, which its agents should run too.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Node agents known to this controller. Cheap to clone; every clone shares
/// the same observations.
#[derive(Clone)]
pub struct Nodes(Arc<Inner>);

impl std::fmt::Debug for Nodes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Nodes")
            .field("config", &*read(&self.0.config))
            .finish_non_exhaustive()
    }
}

struct Inner {
    broker: Broker,
    /// Replaced by [`Nodes::reconfigure`] when a reload changes `nodes`.
    config: RwLock<BTreeMap<String, NodeConfig>>,
    /// The agents' bus logins, shared with the QUIC server.
    logins: quic::NodeLogins,
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
    /// The last exit's status, and whether it was successful.
    pub exit: Option<(String, bool)>,
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

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|error| error.into_inner())
}

impl Nodes {
    pub fn new(broker: Broker, config: &Config) -> Self {
        Self::with_timeout(broker, config, protocol::UNREACHABLE_AFTER)
    }

    /// Like [`Nodes::new`], with a custom silence before a node is unreachable.
    pub fn with_timeout(broker: Broker, config: &Config, unreachable_after: Duration) -> Self {
        Self(Arc::new(Inner {
            broker,
            config: RwLock::new(config.nodes.clone()),
            logins: quic::NodeLogins::default(),
            unreachable_after,
            observed: Mutex::new(BTreeMap::new()),
            reconnected: Mutex::new(BTreeSet::new()),
            wake: Notify::new(),
        }))
    }

    pub fn is_empty(&self) -> bool {
        read(&self.0.config).is_empty()
    }

    /// The node definitions in effect.
    pub fn config(&self) -> BTreeMap<String, NodeConfig> {
        read(&self.0.config).clone()
    }

    fn configured(&self, node: &str) -> bool {
        read(&self.0.config).contains_key(node)
    }

    /// The agents' bus logins, for the QUIC server's
    /// [`quic::AuthConfig::nodes`].
    pub fn logins(&self) -> &quic::NodeLogins {
        &self.0.logins
    }

    /// Check that a reload could switch to `logins`, before it commits.
    pub fn check(&self, logins: &[quic::Identity]) -> io::Result<()> {
        self.0.logins.check(logins).map_err(io::Error::other)
    }

    /// Switch to the node definitions of a reloaded configuration and their
    /// `logins` (see [`identities`]). Removed nodes are forgotten and lose
    /// their bus session, as does a node whose token changed; agents of
    /// added nodes may log in from now on. The caller refuses to remove a
    /// node that still runs servers.
    pub fn reconfigure(
        &self,
        nodes: &BTreeMap<String, NodeConfig>,
        logins: Vec<quic::Identity>,
    ) -> io::Result<()> {
        let revoked = self.0.logins.replace(logins).map_err(io::Error::other)?;
        let previous = std::mem::replace(
            &mut *self.0.config.write().unwrap_or_else(|e| e.into_inner()),
            nodes.clone(),
        );
        let removed: Vec<&String> = previous
            .keys()
            .filter(|name| !nodes.contains_key(*name))
            .collect();
        {
            let mut observed = lock(&self.0.observed);
            let mut reconnected = lock(&self.0.reconnected);
            for name in &removed {
                observed.remove(*name);
                reconnected.remove(*name);
            }
        }
        for (name, node) in nodes {
            match previous.get(name) {
                None => log_info!("node {name} added"),
                Some(old) if old != node => log_info!("node {name} changed"),
                Some(_) => {}
            }
        }
        for name in removed {
            log_info!("node {name} removed");
        }
        for principal in revoked {
            if let Some(name) = principal.strip_prefix("node.")
                && nodes.contains_key(name)
            {
                log_info!("node {name} has a new token; its agent must reconnect with it");
            }
        }
        Ok(())
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
                            log_warn!("ignored state from node {node}: {error}");
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
        if !self.configured(node) {
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
            log_info!("node {node} connected");
            if let Some(warning) = mismatch(&observed_state(&self.0.observed, node)) {
                log_warn!("node {node}: {warning}");
            }
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

    /// The host resources from the latest snapshot of an online node; `None`
    /// while it is unreachable or when its agent does not report them.
    pub fn resources(&self, node: &str) -> Option<Resources> {
        lock(&self.0.observed)
            .get(node)
            .filter(|observed| observed.seen.elapsed() <= self.0.unreachable_after)?
            .state
            .resources
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
                .map(|exit| (exit.status.clone(), exit.success)),
        })
    }

    /// The mandatory `requirements` that the agent on `node` cannot apply.
    /// Empty while the node has never reported, since nothing is sent then.
    pub fn lacking(&self, node: &str, requirements: &[Requirement]) -> Vec<&'static str> {
        let observed = lock(&self.0.observed);
        let Some(observed) = observed.get(node) else {
            return Vec::new();
        };
        requirements
            .iter()
            .filter(|requirement| requirement.mandatory)
            .filter(|requirement| !supports(&observed.state, requirement.feature))
            .map(|requirement| requirement.feature)
            .collect()
    }

    /// Refuse a start that needs a feature the agent on `node` lacks, rather
    /// than let the agent silently drop it. A best-effort setting it lacks
    /// (`landlock`, a memory limit that is not `required`) only warns, as it
    /// does for a local server on a kernel without support.
    pub fn check_start(&self, node: &str, start: &protocol::Start) -> io::Result<()> {
        let requirements = start.requirements();
        let (version, skipped) = {
            let observed = lock(&self.0.observed);
            let Some(observed) = observed.get(node) else {
                return Ok(());
            };
            let skipped: Vec<_> = requirements
                .iter()
                .filter(|requirement| !supports(&observed.state, requirement.feature))
                .copied()
                .collect();
            (observed.state.version.clone(), skipped)
        };
        let (refused, warned): (Vec<_>, Vec<_>) = skipped
            .into_iter()
            .partition(|requirement| requirement.mandatory);
        if !refused.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "node {node} runs an agent ({version}) that cannot apply {} for {}; upgrade rift on that node to {VERSION}",
                    settings(&refused),
                    start.server,
                ),
            ));
        }
        if !warned.is_empty() {
            log_warn!(
                "node {node} runs an agent ({version}) that cannot apply {} for {}; starting it without; upgrade rift on that node to {VERSION}",
                settings(&warned),
                start.server,
            );
        }
        Ok(())
    }

    /// Send one command and wait for the agent's reply.
    pub async fn request(
        &self,
        node: &str,
        command: &Command,
        wait: Duration,
    ) -> io::Result<Value> {
        if !self.configured(node) {
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
        let nodes = read(&self.0.config);
        let observed = lock(&self.0.observed);
        Value::Array(
            nodes
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
                        "min_free_disk_mb": node.min_free_disk_mb,
                        "instances": instances,
                        // Only fresh measurements; an unreachable node's
                        // last report would read as current.
                        "resources": seen
                            .filter(|_| reachability == Reachability::Online)
                            .and_then(|seen| seen.state.resources),
                        "running": seen.map(|seen| seen.state.servers.len()),
                        "version": seen.map(|seen| seen.state.version.clone()),
                        "features": seen.map(|seen| seen.state.features.clone()),
                        "missing_features": seen.map(|seen| missing(&seen.state)),
                        "version_mismatch": seen.is_some_and(|seen| seen.state.version != VERSION),
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

/// The bus login of every node in `nodes`: principal `node.<name>`, the
/// token from its `token_env`, and the grants of an agent.
pub fn identities(nodes: &BTreeMap<String, NodeConfig>) -> io::Result<Vec<quic::Identity>> {
    nodes
        .iter()
        .map(|(name, node)| {
            let (publish, subscribe) = protocol::principal_grants(name);
            Ok(quic::Identity {
                name: NodeConfig::principal(name),
                token: token(name, &node.token_env)?,
                publish,
                subscribe,
                control: false,
                players: false,
            })
        })
        .collect()
}

/// A node agent's token from `variable`, shared by its bus login and
/// template downloads.
pub fn token(name: &str, variable: &str) -> io::Result<String> {
    let token = std::env::var(variable).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("nodes.{name}: set {variable} to a secret of at least 32 bytes"),
        )
    })?;
    if !(32..=1024).contains(&token.len()) || token.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("nodes.{name}: token must contain 32..=1024 bytes without control characters"),
        ));
    }
    Ok(token)
}

fn observed_state(observed: &Mutex<BTreeMap<String, Observed>>, node: &str) -> NodeState {
    lock(observed)
        .get(node)
        .map(|observed| observed.state.clone())
        .unwrap_or_default()
}

fn supports(state: &NodeState, feature: &str) -> bool {
    state.features.iter().any(|known| known == feature)
}

/// The controller's features that this agent does not advertise.
fn missing(state: &NodeState) -> Vec<&'static str> {
    protocol::FEATURES
        .iter()
        .copied()
        .filter(|feature| !supports(state, feature))
        .collect()
}

/// Why an agent does not match this controller, if it does not.
fn mismatch(state: &NodeState) -> Option<String> {
    let missing = missing(state);
    if !missing.is_empty() {
        return Some(format!(
            "agent {} lacks {}; Rift refuses to start servers there that need them and starts the rest without; upgrade rift on that node to {VERSION}",
            state.version,
            missing.join(", "),
        ));
    }
    (state.version != VERSION).then(|| {
        format!(
            "agent {} differs from this controller {VERSION}; upgrade agents together with the controller",
            state.version
        )
    })
}

/// The settings behind `requirements`, as an administrator wrote them.
fn settings(requirements: &[Requirement]) -> String {
    requirements
        .iter()
        .map(
            |requirement| match (requirement.feature, requirement.mandatory) {
                (protocol::feature::ENVIRONMENT, _) => "env/inherit_env",
                (protocol::feature::BUS_CERTIFICATE, _) => "its bus login",
                (protocol::feature::ISOLATION, true) => "isolation = \"required\"",
                (protocol::feature::ISOLATION, false) => "isolation = \"landlock\"",
                (protocol::feature::MEMORY_LIMIT, true) => "its required memory_limit",
                (protocol::feature::MEMORY_LIMIT, false) => "its memory_limit",
                (feature, _) => feature,
            },
        )
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a server definition needs from the agents it runs on. A bus login
/// adds variables and a certificate to its start.
pub fn server_requirements(server: &ManagedServer) -> Vec<Requirement> {
    protocol::requirements(
        server.environment != Default::default() || server.messaging.is_some(),
        server.messaging.is_some(),
        server.isolation.mode,
        server.memory_limit.map(Into::into),
    )
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
            features: Vec::new(),
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
                success: false,
            }],
            resources: None,
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
            nodes.observe("node1", "gone").unwrap().exit,
            Some(("exit status: 1".to_owned(), false))
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

    #[tokio::test]
    async fn reconfigure_adds_removes_and_relabels_nodes() {
        let nodes = nodes();
        nodes.record("node1", &state("a", 1, &[])).unwrap();
        let mut definitions = nodes.config();
        definitions.get_mut("node1").unwrap().labels = BTreeSet::from(["games".to_owned()]);
        definitions.insert(
            "node2".into(),
            NodeConfig {
                address: "10.0.0.12".into(),
                memory_mb: None,
                max_instances: 8,
                labels: BTreeSet::new(),
                token_env: "RIFT_NODE_NODE2_TOKEN".into(),
            },
        );
        let login = |name: &str, token: &str| quic::Identity {
            name: NodeConfig::principal(name),
            token: token.into(),
            publish: vec![],
            subscribe: vec![],
            control: false,
            players: false,
        };
        nodes
            .reconfigure(
                &definitions,
                vec![
                    login("node1", "node1-test-only-long-random-token"),
                    login("node2", "node2-test-only-long-random-token"),
                ],
            )
            .unwrap();
        assert_eq!(nodes.logins().names(), ["node.node1", "node.node2"]);
        // An existing node keeps what it reported; an added one may report.
        assert!(nodes.online("node1"));
        nodes.record("node2", &state("b", 1, &[])).unwrap();
        let described = nodes.describe(&Config::default());
        assert_eq!(described[0]["labels"], json!(["games"]));
        assert_eq!(described[1]["name"], "node2");
        // A removed node is forgotten and its snapshots are refused.
        definitions.remove("node1");
        nodes
            .reconfigure(
                &definitions,
                vec![login("node2", "node2-test-only-long-random-token")],
            )
            .unwrap();
        assert_eq!(nodes.reachability("node1"), Reachability::NeverConnected);
        assert!(nodes.record("node1", &state("a", 2, &[])).is_err());
        assert_eq!(
            nodes.describe(&Config::default()).as_array().unwrap().len(),
            1
        );
        // Duplicate tokens are refused without changing anything.
        definitions.insert("node3".into(), definitions["node2"].clone());
        assert!(
            nodes
                .reconfigure(
                    &definitions,
                    vec![
                        login("node2", "node2-test-only-long-random-token"),
                        login("node3", "node2-test-only-long-random-token"),
                    ],
                )
                .is_err()
        );
        assert_eq!(nodes.config().len(), 1);
    }

    #[test]
    fn starts_are_refused_on_agents_that_would_drop_their_settings() {
        use crate::node::protocol::{FEATURES, IsolationSpec, MemoryLimitSpec, Start};
        let nodes = nodes();
        let mut start = Start {
            server: "game-1".into(),
            command: vec!["java".into()],
            directory: "game-1".into(),
            port: 30000,
            environment: Default::default(),
            bus_certificate: None,
            isolation: IsolationSpec::default(),
            memory_limit: Some(MemoryLimitSpec {
                mb: 1024,
                required: true,
            }),
        };
        // Nothing is sent to a node that never reported, so nothing to check.
        nodes.check_start("node1", &start).unwrap();
        // An agent from before `features` advertises none.
        nodes.record("node1", &state("old", 1, &[])).unwrap();
        let error = nodes.check_start("node1", &start).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(
            error.to_string().contains("required memory_limit"),
            "{error}"
        );
        assert_eq!(
            nodes.lacking("node1", &start.requirements()),
            ["memory_limit"]
        );
        // A best-effort limit or `landlock` starts without, as locally.
        start.memory_limit = Some(MemoryLimitSpec {
            mb: 1024,
            required: false,
        });
        start.isolation.mode = "landlock".into();
        nodes.check_start("node1", &start).unwrap();
        assert!(nodes.lacking("node1", &start.requirements()).is_empty());
        start.isolation.mode = "required".into();
        assert!(nodes.check_start("node1", &start).is_err());
        start.isolation.mode = "off".into();
        start.environment.inherit = Some(vec!["PATH".into()]);
        assert!(nodes.check_start("node1", &start).is_err());
        let described = nodes.describe(&Config::default());
        assert_eq!(
            described[0]["missing_features"].as_array().unwrap().len(),
            FEATURES.len()
        );
        assert_eq!(described[0]["version_mismatch"], true);
        // A current agent gets everything.
        let mut current: NodeState = serde_json::from_slice(&state("new", 1, &[])).unwrap();
        current.version = VERSION.into();
        current.features = FEATURES.iter().map(|&f| f.to_owned()).collect();
        nodes
            .record("node1", &serde_json::to_vec(&current).unwrap())
            .unwrap();
        start.isolation.mode = "required".into();
        start.bus_certificate = Some("pem".into());
        nodes.check_start("node1", &start).unwrap();
        let described = nodes.describe(&Config::default());
        assert_eq!(described[0]["missing_features"], serde_json::json!([]));
        assert_eq!(described[0]["version_mismatch"], false);
    }
}
