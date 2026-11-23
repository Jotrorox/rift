//! Peers as nodes (docs/clustering.md, "Servers on the peers' own hosts").
//!
//! In a cluster every peer that serves players is also a node named after
//! itself. Its agent runs inside the peer's process
//! ([`crate::node::agent::Agent::serve_broker`]) and answers on the peer's
//! own broker, so it needs no `rift node` process, no `nodes.<peer>` entry
//! and no token. What a peer-node is comes from the replicated peer table:
//!
//! - its **address** is the host of the peer's `advertise` address;
//! - its **labels** come from `[labels]` in the peer's `cluster.toml`: every
//!   key, and every value that is a valid label (`region = "eu"` gives the
//!   labels `region` and `eu`);
//! - its **limits** (`memory_mb`, `max_instances`, `min_free_disk_mb`) come
//!   from the `[node]` table of the peer's `cluster.toml`, announced in
//!   `peer joined` under the reserved `node.*` label keys.
//!
//! Witnesses run no agent and are no nodes. A `nodes.<peer>` entry in the
//! configuration (a separate `rift node` agent, as step 7 first required)
//! takes precedence over the peer-node of that name.
//!
//! The process keeps the current peer-nodes in a registry that
//! configuration parsing reads: a configuration evaluated on a peer has its
//! peer-nodes in `Config.nodes`, marked [`NodeConfig::peer`], so placement,
//! node commands and the dashboard treat them like any node. Until the
//! registry is loaded (before the peer attached its cluster, and in
//! `rift check`), validation accepts node names that are valid peer names.
//! Service groups accept them afterwards too: a group may name a peer that
//! has not joined yet, and its instances wait until it does.
use super::{
    Cluster,
    settings::{self, Settings},
    state::PeerRow,
};
use crate::config::NodeConfig;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};
use tokio::{sync::watch, task::JoinHandle};

/// Most labels a node may have (`rift.config.nodes.*.labels`).
const MAX_LABELS: usize = 32;

/// The peer-nodes of this process's cluster; `None` until loaded.
static NODES: LazyLock<watch::Sender<Option<BTreeMap<String, NodeConfig>>>> =
    LazyLock::new(|| watch::Sender::new(None));

/// The current peer-nodes, `None` before [`load`] (and outside a cluster).
pub fn current() -> Option<BTreeMap<String, NodeConfig>> {
    NODES.borrow().clone()
}

/// Whether this process is a cluster peer whose peer-nodes are not known
/// yet: configuration validation then accepts any valid peer name as a node.
pub fn pending() -> bool {
    settings::local_peer().is_some() && NODES.borrow().is_none()
}

/// Whether `name` may be a peer-node while [`pending`].
pub fn may_be_peer(name: &str) -> bool {
    pending() && settings::valid_peer_name(name)
}

/// Follow changes of the registry.
pub fn subscribe() -> watch::Receiver<Option<BTreeMap<String, NodeConfig>>> {
    NODES.subscribe()
}

/// Replace the registry; whether it changed.
pub fn set(nodes: BTreeMap<String, NodeConfig>) -> bool {
    NODES.send_if_modified(|current| {
        if current.as_ref() == Some(&nodes) {
            false
        } else {
            *current = Some(nodes);
            true
        }
    })
}

/// Add the registry's peer-nodes to `nodes` (the `rift.config.nodes` of a
/// configuration being parsed) on a cluster peer; a configured node of the
/// same name wins. Outside a cluster, or before [`load`], nothing changes.
pub fn add_to(nodes: &mut BTreeMap<String, NodeConfig>) {
    if settings::local_peer().is_none() {
        return;
    }
    if let Some(peers) = current() {
        add(nodes, &peers);
    }
}

/// Add `peers` to `nodes`, keeping configured nodes of the same name.
pub fn add(nodes: &mut BTreeMap<String, NodeConfig>, peers: &BTreeMap<String, NodeConfig>) {
    for (name, node) in peers {
        nodes.entry(name.clone()).or_insert_with(|| node.clone());
    }
}

/// The node labels of a peer's `[labels]`: every key, and every value that
/// is a valid label; the reserved `node.*` keys are left out.
pub fn node_labels(labels: &BTreeMap<String, String>) -> BTreeSet<String> {
    let valid = |label: &str| {
        !label.is_empty()
            && label.len() <= 64
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    };
    labels
        .iter()
        .filter(|(key, _)| !key.starts_with(settings::NODE_LABEL_PREFIX))
        .flat_map(|(key, value)| [key.as_str(), value.as_str()])
        .filter(|label| valid(label))
        .map(str::to_owned)
        .take(MAX_LABELS)
        .collect()
}

/// The node a peer is, from its advertised address and announced labels.
fn node(address: &str, labels: &BTreeMap<String, String>) -> NodeConfig {
    let number = |key: &str| {
        labels
            .get(key)
            .and_then(|value| value.parse::<usize>().ok())
    };
    NodeConfig {
        address: settings::host_of(address).to_owned(),
        memory_mb: number(settings::NODE_MEMORY_LABEL).filter(|memory| *memory > 0),
        max_instances: number(settings::NODE_INSTANCES_LABEL)
            .filter(|count| (1..=4096).contains(count))
            .unwrap_or(128),
        min_free_disk_mb: number(settings::NODE_DISK_LABEL)
            .unwrap_or(crate::config::DEFAULT_MIN_FREE_DISK_MB),
        labels: node_labels(labels),
        token_env: String::new(),
        peer: true,
    }
}

/// This peer's own node, from its `cluster.toml`.
pub fn own(settings: &Settings) -> NodeConfig {
    node(&settings.advertise, &settings.announced_labels())
}

/// The peer-nodes of `rows` (the peer table): every peer except
/// witnesses. `local`, this peer's settings, describes this peer even
/// before its `peer joined` applied.
pub fn from_rows(rows: &[PeerRow], local: Option<&Settings>) -> BTreeMap<String, NodeConfig> {
    let mut nodes = BTreeMap::new();
    for row in rows {
        if row.witness {
            continue;
        }
        nodes.insert(row.name.clone(), node(&row.address, &row.labels));
    }
    if let Some(local) = local.filter(|local| !local.witness) {
        nodes.insert(local.peer.clone(), own(local));
    }
    nodes
}

/// Load the registry from `cluster`'s replica and keep it current as the
/// peer table changes. Call once the replica holds the cluster's state;
/// the task ends with the cluster.
pub fn load(cluster: &Cluster) -> std::io::Result<JoinHandle<()>> {
    let local = cluster.settings().clone();
    let rows = cluster.state().peers()?;
    set_expired(&rows);
    set(from_rows(&rows, Some(&local)));
    let mut changes = cluster.state().subscribe_changes();
    let weak = std::sync::Arc::downgrade(&cluster.inner);
    Ok(tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match changes.recv().await {
                Ok(change) if !change.touched("cluster_peers") => continue,
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            }
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let cluster = Cluster { inner };
            match cluster.state().peers() {
                Ok(rows) => {
                    set_expired(&rows);
                    if set(from_rows(&rows, Some(&local))) {
                        log_info!("cluster: the peer-nodes changed");
                    }
                }
                Err(error) => log_warn!("cluster: reading the peer table for peer-nodes: {error}"),
            }
        }
    }))
}

/// Peers the leader expired: their nodes are unreachable whatever their
/// agents last reported.
static EXPIRED: std::sync::RwLock<BTreeSet<String>> = std::sync::RwLock::new(BTreeSet::new());

fn set_expired(rows: &[PeerRow]) {
    *EXPIRED.write().unwrap_or_else(|e| e.into_inner()) = rows
        .iter()
        .filter(|row| row.expired)
        .map(|row| row.name.clone())
        .collect();
}

/// Whether `peer` is expired, as of the last peer table this process read.
pub fn is_expired(peer: &str) -> bool {
    EXPIRED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_limits_come_from_the_announced_labels() {
        let labels = BTreeMap::from([
            ("region".to_owned(), "eu".to_owned()),
            ("rack".to_owned(), "a b".to_owned()),
            (settings::NODE_MEMORY_LABEL.to_owned(), "8192".to_owned()),
            (settings::NODE_INSTANCES_LABEL.to_owned(), "4".to_owned()),
            (settings::NODE_DISK_LABEL.to_owned(), "0".to_owned()),
        ]);
        let node = node("10.0.0.2:7444", &labels);
        assert_eq!(node.address, "10.0.0.2");
        assert_eq!(
            node.labels,
            BTreeSet::from(["eu".to_owned(), "rack".to_owned(), "region".to_owned()])
        );
        assert_eq!(node.memory_mb, Some(8192));
        assert_eq!(node.max_instances, 4);
        assert_eq!(node.min_free_disk_mb, 0);
        assert!(node.peer);
        // A peer that announced nothing gets the defaults of a node.
        let bare = super::node("[fd00::3]:7444", &BTreeMap::new());
        assert_eq!(bare.address, "fd00::3");
        assert_eq!(bare.memory_mb, None);
        assert_eq!(bare.max_instances, 128);
        assert_eq!(
            bare.min_free_disk_mb,
            crate::config::DEFAULT_MIN_FREE_DISK_MB
        );
    }

    #[test]
    fn configured_nodes_win_over_peer_nodes() {
        let mut nodes = BTreeMap::from([(
            "eu-1".to_owned(),
            NodeConfig {
                address: "10.0.0.9".into(),
                memory_mb: None,
                max_instances: 1,
                min_free_disk_mb: 0,
                labels: BTreeSet::new(),
                token_env: "RIFT_NODE_EU_1_TOKEN".into(),
                peer: false,
            },
        )]);
        let peers = BTreeMap::from([
            ("eu-1".to_owned(), node("10.0.0.1:7444", &BTreeMap::new())),
            ("eu-2".to_owned(), node("10.0.0.2:7444", &BTreeMap::new())),
        ]);
        add(&mut nodes, &peers);
        assert!(!nodes["eu-1"].peer);
        assert_eq!(nodes["eu-1"].address, "10.0.0.9");
        assert!(nodes["eu-2"].peer);
    }
}
