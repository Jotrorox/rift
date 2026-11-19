//! Leader tasks on three simulated peers (plan step 7): server states the
//! leader records reach every peer and outlive its leadership, a deposed
//! leader's term-tagged writes are refused, followers hand operator
//! requests to the leader, and an idle stop needs every peer's consent.
//!
//! The stores' synchronous APIs block their thread until the write is
//! applied, so the tests call them on a thread of their own, as the
//! database tests do.
use super::{Sim, until};
use crate::{
    cluster::leader::{self, Coordination},
    config::Config,
    instance_store::{ServerState, Store},
    managed::{Coordinator, ManagedServers},
    players::PlayerRegistry,
};
use std::{io, sync::Arc, time::Duration};

/// Run `work` on a thread outside the runtime and wait for it.
async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver.await.expect("the thread finished")
}

const CONFIG: &str = "rift.config.listeners.public='127.0.0.1:0'
rift.config.routes.public='game'
rift.config.nodes.a={address='10.0.0.1'}
rift.config.templates.t={server_jar='paper.jar'}
rift.config.service_groups.game={template='t',storage='disposable',port_range={30000,30099},nodes={'a'}}";

/// Every peer's configuration: instance `game-1` on node `a`, which never
/// connects, starting on connect.
fn config() -> Config {
    let mut config = Config::from_lua(CONFIG, "leader.lua").unwrap();
    config
        .add_instance_on("game", "game-1", 30000, Some("a"))
        .unwrap();
    config
        .managed_servers
        .get_mut("game-1")
        .unwrap()
        .start_on_connect = true;
    config
}

/// One peer's managed servers, coordinated through its cluster.
struct Peer {
    store: Arc<Store>,
    managed: ManagedServers,
    coordination: Coordination,
}

fn coordinate(sim: &Sim, index: usize) -> Peer {
    let store = Arc::new(Store::new(sim.database(index)).unwrap());
    let managed = ManagedServers::new(&config(), Arc::new(PlayerRegistry::default()));
    let coordination = Coordination::start(sim.peer(index), &managed, &store).unwrap();
    managed.launch().unwrap();
    Peer {
        store,
        managed,
        coordination,
    }
}

/// Stop the workers and the coordination, so the peers' databases close.
async fn stop(peers: Vec<Peer>) {
    for peer in &peers {
        peer.managed.shutdown().await;
        peer.coordination.stop();
    }
}

fn running(restart_attempts: u32) -> ServerState {
    ServerState {
        phase: "running".into(),
        last_error: None,
        automatic: true,
        restart_attempts,
    }
}

#[tokio::test(start_paused = true)]
async fn server_states_reach_every_peer_and_a_deposed_leader_is_refused() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    let peers: Vec<Peer> = (0..3).map(|index| coordinate(&sim, index)).collect();
    for (index, peer) in peers.iter().enumerate() {
        assert_eq!(
            peer.coordination.acting_term().is_some(),
            index == leader,
            "only the leader acts"
        );
    }
    let term = peers[leader].coordination.acting_term().unwrap();
    peers[leader].coordination.record("lobby", running(1), term);
    for peer in &peers {
        let coordination = peer.coordination.clone();
        until(Duration::from_secs(5), move || {
            coordination.recorded("lobby") == Some(running(1))
        })
        .await;
    }

    // The leadership moves; the new leader starts from the old one's view.
    sim.admin(leader, &["transfer-leader"]).await.unwrap();
    let mut next = sim.leader().await;
    while next == leader {
        tokio::time::sleep(Duration::from_millis(50)).await;
        next = sim.leader().await;
    }
    assert_eq!(peers[next].coordination.recorded("lobby"), Some(running(1)));
    let new_term = peers[next].coordination.acting_term().unwrap();
    assert!(new_term > term);
    assert!(peers[leader].coordination.acting_term().is_none());

    // A write the deposed leader decided in its term is refused, wherever
    // it is sent from, through the store as leader tasks make it.
    for index in [leader, next] {
        let store = peers[index].store.clone();
        let refused = off_thread(move || {
            leader::with_thread_term(Some(term), || store.set_server_state("lobby", running(9)))
        })
        .await
        .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");
    }
    // The current leader's term is accepted.
    let store = peers[next].store.clone();
    off_thread(move || {
        leader::with_thread_term(Some(new_term), || {
            store.set_server_state("lobby", running(2))
        })
    })
    .await
    .unwrap();
    for peer in &peers {
        let coordination = peer.coordination.clone();
        until(Duration::from_secs(5), move || {
            coordination.recorded("lobby") == Some(running(2))
        })
        .await;
    }
    stop(peers).await;
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn followers_hand_requests_to_the_leader_and_mirror_its_record() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    let follower = (0..3).find(|index| *index != leader).unwrap();
    let peers: Vec<Peer> = (0..3).map(|index| coordinate(&sim, index)).collect();
    let automatic = |peer: &Peer| {
        peer.managed
            .snapshot()
            .into_iter()
            .find(|server| server.name == "game-1")
            .map(|server| server.automatic_enabled)
    };

    // An administrator stop on a follower runs on the leader's worker, and
    // every peer mirrors the stop the leader records.
    peers[follower].managed.stop("game-1").await.unwrap();
    assert_eq!(automatic(&peers[leader]), Some(false));
    for peer in &peers {
        let managed = peer.managed.clone();
        until(Duration::from_secs(30), move || {
            managed
                .snapshot()
                .into_iter()
                .any(|server| server.name == "game-1" && !server.automatic_enabled)
        })
        .await;
    }

    // A start is refused by the leader, whose node is unreachable, and the
    // refusal comes back to the follower.
    let refused = peers[follower].managed.start("game-1").await.unwrap_err();
    assert!(
        refused.to_string().contains("node a is unreachable"),
        "{refused}"
    );
    let last_error = peers[leader]
        .managed
        .snapshot()
        .into_iter()
        .find(|server| server.name == "game-1")
        .and_then(|server| server.last_error);
    assert_eq!(last_error.as_deref(), Some("node a is unreachable"));
    stop(peers).await;
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn an_idle_stop_needs_every_peer_and_an_inbound_player_vetoes_it() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    let follower = (0..3).find(|index| *index != leader).unwrap();
    let peers: Vec<Peer> = (0..3).map(|index| coordinate(&sim, index)).collect();

    // A player on its way to the server through a follower vetoes.
    let lease = peers[follower]
        .managed
        .reserve("game-1")
        .unwrap()
        .expect("a managed server");
    assert!(!peers[leader].coordination.may_stop("game-1").await);
    drop(lease);
    assert!(peers[leader].coordination.may_stop("game-1").await);
    // Having agreed, the follower routes no new player there for a while.
    assert!(peers[follower].managed.reserve("game-1").is_err());

    // A peer the leader cannot ask counts as a veto.
    sim.isolate(follower);
    assert!(!peers[leader].coordination.may_stop("game-1").await);
    sim.heal();
    stop(peers).await;
    sim.shutdown().await;
}

#[test]
fn a_cluster_refuses_servers_on_the_proxy_host() {
    config().check_cluster_placement().unwrap();
    let local = CONFIG.replace(",nodes={'a'}", "");
    let error = Config::from_lua(&local, "local.lua")
        .unwrap()
        .check_cluster_placement()
        .unwrap_err();
    assert!(error.to_string().contains("service_groups.game"), "{error}");
}
