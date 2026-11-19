//! This peer's identity, its contact with the leader, and what follows
//! from losing it (docs/clustering.md, "When a peer disappears").
//!
//! - **Contact.** A follower probes the leader over the `presence` stream;
//!   only an answer from a leader that still holds its lease counts, so a
//!   peer that can reach a leader cut off from the majority is not in
//!   contact. A leader is in contact while its lease holds.
//! - **Fencing.** Without contact for half of `cluster.peer_timeout`, the
//!   peer refuses logins with `cluster.partition_message`. The leader
//!   expires a peer it has not heard from for the whole `peer_timeout`, so a
//!   peer has always stopped admitting before its claims are released.
//! - **Partition grace.** Still cut off `cluster.partition_grace` after it
//!   fenced itself, the peer disconnects its players with the partition
//!   message, so they can log in through a healthy peer.
//! - **Rejoin.** In contact again and expired, the peer proposes `peer
//!   joined` with the incarnation it runs as and claims its players again
//!   ([`Players::reclaim`]); a claim that somebody else holds now
//!   disconnects the local session.
//! - **Catching up.** In contact, it refuses logins while it has not joined
//!   with its current incarnation, while it has not applied what the
//!   leader had applied more than a second ago, and while its replica is
//!   diverged.
//!
//! Expiring a peer is the leader's maintenance (`Cluster::maintain` in
//! `node.rs`), which starts every peer's timer afresh when leadership
//! changes. Applying the expiry here holds back idle stops of the servers
//! the expired peer's players were on ([`super::Presence::stop_holds`]).
use super::{claims::ClaimShared, presence};
use crate::{
    cluster::{
        Cluster, Entry, EntryKind, Reply, VERSION,
        state::{PeerEntry, PeerRow},
    },
    config::ClusterConfig,
    extensions::Players,
    players::{
        Admission, PlayerRegistry,
        identity::{self, Identity, MAX_PEER},
    },
    service_state::ServiceStates,
    text::{
        Text,
        catalog::{self, Notice},
    },
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast::error::RecvError, mpsc},
    task::JoinHandle,
    time::Instant,
};

/// How far behind the leader a peer may trail and still admit players.
pub const CATCH_UP: Duration = Duration::from_secs(1);
/// Leader positions remembered for [`CATCH_UP`].
const OBSERVED: usize = 64;

/// What the leader told this peer, and when.
struct Contact {
    /// The last moment this peer was in contact with a leader holding its
    /// lease (or held the lease itself).
    last: Instant,
    /// A probe succeeded since the start.
    probed: bool,
    /// The leader's applied index at the moments it answered, oldest first,
    /// as long as this peer has not applied it.
    observed: VecDeque<(Instant, u64)>,
}

/// Everything the presence tasks of one peer share.
pub struct Shared {
    pub(super) cluster: Cluster,
    pub(super) registry: Arc<PlayerRegistry>,
    pub(super) players: Players,
    pub(super) states: ServiceStates,
    pub(super) claims: Arc<ClaimShared>,
    process_identity: bool,
    ready: AtomicBool,
    warned: AtomicBool,
    /// Expired, and not done claiming its players again.
    withheld: AtomicBool,
    outboxes: Mutex<BTreeMap<String, mpsc::UnboundedSender<presence::Outbound>>>,
    contact: Mutex<Contact>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Why logins are refused right now; `None` while they are admitted.
    held_back: Mutex<Option<String>>,
}

impl Shared {
    pub(super) fn new(
        cluster: Cluster,
        registry: Arc<PlayerRegistry>,
        players: Players,
        states: ServiceStates,
        claims: Arc<ClaimShared>,
        process_identity: bool,
    ) -> Self {
        Self {
            cluster,
            registry,
            players,
            states,
            claims,
            process_identity,
            ready: AtomicBool::new(false),
            warned: AtomicBool::new(false),
            withheld: AtomicBool::new(false),
            outboxes: Mutex::default(),
            contact: Mutex::new(Contact {
                last: Instant::now(),
                probed: false,
                observed: VecDeque::new(),
            }),
            tasks: Mutex::default(),
            held_back: Mutex::default(),
        }
    }

    /// Refuse logins with `admission` (never [`Admission::Open`]) because
    /// of `reason`, which [`Shared::held_back`] reports.
    pub(super) fn hold_back(&self, admission: Admission, reason: impl Into<String>) {
        let reason = reason.into();
        {
            let mut held = self
                .held_back
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if held.as_deref() != Some(reason.as_str()) {
                log_debug!("cluster: {} refuses logins: {reason}", self.cluster.name());
                *held = Some(reason);
            }
        }
        if self.registry.admission() != admission {
            self.registry.set_admission(admission);
        }
    }

    /// Admit logins.
    fn admit(&self) {
        let was = self
            .held_back
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if was.is_some() {
            log_debug!("cluster: {} admits logins", self.cluster.name());
        }
        if self.registry.admission() != Admission::Open {
            self.registry.set_admission(Admission::Open);
        }
    }

    /// Why this peer refuses logins now, `None` while it admits them.
    pub(super) fn held_back(&self) -> Option<String> {
        self.held_back
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether this peer has its identity, so that it may mint holders and
    /// send attributes.
    pub(super) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Whether this peer's players are kept from other peers: it was
    /// expired and has not claimed them again yet.
    pub(super) fn is_withheld(&self) -> bool {
        self.withheld.load(Ordering::Acquire)
    }

    /// Take this peer's Raft id and incarnation as its identity once it has
    /// a Raft id; whether it has its identity now.
    pub(super) fn install_identity(&self) -> bool {
        if self.is_ready() {
            return true;
        }
        let Some(raft_id) = self.cluster.raft_id() else {
            return false;
        };
        let Some(peer) = u16::try_from(raft_id)
            .ok()
            .filter(|peer| (1..=MAX_PEER).contains(peer))
        else {
            if !self.warned.swap(true, Ordering::Relaxed) {
                log_error!(
                    "cluster: raft id {raft_id} of {} does not fit in a connection id (1..={MAX_PEER}); this peer admits no players",
                    self.cluster.name()
                );
            }
            return false;
        };
        let incarnation = self.cluster.incarnation();
        if self.process_identity
            && let Err(error) = identity::set_identity(peer, incarnation)
        {
            if !self.warned.swap(true, Ordering::Relaxed) {
                log_error!("cluster: {error}; this peer admits no players");
            }
            return false;
        }
        self.registry
            .assume_identity(Identity { peer, incarnation });
        if self.process_identity {
            identity::set_peer_name(peer, self.cluster.name());
        }
        self.ready.store(true, Ordering::Release);
        log_info!(
            "cluster: {} serves players as peer {peer}, incarnation {incarnation}",
            self.cluster.name()
        );
        true
    }

    pub(super) fn lock_outboxes(
        &self,
    ) -> MutexGuard<'_, BTreeMap<String, mpsc::UnboundedSender<presence::Outbound>>> {
        self.outboxes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Put `item` into every outbox. Never blocks.
    pub(super) fn broadcast(&self, item: &presence::Outbound) {
        for outbox in self.lock_outboxes().values() {
            let _ = outbox.send(item.clone());
        }
    }

    pub(super) fn add_task(&self, task: JoinHandle<()>) {
        self.add_tasks(vec![task]);
    }

    pub(super) fn add_tasks(&self, added: Vec<JoinHandle<()>>) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.retain(|task| !task.is_finished());
        tasks.extend(added);
    }

    /// Stop every task and outbox.
    pub(super) fn stop(&self) {
        self.registry.set_sink(None);
        self.states.set_sync(None);
        self.lock_outboxes().clear();
        let tasks: Vec<JoinHandle<()>> =
            std::mem::take(&mut *self.tasks.lock().unwrap_or_else(PoisonError::into_inner));
        for task in tasks {
            task.abort();
        }
    }

    fn lock_contact(&self) -> MutexGuard<'_, Contact> {
        self.contact.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// In contact now; `applied` is the leader's applied index (`None`:
    /// this peer leads).
    fn contacted(&self, applied: Option<u64>) {
        let now = Instant::now();
        let mut contact = self.lock_contact();
        contact.last = now;
        contact.probed = true;
        match applied {
            Some(index) => {
                if contact
                    .observed
                    .back()
                    .is_none_or(|(_, newest)| *newest < index)
                {
                    contact.observed.push_back((now, index));
                }
                if contact.observed.len() > OBSERVED {
                    contact.observed.pop_front();
                }
            }
            None => contact.observed.clear(),
        }
    }

    fn last_contact(&self) -> Instant {
        self.lock_contact().last
    }

    /// Whether this peer has not applied what the leader had applied more
    /// than [`CATCH_UP`] ago, or never heard from the leader.
    fn behind(&self) -> bool {
        let applied = self.cluster.state().applied_index();
        let mut contact = self.lock_contact();
        if !contact.probed {
            return true;
        }
        while contact
            .observed
            .front()
            .is_some_and(|(_, index)| *index <= applied)
        {
            contact.observed.pop_front();
        }
        contact
            .observed
            .front()
            .is_some_and(|(at, _)| at.elapsed() > CATCH_UP)
    }

    /// What the leader had applied when it last answered (0 when this peer
    /// leads or had applied all of it); `None` before the first answer.
    fn leader_applied(&self) -> Option<u64> {
        let contact = self.lock_contact();
        contact
            .probed
            .then(|| contact.observed.back().map_or(0, |(_, index)| *index))
    }

    /// This peer's row in the peer table.
    fn own_row(&self) -> Option<PeerRow> {
        self.cluster
            .state()
            .peers()
            .ok()?
            .into_iter()
            .find(|row| row.name == self.cluster.name())
    }

    /// Name the other peers for listings; forget the players and attribute
    /// streams of peers that expired (once per expiry). `dropped` holds the
    /// peers that are expired now.
    fn sync_peers(&self, dropped: &mut BTreeSet<String>) {
        let Ok(rows) = self.cluster.state().peers() else {
            return;
        };
        for row in rows {
            let Some(peer) = u16::try_from(row.raft_id)
                .ok()
                .filter(|peer| (1..=MAX_PEER).contains(peer))
            else {
                continue;
            };
            // The names are process-wide, like the identity.
            if self.process_identity {
                identity::set_peer_name(peer, row.name.clone());
            }
            if row.name == self.cluster.name() {
                continue;
            }
            if !row.expired {
                dropped.remove(&row.name);
            } else if dropped.insert(row.name.clone()) {
                self.registry.drop_peer(peer);
            }
        }
    }

    /// Propose `peer joined` with the incarnation this peer runs as, after
    /// the leader expired it; whether it applied.
    async fn rejoin(&self) -> bool {
        let Some(raft_id) = self.cluster.raft_id() else {
            return false;
        };
        let settings = self.cluster.settings();
        let entry = Entry::Peer(PeerEntry::Joined {
            v: 1,
            name: settings.peer.clone(),
            raft_id,
            incarnation: self.cluster.incarnation(),
            address: settings.advertise.clone(),
            public_address: settings.public_address.clone(),
            labels: settings.labels.clone(),
            witness: settings.witness,
            version: VERSION.to_owned(),
            kinds: self.cluster.links().identity().kinds.clone(),
        });
        match self.cluster.propose(entry).await {
            Ok(Reply::Ok(_)) => {
                log_info!(
                    "cluster: {} joined again with incarnation {} after it was expired",
                    settings.peer,
                    self.cluster.incarnation()
                );
                true
            }
            Ok(Reply::Err(reason)) => {
                log_warn!("cluster: joining again was refused: {reason}");
                false
            }
            Err(error) => {
                log_debug!("cluster: joining again: {error}");
                false
            }
        }
    }
}

/// How often contact is probed and the admission decided: a tenth of
/// `peer_timeout`, between 50 and 500 ms.
fn interval(config: &ClusterConfig) -> Duration {
    (config.peer_timeout / 10).clamp(Duration::from_millis(50), Duration::from_millis(500))
}

/// The configured partition message, or `None` for the catalogue's
/// (translatable) default.
fn partition_message(config: &ClusterConfig) -> Option<Text> {
    (config.partition_message != ClusterConfig::default().partition_message)
        .then(|| Text::Plain(config.partition_message.clone()))
}

/// Probe the leader for contact and its applied index.
pub async fn probe(shared: Arc<Shared>) {
    let config = shared.cluster.shared().clone();
    let every = interval(&config);
    let timeout = (config.peer_timeout / 4).min(presence::REQUEST_TIMEOUT);
    loop {
        tokio::time::sleep(every).await;
        if shared.cluster.is_leader_with_lease() {
            shared.contacted(None);
            continue;
        }
        let Some(leader) = shared
            .cluster
            .leadership()
            .leader_name
            .filter(|leader| leader.as_str() != shared.cluster.name())
        else {
            continue;
        };
        match presence::request(&shared, &leader, &presence::PresenceMessage::Probe, timeout).await
        {
            Ok(presence::PresenceReply::Probe {
                applied,
                lease: true,
            }) => shared.contacted(Some(applied)),
            Ok(_) => {}
            Err(error) => log_debug!("cluster: probing the leader {leader}: {error}"),
        }
    }
}

/// Decide the admission, fence, disconnect after the grace, rejoin.
pub async fn fence(shared: Arc<Shared>) {
    let config = shared.cluster.shared().clone();
    let every = interval(&config);
    let half = config.peer_timeout / 2;
    let message = partition_message(&config);
    let mut fenced_since: Option<Instant> = None;
    let mut disconnected = false;
    let mut reclaim = false;
    // After the start and after every fence: admit only once this replica
    // holds what the leader had applied when contact (re)started, so that a
    // peer that comes back sees its own expiry before it admits anybody.
    let mut resumed = true;
    let mut target: Option<u64> = None;
    shared.hold_back(Admission::CatchingUp, "starting");
    loop {
        tokio::time::sleep(every).await;
        let was_ready = shared.is_ready();
        if !shared.install_identity() {
            shared.hold_back(
                Admission::CatchingUp,
                "no identity yet: this peer has no usable raft id",
            );
            continue;
        }
        if !was_ready {
            presence::connect_all(&shared);
        }
        let now = Instant::now();
        if now.saturating_duration_since(shared.last_contact()) > half {
            let since = *fenced_since.get_or_insert_with(|| {
                log_warn!(
                    "cluster: {} lost contact with the leader; it refuses logins",
                    shared.cluster.name()
                );
                now
            });
            shared.hold_back(
                Admission::Fenced {
                    message: message.clone(),
                },
                "fenced: no contact with a leader holding its lease",
            );
            if !disconnected && now.saturating_duration_since(since) >= config.partition_grace {
                disconnected = true;
                log_warn!(
                    "cluster: {} is still cut off after {} ms; disconnecting its players",
                    shared.cluster.name(),
                    config.partition_grace.as_millis()
                );
                let players = shared.players.clone();
                let reason = message
                    .clone()
                    .unwrap_or_else(|| catalog::notice(Notice::ClusterPartition));
                shared.add_task(tokio::spawn(async move {
                    let kicked = players.disconnect_all(reason).await;
                    log_info!("cluster: disconnected {kicked} players after the partition grace");
                }));
            }
            continue;
        }
        if fenced_since.take().is_some() {
            log_info!(
                "cluster: {} is in contact with the leader again",
                shared.cluster.name()
            );
            resumed = true;
            target = None;
        }
        disconnected = false;
        if resumed {
            if target.is_none() {
                target = shared.leader_applied();
            }
            let applied = shared.cluster.state().applied_index();
            match target {
                Some(index) if applied >= index => {
                    resumed = false;
                    target = None;
                }
                Some(index) => {
                    shared.hold_back(
                        Admission::CatchingUp,
                        format!(
                            "catching up: applied {applied}, the leader had applied {index} when contact started"
                        ),
                    );
                    continue;
                }
                None => {
                    shared.hold_back(
                        Admission::CatchingUp,
                        "no answer from a leader holding its lease yet",
                    );
                    continue;
                }
            }
        }
        let incarnation = shared.cluster.incarnation();
        let own = shared.own_row();
        if own
            .as_ref()
            .is_some_and(|row| row.expired && row.incarnation == incarnation)
        {
            shared.withheld.store(true, Ordering::Release);
            shared.hold_back(
                Admission::CatchingUp,
                format!("expired with incarnation {incarnation}; joining again"),
            );
            if shared.rejoin().await {
                reclaim = true;
            }
            continue;
        }
        let joined = own
            .as_ref()
            .is_some_and(|row| !row.expired && row.incarnation == incarnation);
        if joined && reclaim {
            reclaim = false;
            let task = shared.clone();
            shared.add_task(tokio::spawn(async move {
                let lost = task.players.reclaim().await;
                if lost > 0 {
                    log_warn!(
                        "cluster: {lost} players logged in elsewhere while this peer was expired; disconnected here"
                    );
                }
                task.withheld.store(false, Ordering::Release);
                // Other peers dropped this peer's players at the expiry.
                task.broadcast(&presence::Outbound::Resync);
            }));
        }
        // Claims cannot be proposed before a committed features entry
        // enables them.
        let claims = shared.cluster.is_enabled(EntryKind::Claim).unwrap_or(false);
        let reason = if !joined {
            Some(match &own {
                None => "not in the peer table yet".to_owned(),
                Some(row) => format!(
                    "has not joined with incarnation {incarnation} yet (the peer table has incarnation {}{})",
                    row.incarnation,
                    if row.expired { ", expired" } else { "" }
                ),
            })
        } else if !claims {
            Some("claim entries are not enabled by a committed features entry yet".to_owned())
        } else if shared.behind() {
            Some(format!(
                "no probe answered yet, or more than {} ms behind the leader (applied {})",
                CATCH_UP.as_millis(),
                shared.cluster.state().applied_index()
            ))
        } else if shared.cluster.state().is_diverged() {
            Some("the replica diverged".to_owned())
        } else {
            None
        };
        match reason {
            Some(reason) => shared.hold_back(Admission::CatchingUp, reason),
            None => shared.admit(),
        }
    }
}

/// Follow the peer table: names for listings, and the players of expired
/// peers leave the roster.
pub async fn follow_peers(shared: Arc<Shared>) {
    let mut changes = shared.cluster.state().subscribe_changes();
    let mut dropped = BTreeSet::new();
    shared.sync_peers(&mut dropped);
    loop {
        match changes.recv().await {
            Ok(applied) if !applied.touched("cluster_peers") => continue,
            Ok(_) | Err(RecvError::Lagged(_)) => shared.sync_peers(&mut dropped),
            Err(RecvError::Closed) => return,
        }
    }
}
