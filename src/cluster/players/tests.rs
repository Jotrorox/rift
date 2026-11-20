//! Presence and routed actions on simulated clusters: real peers on the
//! fault-injecting in-memory network, each with a real [`PlayerRegistry`]
//! and [`Players`], on the paused tokio clock.
//!
//! [`PlayerRegistry`]: crate::players::PlayerRegistry
use super::{Presence, attach, components};
use crate::{
    cluster::{
        Cluster, EntryKind,
        sim::{Sim, until},
    },
    config::{ClusterConfig, PermissionsConfig},
    dialog::Owner,
    extensions::{
        Context, PlayerAction, PlayerHandle, Players,
        routing::{RouteFuture, SessionHooks},
    },
    messaging::Broker,
    permissions::Authority,
    players::{Admission, NameReservation, PlayerRegistration, claims::Claims},
    service_state::ServiceStates,
    text::Text,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};

/// Clients new enough for every action.
const PROTOCOL: i32 = 774;
/// The context key backends may report in these tests.
const ARENA: &str = "arena";

/// One peer's proxy side, made anew for every start of the peer.
#[derive(Clone)]
struct Peer {
    players: Players,
    authority: Arc<Authority>,
    states: ServiceStates,
}

impl Peer {
    fn new() -> Self {
        let authority = Arc::new(Authority::memory(&PermissionsConfig {
            external_contexts: BTreeSet::from([ARENA.to_owned()]),
            ..PermissionsConfig::default()
        }));
        let players = Players::new(
            Arc::new(Mutex::new(["lobby", "survival"].map(String::from).into())),
            Arc::new(Mutex::new(BTreeMap::new())),
            Broker::new(Default::default()).unwrap(),
            authority.clone(),
        );
        Self {
            players,
            authority,
            states: ServiceStates::default(),
        }
    }
}

/// A simulated cluster whose peers serve players.
struct Net {
    sim: Sim,
    count: usize,
    peers: Arc<Mutex<BTreeMap<usize, Peer>>>,
    presences: BTreeMap<usize, Presence>,
}

impl Net {
    fn new(count: usize, config: ClusterConfig) -> Self {
        let mut sim = Sim::new(count);
        sim.shared = config.clone();
        let peers: Arc<Mutex<BTreeMap<usize, Peer>>> = Arc::default();
        let made = peers.clone();
        sim.components = Some(Arc::new(move |index: usize| {
            match made.lock().unwrap().get(&index) {
                Some(peer) => components(peer.players.roster(), &config),
                None => Vec::new(),
            }
        }));
        let net = Self {
            sim,
            count,
            peers,
            presences: BTreeMap::new(),
        };
        for index in 0..count {
            net.fresh(index);
        }
        net
    }

    /// The default simulated settings: `peer_timeout` 3 s.
    fn config() -> ClusterConfig {
        ClusterConfig {
            peer_timeout: Duration::from_secs(3),
            ..ClusterConfig::default()
        }
    }

    fn fresh(&self, index: usize) {
        self.peers.lock().unwrap().insert(index, Peer::new());
    }

    fn peer(&self, index: usize) -> Peer {
        self.peers.lock().unwrap()[&index].clone()
    }

    fn players(&self, index: usize) -> Players {
        self.peer(index).players
    }

    fn cluster(&self, index: usize) -> Cluster {
        self.sim.peer(index).clone()
    }

    async fn attach(&mut self, index: usize) {
        let peer = self.peer(index);
        let presence = attach(
            self.sim.peer(index),
            peer.players.roster(),
            &peer.players,
            &peer.states,
            None,
            false,
        )
        .await
        .unwrap();
        self.presences.insert(index, presence);
    }

    /// Form the cluster with `voters` voters and wait until every peer
    /// admits players.
    async fn form(&mut self, voters: usize) {
        self.sim.form(voters).await;
        for index in 0..self.count {
            self.attach(index).await;
        }
        for index in 0..self.count {
            self.open(index).await;
        }
    }

    /// Wait until peer `index` admits players; the panic says why it did
    /// not.
    async fn open(&self, index: usize) {
        let roster = self.players(index).roster().clone();
        let deadline = Instant::now() + Duration::from_secs(20);
        while roster.admission() != Admission::Open {
            assert!(
                Instant::now() < deadline,
                "peer {index} does not admit players: {:?}, {}",
                roster.admission(),
                self.presences
                    .get(&index)
                    .and_then(Presence::held_back)
                    .unwrap_or_else(|| "no reason recorded (not attached?)".to_owned())
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn crash(&mut self, index: usize) {
        if let Some(presence) = self.presences.remove(&index) {
            presence.stop();
        }
        self.sim.crash(index).await;
    }

    async fn restart(&mut self, index: usize) {
        self.crash(index).await;
        self.fresh(index);
        self.sim.start(index).await.unwrap();
        self.attach(index).await;
    }

    /// A running peer other than `but`.
    fn other(&self, but: &[usize]) -> usize {
        (0..self.count)
            .find(|index| !but.contains(index))
            .expect("another peer")
    }

    async fn shutdown(mut self) {
        for presence in std::mem::take(&mut self.presences).into_values() {
            presence.stop();
        }
        self.sim.shutdown().await;
    }
}

struct Session {
    connection: u64,
    registration: PlayerRegistration,
    _handle: PlayerHandle,
    actions: mpsc::Receiver<PlayerAction>,
}

/// Log a player in as a session does: name claim, UUID claim, join.
async fn login(
    players: &Players,
    counter: u64,
    uuid: u8,
    name: &str,
    server: &str,
) -> io::Result<Session> {
    let roster = players.roster().clone();
    let connection = roster.identity().connection_id(counter);
    let reservation = roster.claim_name(name, connection).await?;
    let registration = reservation
        .claim_player([uuid; 16], server, None, false)
        .await?;
    let (handle, actions) = players.register(connection, &[uuid; 16], name, PROTOCOL, server);
    Ok(Session {
        connection,
        registration,
        _handle: handle,
        actions,
    })
}

async fn next(session: &mut Session) -> PlayerAction {
    tokio::time::timeout(Duration::from_secs(15), session.actions.recv())
        .await
        .expect("an action arrives")
        .expect("the session is open")
}

/// Whether `players` sees a live claim of the lowercase name `key`.
fn claimed(players: &Players, key: &str) -> bool {
    Claims::live(&*players.roster().claims())
        .iter()
        .any(|record| record.key == key)
}

fn expired(cluster: &Cluster, name: &str) -> bool {
    cluster
        .state()
        .peers()
        .unwrap()
        .iter()
        .any(|row| row.name == name && row.expired)
}

struct Hooks;

impl SessionHooks for Hooks {
    fn transfer(
        &self,
        connection: u64,
        backend: String,
        admin: bool,
    ) -> RouteFuture<Result<Value, String>> {
        Box::pin(std::future::ready(Ok(
            json!({"connection_id": connection, "backend": backend, "admin": admin}),
        )))
    }

    fn dialog(&self, _: Context, _: Owner) -> RouteFuture<io::Result<()>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

#[tokio::test(start_paused = true)]
async fn simultaneous_logins_on_two_peers_let_exactly_one_win() {
    let mut net = Net::new(3, Net::config());
    net.form(3).await;
    let (a, b) = (net.players(0), net.players(1));
    // One name in two spellings (offline mode, two UUIDs).
    let (first, second) = tokio::join!(
        login(&a, 1, 1, "Alice", "lobby"),
        login(&b, 1, 2, "aLiCe", "lobby")
    );
    assert_eq!(u8::from(first.is_ok()) + u8::from(second.is_ok()), 1);
    let error = first
        .as_ref()
        .err()
        .or_else(|| second.as_ref().err())
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    // One UUID under two names.
    let (third, fourth) = tokio::join!(
        login(&a, 2, 3, "Bob", "lobby"),
        login(&b, 2, 3, "Robert", "lobby")
    );
    assert_eq!(u8::from(third.is_ok()) + u8::from(fourth.is_ok()), 1);
    let error = third
        .as_ref()
        .err()
        .or_else(|| fourth.as_ref().err())
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    // Every replica agrees, and the losers left nothing behind.
    for index in 0..3 {
        let players = net.players(index);
        until(Duration::from_secs(5), move || {
            let live = Claims::live(&*players.roster().claims());
            live.len() == 2 && live.iter().all(|record| record.uuid.is_some())
        })
        .await;
    }
    drop((first, second, third, fourth));
    let players = net.players(2);
    until(Duration::from_secs(5), move || {
        Claims::live(&*players.roster().claims()).is_empty()
    })
    .await;
    net.shutdown().await;
}

/// The decision the runtime makes before a backend sees a login
/// (`PlayerRegistry::claims_before_backend`), for a configuration a single
/// Rift passes through without reading the name (no network, hooks,
/// managed servers or online mode, as in tests/cluster.rs): a peer's
/// registry, once attached, makes every login claim its name, so of two
/// simultaneous logins of one name on two peers exactly one reaches its
/// backend. A registry nobody attached decides alone and would not.
#[tokio::test(start_paused = true)]
async fn a_plain_proxy_peer_claims_every_name_before_its_backend() {
    let single = Peer::new();
    assert!(!single.players.roster().clustered());
    assert!(!single.players.roster().claims_before_backend(false));
    let mut net = Net::new(3, Net::config());
    net.form(3).await;
    for index in 0..3 {
        let roster = net.players(index).roster().clone();
        assert!(roster.clustered(), "peer {index}");
        assert!(roster.claims_before_backend(false), "peer {index}");
    }
    /// A plain login up to the moment its backend would see it: the name
    /// it holds then, `None` when it would not have claimed one.
    async fn reach_backend(
        players: &Players,
        counter: u64,
        name: &str,
    ) -> io::Result<Option<NameReservation>> {
        let roster = players.roster().clone();
        if !roster.claims_before_backend(false) {
            return Ok(None);
        }
        let connection = roster.identity().connection_id(counter);
        roster.claim_name(name, connection).await.map(Some)
    }
    let (a, b) = (net.players(0), net.players(1));
    let (first, second) =
        tokio::join!(reach_backend(&a, 10, "Twin"), reach_backend(&b, 10, "twin"));
    let held: Vec<bool> = [&first, &second]
        .iter()
        .map(|outcome| matches!(outcome, Ok(Some(_))))
        .collect();
    assert_eq!(held.iter().filter(|held| **held).count(), 1, "{held:?}");
    let error = first
        .as_ref()
        .err()
        .or_else(|| second.as_ref().err())
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    drop((first, second));
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn the_network_cap_holds_on_every_peer_unless_bypassed() {
    let mut net = Net::new(
        3,
        ClusterConfig {
            max_players: Some(2),
            ..Net::config()
        },
    );
    net.form(3).await;
    let _first = login(&net.players(0), 1, 1, "First", "lobby")
        .await
        .unwrap();
    let _second = login(&net.players(1), 1, 2, "Second", "lobby")
        .await
        .unwrap();
    let full = login(&net.players(2), 1, 3, "Third", "lobby")
        .await
        .err()
        .expect("the network is full");
    assert_eq!(full.kind(), io::ErrorKind::QuotaExceeded);
    // The refused upgrade released the pending name.
    let players = net.players(2);
    until(Duration::from_secs(5), move || !claimed(&players, "third")).await;
    let roster = net.players(2).roster().clone();
    let connection = roster.identity().connection_id(2);
    let staff = roster
        .claim_player([4; 16], "Staff", "lobby", None, connection, true)
        .await
        .unwrap();
    assert_eq!(staff.connection(), connection);
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_killed_peer_frees_its_names_once_it_expires() {
    let mut net = Net::new(3, Net::config());
    net.form(3).await;
    let leader = net.sim.leader().await;
    let victim = net.other(&[leader]);
    let _alice = login(&net.players(victim), 1, 1, "Alice", "lobby")
        .await
        .unwrap();
    let players = net.players(leader);
    until(Duration::from_secs(5), move || claimed(&players, "alice")).await;
    net.crash(victim).await;
    let error = login(&net.players(leader), 1, 1, "Alice", "lobby")
        .await
        .err()
        .expect("still claimed");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    let cluster = net.cluster(leader);
    let name = net.sim.name(victim);
    until(Duration::from_secs(20), move || expired(&cluster, &name)).await;
    let players = net.players(leader);
    until(Duration::from_secs(5), move || !claimed(&players, "alice")).await;
    login(&net.players(leader), 2, 1, "Alice", "lobby")
        .await
        .unwrap();
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_restarted_peer_frees_its_names_at_once() {
    let mut net = Net::new(3, Net::config());
    net.form(3).await;
    let leader = net.sim.leader().await;
    let victim = net.other(&[leader]);
    let _alice = login(&net.players(victim), 1, 1, "Alice", "lobby")
        .await
        .unwrap();
    let players = net.players(leader);
    until(Duration::from_secs(5), move || claimed(&players, "alice")).await;
    let restarted = Instant::now();
    net.restart(victim).await;
    let players = net.players(leader);
    // Well before the leader would expire it: its new incarnation's
    // `peer joined` released the claims.
    until(Duration::from_secs(2), move || !claimed(&players, "alice")).await;
    assert!(restarted.elapsed() < net.sim.shared.peer_timeout);
    assert!(!expired(&net.cluster(leader), &net.sim.name(victim)));
    net.open(victim).await;
    login(&net.players(victim), 2, 1, "Alice", "lobby")
        .await
        .unwrap();
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_cut_off_peer_fences_before_it_expires_and_disconnects_after_the_grace() {
    let mut net = Net::new(
        3,
        ClusterConfig {
            partition_grace: Duration::from_secs(4),
            ..Net::config()
        },
    );
    net.form(3).await;
    let leader = net.sim.leader().await;
    let victim = net.other(&[leader]);
    let majority = net.other(&[leader, victim]);
    let mut bob = login(&net.players(victim), 1, 2, "Bob", "lobby")
        .await
        .unwrap();
    let players = net.players(leader);
    until(Duration::from_secs(5), move || claimed(&players, "bob")).await;
    let roster = net.players(victim).roster().clone();
    let cluster = net.cluster(leader);
    let name = net.sim.name(victim);
    net.sim.isolate(victim);
    let start = Instant::now();
    let (mut fenced, mut expiry) = (None, None);
    while fenced.is_none() || expiry.is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "no fence or expiry"
        );
        let now = Instant::now();
        if fenced.is_none() && matches!(roster.admission(), Admission::Fenced { .. }) {
            fenced = Some(now);
        }
        if expiry.is_none() && expired(&cluster, &name) {
            expiry = Some(now);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        fenced.unwrap() < expiry.unwrap(),
        "the peer must stop admitting before the leader expires it"
    );
    // The fenced peer refuses logins with the partition message.
    let error = login(&net.players(victim), 2, 3, "Carol", "lobby")
        .await
        .err()
        .expect("fenced");
    assert_eq!(error.kind(), io::ErrorKind::NotConnected);
    // The majority keeps working, and Bob's name is free there.
    let _carol = login(&net.players(majority), 2, 3, "Carol", "lobby")
        .await
        .unwrap();
    let players = net.players(majority);
    until(Duration::from_secs(5), move || !claimed(&players, "bob")).await;
    // After the grace the cut-off peer disconnects Bob.
    assert!(matches!(next(&mut bob).await, PlayerAction::Kick(_)));
    assert!(fenced.unwrap().elapsed() >= Duration::from_millis(3500));
    net.sim.heal();
    let rejoined = net.cluster(leader);
    let name = net.sim.name(victim);
    until(Duration::from_secs(20), move || !expired(&rejoined, &name)).await;
    net.open(victim).await;
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_rejoining_peer_loses_players_who_logged_in_elsewhere() {
    let mut net = Net::new(3, Net::config());
    net.form(3).await;
    let leader = net.sim.leader().await;
    let victim = net.other(&[leader]);
    let mut dave = login(&net.players(victim), 1, 4, "Dave", "lobby")
        .await
        .unwrap();
    let mut erin = login(&net.players(victim), 2, 5, "Erin", "lobby")
        .await
        .unwrap();
    let players = net.players(leader);
    until(Duration::from_secs(5), move || {
        claimed(&players, "dave") && claimed(&players, "erin")
    })
    .await;
    net.sim.isolate(victim);
    let cluster = net.cluster(leader);
    let name = net.sim.name(victim);
    until(Duration::from_secs(20), move || expired(&cluster, &name)).await;
    // Dave logs in again through the majority while his old peer is away.
    let _again = login(&net.players(leader), 3, 4, "Dave", "lobby")
        .await
        .unwrap();
    net.sim.heal();
    assert!(matches!(next(&mut dave).await, PlayerAction::Kick(_)));
    // Erin was claimed again by her peer and stays.
    let players = net.players(leader);
    until(Duration::from_secs(20), move || claimed(&players, "erin")).await;
    let holder = Claims::live(&*net.players(leader).roster().claims())
        .into_iter()
        .find(|record| record.key == "dave")
        .unwrap()
        .holder;
    assert_eq!(holder.peer, net.players(leader).roster().identity().peer);
    assert!(
        erin.actions.try_recv().is_err(),
        "Erin was not disconnected"
    );
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn actions_and_broadcasts_reach_players_on_other_peers() {
    let mut net = Net::new(3, Net::config());
    net.form(3).await;
    let (a, b) = (net.players(0), net.players(1));
    b.set_session_hooks(Arc::new(Hooks));
    let mut alice = login(&b, 1, 1, "Alice", "lobby").await.unwrap();
    let seen = a.clone();
    until(Duration::from_secs(5), move || {
        seen.roster().remote_by_name("alice").is_some()
    })
    .await;
    let hello = Text::Plain("hello".into());
    assert!(
        a.deliver("Alice", PlayerAction::Message(hello.clone()))
            .await
            .unwrap()
    );
    assert_eq!(next(&mut alice).await, PlayerAction::Message(hello.clone()));
    // Without waiting, as Lua and plugins send.
    assert!(
        a.send("alice", PlayerAction::ActionBar(hello.clone()))
            .unwrap()
    );
    assert_eq!(
        next(&mut alice).await,
        PlayerAction::ActionBar(hello.clone())
    );
    let transfer = a
        .route_transfer(alice.connection, "survival", true)
        .await
        .unwrap();
    assert_eq!(transfer["backend"], "survival");
    assert_eq!(transfer["admin"], true);
    // Broadcasts count the whole network.
    let broadcast = crate::text::Unrendered::new(hello.clone(), None, "message").unwrap();
    assert_eq!(a.broadcast_routed(&broadcast, None).await.unwrap(), 1);
    assert_eq!(next(&mut alice).await, PlayerAction::Message(hello));
    let reason = Text::Plain("bye".into());
    assert!(
        a.deliver("Alice", PlayerAction::Kick(reason.clone()))
            .await
            .unwrap()
    );
    assert_eq!(next(&mut alice).await, PlayerAction::Kick(reason));
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn attributes_resync_after_a_partition_heals() {
    let mut net = Net::new(3, Net::config());
    net.form(3).await;
    let leader = net.sim.leader().await;
    let victim = net.other(&[leader]);
    let erin = login(&net.players(victim), 1, 5, "Erin", "lobby")
        .await
        .unwrap();
    let observer = net.players(leader);
    until(Duration::from_secs(5), move || {
        observer
            .roster()
            .get(&[5; 16])
            .is_some_and(|player| player.server == "lobby")
    })
    .await;
    // A short blip, shorter than half the peer timeout: nobody is fenced.
    net.sim.isolate(victim);
    erin.registration.set_server("survival");
    tokio::time::sleep(Duration::from_millis(800)).await;
    net.sim.heal();
    let observer = net.players(leader);
    until(Duration::from_secs(10), move || {
        observer
            .roster()
            .get(&[5; 16])
            .is_some_and(|player| player.server == "survival")
    })
    .await;
    assert!(!expired(&net.cluster(leader), &net.sim.name(victim)));
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_new_leader_does_not_expire_every_peer() {
    let mut net = Net::new(3, Net::config());
    net.form(3).await;
    let old = net.sim.leader().await;
    let follower = net.other(&[old]);
    let _fay = login(&net.players(follower), 1, 6, "Fay", "lobby")
        .await
        .unwrap();
    let players = net.players(net.other(&[old, follower]));
    until(Duration::from_secs(5), move || claimed(&players, "fay")).await;
    net.crash(old).await;
    let new = net.sim.leader().await;
    assert_ne!(new, old);
    // Two peer timeouts later only the crashed peer expired.
    tokio::time::sleep(net.sim.shared.peer_timeout * 2).await;
    let cluster = net.cluster(new);
    assert!(!expired(&cluster, &net.sim.name(follower)));
    assert!(claimed(&net.players(new), "fay"));
    let name = net.sim.name(old);
    until(Duration::from_secs(10), move || expired(&cluster, &name)).await;
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn claims_survive_a_snapshot_install() {
    let mut net = Net::new(4, Net::config());
    net.sim.init(0).await;
    for index in 1..3 {
        net.sim.start(index).await.unwrap();
        let (name, address) = (net.sim.name(index), net.sim.address(index));
        net.sim.admin(0, &["add", &name, &address]).await.unwrap();
        net.sim.retry_admin(0, &["promote", &name]).await;
    }
    let leader = net.sim.leader().await;
    let cluster = net.cluster(leader);
    until(Duration::from_secs(10), || {
        cluster.is_enabled(EntryKind::Claim).unwrap_or(false)
    })
    .await;
    for index in 0..3 {
        net.attach(index).await;
    }
    for index in 0..3 {
        net.open(index).await;
    }
    let _gus = login(&net.players(1), 1, 7, "Gus", "lobby").await.unwrap();
    let players = net.players(leader);
    until(Duration::from_secs(5), move || claimed(&players, "gus")).await;
    let raft = cluster.raft().unwrap().clone();
    raft.trigger().snapshot().await.unwrap();
    let applied = cluster.state().applied_index();
    until(Duration::from_secs(10), || {
        cluster
            .metrics()
            .and_then(|metrics| metrics.purged)
            .is_some_and(|purged| purged.index >= applied)
    })
    .await;
    net.sim.start(3).await.unwrap();
    net.sim.retry_admin(leader, &["add", "p3", "p3:7444"]).await;
    net.attach(3).await;
    let late = net.players(3);
    until(Duration::from_secs(20), move || claimed(&late, "gus")).await;
    assert!(net.cluster(3).metrics().unwrap().snapshot.is_some());
    net.open(3).await;
    let error = login(&net.players(3), 1, 7, "Gus", "lobby")
        .await
        .err()
        .expect("Gus is claimed");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn context_reports_reach_the_peer_that_holds_the_player() {
    let mut net = Net::new(3, Net::config());
    net.form(3).await;
    let (a, b) = (net.players(0), net.players(1));
    let _hank = login(&b, 1, 8, "Hank", "lobby").await.unwrap();
    let seen = a.clone();
    until(Duration::from_secs(5), move || {
        seen.roster().remote_by_name("hank").is_some()
    })
    .await;
    let contexts = rift_permissions::ContextSet::of(ARENA, "red").unwrap();
    // The backend's bus client is connected to peer 0; Hank is on peer 1.
    let player = rift_permissions::Uuid::from_bytes([8; 16]);
    assert!(
        !net.peer(0)
            .authority
            .report_contexts(player, "lobby", contexts.clone())
            .unwrap()
    );
    assert_eq!(
        a.route_contexts([8; 16], "lobby", &contexts).await,
        Some(Ok(true))
    );
    assert_eq!(
        net.peer(1).authority.contexts(player).get(ARENA),
        Some("red")
    );
    // A report from another backend than Hank's is refused there.
    assert_eq!(
        a.route_contexts([8; 16], "survival", &contexts).await,
        Some(Ok(false))
    );
    // Nobody holds this player.
    assert_eq!(a.route_contexts([9; 16], "lobby", &contexts).await, None);
    net.shutdown().await;
}
