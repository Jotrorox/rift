//! A deterministic simulation of a cluster, for tests: N peers in one
//! process on the in-memory network, each with its own directory, on the
//! paused tokio clock. The network partitions, delays and drops on demand;
//! peers crash (shut down without a goodbye) and restart from their files.
//!
//! The network and the clock are deterministic; openraft's election timers
//! draw random timeouts, so the tests assert properties (one leader, no lost
//! entry, the lease ends first) rather than exact schedules. Later steps add
//! their components and tests here.
use super::{
    Cluster,
    link::Timing,
    log_store::LogStore,
    node::{MEMBER_WITHOUT_LOG, Options, RaftTiming},
    operations::ClusterExecutor,
    settings::Settings,
    state::{
        ApplyContext, Component, Components, Entry, EntryKind, Origin, Payload, Proposal, Reply,
    },
    transport::memory::Network,
};
use crate::{
    config::ClusterConfig,
    database::{Database, Mode},
};
use rusqlite::{Connection, OptionalExtension};
use std::{
    collections::BTreeMap,
    io,
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;

/// Fast Raft timing on the virtual clock.
pub const TIMING: RaftTiming = RaftTiming {
    heartbeat: Duration::from_millis(50),
    election_min: Duration::from_millis(300),
    election_max: Duration::from_millis(600),
    keep_after_snapshot: 0,
    maintenance: Duration::from_millis(200),
};
const TOKEN: &str = "simulated-cluster-token";

/// A test component: `config` entries count per key, so applying an entry
/// twice would show. Registered unless a test registers the configuration
/// component of step 8, which owns that kind.
pub struct Counter;

impl Component for Counter {
    fn name(&self) -> &'static str {
        "counter"
    }

    fn kinds(&self) -> Vec<EntryKind> {
        vec![EntryKind::Config]
    }

    fn apply(&self, db: &Connection, entry: &Entry, ctx: &mut ApplyContext) -> io::Result<Reply> {
        let Some(key) = entry
            .payload()
            .and_then(|payload| payload.data["key"].as_str())
        else {
            return Ok(Reply::err("no key"));
        };
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS sim_counter (key TEXT PRIMARY KEY NOT NULL, n INTEGER NOT NULL) STRICT",
        )
        .map_err(super::sql_error)?;
        db.execute(
            "INSERT INTO sim_counter (key, n) VALUES (?1, 1) ON CONFLICT (key) DO UPDATE SET n = n + 1",
            [key],
        )
        .map_err(super::sql_error)?;
        let n: i64 = db
            .query_row("SELECT n FROM sim_counter WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .map_err(super::sql_error)?;
        ctx.changes.touch("sim_counter", key);
        Ok(Reply::ok(n))
    }
}

/// An entry [`Counter`] counts.
pub fn kv(key: &str) -> Entry {
    Entry::Config(Payload::new(1, serde_json::json!({ "key": key })))
}

/// What `key` counts on `cluster`'s replica.
pub fn count(cluster: &Cluster, key: &str) -> i64 {
    cluster
        .state()
        .read(|conn| {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE name = 'sim_counter'",
                    [],
                    |row| row.get(0),
                )
                .map_err(super::sql_error)?;
            if !exists {
                return Ok(0);
            }
            Ok(conn
                .query_row("SELECT n FROM sim_counter WHERE key = ?1", [key], |row| {
                    row.get(0)
                })
                .optional()
                .map_err(super::sql_error)?
                .unwrap_or(0))
        })
        .unwrap()
}

/// Components of later steps for the peer with the given index, created
/// anew at every start of it.
pub type ExtraComponents = Arc<dyn Fn(usize) -> Vec<Arc<dyn Component>> + Send + Sync>;

pub struct Sim {
    pub network: Network,
    /// `rift.config.cluster` every peer starts with.
    pub shared: ClusterConfig,
    /// Registered beside [`Counter`] at every start.
    pub components: Option<ExtraComponents>,
    /// `[labels]` of each peer's `cluster.toml`, by index.
    pub labels: BTreeMap<usize, BTreeMap<String, String>>,
    root: PathBuf,
    peers: Vec<Option<Cluster>>,
    /// The instance database of each running peer.
    databases: Vec<Option<Arc<Database>>>,
    /// The instance database each peer opened last, to wait until it is
    /// closed (and its lock released) before the peer starts again.
    released: Vec<Weak<Database>>,
}

impl Sim {
    pub fn new(count: usize) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // Canonical: the instance database refuses paths through symlinks
        // (macOS keeps the temporary directory behind one).
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "rift-cluster-sim-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self {
            network: Network::default(),
            shared: ClusterConfig {
                peer_timeout: Duration::from_secs(3),
                ..ClusterConfig::default()
            },
            components: None,
            labels: BTreeMap::new(),
            root,
            peers: (0..count).map(|_| None).collect(),
            databases: (0..count).map(|_| None).collect(),
            released: (0..count).map(|_| Weak::new()).collect(),
        }
    }

    pub fn name(&self, index: usize) -> String {
        format!("p{index}")
    }

    pub fn address(&self, index: usize) -> String {
        format!("p{index}:7444")
    }

    fn addresses(&self) -> Vec<String> {
        (0..self.peers.len())
            .map(|index| self.address(index))
            .collect()
    }

    pub fn directory(&self, index: usize) -> PathBuf {
        self.root.join(self.name(index))
    }

    pub fn settings(&self, index: usize) -> Settings {
        let directory = self.directory(index);
        Settings {
            peer: self.name(index),
            listen: "127.0.0.1:7444".parse().unwrap(),
            advertise: self.address(index),
            certificate: directory.join("unused.pem"),
            private_key: directory.join("unused.key"),
            ca: directory.join("unused-ca.pem"),
            token_env: "RIFT_CLUSTER_TOKEN".into(),
            seeds: (0..self.peers.len())
                .filter(|other| *other != index)
                .map(|other| self.address(other))
                .collect(),
            labels: self.labels.get(&index).cloned().unwrap_or_default(),
            public_address: None,
            bus_address: None,
            witness: false,
            directory,
            node: super::settings::NodeSettings::default(),
        }
    }

    pub async fn start(&mut self, index: usize) -> io::Result<Cluster> {
        let mut components = Components::new();
        if let Some(extra) = &self.components {
            for component in extra(index) {
                components.register(component)?;
            }
        }
        // `config` entries belong to the configuration component (step 8)
        // when a test registers it.
        if !components.supported().contains(&EntryKind::Config) {
            components.register(Arc::new(Counter))?;
        }
        if !self.wait_released(index).await {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!(
                    "p{index}: the instance database of its previous start is still open 10 s after it stopped ({} strong references): something the peer started still holds its cluster or database",
                    self.released[index].strong_count()
                ),
            ));
        }
        let settings = self.settings(index);
        std::fs::create_dir_all(&settings.directory)?;
        let database = Arc::new(Database::open_with(
            &settings.default_database(),
            Mode::Replicated,
        )?);
        self.released[index] = Arc::downgrade(&database);
        let mut options = Options::new(self.shared.clone());
        options.database = Some(database.clone());
        options.components = components;
        options.transport = Some(self.network.bind(&self.address(index)));
        options.token = Some(TOKEN.into());
        options.timing = TIMING;
        options.link_timing = Timing {
            ping_interval: Duration::from_millis(100),
            connect_timeout: Duration::from_millis(500),
        };
        let cluster = Cluster::start(settings, options).await?;
        database.set_executor(Arc::new(ClusterExecutor::new(&cluster)));
        self.peers[index] = Some(cluster.clone());
        self.databases[index] = Some(database);
        Ok(cluster)
    }

    /// The instance database of running peer `index`.
    pub fn database(&self, index: usize) -> Arc<Database> {
        self.databases[index].clone().expect("peer is running")
    }

    /// Wait (on the virtual clock, at most 10 s) until the instance database
    /// peer `index` opened last is closed: its aborted tasks drop their
    /// handles a moment after a crash. Whether it is closed.
    async fn wait_released(&self, index: usize) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self.released[index].strong_count() == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// `rift cluster init` on peer `index`, then start it.
    pub async fn init(&mut self, index: usize) -> Cluster {
        let settings = self.settings(index);
        std::fs::create_dir_all(&settings.directory).unwrap();
        let log = LogStore::open(&settings.raft_file()).unwrap();
        log.adopt("simulated", 1, &settings.peer).unwrap();
        drop(log);
        let cluster = self.start(index).await.unwrap();
        super::cli::initialize(&cluster).await.unwrap();
        cluster
    }

    /// Peer 0 initializes; the others are added and promoted: `voters`
    /// voters, the rest learners.
    pub async fn form(&mut self, voters: usize) {
        self.init(0).await;
        for index in 1..self.peers.len() {
            self.start(index).await.unwrap();
            self.admin(0, &["add", &self.name(index), &self.address(index)])
                .await
                .unwrap();
            if index < voters {
                self.retry_admin(0, &["promote", &self.name(index)]).await;
            }
        }
        for index in 0..self.peers.len() {
            let cluster = self.peer(index).clone();
            until(Duration::from_secs(10), || {
                cluster.is_enabled(EntryKind::Kv).unwrap_or(false)
            })
            .await;
        }
    }

    pub fn peer(&self, index: usize) -> &Cluster {
        self.peers[index].as_ref().expect("peer is running")
    }

    pub fn running(&self) -> impl Iterator<Item = (usize, &Cluster)> {
        self.peers
            .iter()
            .enumerate()
            .filter_map(|(index, peer)| peer.as_ref().map(|peer| (index, peer)))
    }

    pub async fn admin(&self, index: usize, args: &[&str]) -> Result<serde_json::Value, String> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        self.peer(index).admin(&args).await
    }

    /// Run an admin command until it succeeds (a learner catching up, a
    /// change still committing).
    pub async fn retry_admin(&self, index: usize, args: &[&str]) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.admin(index, args).await {
                Ok(value) => return value,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(error) => panic!("{args:?}: {error}"),
            }
        }
    }

    /// Shut peer `index` down abruptly; its files stay.
    pub async fn crash(&mut self, index: usize) {
        if let Some(cluster) = self.peers[index].take() {
            cluster.shutdown().await;
        }
        self.databases[index] = None;
        let started = Instant::now();
        if !self.wait_released(index).await {
            // The wait itself moved the virtual clock past timeouts (peer
            // expiry, elections) the test may rely on: say so.
            eprintln!(
                "sim: p{index} crashed, but its instance database is still open after {:?} ({} strong references)",
                started.elapsed(),
                self.released[index].strong_count()
            );
        }
    }

    pub async fn restart(&mut self, index: usize) -> Cluster {
        self.crash(index).await;
        self.start(index).await.unwrap()
    }

    pub fn isolate(&self, index: usize) {
        self.network
            .isolate(&self.address(index), &self.addresses());
    }

    pub fn heal(&self) {
        self.network.heal();
    }

    /// The single running peer that leads with a lease, once there is one.
    pub async fn leader(&self) -> usize {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let leaders: Vec<usize> = self
                .running()
                .filter(|(_, peer)| peer.is_leader_with_lease())
                .map(|(index, _)| index)
                .collect();
            if let [leader] = leaders.as_slice() {
                return *leader;
            }
            assert!(Instant::now() < deadline, "no single leader: {leaders:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn shutdown(mut self) {
        for index in 0..self.peers.len() {
            self.crash(index).await;
        }
    }
}

impl Drop for Sim {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Poll `condition` on the virtual clock until it holds; panics after
/// `timeout`.
pub async fn until(timeout: Duration, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "condition not reached in {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn one_leader_is_elected_and_everyone_follows_it() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    let name = sim.name(leader);
    for (_, peer) in sim.running() {
        let peer = peer.clone();
        let name = name.clone();
        until(Duration::from_secs(5), move || {
            peer.leadership().leader_name.as_deref() == Some(name.as_str())
        })
        .await;
    }
    let voters = sim
        .peer(leader)
        .peers()
        .unwrap()
        .iter()
        .filter(|peer| peer.role.votes())
        .count();
    assert_eq!(voters, 3);
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn learners_replicate_and_reads_see_writes() {
    let mut sim = Sim::new(4);
    sim.form(3).await;
    let leader = sim.leader().await;
    for _ in 0..5 {
        let reply = sim.peer(3).propose(kv("a")).await.unwrap();
        assert!(matches!(reply, Reply::Ok(_)), "{reply:?}");
    }
    // A learner's own write is applied on it when propose returns.
    assert_eq!(count(sim.peer(3), "a"), 5);
    let learner = sim.peer(3).clone();
    let role = learner
        .peers()
        .unwrap()
        .into_iter()
        .find(|peer| peer.name == "p3")
        .unwrap()
        .role;
    assert_eq!(role, super::Role::Learner);
    sim.peer(leader).propose(kv("b")).await.unwrap();
    sim.peer(3).read_barrier().await.unwrap();
    assert_eq!(count(sim.peer(3), "b"), 1);
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_new_leader_keeps_every_committed_entry() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let old = sim.leader().await;
    for _ in 0..5 {
        sim.peer(old).propose(kv("k")).await.unwrap();
    }
    sim.crash(old).await;
    let new = sim.leader().await;
    assert_ne!(new, old);
    assert_eq!(count(sim.peer(new), "k"), 5);
    sim.peer(new).propose(kv("k")).await.unwrap();
    for (_, peer) in sim.running() {
        let peer = peer.clone();
        until(Duration::from_secs(5), move || count(&peer, "k") == 6).await;
    }
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_retry_after_a_leader_change_gets_the_stored_result() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let old = sim.leader().await;
    let proposal = Proposal {
        origin: Some(Origin {
            peer: "client".into(),
            incarnation: 1,
            serial: 7,
        }),
        time_ms: 0,
        term: None,
        entry: kv("once"),
    };
    let (first, _) = sim
        .peer(old)
        .submit(proposal.clone(), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(first, Reply::ok(1));
    sim.crash(old).await;
    let new = sim.leader().await;
    let (again, _) = sim
        .peer(new)
        .submit(proposal, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(again, first);
    assert_eq!(count(sim.peer(new), "once"), 1);
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn membership_changes_refuse_to_leave_fewer_than_three_voters() {
    let mut sim = Sim::new(4);
    sim.form(3).await;
    let leader = sim.leader().await;
    let follower = (0..3).find(|index| *index != leader).unwrap();
    let name = sim.name(follower);
    let error = sim.admin(leader, &["demote", &name]).await.unwrap_err();
    assert!(error.contains("--force"), "{error}");
    let error = sim.admin(leader, &["remove", &name]).await.unwrap_err();
    assert!(error.contains("--force"), "{error}");
    // Growing is always allowed: p3 votes, then one voter may go.
    sim.retry_admin(leader, &["promote", "p3"]).await;
    sim.retry_admin(leader, &["demote", &name]).await;
    sim.retry_admin(leader, &["remove", &name]).await;
    let peers = sim.peer(leader).peers().unwrap();
    assert!(peers.iter().all(|peer| peer.name != name));
    assert_eq!(peers.iter().filter(|peer| peer.role.votes()).count(), 3);
    let error = sim
        .admin(leader, &["add", "p3", "p3:7444"])
        .await
        .unwrap_err();
    assert!(error.contains("already a member"), "{error}");
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_restart_announces_a_new_incarnation() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let incarnation = sim.peer(1).incarnation();
    let restarted = sim.restart(1).await;
    assert_eq!(restarted.incarnation(), incarnation + 1);
    let leader = sim.leader().await;
    let observer = sim.peer(leader).clone();
    until(Duration::from_secs(10), move || {
        observer
            .peers()
            .unwrap()
            .iter()
            .any(|peer| peer.name == "p1" && peer.incarnation == incarnation + 1)
    })
    .await;
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_new_peer_catches_up_from_a_snapshot() {
    let mut sim = Sim::new(4);
    // Three voters; p3 starts later.
    sim.init(0).await;
    for index in 1..3 {
        sim.start(index).await.unwrap();
        sim.admin(0, &["add", &sim.name(index), &sim.address(index)])
            .await
            .unwrap();
        sim.retry_admin(0, &["promote", &sim.name(index)]).await;
    }
    let leader = sim.leader().await;
    let cluster = sim.peer(leader).clone();
    until(Duration::from_secs(10), || {
        cluster.is_enabled(EntryKind::Kv).unwrap_or(false)
    })
    .await;
    for _ in 0..20 {
        cluster.propose(kv("s")).await.unwrap();
    }
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
    sim.start(3).await.unwrap();
    sim.retry_admin(leader, &["add", "p3", "p3:7444"]).await;
    let late = sim.peer(3).clone();
    until(Duration::from_secs(20), || count(&late, "s") == 20).await;
    assert!(late.metrics().unwrap().snapshot.is_some());
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_partitioned_leader_loses_its_lease_before_another_leader_has_one() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let old = sim.leader().await;
    sim.isolate(old);
    let mut lost = None;
    let mut gained = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while gained.is_none() {
        assert!(Instant::now() < deadline, "no new leader");
        let now = Instant::now();
        if lost.is_none() && !sim.peer(old).is_leader_with_lease() {
            lost = Some(now);
        }
        if sim
            .running()
            .any(|(index, peer)| index != old && peer.is_leader_with_lease())
        {
            gained = Some(now);
            assert!(
                lost.is_some(),
                "a new leader had its lease while the old one still had one"
            );
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(lost.unwrap() < gained.unwrap());
    sim.heal();
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_diverged_replica_is_detected_and_repaired() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    sim.peer(leader).propose(kv("x")).await.unwrap();
    let victim = (0..3).find(|index| *index != leader).unwrap();
    let damaged = sim.peer(victim).clone();
    until(Duration::from_secs(5), || count(&damaged, "x") == 1).await;
    damaged
        .state()
        .read(|conn| {
            conn.execute("UPDATE sim_counter SET n = n + 100", [])
                .map_err(super::sql_error)
        })
        .unwrap();
    sim.peer(leader).verify_checksum(None).await.unwrap();
    until(Duration::from_secs(5), || damaged.state().is_diverged()).await;
    for (index, peer) in sim.running() {
        if index != victim {
            assert!(!peer.state().is_diverged(), "p{index} diverged");
        }
    }
    // The maintenance loop asks the leader for a copy and swaps it in.
    until(Duration::from_secs(20), || {
        !damaged.state().is_diverged() && count(&damaged, "x") == 1
    })
    .await;
    sim.peer(leader).propose(kv("x")).await.unwrap();
    until(Duration::from_secs(5), || count(&damaged, "x") == 2).await;
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_member_without_its_raft_file_is_refused_until_replaced() {
    let mut sim = Sim::new(4);
    sim.form(4).await;
    let leader = sim.leader().await;
    let lost = (0..4).find(|index| *index != leader).unwrap();
    sim.crash(lost).await;
    // The peer lost its disk. Let the aborted tasks drop their handles on
    // the files first.
    tokio::time::sleep(Duration::from_millis(100)).await;
    std::fs::remove_dir_all(sim.directory(lost)).unwrap();
    let error = sim.start(lost).await.err().expect("refused");
    assert!(error.to_string().contains(MEMBER_WITHOUT_LOG), "{error}");
    let name = sim.name(lost);
    let replaced = sim.retry_admin(leader, &["replace", &name]).await;
    assert!(replaced.get("pending").is_some(), "{replaced}");
    sim.start(lost).await.unwrap();
    let address = sim.address(lost);
    sim.retry_admin(leader, &["add", &name, &address]).await;
    let back = sim.peer(lost).clone();
    until(Duration::from_secs(20), || {
        back.raft_id().is_some_and(|id| id > 4)
    })
    .await;
    sim.shutdown().await;
}

/// Leader changes: one instance per demand, fenced and reconciled agents,
/// stop holds after a peer expiry (step 7).
mod agent_tests;
/// The replicated instance database with real stores (step 3).
mod database_tests;
/// Applied events (`rift.events.instances`) on every peer, once (step 7).
mod events_tests;
/// Leader-only requests (moves, backups, warming) a follower hands the
/// leader for its operator (step 7).
mod forward_tests;
/// Leader tasks: server states, term-tagged writes, forwarded requests and
/// stop vetoes (step 7).
mod leader_tests;
/// Peers as nodes: labels from `cluster.toml`, the embedded agent (step 7).
mod peer_node_tests;
