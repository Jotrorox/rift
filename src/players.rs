//! Process-wide player presence. A session owns its registration, so cancellation,
//! login failure and disconnect all remove the player without a separate cleanup.
//!
//! This is also the roster every reader of "who is online" goes through. A
//! player may carry a visibility rule ([`Player::visible_to`], the permission
//! node from the `rift.visible-to` meta key): Rift does not know what
//! "vanished" means, only who may see whom. Each reader names its [`Viewer`]
//! and gets the players that viewer may see.
//!
//! Presence is split by what needs consensus (see `docs/clustering.md`):
//!
//! - **Claims** ([`claims`]): which login holds which lowercase name and
//!   UUID. Uniqueness is decided by [`Claims`], at once on a single Rift
//!   ([`claims::LocalClaims`]), by a committed log entry in a cluster.
//! - **Attributes** ([`attributes`]): server, display name, skin and the
//!   rest. The peer holding a player owns them and broadcasts every change
//!   through an [`attributes::AttributeSink`]; other peers apply them with
//!   [`PlayerRegistry::apply_remote`].
//!
//! The registry serves the merged view: its own players and the players of
//! every other peer it has attributes for. Without a cluster there are no
//! other peers, no sink, and the registry behaves as it always did.
pub mod attributes;
pub mod claims;
pub mod identity;
pub mod memory;

#[cfg(test)]
mod cluster_tests;

use crate::{
    auth::ProfileProperty,
    protocol::{TabEntry, TabName},
    text::{Text, catalog::Notice},
};
use attributes::{
    ApplyOutcome, AttributeChange, AttributeSink, AttributeUpdate, Attributes, UpdateBody,
};
use claims::{ClaimChange, ClaimFuture, ClaimRecord, Claims, Holder, LocalClaims, Refusal};
use identity::Identity;
use serde_json::Value as Json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, io,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, RwLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    pub uuid: [u8; 16],
    pub name: String,
    pub server: String,
    /// Signed profile properties (the skin) of an authenticated player.
    pub properties: Arc<[ProfileProperty]>,
    /// Only viewers holding this permission node see the player; `None` is
    /// everyone. Resolved from the player's `rift.visible-to` meta.
    pub visible_to: Option<Arc<str>>,
    /// The name with the player's permission prefix and suffix, as other
    /// servers' network tab lists show it; `None` without either.
    pub display: Option<Arc<Json>>,
    /// The weight of the player's primary group when `display` is set, for
    /// the tab list order.
    pub order: i32,
    /// The peer the player is connected to: 0 on a single Rift.
    pub peer: u16,
    /// The session's connection id; 0 for a registration made without one.
    pub connection: u64,
    /// The address the player connected from, without the port; empty until
    /// the session records it.
    pub ip: Arc<str>,
    /// The port the player connected from; 0 until the session records it.
    pub port: u16,
    /// The client's protocol version, once the player joined.
    pub protocol: i32,
    /// The client's language as it reported it.
    pub locale: Option<Arc<str>>,
    /// The language Rift's messages use for the player.
    pub language: Option<Arc<str>>,
    /// When the player registered, in milliseconds since the Unix epoch.
    pub since: u64,
}

impl Player {
    /// The address the player connected from, once the session recorded it.
    pub fn address(&self) -> Option<SocketAddr> {
        let ip: IpAddr = self.ip.parse().ok()?;
        (self.port != 0).then(|| SocketAddr::new(ip, self.port))
    }
}

/// The meta key whose value is the node a viewer needs to see a player.
pub const VISIBLE_TO: &str = "rift.visible-to";

/// The permission node that lets a player log in past `cluster.max_players`.
pub const MAX_PLAYERS_BYPASS: &str = "rift.max-players.bypass";

/// Who is looking at the roster.
///
/// | Viewer | Sees | Readers |
/// | --- | --- | --- |
/// | `Player` | everyone unrestricted, themselves, and players whose node they hold | the network tab list, `{online}`/`{server_online}`, name completion |
/// | `Public` | only unrestricted players | the server-list count, the status page and badge |
/// | `Backend` | only unrestricted players | BungeeCord `PlayerCount`, `PlayerList`, `RiftServers` |
/// | `System` | everyone | capacity, queues, scaling, idle stops, metrics, admin status, kick and transfer by name, `GetPlayerServer` |
///
/// System readers must see hidden players: idle shutdown counting only
/// visible players would stop the server a hidden administrator stands on.
#[derive(Clone, Copy)]
pub enum Viewer<'a> {
    Player {
        uuid: [u8; 16],
        /// Whether the viewer holds a permission node in their contexts.
        holds: &'a dyn Fn(&str) -> bool,
    },
    Public,
    Backend,
    System,
}

impl Viewer<'_> {
    /// Whether this viewer sees a player with `visible_to` and `uuid`.
    pub fn sees(&self, uuid: &[u8; 16], visible_to: Option<&str>) -> bool {
        match (self, visible_to) {
            (_, None) | (Self::System, _) => true,
            (Self::Public | Self::Backend, Some(_)) => false,
            (Self::Player { uuid: own, holds }, Some(node)) => own == uuid || holds(node),
        }
    }

    fn sees_player(&self, player: &Player) -> bool {
        self.sees(&player.uuid, player.visible_to.as_deref())
    }
}

/// Whether this peer accepts new logins. A cluster peer that lost the
/// leader fences itself; one that is catching up with the log waits until
/// its view of the network is current.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Admission {
    #[default]
    Open,
    /// Refuse logins with `message`, or `cluster.partition_message` for
    /// `None`.
    Fenced { message: Option<Text> },
    /// Refuse logins with `login.unavailable` until caught up.
    CatchingUp,
}

/// A player of another peer, with the incarnation of that peer an action
/// for them is addressed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlayer {
    pub player: Player,
    pub incarnation: u64,
}

pub struct PlayerRegistry {
    state: Mutex<RegistryState>,
    /// Changes on every join, server change and disconnect, here or on
    /// another peer, so that tab lists can skip rebuilding while the network
    /// is unchanged. Sessions compare it on their tab list tick (twice a
    /// second), which is what limits rebuilds during a burst of joins: many
    /// changes between two ticks cost one rebuild.
    generation: AtomicU64,
    claims: RwLock<Arc<dyn Claims>>,
    admission: RwLock<Admission>,
    /// `cluster.max_players`; `usize::MAX` is no cap.
    max_players: AtomicUsize,
}

#[derive(Default)]
struct RegistryState {
    /// This peer's players.
    players: BTreeMap<[u8; 16], Player>,
    /// The claim behind each of this peer's players, by lowercase name.
    holders: BTreeMap<String, ([u8; 16], Holder)>,
    /// This peer's players whose claim somebody else holds now: their
    /// sessions were told to end, and are not told again.
    lost: BTreeSet<[u8; 16]>,
    /// Other peers' players, from their attributes.
    remote: BTreeMap<[u8; 16], Player>,
    /// Where each other peer's attribute stream stands.
    peers: BTreeMap<u16, PeerStream>,
    /// Set only by the in-memory cluster, which runs several peers in one
    /// process; otherwise the process-wide identity applies.
    identity: Option<Identity>,
    sink: Option<Arc<dyn AttributeSink>>,
    /// The sequence number of the last update sent to `sink`.
    sequence: u64,
}

#[derive(Debug, Clone, Copy)]
struct PeerStream {
    incarnation: u64,
    sequence: u64,
    /// A full resync arrived since the incarnation started.
    synced: bool,
}

impl RegistryState {
    fn identity(&self) -> Identity {
        self.identity.unwrap_or_else(identity::identity)
    }

    fn all(&self) -> impl Iterator<Item = &Player> {
        self.players.values().chain(self.remote.values())
    }

    fn remote_player(&self, player: &Player) -> RemotePlayer {
        RemotePlayer {
            player: player.clone(),
            incarnation: self
                .peers
                .get(&player.peer)
                .map_or(0, |stream| stream.incarnation),
        }
    }

    /// Send one change to the sink, if there is one; the change is built
    /// only then.
    fn publish(&mut self, change: impl FnOnce(&Self) -> Option<AttributeChange>) {
        let Some(sink) = self.sink.clone() else {
            return;
        };
        let Some(change) = change(self) else {
            return;
        };
        self.sequence += 1;
        let identity = self.identity();
        sink.send(AttributeUpdate {
            peer: identity.peer,
            incarnation: identity.incarnation,
            sequence: self.sequence,
            body: UpdateBody::Changes(vec![change]),
        });
    }

    fn publish_player(&mut self, uuid: &[u8; 16]) {
        self.publish(|state| {
            state
                .players
                .get(uuid)
                .map(|player| AttributeChange::Upsert(Attributes::of(player)))
        });
    }

    fn publish_removal(&mut self, uuid: [u8; 16]) {
        self.publish(|_| Some(AttributeChange::Remove(uuid)));
    }

    /// Forget every player of `peer`; whether there were any.
    fn remove_peer_players(&mut self, peer: u16) -> bool {
        let before = self.remote.len();
        self.remote.retain(|_, player| player.peer != peer);
        before != self.remote.len()
    }

    /// Local players whose claim `record` now contradicts and who were not
    /// reported before; they are reported once.
    fn conflicts(&mut self, record: &ClaimRecord) -> Vec<[u8; 16]> {
        let mut conflicts = Vec::new();
        if let Some((uuid, holder)) = self.holders.get(&record.key)
            && *holder != record.holder
        {
            conflicts.push(*uuid);
        }
        if let Some(uuid) = record.uuid
            && let Some(player) = self.players.get(&uuid)
            && let Some((_, holder)) = self.holders.get(&player.name.to_ascii_lowercase())
            && *holder != record.holder
            && !conflicts.contains(&uuid)
        {
            conflicts.push(uuid);
        }
        conflicts.retain(|uuid| self.lost.insert(*uuid));
        conflicts
    }
}

impl Default for PlayerRegistry {
    fn default() -> Self {
        Self::with_claims(Arc::new(LocalClaims::default()))
    }
}

impl fmt::Debug for PlayerRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("PlayerRegistry");
        if let Ok(state) = self.state.try_lock() {
            debug
                .field("players", &state.players.len())
                .field("remote", &state.remote.len());
        }
        debug
            .field("generation", &self.generation())
            .finish_non_exhaustive()
    }
}

impl PlayerRegistry {
    /// A registry whose claims `claims` decides.
    pub fn with_claims(claims: Arc<dyn Claims>) -> Self {
        Self {
            state: Mutex::default(),
            generation: AtomicU64::new(0),
            claims: RwLock::new(claims),
            admission: RwLock::default(),
            max_players: AtomicUsize::new(usize::MAX),
        }
    }

    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Decide claims with `claims` from now on. Install it before the first
    /// login: claims held under the previous one stay there.
    pub fn set_claims(&self, claims: Arc<dyn Claims>) {
        *self.claims.write().unwrap_or_else(PoisonError::into_inner) = claims;
    }

    /// Who decides claims.
    pub fn claims(&self) -> Arc<dyn Claims> {
        self.claims
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The peer and incarnation this registry's players belong to.
    pub fn identity(&self) -> Identity {
        self.lock().identity()
    }

    /// Run as `identity` instead of the process-wide one; for simulating
    /// several peers in one process (`src/players/memory.rs`).
    pub fn assume_identity(&self, identity: Identity) {
        self.lock().identity = Some(identity);
    }

    /// Send changes to this peer's players to `sink`; `None` stops it.
    pub fn set_sink(&self, sink: Option<Arc<dyn AttributeSink>>) {
        self.lock().sink = sink;
    }

    /// `cluster.max_players`: claims beyond it fail unless they carry a
    /// bypass. `None` is no cap.
    pub fn set_max_players(&self, cap: Option<usize>) {
        self.max_players
            .store(cap.unwrap_or(usize::MAX), Ordering::Relaxed);
    }

    pub fn max_players(&self) -> Option<usize> {
        let cap = self.max_players.load(Ordering::Relaxed);
        (cap != usize::MAX).then_some(cap)
    }

    /// Whether new logins are accepted.
    pub fn admission(&self) -> Admission {
        self.admission
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Open, fence or hold back logins; consulted before every claim.
    pub fn set_admission(&self, admission: Admission) {
        *self
            .admission
            .write()
            .unwrap_or_else(PoisonError::into_inner) = admission;
    }

    fn admit(&self) -> io::Result<()> {
        match &*self
            .admission
            .read()
            .unwrap_or_else(PoisonError::into_inner)
        {
            Admission::Open => Ok(()),
            Admission::Fenced { message } => Err(crate::text::error(
                io::ErrorKind::NotConnected,
                message
                    .clone()
                    .unwrap_or_else(|| crate::text::catalog::notice(Notice::ClusterPartition)),
            )),
            Admission::CatchingUp => Err(crate::text::notice_error(
                io::ErrorKind::NotConnected,
                Notice::LoginUnavailable,
            )),
        }
    }

    fn name_claim(
        &self,
        name: String,
        connection: u64,
    ) -> io::Result<(String, String, Holder, ClaimFuture<Result<(), Refusal>>)> {
        self.admit()?;
        let name = checked_name(name)?;
        let key = name.to_ascii_lowercase();
        let holder = Holder::new(self.identity(), connection);
        let claimed = self.claims().claim_name(key.clone(), name.clone(), holder);
        Ok((name, key, holder, claimed))
    }

    fn player_claim(
        &self,
        key: String,
        name: String,
        uuid: [u8; 16],
        holder: Holder,
        upgrade: bool,
        bypass_cap: bool,
    ) -> ClaimFuture<Result<(), Refusal>> {
        self.claims().claim_player(
            key,
            name,
            uuid,
            holder,
            upgrade,
            bypass_cap,
            self.max_players(),
        )
    }

    /// Claim a case-insensitive name before contacting a backend, which
    /// might otherwise evict an existing player before replying to a
    /// duplicate login. Pending logins are not exposed as connected players.
    /// In a cluster this waits until the claim is committed.
    pub async fn claim_name(
        self: &Arc<Self>,
        name: &str,
        connection: u64,
    ) -> io::Result<NameReservation> {
        let (name, key, holder, claimed) = self.name_claim(name.to_owned(), connection)?;
        claimed.await.map_err(refused)?;
        Ok(NameReservation {
            registry: self.clone(),
            name,
            key,
            holder,
            pending: true,
        })
    }

    /// [`PlayerRegistry::claim_name`] for claims that are decided at once,
    /// as on a single Rift.
    pub fn reserve_name(self: &Arc<Self>, name: impl Into<String>) -> io::Result<NameReservation> {
        let (name, key, holder, claimed) = self.name_claim(name.into(), 0)?;
        decided(claimed)?.map_err(refused)?;
        Ok(NameReservation {
            registry: self.clone(),
            name,
            key,
            holder,
            pending: true,
        })
    }

    /// Claim both the UUID and the case-insensitive name of a confirmed
    /// identity and register the player, with their visibility rule so no
    /// reader ever sees them without it. `bypass_cap` lets the player past
    /// `cluster.max_players`. In a cluster this waits for the commit.
    pub async fn claim_player(
        self: &Arc<Self>,
        uuid: [u8; 16],
        name: &str,
        server: &str,
        visible_to: Option<Arc<str>>,
        connection: u64,
        bypass_cap: bool,
    ) -> io::Result<PlayerRegistration> {
        self.admit()?;
        let name = checked_name(name.to_owned())?;
        let key = name.to_ascii_lowercase();
        let holder = Holder::new(self.identity(), connection);
        let claimed = self.player_claim(key.clone(), name.clone(), uuid, holder, false, bypass_cap);
        claimed.await.map_err(refused)?;
        Ok(self.insert_local(uuid, name, key, holder, server.to_owned(), visible_to))
    }

    /// Atomically reserve both the UUID and case-insensitive name. The registry
    /// must be shared across configuration reloads and all listening sockets.
    pub fn register(
        self: &Arc<Self>,
        uuid: [u8; 16],
        name: impl Into<String>,
        server: impl Into<String>,
    ) -> io::Result<PlayerRegistration> {
        self.register_visible(uuid, name, server, None)
    }

    /// [`PlayerRegistry::register`] with the player's visibility rule, so no
    /// reader ever sees them without it. For claims that are decided at
    /// once; see [`PlayerRegistry::claim_player`].
    pub fn register_visible(
        self: &Arc<Self>,
        uuid: [u8; 16],
        name: impl Into<String>,
        server: impl Into<String>,
        visible_to: Option<Arc<str>>,
    ) -> io::Result<PlayerRegistration> {
        self.admit()?;
        let name = checked_name(name.into())?;
        let key = name.to_ascii_lowercase();
        let holder = Holder::new(self.identity(), 0);
        decided(self.player_claim(key.clone(), name.clone(), uuid, holder, false, false))?
            .map_err(refused)?;
        Ok(self.insert_local(uuid, name, key, holder, server.into(), visible_to))
    }

    /// Put a player whose claim succeeded on the roster.
    fn insert_local(
        self: &Arc<Self>,
        uuid: [u8; 16],
        name: String,
        key: String,
        holder: Holder,
        server: String,
        visible_to: Option<Arc<str>>,
    ) -> PlayerRegistration {
        let mut state = self.lock();
        // A record another peer has not withdrawn yet: the claim says the
        // player is here now.
        state.remote.remove(&uuid);
        state.players.insert(
            uuid,
            Player {
                uuid,
                name,
                server,
                properties: Arc::default(),
                visible_to,
                display: None,
                order: 0,
                peer: holder.peer,
                connection: holder.connection,
                ip: Arc::from(""),
                port: 0,
                protocol: 0,
                locale: None,
                language: None,
                since: unix_millis(),
            },
        );
        state.holders.insert(key.clone(), (uuid, holder));
        self.generation.fetch_add(1, Ordering::Relaxed);
        state.publish_player(&uuid);
        drop(state);
        PlayerRegistration {
            registry: self.clone(),
            uuid,
            key,
            holder,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Force every session to rebuild its view, for a change that alters
    /// what a viewer may see without touching a record: a viewer's own
    /// permissions.
    pub fn touch(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Change one of this peer's players with `change`, which says whether
    /// it changed anything; a change is announced to other peers, and with
    /// `shown` it also makes sessions rebuild their tab lists.
    fn update_local(
        &self,
        uuid: &[u8; 16],
        shown: bool,
        change: impl FnOnce(&mut Player) -> bool,
    ) -> bool {
        let mut state = self.lock();
        let changed = state.players.get_mut(uuid).is_some_and(change);
        if changed {
            if shown {
                self.generation.fetch_add(1, Ordering::Relaxed);
            }
            state.publish_player(uuid);
        }
        changed
    }

    /// Set a connected player's visibility rule; returns whether it changed.
    pub fn set_visibility(&self, uuid: &[u8; 16], visible_to: Option<Arc<str>>) -> bool {
        self.update_local(uuid, true, |player| {
            if player.visible_to == visible_to {
                return false;
            }
            player.visible_to = visible_to;
            true
        })
    }

    /// Set a connected player's tab list display name and order; returns
    /// whether either changed.
    pub fn set_display(&self, uuid: &[u8; 16], display: Option<Arc<Json>>, order: i32) -> bool {
        self.update_local(uuid, true, |player| {
            if player.display == display && player.order == order {
                return false;
            }
            player.display = display;
            player.order = order;
            true
        })
    }

    /// Record what the joined client reported: its protocol, locale and the
    /// language Rift's messages use for it. Tab lists do not show these.
    pub fn set_client(
        &self,
        uuid: &[u8; 16],
        protocol: i32,
        locale: Option<&str>,
        language: Option<&str>,
    ) -> bool {
        self.update_local(uuid, false, |player| {
            if player.protocol == protocol
                && player.locale.as_deref() == locale
                && player.language.as_deref() == language
            {
                return false;
            }
            player.protocol = protocol;
            player.locale = locale.map(Arc::from);
            player.language = language.map(Arc::from);
            true
        })
    }

    /// Tab list entries for every connected player who is not on `server`
    /// and whom `viewer` may see.
    pub fn tab_entries(&self, server: &str, viewer: Viewer<'_>) -> Vec<TabEntry> {
        self.lock()
            .all()
            .filter(|player| player.server != server && viewer.sees_player(player))
            .map(|player| TabEntry {
                uuid: player.uuid,
                name: player.name.clone(),
                properties: player.properties.clone(),
                display: player.display.clone(),
                order: player.order,
            })
            .collect()
    }

    /// The display name and list order of every connected player who has
    /// one, for the entries backends list themselves. Visibility does not
    /// apply: these only name entries the backend already shows.
    pub fn tab_names(&self) -> BTreeMap<[u8; 16], TabName> {
        self.lock()
            .all()
            .filter(|player| player.display.is_some() || player.order != 0)
            .map(|player| {
                (
                    player.uuid,
                    TabName {
                        display: player.display.clone(),
                        order: player.order,
                    },
                )
            })
            .collect()
    }

    /// Every connected player; the [`Viewer::System`] view.
    pub fn snapshot(&self) -> Vec<Player> {
        self.snapshot_for(Viewer::System)
    }

    /// The connected players `viewer` may see, by UUID.
    pub fn snapshot_for(&self, viewer: Viewer<'_>) -> Vec<Player> {
        let state = self.lock();
        let mut players: Vec<Player> = state
            .all()
            .filter(|player| viewer.sees_player(player))
            .cloned()
            .collect();
        if !state.remote.is_empty() {
            players.sort_unstable_by_key(|player| player.uuid);
        }
        players
    }

    /// How many connected players `viewer` may see, on `server` or anywhere.
    pub fn count_for(&self, viewer: Viewer<'_>, server: Option<&str>) -> usize {
        self.lock()
            .all()
            .filter(|player| server.is_none_or(|server| player.server == server))
            .filter(|player| viewer.sees_player(player))
            .count()
    }

    /// [`PlayerRegistry::server_counts`] as `viewer` sees it.
    pub fn server_counts_for(&self, viewer: Viewer<'_>) -> BTreeMap<String, usize> {
        let state = self.lock();
        let mut counts = BTreeMap::new();
        for player in state.all().filter(|player| viewer.sees_player(player)) {
            *counts.entry(player.server.clone()).or_default() += 1;
        }
        counts
    }

    /// Count connected players from one consistent view of the registry.
    /// Clone only distinct server names, not player records or pending logins.
    /// The [`Viewer::System`] view: capacity and scaling count everyone.
    pub(crate) fn server_counts(&self) -> BTreeMap<String, usize> {
        let state = self.lock();
        let mut counts = BTreeMap::new();
        for player in state.all() {
            if let Some(count) = counts.get_mut(&player.server) {
                *count += 1;
            } else {
                counts.insert(player.server.clone(), 1);
            }
        }
        counts
    }

    /// Check one backend's occupancy without cloning player records. The
    /// [`Viewer::System`] view: a hidden player keeps their server running.
    pub(crate) fn count_on_server(&self, server: &str) -> usize {
        self.lock()
            .all()
            .filter(|player| player.server == server)
            .count()
    }

    pub fn get(&self, uuid: &[u8; 16]) -> Option<Player> {
        let state = self.lock();
        state
            .players
            .get(uuid)
            .or_else(|| state.remote.get(uuid))
            .cloned()
    }

    pub fn len(&self) -> usize {
        let state = self.lock();
        state.players.len() + state.remote.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A player of another peer by UUID.
    pub fn remote_by_uuid(&self, uuid: &[u8; 16]) -> Option<RemotePlayer> {
        let state = self.lock();
        state
            .remote
            .get(uuid)
            .map(|player| state.remote_player(player))
    }

    /// A player of another peer by case-insensitive name.
    pub fn remote_by_name(&self, name: &str) -> Option<RemotePlayer> {
        let state = self.lock();
        state
            .remote
            .values()
            .find(|player| player.name.eq_ignore_ascii_case(name))
            .map(|player| state.remote_player(player))
    }

    /// A player of another peer by connection id.
    pub fn remote_by_connection(&self, connection: u64) -> Option<RemotePlayer> {
        let state = self.lock();
        state
            .remote
            .values()
            .find(|player| player.connection == connection)
            .map(|player| state.remote_player(player))
    }

    /// Every player of other peers.
    pub fn remote_players(&self) -> Vec<RemotePlayer> {
        let state = self.lock();
        state
            .remote
            .values()
            .map(|player| state.remote_player(player))
            .collect()
    }

    /// The incarnation of every peer this registry has attributes from.
    pub fn peer_incarnations(&self) -> BTreeMap<u16, u64> {
        self.lock()
            .peers
            .iter()
            .map(|(peer, stream)| (*peer, stream.incarnation))
            .collect()
    }

    /// The claims behind this peer's players, to claim them again after the
    /// peer lost contact with the cluster; players already lost to another
    /// login are left out.
    pub fn local_claims(&self) -> Vec<ClaimRecord> {
        let state = self.lock();
        state
            .holders
            .iter()
            .filter(|(_, (uuid, _))| !state.lost.contains(uuid))
            .filter_map(|(key, (uuid, holder))| {
                Some(ClaimRecord {
                    key: key.clone(),
                    name: state.players.get(uuid)?.name.clone(),
                    uuid: Some(*uuid),
                    holder: *holder,
                })
            })
            .collect()
    }

    /// Every one of this peer's players, for a peer that (re)connects or
    /// missed an update: later changes continue from its sequence number.
    pub fn full_resync(&self) -> AttributeUpdate {
        let state = self.lock();
        let identity = state.identity();
        AttributeUpdate {
            peer: identity.peer,
            incarnation: identity.incarnation,
            sequence: state.sequence,
            body: UpdateBody::Resync(state.players.values().map(Attributes::of).collect()),
        }
    }

    /// Apply an update from another peer's attribute stream.
    pub fn apply_remote(&self, update: AttributeUpdate) -> ApplyOutcome {
        let mut guard = self.lock();
        let state = &mut *guard;
        let peer = update.peer;
        if peer == state.identity().peer {
            return ApplyOutcome::Ignored;
        }
        let mut changed = false;
        let mut stream = match state.peers.get(&peer) {
            Some(stream) if update.incarnation < stream.incarnation => {
                return ApplyOutcome::Ignored;
            }
            Some(stream) if update.incarnation == stream.incarnation => *stream,
            // A peer seen for the first time, or one that started again:
            // what it had before is gone.
            _ => {
                changed |= state.remove_peer_players(peer);
                PeerStream {
                    incarnation: update.incarnation,
                    sequence: 0,
                    synced: false,
                }
            }
        };
        let outcome = match update.body {
            UpdateBody::Resync(players) => {
                changed |= state.remove_peer_players(peer);
                for attributes in players {
                    if state.players.contains_key(&attributes.uuid) {
                        continue;
                    }
                    state
                        .remote
                        .insert(attributes.uuid, attributes.into_player(peer));
                    changed = true;
                }
                stream.sequence = update.sequence;
                stream.synced = true;
                ApplyOutcome::Applied
            }
            UpdateBody::Changes(_) if !stream.synced || update.sequence != stream.sequence + 1 => {
                ApplyOutcome::NeedResync
            }
            UpdateBody::Changes(changes) => {
                for change in changes {
                    match change {
                        AttributeChange::Upsert(attributes) => {
                            if !state.players.contains_key(&attributes.uuid) {
                                state
                                    .remote
                                    .insert(attributes.uuid, attributes.into_player(peer));
                                changed = true;
                            }
                        }
                        AttributeChange::Remove(uuid) => {
                            if state
                                .remote
                                .get(&uuid)
                                .is_some_and(|player| player.peer == peer)
                            {
                                state.remote.remove(&uuid);
                                changed = true;
                            }
                        }
                    }
                }
                stream.sequence = update.sequence;
                ApplyOutcome::Applied
            }
        };
        state.peers.insert(peer, stream);
        if changed {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        outcome
    }

    /// Record that this peer's player `uuid` lost their claim to another
    /// login; false when that was known.
    pub fn lose(&self, uuid: &[u8; 16]) -> bool {
        let mut state = self.lock();
        state.players.contains_key(uuid) && state.lost.insert(*uuid)
    }

    /// Forget another peer's players and where its stream stood.
    pub fn drop_peer(&self, peer: u16) {
        let mut state = self.lock();
        state.peers.remove(&peer);
        if state.remove_peer_players(peer) {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Follow one change of the claims: another peer's player whose claim
    /// was released leaves the view. Returns this peer's players whose
    /// claims are now held by somebody else; their sessions must end.
    pub fn apply_claim_change(&self, change: &ClaimChange) -> Vec<[u8; 16]> {
        let mut state = self.lock();
        match change {
            ClaimChange::Released(record) => {
                if let Some(uuid) = record.uuid
                    && state.remote.get(&uuid).is_some_and(|player| {
                        player.peer == record.holder.peer
                            && player.connection == record.holder.connection
                    })
                {
                    state.remote.remove(&uuid);
                    self.generation.fetch_add(1, Ordering::Relaxed);
                }
                Vec::new()
            }
            ClaimChange::Claimed(record) => state.conflicts(record),
        }
    }

    /// Compare everything with the live claims, after missing changes:
    /// other peers' players without a claim leave the view, and the
    /// returned players of this peer have lost theirs to somebody else.
    pub fn reconcile(&self, live: &[ClaimRecord]) -> Vec<[u8; 16]> {
        let by_uuid: BTreeMap<[u8; 16], &ClaimRecord> = live
            .iter()
            .filter_map(|record| Some((record.uuid?, record)))
            .collect();
        let mut state = self.lock();
        let before = state.remote.len();
        state.remote.retain(|uuid, player| {
            by_uuid.get(uuid).is_some_and(|record| {
                record.holder.peer == player.peer && record.holder.connection == player.connection
            })
        });
        if before != state.remote.len() {
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
        let mut conflicts = Vec::new();
        for record in live {
            for uuid in state.conflicts(record) {
                if !conflicts.contains(&uuid) {
                    conflicts.push(uuid);
                }
            }
        }
        conflicts
    }
}

/// A pending login's exclusive name claim. Cancellation releases it, while
/// successful backend login converts it into a full player registration.
#[derive(Debug)]
pub struct NameReservation {
    registry: Arc<PlayerRegistry>,
    name: String,
    key: String,
    holder: Holder,
    pending: bool,
}

impl NameReservation {
    pub fn register(
        self,
        uuid: [u8; 16],
        server: impl Into<String>,
    ) -> io::Result<PlayerRegistration> {
        self.register_visible(uuid, server, None)
    }

    /// [`NameReservation::register`] with the player's visibility rule, for
    /// claims that are decided at once.
    pub fn register_visible(
        self,
        uuid: [u8; 16],
        server: impl Into<String>,
        visible_to: Option<Arc<str>>,
    ) -> io::Result<PlayerRegistration> {
        self.registry.admit()?;
        let claimed = self.registry.player_claim(
            self.key.clone(),
            self.name.clone(),
            uuid,
            self.holder,
            true,
            false,
        );
        // On failure, dropping `self` releases the pending name claim.
        decided(claimed)?.map_err(refused)?;
        Ok(self.complete(uuid, server.into(), visible_to))
    }

    /// Turn the name claim into a claim of the player's UUID and name and
    /// register the player; see [`PlayerRegistry::claim_player`]. The name
    /// is never free in between.
    pub async fn claim_player(
        self,
        uuid: [u8; 16],
        server: &str,
        visible_to: Option<Arc<str>>,
        bypass_cap: bool,
    ) -> io::Result<PlayerRegistration> {
        self.registry.admit()?;
        let claimed = self.registry.player_claim(
            self.key.clone(),
            self.name.clone(),
            uuid,
            self.holder,
            true,
            bypass_cap,
        );
        claimed.await.map_err(refused)?;
        Ok(self.complete(uuid, server.to_owned(), visible_to))
    }

    fn complete(
        mut self,
        uuid: [u8; 16],
        server: String,
        visible_to: Option<Arc<str>>,
    ) -> PlayerRegistration {
        // The claim now belongs to the registration.
        self.pending = false;
        let registry = self.registry.clone();
        registry.insert_local(
            uuid,
            std::mem::take(&mut self.name),
            std::mem::take(&mut self.key),
            self.holder,
            server,
            visible_to,
        )
    }
}

impl Drop for NameReservation {
    fn drop(&mut self) {
        if self.pending {
            self.registry
                .claims()
                .release(std::mem::take(&mut self.key), self.holder);
        }
    }
}

/// A unique session reservation. Intentionally not cloneable.
#[derive(Debug)]
pub struct PlayerRegistration {
    registry: Arc<PlayerRegistry>,
    uuid: [u8; 16],
    key: String,
    holder: Holder,
}

impl PlayerRegistration {
    /// Commit presence only after the replacement server has accepted the player.
    pub fn set_server(&self, server: &str) {
        self.registry.update_local(&self.uuid, true, |player| {
            player.server = server.to_owned();
            true
        });
    }

    /// Record the signed profile properties of an authenticated player.
    pub fn set_properties(&self, properties: &[ProfileProperty]) {
        self.registry.update_local(&self.uuid, true, |player| {
            player.properties = properties.into();
            true
        });
    }

    /// Record the address the player connected from, for other peers'
    /// BungeeCord `IPOther`.
    pub fn set_address(&self, address: SocketAddr) {
        let ip = address.ip().to_canonical().to_string();
        self.registry.update_local(&self.uuid, false, |player| {
            if *player.ip == *ip && player.port == address.port() {
                return false;
            }
            player.ip = Arc::from(ip);
            player.port = address.port();
            true
        });
    }

    /// The connection id the claim was made for.
    pub fn connection(&self) -> u64 {
        self.holder.connection
    }
}

impl Drop for PlayerRegistration {
    fn drop(&mut self) {
        {
            let mut state = self.registry.lock();
            if state
                .holders
                .get(&self.key)
                .is_some_and(|(uuid, holder)| *uuid == self.uuid && *holder == self.holder)
            {
                state.holders.remove(&self.key);
                state.lost.remove(&self.uuid);
                if state.players.remove(&self.uuid).is_some() {
                    self.registry.generation.fetch_add(1, Ordering::Relaxed);
                    state.publish_removal(self.uuid);
                }
            }
        }
        // After the record is gone, so no reader sees a player whose name a
        // new login could already claim.
        self.registry
            .claims()
            .release(std::mem::take(&mut self.key), self.holder);
    }
}

/// The value of an already decided claim future.
fn decided<T>(future: ClaimFuture<T>) -> io::Result<T> {
    claims::ready_now(future).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "this player claim waits for the cluster; use the asynchronous registration",
        )
    })
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn checked_name(name: String) -> io::Result<String> {
    if valid_username(&name) {
        Ok(name)
    } else {
        Err(crate::text::notice_error(
            io::ErrorKind::InvalidInput,
            Notice::LoginInvalidName,
        ))
    }
}

fn already_connected() -> io::Error {
    crate::text::notice_error(io::ErrorKind::AlreadyExists, Notice::LoginDuplicateName)
}

/// The error a refused claim gives the login. Both kinds of duplicate keep
/// the error a single Rift always gave (`AlreadyExists`).
fn refused(refusal: Refusal) -> io::Error {
    match refusal {
        Refusal::AlreadyConnected | Refusal::DuplicateName => already_connected(),
        Refusal::NetworkFull => {
            crate::text::notice_error(io::ErrorKind::QuotaExceeded, Notice::ClusterMaxPlayers)
        }
        Refusal::Fenced => {
            crate::text::notice_error(io::ErrorKind::NotConnected, Notice::ClusterPartition)
        }
        Refusal::Unavailable(reason) => {
            log_warn!("a player claim could not be decided: {reason}");
            crate::text::notice_error(io::ErrorKind::TimedOut, Notice::LoginUnavailable)
        }
    }
}

/// The UUID an offline-mode server gives `name`: a version 3 UUID of the MD5
/// of `OfflinePlayer:<name>`, as vanilla and Bukkit compute it. Pumpkin
/// derives its own (the first 16 bytes of SHA-256 of the name); sessions then
/// use the UUID the backend confirms at login.
pub fn offline_uuid(name: &str) -> [u8; 16] {
    let mut digest = md5(format!("OfflinePlayer:{name}").as_bytes());
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    digest
}

/// RFC 1321 MD5, only for offline UUIDs; never for security.
fn md5(input: &[u8]) -> [u8; 16] {
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let table: [u32; 64] =
        std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32);
    let mut message = input.to_vec();
    let bits = (input.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_le_bytes());
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    for chunk in message.as_chunks::<64>().0 {
        let words: [u32; 16] = std::array::from_fn(|i| {
            u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().expect("four bytes"))
        });
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(table[i])
                .wrapping_add(words[g])
                .rotate_left(SHIFTS[i]);
            (a, b, c, d) = (d, b.wrapping_add(rotated), b, c);
        }
        for (word, value) in state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(value);
        }
    }
    let mut digest = [0; 16];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    digest
}

pub(crate) fn valid_username(name: &str) -> bool {
    (1..=16).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the roster shows of a player, without the join time.
    fn shown(player: &Player) -> (&[u8; 16], &str, &str, Option<&str>, bool, i32) {
        (
            &player.uuid,
            player.name.as_str(),
            player.server.as_str(),
            player.visible_to.as_deref(),
            player.display.is_some(),
            player.order,
        )
    }

    #[test]
    fn offline_uuids_match_bukkit() {
        assert_eq!(
            md5(b""),
            [
                0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
                0x42, 0x7e
            ]
        );
        // python3 -c 'import uuid,hashlib;print(uuid.UUID(bytes=hashlib.md5(b"OfflinePlayer:Notch").digest(),version=3).hex)'
        assert_eq!(
            rift_permissions::Uuid::from_bytes(offline_uuid("Notch")).to_string(),
            "b50ad385829d3141a2167e7d7539ba7f"
        );
    }

    #[test]
    fn presence_tracks_switches_and_cleans_up_on_drop() {
        let registry = Arc::new(PlayerRegistry::default());
        let registration = registry.register([1; 16], "Alice", "lobby").unwrap();
        assert_eq!(registry.len(), 1);
        let before = registry.snapshot();
        registration.set_server("survival");
        assert_eq!(before[0].server, "lobby");
        assert_eq!(registry.get(&[1; 16]).unwrap().server, "survival");
        drop(registration);
        assert!(registry.is_empty());
        assert!(registry.register([1; 16], "Alice", "lobby").is_ok());
    }

    #[test]
    fn uuid_and_names_are_independently_unique() {
        let registry = Arc::new(PlayerRegistry::default());
        let original = registry.register([1; 16], "Alice", "lobby").unwrap();
        for (uuid, name) in [([1; 16], "Bob"), ([2; 16], "aLiCe")] {
            assert_eq!(
                registry
                    .register(uuid, name, "survival")
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::AlreadyExists
            );
        }
        assert_eq!(registry.snapshot()[0].name, "Alice");
        drop(original);
        assert!(registry.register([2; 16], "alice", "survival").is_ok());
    }

    #[test]
    fn simultaneous_registrations_cannot_claim_one_name() {
        let registry = Arc::new(PlayerRegistry::default());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let attempts: Vec<_> = (0..2)
            .map(|index| {
                let registry = registry.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    registry.register([index; 16], "Alice", "lobby")
                })
            })
            .collect();
        let registrations: Vec<_> = attempts
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            registrations.iter().filter(|entry| entry.is_ok()).count(),
            1
        );
        assert_eq!(registry.len(), 1);
        drop(registrations);
        assert!(registry.is_empty());
    }

    #[test]
    fn player_names_are_bounded_and_cannot_contain_command_syntax() {
        let registry = Arc::new(PlayerRegistry::default());
        for name in [
            "",
            "too_long_username",
            "Alice Bob",
            "*",
            "Alice\n",
            "Ålice",
        ] {
            assert_eq!(
                registry
                    .register([1; 16], name, "lobby")
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(registry.register([1; 16], "Alice_123", "lobby").is_ok());
    }

    #[test]
    fn pending_names_are_exclusive_but_invisible_and_cancel_safely() {
        let registry = Arc::new(PlayerRegistry::default());
        let pending = registry.reserve_name("Alice").unwrap();
        assert!(registry.is_empty());
        assert!(registry.snapshot().is_empty());
        assert_eq!(
            registry.reserve_name("ALICE").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            registry
                .register([1; 16], "alice", "lobby")
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        drop(pending);
        assert!(registry.reserve_name("Alice").is_ok());
        assert!(registry.register([1; 16], "Alice", "lobby").is_ok());
        for invalid in ["", "Alice Bob", "*", "too_long_username"] {
            assert_eq!(
                registry.reserve_name(invalid).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn pending_conversion_keeps_name_reserved_until_player_disconnects() {
        let registry = Arc::new(PlayerRegistry::default());
        let pending = registry.reserve_name("Alice").unwrap();
        let player = pending.register([1; 16], "lobby").unwrap();
        let snapshot = registry.snapshot();
        assert_eq!(
            snapshot.iter().map(shown).collect::<Vec<_>>(),
            [(&[1; 16], "Alice", "lobby", None, false, 0)]
        );
        assert_eq!((snapshot[0].peer, snapshot[0].connection), (0, 0));
        assert!(snapshot[0].properties.is_empty());
        assert_eq!(
            registry.reserve_name("aLiCe").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        player.set_server("survival");
        assert_eq!(registry.get(&[1; 16]).unwrap().server, "survival");
        drop(player);
        assert!(registry.reserve_name("Alice").is_ok());
    }

    #[test]
    fn duplicates_never_replace_existing_players_and_failed_conversion_releases_name() {
        let registry = Arc::new(PlayerRegistry::default());
        let original = registry.register([1; 16], "Alice", "survival").unwrap();
        assert_eq!(
            registry.reserve_name("Alice").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let pending = registry.reserve_name("Bob").unwrap();
        assert_eq!(
            pending.register([1; 16], "lobby").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            registry.snapshot().iter().map(shown).collect::<Vec<_>>(),
            [(&[1; 16], "Alice", "survival", None, false, 0)]
        );
        let bob = registry
            .reserve_name("Bob")
            .unwrap()
            .register([2; 16], "lobby")
            .unwrap();
        assert_eq!(registry.len(), 2);
        drop(bob);
        drop(original);
        assert!(registry.is_empty());
    }

    #[test]
    fn converting_a_pending_name_never_opens_a_duplicate_registration_gap() {
        let registry = Arc::new(PlayerRegistry::default());
        let pending = registry.reserve_name("Alice").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let other_registry = registry.clone();
        let other_barrier = barrier.clone();
        let duplicate = std::thread::spawn(move || {
            other_barrier.wait();
            other_registry.register([2; 16], "ALICE", "survival")
        });
        barrier.wait();
        let _player = pending.register([1; 16], "lobby").unwrap();
        assert_eq!(
            duplicate.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn a_fenced_or_catching_up_registry_admits_nobody() {
        let registry = Arc::new(PlayerRegistry::default());
        registry.set_admission(Admission::Fenced { message: None });
        let error = registry.reserve_name("Alice").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        assert_eq!(
            crate::text::reason(&error),
            crate::text::catalog::notice(Notice::ClusterPartition)
        );
        registry.set_admission(Admission::CatchingUp);
        assert_eq!(
            registry
                .register([1; 16], "Alice", "lobby")
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotConnected
        );
        registry.set_admission(Admission::Open);
        assert!(registry.register([1; 16], "Alice", "lobby").is_ok());
    }

    #[test]
    fn local_changes_reach_the_sink_in_sequence() {
        #[derive(Default)]
        struct Recorder(Mutex<Vec<AttributeUpdate>>);
        impl AttributeSink for Recorder {
            fn send(&self, update: AttributeUpdate) {
                self.0.lock().unwrap().push(update);
            }
        }
        let registry = Arc::new(PlayerRegistry::default());
        let recorder = Arc::new(Recorder::default());
        registry.set_sink(Some(recorder.clone()));
        let alice = registry.register([1; 16], "Alice", "lobby").unwrap();
        alice.set_server("survival");
        alice.set_address("[::ffff:192.0.2.1]:25565".parse().unwrap());
        drop(alice);
        let updates = recorder.0.lock().unwrap();
        assert_eq!(
            updates
                .iter()
                .map(|update| update.sequence)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        let UpdateBody::Changes(changes) = &updates[2].body else {
            panic!("expected changes");
        };
        let [AttributeChange::Upsert(attributes)] = &changes[..] else {
            panic!("expected an upsert");
        };
        assert_eq!(
            (
                attributes.server.as_str(),
                attributes.ip.as_str(),
                attributes.port
            ),
            ("survival", "192.0.2.1", 25565)
        );
        assert_eq!(
            updates[3].body,
            UpdateBody::Changes(vec![AttributeChange::Remove([1; 16])])
        );
    }
}
