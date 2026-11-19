//! An in-memory cluster for tests: several peers in one process, sharing one
//! ordered claim log and talking over direct calls.
//!
//! It implements the presence traits the way a Raft cluster does, minus the
//! network: [`Claims`] applies every operation to one [`ClaimTable`] in one
//! order (the "log") and tells every peer about the change; a partitioned
//! peer can propose nothing and learns about changes only once it is healed,
//! as a peer catching up with the log would. [`PlayerRouter`] calls the
//! target peer's [`Players::handle_remote`]. [`AttributeSink`] queues
//! updates until [`MemoryCluster::pump`] delivers them, which keeps the
//! order of deliveries in the test's hands and never calls one registry
//! while another is locked.
use super::{
    PlayerRegistry,
    attributes::{ApplyOutcome, AttributeSink, AttributeUpdate},
    claims::{ClaimChange, ClaimFuture, ClaimOp, ClaimRecord, ClaimTable, Claims, Holder, Refusal},
    identity::Identity,
};
use crate::extensions::{
    Players,
    routing::{PlayerRouter, RouteError, RouteFuture, WireRequest, WireResult},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
};
use tokio::sync::broadcast;

/// A simulated cluster. Cloning shares it.
#[derive(Clone, Default)]
pub struct MemoryCluster(Arc<Mutex<Shared>>);

#[derive(Default)]
struct Shared {
    table: ClaimTable,
    peers: BTreeMap<u16, Peer>,
    /// Attribute updates in flight, by receiving peer.
    inbox: Vec<(u16, AttributeUpdate)>,
    /// Full resyncs owed: (from, to).
    resyncs: BTreeSet<(u16, u16)>,
}

struct Peer {
    incarnation: u64,
    changes: broadcast::Sender<ClaimChange>,
    /// Changes a partitioned peer has not seen yet.
    backlog: Vec<ClaimChange>,
    partitioned: bool,
    players: Option<Players>,
}

impl Shared {
    fn apply(&mut self, op: &ClaimOp) -> Result<(), Refusal> {
        let mut changes = Vec::new();
        let result = self.table.apply(op, Some(&mut changes));
        for change in changes {
            for peer in self.peers.values_mut() {
                if peer.partitioned {
                    peer.backlog.push(change.clone());
                } else {
                    let _ = peer.changes.send(change.clone());
                }
            }
        }
        result
    }

    fn reachable(&self, peer: u16) -> bool {
        self.peers.get(&peer).is_some_and(|peer| !peer.partitioned)
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MemoryCluster {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        lock(&self.0)
    }

    /// Start `peer` as `incarnation`, or start it again: applying the join
    /// releases every claim of its older incarnations at once.
    pub fn join(&self, peer: u16, incarnation: u64) {
        let mut shared = self.lock();
        match shared.peers.get_mut(&peer) {
            Some(existing) => {
                existing.incarnation = incarnation;
                existing.partitioned = false;
                existing.players = None;
            }
            None => {
                shared.peers.insert(
                    peer,
                    Peer {
                        incarnation,
                        changes: broadcast::channel(1024).0,
                        backlog: Vec::new(),
                        partitioned: false,
                        players: None,
                    },
                );
            }
        }
        let _ = shared.apply(&ClaimOp::ExpirePeer {
            peer,
            before: Some(incarnation),
        });
    }

    /// The identity `peer` runs as.
    pub fn identity(&self, peer: u16) -> Identity {
        Identity {
            peer,
            incarnation: self
                .lock()
                .peers
                .get(&peer)
                .map_or(0, |peer| peer.incarnation),
        }
    }

    /// Make `players` peer `peer`: its identity, claims, attribute sink and
    /// router. Join the peer first.
    pub fn attach(&self, peer: u16, players: &Players) {
        let roster = players.roster();
        roster.assume_identity(self.identity(peer));
        roster.set_claims(self.claims(peer));
        roster.set_sink(Some(self.sink(peer)));
        players.set_router(self.router(peer));
        if let Some(entry) = self.lock().peers.get_mut(&peer) {
            entry.players = Some(players.clone());
        }
    }

    pub fn claims(&self, peer: u16) -> Arc<dyn Claims> {
        Arc::new(MemoryClaims {
            shared: Arc::downgrade(&self.0),
            peer,
        })
    }

    pub fn router(&self, peer: u16) -> Arc<dyn PlayerRouter> {
        Arc::new(MemoryRouter {
            shared: Arc::downgrade(&self.0),
            peer,
        })
    }

    pub fn sink(&self, peer: u16) -> Arc<dyn AttributeSink> {
        Arc::new(MemorySink {
            shared: Arc::downgrade(&self.0),
            peer,
        })
    }

    /// Cut `peer` off: it can neither propose claims nor reach other peers.
    pub fn partition(&self, peer: u16) {
        if let Some(entry) = self.lock().peers.get_mut(&peer) {
            entry.partitioned = true;
        }
    }

    /// Reconnect `peer`: it catches up with the claim changes it missed, and
    /// it and every other peer owe each other a full attribute resync.
    pub fn heal(&self, peer: u16) {
        let mut shared = self.lock();
        let others: Vec<u16> = shared
            .peers
            .keys()
            .copied()
            .filter(|other| *other != peer)
            .collect();
        if let Some(entry) = shared.peers.get_mut(&peer) {
            entry.partitioned = false;
            for change in entry.backlog.drain(..) {
                let _ = entry.changes.send(change);
            }
        }
        for other in others {
            shared.resyncs.insert((peer, other));
            shared.resyncs.insert((other, peer));
        }
    }

    /// The leader expires `peer`: every claim it held is released.
    pub fn expire(&self, peer: u16) {
        let _ = self
            .lock()
            .apply(&ClaimOp::ExpirePeer { peer, before: None });
    }

    /// Every live claim, as the log has them.
    pub fn live(&self) -> Vec<ClaimRecord> {
        self.lock().table.live().cloned().collect()
    }

    /// Deliver every queued attribute update and owed resync, until none is
    /// left. A receiver that misses a sequence number gets a full resync.
    pub fn pump(&self) {
        loop {
            let (inbox, resyncs, players) = {
                let mut shared = self.lock();
                let inbox = std::mem::take(&mut shared.inbox);
                let resyncs = std::mem::take(&mut shared.resyncs);
                let players: BTreeMap<u16, Players> = shared
                    .peers
                    .iter()
                    .filter(|(_, peer)| !peer.partitioned)
                    .filter_map(|(id, peer)| Some((*id, peer.players.clone()?)))
                    .collect();
                (inbox, resyncs, players)
            };
            if inbox.is_empty() && resyncs.is_empty() {
                return;
            }
            let mut owed = BTreeSet::new();
            for (to, update) in inbox {
                let from = update.peer;
                let Some(receiver) = players.get(&to) else {
                    continue;
                };
                if receiver.roster().apply_remote(update) == ApplyOutcome::NeedResync {
                    owed.insert((from, to));
                }
            }
            for (from, to) in resyncs.into_iter().chain(owed) {
                if let (Some(sender), Some(receiver)) = (players.get(&from), players.get(&to)) {
                    receiver
                        .roster()
                        .apply_remote(sender.roster().full_resync());
                }
            }
        }
    }
}

struct MemoryClaims {
    shared: Weak<Mutex<Shared>>,
    peer: u16,
}

impl Claims for MemoryClaims {
    fn propose(&self, op: ClaimOp) -> ClaimFuture<Result<(), Refusal>> {
        let result = match self.shared.upgrade() {
            Some(shared) => {
                let mut shared = lock(&shared);
                if shared.reachable(self.peer) {
                    shared.apply(&op)
                } else {
                    Err(Refusal::Unavailable("no quorum".into()))
                }
            }
            None => Err(Refusal::Unavailable("the cluster is gone".into())),
        };
        Box::pin(std::future::ready(result))
    }

    fn release(&self, key: String, holder: Holder) {
        // A partitioned peer's release waits for nothing here: the leader
        // expires the peer, which releases it anyway.
        if let Some(shared) = self.shared.upgrade() {
            let mut shared = lock(&shared);
            if shared.reachable(self.peer) {
                let _ = shared.apply(&ClaimOp::Release { key, holder });
            }
        }
    }

    fn live(&self) -> Vec<ClaimRecord> {
        let Some(shared) = self.shared.upgrade() else {
            return Vec::new();
        };
        let shared = lock(&shared);
        shared.table.live().cloned().collect()
    }

    fn subscribe(&self) -> broadcast::Receiver<ClaimChange> {
        let shared = self.shared.upgrade();
        let shared = shared.as_deref().map(lock);
        match shared
            .as_ref()
            .and_then(|shared| shared.peers.get(&self.peer))
        {
            Some(peer) => peer.changes.subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    fn networked(&self) -> bool {
        true
    }
}

struct MemoryRouter {
    shared: Weak<Mutex<Shared>>,
    peer: u16,
}

impl PlayerRouter for MemoryRouter {
    fn deliver(
        &self,
        peer: u16,
        request: WireRequest,
    ) -> RouteFuture<Result<WireResult, RouteError>> {
        let target = self.shared.upgrade().and_then(|shared| {
            let shared = lock(&shared);
            if !shared.reachable(self.peer) || !shared.reachable(peer) {
                return None;
            }
            shared.peers.get(&peer)?.players.clone()
        });
        Box::pin(async move {
            match target {
                Some(players) => Ok(players.handle_remote(request).await),
                None => Err(RouteError::Unreachable(peer)),
            }
        })
    }

    fn peers(&self) -> Vec<u16> {
        self.shared
            .upgrade()
            .map(|shared| {
                let shared = lock(&shared);
                if !shared.reachable(self.peer) {
                    return Vec::new();
                }
                shared
                    .peers
                    .iter()
                    .filter(|(id, peer)| **id != self.peer && !peer.partitioned)
                    .map(|(id, _)| *id)
                    .collect()
            })
            .unwrap_or_default()
    }
}

struct MemorySink {
    shared: Weak<Mutex<Shared>>,
    peer: u16,
}

impl AttributeSink for MemorySink {
    fn send(&self, update: AttributeUpdate) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let mut shared = lock(&shared);
        if !shared.reachable(self.peer) {
            return;
        }
        let receivers: Vec<u16> = shared
            .peers
            .keys()
            .copied()
            .filter(|peer| *peer != self.peer)
            .collect();
        for receiver in receivers {
            if shared.reachable(receiver) {
                shared.inbox.push((receiver, update.clone()));
            }
        }
    }
}

/// A registry of `peer` in `cluster`, without a player directory, for
/// tests of the roster alone.
pub fn registry(cluster: &MemoryCluster, peer: u16) -> Arc<PlayerRegistry> {
    let registry = Arc::new(PlayerRegistry::with_claims(cluster.claims(peer)));
    registry.assume_identity(cluster.identity(peer));
    registry
}
