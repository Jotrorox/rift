//! Work that runs once in the network: on the leader (docs/clustering.md,
//! "Work that runs once: the leader", plan step 7).
//!
//! - **Terms of leader tasks.** [`in_term`] runs a leader task with the term
//!   it decided in, and [`with_thread_term`] carries that term onto a
//!   blocking thread. Every write such a task makes through
//!   [`crate::database::Database::run`] then reaches the leader as
//!   [`Cluster::run_in_term`] (see [`super::ClusterExecutor`]), so a deposed
//!   leader's decision is refused instead of applied by the new leader.
//! - **Control-loop decisions.** [`decide`] asks the scaler (or any other
//!   decision function) only on a leader that caught up with its term, and
//!   keeps that term with the decision; [`Acting::run`] runs the decided
//!   change (an instance creation) in it. [`fence_and_reconcile`] fences
//!   and reconciles the node agents for a new leader.
//! - **[`Coordination`]**: the [`Coordinator`] of this peer's managed
//!   servers. Their workers act only while this peer leads with a lease;
//!   the leader records each server's state as a `server_states` write, and
//!   every other peer mirrors those records and hands its requests (operator
//!   starts and stops, start on connect) to the leader over the `load`
//!   stream. Node agents accept control commands only with the leader's
//!   term ([`crate::node::Nodes::set_cluster`]).
//! - **Inputs from other peers**, on the `load` stream: every peer reports
//!   its reservations to the leader each second, and the leader sums the
//!   reports younger than five seconds for the scaler. Before an idle stop
//!   or a scale-down, the leader asks every live peer, which stops routing
//!   new players to the server and vetoes while a player is on its way.
//! - Requests a follower received for the leader: instance creations and
//!   removals ([`Coordination::set_instance_handler`]), and the drain that
//!   `rift admin cluster leave` asks of a peer
//!   ([`Coordination::set_leave_handler`]).
//!
//! A single Rift never creates any of this.
use super::{
    Cluster, StreamKind,
    link::remote_error,
    node::{LeaderContext, Role},
    players::Presence,
};
use crate::{
    database::{Operation, Outcome},
    instance_store::{InstanceOp, InstanceOutcome, ServerState, Store},
    managed::{Coordinator, CoordinatorFuture, Forward, ManagedServers, Phase, Quiesced},
    node::{Nodes, protocol::Command},
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt, io,
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, task::JoinHandle, time::Instant};

/// How often each peer reports its reservations to the leader.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// A report older than this counts as empty.
pub const REPORT_FRESH: Duration = Duration::from_secs(5);
/// How long a peer keeps routing no new players to a server the leader is
/// about to stop; the leader's record of the stop takes over before.
const STOP_HOLD: Duration = Duration::from_secs(15);
/// How long the leader waits for each peer's answer before an idle stop.
const STOP_QUERY_WAIT: Duration = Duration::from_secs(3);
/// How long a follower waits, after the leader started a server for a
/// player, until its own replica shows the server running.
const RUNNING_WAIT: Duration = Duration::from_secs(5);

tokio::task_local! {
    static TERM: u64;
}

thread_local! {
    static THREAD_TERM: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Run `work` as a leader task decided in `term`: its writes to the
/// instance database carry the term.
pub async fn in_term<F: Future>(term: u64, work: F) -> F::Output {
    TERM.scope(term, work).await
}

/// Run `work` on this (blocking) thread with the leader term `term`, as
/// [`in_term`] does for a task; `None` runs it as an ordinary write.
pub fn with_thread_term<T>(term: Option<u64>, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0;
            THREAD_TERM.with(|cell| cell.set(previous));
        }
    }
    let _restore = Restore(THREAD_TERM.with(|cell| cell.replace(term)));
    work()
}

/// The leader term the current task or thread decided in, if it is a
/// leader task.
pub fn current_term() -> Option<u64> {
    THREAD_TERM
        .with(Cell::get)
        .or_else(|| TERM.try_with(|term| *term).ok())
}

/// What a peer does when `rift admin cluster leave` names it.
pub type LeaveHandler = Arc<dyn Fn() + Send + Sync>;

/// How this process drains when `rift admin cluster leave` names it.
static DRAIN: RwLock<Option<LeaveHandler>> = RwLock::new(None);

/// Drain this process as `rift admin cluster leave` does; whether it could.
pub fn drain_here() -> bool {
    let handler = DRAIN.read().unwrap_or_else(|e| e.into_inner()).clone();
    match handler {
        Some(handler) => {
            handler();
            true
        }
        None => false,
    }
}

/// Whether work that runs once in the network may run in this process now:
/// always on a single Rift; in a cluster only on the leader with a lease.
pub fn runs_here() -> bool {
    super::global().is_none_or(Cluster::is_leader_with_lease)
}

/// The term this peer leads in while its lease holds.
pub fn acting_term(cluster: &Cluster) -> Option<u64> {
    cluster
        .is_leader_with_lease()
        .then(|| cluster.current_term())
        .filter(|term| *term > 0)
}

/// How this process takes part in work that runs once in the network and
/// that a control loop decides (the scaler, instance creations and
/// removals); see [`Acting::of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acting {
    /// A single Rift: it decides and runs everything itself, as ordinary
    /// writes.
    Alone,
    /// This cluster peer leads with a lease in this term: it decides, and
    /// runs what it decided in that term.
    Leads(u64),
    /// Another peer leads, or none does right now: this peer decides
    /// nothing and hands requests to the leader.
    Follows,
}

impl Acting {
    /// How this process takes part now: `coordination` is `None` on a
    /// single Rift.
    pub fn of(coordination: Option<&Coordination>) -> Self {
        match coordination {
            None => Self::Alone,
            Some(coordination) => coordination
                .acting_term()
                .map_or(Self::Follows, Self::Leads),
        }
    }

    /// The term a cluster leader acts in.
    pub fn term(self) -> Option<u64> {
        match self {
            Self::Leads(term) => Some(term),
            Self::Alone | Self::Follows => None,
        }
    }

    /// Run `work` as this process decided it: a cluster leader's in the
    /// term it decided in ([`in_term`]), so once another peer leads, the
    /// writes it makes are refused instead of applied by the new leader.
    pub async fn run<F: Future>(self, work: F) -> F::Output {
        match self {
            Self::Leads(term) => in_term(term, work).await,
            Self::Alone | Self::Follows => work.await,
        }
    }
}

/// A change a control loop decided ([`decide`]), with how it was decided:
/// run it with [`Acting::run`].
#[derive(Debug)]
pub struct Decision<T> {
    pub change: T,
    pub acting: Acting,
}

/// One step of a control loop's work that runs once in the network: ask
/// `decide` for the next change if this process decides now, and keep the
/// term it decided in with the change. A single Rift (`coordination` is
/// `None`) always decides, with no reservations of other peers. A cluster
/// peer decides only while it leads with a lease and once it applied
/// everything committed before its term ([`Coordination::deciding_term`]),
/// so it never decides from an older view than the previous leader's; it
/// passes `decide` the reservations the other peers reported
/// ([`Coordination::remote_reservations`]). Every other peer decides
/// nothing.
pub fn decide<T>(
    coordination: Option<&Coordination>,
    decide: impl FnOnce(&BTreeMap<String, usize>) -> Option<T>,
) -> Option<Decision<T>> {
    let (acting, remote) = match coordination {
        None => (Acting::Alone, BTreeMap::new()),
        Some(coordination) => {
            let term = coordination.deciding_term()?;
            (Acting::Leads(term), coordination.remote_reservations())
        }
    };
    decide(&remote).map(|change| Decision { change, acting })
}

/// Fence every online node agent for the term of `context`, then reconcile
/// it with the servers this leader assigns to it (`assigned`), as a
/// restarted controller adopts servers: a peer that observed the agents as
/// a follower would not see them reconnect (docs/clustering.md, "Leader
/// exclusivity"). `configured` lists the configured nodes. An agent that
/// reconnects is fenced and reconciled again; one that cannot be fenced
/// gets no control command and is reported once. Acts only while the lease
/// of `context` holds, for as long as the leadership lasts (the cluster
/// aborts it when the leadership ends).
pub async fn fence_and_reconcile(
    context: LeaderContext,
    nodes: Nodes,
    configured: impl Fn() -> Vec<String> + Send + Sync,
    assigned: impl Fn(&str) -> Vec<String> + Send + Sync,
) {
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut reported: BTreeSet<String> = BTreeSet::new();
    loop {
        if context.has_lease() {
            let names = configured();
            for node in names {
                if done.contains(&node) || !nodes.online(&node) {
                    continue;
                }
                if let Err(error) = nodes.fence(&node, context.term).await {
                    if reported.insert(node.clone()) {
                        log_warn!(
                            "cluster: node {node} is not fenced for term {}: {error}",
                            context.term
                        );
                    }
                    continue;
                }
                reported.remove(&node);
                let servers = assigned(&node);
                let command = Command::Reconcile { servers };
                match nodes
                    .request(&node, &command, Duration::from_secs(120))
                    .await
                {
                    Ok(result) => {
                        let changed = result["stopped"].as_array().is_some_and(|v| !v.is_empty())
                            || result["removed"].as_array().is_some_and(|v| !v.is_empty());
                        if changed {
                            log_info!("node {node} reconciled: {result}");
                        }
                        done.insert(node);
                    }
                    Err(error) => log_error!("node {node} reconciliation failed: {error}"),
                }
            }
        }
        tokio::select! {
            reconnected = nodes.reconnected() => {
                for node in reconnected {
                    done.remove(&node);
                }
            }
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

/// Wait until this peer, which just gained leadership in `context`'s term,
/// applied every entry committed before: its view (instances, server
/// states, schedules) is then the previous leader's.
pub async fn caught_up(context: &LeaderContext) -> bool {
    loop {
        if !context.has_lease() {
            // The lease follows the first quorum acknowledgement.
            tokio::time::sleep(Duration::from_millis(100)).await;
            if context.cluster().current_term() != context.term {
                return false;
            }
            continue;
        }
        match context.cluster().read_barrier().await {
            Ok(_) => return context.has_lease(),
            Err(error) => {
                log_warn!("cluster: the new leader could not catch up yet: {error}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

/// A message on the `load` stream.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LoadMessage {
    /// A peer's pending connections per server, for the leader.
    Report {
        reservations: BTreeMap<String, usize>,
    },
    /// The leader is about to stop `server`: stop routing players there,
    /// and veto while one is on the way.
    StopQuery { server: String },
    /// A follower hands the leader a request for a managed server.
    Managed {
        server: String,
        request: Forward,
        wait_ms: u64,
    },
    /// A follower hands the leader an instance creation or removal.
    Instance { request: serde_json::Value },
    /// A follower hands the leader another leader-only request (moves,
    /// backups and restores, cache warming; see [`ForwardedRequest`]).
    Request { request: ForwardedRequest },
    /// `rift admin cluster leave` asks this peer to drain.
    Leave,
}

/// A leader-only request a follower received and hands to the leader, with
/// the operator who made it, so the leader's audit trail names them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForwardedRequest {
    /// What to run; the runtime's [`RequestHandler`] dispatches on it.
    pub kind: String,
    /// Who asked, as the receiving peer authenticated them.
    pub actor: String,
    pub body: serde_json::Value,
}

/// Runs a [`ForwardedRequest`] on the leader: the runtime's part. It runs
/// in the leader's term (see [`in_term`]).
pub type RequestHandler = Arc<
    dyn Fn(ForwardedRequest) -> CoordinatorFuture<Result<serde_json::Value, String>> + Send + Sync,
>;

#[derive(Debug, Serialize, Deserialize)]
struct StopAnswer {
    veto: bool,
}

/// Runs an instance request on the leader: the runtime's part.
pub type InstanceHandler = Arc<
    dyn Fn(serde_json::Value) -> CoordinatorFuture<Result<serde_json::Value, String>> + Send + Sync,
>;

/// What the leader does about managed servers, and what the other peers
/// give it (see the module documentation). Cheap to clone.
#[derive(Clone)]
pub struct Coordination {
    inner: Arc<Inner>,
}

/// A peer's reservations per server, and when they arrived.
type Report = (Instant, BTreeMap<String, usize>);

struct Inner {
    cluster: Cluster,
    managed: ManagedServers,
    /// What the leader recorded, as of this replica.
    states: RwLock<BTreeMap<String, ServerState>>,
    /// The latest report of each other peer, with its arrival.
    reports: Mutex<HashMap<String, Report>>,
    /// Stop holds the leader asked this peer for, with their end.
    holds: Mutex<Vec<(Instant, Quiesced)>>,
    presence: RwLock<Option<Arc<Presence>>>,
    /// States to record, the latest per server, with their term.
    pending: Mutex<BTreeMap<String, (ServerState, u64)>>,
    flush: Notify,
    instance: RwLock<Option<InstanceHandler>>,
    request: RwLock<Option<RequestHandler>>,
    leave: RwLock<Option<LeaveHandler>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// The latest term in which this peer, leading, applied everything
    /// committed before the term ([`caught_up`]); 0 before.
    caught_up: AtomicU64,
}

impl fmt::Debug for Coordination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Coordination")
            .field("peer", &self.inner.cluster.name())
            .finish_non_exhaustive()
    }
}

impl Coordination {
    /// Coordinate `managed` (and its nodes) through `cluster`: mirror the
    /// recorded server states from `store`, answer the `load` stream and
    /// report this peer's reservations. Call once per cluster start, after
    /// the stores opened and before the workers launch.
    pub fn start(cluster: &Cluster, managed: &ManagedServers, store: &Store) -> io::Result<Self> {
        let coordination = Self {
            inner: Arc::new(Inner {
                cluster: cluster.clone(),
                managed: managed.clone(),
                states: RwLock::new(BTreeMap::new()),
                reports: Mutex::new(HashMap::new()),
                holds: Mutex::new(Vec::new()),
                presence: RwLock::new(None),
                pending: Mutex::new(BTreeMap::new()),
                flush: Notify::new(),
                instance: RwLock::new(None),
                request: RwLock::new(None),
                leave: RwLock::new(None),
                tasks: Mutex::new(Vec::new()),
                caught_up: AtomicU64::new(0),
            }),
        };
        let weak = Arc::downgrade(&coordination.inner);
        cluster.on_leader_gain(move |context| {
            let weak = weak.clone();
            async move {
                if caught_up(&context).await
                    && let Some(inner) = weak.upgrade()
                {
                    inner.caught_up.store(context.term, Ordering::Release);
                }
            }
        });
        let weak = Arc::downgrade(&coordination.inner);
        store.watch_server_states(move |states| {
            if let Some(inner) = weak.upgrade() {
                *inner.states.write().unwrap_or_else(|e| e.into_inner()) = states;
            }
        })?;
        let weak = Arc::downgrade(&coordination.inner);
        cluster
            .links()
            .on_request(StreamKind::Load, move |peer, body: Bytes| {
                let inner = weak.upgrade();
                async move {
                    match inner {
                        Some(inner) => Coordination { inner }.receive(peer, body).await,
                        None => Err("this peer is stopping".to_owned()),
                    }
                }
            });
        {
            let acting = cluster.clone();
            managed
                .nodes()
                .set_cluster(Arc::new(move || acting_term(&acting)));
        }
        managed.set_coordinator(Arc::new(coordination.clone()));
        let tasks = vec![
            tokio::spawn(report(Arc::downgrade(&coordination.inner))),
            tokio::spawn(flush(Arc::downgrade(&coordination.inner))),
        ];
        coordination
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(tasks);
        Ok(coordination)
    }

    /// Hold back idle stops of servers an expired peer's players may still
    /// be on (`Presence::stop_holds`).
    pub fn set_presence(&self, presence: Arc<Presence>) {
        *self
            .inner
            .presence
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(presence);
    }

    /// How the leader runs instance requests that followers hand it.
    pub fn set_instance_handler(&self, handler: InstanceHandler) {
        *self
            .inner
            .instance
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }

    /// How the leader runs the other requests followers hand it
    /// ([`Coordination::forward_request`]).
    pub fn set_request_handler(&self, handler: RequestHandler) {
        *self
            .inner
            .request
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }

    /// The name of the current leader if it is another peer; an error that
    /// says why there is none to ask otherwise.
    pub fn leader_name(&self) -> io::Result<String> {
        self.leader()
    }

    /// On a follower: run `request` on the leader and return its answer.
    /// The leader answers with the error its handler returned, or one that
    /// says it does not lead (any more).
    pub async fn forward_request(
        &self,
        request: ForwardedRequest,
        wait: Duration,
    ) -> io::Result<serde_json::Value> {
        let leader = self.leader()?;
        let answer = self
            .ask(&leader, &LoadMessage::Request { request }, wait)
            .await?;
        serde_json::from_slice(&answer).map_err(io::Error::other)
    }

    /// What this peer does when `rift admin cluster leave` names it.
    pub fn set_leave_handler(&self, handler: LeaveHandler) {
        *DRAIN.write().unwrap_or_else(|e| e.into_inner()) = Some(handler.clone());
        *self.inner.leave.write().unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }

    pub fn cluster(&self) -> &Cluster {
        &self.inner.cluster
    }

    /// The term this peer leads in while its lease holds.
    pub fn acting_term(&self) -> Option<u64> {
        acting_term(&self.inner.cluster)
    }

    /// [`Coordination::acting_term`], once this peer applied everything
    /// committed before that term: decisions from replicated state
    /// ([`decide`]) wait for it, so a new leader never decides from an
    /// older view than its predecessor's.
    pub fn deciding_term(&self) -> Option<u64> {
        self.acting_term()
            .filter(|term| self.inner.caught_up.load(Ordering::Acquire) == *term)
    }

    /// The reservations the other peers reported, summed per server; a
    /// report older than [`REPORT_FRESH`] counts as empty.
    pub fn remote_reservations(&self) -> BTreeMap<String, usize> {
        let mut sum: BTreeMap<String, usize> = BTreeMap::new();
        let reports = self.inner.reports.lock().unwrap_or_else(|e| e.into_inner());
        for (at, reservations) in reports.values() {
            if at.elapsed() > REPORT_FRESH {
                continue;
            }
            for (server, count) in reservations {
                let total = sum.entry(server.clone()).or_default();
                *total = total.saturating_add(*count);
            }
        }
        sum
    }

    /// The name of the current leader, if it is another peer.
    fn leader(&self) -> io::Result<String> {
        let leadership = self.inner.cluster.leadership();
        match leadership.leader_name {
            Some(name) if name != self.inner.cluster.name() => Ok(name),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "this peer leads, but its lease ran out; retry in a moment",
            )),
            None => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the cluster has no leader right now; retry once one is elected",
            )),
        }
    }

    /// Send `message` to `peer` and return its answer.
    async fn ask(&self, peer: &str, message: &LoadMessage, wait: Duration) -> io::Result<Bytes> {
        let body = serde_json::to_vec(message).map_err(io::Error::other)?;
        self.inner
            .cluster
            .links()
            .request(peer, StreamKind::Load, Bytes::from(body), wait)
            .await
            .map_err(|error| match remote_error(&error) {
                Some(remote) => io::Error::other(remote.message.clone()),
                None => error,
            })
    }

    /// On a follower: run an instance request on the leader.
    pub async fn forward_instance(
        &self,
        request: serde_json::Value,
        wait: Duration,
    ) -> io::Result<serde_json::Value> {
        let leader = self.leader()?;
        let answer = self
            .ask(&leader, &LoadMessage::Instance { request }, wait)
            .await?;
        serde_json::from_slice(&answer).map_err(io::Error::other)
    }

    /// Ask `peer` to drain, as `rift admin cluster leave` does.
    pub async fn ask_to_leave(cluster: &Cluster, peer: &str) -> io::Result<()> {
        let body = serde_json::to_vec(&LoadMessage::Leave).map_err(io::Error::other)?;
        cluster
            .links()
            .request(
                peer,
                StreamKind::Load,
                Bytes::from(body),
                Duration::from_secs(10),
            )
            .await
            .map(|_| ())
    }

    /// Stop the tasks [`Coordination::start`] spawned and give back the
    /// stop holds this peer granted.
    pub fn stop(&self) {
        for task in self
            .inner
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            task.abort();
        }
        self.inner
            .holds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.inner.managed.clear_coordinator();
    }

    /// One message on the `load` stream from `peer`.
    async fn receive(&self, peer: String, body: Bytes) -> Result<Bytes, String> {
        let message: LoadMessage = serde_json::from_slice(&body)
            .map_err(|error| format!("invalid load message: {error}"))?;
        match message {
            LoadMessage::Report { reservations } => {
                self.inner
                    .reports
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(peer, (Instant::now(), reservations));
                Ok(Bytes::new())
            }
            LoadMessage::StopQuery { server } => {
                let mut veto = self.inner.managed.has_reservation_or_inbound(&server);
                if let Some(hold) = self.inner.managed.quiesce(&server) {
                    // A reservation taken before the hold still counts.
                    veto = veto || hold.has_inbound();
                    if !veto {
                        self.inner
                            .holds
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push((Instant::now() + STOP_HOLD, hold));
                    }
                }
                serde_json::to_vec(&StopAnswer { veto })
                    .map(Bytes::from)
                    .map_err(|error| error.to_string())
            }
            LoadMessage::Managed {
                server,
                request,
                wait_ms,
            } => {
                if self.acting_term().is_none() {
                    return Err("this peer does not lead the cluster (any more); retry".into());
                }
                let managed = self.inner.managed.clone();
                let work = async move {
                    match request {
                        Forward::Start => managed.start(&server).await,
                        Forward::Stop => managed.stop(&server).await,
                        Forward::Demand => managed.ensure_running(&server).await,
                        Forward::ScaleStart => managed.request_scale_start(&server),
                    }
                };
                match tokio::time::timeout(Duration::from_millis(wait_ms), work).await {
                    Ok(Ok(())) => Ok(Bytes::new()),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err("the leader did not finish the request in time".into()),
                }
            }
            LoadMessage::Instance { request } => {
                if self.acting_term().is_none() {
                    return Err("this peer does not lead the cluster (any more); retry".into());
                }
                let handler = self
                    .inner
                    .instance
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .ok_or("the leader is still starting; retry")?;
                let answer = handler(request).await?;
                serde_json::to_vec(&answer)
                    .map(Bytes::from)
                    .map_err(|error| error.to_string())
            }
            LoadMessage::Request { request } => {
                let Some(term) = self.acting_term() else {
                    return Err("this peer does not lead the cluster (any more); retry".into());
                };
                let handler = self
                    .inner
                    .request
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .ok_or("the leader is still starting; retry")?;
                log_debug!(
                    "cluster: {peer} handed this leader {} for {}",
                    request.kind,
                    request.actor
                );
                let answer = in_term(term, handler(request)).await?;
                serde_json::to_vec(&answer)
                    .map(Bytes::from)
                    .map_err(|error| error.to_string())
            }
            LoadMessage::Leave => {
                let handler = self
                    .inner
                    .leave
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .ok_or("this peer cannot drain yet; stop it with SIGTERM")?;
                log_info!("cluster: {peer} asked this peer to leave; draining");
                handler();
                Ok(Bytes::new())
            }
        }
    }

    /// Wait until this replica shows `name` running, as the leader records it.
    async fn running_here(&self, name: &str) -> io::Result<()> {
        let deadline = Instant::now() + RUNNING_WAIT;
        loop {
            match self.recorded(name) {
                Some(state) if state.phase == Phase::Running.as_str() => return Ok(()),
                Some(state) if state.phase == Phase::Failed.as_str() => {
                    return Err(io::Error::other(state.last_error.unwrap_or_else(|| {
                        format!("{name} failed to start on the cluster leader")
                    })));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "the cluster leader started {name}, but this peer does not see it running yet"
                    ),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Coordinator for Coordination {
    fn acting_term(&self) -> Option<u64> {
        acting_term(&self.inner.cluster)
    }

    fn recorded(&self, name: &str) -> Option<ServerState> {
        self.inner
            .states
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    fn record(&self, name: &str, state: ServerState, term: u64) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned(), (state, term));
        self.inner.flush.notify_one();
    }

    fn forward(
        &self,
        name: &str,
        request: Forward,
        wait: Duration,
    ) -> CoordinatorFuture<io::Result<()>> {
        let this = self.clone();
        let name = name.to_owned();
        Box::pin(async move {
            let leader = this.leader()?;
            let message = LoadMessage::Managed {
                server: name.clone(),
                request,
                wait_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
            };
            // The leader answers within `wait`; the link waits a bit longer.
            this.ask(
                &leader,
                &message,
                wait.saturating_add(Duration::from_secs(2)),
            )
            .await?;
            if request == Forward::Demand {
                this.running_here(&name).await?;
            }
            Ok(())
        })
    }

    fn may_stop(&self, name: &str) -> CoordinatorFuture<bool> {
        let this = self.clone();
        let name = name.to_owned();
        Box::pin(async move {
            let held = this
                .inner
                .presence
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|presence| presence.is_held(&name));
            if held {
                return false;
            }
            let peers = match this.inner.cluster.peers() {
                Ok(peers) => peers,
                Err(error) => {
                    log_warn!(
                        "cluster: {name} stays running: the peer table is unreadable: {error}"
                    );
                    return false;
                }
            };
            let mut asks = tokio::task::JoinSet::new();
            for peer in peers {
                let skipped = matches!(peer.role, Role::Witness | Role::Pending);
                if peer.this_peer || peer.expired || skipped {
                    continue;
                }
                let this = this.clone();
                let message = LoadMessage::StopQuery {
                    server: name.clone(),
                };
                asks.spawn(async move {
                    let answer = this.ask(&peer.name, &message, STOP_QUERY_WAIT).await;
                    (peer.name, answer)
                });
            }
            let mut agreed = true;
            while let Some(joined) = asks.join_next().await {
                let Ok((peer, answer)) = joined else {
                    agreed = false;
                    continue;
                };
                match answer.and_then(|body| {
                    serde_json::from_slice::<StopAnswer>(&body).map_err(io::Error::other)
                }) {
                    Ok(StopAnswer { veto: false }) => {}
                    Ok(StopAnswer { veto: true }) => {
                        log_debug!(
                            "cluster: {peer} vetoed stopping {name}: a player is on the way"
                        );
                        agreed = false;
                    }
                    Err(error) => {
                        log_debug!("cluster: {name} stays running: {peer} did not answer: {error}");
                        agreed = false;
                    }
                }
            }
            agreed
        })
    }
}

/// Every second: report this peer's reservations to the leader, and give
/// back stop holds that ran out.
async fn report(inner: Weak<Inner>) {
    let mut tick = tokio::time::interval(REPORT_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(inner) = inner.upgrade() else { return };
        let coordination = Coordination { inner };
        {
            let now = Instant::now();
            coordination
                .inner
                .holds
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(until, _)| *until > now);
        }
        let leadership = coordination.inner.cluster.leadership();
        if leadership.is_leader {
            continue;
        }
        let Ok(leader) = coordination.leader() else {
            continue;
        };
        let reservations = coordination.inner.managed.reservations();
        if let Err(error) = coordination
            .ask(
                &leader,
                &LoadMessage::Report { reservations },
                REPORT_INTERVAL * 2,
            )
            .await
        {
            log_debug!("cluster: reservations not reported to {leader}: {error}");
        }
    }
}

/// Write the server states the leader's workers recorded, the latest per
/// server, each in the term it was decided in.
async fn flush(inner: Weak<Inner>) {
    loop {
        let Some(strong) = inner.upgrade() else {
            return;
        };
        tokio::select! {
            () = strong.flush.notified() => {}
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        let batch = std::mem::take(&mut *strong.pending.lock().unwrap_or_else(|e| e.into_inner()));
        for (name, (state, term)) in batch {
            if acting_term(&strong.cluster) != Some(term) {
                // A later leadership records afresh.
                continue;
            }
            let operation = Operation::Instances(InstanceOp::SetServerState {
                name: name.clone(),
                state: state.clone(),
            });
            match strong.cluster.run_in_term(operation, term).await {
                Ok(Outcome::Instances(InstanceOutcome::Failed(message))) => {
                    log_warn!("cluster: the state of {name} was not recorded: {message}");
                }
                Ok(_) => {}
                Err(error) => {
                    log_debug!("cluster: the state of {name} was not recorded yet: {error}");
                    // Retried unless a newer state is waiting already.
                    strong
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .entry(name)
                        .or_insert((state, term));
                }
            }
        }
    }
}
