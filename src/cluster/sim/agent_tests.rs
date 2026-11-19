//! Leader work across leader changes on three simulated peers (plan step
//! 7): one demand makes one instance although the leadership moves in the
//! middle of its creation, a node agent is fenced and reconciled by every
//! new leader and refuses the old term's control commands but not
//! inspection, and the servers an expired peer's players were on are held
//! back from stops until the partition grace ends.
//!
//! The stores' synchronous APIs block their thread until the write is
//! applied, so the tests call them on a thread of their own, as the
//! database tests do.
use super::Sim;
use crate::{
    cluster::{
        leader::{self, Coordination},
        players::{self as cluster_players, Presence},
    },
    config::{ClusterConfig, Config, PermissionsConfig},
    database::params,
    extensions::{PlayerAction, PlayerHandle, Players},
    instance_store::Store,
    managed::{Coordinator, ManagedServers},
    messaging::Broker,
    node::{
        Nodes,
        fence::Fence,
        protocol::{self, Command, NodeState},
    },
    permissions::Authority,
    players::{Admission, PlayerRegistration, PlayerRegistry},
    provisioning::Plan,
    service_state::ServiceStates,
};
use bytes::Bytes;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Instant,
};

/// The node every instance and agent of these tests is on.
const NODE: &str = "a";

/// Nodes need the QUIC messaging listener (validation only: nothing binds
/// it here).
const CONFIG: &str = "rift.config.listeners.public='127.0.0.1:0'
rift.config.routes.public='game'
rift.config.messaging={listen='127.0.0.1:0',certificate='c.pem',private_key='k.pem'}
rift.config.nodes.a={address='10.0.0.1'}
rift.config.templates.t={server_jar='paper.jar'}
rift.config.service_groups.game={template='t',storage='disposable',port_range={30000,30099},nodes={'a'}}";

/// Every peer's configuration: group `game` on node `a`, with instance
/// `game-1` registered if `instance`.
fn config(instance: bool) -> Config {
    let mut config = Config::from_lua(CONFIG, "agent.lua").unwrap();
    if instance {
        config
            .add_instance_on("game", "game-1", 30000, Some(NODE))
            .unwrap();
    }
    config
}

/// Run `work` on a thread outside the runtime and wait for it.
async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver.await.expect("the thread finished")
}

/// Poll `probe` on the virtual clock until its first part holds; after
/// `timeout` the panic names `what` and shows the second part, the state
/// the test saw last.
async fn eventually<T: Debug>(what: &str, timeout: Duration, probe: impl Fn() -> (bool, T)) {
    let deadline = Instant::now() + timeout;
    loop {
        let (done, seen) = probe();
        if done {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: not reached in {timeout:?}; last seen: {seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The peer that leads with a lease once it is not `old` any more.
async fn next_leader(sim: &Sim, old: usize) -> usize {
    loop {
        let next = sim.leader().await;
        if next != old {
            return next;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One peer's managed servers (no workers), coordinated through its
/// cluster.
struct Peer {
    store: Arc<Store>,
    coordination: Coordination,
}

fn coordinate(sim: &Sim, index: usize, config: &Config) -> Peer {
    let store = Arc::new(Store::new(sim.database(index)).unwrap());
    let managed = ManagedServers::new(config, Arc::new(PlayerRegistry::default()));
    let coordination = Coordination::start(sim.peer(index), &managed, &store).unwrap();
    Peer {
        store,
        coordination,
    }
}

/// The instance records on `store`'s replica: name and phase, by name.
fn phases(store: &Store) -> Vec<(String, String)> {
    store
        .database()
        .read()
        .query_map(
            "SELECT name, phase FROM instances ORDER BY name",
            params![],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap()
}

/// What one peer's control loop tried: the term it decided in, the
/// instance it began and how the creation ended.
#[derive(Debug, Clone)]
struct Attempt {
    peer: usize,
    term: Option<u64>,
    name: String,
    outcome: Result<(), io::ErrorKind>,
}

/// Record the creation of a new instance of `group` on node `a`, as the
/// leader does before it asks the node to prepare the directory: allocate
/// a name and a free port and write the `creating` record.
fn begin(store: &Store, config: &Config, group: &str) -> io::Result<String> {
    let name = store.allocate_name(config, group)?;
    let reserved = store.reserved_ports_on(Some(NODE))?;
    let port = (30000..=30099)
        .find(|port| !reserved.contains(port))
        .ok_or_else(|| io::Error::other("the port range is exhausted"))?;
    let mut config = config.clone();
    config.add_instance_on(group, &name, port, Some(NODE))?;
    let directory = config.managed_servers[&name].directory.clone();
    let plan = Plan {
        directory,
        marker: None,
        stage: None,
        created: false,
    };
    store.begin_creation(&config, &name, &plan)?;
    Ok(name)
}

/// One instance creation as the leader runs it, in the term of the task
/// that runs it: record it, wait until the node answered (`nodes` turns
/// true), commit it. The name it began, and how it ended.
async fn create(
    store: Arc<Store>,
    config: Arc<Config>,
    group: &'static str,
    mut nodes: watch::Receiver<bool>,
) -> (String, io::Result<()>) {
    let term = leader::current_term();
    let begun = {
        let store = store.clone();
        off_thread(move || leader::with_thread_term(term, || begin(&store, &config, group))).await
    };
    let name = match begun {
        Ok(name) => name,
        Err(error) => return (String::new(), Err(error)),
    };
    let answered = nodes.wait_for(|answered| *answered).await.is_ok();
    if !answered {
        return (name, Err(io::Error::other("the test ended")));
    }
    let committed = {
        let name = name.clone();
        off_thread(move || leader::with_thread_term(term, || store.commit_creation(&name))).await
    };
    (name, committed)
}

/// The demand: one instance of group `game`. An instance counts from the
/// moment its creation is recorded, as a leader counts its own creations
/// in flight.
fn wanted(store: &Store) -> Option<&'static str> {
    phases(store).is_empty().then_some("game")
}

/// A peer's control loop, as `runtime::serve` runs the scaler: every tick,
/// decide through [`leader::decide`] and run what was decided in its term.
async fn control_loop(
    index: usize,
    coordination: Coordination,
    store: Arc<Store>,
    config: Arc<Config>,
    nodes: watch::Receiver<bool>,
    attempts: Arc<Mutex<Vec<Attempt>>>,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    loop {
        tick.tick().await;
        let Some(decision) = leader::decide(Some(&coordination), |_| wanted(&store)) else {
            continue;
        };
        let acting = decision.acting;
        let work = create(
            store.clone(),
            config.clone(),
            decision.change,
            nodes.clone(),
        );
        let (name, outcome) = acting.run(work).await;
        attempts.lock().unwrap().push(Attempt {
            peer: index,
            term: acting.term(),
            name,
            outcome: outcome.map_err(|error| error.kind()),
        });
    }
}

/// What a new leader does once it caught up with its term, as
/// `cluster_runtime::take_charge` does: abort the creations the previous
/// leader left unfinished.
fn recover_on_gain(sim: &Sim, index: usize, store: &Arc<Store>, config: &Arc<Config>) {
    let store = Arc::downgrade(store);
    let config = config.clone();
    sim.peer(index).on_leader_gain(move |context| {
        let (store, config) = (store.clone(), config.clone());
        async move {
            if !leader::caught_up(&context).await {
                return;
            }
            let Some(store) = store.upgrade() else {
                return;
            };
            let term = context.term;
            let recovered =
                off_thread(move || leader::with_thread_term(Some(term), || store.recover(&config)))
                    .await;
            if let Err(error) = recovered {
                eprintln!("p{index}: recovery in term {term} failed: {error}");
            }
        }
    });
}

#[tokio::test(start_paused = true)]
async fn one_demand_makes_one_instance_across_a_leader_change() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let old = sim.leader().await;
    let config = Arc::new(config(false));
    let peers: Vec<Peer> = (0..3)
        .map(|index| coordinate(&sim, index, &config))
        .collect();
    for (index, peer) in peers.iter().enumerate() {
        recover_on_gain(&sim, index, &peer.store, &config);
    }
    let (answer, nodes) = watch::channel(false);
    let attempts: Arc<Mutex<Vec<Attempt>>> = Arc::default();
    let loops: Vec<JoinHandle<()>> = peers
        .iter()
        .enumerate()
        .map(|(index, peer)| {
            tokio::spawn(control_loop(
                index,
                peer.coordination.clone(),
                peer.store.clone(),
                config.clone(),
                nodes.clone(),
                attempts.clone(),
            ))
        })
        .collect();

    // The leader decides and records the creation; the node has not
    // answered yet.
    let store = peers[old].store.clone();
    eventually(
        "the leader records a creation",
        Duration::from_secs(10),
        || {
            let seen = phases(&store);
            let creating = matches!(seen.as_slice(), [(_, phase)] if phase == "creating");
            (creating, seen)
        },
    )
    .await;
    let begun = phases(&store)[0].0.clone();
    let old_term = peers[old]
        .coordination
        .acting_term()
        .expect("the old leader acts");

    // The leadership moves in the middle of the creation.
    sim.admin(old, &["transfer-leader"]).await.unwrap();
    let new = next_leader(&sim, old).await;
    let new_term = peers[new]
        .coordination
        .acting_term()
        .expect("the new leader acts");
    assert!(new_term > old_term);
    assert!(peers[old].coordination.acting_term().is_none());

    // Now the node answers: the deposed leader's creation, and every
    // creation after it.
    answer.send_replace(true);
    for (index, peer) in peers.iter().enumerate() {
        eventually(
            &format!("p{index} has one ready instance"),
            Duration::from_secs(20),
            || {
                let seen = phases(&peer.store);
                let ready = matches!(seen.as_slice(), [(_, phase)] if phase == "ready");
                (ready, seen)
            },
        )
        .await;
    }
    eventually(
        "the deposed leader's creation ended",
        Duration::from_secs(10),
        || {
            let attempts = attempts.lock().unwrap().clone();
            (attempts.iter().any(|attempt| attempt.peer == old), attempts)
        },
    )
    .await;
    // Nothing else is created later.
    tokio::time::sleep(Duration::from_secs(3)).await;
    for handle in loops {
        handle.abort();
        let _ = handle.await;
    }
    let attempts = attempts.lock().unwrap().clone();
    for peer in &peers {
        let seen = phases(&peer.store);
        assert!(
            matches!(seen.as_slice(), [(_, phase)] if phase == "ready"),
            "{seen:?}; {attempts:?}"
        );
    }
    let ready = phases(&peers[new].store)[0].0.clone();
    let created: Vec<&Attempt> = attempts
        .iter()
        .filter(|attempt| attempt.outcome.is_ok())
        .collect();
    assert_eq!(created.len(), 1, "exactly one creation: {attempts:?}");
    assert_eq!(created[0].peer, new, "{attempts:?}");
    assert_eq!(created[0].term, Some(new_term), "{attempts:?}");
    assert_eq!(created[0].name, ready);
    // The deposed leader's creation was refused in its old term, and the
    // new leader aborted what it had recorded.
    assert_ne!(ready, begun);
    let deposed: Vec<&Attempt> = attempts
        .iter()
        .filter(|attempt| attempt.peer == old)
        .collect();
    assert_eq!(deposed.len(), 1, "{attempts:?}");
    assert_eq!(deposed[0].term, Some(old_term));
    assert_eq!(deposed[0].name, begun);
    assert_eq!(
        deposed[0].outcome,
        Err(io::ErrorKind::PermissionDenied),
        "{attempts:?}"
    );
    for peer in peers {
        peer.coordination.stop();
    }
    sim.shutdown().await;
}

/// A directory of its own below the temporary directory.
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "rift-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// One command a [`FakeAgent`] was sent: what it was, the term it carried
/// and whether the agent refused it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    op: &'static str,
    term: Option<u64>,
    refused: bool,
}

fn op(command: &Command) -> &'static str {
    match command {
        Command::Fence { .. } => "fence",
        Command::Reconcile { .. } => "reconcile",
        Command::Stop { .. } => "stop",
        Command::Logs { .. } => "logs",
        Command::Tree { .. } => "tree",
        Command::Read { .. } => "read",
        _ => "other",
    }
}

/// A node agent for the tests: it keeps its fence below its root with the
/// agent's [`Fence`] and gates commands as `Agent::handle_tagged` does,
/// answers every command it lets through, and remembers what it was sent.
struct FakeAgent {
    fence: Fence,
    seen: Mutex<Vec<Seen>>,
}

impl FakeAgent {
    fn handle(&self, command: &Command, term: Option<u64>) -> io::Result<Value> {
        if !command.is_inspection() && !matches!(command, Command::Fence { .. }) {
            self.fence.check(term)?;
        }
        Ok(match command {
            Command::Fence { term } => {
                let term = self.fence.raise(*term)?;
                json!({ "term": term })
            }
            Command::Reconcile { .. } => json!({ "stopped": [], "removed": [] }),
            Command::Logs { .. } => json!({ "text": "Done!\n", "cursor": 6 }),
            _ => json!({}),
        })
    }

    /// The reply to one encoded command.
    fn answer(&self, payload: &[u8]) -> Value {
        let result = protocol::decode(payload).and_then(|(command, term)| {
            let result = self.handle(&command, term);
            self.seen.lock().unwrap().push(Seen {
                op: op(&command),
                term,
                refused: result.is_err(),
            });
            result
        });
        protocol::reply(result)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// Serve `agent` as node `a` on `broker`: answer its commands and publish
/// its state, with every feature, as often as agents do.
async fn serve(agent: Arc<FakeAgent>, broker: Broker) {
    let mut commands = broker
        .subscribe(&protocol::command_subject(NODE), None)
        .unwrap();
    let state = NodeState {
        agent: "fake-agent".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        features: protocol::FEATURES
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect(),
        revision: 1,
        servers: Vec::new(),
        exits: Vec::new(),
        resources: None,
    };
    let state = Bytes::from(serde_json::to_vec(&state).unwrap());
    let mut report = tokio::time::interval(protocol::STATE_INTERVAL);
    loop {
        tokio::select! {
            _ = report.tick() => {
                let _ = broker.publish(&protocol::state_subject(NODE), state.clone());
            }
            message = commands.recv() => {
                let Ok(message) = message else { return };
                if let Some(reply) = &message.reply {
                    let answer = agent.answer(&message.payload).to_string();
                    let _ = broker.publish(reply, Bytes::from(answer));
                }
            }
        }
    }
}

/// Send `command` tagged with `term` to node `a` directly on `broker`, as a
/// peer that still believes it leads in `term` would.
async fn deliver(broker: &Broker, command: &Command, term: Option<u64>) -> io::Result<Value> {
    let payload = protocol::encode(command, term)?;
    let reply = broker
        .request(
            &protocol::command_subject(NODE),
            Bytes::from(payload),
            Duration::from_secs(5),
        )
        .await
        .map_err(io::Error::other)?;
    protocol::parse_reply(&reply.payload)
}

/// Whether the agent accepted a fence and then a reconcile of `term`.
fn reconciled(seen: &[Seen], term: u64) -> bool {
    let accepted = |op: &str| {
        seen.iter()
            .position(|seen| seen.op == op && seen.term == Some(term) && !seen.refused)
    };
    matches!(
        (accepted("fence"), accepted("reconcile")),
        (Some(fence), Some(reconcile)) if fence < reconcile
    )
}

#[tokio::test(start_paused = true)]
async fn a_new_leader_fences_and_reconciles_the_agent_which_refuses_the_old_term() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let old = sim.leader().await;
    let config = config(false);
    // The node's bus: every peer reaches the agent and hears its state, so
    // a new leader observed the agent already and sees no reconnect.
    let broker = Broker::default();
    let root = scratch("fenced-agent");
    let agent = Arc::new(FakeAgent {
        fence: Fence::open(&root).unwrap(),
        seen: Mutex::default(),
    });
    let nodes: Vec<Arc<Nodes>> = (0..3)
        .map(|index| {
            let nodes = Arc::new(Nodes::new(broker.clone(), &config));
            let cluster = sim.peer(index).clone();
            nodes.set_cluster(Arc::new(move || leader::acting_term(&cluster)));
            nodes
        })
        .collect();
    let listeners: Vec<JoinHandle<()>> = nodes
        .iter()
        .map(|nodes| tokio::spawn(nodes.listen().unwrap()))
        .collect();
    // Each peer's leader task, as `cluster_runtime::lead_term` runs it.
    for (index, nodes) in nodes.iter().enumerate() {
        let weak = Arc::downgrade(nodes);
        sim.peer(index).on_leader_gain(move |context| {
            let nodes = weak.upgrade().map(|nodes| (*nodes).clone());
            async move {
                let Some(nodes) = nodes else { return };
                if !leader::caught_up(&context).await {
                    return;
                }
                leader::fence_and_reconcile(
                    context,
                    nodes,
                    || vec![NODE.to_owned()],
                    |_| vec!["game-1".to_owned()],
                )
                .await;
            }
        });
    }
    let serving = tokio::spawn(serve(agent.clone(), broker.clone()));

    let old_term = leader::acting_term(sim.peer(old)).expect("the old leader acts");
    eventually(
        "the leader fences and reconciles the agent",
        Duration::from_secs(15),
        || {
            let seen = agent.seen();
            (reconciled(&seen, old_term), seen)
        },
    )
    .await;
    assert_eq!(agent.fence.term(), old_term);
    for (index, nodes) in nodes.iter().enumerate() {
        assert!(nodes.online(NODE), "p{index} observes the agent");
    }

    // The leadership moves: the new leader fences with its term first,
    // then reconciles, although it saw the agent all along.
    sim.admin(old, &["transfer-leader"]).await.unwrap();
    let new = next_leader(&sim, old).await;
    let new_term = leader::acting_term(sim.peer(new)).expect("the new leader acts");
    assert!(new_term > old_term);
    eventually(
        "the new leader fences and reconciles the agent",
        Duration::from_secs(15),
        || {
            let seen = agent.seen();
            (reconciled(&seen, new_term), seen)
        },
    )
    .await;
    assert_eq!(agent.fence.term(), new_term);
    assert_eq!(
        Fence::open(&root).unwrap().term(),
        new_term,
        "the fence is on disk"
    );
    assert_eq!(nodes[new].fenced_term(NODE), Some(new_term));

    // The deposed leader sends no control command any more, and one that
    // still carries its term is refused by the agent, as is one without.
    let stop = Command::Stop {
        server: "game-1".into(),
        timeout_ms: 0,
    };
    // (The leaders' reconcile loops keep running meanwhile.)
    let stops = || -> Vec<Seen> {
        agent
            .seen()
            .into_iter()
            .filter(|seen| seen.op == "stop")
            .collect()
    };
    let refused = nodes[old]
        .request(NODE, &stop, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");
    assert!(stops().is_empty(), "nothing reached the agent");
    let stale = deliver(&broker, &stop, Some(old_term)).await.unwrap_err();
    assert_eq!(stale.kind(), io::ErrorKind::PermissionDenied, "{stale}");
    assert!(stale.to_string().contains("deposed leader"), "{stale}");
    let untagged = deliver(&broker, &stop, None).await.unwrap_err();
    assert_eq!(
        untagged.kind(),
        io::ErrorKind::PermissionDenied,
        "{untagged}"
    );
    // The new leader's control commands go through.
    nodes[new]
        .request(NODE, &stop, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(
        stops(),
        [
            Seen {
                op: "stop",
                term: Some(old_term),
                refused: true,
            },
            Seen {
                op: "stop",
                term: None,
                refused: true,
            },
            Seen {
                op: "stop",
                term: Some(new_term),
                refused: false,
            },
        ]
    );

    // Inspection is never fenced: every other peer still reads logs, trees
    // and files, and even the old term does not stop it.
    let directory = PathBuf::from("services/game-1");
    let inspections = [
        Command::Logs {
            server: "game-1".into(),
            directory: directory.clone(),
            cursor: None,
        },
        Command::Tree {
            server: "game-1".into(),
            directory: directory.clone(),
            offset: 0,
        },
        Command::Read {
            server: "game-1".into(),
            directory,
            path: "server.properties".into(),
            offset: 0,
            snapshot: false,
        },
    ];
    for index in (0..3).filter(|index| *index != new) {
        for command in &inspections {
            nodes[index]
                .request(NODE, command, Duration::from_secs(5))
                .await
                .unwrap_or_else(|error| panic!("p{index}, {}: {error}", op(command)));
        }
    }
    let logs = deliver(&broker, &inspections[0], Some(old_term))
        .await
        .unwrap();
    assert_eq!(logs["text"], "Done!\n");
    let inspected: Vec<Seen> = agent
        .seen()
        .into_iter()
        .filter(|seen| matches!(seen.op, "logs" | "tree" | "read"))
        .collect();
    assert_eq!(inspected.len(), 7, "{inspected:?}");
    assert!(inspected.iter().all(|seen| !seen.refused), "{inspected:?}");

    serving.abort();
    let _ = serving.await;
    for listener in listeners {
        listener.abort();
        let _ = listener.await;
    }
    drop(nodes);
    sim.shutdown().await;
    let _ = std::fs::remove_dir_all(root);
}

/// Clients new enough for every action.
const PROTOCOL: i32 = 774;

/// One peer's proxy side: its players, with an in-memory permission store.
fn proxy_players() -> Players {
    let authority = Arc::new(Authority::memory(&PermissionsConfig::default()));
    Players::new(
        Arc::new(Mutex::new(BTreeSet::from([
            "game-1".to_owned(),
            "lobby".to_owned(),
        ]))),
        Arc::new(Mutex::new(BTreeMap::new())),
        Broker::default(),
        authority,
    )
}

/// Log a player in as a session does: name claim, UUID claim, join.
async fn login(
    players: &Players,
    counter: u64,
    uuid: u8,
    name: &str,
    server: &str,
) -> (
    PlayerRegistration,
    PlayerHandle,
    mpsc::Receiver<PlayerAction>,
) {
    let roster = players.roster().clone();
    let connection = roster.identity().connection_id(counter);
    let reservation = roster.claim_name(name, connection).await.unwrap();
    let registration = reservation
        .claim_player([uuid; 16], server, None, false)
        .await
        .unwrap();
    let (handle, actions) = players.register(connection, &[uuid; 16], name, PROTOCOL, server);
    (registration, handle, actions)
}

#[tokio::test(start_paused = true)]
async fn an_expired_peers_servers_are_held_from_stops_until_the_grace_ends() {
    const GRACE: Duration = Duration::from_secs(20);
    let mut sim = Sim::new(3);
    sim.shared = ClusterConfig {
        peer_timeout: Duration::from_secs(3),
        partition_grace: GRACE,
        ..ClusterConfig::default()
    };
    let players: Vec<Players> = (0..3).map(|_| proxy_players()).collect();
    let rosters: Vec<Arc<PlayerRegistry>> = players
        .iter()
        .map(|players| players.roster().clone())
        .collect();
    let shared = sim.shared.clone();
    sim.components = Some(Arc::new(move |index: usize| {
        cluster_players::components(&rosters[index], &shared)
    }));
    sim.form(3).await;
    let mut presences: Vec<Option<Arc<Presence>>> = Vec::new();
    for (index, players) in players.iter().enumerate() {
        let presence = cluster_players::attach(
            sim.peer(index),
            players.roster(),
            players,
            &ServiceStates::default(),
            None,
            false,
        )
        .await
        .unwrap();
        presences.push(Some(Arc::new(presence)));
    }
    for (index, players) in players.iter().enumerate() {
        eventually(
            &format!("p{index} admits players"),
            Duration::from_secs(20),
            || {
                let admission = players.roster().admission();
                (admission == Admission::Open, admission)
            },
        )
        .await;
    }
    let leader = sim.leader().await;
    let victim = (0..3).find(|index| *index != leader).unwrap();
    let other = (0..3)
        .find(|index| *index != leader && *index != victim)
        .unwrap();
    let config = config(true);
    let mut peers: Vec<Option<Peer>> = (0..3)
        .map(|index| Some(coordinate(&sim, index, &config)))
        .collect();
    for (peer, presence) in peers.iter().zip(&presences) {
        if let (Some(peer), Some(presence)) = (peer, presence) {
            peer.coordination.set_presence(presence.clone());
        }
    }

    // Bob plays on game-1 through the victim; every peer knows where.
    let _bob = login(&players[victim], 1, 2, "Bob", "game-1").await;
    for index in [leader, other] {
        eventually(
            &format!("p{index} sees Bob on game-1"),
            Duration::from_secs(5),
            || {
                let server = players[index]
                    .roster()
                    .get(&[2; 16])
                    .map(|player| player.server);
                (server.as_deref() == Some("game-1"), server)
            },
        )
        .await;
    }
    let lead = presences[leader].clone().unwrap();
    let follow = presences[other].clone().unwrap();
    assert!(lead.stop_holds().is_empty());

    // The victim dies; the leader expires it.
    if let Some(peer) = peers[victim].take() {
        peer.coordination.stop();
    }
    if let Some(presence) = presences[victim].take() {
        presence.stop();
    }
    sim.crash(victim).await;
    let cluster = sim.peer(leader).clone();
    let name = sim.name(victim);
    eventually(
        "the leader expires the victim",
        Duration::from_secs(20),
        || {
            let rows = cluster.peers().unwrap();
            let expired = rows.iter().any(|row| row.name == name && row.expired);
            (expired, rows)
        },
    )
    .await;

    // Every replica holds game-1, where Bob was, until the grace ends.
    eventually("the leader holds game-1", Duration::from_secs(5), || {
        (lead.is_held("game-1"), lead.stop_holds())
    })
    .await;
    let until = lead.stop_holds()["game-1"];
    let now = Instant::now();
    assert!(
        until > now && until <= now + GRACE,
        "the hold ends {:?} from now",
        until.saturating_duration_since(now)
    );
    assert!(follow.is_held("game-1"), "every replica holds it");
    assert!(!lead.is_held("lobby"));
    let coordination = peers[leader].as_ref().unwrap().coordination.clone();
    assert!(!coordination.may_stop("game-1").await, "held back");
    // A server no expired player was on may stop: the other peer agrees,
    // and the expired one is not asked.
    assert!(coordination.may_stop("lobby").await);

    // Once the grace passed, game-1 may stop again.
    tokio::time::sleep_until(until + Duration::from_millis(100)).await;
    assert!(!lead.is_held("game-1"));
    assert!(lead.stop_holds().is_empty());
    assert!(
        coordination.may_stop("game-1").await,
        "the hold ended with the grace"
    );

    drop(coordination);
    for peer in peers.into_iter().flatten() {
        peer.coordination.stop();
    }
    for presence in presences.into_iter().flatten() {
        presence.stop();
    }
    drop((lead, follow));
    sim.shutdown().await;
}
