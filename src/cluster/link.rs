//! Peer links: authenticated connections between peers, with typed stream
//! kinds, handlers, liveness and reconnects.
//!
//! Each pair of peers keeps two connections: **control** for consensus and
//! small messages, **bulk** for snapshots and file contents, so a large
//! transfer never shares a congestion window with heartbeats. Every message
//! is one stream: `[kind][flags][body]`, answered with `[status][body]`.
//!
//! The first stream of a connection is the handshake. Both sides present a
//! [`Hello`]: name, address, cluster id, Raft id, incarnation, version, the
//! entry kinds they support and the hash of their shared configuration. They
//! prove that they hold the cluster token with an HMAC over the connection's
//! TLS-derived binding (so the proof cannot be replayed on another
//! connection) and send an HMAC of every shared secret keyed by the token. A
//! peer of another cluster, without the token, or with a differing secret is
//! refused, and the log names the reason. TLS itself (mutual, against the
//! cluster CA) is the transport's job.
//!
//! When both sides dial each other, both connections survive the handshake;
//! each side then sends on the one dialed by the peer with the smaller name
//! and closes the other a few seconds later, so the pair settles on one
//! connection per class without coordination.
//!
//! **Liveness comes from links.** Every member is pinged on the control
//! connection each [`Timing::ping_interval`], and every message received
//! updates [`Links::last_heard`]. A member without a control connection is
//! redialed with exponential backoff (100 ms doubling to 5 s).
use super::{
    hex,
    settings::valid_peer_name,
    transport::{Connection, Incoming, ReplySink, Transport, stopped},
    unhex,
};
use aws_lc_rs::hmac;
use bytes::{BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    io,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{broadcast, watch},
    time::Instant,
};

/// The ALPN of peer links.
pub const ALPN: &[u8] = b"rift-cluster/1";
const PROTOCOL: u32 = 1;
const HELLO: u8 = 0xf0;
const PING: u8 = 0xf1;
const FLAG_REPLY: u8 = 1;
const OK: u8 = 0;
const FAILED: u8 = 1;
const UNHANDLED: u8 = 2;
/// The largest message a link accepts; snapshots travel in smaller chunks.
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;
const MAX_HELLO: usize = 64 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the losing one of two simultaneous connections stays open, so
/// messages already sent on it are still answered.
const DUPLICATE_GRACE: Duration = Duration::from_secs(5);
const BACKOFF_FIRST: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Which of a pair's two connections a stream kind uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Control,
    Bulk,
}

impl Class {
    fn index(self) -> usize {
        match self {
            Self::Control => 0,
            Self::Bulk => 1,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Bulk => "bulk",
        }
    }
}

/// The typed streams of docs/clustering.md, "Peer links".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StreamKind {
    /// AppendEntries, votes, forwarded proposals and reads.
    Raft = 1,
    /// Player attributes and server moves.
    Presence = 2,
    /// Actions for players connected to the receiving peer.
    Player = 3,
    /// Bus interest and forwarded messages.
    Bus = 4,
    /// Reservations and stop vetoes.
    Load = 5,
    /// Queue tickets for the leader's queue service, and its turns.
    Queue = 6,
    /// Snapshot chunks.
    Snapshot = 16,
    /// Template and file contents by SHA-256.
    Blob = 17,
}

impl StreamKind {
    pub const ALL: [Self; 8] = [
        Self::Raft,
        Self::Presence,
        Self::Player,
        Self::Bus,
        Self::Load,
        Self::Queue,
        Self::Snapshot,
        Self::Blob,
    ];

    pub fn class(self) -> Class {
        match self {
            Self::Snapshot | Self::Blob => Class::Bulk,
            _ => Class::Control,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Raft => "raft",
            Self::Presence => "presence",
            Self::Player => "player",
            Self::Bus => "bus",
            Self::Load => "load",
            Self::Queue => "queue",
            Self::Snapshot => "snapshot",
            Self::Blob => "blob",
        }
    }

    fn code(self) -> u8 {
        self as u8
    }
}

/// One lock per peer and connection class, so two dials never race.
type Dials = HashMap<(String, Class), Arc<tokio::sync::Mutex<()>>>;

/// What this peer presents in every handshake.
#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub name: String,
    /// The address other peers dial (`advertise`).
    pub address: String,
    pub version: String,
    /// Entry kinds this build can apply.
    pub kinds: Vec<String>,
    /// Hash of the shared sections of the evaluated configuration.
    pub config_hash: String,
    /// Shared secrets by name (forwarding secret, node tokens). Only their
    /// HMACs keyed by the cluster token leave the process.
    pub secrets: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub ping_interval: Duration,
    pub connect_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(5),
        }
    }
}

/// The handshake message, sent by both sides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub name: String,
    pub address: String,
    pub class: Class,
    pub cluster_id: Option<String>,
    pub raft_id: Option<u64>,
    pub incarnation: u64,
    pub version: String,
    pub kinds: Vec<String>,
    pub config_hash: String,
    /// Hex HMAC of the connection binding, keyed by the cluster token.
    pub proof: String,
    /// Hex HMAC of each shared secret, keyed by the cluster token.
    pub secrets: BTreeMap<String, String>,
}

/// A handler's error, as the remote peer reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteError {
    pub peer: String,
    pub message: String,
}

impl std::fmt::Display for RemoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "peer {}: {}", self.peer, self.message)
    }
}

impl std::error::Error for RemoteError {}

/// The remote handler's error inside `error`, when that is what it is
/// rather than a network failure.
pub fn remote_error(error: &io::Error) -> Option<&RemoteError> {
    error.get_ref()?.downcast_ref::<RemoteError>()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// The control connection to this peer is established.
    Up(String),
    /// The control connection to this peer is gone.
    Down(String),
}

/// A peer as the links see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LinkStatus {
    pub name: String,
    pub address: Option<String>,
    pub member: bool,
    pub up: bool,
    /// Milliseconds since anything arrived from this peer.
    pub last_heard_ms: Option<u64>,
    pub version: Option<String>,
    pub config_hash: Option<String>,
    pub kinds: Vec<String>,
    pub raft_id: Option<u64>,
    pub incarnation: Option<u64>,
}

type RequestFuture = super::transport::BoxFuture<'static, Result<Bytes, String>>;
type RequestHandler = Arc<dyn Fn(String, Bytes) -> RequestFuture + Send + Sync>;
type MessageHandler = Arc<dyn Fn(String, Bytes) + Send + Sync>;
/// Decides whether a peer that passed the handshake may link at all.
pub type Admission = Arc<dyn Fn(&Hello) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
enum Handler {
    Request(RequestHandler),
    Message(MessageHandler),
}

#[derive(Clone)]
struct Link {
    id: u64,
    class: Class,
    conn: Arc<dyn Connection>,
    /// The name of the peer that dialed this connection.
    dialer: String,
}

#[derive(Default)]
struct Slot {
    address: Option<String>,
    member: bool,
    links: [Option<Link>; 2],
    failures: [u32; 2],
    retry_at: [Option<Instant>; 2],
    last_heard: Option<Instant>,
    hello: Option<Hello>,
}

#[derive(Default)]
struct State {
    peers: HashMap<String, Slot>,
    /// Seed addresses not yet resolved to a peer name.
    seeds: Vec<String>,
    seed_retry: HashMap<String, Instant>,
    /// Peers with a ping or dial in flight.
    busy: HashSet<String>,
}

#[derive(Default, Clone)]
struct ClusterIdentity {
    cluster_id: Option<String>,
    raft_id: Option<u64>,
    incarnation: u64,
}

struct Inner {
    transport: Arc<dyn Transport>,
    identity: Identity,
    /// The shared-configuration hash handshakes present from now on: it
    /// starts as [`Identity::config_hash`] and follows reloads.
    config_hash: RwLock<String>,
    key: hmac::Key,
    secret_proofs: BTreeMap<String, String>,
    timing: Timing,
    cluster: RwLock<ClusterIdentity>,
    state: Mutex<State>,
    handlers: RwLock<HashMap<u8, Handler>>,
    admission: RwLock<Option<Admission>>,
    events: broadcast::Sender<LinkEvent>,
    dials: Mutex<Dials>,
    shutdown: watch::Sender<bool>,
    next_link: AtomicU64,
}

/// The peer links of this process. Cheap to clone.
#[derive(Clone)]
pub struct Links {
    inner: Arc<Inner>,
}

impl Links {
    /// Start accepting on `transport` and pinging members. `seeds` are
    /// addresses to dial until they answer.
    pub fn new(
        transport: Arc<dyn Transport>,
        identity: Identity,
        token: &str,
        timing: Timing,
        seeds: Vec<String>,
    ) -> Self {
        let key = hmac::Key::new(hmac::HMAC_SHA256, token.as_bytes());
        let secret_proofs = identity
            .secrets
            .iter()
            .map(|(name, value)| {
                let mut message = b"rift-cluster/1 secret\0".to_vec();
                message.extend_from_slice(name.as_bytes());
                message.push(0);
                message.extend_from_slice(value.as_bytes());
                (name.clone(), hex(hmac::sign(&key, &message).as_ref()))
            })
            .collect();
        let (events, _) = broadcast::channel(256);
        let (shutdown, _) = watch::channel(false);
        let links = Self {
            inner: Arc::new(Inner {
                transport,
                config_hash: RwLock::new(identity.config_hash.clone()),
                identity,
                key,
                secret_proofs,
                timing,
                cluster: RwLock::new(ClusterIdentity::default()),
                state: Mutex::new(State {
                    seeds,
                    ..State::default()
                }),
                handlers: RwLock::new(HashMap::new()),
                admission: RwLock::new(None),
                events,
                dials: Mutex::new(HashMap::new()),
                shutdown,
                next_link: AtomicU64::new(1),
            }),
        };
        links.spawn_accept();
        links.spawn_pinger();
        links
    }

    pub fn name(&self) -> &str {
        &self.inner.identity.name
    }

    pub fn identity(&self) -> &Identity {
        &self.inner.identity
    }

    /// What this peer presents about its cluster membership from now on.
    pub fn set_cluster(&self, cluster_id: Option<String>, raft_id: Option<u64>, incarnation: u64) {
        *self.inner.cluster.write().unwrap() = ClusterIdentity {
            cluster_id,
            raft_id,
            incarnation,
        };
    }

    /// The hash of the shared sections of this peer's evaluated
    /// configuration, presented in handshakes from now on (after a reload).
    pub fn set_config_hash(&self, hash: &str) {
        *self
            .inner
            .config_hash
            .write()
            .unwrap_or_else(|e| e.into_inner()) = hash.to_owned();
    }

    /// The shared-configuration hash this peer presents.
    pub fn config_hash(&self) -> String {
        self.inner
            .config_hash
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_admission(&self, admission: Admission) {
        *self.inner.admission.write().unwrap() = Some(admission);
    }

    /// Answer requests of `kind`. The handler gets the sending peer's name
    /// and the body; its `Err` reaches the caller as a [`RemoteError`].
    pub fn on_request<F, Fut>(&self, kind: StreamKind, handler: F)
    where
        F: Fn(String, Bytes) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Bytes, String>> + Send + 'static,
    {
        let handler: RequestHandler = Arc::new(move |peer, body| Box::pin(handler(peer, body)));
        self.inner
            .handlers
            .write()
            .unwrap()
            .insert(kind.code(), Handler::Request(handler));
    }

    /// Receive one-way messages of `kind`. The handler runs on the
    /// connection's task and must not block.
    pub fn on_message<F>(&self, kind: StreamKind, handler: F)
    where
        F: Fn(String, Bytes) + Send + Sync + 'static,
    {
        self.inner
            .handlers
            .write()
            .unwrap()
            .insert(kind.code(), Handler::Message(Arc::new(handler)));
    }

    /// Send `body` to `peer` and wait at most `timeout` for its handler's
    /// answer. Dials the peer if needed.
    pub async fn request(
        &self,
        peer: &str,
        kind: StreamKind,
        body: Bytes,
        timeout: Duration,
    ) -> io::Result<Bytes> {
        let deadline = Instant::now() + timeout;
        let link = tokio::time::timeout_at(deadline, self.link(peer, kind.class()))
            .await
            .map_err(|_| timed_out(peer))??;
        let message = frame(kind.code(), FLAG_REPLY, &body);
        let reply =
            match tokio::time::timeout_at(deadline, link.conn.exchange(message, true, MAX_MESSAGE))
                .await
            {
                Err(_) => return Err(timed_out(peer)),
                Ok(Err(error)) => {
                    self.drop_link(peer, &link);
                    return Err(error);
                }
                Ok(Ok(None)) => return Err(io::Error::other("no reply")),
                Ok(Ok(Some(reply))) => reply,
            };
        self.heard(peer);
        self.answer(peer, reply)
    }

    /// Send a one-way message: returns once it is on its way, not handled.
    pub async fn send(&self, peer: &str, kind: StreamKind, body: Bytes) -> io::Result<()> {
        let link = tokio::time::timeout(
            self.inner.timing.connect_timeout,
            self.link(peer, kind.class()),
        )
        .await
        .map_err(|_| timed_out(peer))??;
        let message = frame(kind.code(), 0, &body);
        if let Err(error) = link.conn.exchange(message, false, MAX_MESSAGE).await {
            self.drop_link(peer, &link);
            return Err(error);
        }
        Ok(())
    }

    /// Dial `address`, whose peer name is not known yet, and return the name
    /// it presented. Its links are then used like any other peer's.
    pub async fn connect_address(&self, address: &str, timeout: Duration) -> io::Result<String> {
        let (name, _) = tokio::time::timeout(timeout, self.dial(address, Class::Control))
            .await
            .map_err(|_| timed_out(address))??;
        Ok(name)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LinkEvent> {
        self.inner.events.subscribe()
    }

    /// When anything last arrived from `peer` on any connection.
    pub fn last_heard(&self, peer: &str) -> Option<Instant> {
        self.inner.state.lock().unwrap().peers.get(peer)?.last_heard
    }

    pub fn is_up(&self, peer: &str) -> bool {
        self.inner
            .state
            .lock()
            .unwrap()
            .peers
            .get(peer)
            .is_some_and(|slot| slot.links[Class::Control.index()].is_some())
    }

    /// The handshake `peer` presented last.
    pub fn hello(&self, peer: &str) -> Option<Hello> {
        self.inner
            .state
            .lock()
            .unwrap()
            .peers
            .get(peer)?
            .hello
            .clone()
    }

    pub fn statuses(&self) -> Vec<LinkStatus> {
        let now = Instant::now();
        let state = self.inner.state.lock().unwrap();
        let mut statuses: Vec<_> = state
            .peers
            .iter()
            .map(|(name, slot)| LinkStatus {
                name: name.clone(),
                address: slot.address.clone(),
                member: slot.member,
                up: slot.links[Class::Control.index()].is_some(),
                last_heard_ms: slot.last_heard.map(|at| {
                    u64::try_from(now.saturating_duration_since(at).as_millis()).unwrap_or(u64::MAX)
                }),
                version: slot.hello.as_ref().map(|hello| hello.version.clone()),
                config_hash: slot.hello.as_ref().map(|hello| hello.config_hash.clone()),
                kinds: slot
                    .hello
                    .as_ref()
                    .map(|hello| hello.kinds.clone())
                    .unwrap_or_default(),
                raft_id: slot.hello.as_ref().and_then(|hello| hello.raft_id),
                incarnation: slot.hello.as_ref().map(|hello| hello.incarnation),
            })
            .collect();
        statuses.sort_by(|a, b| a.name.cmp(&b.name));
        statuses
    }

    /// The members to keep linked, by name and address; this peer itself is
    /// skipped. Peers no longer listed stay reachable but are not pinged.
    pub fn set_members(&self, members: &[(String, String)]) {
        let mut state = self.inner.state.lock().unwrap();
        for slot in state.peers.values_mut() {
            slot.member = false;
        }
        for (name, address) in members {
            if *name == self.inner.identity.name {
                continue;
            }
            let slot = state.peers.entry(name.clone()).or_default();
            slot.member = true;
            slot.address = Some(address.clone());
        }
    }

    /// Close every link and stop accepting. The handle is useless afterwards.
    pub fn shutdown(&self) {
        self.inner.shutdown.send_replace(true);
        self.inner.transport.close();
        let links: Vec<Link> = {
            let mut state = self.inner.state.lock().unwrap();
            state
                .peers
                .values_mut()
                .flat_map(|slot| slot.links.iter_mut().filter_map(Option::take))
                .collect()
        };
        for link in links {
            link.conn.close("shutting down");
        }
        // Handlers hold what they serve (players, coordination, queues),
        // which may hold this peer's cluster: drop them, so a stopped peer
        // releases its replica and instance database.
        self.inner.handlers.write().unwrap().clear();
    }

    fn answer(&self, peer: &str, reply: Bytes) -> io::Result<Bytes> {
        let Some((&status, _)) = reply.split_first() else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "empty reply"));
        };
        let body = reply.slice(1..);
        match status {
            OK => Ok(body),
            UNHANDLED => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("peer {peer}: {}", String::from_utf8_lossy(&body)),
            )),
            _ => Err(io::Error::other(RemoteError {
                peer: peer.to_owned(),
                message: String::from_utf8_lossy(&body).into_owned(),
            })),
        }
    }

    fn heard(&self, peer: &str) {
        if let Some(slot) = self.inner.state.lock().unwrap().peers.get_mut(peer) {
            slot.last_heard = Some(Instant::now());
        }
    }

    /// The link of `class` to `peer`, dialing it if there is none.
    async fn link(&self, peer: &str, class: Class) -> io::Result<Link> {
        if let Some(link) = self.current(peer, class)? {
            return Ok(link);
        }
        let lock = self
            .inner
            .dials
            .lock()
            .unwrap()
            .entry((peer.to_owned(), class))
            .or_default()
            .clone();
        let _dialing = lock.lock().await;
        // Another caller may have dialed while this one waited.
        if let Some(link) = self.current(peer, class)? {
            return Ok(link);
        }
        let address = {
            let state = self.inner.state.lock().unwrap();
            let slot = state.peers.get(peer).ok_or_else(|| unknown(peer))?;
            if let Some(retry_at) = slot.retry_at[class.index()]
                && retry_at > Instant::now()
            {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("peer {peer} is unreachable; retrying shortly"),
                ));
            }
            slot.address.clone().ok_or_else(|| unknown(peer))?
        };
        match tokio::time::timeout(
            self.inner.timing.connect_timeout,
            self.dial(&address, class),
        )
        .await
        {
            Ok(Ok((name, _))) if name == peer => {}
            Ok(Ok((name, _))) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{address} belongs to peer {name}, not {peer}"),
                ));
            }
            Ok(Err(error)) => {
                self.failed(peer, class);
                return Err(error);
            }
            Err(_) => {
                self.failed(peer, class);
                return Err(timed_out(peer));
            }
        }
        self.current(peer, class)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                format!("peer {peer}: link closed"),
            )
        })
    }

    fn current(&self, peer: &str, class: Class) -> io::Result<Option<Link>> {
        let state = self.inner.state.lock().unwrap();
        let slot = state.peers.get(peer).ok_or_else(|| unknown(peer))?;
        Ok(slot.links[class.index()].clone())
    }

    fn failed(&self, peer: &str, class: Class) {
        if let Some(slot) = self.inner.state.lock().unwrap().peers.get_mut(peer) {
            let failures = &mut slot.failures[class.index()];
            *failures = failures.saturating_add(1);
            slot.retry_at[class.index()] = Some(Instant::now() + backoff(*failures));
        }
    }

    /// Dial `address` for `class`, run the handshake and register the link.
    async fn dial(&self, address: &str, class: Class) -> io::Result<(String, Link)> {
        let conn = self.inner.transport.connect(address).await?;
        let binding = conn.binding()?;
        let hello = self.hello_for(class, &binding, Role::Dialer);
        let body = serde_json::to_vec(&hello).map_err(super::json_error)?;
        let reply = match conn
            .exchange(frame(HELLO, FLAG_REPLY, &body), true, MAX_HELLO)
            .await
        {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                conn.close("no handshake answer");
                return Err(io::Error::other("no handshake answer"));
            }
            Err(error) => {
                conn.close("handshake failed");
                return Err(error);
            }
        };
        let Some((&status, body)) = reply.split_first() else {
            conn.close("empty handshake answer");
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty handshake answer",
            ));
        };
        if status != OK {
            let reason = String::from_utf8_lossy(body).into_owned();
            conn.close("refused");
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{address} refused the link: {reason}"),
            ));
        }
        let welcome: Hello = match serde_json::from_slice(body) {
            Ok(welcome) => welcome,
            Err(error) => {
                conn.close("malformed handshake");
                return Err(super::json_error(error));
            }
        };
        if let Err(reason) = self.verify(&welcome, &binding, Role::Acceptor, class) {
            conn.close(&reason);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("refused the link to {address} ({}): {reason}", welcome.name),
            ));
        }
        let name = welcome.name.clone();
        let link = self.register(&welcome, conn, self.inner.identity.name.clone(), class);
        self.spawn_serve(name.clone(), link.clone());
        Ok((name, link))
    }

    /// Answer the handshake on an inbound connection, then serve it.
    async fn accept_link(&self, conn: Arc<dyn Connection>) {
        let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let incoming = conn.next().await?;
            incoming.read.await.ok()
        })
        .await;
        let Ok(Some((message, sink))) = first else {
            conn.close("no handshake");
            return;
        };
        let hello = parse_frame(&message)
            .filter(|(code, _, _)| *code == HELLO)
            .and_then(|(_, _, body)| serde_json::from_slice::<Hello>(&body).ok());
        let Some(hello) = hello else {
            refuse(conn, sink, "expected a handshake").await;
            return;
        };
        let binding = match conn.binding() {
            Ok(binding) => binding,
            Err(error) => {
                refuse(conn, sink, &error.to_string()).await;
                return;
            }
        };
        if let Err(reason) = self.verify(&hello, &binding, Role::Dialer, hello.class) {
            log_warn!(
                "cluster: refused a link from {} at {}: {reason}",
                hello.name,
                conn.remote()
            );
            refuse(conn, sink, &reason).await;
            return;
        }
        let welcome = self.hello_for(hello.class, &binding, Role::Acceptor);
        let body = match serde_json::to_vec(&welcome) {
            Ok(body) => body,
            Err(error) => {
                refuse(conn, sink, &error.to_string()).await;
                return;
            }
        };
        sink.send(status_frame(OK, &body)).await;
        let link = self.register(&hello, conn, hello.name.clone(), hello.class);
        self.serve(hello.name.clone(), link).await;
    }

    fn hello_for(&self, class: Class, binding: &[u8; 32], role: Role) -> Hello {
        let identity = &self.inner.identity;
        let cluster = self.inner.cluster.read().unwrap().clone();
        let proof = hmac::sign(
            &self.inner.key,
            &proof_message(role, binding, &identity.name),
        );
        Hello {
            protocol: PROTOCOL,
            name: identity.name.clone(),
            address: identity.address.clone(),
            class,
            cluster_id: cluster.cluster_id,
            raft_id: cluster.raft_id,
            incarnation: cluster.incarnation,
            version: identity.version.clone(),
            kinds: identity.kinds.clone(),
            config_hash: self.config_hash(),
            proof: hex(proof.as_ref()),
            secrets: self.inner.secret_proofs.clone(),
        }
    }

    /// Check a handshake `hello` sent by the side in `role`.
    fn verify(
        &self,
        hello: &Hello,
        binding: &[u8; 32],
        role: Role,
        class: Class,
    ) -> Result<(), String> {
        if hello.protocol != PROTOCOL {
            return Err(format!(
                "speaks peer protocol {}, this peer {PROTOCOL}",
                hello.protocol
            ));
        }
        if hello.class != class {
            return Err("answered for another connection class".into());
        }
        if !valid_peer_name(&hello.name) {
            return Err(format!("invalid peer name {:?}", hello.name));
        }
        if hello.name == self.inner.identity.name {
            return Err(format!(
                "presents this peer's own name {:?}; check seeds, advertise and that names are unique",
                hello.name
            ));
        }
        let proof = unhex(&hello.proof).ok_or("malformed token proof")?;
        hmac::verify(
            &self.inner.key,
            &proof_message(role, binding, &hello.name),
            &proof,
        )
        .map_err(|_| "does not hold the cluster token".to_owned())?;
        let ours = self.inner.cluster.read().unwrap().cluster_id.clone();
        if let (Some(ours), Some(theirs)) = (&ours, &hello.cluster_id)
            && ours != theirs
        {
            return Err(format!(
                "belongs to cluster {theirs}, this peer to cluster {ours}"
            ));
        }
        let names: std::collections::BTreeSet<&String> = self
            .inner
            .secret_proofs
            .keys()
            .chain(hello.secrets.keys())
            .collect();
        for name in names {
            match (self.inner.secret_proofs.get(name), hello.secrets.get(name)) {
                (Some(ours), Some(theirs)) if ours == theirs => {}
                (Some(_), Some(_)) => {
                    return Err(format!("secret {name} differs between the peers"));
                }
                (Some(_), None) => {
                    return Err(format!(
                        "secret {name} is set on this peer but not on {}",
                        hello.name
                    ));
                }
                (None, Some(_)) => {
                    return Err(format!(
                        "secret {name} is set on {} but not on this peer",
                        hello.name
                    ));
                }
                (None, None) => {}
            }
        }
        let admission = self.inner.admission.read().unwrap().clone();
        if let Some(admission) = admission {
            admission(hello)?;
        }
        // Not a refusal: during a deployment the peers reload one after the
        // other. `rift admin cluster` names the sections that differ.
        let ours = self.config_hash();
        if class == Class::Control
            && !ours.is_empty()
            && !hello.config_hash.is_empty()
            && ours != hello.config_hash
        {
            log_info!(
                "cluster: {} evaluates the shared configuration sections differently (hash {}, this peer {}); `rift admin cluster` shows which sections",
                hello.name,
                short(&hello.config_hash),
                short(&ours)
            );
        }
        Ok(())
    }

    /// Record a handshaken connection. Of two connections of one class the
    /// one dialed by the smaller peer name wins; the other closes later.
    fn register(
        &self,
        hello: &Hello,
        conn: Arc<dyn Connection>,
        dialer: String,
        class: Class,
    ) -> Link {
        let link = Link {
            id: self.inner.next_link.fetch_add(1, Ordering::Relaxed),
            class,
            conn,
            dialer,
        };
        let (came_up, loser) = {
            let mut state = self.inner.state.lock().unwrap();
            state.seeds.retain(|seed| *seed != hello.address);
            let slot = state.peers.entry(hello.name.clone()).or_default();
            if slot.address.is_none() {
                slot.address = Some(hello.address.clone());
            }
            slot.hello = Some(hello.clone());
            slot.last_heard = Some(Instant::now());
            slot.failures[class.index()] = 0;
            slot.retry_at[class.index()] = None;
            let current = &mut slot.links[class.index()];
            match current.take() {
                None => {
                    *current = Some(link.clone());
                    (class == Class::Control, None)
                }
                Some(existing) if link.dialer <= existing.dialer => {
                    *current = Some(link.clone());
                    (false, Some(existing))
                }
                Some(existing) => {
                    *current = Some(existing);
                    (false, Some(link.clone()))
                }
            }
        };
        if came_up {
            let _ = self.inner.events.send(LinkEvent::Up(hello.name.clone()));
        }
        if let Some(loser) = loser {
            tokio::spawn(async move {
                tokio::time::sleep(DUPLICATE_GRACE).await;
                loser.conn.close("duplicate link");
            });
        }
        link
    }

    /// Forget `link` (after it failed or closed) unless it was replaced.
    fn drop_link(&self, peer: &str, link: &Link) {
        let went_down = {
            let mut state = self.inner.state.lock().unwrap();
            match state.peers.get_mut(peer) {
                Some(slot) => {
                    let current = &mut slot.links[link.class.index()];
                    if current
                        .as_ref()
                        .is_some_and(|current| current.id == link.id)
                    {
                        *current = None;
                        link.class == Class::Control
                    } else {
                        false
                    }
                }
                None => false,
            }
        };
        link.conn.close("link dropped");
        if went_down {
            let _ = self.inner.events.send(LinkEvent::Down(peer.to_owned()));
        }
    }

    fn spawn_serve(&self, peer: String, link: Link) {
        let links = self.clone();
        tokio::spawn(async move { links.serve(peer, link).await });
    }

    /// Handle the streams the peer opens on `link` until it closes.
    async fn serve(&self, peer: String, link: Link) {
        let mut shutdown = self.inner.shutdown.subscribe();
        loop {
            let incoming = tokio::select! {
                incoming = link.conn.next() => incoming,
                _ = stopped(&mut shutdown) => None,
            };
            let Some(incoming) = incoming else { break };
            let links = self.clone();
            let peer = peer.clone();
            tokio::spawn(async move { links.handle(peer, incoming).await });
        }
        self.drop_link(&peer, &link);
    }

    async fn handle(&self, peer: String, incoming: Incoming) {
        let Ok((message, sink)) = incoming.read.await else {
            return;
        };
        self.heard(&peer);
        let Some((code, _flags, body)) = parse_frame(&message) else {
            sink.send(status_frame(FAILED, b"malformed message")).await;
            return;
        };
        if code == PING {
            sink.send(status_frame(OK, b"")).await;
            return;
        }
        let handler = self.inner.handlers.read().unwrap().get(&code).cloned();
        let reply = match handler {
            Some(Handler::Request(handler)) => match handler(peer, body).await {
                Ok(body) => status_frame(OK, &body),
                Err(error) => status_frame(FAILED, error.as_bytes()),
            },
            Some(Handler::Message(handler)) => {
                handler(peer, body);
                status_frame(OK, b"")
            }
            None => status_frame(
                UNHANDLED,
                format!("no handler for stream kind {code}").as_bytes(),
            ),
        };
        sink.send(reply).await;
    }

    fn spawn_accept(&self) {
        let links = self.clone();
        tokio::spawn(async move {
            let mut shutdown = links.inner.shutdown.subscribe();
            loop {
                let conn = tokio::select! {
                    conn = links.inner.transport.accept() => conn,
                    _ = stopped(&mut shutdown) => None,
                };
                let Some(conn) = conn else { break };
                let links = links.clone();
                tokio::spawn(async move { links.accept_link(conn).await });
            }
        });
    }

    /// Ping members on their control connection, dial members without one
    /// and seeds not yet resolved.
    fn spawn_pinger(&self) {
        let links = self.clone();
        tokio::spawn(async move {
            let mut shutdown = links.inner.shutdown.subscribe();
            let interval = links.inner.timing.ping_interval;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(interval) => {}
                    _ = stopped(&mut shutdown) => break,
                }
                links.tick();
            }
        });
    }

    fn tick(&self) {
        let now = Instant::now();
        let (pings, dials, seeds) = {
            let mut state = self.inner.state.lock().unwrap();
            let mut pings = Vec::new();
            let mut dials = Vec::new();
            let candidates: Vec<(String, Option<Link>, bool)> = state
                .peers
                .iter()
                .filter(|(name, slot)| slot.member && !state.busy.contains(*name))
                .map(|(name, slot)| {
                    let due = slot.retry_at[Class::Control.index()].is_none_or(|at| at <= now);
                    (
                        name.clone(),
                        slot.links[Class::Control.index()].clone(),
                        due && slot.address.is_some(),
                    )
                })
                .collect();
            for (name, link, dialable) in candidates {
                match link {
                    Some(link) => {
                        state.busy.insert(name.clone());
                        pings.push((name, link));
                    }
                    None if dialable => {
                        state.busy.insert(name.clone());
                        dials.push(name);
                    }
                    None => {}
                }
            }
            let mut seeds = Vec::new();
            let pending: Vec<String> = state.seeds.clone();
            for seed in pending {
                if state.seed_retry.get(&seed).is_none_or(|at| *at <= now) {
                    state.seed_retry.insert(seed.clone(), now + BACKOFF_MAX);
                    seeds.push(seed);
                }
            }
            (pings, dials, seeds)
        };
        let timeout = self.inner.timing.ping_interval * 3;
        for (name, link) in pings {
            let links = self.clone();
            tokio::spawn(async move {
                let ping = frame(PING, FLAG_REPLY, b"");
                let answered =
                    tokio::time::timeout(timeout, link.conn.exchange(ping, true, 1024)).await;
                match answered {
                    Ok(Ok(Some(_))) => links.heard(&name),
                    _ => links.drop_link(&name, &link),
                }
                links.inner.state.lock().unwrap().busy.remove(&name);
            });
        }
        for name in dials {
            let links = self.clone();
            tokio::spawn(async move {
                let _ = links.link(&name, Class::Control).await;
                links.inner.state.lock().unwrap().busy.remove(&name);
            });
        }
        for seed in seeds {
            let links = self.clone();
            tokio::spawn(async move {
                let connect = links.inner.timing.connect_timeout;
                if let Err(error) = links.connect_address(&seed, connect).await {
                    log_debug!("cluster: seed {seed}: {error}");
                }
            });
        }
    }
}

/// Answer a handshake with a refusal and close the connection once the
/// answer had time to reach the dialer.
async fn refuse(conn: Arc<dyn Connection>, sink: ReplySink, reason: &str) {
    sink.send(status_frame(FAILED, reason.as_bytes())).await;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        conn.close("refused");
    });
}

#[derive(Clone, Copy)]
enum Role {
    Dialer,
    Acceptor,
}

fn proof_message(role: Role, binding: &[u8; 32], name: &str) -> Vec<u8> {
    let mut message = b"rift-cluster/1 proof\0".to_vec();
    message.extend_from_slice(match role {
        Role::Dialer => b"dialer",
        Role::Acceptor => b"acceptor",
    });
    message.push(0);
    message.extend_from_slice(binding);
    message.push(0);
    message.extend_from_slice(name.as_bytes());
    message
}

fn frame(code: u8, flags: u8, body: &[u8]) -> Bytes {
    let mut message = BytesMut::with_capacity(body.len() + 2);
    message.put_u8(code);
    message.put_u8(flags);
    message.put_slice(body);
    message.freeze()
}

fn parse_frame(message: &Bytes) -> Option<(u8, u8, Bytes)> {
    if message.len() < 2 {
        return None;
    }
    Some((message[0], message[1], message.slice(2..)))
}

fn status_frame(status: u8, body: &[u8]) -> Bytes {
    let mut reply = BytesMut::with_capacity(body.len() + 1);
    reply.put_u8(status);
    reply.put_slice(body);
    reply.freeze()
}

fn backoff(failures: u32) -> Duration {
    let factor = 1u32 << failures.saturating_sub(1).min(16);
    (BACKOFF_FIRST * factor).min(BACKOFF_MAX)
}

fn timed_out(peer: &str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, format!("peer {peer}: timed out"))
}

fn unknown(peer: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("peer {peer}: no known address"),
    )
}

/// The first 12 characters of a hash, for logs.
fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::transport::memory::Network;

    fn identity(name: &str, secret: &str) -> Identity {
        Identity {
            name: name.into(),
            address: name.into(),
            version: "test".into(),
            kinds: vec!["peer".into()],
            config_hash: "hash".into(),
            secrets: BTreeMap::from([("forwarding".to_owned(), secret.to_owned())]),
        }
    }

    fn links(network: &Network, name: &str, token: &str, secret: &str) -> Links {
        Links::new(
            network.bind(name),
            identity(name, secret),
            token,
            Timing::default(),
            Vec::new(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn requests_reach_handlers_and_errors_come_back_typed() {
        let network = Network::default();
        let a = links(&network, "a", "0123456789abcdef", "s");
        let b = links(&network, "b", "0123456789abcdef", "s");
        b.on_request(StreamKind::Raft, |peer, body| async move {
            if body.as_ref() == b"fail" {
                Err(format!("refused for {peer}"))
            } else {
                Ok(Bytes::from(format!(
                    "{peer}:{}",
                    String::from_utf8_lossy(&body)
                )))
            }
        });
        a.set_members(&[("b".into(), "b".into())]);
        let reply = a
            .request(
                "b",
                StreamKind::Raft,
                Bytes::from_static(b"hi"),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(reply.as_ref(), b"a:hi");
        let error = a
            .request(
                "b",
                StreamKind::Raft,
                Bytes::from_static(b"fail"),
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert_eq!(remote_error(&error).unwrap().message, "refused for a");
        let error = a
            .request("b", StreamKind::Load, Bytes::new(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(a.is_up("b"));
        assert!(a.last_heard("b").is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_wrong_token_or_secret_is_refused() {
        let network = Network::default();
        let a = links(&network, "a", "0123456789abcdef", "s");
        let _b = links(&network, "b", "fedcba9876543210", "s");
        let _c = links(&network, "c", "0123456789abcdef", "other");
        let error = a
            .connect_address("b", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert!(error.to_string().contains("cluster token"), "{error}");
        let error = a
            .connect_address("c", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("secret forwarding"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn partitions_bring_links_down_and_healing_brings_them_back() {
        let network = Network::default();
        let a = links(&network, "a", "0123456789abcdef", "s");
        let _b = links(&network, "b", "0123456789abcdef", "s");
        let mut events = a.subscribe();
        a.set_members(&[("b".into(), "b".into())]);
        assert_eq!(events.recv().await.unwrap(), LinkEvent::Up("b".into()));
        network.partition("a", "b");
        assert_eq!(events.recv().await.unwrap(), LinkEvent::Down("b".into()));
        network.heal();
        assert_eq!(events.recv().await.unwrap(), LinkEvent::Up("b".into()));
    }
}
