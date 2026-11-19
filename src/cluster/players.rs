//! Presence and routed player actions in a cluster (docs/clustering.md,
//! "Players", plan steps 4 and 6).
//!
//! Everything here sits on seams that work the same without a cluster:
//! [`Claims`](crate::players::claims::Claims),
//! [`AttributeSink`](crate::players::attributes::AttributeSink),
//! [`PlayerRouter`](crate::extensions::routing::PlayerRouter) and
//! [`ReportSync`](crate::service_state::ReportSync). A single Rift never
//! calls this module, and keeps its in-process implementations.
//!
//! | Part | How it travels | Module |
//! | --- | --- | --- |
//! | Name and UUID claims | `claim` entries in the Raft log, group-committed per peer, applied to a persistent table | [`claims`] |
//! | Player attributes, service states | the `presence` stream, per-peer sequence numbers, full resync on reconnect | [`presence`] |
//! | Player actions, broadcasts, transfers, dialog clicks, context reports | requests on the `player` stream | [`router`] |
//! | Identity, fencing, partition grace, rejoin, catching up, stop holds | the leader's lease, probed over `presence` | [`watch`] |
//!
//! # Wiring
//!
//! Before [`Cluster::start`]: register [`components`] in
//! [`Options::components`](super::Options::components), with the registry
//! the proxy's [`Players`] serve (`players.roster()`). After the start:
//! call [`attach`] once, before the first login is accepted.
//!
//! ```ignore
//! let players = extensions.players().clone();
//! for component in cluster::players::components(players.roster(), &options.shared) {
//!     options.components.register(component)?;
//! }
//! let cluster = Cluster::start(settings, options).await?;
//! let presence = cluster::players::attach(
//!     &cluster, players.roster(), &players, &service_states, Some(&permission_bus), true,
//! ).await?;
//! // Leader tasks consult `presence.stop_holds()` before idle stops.
//! ```
pub mod claims;
pub mod presence;
pub mod router;
pub mod watch;

#[cfg(test)]
mod tests;

use super::{Cluster, Component};
use crate::{
    config::ClusterConfig, extensions::Players, players::PlayerRegistry,
    service_state::ServiceStates,
};
use claims::{ClaimComponent, ClaimShared, RaftClaims};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex, PoisonError, Weak},
};
use tokio::{task::JoinHandle, time::Instant};

/// Claim tables made by [`components`], found again by [`attach`] through
/// the registry they were made for. Entries of registries that are gone are
/// pruned on every call.
static PREPARED: Mutex<Vec<(Weak<PlayerRegistry>, Weak<ClaimShared>)>> = Mutex::new(Vec::new());

/// The state machine components of presence, for
/// [`Options::components`](super::Options::components) before the cluster
/// starts: the `claim` entry kind with its persistent claims table, which
/// also releases claims when a `peer` entry says that a peer restarted,
/// expired or was removed. `registry` is the one the proxy's [`Players`]
/// serve; pass the same one to [`attach`]. `shared` is
/// `rift.config.cluster`: its `partition_grace` is how long servers that
/// had an expired peer's players are held back from idle stops.
pub fn components(
    registry: &Arc<PlayerRegistry>,
    shared: &ClusterConfig,
) -> Vec<Arc<dyn Component>> {
    let claims = Arc::new(ClaimShared::new(registry, shared.partition_grace));
    {
        let mut prepared = PREPARED.lock().unwrap_or_else(PoisonError::into_inner);
        prepared
            .retain(|(registry, claims)| registry.strong_count() > 0 && claims.strong_count() > 0);
        prepared.push((Arc::downgrade(registry), Arc::downgrade(&claims)));
    }
    let component: Arc<dyn Component> = Arc::new(ClaimComponent::new(claims));
    vec![component]
}

/// What [`attach`] started. Keep it for the life of the cluster.
pub struct Presence {
    shared: Arc<watch::Shared>,
}

impl Presence {
    /// Servers that held players of a peer the cluster expired, each with
    /// the moment its hold ends (`cluster.partition_grace` after the
    /// expiry). Idle stops and scale-down leave these servers alone until
    /// then, since the expired peer's players may still be on them
    /// (docs/clustering.md, "When a peer disappears"). Holds that ended are
    /// left out.
    pub fn stop_holds(&self) -> BTreeMap<String, Instant> {
        self.shared.claims.stop_holds()
    }

    /// Whether `server` is held back from idle stops; see
    /// [`Presence::stop_holds`].
    pub fn is_held(&self, server: &str) -> bool {
        self.stop_holds().contains_key(server)
    }

    /// Why this peer refuses logins now (not joined yet, catching up,
    /// fenced, ...); `None` while it admits them.
    pub fn held_back(&self) -> Option<String> {
        self.shared.held_back()
    }

    /// Stop every task [`attach`] started, before the cluster shuts down:
    /// no more proposals, attributes, service states or fencing. The
    /// registry keeps its claims handle, which then refuses every claim.
    pub fn stop(&self) {
        self.shared.stop();
    }
}

/// Make `players` (whose roster is `registry`, the one passed to
/// [`components`]) one peer of `cluster`:
///
/// - **identity**: the peer's Raft id and incarnation become the registry's
///   identity, and with `process_identity` also the process-wide one that
///   connection ids are minted from (`crate::players::identity`). Tests
///   that run several peers in one process pass `false`. A peer that is not
///   in the cluster yet takes its identity once a member adds it;
/// - **claims**: [`RaftClaims`] decide every name and UUID claim by a
///   committed `claim` entry, and the players follow the claims (a released
///   claim of another peer's player leaves the roster; a local player whose
///   claim somebody else holds now is disconnected);
///   `cluster.max_players` caps the network;
/// - **attributes and service states** travel on the `presence` stream
///   ([`presence`]); this peer asks another one for a full copy of the
///   service states once;
/// - **player actions** for other peers' players go on the `player` stream
///   ([`router`]), and requests from other peers run here; with a
///   permission bus, context reports for players another peer holds go to
///   that peer;
/// - **fencing** ([`watch`]): without the leader for half of
///   `cluster.peer_timeout` this peer refuses logins with
///   `cluster.partition_message`, after `cluster.partition_grace` it
///   disconnects its players, and back in contact it joins again and
///   reclaims its players if it was expired. While it trails the leader by
///   more than a second, or its replica diverged, it refuses logins too.
///
/// Call once per cluster start, before the first login is accepted.
pub async fn attach(
    cluster: &Cluster,
    registry: &Arc<PlayerRegistry>,
    players: &Players,
    service_states: &ServiceStates,
    permissions: Option<&Arc<crate::permissions::control::Bus>>,
    process_identity: bool,
) -> io::Result<Presence> {
    if !Arc::ptr_eq(registry, players.roster()) {
        return Err(super::invalid(
            "cluster presence: the registry must be the one the players serve",
        ));
    }
    let claims = {
        let prepared = PREPARED.lock().unwrap_or_else(PoisonError::into_inner);
        prepared
            .iter()
            .find(|(prepared, _)| {
                prepared
                    .upgrade()
                    .is_some_and(|prepared| Arc::ptr_eq(&prepared, registry))
            })
            .and_then(|(_, claims)| claims.upgrade())
    };
    let Some(claims) = claims else {
        return Err(super::invalid(
            "cluster presence: register cluster::players::components for this registry before the cluster starts",
        ));
    };
    cluster.state().read(|conn| claims.ensure_loaded(conn))?;
    registry.set_max_players(
        cluster
            .shared()
            .max_players
            .map(|cap| usize::try_from(cap).unwrap_or(usize::MAX)),
    );
    let shared = Arc::new(watch::Shared::new(
        cluster.clone(),
        registry.clone(),
        players.clone(),
        service_states.clone(),
        claims.clone(),
        process_identity,
    ));
    // A registry admits by default, and the fence task decides only from
    // its first tick on: until then, no login may claim anything with an
    // incarnation that has not joined yet.
    shared.hold_back(
        crate::players::Admission::CatchingUp,
        "attached; the admission is not decided yet",
    );
    // Before anything that mints holders or attributes.
    shared.install_identity();
    let (raft_claims, batcher) = RaftClaims::new(cluster.clone(), claims);
    registry.set_claims(Arc::new(raft_claims));
    let mut tasks: Vec<JoinHandle<()>> = vec![batcher];
    players.reconcile_claims();
    tasks.push(tokio::spawn(players.clone().follow_claims()));
    players.set_router(Arc::new(router::LinkRouter::new(cluster.clone())));
    router::serve(cluster, players);
    presence::serve(&shared);
    registry.set_sink(Some(Arc::new(presence::AttributeOut::new(&shared))));
    service_states.set_sync(Some(Arc::new(presence::StateOut::new(&shared))));
    if let Some(bus) = permissions {
        bus.route_through(players.clone());
    }
    tasks.push(tokio::spawn(presence::fetch_states(shared.clone())));
    tasks.push(tokio::spawn(presence::follow_links(shared.clone())));
    tasks.push(tokio::spawn(watch::probe(shared.clone())));
    tasks.push(tokio::spawn(watch::fence(shared.clone())));
    tasks.push(tokio::spawn(watch::follow_peers(shared.clone())));
    shared.add_tasks(tasks);
    presence::connect_all(&shared);
    Ok(Presence { shared })
}
