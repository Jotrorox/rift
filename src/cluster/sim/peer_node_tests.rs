//! Peers as nodes (docs/clustering.md, "Servers on the peers' own hosts"):
//! the labels of a peer's `cluster.toml` reach every peer through the peer
//! table and select it as a node, and the agent a peer embeds starts a
//! server for the leader on its own broker, fenced like any agent.
use super::{Sim, until};
use crate::{
    cluster::{peer_nodes, state::PeerRow},
    config::{Config, NodeSelector},
    messaging::Broker,
    node::{
        Nodes,
        agent::{Agent, Options},
        protocol::{Command, Start},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::Arc,
    time::Duration,
};

/// The node a selector with `label` picks among `nodes`.
fn chosen(nodes: &BTreeMap<String, crate::config::NodeConfig>, label: &str) -> Vec<String> {
    let selector = NodeSelector {
        names: BTreeSet::new(),
        labels: BTreeSet::from([label.to_owned()]),
    };
    nodes
        .iter()
        .filter(|(name, node)| selector.matches(name, node))
        .map(|(name, _)| name.clone())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn peers_are_nodes_with_the_labels_of_their_cluster_toml() {
    let mut sim = Sim::new(3);
    sim.labels
        .insert(1, BTreeMap::from([("pool".to_owned(), "games".to_owned())]));
    sim.form(3).await;
    for (_, peer) in sim.running() {
        let peer = peer.clone();
        until(Duration::from_secs(10), move || {
            peer.state().peers().unwrap_or_default().iter().any(|row| {
                row.name == "p1" && row.labels.get("pool").map(String::as_str) == Some("games")
            })
        })
        .await;
    }
    let rows = sim.peer(2).state().peers().unwrap();
    let nodes = peer_nodes::from_rows(&rows, None);
    assert_eq!(
        nodes.keys().cloned().collect::<Vec<_>>(),
        ["p0", "p1", "p2"]
    );
    assert!(
        nodes
            .values()
            .all(|node| node.peer && node.token_env.is_empty())
    );
    assert_eq!(nodes["p1"].address, "p1");
    assert_eq!(
        nodes["p1"].labels,
        BTreeSet::from(["games".to_owned(), "pool".to_owned()])
    );
    // The limits of `[node]` travel with the labels; these are the defaults.
    assert_eq!(nodes["p1"].max_instances, 128);
    assert_eq!(
        nodes["p1"].min_free_disk_mb,
        crate::config::DEFAULT_MIN_FREE_DISK_MB
    );
    assert_eq!(chosen(&nodes, "games"), ["p1"]);
    sim.shutdown().await;
}

fn row(name: &str, address: &str, labels: &[(&str, &str)], witness: bool) -> PeerRow {
    PeerRow {
        name: name.to_owned(),
        raft_id: 1,
        incarnation: 1,
        address: address.to_owned(),
        public_address: None,
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        witness,
        version: String::new(),
        kinds: Vec::new(),
        expired: false,
        since_ms: 0,
    }
}

/// Real time: the agent runs a real process.
#[tokio::test]
async fn the_embedded_agent_starts_a_server_on_the_peer_node_its_labels_select() {
    let root = std::env::temp_dir().join(format!("rift-peer-node-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("lobby")).unwrap();
    let rows = [
        row("eu-1", "10.0.0.1:7444", &[("pool", "proxy")], false),
        row(
            "eu-2",
            "10.0.0.2:7444",
            &[("pool", "games"), ("node.memory_mb", "4096")],
            false,
        ),
        row("eu-3", "10.0.0.3:7444", &[("pool", "games")], true),
    ];
    let config = Config {
        nodes: peer_nodes::from_rows(&rows, None),
        ..Config::default()
    };
    // A witness runs no agent and is no node.
    assert!(!config.nodes.contains_key("eu-3"));
    assert_eq!(config.nodes["eu-2"].memory_mb, Some(4096));
    let node = match chosen(&config.nodes, "games").as_slice() {
        [node] => node.clone(),
        other => panic!("one node should match, not {other:?}"),
    };
    assert_eq!(node, "eu-2");

    // The leader's registry and the peer's embedded agent on one broker:
    // the cluster's owner routing would carry the subjects between peers.
    let broker = Broker::default();
    let nodes = Nodes::new(broker.clone(), &config);
    nodes.set_cluster(Arc::new(|| Some(3)));
    tokio::spawn(nodes.listen().unwrap());
    let agent = Agent::new(Options::embedded(&node, root.clone(), None, None)).unwrap();
    tokio::spawn(agent.clone().monitor());
    tokio::spawn(agent.clone().serve_broker(broker.clone()));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !nodes.online(&node) {
        assert!(
            std::time::Instant::now() < deadline,
            "the agent never reported"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A fake server: this test program, listing its tests and exiting.
    let port = std::net::TcpListener::bind(("0.0.0.0", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let program = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let start = Start {
        server: "lobby".into(),
        command: vec![program, "--list".into()],
        directory: "lobby".into(),
        port,
        environment: Default::default(),
        bus_certificate: None,
        isolation: Default::default(),
        memory_limit: None,
    };
    let started = nodes
        .request(&node, &Command::Start(start), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(started["adopted"], false, "{started}");
    assert!(started["pid"].as_u64().is_some(), "{started}");
    // The control command went out fenced for the leader's term.
    assert_eq!(nodes.fenced_term(&node), Some(3));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let seen = nodes
            .observe(&node, "lobby")
            .is_some_and(|seen| seen.running.is_some() || seen.exit.is_some());
        if seen {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the agent never reported the server"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A deposed leader's term is refused; inspection still answers.
    nodes.set_cluster(Arc::new(|| Some(2)));
    let refused = nodes
        .request(
            &node,
            &Command::Reconcile {
                servers: vec!["lobby".into()],
            },
            Duration::from_secs(10),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");
    let logs = nodes
        .request(
            &node,
            &Command::Logs {
                server: "lobby".into(),
                directory: "lobby".into(),
                cursor: None,
            },
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(logs.get("text").is_some(), "{logs}");
    agent.shutdown().await;
    let _ = std::fs::remove_dir_all(&root);
}
