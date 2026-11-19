//! The [`Cluster`] handle: one peer's Raft node with its links, log and
//! state machine.
//!
//! # What later steps call
//!
//! - [`Cluster::propose`] (and [`LeaderContext::propose`] for leader
//!   tasks): append an entry and get its result once it is applied **here**,
//!   so the caller reads its own write. Followers and learners forward to the
//!   leader; retries reuse the proposal's serial, so a proposal applies at
//!   most once even across leader changes.
//! - [`Cluster::run_operation`] / [`Cluster::run_in_term`] (in
//!   [`super::operations`]): a write to the instance database, executed by
//!   the leader; the database's executor calls it for every write.
//! - [`Cluster::read_barrier`]: a linearizable read: afterwards the local
//!   replica holds every entry committed before the call.
//! - [`Cluster::on_leader_gain`] / [`Cluster::on_leader_loss`]: work that
//!   runs once in the network. Each gain task gets a [`LeaderContext`] whose
//!   [`LeaderContext::has_lease`] must be checked before every side effect.
//! - [`Cluster::links`]: register handlers for the other stream kinds and
//!   talk to peers.
//! - [`super::state::Component`]s, passed in [`Options::components`].
//!
//! # Leadership and the lease
//!
//! openraft 0.10 (verified in its source, `proposer/leader.rs` and
//! `docs/protocol/leader_lease.md`) gives a leader a lease of
//! `election_timeout_max` from the send time of the last RPC a quorum
//! acknowledged (`RaftMetrics::last_quorum_acked`). A follower that heard
//! from the leader refuses votes for `election_timeout_max` after that,
//! except for a transfer the leader itself started, and a single voter needs
//! no acknowledgement. openraft does **not** step a leader down when its
//! lease runs out; it only refuses new proposals. So
//! [`Cluster::is_leader_with_lease`] checks the lease itself, with a margin:
//! it holds for `3/4 · election_timeout_max` after the last quorum
//! acknowledgement, ends at once when this peer starts a leadership
//! transfer, and needs the leader to be in the term the caller decided in.
//! Since no other peer can win an election before a full
//! `election_timeout_max` has passed since that acknowledgement, a leader cut
//! off from the majority stops acting before another leader exists, as long
//! as the clocks of the peers advance at similar rates.
use super::{
    LogId, PeerId, PeerNode, Raft, StoredMembership, TypeConfig, VERSION,
    link::{self, Hello, Links, StreamKind, remote_error},
    log_store::{LocalIdentity, LogStore},
    network::{NetworkFactory, RaftRequest, RaftResponse},
    operations::Queue,
    replicated,
    settings::Settings,
    snapshot,
    state::{
        ChecksumEntry, Components, Entry, EntryKind, FeaturesEntry, Origin, PeerEntry, PeerRow,
        Proposal, Reply, SnapshotFile, StateMachine,
    },
    transport::{BoxFuture, Transport, stopped},
};
use crate::config::ClusterConfig;
use bytes::Bytes;
use openraft::{
    Instant as _, RaftMetrics, ReadPolicy, ServerState, async_runtime::WatchReceiver as _,
    raft::AppendEntriesResponse,
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io,
    sync::{
        Arc, Mutex, OnceLock, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle, time::Instant};

/// How long [`Cluster::propose`] keeps retrying before it gives up.
pub const PROPOSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Entries between two `checksum` marks.
pub const CHECKSUM_INTERVAL: u64 = 10_000;
/// Log size after which a snapshot is built so openraft can purge.
pub const SNAPSHOT_LOG_BYTES: u64 = 64 * 1024 * 1024;
/// Part of the refusal a member sends a peer that lost its Raft file.
pub(crate) const MEMBER_WITHOUT_LOG: &str = "is a member, but its raft file is empty";
const PROPOSAL_BUCKETS: [f64; 10] = [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 1.0, 2.5, 10.0];

/// Raft timing. Production keeps openraft's ratios at a calmer pace: a
/// 1.5–3 s election timeout, as docs/clustering.md promises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaftTiming {
    pub heartbeat: Duration,
    pub election_min: Duration,
    pub election_max: Duration,
    /// Entries kept behind a snapshot for followers that lag a little.
    pub keep_after_snapshot: u64,
    /// How often the maintenance loop runs.
    pub maintenance: Duration,
}

impl RaftTiming {
    pub const PRODUCTION: Self = Self {
        heartbeat: Duration::from_millis(250),
        election_min: Duration::from_millis(1500),
        election_max: Duration::from_millis(3000),
        keep_after_snapshot: 10_000,
        maintenance: Duration::from_secs(1),
    };

    /// See the module documentation.
    pub fn lease(&self) -> Duration {
        self.election_max * 3 / 4
    }
}

impl Default for RaftTiming {
    fn default() -> Self {
        Self::PRODUCTION
    }
}

/// How to start a peer. Everything but `shared` has a production default.
pub struct Options {
    /// `rift.config.cluster`.
    pub shared: ClusterConfig,
    /// Components applying the entry kinds of later steps.
    pub components: Components,
    /// The instance database the state machine lives in, opened in
    /// [`crate::database::Mode::Replicated`]; default: `rift.sqlite3` in the
    /// peer's directory.
    pub database: Option<Arc<crate::database::Database>>,
    /// Default: QUIC on `listen`, mutual TLS against the cluster CA.
    pub transport: Option<Arc<dyn Transport>>,
    /// Default: the variable `token_env` names.
    pub token: Option<String>,
    pub timing: RaftTiming,
    pub link_timing: link::Timing,
    /// Hash of the shared configuration, compared in handshakes.
    pub config_hash: String,
    /// Shared secrets (by name) whose equality handshakes check.
    pub secrets: BTreeMap<String, String>,
}

impl Options {
    /// The options for a peer running `config`: its `cluster` section, the
    /// hash of the shared sections of the evaluated configuration
    /// ([`super::files::shared`]; the configuration component replaces it
    /// with one that knows the configuration's directory once attached) and
    /// the shared secrets whose equality handshakes check: the forwarding
    /// secret and the node tokens, as far as this environment holds them.
    /// `_source`, the text of `rift.lua`, is no longer hashed.
    pub fn from_config(config: &crate::config::Config, _source: Option<&[u8]>) -> io::Result<Self> {
        if config.database.is_some() {
            return Err(super::invalid(
                "cluster: database.backend = \"postgres\" or \"mariadb\" cannot be combined with cluster.toml; a cluster replicates SQLite",
            ));
        }
        let mut options = Self::new(config.cluster.clone().unwrap_or_default());
        options.config_hash = super::files::shared::of_config(config, None).hash;
        if let Some(variable) = config
            .forwarding
            .as_ref()
            .and_then(|forwarding| forwarding.secret_env.as_ref())
            && let Ok(value) = std::env::var(variable)
        {
            options.secrets.insert("forwarding".to_owned(), value);
        }
        for (name, node) in &config.nodes {
            if let Ok(value) = std::env::var(&node.token_env) {
                options.secrets.insert(format!("nodes.{name}"), value);
            }
        }
        Ok(options)
    }

    pub fn new(shared: ClusterConfig) -> Self {
        Self {
            shared,
            components: Components::new(),
            database: None,
            transport: None,
            token: None,
            timing: RaftTiming::PRODUCTION,
            link_timing: link::Timing::default(),
            config_hash: String::new(),
            secrets: BTreeMap::new(),
        }
    }
}

/// A peer's part in the membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Voter,
    /// A voter that takes no players.
    Witness,
    Learner,
    /// Added, but not (yet) in the Raft membership.
    Pending,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Self::Voter => "voter",
            Self::Witness => "witness",
            Self::Learner => "learner",
            Self::Pending => "pending",
        }
    }

    pub fn votes(self) -> bool {
        matches!(self, Self::Voter | Self::Witness)
    }
}

/// One row of the peer table.
#[derive(Debug, Clone, Serialize)]
pub struct PeerStatus {
    pub name: String,
    pub raft_id: Option<PeerId>,
    pub incarnation: u64,
    pub role: Role,
    pub address: String,
    pub public_address: Option<String>,
    pub leader: bool,
    pub this_peer: bool,
    pub up: bool,
    pub expired: bool,
    /// Milliseconds since this peer heard from it.
    pub last_heard_ms: Option<u64>,
    /// Entries the leader has that it does not (only known on the leader).
    pub lag: Option<u64>,
    pub version: String,
    pub labels: BTreeMap<String, String>,
}

/// Who leads, as this peer sees it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Leadership {
    pub term: u64,
    pub leader: Option<PeerId>,
    pub leader_name: Option<String>,
    /// This peer leads (whether its lease holds is a separate question).
    pub is_leader: bool,
}

/// Handed to leader tasks: the term they run in and the cluster.
#[derive(Clone)]
pub struct LeaderContext {
    pub term: u64,
    cluster: Cluster,
}

impl LeaderContext {
    /// Still the leader of this term, with a lease. Check before every side
    /// effect.
    pub fn has_lease(&self) -> bool {
        self.cluster.is_leader_with_lease() && self.cluster.current_term() == self.term
    }

    /// Propose tagged with this term: if another leader appends it, it is
    /// refused when applied.
    pub async fn propose(&self, entry: Entry) -> io::Result<Reply> {
        self.cluster.propose_in_term(entry, self.term).await
    }

    pub fn cluster(&self) -> &Cluster {
        &self.cluster
    }
}

type GainHook = Arc<dyn Fn(LeaderContext) -> BoxFuture<'static, ()> + Send + Sync>;
type LossHook = Arc<dyn Fn(u64) + Send + Sync>;

#[derive(Default)]
struct LeaderState {
    term: Option<u64>,
    since: Option<Instant>,
    /// This peer started a leadership transfer: its lease is over.
    transferring: bool,
    tasks: Vec<JoinHandle<()>>,
}

#[derive(Default)]
struct Measurements {
    proposal_buckets: [u64; PROPOSAL_BUCKETS.len()],
    proposal_count: u64,
    proposal_seconds: f64,
    operations: BTreeMap<(&'static str, &'static str), u64>,
}

pub(super) struct Inner {
    pub(super) settings: Settings,
    pub(super) shared: ClusterConfig,
    pub(super) timing: RaftTiming,
    pub(super) links: Links,
    pub(super) log: LogStore,
    pub(super) state: StateMachine,
    raft: OnceLock<Raft>,
    config: Arc<openraft::Config>,
    identity: RwLock<LocalIdentity>,
    incarnation: u64,
    serial: AtomicU64,
    /// Operations for this peer to execute while it leads.
    pub(super) operations: Queue,
    start: tokio::sync::Mutex<()>,
    /// One membership change at a time.
    pub(super) membership: tokio::sync::Mutex<()>,
    leadership: watch::Sender<Leadership>,
    leader: Mutex<LeaderState>,
    gain: Mutex<Vec<GainHook>>,
    loss: Mutex<Vec<LossHook>>,
    members: Mutex<Option<LogId>>,
    measurements: Mutex<Measurements>,
    last_repair: Mutex<Option<Instant>>,
    pub(super) stop: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// This process's peer. Cheap to clone.
#[derive(Clone)]
pub struct Cluster {
    pub(super) inner: Arc<Inner>,
}

enum Attempt {
    Retry(String),
    Fatal(io::Error),
}

pub(crate) enum ExecuteError {
    NotLeader(Option<PeerId>),
    Retry(String),
    Fatal(io::Error),
}

impl Cluster {
    /// Start this peer. A peer whose Raft file names a Raft id starts its
    /// Raft node at once; a new peer waits until a member adds it. A new
    /// peer that a seed reports as a member whose Raft file is gone is
    /// refused: it could vote twice in one term.
    pub async fn start(settings: Settings, options: Options) -> io::Result<Self> {
        std::fs::create_dir_all(&settings.directory)?;
        let log = LogStore::open(&settings.raft_file())?;
        let mut identity = log.identity()?;
        if let Some(peer) = &identity.peer
            && *peer != settings.peer
        {
            return Err(super::invalid(format!(
                "{}: belongs to peer {peer}, but cluster.toml names this peer {}",
                log.path().display(),
                settings.peer
            )));
        }
        let incarnation = log.next_incarnation()?;
        identity.incarnation = incarnation;
        let database = match options.database {
            Some(database) => database,
            None => Arc::new(crate::database::Database::open_with(
                &settings.default_database(),
                crate::database::Mode::Replicated,
            )?),
        };
        // The entry kinds this build applies, and the schema versions it
        // runs against (see `replicated::schema_feature`).
        let mut supported: Vec<String> = options
            .components
            .supported()
            .into_iter()
            .map(|kind| kind.name().to_owned())
            .collect();
        supported.extend(replicated::schema_features());
        let state =
            StateMachine::open(database, options.components, settings.snapshot_directory())?;
        if identity.raft_id.is_none() && state.applied_index() > 0 {
            return Err(super::invalid(format!(
                "{} is empty, but {} holds the state of a cluster; this peer lost its Raft file: move the state away and have a member run `rift admin cluster replace {}`",
                log.path().display(),
                state.database().label(),
                settings.peer
            )));
        }
        let token = match options.token {
            Some(token) => token,
            None => settings.token()?,
        };
        let transport: Arc<dyn Transport> = match options.transport {
            Some(transport) => transport,
            None => Arc::new(super::quic::QuicTransport::bind(&settings)?),
        };
        let links = Links::new(
            transport,
            link::Identity {
                name: settings.peer.clone(),
                address: settings.advertise.clone(),
                version: VERSION.to_owned(),
                kinds: supported,
                config_hash: options.config_hash,
                secrets: options.secrets,
            },
            &token,
            options.link_timing,
            settings.seeds.clone(),
        );
        links.set_cluster(identity.cluster_id.clone(), identity.raft_id, incarnation);
        let config = raft_config(&options.timing)?;
        let (leadership, _) = watch::channel(Leadership::default());
        let (stop, _) = watch::channel(false);
        let raft_id = identity.raft_id;
        let cluster = Self {
            inner: Arc::new(Inner {
                settings,
                shared: options.shared,
                timing: options.timing,
                links,
                log,
                state,
                raft: OnceLock::new(),
                config,
                identity: RwLock::new(identity),
                incarnation,
                serial: AtomicU64::new(0),
                operations: Queue::new(),
                start: tokio::sync::Mutex::new(()),
                membership: tokio::sync::Mutex::new(()),
                leadership,
                leader: Mutex::new(LeaderState::default()),
                gain: Mutex::new(Vec::new()),
                loss: Mutex::new(Vec::new()),
                members: Mutex::new(None),
                measurements: Mutex::new(Measurements::default()),
                last_repair: Mutex::new(None),
                stop,
                tasks: Mutex::new(Vec::new()),
            }),
        };
        cluster.install_handlers();
        match raft_id {
            Some(raft_id) => cluster.start_raft(raft_id).await?,
            None => cluster.probe_seeds().await?,
        }
        Ok(cluster)
    }

    pub fn name(&self) -> &str {
        &self.inner.settings.peer
    }

    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    /// `rift.config.cluster` as this peer started with it.
    pub fn shared(&self) -> &ClusterConfig {
        &self.inner.shared
    }

    pub fn links(&self) -> &Links {
        &self.inner.links
    }

    pub fn state(&self) -> &StateMachine {
        &self.inner.state
    }

    pub fn timing(&self) -> RaftTiming {
        self.inner.timing
    }

    /// The Raft node; `None` until this peer was added to a cluster.
    pub fn raft(&self) -> Option<&Raft> {
        self.inner.raft.get()
    }

    pub fn raft_id(&self) -> Option<PeerId> {
        self.inner.identity.read().unwrap().raft_id
    }

    pub fn cluster_id(&self) -> Option<String> {
        self.inner.identity.read().unwrap().cluster_id.clone()
    }

    /// Raised at every start of this peer.
    pub fn incarnation(&self) -> u64 {
        self.inner.incarnation
    }

    pub fn subscribe_leadership(&self) -> watch::Receiver<Leadership> {
        self.inner.leadership.subscribe()
    }

    pub fn leadership(&self) -> Leadership {
        self.inner.leadership.borrow().clone()
    }

    pub fn current_term(&self) -> u64 {
        self.metrics().map_or(0, |metrics| metrics.current_term)
    }

    /// openraft's metrics of this node, if it runs.
    pub fn metrics(&self) -> Option<RaftMetrics<TypeConfig>> {
        Some(self.raft()?.metrics().borrow_watched().clone())
    }

    /// The effective membership.
    pub fn membership(&self) -> Option<Arc<StoredMembership>> {
        Some(
            self.raft()?
                .metrics()
                .borrow_watched()
                .membership_config
                .clone(),
        )
    }

    /// This peer leads and a quorum acknowledged it recently enough that no
    /// other leader can exist yet. See the module documentation.
    pub fn is_leader_with_lease(&self) -> bool {
        let Some(metrics) = self.metrics() else {
            return false;
        };
        if metrics.state != ServerState::Leader || metrics.current_leader != Some(metrics.id) {
            return false;
        }
        if self.inner.leader.lock().unwrap().transferring {
            return false;
        }
        let voters: Vec<PeerId> = metrics.membership_config.voter_ids().collect();
        if voters == [metrics.id] {
            return true;
        }
        let lease = self.inner.timing.lease();
        metrics
            .last_quorum_acked
            .is_some_and(|acked| acked.into_inner().elapsed() < lease)
    }

    /// Run `task` whenever this peer becomes leader, with the term; it is
    /// aborted when leadership ends. Registered while leading, it starts
    /// at once.
    pub fn on_leader_gain<F, Fut>(&self, task: F)
    where
        F: Fn(LeaderContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let hook: GainHook = Arc::new(move |context| Box::pin(task(context)));
        self.inner.gain.lock().unwrap().push(hook.clone());
        let mut leader = self.inner.leader.lock().unwrap();
        if let Some(term) = leader.term {
            let context = LeaderContext {
                term,
                cluster: self.clone(),
            };
            leader.tasks.push(tokio::spawn(hook(context)));
        }
    }

    /// Call `hook` with the term whenever this peer stops being leader.
    pub fn on_leader_loss<F>(&self, hook: F)
    where
        F: Fn(u64) + Send + Sync + 'static,
    {
        self.inner.loss.lock().unwrap().push(Arc::new(hook));
    }

    /// Whether `kind` may be proposed: always for `peer` and `features`,
    /// otherwise once a committed `features` entry lists it.
    pub fn is_enabled(&self, kind: EntryKind) -> io::Result<bool> {
        if kind.always_enabled() {
            return Ok(true);
        }
        Ok(self
            .inner
            .state
            .features()?
            .is_some_and(|features| features.kinds.contains(kind.name())))
    }

    /// Append `entry` and return its result once it is applied on this peer.
    pub async fn propose(&self, entry: Entry) -> io::Result<Reply> {
        self.propose_entry(entry, None)
            .await
            .map(|(reply, _)| reply)
    }

    /// [`Cluster::propose`] for a leader task decided in `term`.
    pub async fn propose_in_term(&self, entry: Entry, term: u64) -> io::Result<Reply> {
        self.propose_entry(entry, Some(term))
            .await
            .map(|(reply, _)| reply)
    }

    /// A new `(peer, incarnation, serial)` for a proposal or operation of
    /// this peer: what makes its retries idempotent.
    pub(crate) fn next_origin(&self) -> Origin {
        Origin {
            peer: self.inner.settings.peer.clone(),
            incarnation: self.inner.incarnation,
            serial: self.inner.serial.fetch_add(1, Ordering::Relaxed) + 1,
        }
    }

    /// Propose and also return the entry's log index.
    pub async fn propose_entry(&self, entry: Entry, term: Option<u64>) -> io::Result<(Reply, u64)> {
        let kind = entry.kind();
        if !self.is_enabled(kind)? {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{kind} entries are not enabled yet: a committed features entry must list them"
                ),
            ));
        }
        let proposal = Proposal {
            origin: Some(self.next_origin()),
            time_ms: 0,
            term,
            entry,
        };
        self.submit(proposal, PROPOSE_TIMEOUT).await
    }

    /// Get `proposal` applied, retrying (with the same serial) through
    /// leader changes until `timeout`; returns once it is applied here.
    pub async fn submit(&self, proposal: Proposal, timeout: Duration) -> io::Result<(Reply, u64)> {
        let kind = proposal.entry.kind().name();
        let started = Instant::now();
        let deadline = started + timeout;
        let mut delay = Duration::from_millis(25);
        let result = loop {
            match self.try_submit(&proposal, deadline).await {
                Ok((reply, index)) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if let Err(error) = self
                        .inner
                        .state
                        .wait_applied(index, remaining.max(Duration::from_secs(1)))
                        .await
                    {
                        break Err(error);
                    }
                    break Ok((reply, index));
                }
                Err(Attempt::Fatal(error)) => break Err(error),
                Err(Attempt::Retry(reason)) => {
                    if Instant::now() + delay >= deadline {
                        break Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("proposal not committed in time: {reason}"),
                        ));
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(500));
                }
            }
        };
        let outcome = match &result {
            Ok((Reply::Ok(_), _)) => "ok",
            Ok((Reply::Err(_), _)) => "refused",
            Err(_) => "failed",
        };
        self.measure(kind, outcome, started.elapsed());
        result
    }

    async fn try_submit(
        &self,
        proposal: &Proposal,
        deadline: Instant,
    ) -> Result<(Reply, u64), Attempt> {
        match self.execute(proposal.clone()).await {
            Ok(applied) => Ok(applied),
            Err(ExecuteError::Fatal(error)) => Err(Attempt::Fatal(error)),
            Err(ExecuteError::Retry(reason)) => Err(Attempt::Retry(reason)),
            Err(ExecuteError::NotLeader(None)) => Err(Attempt::Retry("no leader".into())),
            Err(ExecuteError::NotLeader(Some(leader))) => {
                let name = self.peer_name(leader).ok_or_else(|| {
                    Attempt::Retry(format!("leader {leader} has no known address"))
                })?;
                let timeout = deadline.saturating_duration_since(Instant::now());
                match self
                    .raft_request(&name, &RaftRequest::Propose(proposal.clone()), timeout)
                    .await
                {
                    Ok(RaftResponse::Proposed { index, reply }) => Ok((reply, index)),
                    Ok(RaftResponse::NotLeader { .. }) => {
                        Err(Attempt::Retry(format!("{name} is no longer the leader")))
                    }
                    Ok(_) => Err(Attempt::Retry(format!("unexpected answer from {name}"))),
                    Err(error) => Err(Attempt::Retry(error.to_string())),
                }
            }
        }
    }

    /// On the leader: stamp `proposal` and append it. Returns once it is
    /// applied on the leader.
    pub(crate) async fn execute(
        &self,
        mut proposal: Proposal,
    ) -> Result<(Reply, u64), ExecuteError> {
        let raft = self
            .raft()
            .ok_or_else(|| ExecuteError::Fatal(not_joined()))?;
        proposal.time_ms = super::wall_clock_ms();
        match raft.client_write(proposal).await {
            Ok(response) => Ok((response.data, response.log_id.index)),
            Err(error) => {
                if let Some(forward) = error.forward_to_leader() {
                    // Our own id means the lease ran out; retry here.
                    let leader = forward
                        .leader_id
                        .filter(|leader| Some(*leader) != self.raft_id());
                    return Err(match leader {
                        Some(leader) => ExecuteError::NotLeader(Some(leader)),
                        None => ExecuteError::Retry("leader without a valid lease, or none".into()),
                    });
                }
                match error {
                    openraft::errors::RaftError::Fatal(fatal) => Err(ExecuteError::Fatal(
                        io::Error::other(format!("raft stopped: {fatal}")),
                    )),
                    openraft::errors::RaftError::APIError(error) => {
                        Err(ExecuteError::Retry(error.to_string()))
                    }
                }
            }
        }
    }

    /// A linearizable read barrier: confirms leadership with a quorum (here
    /// or on the leader) and waits until this replica applied everything
    /// committed before the call. Returns the index waited for.
    pub async fn read_barrier(&self) -> io::Result<u64> {
        let deadline = Instant::now() + PROPOSE_TIMEOUT;
        let mut delay = Duration::from_millis(25);
        loop {
            let attempt = match self.local_read_index().await {
                Ok(index) => Ok(index),
                Err(ExecuteError::NotLeader(Some(leader))) => match self.peer_name(leader) {
                    Some(name) => {
                        let timeout = deadline.saturating_duration_since(Instant::now());
                        match self
                            .raft_request(&name, &RaftRequest::ReadIndex, timeout)
                            .await
                        {
                            Ok(RaftResponse::ReadIndex(index)) => Ok(index),
                            Ok(_) => Err(format!("{name} is no longer the leader")),
                            Err(error) => Err(error.to_string()),
                        }
                    }
                    None => Err(format!("leader {leader} has no known address")),
                },
                Err(ExecuteError::NotLeader(None)) => Err("no leader".to_owned()),
                Err(ExecuteError::Retry(reason)) => Err(reason),
                Err(ExecuteError::Fatal(error)) => return Err(error),
            };
            match attempt {
                Ok(index) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    self.inner
                        .state
                        .wait_applied(index, remaining.max(Duration::from_secs(1)))
                        .await?;
                    return Ok(index);
                }
                Err(reason) => {
                    if Instant::now() + delay >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("no linearizable read in time: {reason}"),
                        ));
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(500));
                }
            }
        }
    }

    /// On the leader: the index a linearizable read must wait for.
    pub(super) async fn local_read_index(&self) -> Result<u64, ExecuteError> {
        let raft = self
            .raft()
            .ok_or_else(|| ExecuteError::Fatal(not_joined()))?;
        match raft.get_read_linearizer(ReadPolicy::ReadIndex).await {
            Ok(linearizer) => Ok(linearizer.read_log_id().index()),
            Err(error) => {
                if let Some(forward) = error.forward_to_leader() {
                    let leader = forward
                        .leader_id
                        .filter(|leader| Some(*leader) != self.raft_id());
                    return Err(ExecuteError::NotLeader(leader));
                }
                match error {
                    openraft::errors::RaftError::Fatal(fatal) => Err(ExecuteError::Fatal(
                        io::Error::other(format!("raft stopped: {fatal}")),
                    )),
                    openraft::errors::RaftError::APIError(error) => {
                        Err(ExecuteError::Retry(error.to_string()))
                    }
                }
            }
        }
    }

    /// The peer table: the replicated rows, the membership, the links and,
    /// on the leader, replication lag.
    pub fn peers(&self) -> io::Result<Vec<PeerStatus>> {
        let rows = self.inner.state.peers()?;
        let metrics = self.metrics();
        let membership = metrics
            .as_ref()
            .map(|metrics| metrics.membership_config.clone());
        let voters: BTreeSet<PeerId> = membership
            .as_ref()
            .map(|membership| membership.voter_ids().collect())
            .unwrap_or_default();
        let nodes: BTreeMap<PeerId, PeerNode> = membership
            .as_ref()
            .map(|membership| {
                membership
                    .nodes()
                    .map(|(id, node)| (*id, node.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let leader = metrics.as_ref().and_then(|metrics| metrics.current_leader);
        let last_index = metrics
            .as_ref()
            .and_then(|metrics| metrics.last_log_index)
            .unwrap_or(0);
        let replication = metrics
            .as_ref()
            .and_then(|metrics| metrics.replication.clone());
        let links: BTreeMap<String, link::LinkStatus> = self
            .inner
            .links
            .statuses()
            .into_iter()
            .map(|status| (status.name.clone(), status))
            .collect();
        let me = self.raft_id();
        let mut seen = BTreeSet::new();
        let mut peers = Vec::new();
        let status = |name: String,
                      raft_id: Option<PeerId>,
                      row: Option<&PeerRow>,
                      node: Option<&PeerNode>| {
            let in_membership = raft_id.is_some_and(|id| nodes.contains_key(&id));
            let witness = row.is_some_and(|row| row.witness);
            let role = match raft_id {
                Some(id) if voters.contains(&id) && witness => Role::Witness,
                Some(id) if voters.contains(&id) => Role::Voter,
                Some(_) if in_membership => Role::Learner,
                _ => Role::Pending,
            };
            let this_peer = raft_id.is_some() && raft_id == me;
            let link = links.get(&name);
            let lag = raft_id.and_then(|id| {
                replication.as_ref()?.get(&id).map(|matched| {
                    last_index.saturating_sub(matched.map_or(0, |matched| matched.index))
                })
            });
            PeerStatus {
                address: row
                    .map(|row| row.address.clone())
                    .or_else(|| node.map(|node| node.address.clone()))
                    .unwrap_or_default(),
                public_address: row.and_then(|row| row.public_address.clone()),
                raft_id,
                incarnation: row.map_or(0, |row| row.incarnation),
                role,
                leader: raft_id.is_some() && raft_id == leader,
                this_peer,
                up: this_peer || link.is_some_and(|link| link.up),
                expired: row.is_some_and(|row| row.expired),
                last_heard_ms: if this_peer {
                    Some(0)
                } else {
                    link.and_then(|link| link.last_heard_ms)
                },
                lag: if this_peer {
                    (leader == me).then_some(0)
                } else {
                    lag
                },
                version: row.map(|row| row.version.clone()).unwrap_or_default(),
                labels: row.map(|row| row.labels.clone()).unwrap_or_default(),
                name,
            }
        };
        for row in &rows {
            seen.insert(row.raft_id);
            let node = nodes.get(&row.raft_id);
            peers.push(status(row.name.clone(), Some(row.raft_id), Some(row), node));
        }
        for (id, node) in &nodes {
            if !seen.contains(id) {
                peers.push(status(node.name.clone(), Some(*id), None, Some(node)));
            }
        }
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(peers)
    }

    /// Everything `rift admin cluster` and `GET /api/cluster` show.
    pub fn status(&self) -> serde_json::Value {
        let metrics = self.metrics();
        let leadership = self.leadership();
        let peers = self.peers().unwrap_or_else(|error| {
            log_warn!("cluster: peer table: {error}");
            Vec::new()
        });
        serde_json::json!({
            "peer": self.name(),
            "cluster_id": self.cluster_id(),
            "raft_id": self.raft_id(),
            "incarnation": self.incarnation(),
            "joined": self.raft().is_some(),
            "witness": self.inner.settings.witness,
            "state": metrics.as_ref().map(|metrics| format!("{:?}", metrics.state).to_lowercase()),
            "term": metrics.as_ref().map(|metrics| metrics.current_term),
            "leader": leadership.leader_name,
            "is_leader": leadership.is_leader,
            "lease": self.is_leader_with_lease(),
            "last_log_index": metrics.as_ref().and_then(|metrics| metrics.last_log_index),
            "commit_index": metrics.as_ref().and_then(|metrics| metrics.local_committed.map(|log_id| log_id.index)),
            "applied_index": self.inner.state.applied_index(),
            "snapshot_index": metrics.as_ref().and_then(|metrics| metrics.snapshot.map(|log_id| log_id.index)),
            "log_bytes": self.inner.log.log_bytes(),
            "diverged": self.inner.state.is_diverged(),
            "membership_changing": self.membership_changing(),
            "peers": peers,
            "configuration": super::files::status(self),
        })
    }

    /// Prometheus series of docs/clustering.md, "Operating a cluster".
    pub fn render_metrics(&self) -> String {
        let metrics = self.metrics();
        let mut text = String::new();
        let gauge = |text: &mut String, name: &str, value: u64| {
            text.push_str(&format!(
                "# TYPE rift_cluster_{name} gauge\nrift_cluster_{name} {value}\n"
            ));
        };
        gauge(
            &mut text,
            "term",
            metrics.as_ref().map_or(0, |metrics| metrics.current_term),
        );
        gauge(
            &mut text,
            "is_leader",
            u64::from(self.leadership().is_leader),
        );
        gauge(&mut text, "lease", u64::from(self.is_leader_with_lease()));
        gauge(
            &mut text,
            "commit_index",
            metrics
                .as_ref()
                .and_then(|metrics| metrics.local_committed.map(|log_id| log_id.index))
                .unwrap_or(0),
        );
        gauge(&mut text, "applied_index", self.inner.state.applied_index());
        gauge(&mut text, "log_bytes", self.inner.log.log_bytes());
        gauge(
            &mut text,
            "diverged",
            u64::from(self.inner.state.is_diverged()),
        );
        if let Ok(peers) = self.peers() {
            text.push_str("# TYPE rift_cluster_peer_up gauge\n");
            for peer in &peers {
                text.push_str(&format!(
                    "rift_cluster_peer_up{{peer=\"{}\",role=\"{}\"}} {}\n",
                    peer.name,
                    peer.role.name(),
                    u8::from(peer.up)
                ));
            }
            text.push_str("# TYPE rift_cluster_replication_lag gauge\n");
            for peer in peers.iter().filter(|peer| !peer.this_peer) {
                if let Some(lag) = peer.lag {
                    text.push_str(&format!(
                        "rift_cluster_replication_lag{{peer=\"{}\"}} {lag}\n",
                        peer.name
                    ));
                }
            }
        }
        let measurements = self.inner.measurements.lock().unwrap();
        text.push_str("# TYPE rift_cluster_proposal_seconds histogram\n");
        let mut cumulative = 0;
        for (upper, count) in PROPOSAL_BUCKETS.iter().zip(measurements.proposal_buckets) {
            cumulative += count;
            text.push_str(&format!(
                "rift_cluster_proposal_seconds_bucket{{le=\"{upper}\"}} {cumulative}\n"
            ));
        }
        text.push_str(&format!(
            "rift_cluster_proposal_seconds_bucket{{le=\"+Inf\"}} {}\nrift_cluster_proposal_seconds_sum {}\nrift_cluster_proposal_seconds_count {}\n",
            measurements.proposal_count, measurements.proposal_seconds, measurements.proposal_count
        ));
        text.push_str("# TYPE rift_cluster_operations_total counter\n");
        for ((kind, outcome), count) in &measurements.operations {
            text.push_str(&format!(
                "rift_cluster_operations_total{{kind=\"{kind}\",outcome=\"{outcome}\"}} {count}\n"
            ));
        }
        drop(measurements);
        text.push_str(&super::files::render_metrics(self));
        text
    }

    /// Stop this peer: leader tasks, the Raft node and the links.
    pub async fn shutdown(&self) {
        self.inner.stop.send_replace(true);
        let tasks: Vec<JoinHandle<()>> = self.inner.tasks.lock().unwrap().drain(..).collect();
        for task in tasks {
            task.abort();
        }
        let leader_tasks: Vec<JoinHandle<()>> =
            std::mem::take(&mut self.inner.leader.lock().unwrap().tasks);
        for task in leader_tasks {
            task.abort();
        }
        if let Some(raft) = self.raft()
            && let Err(error) = raft.shutdown().await
        {
            log_warn!("cluster: raft shutdown: {error}");
        }
        self.inner.links.shutdown();
    }

    /// Hand leadership to `to` (a voter). Ends this peer's lease at once.
    pub async fn transfer_leadership(&self, to: PeerId) -> io::Result<()> {
        let raft = self.raft().ok_or_else(not_joined)?;
        self.inner.leader.lock().unwrap().transferring = true;
        raft.trigger()
            .transfer_leader(to)
            .await
            .map_err(|error| io::Error::other(format!("raft stopped: {error}")))
    }

    /// The name of the peer with Raft id `id`, from the membership or the
    /// peer table.
    pub fn peer_name(&self, id: PeerId) -> Option<String> {
        if let Some(membership) = self.membership()
            && let Some(node) = membership.get_node(&id)
        {
            return Some(node.name.clone());
        }
        self.inner
            .state
            .peers()
            .ok()?
            .into_iter()
            .find(|row| row.raft_id == id)
            .map(|row| row.name)
    }

    /// Whether the last membership change is not committed yet.
    pub fn membership_changing(&self) -> bool {
        self.metrics().is_some_and(|metrics| {
            metrics.membership_config.log_id() != metrics.committed_membership_config.log_id()
        })
    }

    pub(crate) async fn raft_request(
        &self,
        peer: &str,
        request: &RaftRequest,
        timeout: Duration,
    ) -> io::Result<RaftResponse> {
        let body = serde_json::to_vec(request).map_err(super::json_error)?;
        let reply = self
            .inner
            .links
            .request(peer, StreamKind::Raft, Bytes::from(body), timeout)
            .await?;
        serde_json::from_slice(&reply).map_err(super::json_error)
    }

    fn measure(&self, kind: &'static str, outcome: &'static str, elapsed: Duration) {
        let mut measurements = self.inner.measurements.lock().unwrap();
        let seconds = elapsed.as_secs_f64();
        if let Some(bucket) = PROPOSAL_BUCKETS.iter().position(|upper| seconds <= *upper) {
            measurements.proposal_buckets[bucket] += 1;
        }
        measurements.proposal_count += 1;
        measurements.proposal_seconds += seconds;
        *measurements.operations.entry((kind, outcome)).or_default() += 1;
    }

    fn install_handlers(&self) {
        let weak = Arc::downgrade(&self.inner);
        self.inner
            .links
            .on_request(StreamKind::Raft, move |peer, body| {
                let cluster = upgrade(&weak);
                async move {
                    match cluster {
                        Some(cluster) => cluster.serve_raft(peer, body).await,
                        None => Err("shutting down".to_owned()),
                    }
                }
            });
        let weak = Arc::downgrade(&self.inner);
        self.inner
            .links
            .on_request(StreamKind::Snapshot, move |peer, body| {
                let cluster = upgrade(&weak);
                async move {
                    match cluster {
                        Some(cluster) => cluster.receive_snapshot(peer, body).await,
                        None => Err("shutting down".to_owned()),
                    }
                }
            });
        let weak = Arc::downgrade(&self.inner);
        self.inner
            .links
            .set_admission(Arc::new(move |hello: &Hello| match upgrade(&weak) {
                Some(cluster) => cluster.admit(hello),
                None => Ok(()),
            }));
    }

    /// A peer that lost its Raft file must not come back under its id, and
    /// a peer that cannot apply an enabled entry kind must not link at all.
    fn admit(&self, hello: &Hello) -> Result<(), String> {
        if let Ok(Some(features)) = self.inner.state.features() {
            let missing: Vec<&str> = features
                .kinds
                .iter()
                .filter(|kind| !hello.kinds.contains(kind))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "{} (version {}) cannot apply the enabled entry kinds {}; upgrade it",
                    hello.name,
                    hello.version,
                    missing.join(", ")
                ));
            }
        }
        let Some(membership) = self.membership() else {
            return Ok(());
        };
        let member = membership
            .nodes()
            .find(|(_, node)| node.name == hello.name)
            .map(|(id, _)| *id);
        match (member, hello.raft_id) {
            (Some(id), None) => Err(format!(
                "{} {MEMBER_WITHOUT_LOG} (raft id {id}); run `rift admin cluster replace {}` on a member",
                hello.name, hello.name
            )),
            (Some(id), Some(theirs)) if id != theirs => Err(format!(
                "{} presents raft id {theirs}, but the membership gives it {id}; it was replaced",
                hello.name
            )),
            _ => Ok(()),
        }
    }

    /// A new peer: refuse to start if a seed says this name is a member
    /// whose Raft file is gone.
    async fn probe_seeds(&self) -> io::Result<()> {
        for seed in &self.inner.settings.seeds {
            match self
                .inner
                .links
                .connect_address(seed, Duration::from_secs(3))
                .await
            {
                Err(error) if error.to_string().contains(MEMBER_WITHOUT_LOG) => {
                    self.inner.links.shutdown();
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("cluster: refusing to start: {error}"),
                    ));
                }
                Err(error) => log_debug!("cluster: seed {seed}: {error}"),
                Ok(_) => {}
            }
        }
        log_info!(
            "cluster: peer {} is not in a cluster yet; add it on a member with `rift admin cluster add {} {}`",
            self.inner.settings.peer,
            self.inner.settings.peer,
            self.inner.settings.advertise
        );
        Ok(())
    }

    async fn start_raft(&self, raft_id: PeerId) -> io::Result<()> {
        let _starting = self.inner.start.lock().await;
        if self.raft().is_some() {
            return Ok(());
        }
        let network = NetworkFactory::new(self.inner.links.clone());
        let raft = Raft::new(
            raft_id,
            self.inner.config.clone(),
            network,
            self.inner.log.clone(),
            self.inner.state.clone(),
        )
        .await
        .map_err(|error| io::Error::other(format!("cluster: raft did not start: {error}")))?;
        if self.inner.raft.set(raft).is_err() {
            return Ok(());
        }
        let watcher = self.clone();
        let maintenance = self.clone();
        let announcer = self.clone();
        let leader = self.clone();
        let mut tasks = self.inner.tasks.lock().unwrap();
        tasks.push(tokio::spawn(async move { watcher.watch().await }));
        tasks.push(tokio::spawn(async move { maintenance.maintain().await }));
        tasks.push(tokio::spawn(async move { announcer.announce().await }));
        tasks.push(tokio::spawn(async move { leader.lead().await }));
        Ok(())
    }

    /// Follow openraft's metrics: leadership changes, leader tasks, the
    /// membership the links keep alive.
    async fn watch(&self) {
        let Some(raft) = self.raft() else { return };
        let mut metrics = raft.metrics();
        let mut stop = self.inner.stop.subscribe();
        loop {
            let current = metrics.borrow_watched().clone();
            self.observe(&current);
            tokio::select! {
                changed = metrics.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                _ = tokio::time::sleep(self.inner.timing.heartbeat) => {}
                _ = stopped(&mut stop) => break,
            }
        }
    }

    fn observe(&self, metrics: &RaftMetrics<TypeConfig>) {
        let me = metrics.id;
        let leading = metrics.state == ServerState::Leader && metrics.current_leader == Some(me);
        let term = metrics.current_term;
        let (gained, lost) = {
            let mut leader = self.inner.leader.lock().unwrap();
            let previous = leader.term;
            match (previous, leading) {
                (Some(previous), true) if previous == term => (None, None),
                (previous, true) => {
                    for task in leader.tasks.drain(..) {
                        task.abort();
                    }
                    leader.term = Some(term);
                    leader.since = Some(Instant::now());
                    leader.transferring = false;
                    (Some(term), previous)
                }
                (Some(previous), false) => {
                    for task in leader.tasks.drain(..) {
                        task.abort();
                    }
                    leader.term = None;
                    leader.since = None;
                    leader.transferring = false;
                    (None, Some(previous))
                }
                (None, false) => (None, None),
            }
        };
        if let Some(previous) = lost {
            log_info!("cluster: {} no longer leads (term {previous})", self.name());
            let hooks: Vec<LossHook> = self.inner.loss.lock().unwrap().clone();
            for hook in hooks {
                hook(previous);
            }
        }
        if let Some(term) = gained {
            log_info!("cluster: {} leads in term {term}", self.name());
            let hooks: Vec<GainHook> = self.inner.gain.lock().unwrap().clone();
            let tasks: Vec<JoinHandle<()>> = hooks
                .into_iter()
                .map(|hook| {
                    tokio::spawn(hook(LeaderContext {
                        term,
                        cluster: self.clone(),
                    }))
                })
                .collect();
            self.inner.leader.lock().unwrap().tasks.extend(tasks);
        }
        let leader_name = metrics.current_leader.and_then(|id| {
            metrics
                .membership_config
                .get_node(&id)
                .map(|node| node.name.clone())
        });
        let leadership = Leadership {
            term,
            leader: metrics.current_leader,
            leader_name,
            is_leader: leading,
        };
        self.inner.leadership.send_if_modified(|current| {
            if *current == leadership {
                false
            } else {
                *current = leadership;
                true
            }
        });
        let membership_id = *metrics.membership_config.log_id();
        let changed = {
            let mut members = self.inner.members.lock().unwrap();
            if *members == membership_id && membership_id.is_some() {
                false
            } else {
                *members = membership_id;
                true
            }
        };
        if changed {
            let members: Vec<(String, String)> = metrics
                .membership_config
                .nodes()
                .map(|(_, node)| (node.name.clone(), node.address.clone()))
                .collect();
            self.inner.links.set_members(&members);
        }
    }

    /// Announce this incarnation with `peer joined` until it is applied.
    async fn announce(&self) {
        let mut stop = self.inner.stop.subscribe();
        let mut delay = Duration::from_millis(100);
        loop {
            let Some(raft_id) = self.raft_id() else {
                return;
            };
            let settings = &self.inner.settings;
            let entry = Entry::Peer(PeerEntry::Joined {
                v: 1,
                name: settings.peer.clone(),
                raft_id,
                incarnation: self.inner.incarnation,
                address: settings.advertise.clone(),
                public_address: settings.public_address.clone(),
                labels: settings.labels.clone(),
                witness: settings.witness,
                version: VERSION.to_owned(),
                kinds: self.inner.links.identity().kinds.clone(),
            });
            match self.propose(entry).await {
                Ok(Reply::Ok(_)) => {
                    log_info!(
                        "cluster: {} joined with incarnation {}",
                        settings.peer,
                        self.inner.incarnation
                    );
                    return;
                }
                Ok(Reply::Err(reason)) if reason.contains("is not a member") => {
                    // Added, but the row has not reached the leader's view
                    // yet, or this peer was removed: keep trying quietly.
                    log_debug!("cluster: join: {reason}");
                }
                Ok(Reply::Err(reason)) => {
                    log_warn!("cluster: join refused: {reason}");
                    return;
                }
                Err(error) => log_debug!("cluster: join: {error}"),
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = stopped(&mut stop) => return,
            }
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }

    /// Periodic work: on the leader, peer expiry, features, checksums, the
    /// witness handoff and log size; on every peer, repair after divergence.
    async fn maintain(&self) {
        let mut stop = self.inner.stop.subscribe();
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.inner.timing.maintenance) => {}
                _ = stopped(&mut stop) => return,
            }
            if self.inner.state.is_diverged() {
                self.request_repair().await;
            }
            if let Err(error) = self.compact().await {
                log_warn!("cluster: snapshot: {error}");
            }
            let term = self.inner.leader.lock().unwrap().term;
            let Some(term) = term else { continue };
            if !self.is_leader_with_lease() {
                continue;
            }
            if let Err(error) = self.expire_peers(term).await {
                log_warn!("cluster: peer expiry: {error}");
            }
            if let Err(error) = self.update_features(term).await {
                log_warn!("cluster: features: {error}");
            }
            if let Err(error) = self.checksum(term).await {
                log_warn!("cluster: checksum: {error}");
            }
            if let Err(error) = self.migrate_schemas(term).await {
                log_warn!("cluster: schema migration: {error}");
            }
            if self.inner.settings.witness {
                self.hand_off().await;
            }
        }
    }

    async fn expire_peers(&self, term: u64) -> io::Result<()> {
        let since = self.inner.leader.lock().unwrap().since;
        let Some(since) = since else { return Ok(()) };
        let timeout = self.inner.shared.peer_timeout;
        let now = Instant::now();
        for row in self.inner.state.peers()? {
            if row.expired || row.incarnation == 0 || row.name == self.inner.settings.peer {
                continue;
            }
            // A new leader starts every peer's timer afresh.
            let heard = self
                .inner
                .links
                .last_heard(&row.name)
                .map_or(since, |heard| heard.max(since));
            if now.saturating_duration_since(heard) > timeout {
                log_warn!(
                    "cluster: expiring peer {} (incarnation {}): not heard from for {} ms",
                    row.name,
                    row.incarnation,
                    now.saturating_duration_since(heard).as_millis()
                );
                let entry = Entry::Peer(PeerEntry::Expired {
                    v: 1,
                    name: row.name.clone(),
                    incarnation: row.incarnation,
                });
                self.propose_in_term(entry, term).await?;
            }
        }
        Ok(())
    }

    /// Enable the entry kinds every member supports. A member's kinds come
    /// from its last `peer joined` (so a member that is down still counts)
    /// or, before it ever joined, from its handshake; a member never heard
    /// of does not hold activation back. Kinds are never disabled again: a
    /// peer lacking an enabled kind is refused when it links (see `admit`).
    async fn update_features(&self, term: u64) -> io::Result<()> {
        let Some(membership) = self.membership() else {
            return Ok(());
        };
        let rows: BTreeMap<PeerId, PeerRow> = self
            .inner
            .state
            .peers()?
            .into_iter()
            .map(|row| (row.raft_id, row))
            .collect();
        let mut common: Option<BTreeSet<String>> = None;
        for (id, node) in membership.nodes() {
            let kinds: BTreeSet<String> = if Some(*id) == self.raft_id() {
                self.inner.links.identity().kinds.iter().cloned().collect()
            } else {
                match rows.get(id) {
                    Some(row) if row.incarnation > 0 => row.kinds.iter().cloned().collect(),
                    _ => match self.inner.links.hello(&node.name) {
                        Some(hello) => hello.kinds.into_iter().collect(),
                        None => continue,
                    },
                }
            };
            common = Some(match common {
                None => kinds,
                Some(common) => common.intersection(&kinds).cloned().collect(),
            });
        }
        let Some(common) = common else { return Ok(()) };
        let current = self.inner.state.features()?.unwrap_or_default();
        let kinds: BTreeSet<String> = current.kinds.union(&common).cloned().collect();
        if current.kinds == kinds {
            return Ok(());
        }
        log_info!(
            "cluster: enabling entry kinds {}",
            kinds
                .difference(&current.kinds)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        let entry = Entry::Features(FeaturesEntry {
            v: 1,
            kinds,
            settings: current.settings,
        });
        self.propose_in_term(entry, term).await?;
        Ok(())
    }

    /// Every [`CHECKSUM_INTERVAL`] entries: mark, then verify with this
    /// leader's hash.
    async fn checksum(&self, term: u64) -> io::Result<()> {
        if !self.is_enabled(EntryKind::Checksum)? {
            return Ok(());
        }
        let last: i64 = self.inner.state.read(|conn| {
            conn.query_row(
                "SELECT COALESCE(MAX(idx), 0) FROM cluster_checksums",
                [],
                |row| row.get(0),
            )
            .map_err(super::sql_error)
        })?;
        let last = u64::try_from(last).unwrap_or(0);
        if self.inner.state.applied_index().saturating_sub(last) < CHECKSUM_INTERVAL {
            return Ok(());
        }
        self.verify_checksum(Some(term)).await.map(|_| ())
    }

    /// Mark the state and verify it against this peer's hash: the leader's
    /// part of divergence detection. Returns the marked index.
    pub async fn verify_checksum(&self, term: Option<u64>) -> io::Result<u64> {
        let (_, index) = self
            .propose_entry(Entry::Checksum(ChecksumEntry::Mark { v: 1 }), term)
            .await?;
        let hash = self
            .inner
            .state
            .checksum_at(index)?
            .ok_or_else(|| io::Error::other(format!("no local checksum at entry {index}")))?;
        self.propose_entry(
            Entry::Checksum(ChecksumEntry::Verify { v: 1, index, hash }),
            term,
        )
        .await?;
        Ok(index)
    }

    /// A witness leads only while no player-serving voter can take over.
    async fn hand_off(&self) {
        let Some(metrics) = self.metrics() else {
            return;
        };
        let Ok(rows) = self.inner.state.peers() else {
            return;
        };
        let last = metrics.last_log_index.unwrap_or(0);
        let target = metrics.membership_config.voter_ids().find(|id| {
            *id != metrics.id
                && rows.iter().any(|row| row.raft_id == *id && !row.witness)
                && self
                    .peer_name(*id)
                    .is_some_and(|name| self.inner.links.is_up(&name))
                && metrics
                    .replication
                    .as_ref()
                    .and_then(|replication| replication.get(id).copied().flatten())
                    .is_some_and(|matched| matched.index >= last)
        });
        if let Some(target) = target {
            log_info!(
                "cluster: witness {} hands leadership to {target}",
                self.name()
            );
            if let Err(error) = self.transfer_leadership(target).await {
                log_warn!("cluster: leadership handoff: {error}");
            }
        }
    }

    /// Build a snapshot once the log passes [`SNAPSHOT_LOG_BYTES`]; openraft
    /// then purges the log behind it.
    async fn compact(&self) -> io::Result<()> {
        if self.inner.log.log_bytes() < SNAPSHOT_LOG_BYTES {
            return Ok(());
        }
        let Some(raft) = self.raft() else {
            return Ok(());
        };
        let snapshot = raft
            .metrics()
            .borrow_watched()
            .snapshot
            .map_or(0, |log_id| log_id.index);
        if self.inner.state.applied_index() <= snapshot {
            return Ok(());
        }
        raft.trigger()
            .snapshot()
            .await
            .map_err(|error| io::Error::other(format!("raft stopped: {error}")))
    }

    /// Ask the leader for a fresh copy after a detected divergence.
    async fn request_repair(&self) {
        {
            let mut last = self.inner.last_repair.lock().unwrap();
            if last.is_some_and(|at| at.elapsed() < Duration::from_secs(30)) {
                return;
            }
            *last = Some(Instant::now());
        }
        let Some(leader) = self.leadership().leader_name else {
            return;
        };
        if leader == self.inner.settings.peer {
            log_error!(
                "cluster: the leader's own replica diverged; transfer leadership and repair it"
            );
            return;
        }
        let request = RaftRequest::Repair {
            min_index: self.inner.state.applied_index(),
        };
        if let Err(error) = self
            .raft_request(&leader, &request, Duration::from_secs(10))
            .await
        {
            log_warn!("cluster: repair request to {leader}: {error}");
        }
    }

    async fn serve_raft(&self, peer: String, body: Bytes) -> Result<Bytes, String> {
        let request: RaftRequest =
            serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        let response = match request {
            RaftRequest::Append(rpc) => {
                let raft = self.raft().ok_or("not in a cluster yet")?;
                let result = raft
                    .append_entries(rpc)
                    .await
                    .map_err(|error| error.to_string())?;
                RaftResponse::Append(AppendEntriesResponse::from(result))
            }
            RaftRequest::Vote(rpc) => {
                let raft = self.raft().ok_or("not in a cluster yet")?;
                RaftResponse::Vote(raft.vote(rpc).await.map_err(|error| error.to_string())?)
            }
            RaftRequest::PreVote(rpc) => {
                let raft = self.raft().ok_or("not in a cluster yet")?;
                RaftResponse::Vote(
                    raft.pre_vote(rpc)
                        .await
                        .map_err(|error| error.to_string())?,
                )
            }
            RaftRequest::TransferLeader(rpc) => {
                let raft = self.raft().ok_or("not in a cluster yet")?;
                RaftResponse::TransferLeader(
                    raft.handle_transfer_leader(rpc)
                        .await
                        .map_err(|error| error.to_string())?,
                )
            }
            RaftRequest::Propose(proposal) => match self.execute(proposal).await {
                Ok((reply, index)) => RaftResponse::Proposed { index, reply },
                Err(ExecuteError::NotLeader(leader)) => RaftResponse::NotLeader { leader },
                Err(ExecuteError::Retry(_)) => RaftResponse::NotLeader { leader: None },
                Err(ExecuteError::Fatal(error)) => return Err(error.to_string()),
            },
            RaftRequest::Operate(request) => {
                RaftResponse::Operated(self.serve_operation(request).await)
            }
            RaftRequest::ReadIndex => match self.local_read_index().await {
                Ok(index) => RaftResponse::ReadIndex(index),
                Err(ExecuteError::NotLeader(leader)) => RaftResponse::NotLeader { leader },
                Err(ExecuteError::Retry(_)) => RaftResponse::NotLeader { leader: None },
                Err(ExecuteError::Fatal(error)) => return Err(error.to_string()),
            },
            RaftRequest::Assign {
                cluster_id,
                raft_id,
            } => {
                self.assigned(&peer, cluster_id, raft_id)
                    .await
                    .map_err(|error| error.to_string())?;
                RaftResponse::Done
            }
            RaftRequest::Admin(args) => RaftResponse::Admin(self.admin_here(&args).await?),
            RaftRequest::Repair { min_index } => {
                let cluster = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = cluster.send_repair(&peer, min_index).await {
                        log_warn!("cluster: repair of {peer}: {error}");
                    }
                });
                RaftResponse::Done
            }
        };
        serde_json::to_vec(&response)
            .map(Bytes::from)
            .map_err(|error| error.to_string())
    }

    /// A member added this peer: record the cluster and Raft id, start Raft.
    async fn assigned(&self, from: &str, cluster_id: String, raft_id: PeerId) -> io::Result<()> {
        if let Some(raft) = self.raft() {
            if *raft.node_id() == raft_id
                && self.cluster_id().as_deref() == Some(cluster_id.as_str())
            {
                return Ok(());
            }
            return Err(super::invalid(format!(
                "peer {} already is raft id {} of cluster {}",
                self.name(),
                raft.node_id(),
                self.cluster_id().unwrap_or_default()
            )));
        }
        self.inner
            .log
            .adopt(&cluster_id, raft_id, &self.inner.settings.peer)?;
        {
            let mut identity = self.inner.identity.write().unwrap();
            identity.cluster_id = Some(cluster_id.clone());
            identity.raft_id = Some(raft_id);
            identity.peer = Some(self.inner.settings.peer.clone());
        }
        self.inner.links.set_cluster(
            Some(cluster_id.clone()),
            Some(raft_id),
            self.inner.incarnation,
        );
        self.start_raft(raft_id).await?;
        log_info!(
            "cluster: {} joined cluster {cluster_id} as raft id {raft_id}, added by {from}",
            self.name()
        );
        Ok(())
    }

    async fn receive_snapshot(&self, _peer: String, body: Bytes) -> Result<Bytes, String> {
        let (header, data) = snapshot::decode(&body).map_err(|error| error.to_string())?;
        let outcome = snapshot::receive(&self.inner.state, &header, &data)
            .map_err(|error| error.to_string())?;
        let reply = match outcome {
            snapshot::Received::Expect(offset) => snapshot::ChunkReply::Next(offset),
            snapshot::Received::Complete(path) => {
                let Some((vote, meta)) = header.done else {
                    return Err("snapshot ended without its description".into());
                };
                if header.repair {
                    let index = self
                        .inner
                        .state
                        .repair_from(&path)
                        .map_err(|error| error.to_string())?;
                    let _ = std::fs::remove_file(path);
                    log_info!("cluster: replica repaired from a copy at entry {index}");
                    snapshot::ChunkReply::Installed(vote)
                } else {
                    let raft = self.raft().ok_or("not in a cluster yet")?;
                    let response = raft
                        .install_full_snapshot(
                            vote,
                            openraft::Snapshot {
                                meta,
                                snapshot: SnapshotFile { path },
                            },
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    snapshot::ChunkReply::Installed(response.vote)
                }
            }
        };
        snapshot::encode_reply(&reply).map_err(|error| error.to_string())
    }

    /// On the leader: send `peer` a copy of the state at or after
    /// `min_index`, to replace its diverged replica.
    async fn send_repair(&self, peer: &str, min_index: u64) -> io::Result<()> {
        let raft = self.raft().ok_or_else(not_joined)?;
        self.inner
            .state
            .wait_applied(min_index, Duration::from_secs(30))
            .await?;
        let copy = self.inner.state.build_snapshot_file()?;
        let vote = raft.metrics().borrow_watched().vote;
        snapshot::send(
            &self.inner.links,
            peer,
            vote,
            copy,
            true,
            std::future::pending(),
            Duration::from_secs(60),
        )
        .await
        .map(|_| ())
        .map_err(|error| io::Error::other(error.to_string()))
    }
}

fn upgrade(weak: &Weak<Inner>) -> Option<Cluster> {
    weak.upgrade().map(|inner| Cluster { inner })
}

pub(crate) fn not_joined() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "this peer is not in a cluster yet; add it with `rift admin cluster add` on a member",
    )
}

/// Whether an error from a peer link is the peer's own refusal rather than
/// an unreachable peer.
pub(crate) fn is_refusal(error: &io::Error) -> bool {
    remote_error(error).is_some() || error.kind() == io::ErrorKind::PermissionDenied
}

fn raft_config(timing: &RaftTiming) -> io::Result<Arc<openraft::Config>> {
    let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    let config = openraft::Config {
        cluster_name: "rift".to_owned(),
        heartbeat_interval: millis(timing.heartbeat),
        election_timeout_min: millis(timing.election_min),
        election_timeout_max: millis(timing.election_max),
        // Snapshots of a large instance database take a while to send.
        install_snapshot_timeout: 600_000,
        // Built on demand (a member needs one, or the log passed 64 MiB).
        snapshot_policy: openraft::SnapshotPolicy::Never,
        max_in_snapshot_log_to_keep: timing.keep_after_snapshot,
        replication_lag_threshold: 1000,
        // A peer that was cut off must not raise the term when it returns.
        enable_pre_vote: Some(true),
        ..openraft::Config::default()
    };
    config
        .validate()
        .map(Arc::new)
        .map_err(|error| super::invalid(format!("cluster: raft timing: {error}")))
}
