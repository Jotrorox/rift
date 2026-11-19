//! The `presence` stream: player attributes, service states and the probe
//! that tells a peer whether it is in contact with a leader that holds its
//! lease, and how far behind it is.
//!
//! Every peer keeps one outbox per other peer. The registry's
//! [`AttributeSink`] and the service states' [`ReportSync`] put each change
//! into every outbox without blocking. One task per outbox sends what
//! accumulated as one [`PresenceMessage::Batch`] request and waits for the
//! answer before it sends the next, so the receiver applies a peer's
//! updates in their sequence order. The first batch to a peer, the first
//! after its link came up again, after a failed send and after the receiver
//! reported a gap starts with a full resync of this peer's players
//! ([`PlayerRegistry::full_resync`](crate::players::PlayerRegistry::full_resync));
//! updates it already covers are dropped. Service state reports are whole
//! reports per server, kept until a send succeeds (the last one per server
//! wins).
//!
//! A peer that starts asks the first peer that answers for a full copy of
//! the service states.
use super::watch::Shared;
use crate::{
    cluster::{LinkEvent, StreamKind},
    players::attributes::{ApplyOutcome, AttributeSink, AttributeUpdate},
    service_state::{ReportSync, StateUpdate},
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::{
    sync::{broadcast::error::RecvError, mpsc},
    time::Instant,
};

/// How long one presence request waits for its answer.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Pause before an outbox tries a peer that was down again.
const RETRY: Duration = Duration::from_millis(250);
/// Attribute updates an outbox keeps for a peer it cannot reach; beyond
/// them it sends a full resync instead.
const MAX_BUFFERED: usize = 10_000;
/// How long a starting peer keeps asking for the service states.
const STATES_WINDOW: Duration = Duration::from_secs(60);

/// A request on the `presence` stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceMessage {
    /// The sender's attribute updates, in sequence order, and service state
    /// reports.
    Batch {
        attributes: Vec<AttributeUpdate>,
        states: Vec<StateUpdate>,
    },
    /// Whether the receiver leads with a lease, and what it applied.
    Probe,
    /// Every service state report the receiver knows.
    States,
}

/// The answer to a [`PresenceMessage`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceReply {
    /// A batch was applied; with `resync`, a sequence number was missing
    /// and the sender owes a full resync.
    Applied {
        resync: bool,
    },
    Probe {
        applied: u64,
        lease: bool,
        /// Whether the receiver admits logins now (not fenced, catching up
        /// or draining). Peers of earlier releases leave it out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        admits: Option<bool>,
    },
    States(Vec<StateUpdate>),
}

/// One item of an outbox.
#[derive(Debug, Clone)]
pub enum Outbound {
    Attributes(AttributeUpdate),
    State(StateUpdate),
    /// Send a full resync next.
    Resync,
}

/// Send `message` to `peer` and wait for its answer.
pub async fn request(
    shared: &Shared,
    peer: &str,
    message: &PresenceMessage,
    timeout: Duration,
) -> io::Result<PresenceReply> {
    let body = serde_json::to_vec(message).map_err(crate::cluster::json_error)?;
    let reply = shared
        .cluster
        .links()
        .request(peer, StreamKind::Presence, Bytes::from(body), timeout)
        .await?;
    serde_json::from_slice(&reply).map_err(crate::cluster::json_error)
}

/// Answer presence requests from other peers.
pub fn serve(shared: &Arc<Shared>) {
    let weak = Arc::downgrade(shared);
    shared
        .cluster
        .links()
        .on_request(StreamKind::Presence, move |_peer, body| {
            let shared = weak.upgrade();
            async move {
                match shared {
                    Some(shared) => answer(&shared, &body),
                    None => Err("presence stopped".to_owned()),
                }
            }
        });
}

fn answer(shared: &Shared, body: &[u8]) -> Result<Bytes, String> {
    let message: PresenceMessage =
        serde_json::from_slice(body).map_err(|error| format!("presence request: {error}"))?;
    let reply = match message {
        PresenceMessage::Batch { attributes, states } => {
            let mut resync = false;
            for update in attributes {
                if shared.registry.apply_remote(update) == ApplyOutcome::NeedResync {
                    resync = true;
                }
            }
            for update in states {
                shared.states.apply_remote(update);
            }
            PresenceReply::Applied { resync }
        }
        PresenceMessage::Probe => PresenceReply::Probe {
            applied: shared.cluster.state().applied_index(),
            lease: shared.cluster.is_leader_with_lease(),
            admits: Some(shared.admits()),
        },
        PresenceMessage::States => PresenceReply::States(shared.states.full_copy()),
    };
    serde_json::to_vec(&reply)
        .map(Bytes::from)
        .map_err(|error| format!("presence answer: {error}"))
}

/// The registry's sink: every change of this peer's players goes to every
/// outbox.
pub struct AttributeOut(Weak<Shared>);

impl AttributeOut {
    pub fn new(shared: &Arc<Shared>) -> Self {
        Self(Arc::downgrade(shared))
    }
}

impl AttributeSink for AttributeOut {
    fn send(&self, update: AttributeUpdate) {
        if let Some(shared) = self.0.upgrade() {
            shared.broadcast(&Outbound::Attributes(update));
        }
    }
}

/// The service states' sync: every report this peer's backends make goes
/// to every outbox.
pub struct StateOut(Weak<Shared>);

impl StateOut {
    pub fn new(shared: &Arc<Shared>) -> Self {
        Self(Arc::downgrade(shared))
    }
}

impl ReportSync for StateOut {
    fn send(&self, update: StateUpdate) {
        if let Some(shared) = self.0.upgrade() {
            shared.broadcast(&Outbound::State(update));
        }
    }
}

/// The outbox for `peer`, created (and its task started) if there is none;
/// with `resync`, an existing one sends a full resync next. Nothing before
/// this peer has its identity.
pub fn ensure_outbox(shared: &Arc<Shared>, peer: &str, resync: bool) {
    if !shared.is_ready() || peer == shared.cluster.name() {
        return;
    }
    let receiver = {
        let mut outboxes = shared.lock_outboxes();
        if let Some(outbox) = outboxes.get(peer) {
            if resync {
                let _ = outbox.send(Outbound::Resync);
            }
            return;
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        outboxes.insert(peer.to_owned(), sender);
        receiver
    };
    let task = tokio::spawn(run_outbox(shared.clone(), peer.to_owned(), receiver));
    shared.add_task(task);
}

/// An outbox for every other peer in the peer table.
pub fn connect_all(shared: &Arc<Shared>) {
    if !shared.is_ready() {
        return;
    }
    let Ok(rows) = shared.cluster.state().peers() else {
        return;
    };
    for row in rows {
        ensure_outbox(shared, &row.name, false);
    }
}

/// Keep outboxes for peers whose links come up, and resync them.
pub async fn follow_links(shared: Arc<Shared>) {
    let mut events = shared.cluster.links().subscribe();
    loop {
        match events.recv().await {
            Ok(LinkEvent::Up(peer)) => ensure_outbox(&shared, &peer, true),
            Ok(LinkEvent::Down(_)) => {}
            Err(RecvError::Lagged(_)) => {
                connect_all(&shared);
                shared.broadcast(&Outbound::Resync);
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// What an outbox holds between two sends.
#[derive(Default)]
struct Pending {
    resync: bool,
    attributes: Vec<AttributeUpdate>,
    states: BTreeMap<String, StateUpdate>,
}

impl Pending {
    fn take(&mut self, item: Outbound) {
        match item {
            Outbound::Attributes(update) => {
                if self.attributes.len() >= MAX_BUFFERED {
                    self.attributes.clear();
                    self.resync = true;
                } else {
                    self.attributes.push(update);
                }
            }
            Outbound::State(update) => {
                self.states.insert(update.server.clone(), update);
            }
            Outbound::Resync => self.resync = true,
        }
    }

    fn is_empty(&self) -> bool {
        !self.resync && self.attributes.is_empty() && self.states.is_empty()
    }
}

/// Send `peer` what its outbox collects, one batch at a time.
async fn run_outbox(
    shared: Arc<Shared>,
    peer: String,
    mut queue: mpsc::UnboundedReceiver<Outbound>,
) {
    let mut pending = Pending {
        resync: true,
        ..Pending::default()
    };
    // The sequence number the last full resync covered: updates up to it
    // that were still queued then are not sent again.
    let mut covered = 0;
    loop {
        if pending.is_empty() {
            match queue.recv().await {
                Some(item) => pending.take(item),
                None => return,
            }
        }
        while let Ok(item) = queue.try_recv() {
            pending.take(item);
        }
        if !shared.cluster.links().is_up(&peer) {
            // It gets everything in one resync once it is back.
            pending.resync = true;
            pending.attributes.clear();
            tokio::time::sleep(RETRY).await;
            continue;
        }
        let mut attributes = Vec::with_capacity(pending.attributes.len() + 1);
        if shared.is_withheld() {
            // Expired and not done claiming its players again: they hold
            // no claims, so other peers must not list them yet. Service
            // states still travel.
            pending.resync = true;
            pending.attributes.clear();
            if pending.states.is_empty() {
                tokio::time::sleep(RETRY).await;
                continue;
            }
        } else {
            if pending.resync {
                let full = shared.registry.full_resync();
                covered = full.sequence;
                attributes.push(full);
                pending.resync = false;
            }
            attributes.extend(
                pending
                    .attributes
                    .drain(..)
                    .filter(|update| update.sequence > covered),
            );
        }
        let states: Vec<StateUpdate> = pending.states.values().cloned().collect();
        let message = PresenceMessage::Batch { attributes, states };
        match request(&shared, &peer, &message, REQUEST_TIMEOUT).await {
            Ok(PresenceReply::Applied { resync }) => {
                pending.resync |= resync;
                pending.states.clear();
            }
            Ok(other) => {
                log_debug!("cluster: unexpected presence answer from {peer}: {other:?}");
                pending.resync = true;
                tokio::time::sleep(RETRY).await;
            }
            Err(error) => {
                log_debug!("cluster: presence to {peer}: {error}");
                pending.resync = true;
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Install the service states of the first peer that answers, once.
pub async fn fetch_states(shared: Arc<Shared>) {
    let deadline = Instant::now() + STATES_WINDOW;
    while Instant::now() < deadline {
        let peers: Vec<String> = shared
            .cluster
            .links()
            .statuses()
            .into_iter()
            .filter(|status| status.up)
            .map(|status| status.name)
            .collect();
        for peer in peers {
            if let Ok(PresenceReply::States(copy)) =
                request(&shared, &peer, &PresenceMessage::States, REQUEST_TIMEOUT).await
            {
                log_debug!(
                    "cluster: installed {} service state reports from {peer}",
                    copy.len()
                );
                shared.states.install_copy(copy);
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
