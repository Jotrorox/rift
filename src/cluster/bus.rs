//! The message bus of a cluster (docs/clustering.md, "Message bus"): plan
//! step 5 on top of the seams in [`crate::messaging`].
//!
//! Every peer runs its own [`Broker`]. This module connects them:
//!
//! - **Federation** ([`federation`]): a [`crate::messaging::Federation`] over
//!   the peer links' `bus` stream. Frames to one peer wait in a bounded FIFO
//!   queue that one worker per peer sends in batches, each answered before
//!   the next goes out, so frames arrive in the order the broker sent them.
//!   A queue that overflows, or a batch that fails, drops what it held and
//!   makes the peer resend its whole interest; a link that goes down drops
//!   its queue, and the broker exchanges interest anew when it comes back.
//! - **Owner routing** ([`sessions`]): `rift.node.<n>.cmd` goes to the peer
//!   holding node `n`'s session claim, `rift.node.<n>.state` to the leader.
//!   A node agent connecting to any peer claims its session through Raft
//!   (`bus` entries); the peer that held the older session closes it, and
//!   the agent logs "replaced by a newer session of this node".
//! - **Bus logins** ([`tables`]): the key bus-login tokens derive from lives
//!   in the replicated state, so a token minted on one peer authenticates on
//!   every other. `rift cluster init` moves `local/instance-key` there
//!   ([`import_key`]); a cluster without one takes the key of the first peer
//!   that attaches. Without a cluster the file is used as before.
//! - **Retained streams** ([`streams`]): in a cluster every stream handle is
//!   routed to the stream service on the leader. Appends, consumers and
//!   acknowledgements of durable streams are `stream` entries, so a new
//!   leader rebuilds every stream and its acknowledged positions; records
//!   delivered but not acknowledged are delivered again after a leader
//!   change. Memory streams live on the leader and start empty after a
//!   change. The permission change log stays with each peer (see
//!   [`attach`]).
//! - **Addresses**: each peer records where bus clients reach it, so
//!   [`peer_addresses`] lists every peer's bus address for instances
//!   (`RIFT_BUS_ADDRESSES`) and the dashboard.
//! - **`rift.events.cluster`**: leader changes, membership changes and peers
//!   joining, expiring or leaving, published by every peer to its own
//!   subscribers (`applied` scope).
//!
//! # Wiring
//!
//! Create one [`BusState`] per peer, register [`components`] in
//! [`super::Options::components`] before [`Cluster::start`], and call
//! [`attach`] once the broker, the QUIC listener and the streams exist. Keep
//! the returned [`Attached`] for as long as the cluster runs.
mod federation;
mod sessions;
mod streams;
mod tables;
#[cfg(test)]
mod tests;

use super::{
    Cluster, LinkEvent, StreamKind, hex, node,
    state::{Component, EntryKind},
};
use crate::{
    instance_login::{self, Logins},
    messaging::{Broker, Stream, quic::NodeLogins},
};
use bytes::Bytes;
use federation::{Counters, PeerFederation};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io,
    path::Path,
    sync::{Arc, Mutex, RwLock, Weak},
    time::Duration,
};
use tables::BusEntry;
use tokio::{sync::watch, task::JoinHandle};

/// Where cluster events are published.
pub const EVENTS: &str = "rift.events.cluster";
/// First byte of a `bus` stream message: federation frames.
const FEDERATION: u8 = 0;
/// First byte of a `bus` stream message: a stream request for the leader.
const STREAMS: u8 = 1;

/// Where bus clients reach one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PeerAddress {
    pub peer: String,
    /// `host:port` of its messaging listener (`messaging.advertise`).
    pub address: String,
    /// The name its messaging certificate is verified for.
    pub server_name: String,
}

/// Where bus clients reach this peer, as [`attach`] announces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusAddress {
    pub address: String,
    pub server_name: String,
}

/// A node session of this peer and whether its claim was applied.
#[derive(Debug, Clone, Copy)]
struct LocalSession {
    id: u64,
    claimed: bool,
}

/// The bus state of one peer: what the components keep in memory and what
/// [`attach`] connects them to. Create it before the cluster starts.
pub struct BusState {
    cluster: RwLock<Weak<node::Inner>>,
    /// This peer's name and incarnation, once attached.
    me: RwLock<Option<(String, u64)>>,
    nodes: RwLock<Option<NodeLogins>>,
    logins: RwLock<Option<Arc<Logins>>>,
    /// Node name → the live peer holding its session claim.
    holders: RwLock<HashMap<String, String>>,
    /// Node name → this peer's session of it.
    sessions: Mutex<HashMap<String, LocalSession>>,
    /// Every live peer's bus address.
    addresses: watch::Sender<Vec<PeerAddress>>,
    /// The bus-login key in the replicated state.
    key: RwLock<Option<[u8; instance_login::KEY_BYTES]>>,
}

impl BusState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            cluster: RwLock::new(Weak::new()),
            me: RwLock::new(None),
            nodes: RwLock::new(None),
            logins: RwLock::new(None),
            holders: RwLock::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            addresses: watch::Sender::new(Vec::new()),
            key: RwLock::new(None),
        })
    }

    /// The cluster, while it runs and once attached.
    fn cluster(&self) -> Option<Cluster> {
        let inner = self
            .cluster
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()?;
        Some(Cluster { inner })
    }

    /// This peer's name and incarnation, once attached.
    fn me(&self) -> Option<(String, u64)> {
        self.me.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Every live peer's bus address, as of the last applied entry.
    pub fn addresses(&self) -> Vec<PeerAddress> {
        self.addresses.borrow().clone()
    }

    /// The peer that holds node `node`'s session, if a live one does.
    pub fn session_holder(&self, node: &str) -> Option<String> {
        self.holders
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(node)
            .cloned()
    }

    fn node_logins(&self) -> Option<NodeLogins> {
        self.nodes.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn instance_logins(&self) -> Option<Arc<Logins>> {
        self.logins
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The key changed in the replicated state (or was read at start).
    fn set_key(&self, key: Option<[u8; instance_login::KEY_BYTES]>) {
        *self.key.write().unwrap_or_else(|e| e.into_inner()) = key;
        if let (Some(key), Some(logins)) = (key, self.instance_logins()) {
            logins.set_key(key);
        }
    }

    fn replicated_key(&self) -> Option<[u8; instance_login::KEY_BYTES]> {
        *self.key.read().unwrap_or_else(|e| e.into_inner())
    }

    fn set_addresses(&self, addresses: Vec<PeerAddress>) {
        self.addresses.send_if_modified(|current| {
            if *current == addresses {
                false
            } else {
                *current = addresses;
                true
            }
        });
    }
}

/// The components every peer with a bus registers before the cluster
/// starts: `bus` entries (node session claims, the bus-login key, bus
/// addresses) and `stream` entries (retained streams).
pub fn components(state: &Arc<BusState>) -> Vec<Arc<dyn Component>> {
    vec![
        Arc::new(tables::BusTables {
            state: state.clone(),
        }),
        Arc::new(streams::StreamMachine),
    ]
}

/// What [`attach`] connects besides the cluster and the broker. Leave out
/// what this peer does not have.
#[derive(Default)]
pub struct AttachOptions {
    /// The retained streams by name: `messaging.streams` and the permission
    /// change log, the same handles the QUIC listener and plugins use.
    pub streams: HashMap<String, Stream>,
    /// The node logins of this peer's QUIC listener.
    pub nodes: Option<NodeLogins>,
    /// The instance logins of this peer.
    pub logins: Option<Arc<Logins>>,
    /// Where bus clients reach this peer.
    pub address: Option<BusAddress>,
}

/// A peer's bus, attached to its cluster. Dropping it stops its tasks;
/// [`Attached::shutdown`] also detaches the broker, the streams and the
/// node logins, which then serve this peer alone again.
pub struct Attached {
    tasks: Vec<JoinHandle<()>>,
    broker: Broker,
    federation: Arc<PeerFederation>,
    counters: Arc<Counters>,
    streams: Vec<Stream>,
    nodes: Option<NodeLogins>,
}

impl Attached {
    /// `rift_cluster_bus_forwarded_total` and `rift_cluster_bus_dropped_total`.
    pub fn render_metrics(&self) -> String {
        self.counters.render()
    }

    /// Detach: the broker, the streams and the node logins serve this peer
    /// alone again.
    pub fn shutdown(mut self) {
        self.stop();
        self.broker.clear_federation();
        for stream in &self.streams {
            stream.clear_route();
        }
        if let Some(nodes) = &self.nodes {
            nodes.clear_observer();
        }
    }

    fn stop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.federation.close_all();
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.stop();
    }
}

static METRICS: RwLock<Option<Weak<Counters>>> = RwLock::new(None);

/// The bus metrics of the attached cluster, empty without one; appended to
/// `/metrics` beside [`Cluster::render_metrics`].
pub fn render_metrics() -> String {
    METRICS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(Weak::upgrade)
        .map(|counters| counters.render())
        .unwrap_or_default()
}

/// Connect this peer's bus to `cluster`: federate `broker` with the other
/// peers, route node subjects, replicate node sessions and the bus-login
/// key, serve retained streams from the leader and publish
/// `rift.events.cluster`. `state` must be the one whose [`components`] the
/// cluster was started with.
///
/// The permission change log (`rift.perms-changes`) is not routed: each peer
/// appends every permission revision it applies, so its records are the
/// same on every peer, and a backend reads it on the peer it is connected
/// to.
pub async fn attach(
    cluster: &Cluster,
    broker: &Broker,
    state: &Arc<BusState>,
    options: AttachOptions,
) -> io::Result<Attached> {
    if state
        .cluster()
        .is_some_and(|attached| !Arc::ptr_eq(&attached.inner, &cluster.inner))
    {
        return Err(super::invalid(
            "cluster bus: this bus state is attached to another running cluster",
        ));
    }
    *state.cluster.write().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&cluster.inner);
    *state.me.write().unwrap_or_else(|e| e.into_inner()) =
        Some((cluster.name().to_owned(), cluster.incarnation()));
    *state.nodes.write().unwrap_or_else(|e| e.into_inner()) = options.nodes.clone();
    *state.logins.write().unwrap_or_else(|e| e.into_inner()) = options.logins.clone();
    // Mirrors of entries applied before attaching.
    cluster.state().read(|db| tables::reload(state, db))?;

    let counters = Arc::new(Counters::default());
    *METRICS.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&counters));
    let federation = PeerFederation::new(cluster.name(), cluster.links().clone(), counters.clone());
    let service = streams::Service::new(state.clone(), options.streams.clone());
    {
        let broker = broker.clone();
        let service = service.clone();
        cluster
            .links()
            .on_request(StreamKind::Bus, move |peer, body: Bytes| {
                let broker = broker.clone();
                let service = service.clone();
                async move {
                    match body.first().copied() {
                        Some(FEDERATION) => federation::receive(&broker, &peer, body.slice(1..))
                            .map(|()| Bytes::new()),
                        Some(STREAMS) => Ok(service.handle(body.slice(1..)).await),
                        _ => Err("unknown bus message".to_owned()),
                    }
                }
            });
    }
    let mut tasks = Vec::new();
    broker.set_federation(federation.clone());
    broker.set_owner_router(Arc::new(sessions::Router {
        state: state.clone(),
    }));
    tasks.push(tokio::spawn(follow_links(
        cluster.clone(),
        broker.clone(),
        federation.clone(),
    )));

    if let Some(nodes) = &options.nodes {
        nodes.set_observer(Arc::new(sessions::Observer {
            state: state.clone(),
        }));
    }

    if let Some(logins) = &options.logins {
        match state.replicated_key() {
            Some(key) => logins.set_key(key),
            // Keep the tokens this peer issued valid if it is the first.
            None => tasks.push(tokio::spawn(offer_key(state.clone(), logins.key()))),
        }
        tasks.push(tokio::spawn(follow_addresses(
            state.clone(),
            logins.clone(),
            options.address.clone(),
        )));
    }
    if let Some(address) = options.address.clone() {
        tasks.push(tokio::spawn(announce_address(state.clone(), address)));
    }

    {
        let gain = service.clone();
        cluster.on_leader_gain(move |context| {
            let service = gain.clone();
            async move { service.gain(context).await }
        });
        let loss = service.clone();
        cluster.on_leader_loss(move |term| loss.end(term));
    }
    let backend: Arc<dyn crate::messaging::stream::StreamBackend> =
        Arc::new(streams::Backend::new(service.clone()));
    let mut routed = Vec::new();
    for (name, stream) in &options.streams {
        if name == rift_permissions::wire::CHANGES {
            continue;
        }
        stream.route_through(name, backend.clone());
        routed.push(stream.clone());
    }

    tasks.push(tokio::spawn(publish_events(
        cluster.clone(),
        broker.clone(),
    )));
    Ok(Attached {
        tasks,
        broker: broker.clone(),
        federation,
        counters,
        streams: routed,
        nodes: options.nodes,
    })
}

/// Every live peer's bus address, from `cluster`'s replica.
pub fn peer_addresses(cluster: &Cluster) -> Vec<PeerAddress> {
    cluster
        .state()
        .read(tables::live_addresses)
        .unwrap_or_else(|error| {
            log_warn!("cluster bus: peer addresses: {error}");
            Vec::new()
        })
}

/// The bus addresses of the other live peers: what `RIFT_BUS_ADDRESSES`
/// adds after this peer's own address.
pub fn other_addresses(cluster: &Cluster) -> Vec<String> {
    peer_addresses(cluster)
        .into_iter()
        .filter(|address| address.peer != cluster.name())
        .map(|address| address.address)
        .collect()
}

/// `rift cluster init`: move the bus-login key file beside the configuration
/// at `config` (`local/instance-key`) into the replicated state, so every
/// peer derives and checks tokens from it, and remove the file. Returns
/// whether there was a file. Needs the cluster started with [`components`].
pub async fn import_key(cluster: &Cluster, config: &Path) -> io::Result<bool> {
    let path = instance_login::key_path(config);
    let key = match instance_login::read_key_file(&path) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("{}: {error}", path.display()),
            ));
        }
    };
    let entry = BusEntry::InstanceKey {
        key: hex(&key),
        replace: true,
    };
    cluster
        .propose(entry.entry())
        .await?
        .into_result()
        .map_err(io::Error::other)?;
    std::fs::remove_file(&path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    Ok(true)
}

/// Propose an entry until it is applied, refused or `keep` says it is no
/// longer wanted.
async fn propose_until_applied(
    state: &BusState,
    what: &str,
    entry: impl Fn(&Cluster) -> BusEntry,
    keep: impl Fn() -> bool,
) {
    let mut delay = Duration::from_millis(100);
    loop {
        if !keep() {
            return;
        }
        let Some(cluster) = state.cluster() else {
            return;
        };
        match cluster.propose(entry(&cluster).entry()).await {
            Ok(super::Reply::Ok(_)) => return,
            Ok(super::Reply::Err(reason)) => {
                log_warn!("cluster bus: {what} refused: {reason}");
                return;
            }
            Err(error) => log_debug!("cluster bus: {what}: {error}; retrying"),
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(5));
    }
}

/// Offer this peer's own key as the cluster's, applied only if the
/// replicated state has none yet.
async fn offer_key(state: Arc<BusState>, key: [u8; instance_login::KEY_BYTES]) {
    let key = hex(&key);
    propose_until_applied(
        &state,
        "bus-login key",
        |_| BusEntry::InstanceKey {
            key: key.clone(),
            replace: false,
        },
        || state.replicated_key().is_none(),
    )
    .await;
}

async fn announce_address(state: Arc<BusState>, address: BusAddress) {
    propose_until_applied(
        &state,
        "bus address",
        |cluster| BusEntry::Address {
            peer: cluster.name().to_owned(),
            incarnation: cluster.incarnation(),
            address: address.address.clone(),
            server_name: address.server_name.clone(),
        },
        || true,
    )
    .await;
}

/// Keep the instance logins' bus addresses current: this peer's own first,
/// then every other live peer's. When the peers' certificates name
/// different hosts, no single server name fits, and instances verify each
/// address for its own host.
async fn follow_addresses(state: Arc<BusState>, logins: Arc<Logins>, own: Option<BusAddress>) {
    let mut addresses = state.addresses.subscribe();
    loop {
        let list = addresses.borrow_and_update().clone();
        if let (Some(mut bus), Some((me, _))) = (logins.bus(), state.me()) {
            let others: Vec<&PeerAddress> = list.iter().filter(|peer| peer.peer != me).collect();
            let own_name = own
                .as_ref()
                .map_or_else(|| bus.server_name.clone(), |own| own.server_name.clone());
            let fallback: Vec<String> = others
                .iter()
                .map(|peer| peer.address.clone())
                .filter(|address| *address != bus.address)
                .collect();
            let server_name = if others.iter().all(|peer| peer.server_name == own_name) {
                own_name
            } else {
                String::new()
            };
            if bus.fallback_addresses != fallback || bus.server_name != server_name {
                bus.fallback_addresses = fallback;
                bus.server_name = server_name;
                logins.set_bus(bus);
            }
        }
        if addresses.changed().await.is_err() {
            return;
        }
    }
}

/// Whether `peer` runs a bus: it applies `bus` entries and is no witness
/// (a witness applies them too, but takes no bus clients).
fn runs_bus(cluster: &Cluster, peer: &str) -> bool {
    let applies = cluster
        .links()
        .hello(peer)
        .is_some_and(|hello| hello.kinds.iter().any(|kind| kind == EntryKind::Bus.name()));
    applies
        && !cluster
            .state()
            .peers()
            .unwrap_or_default()
            .iter()
            .any(|row| row.name == peer && row.witness)
}

/// Feed link changes to the federation and the broker.
async fn follow_links(cluster: Cluster, broker: Broker, federation: Arc<PeerFederation>) {
    use tokio::sync::broadcast::error::RecvError;
    let links = cluster.links().clone();
    let mut events = links.subscribe();
    let reconcile = |federation: &Arc<PeerFederation>| {
        let up: BTreeSet<String> = links
            .statuses()
            .into_iter()
            .filter(|status| status.up && runs_bus(&cluster, &status.name))
            .map(|status| status.name)
            .collect();
        for peer in federation.peer_names() {
            if !up.contains(&peer) {
                federation.down(&peer, &broker);
            }
        }
        for peer in up {
            if !federation.has(&peer) {
                federation.up(&peer, &broker);
            }
        }
    };
    reconcile(&federation);
    loop {
        match events.recv().await {
            Ok(LinkEvent::Up(peer)) => {
                if runs_bus(&cluster, &peer) {
                    federation.up(&peer, &broker);
                }
            }
            Ok(LinkEvent::Down(peer)) => federation.down(&peer, &broker),
            Err(RecvError::Lagged(_)) => reconcile(&federation),
            Err(RecvError::Closed) => return,
        }
    }
}

fn emit(broker: &Broker, event: serde_json::Value) {
    let _ = broker.publish(EVENTS, Bytes::from(event.to_string()));
}

/// Name → (incarnation, expired) of every row of the peer table.
fn peer_view(cluster: &Cluster) -> BTreeMap<String, (u64, bool)> {
    cluster
        .state()
        .peers()
        .unwrap_or_default()
        .into_iter()
        .map(|row| (row.name, (row.incarnation, row.expired)))
        .collect()
}

/// The `rift.events.cluster` events that turn `before` into `after`.
fn peer_events(
    before: &BTreeMap<String, (u64, bool)>,
    after: &BTreeMap<String, (u64, bool)>,
) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    for (name, &(incarnation, expired)) in after {
        let event = match before.get(name) {
            None if incarnation == 0 => Some("peer_added"),
            None => Some("peer_joined"),
            Some(&(previous, was_expired)) => {
                if incarnation > previous || (was_expired && !expired) {
                    Some("peer_joined")
                } else if expired && !was_expired {
                    Some("peer_expired")
                } else {
                    None
                }
            }
        };
        if let Some(event) = event {
            events.push(serde_json::json!({
                "event": event,
                "name": name,
                "incarnation": incarnation,
            }));
        }
    }
    for name in before.keys() {
        if !after.contains_key(name) {
            events.push(serde_json::json!({"event": "peer_removed", "name": name}));
        }
    }
    events
}

fn membership_event(cluster: &Cluster) -> Option<serde_json::Value> {
    let membership = cluster.membership()?;
    let voters: BTreeSet<super::PeerId> = membership.voter_ids().collect();
    let mut voter_names = Vec::new();
    let mut learner_names = Vec::new();
    for (id, node) in membership.nodes() {
        if voters.contains(id) {
            voter_names.push(node.name.clone());
        } else {
            learner_names.push(node.name.clone());
        }
    }
    Some(serde_json::json!({
        "event": "membership",
        "voters": voter_names,
        "learners": learner_names,
    }))
}

/// Publish `rift.events.cluster` to this peer's subscribers: each peer does
/// so when it observes a leader change or applies the entry, so every
/// subscriber gets one copy.
async fn publish_events(cluster: Cluster, broker: Broker) {
    use tokio::sync::broadcast::error::RecvError;
    let mut leadership = cluster.subscribe_leadership();
    let mut changes = cluster.state().subscribe_changes();
    let mut peers = peer_view(&cluster);
    let mut last = leadership.borrow_and_update().clone();
    loop {
        tokio::select! {
            changed = leadership.changed() => {
                if changed.is_err() {
                    return;
                }
                let now = leadership.borrow_and_update().clone();
                if now.leader != last.leader || now.term != last.term {
                    emit(&broker, serde_json::json!({
                        "event": "leader",
                        "term": now.term,
                        "leader": now.leader_name,
                        "peer": cluster.name(),
                    }));
                }
                last = now;
            }
            change = changes.recv() => match change {
                Ok(change) => {
                    if change.touched("cluster_membership")
                        && let Some(event) = membership_event(&cluster)
                    {
                        emit(&broker, event);
                    }
                    if change.touched("cluster_peers") {
                        let now = peer_view(&cluster);
                        for event in peer_events(&peers, &now) {
                            emit(&broker, event);
                        }
                        peers = now;
                    }
                }
                Err(RecvError::Lagged(_)) => peers = peer_view(&cluster),
                Err(RecvError::Closed) => return,
            }
        }
    }
}
