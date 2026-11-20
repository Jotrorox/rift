//! Shared, bounded asynchronous messaging for Rift and its plugins.
//!
//! Core publish never awaits a consumer or disk. Payload clones share their
//! allocation. Persistence is an explicit operation in [`stream`].
//!
//! In a cluster every peer runs its own broker and federates the subjects
//! whose [`Scope`] allows it through a [`Federation`]; without one the
//! broker serves only its own subscribers.

pub mod federation;
pub mod quic;
pub mod scope;
pub mod stream;
pub use federation::{
    Federation, FederationFrame, ForwardedMessage, InterestEntry, InterestKey, InterestUpdate,
    OwnerRouter, PeerId,
};
pub use rift_messaging::Message;
pub use scope::{Scope, SubscriptionScope, subject_scope};
pub use stream::{ConsumerConfig, Delivery, DurableConsumer, Record, Stream, StreamConfig};

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    InvalidSubject(String),
    InvalidQueue,
    InvalidConfig(String),
    PayloadTooLarge { size: usize, limit: usize },
    SubscriptionLimit,
    SlowConsumer,
    Closed,
    Timeout,
    NoResponders,
    Io(std::io::Error),
    Storage(String),
    StreamNotMatched,
    ConsumerNotFound,
    InvalidAck,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSubject(s) => write!(f, "invalid subject: {s}"),
            Self::InvalidQueue => f.write_str("invalid queue group"),
            Self::InvalidConfig(s) => write!(f, "invalid messaging configuration: {s}"),
            Self::PayloadTooLarge { size, limit } => {
                write!(f, "payload size {size} exceeds limit {limit}")
            }
            Self::SubscriptionLimit => f.write_str("subscription limit reached"),
            Self::SlowConsumer => {
                f.write_str("subscriber exceeded its bounded queue and was disconnected")
            }
            Self::Closed => f.write_str("subscription closed"),
            Self::Timeout => f.write_str("request timed out"),
            Self::NoResponders => f.write_str("no responders available"),
            Self::Io(e) => write!(f, "messaging I/O: {e}"),
            Self::Storage(s) => write!(f, "stream storage: {s}"),
            Self::StreamNotMatched => f.write_str("subject does not match this stream"),
            Self::ConsumerNotFound => f.write_str("consumer does not exist"),
            Self::InvalidAck => f.write_str("sequence is not pending acknowledgement"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokerConfig {
    pub subscription_capacity: usize,
    pub max_payload_bytes: usize,
    pub max_subject_bytes: usize,
    pub max_subscriptions: usize,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            subscription_capacity: 1024,
            max_payload_bytes: 1024 * 1024,
            max_subject_bytes: 1024,
            max_subscriptions: 65_536,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublishReport {
    /// Subscribers of this broker that received the message.
    pub delivered: usize,
    pub slow_consumers: usize,
    /// Peers the message was forwarded to; always zero without a
    /// [`Federation`].
    pub forwarded: usize,
}

/// How a subscription takes part in a cluster; see [`Broker::subscribe_with`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubscribeOptions {
    /// Whether messages on user subjects published on other peers arrive.
    pub scope: SubscriptionScope,
    /// The subscriber is a remote (QUIC) client of this peer rather than
    /// in-process code; only clients hear other peers' [`Scope::Origin`]
    /// events.
    pub client: bool,
}

#[derive(Clone)]
pub struct Broker {
    inner: Arc<Inner>,
}

struct Inner {
    config: BrokerConfig,
    registry: RwLock<Registry>,
    next_id: AtomicU64,
    /// Whether [`Inner::federation`] is set, read without a lock on every
    /// publish.
    federated: AtomicBool,
    federation: RwLock<Option<Arc<dyn Federation>>>,
    owner_router: RwLock<Option<Arc<dyn OwnerRouter>>>,
    /// What every other peer's subscribers listen to.
    remote: RwLock<HashMap<PeerId, RemoteInterest>>,
    /// Requests waiting on an inbox, by inbox, while federated.
    pending: Mutex<HashMap<Arc<str>, Pending>>,
    /// Picks the peer that serves a queue group without local members.
    queue_cursor: AtomicU64,
}

#[derive(Default)]
struct Registry {
    exact: HashMap<Arc<str>, Vec<Arc<Entry>>>,
    wildcard: Vec<Arc<Entry>>,
    queues: HashMap<Arc<str>, Arc<QueueGroup>>,
    count: usize,
    /// Subscriptions per announced interest key; see [`Entry::interest`].
    interest: HashMap<InterestKey, u32>,
    /// The sequence number of the last interest update sent to peers.
    interest_sequence: u64,
}

struct Entry {
    id: u64,
    pattern: Arc<str>,
    queue: Option<Arc<QueueGroup>>,
    sender: mpsc::Sender<Message>,
    slow: Arc<AtomicBool>,
    last_delivery: AtomicU64,
    scope: SubscriptionScope,
    client: bool,
    /// The interest this subscription announces to other peers, `None` when
    /// nothing it can match ever crosses peers for it.
    interest: Option<InterestKey>,
}

struct QueueGroup {
    name: Arc<str>,
    cursor: AtomicU64,
}

#[derive(Default)]
struct RemoteInterest {
    /// The last update applied; `None` before the first full one.
    sequence: Option<u64>,
    /// A [`FederationFrame::Resync`] went out and no full update came yet.
    resync_requested: bool,
    entries: HashMap<InterestKey, u32>,
}

/// A request that went to other peers.
struct Pending {
    /// The peers it was forwarded to.
    targets: Vec<PeerId>,
    /// Peers that answered with no responders, or whose link went down.
    refusals: mpsc::UnboundedSender<PeerId>,
}

/// Where a message to route comes from.
enum Arrival<'a> {
    /// A publisher of this peer.
    Local,
    /// Another peer, which chose `queues` for this peer to serve.
    Forwarded { queues: &'a [String] },
}

#[derive(Default)]
struct Routed {
    report: PublishReport,
    targets: Vec<PeerId>,
}

pub struct Subscription {
    id: u64,
    pattern: Arc<str>,
    queue_name: Option<Arc<str>>,
    slow: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Message>,
    broker: Weak<Inner>,
    closed: bool,
}

impl Default for Broker {
    fn default() -> Self {
        Self::new(BrokerConfig::default()).expect("valid default broker configuration")
    }
}

impl Broker {
    pub fn new(config: BrokerConfig) -> Result<Self> {
        if config.subscription_capacity == 0
            || config.subscription_capacity > tokio::sync::Semaphore::MAX_PERMITS
            || config.max_payload_bytes == 0
            || config.max_subject_bytes == 0
            || config.max_subscriptions == 0
        {
            return Err(Error::InvalidConfig("limits must be positive".into()));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                registry: RwLock::new(Registry::default()),
                next_id: AtomicU64::new(1),
                federated: AtomicBool::new(false),
                federation: RwLock::new(None),
                owner_router: RwLock::new(None),
                remote: RwLock::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                queue_cursor: AtomicU64::new(0),
            }),
        })
    }

    pub fn config(&self) -> &BrokerConfig {
        &self.inner.config
    }

    /// An in-process subscription with [`SubscriptionScope::Cluster`].
    pub fn subscribe(&self, subject: &str, queue: Option<&str>) -> Result<Subscription> {
        self.subscribe_with(subject, queue, SubscribeOptions::default())
    }

    pub fn subscribe_with(
        &self,
        subject: &str,
        queue: Option<&str>,
        options: SubscribeOptions,
    ) -> Result<Subscription> {
        validate_subject(subject, true, self.inner.config.max_subject_bytes)?;
        if queue.is_some_and(|q| {
            q.is_empty()
                || q.len() > 128
                || !q
                    .bytes()
                    .all(|c| c.is_ascii_graphic() && c != b'*' && c != b'>')
        }) {
            return Err(Error::InvalidQueue);
        }
        let mut registry = self
            .inner
            .registry
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if registry.count >= self.inner.config.max_subscriptions {
            return Err(Error::SubscriptionLimit);
        }
        let (sender, receiver) = mpsc::channel(self.inner.config.subscription_capacity);
        let queue = queue.map(|name| {
            registry
                .queues
                .entry(Arc::from(name))
                .or_insert_with(|| {
                    Arc::new(QueueGroup {
                        name: Arc::from(name),
                        cursor: AtomicU64::new(0),
                    })
                })
                .clone()
        });
        // Replies to this peer's own inboxes find it by their prefix alone.
        let own_inbox = self
            .inner
            .federation()
            .is_some_and(|federation| inbox_peer(subject) == Some(&*federation.local_peer()));
        let interest = (options.scope == SubscriptionScope::Cluster
            && !own_inbox
            && scope::pattern_may_federate(subject, options.client))
        .then(|| InterestKey {
            pattern: subject.to_owned(),
            queue: queue.as_ref().map(|queue| queue.name.to_string()),
            clients: options.client,
        });
        let entry = Arc::new(Entry {
            id: self.inner.next_id.fetch_add(1, Ordering::Relaxed),
            pattern: Arc::from(subject),
            queue,
            sender,
            slow: Arc::new(AtomicBool::new(false)),
            last_delivery: AtomicU64::new(0),
            scope: options.scope,
            client: options.client,
            interest: interest.clone(),
        });
        if subject.contains(['*', '>']) {
            registry.wildcard.push(entry.clone());
        } else {
            registry
                .exact
                .entry(entry.pattern.clone())
                .or_default()
                .push(entry.clone());
        }
        registry.count += 1;
        if let Some(key) = interest {
            self.inner.change_interest(&mut registry, key, true);
        }
        Ok(Subscription {
            id: entry.id,
            pattern: entry.pattern.clone(),
            queue_name: entry.queue.as_ref().map(|queue| queue.name.clone()),
            slow: entry.slow.clone(),
            receiver,
            broker: Arc::downgrade(&self.inner),
            closed: false,
        })
    }

    /// Delivers immediately to available subscribers. Full queues are disconnected
    /// and counted in the report; publisher latency never depends on consumption.
    pub fn publish(&self, subject: &str, payload: Bytes) -> Result<PublishReport> {
        self.publish_with_reply(subject, None, payload)
    }

    pub fn publish_with_reply(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<PublishReport> {
        self.publish_as(subject, reply, payload, None)
    }

    /// [`Broker::publish_with_reply`] on behalf of an authenticated remote
    /// principal, recorded in [`Message::sender`]. Only the QUIC listener
    /// and plugins name a sender; other in-process publishers leave it
    /// `None`. Peers forward the sender with the message, so every peer's
    /// permission checks see the same publisher.
    pub fn publish_as(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
        sender: Option<&str>,
    ) -> Result<PublishReport> {
        self.inner
            .route(subject, reply, payload, sender, Arrival::Local)
            .map(|routed| routed.report)
    }

    pub async fn request(
        &self,
        subject: &str,
        payload: Bytes,
        timeout: Duration,
    ) -> Result<Message> {
        self.request_as(subject, payload, timeout, None).await
    }

    /// [`Broker::request`] on behalf of an authenticated remote principal;
    /// see [`Broker::publish_as`].
    ///
    /// Fails with [`Error::NoResponders`] at once when neither this peer nor
    /// any other has a subscriber, and as soon as every peer the request
    /// went to answered that its subscribers are gone.
    pub async fn request_as(
        &self,
        subject: &str,
        payload: Bytes,
        timeout: Duration,
        sender: Option<&str>,
    ) -> Result<Message> {
        let mut random = [0u8; 16];
        aws_lc_rs::rand::fill(&mut random)
            .map_err(|_| Error::Storage("request inbox entropy unavailable".into()))?;
        let random = u128::from_le_bytes(random);
        let Some(federation) = self.inner.federation() else {
            let inbox = format!("_INBOX.{random:032x}");
            let mut subscription = self.subscribe(&inbox, None)?;
            let report = self.publish_as(subject, Some(&inbox), payload, sender)?;
            if report.delivered == 0 {
                return Err(Error::NoResponders);
            }
            return tokio::time::timeout(timeout, subscription.recv())
                .await
                .map_err(|_| Error::Timeout)?;
        };
        let inbox: Arc<str> =
            Arc::from(format!("_INBOX.{}.{random:032x}", federation.local_peer()));
        let mut subscription = self.subscribe(&inbox, None)?;
        let (refusals_sender, mut refusals) = mpsc::unbounded_channel();
        let _pending = PendingGuard::register(&self.inner, inbox.clone(), refusals_sender);
        let routed = self
            .inner
            .route(subject, Some(&*inbox), payload, sender, Arrival::Local)?;
        if routed.report.delivered == 0 && routed.targets.is_empty() {
            return Err(Error::NoResponders);
        }
        let remote_only = routed.report.delivered == 0;
        if remote_only {
            self.inner.set_targets(&inbox, &routed.targets);
        }
        let mut refused: Vec<PeerId> = Vec::new();
        tokio::time::timeout(timeout, async {
            loop {
                tokio::select! {
                    message = subscription.recv() => return message,
                    Some(peer) = refusals.recv(), if remote_only => {
                        if !refused.contains(&peer) {
                            refused.push(peer);
                        }
                        if routed.targets.iter().all(|target| refused.contains(target)) {
                            return Err(Error::NoResponders);
                        }
                    }
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout)?
    }

    pub fn subscription_count(&self) -> usize {
        self.inner
            .registry
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .count
    }

    /// The prefix of this broker's request inboxes: `_INBOX.` alone, or
    /// `_INBOX.<peer>.` with a federation, so replies from other peers find
    /// their way back by the prefix alone.
    pub fn inbox_prefix(&self) -> String {
        match self.inner.federation() {
            Some(federation) => format!("_INBOX.{}.", federation.local_peer()),
            None => "_INBOX.".to_owned(),
        }
    }

    /// Join a cluster's bus: from now on, subjects whose [`Scope`] allows it
    /// cross peers through `federation`, and this broker's interest is sent
    /// to every peer whose link is up.
    pub fn set_federation(&self, federation: Arc<dyn Federation>) {
        let registry = self
            .inner
            .registry
            .write()
            .unwrap_or_else(|p| p.into_inner());
        *self
            .inner
            .federation
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(federation.clone());
        self.inner.federated.store(true, Ordering::Release);
        for peer in federation.peers() {
            Inner::send_full_interest(&registry, &*federation, &peer);
        }
    }

    /// Leave the cluster's bus: back to serving only this broker's
    /// subscribers. Requests waiting on other peers time out.
    pub fn clear_federation(&self) {
        let _registry = self
            .inner
            .registry
            .write()
            .unwrap_or_else(|p| p.into_inner());
        *self
            .inner
            .federation
            .write()
            .unwrap_or_else(|p| p.into_inner()) = None;
        self.inner.federated.store(false, Ordering::Release);
        self.inner
            .remote
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    pub fn federation(&self) -> Option<Arc<dyn Federation>> {
        self.inner.federation()
    }

    /// Route [`Scope::Owner`] subjects with `router` while federated.
    pub fn set_owner_router(&self, router: Arc<dyn OwnerRouter>) {
        *self
            .inner
            .owner_router
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(router);
    }

    /// The link to `peer` came up (again): forget its old interest, which
    /// it sends anew, and send it all of ours.
    pub fn peer_up(&self, peer: &PeerId) {
        let Some(federation) = self.inner.federation() else {
            return;
        };
        self.inner
            .remote
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(peer);
        let registry = self
            .inner
            .registry
            .write()
            .unwrap_or_else(|p| p.into_inner());
        Inner::send_full_interest(&registry, &*federation, peer);
    }

    /// The link to `peer` went down: stop forwarding to it, and stop
    /// waiting for its replies.
    pub fn peer_down(&self, peer: &PeerId) {
        self.inner
            .remote
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(peer);
        let pending = self.inner.pending.lock().unwrap_or_else(|p| p.into_inner());
        for request in pending.values() {
            if request.targets.contains(peer) {
                let _ = request.refusals.send(peer.clone());
            }
        }
    }

    /// A frame `from` another peer, off the cluster's `bus` stream.
    pub fn handle_federation_frame(&self, from: &PeerId, frame: FederationFrame) {
        match frame {
            FederationFrame::Interest(update) => self.inner.receive_interest(from, update),
            FederationFrame::Message(message) => {
                let Some(federation) = self.inner.federation() else {
                    return;
                };
                let delivered = self
                    .inner
                    .route(
                        &message.subject,
                        message.reply.as_deref(),
                        message.payload,
                        message.sender.as_deref(),
                        Arrival::Forwarded {
                            queues: &message.queues,
                        },
                    )
                    .is_ok_and(|routed| routed.report.delivered > 0);
                if !delivered && let Some(reply) = message.reply {
                    federation.send(from, FederationFrame::NoResponders { reply });
                }
            }
            FederationFrame::NoResponders { reply } => {
                let pending = self.inner.pending.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(request) = pending.get(reply.as_str()) {
                    let _ = request.refusals.send(from.clone());
                }
            }
            FederationFrame::Resync => {
                let Some(federation) = self.inner.federation() else {
                    return;
                };
                let registry = self
                    .inner
                    .registry
                    .write()
                    .unwrap_or_else(|p| p.into_inner());
                Inner::send_full_interest(&registry, &*federation, from);
            }
        }
    }
}

impl Inner {
    fn federation(&self) -> Option<Arc<dyn Federation>> {
        if !self.federated.load(Ordering::Acquire) {
            return None;
        }
        self.federation
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn owner_router(&self) -> Option<Arc<dyn OwnerRouter>> {
        self.owner_router
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Count one subscription more or less for `key`, and tell the peers.
    /// Runs under the registry's write lock, which orders interest updates.
    fn change_interest(&self, registry: &mut Registry, key: InterestKey, add: bool) {
        let count = registry.interest.entry(key.clone()).or_insert(0);
        if add {
            *count = count.saturating_add(1);
        } else {
            *count = count.saturating_sub(1);
        }
        let count = *count;
        if count == 0 {
            registry.interest.remove(&key);
        }
        let Some(federation) = self.federation() else {
            return;
        };
        registry.interest_sequence += 1;
        let update = InterestUpdate {
            sequence: registry.interest_sequence,
            full: false,
            entries: vec![InterestEntry { key, count }],
        };
        for peer in federation.peers() {
            federation.send(&peer, FederationFrame::Interest(update.clone()));
        }
    }

    /// Send `peer` all of this broker's interest, as of the last update.
    fn send_full_interest(registry: &Registry, federation: &dyn Federation, peer: &PeerId) {
        let entries = registry
            .interest
            .iter()
            .map(|(key, &count)| InterestEntry {
                key: key.clone(),
                count,
            })
            .collect();
        federation.send(
            peer,
            FederationFrame::Interest(InterestUpdate {
                sequence: registry.interest_sequence,
                full: true,
                entries,
            }),
        );
    }

    fn receive_interest(&self, from: &PeerId, update: InterestUpdate) {
        let resync = {
            let mut remote = self.remote.write().unwrap_or_else(|p| p.into_inner());
            let interest = remote.entry(from.clone()).or_default();
            if update.full {
                interest.entries = update
                    .entries
                    .into_iter()
                    .filter(|entry| entry.count > 0)
                    .map(|entry| (entry.key, entry.count))
                    .collect();
                interest.sequence = Some(update.sequence);
                interest.resync_requested = false;
                false
            } else {
                let last = interest.sequence;
                match last {
                    Some(last) if update.sequence == last.wrapping_add(1) => {
                        for entry in update.entries {
                            if entry.count == 0 {
                                interest.entries.remove(&entry.key);
                            } else {
                                interest.entries.insert(entry.key, entry.count);
                            }
                        }
                        interest.sequence = Some(update.sequence);
                        false
                    }
                    // Already covered by a full update.
                    Some(last) if update.sequence <= last => false,
                    // A gap, or nothing to apply it to: ask once for all of it.
                    _ => !std::mem::replace(&mut interest.resync_requested, true),
                }
            }
        };
        if resync && let Some(federation) = self.federation() {
            federation.send(from, FederationFrame::Resync);
        }
    }

    fn set_targets(&self, inbox: &str, targets: &[PeerId]) {
        if let Some(request) = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(inbox)
        {
            request.targets = targets.to_vec();
        }
    }

    /// Deliver to this broker's subscribers and, for a local publisher,
    /// forward to the peers that should see the message.
    fn route(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
        sender: Option<&str>,
        arrival: Arrival<'_>,
    ) -> Result<Routed> {
        validate_subject(subject, false, self.config.max_subject_bytes)?;
        if let Some(reply) = reply {
            validate_subject(reply, false, self.config.max_subject_bytes)?;
        }
        if payload.len() > self.config.max_payload_bytes {
            return Err(Error::PayloadTooLarge {
                size: payload.len(),
                limit: self.config.max_payload_bytes,
            });
        }
        // Only a local publisher's messages travel on: one hop, no loops.
        let federation = match arrival {
            Arrival::Local => self.federation(),
            Arrival::Forwarded { .. } => None,
        };
        let scope = match (&federation, &arrival) {
            (None, Arrival::Local) => Scope::Cluster,
            _ => subject_scope(subject),
        };
        if let Some(federation) = &federation
            && scope == Scope::Owner
            && let Some(router) = self.owner_router()
            && let Some(peer) = router.route(subject)
            && peer != federation.local_peer()
        {
            federation.send(
                &peer,
                FederationFrame::Message(ForwardedMessage {
                    subject: subject.to_owned(),
                    reply: reply.map(str::to_owned),
                    payload,
                    sender: sender.map(str::to_owned),
                    origin: federation.local_peer().to_string(),
                    queues: Vec::new(),
                }),
            );
            return Ok(Routed {
                report: PublishReport {
                    forwarded: 1,
                    ..PublishReport::default()
                },
                targets: vec![peer],
            });
        }
        let mut report = PublishReport::default();
        // Queue groups served here, which no other peer serves again.
        let mut served: Vec<Arc<str>> = Vec::new();
        {
            let registry = self.registry.read().unwrap_or_else(|p| p.into_inner());
            let exact = registry
                .exact
                .get(subject)
                .map(Vec::as_slice)
                .unwrap_or_default();
            // No allocation when an exact subject has no listeners and there are no
            // wildcard listeners. Ordinary fanout requires no temporary candidate list.
            if exact.is_empty() && registry.wildcard.is_empty() {
                if federation.is_none() {
                    return Ok(Routed::default());
                }
            } else {
                let message = Message {
                    subject: exact
                        .first()
                        .map_or_else(|| Arc::from(subject), |entry| entry.pattern.clone()),
                    reply: reply.map(Arc::from),
                    payload: payload.clone(),
                    sender: sender.map(Arc::from),
                };
                let mut queues: HashMap<&str, Vec<&Entry>> = HashMap::new();
                for entry in exact.iter().chain(
                    registry
                        .wildcard
                        .iter()
                        .filter(|e| subject_matches(&e.pattern, subject)),
                ) {
                    if entry.slow.load(Ordering::Acquire)
                        || entry.sender.is_closed()
                        || !eligible(entry, &arrival, scope)
                    {
                        continue;
                    }
                    if let Some(queue) = &entry.queue {
                        queues.entry(&queue.name).or_default().push(entry);
                    } else {
                        deliver(entry, &message, &mut report);
                    }
                }
                for candidates in queues.values() {
                    let group = candidates[0].queue.as_ref().unwrap();
                    let stamp = group.cursor.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                    // Least recently served is fair even when different subjects
                    // produce overlapping candidate sets for the same queue name.
                    let preferred = candidates
                        .iter()
                        .min_by_key(|entry| entry.last_delivery.load(Ordering::Relaxed))
                        .unwrap();
                    preferred.last_delivery.store(stamp, Ordering::Relaxed);
                    let mut done = deliver(preferred, &message, &mut report);
                    if !done {
                        for entry in candidates.iter().filter(|entry| entry.id != preferred.id) {
                            if entry.slow.load(Ordering::Acquire) {
                                continue;
                            }
                            entry.last_delivery.store(stamp, Ordering::Relaxed);
                            if deliver(entry, &message, &mut report) {
                                done = true;
                                break;
                            }
                        }
                    }
                    if done && federation.is_some() {
                        served.push(group.name.clone());
                    }
                }
            }
        }
        let Some(federation) = federation else {
            return Ok(Routed {
                report,
                targets: Vec::new(),
            });
        };
        if !matches!(scope, Scope::Cluster | Scope::Origin) {
            return Ok(Routed {
                report,
                targets: Vec::new(),
            });
        }
        let local = federation.local_peer();
        // Peers to forward to: whether their plain subscribers want it, and
        // the queue groups they serve.
        let mut plans: HashMap<PeerId, (bool, Vec<String>)> = HashMap::new();
        match inbox_peer(subject) {
            // A reply goes to the peer named by its inbox, interest or not.
            Some(peer) if peer == &*local => {}
            Some(peer) if federation.peers().iter().any(|p| &**p == peer) => {
                plans.insert(Arc::from(peer), (true, Vec::new()));
            }
            _ => {
                let remote = self.remote.read().unwrap_or_else(|p| p.into_inner());
                let clients_only = scope == Scope::Origin;
                let mut groups: HashMap<&str, Vec<(&PeerId, u64)>> = HashMap::new();
                for (peer, interest) in remote.iter() {
                    for (key, &count) in &interest.entries {
                        if count == 0
                            || (clients_only && !key.clients)
                            || !subject_matches(&key.pattern, subject)
                        {
                            continue;
                        }
                        match &key.queue {
                            None => plans.entry(peer.clone()).or_default().0 = true,
                            Some(group) if !served.iter().any(|name| **name == **group) => {
                                let weights = groups.entry(group.as_str()).or_default();
                                match weights.last_mut() {
                                    Some((last, weight)) if Arc::ptr_eq(*last, peer) => {
                                        *weight += u64::from(count);
                                    }
                                    _ => weights.push((peer, u64::from(count))),
                                }
                            }
                            Some(_) => {}
                        }
                    }
                }
                // One member across the cluster: a peer chosen by its share
                // of the group's members.
                for (group, weights) in groups {
                    let total: u64 = weights.iter().map(|(_, weight)| weight).sum();
                    let mut pick = self.queue_cursor.fetch_add(1, Ordering::Relaxed) % total;
                    for (peer, weight) in weights {
                        if pick < weight {
                            plans
                                .entry(peer.clone())
                                .or_default()
                                .1
                                .push(group.to_owned());
                            break;
                        }
                        pick -= weight;
                    }
                }
            }
        }
        let mut targets = Vec::with_capacity(plans.len());
        for (peer, (_, queues)) in plans {
            federation.send(
                &peer,
                FederationFrame::Message(ForwardedMessage {
                    subject: subject.to_owned(),
                    reply: reply.map(str::to_owned),
                    payload: payload.clone(),
                    sender: sender.map(str::to_owned),
                    origin: local.to_string(),
                    queues,
                }),
            );
            targets.push(peer);
        }
        report.forwarded = targets.len();
        Ok(Routed { report, targets })
    }
}

/// Whether a message that `arrival` brings may reach `entry`. A local
/// publisher's messages reach every subscriber, as on a single Rift.
fn eligible(entry: &Entry, arrival: &Arrival<'_>, scope: Scope) -> bool {
    let Arrival::Forwarded { queues } = arrival else {
        return true;
    };
    entry.scope == SubscriptionScope::Cluster
        && match scope {
            Scope::Cluster | Scope::Owner => true,
            // In-process code sees only its own peer's events, so plugin
            // code across the cluster sees each event once.
            Scope::Origin => entry.client,
            Scope::Local | Scope::Applied => false,
        }
        && entry.queue.as_ref().is_none_or(|group| {
            scope == Scope::Owner || queues.iter().any(|queue| *queue == *group.name)
        })
}

/// The peer named by an `_INBOX.<peer>.<id>` reply subject.
fn inbox_peer(subject: &str) -> Option<&str> {
    let mut tokens = subject.strip_prefix("_INBOX.")?.split('.');
    let peer = tokens.next()?;
    tokens.next().is_some().then_some(peer)
}

/// Unregisters a [`Pending`] request when the request ends.
struct PendingGuard<'a> {
    inner: &'a Inner,
    inbox: Arc<str>,
}

impl<'a> PendingGuard<'a> {
    fn register(
        inner: &'a Inner,
        inbox: Arc<str>,
        refusals: mpsc::UnboundedSender<PeerId>,
    ) -> Self {
        inner
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                inbox.clone(),
                Pending {
                    targets: Vec::new(),
                    refusals,
                },
            );
        Self { inner, inbox }
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.inbox);
    }
}

fn deliver(entry: &Entry, message: &Message, report: &mut PublishReport) -> bool {
    match entry.sender.try_send(message.clone()) {
        Ok(()) => {
            report.delivered += 1;
            true
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            if !entry.slow.swap(true, Ordering::AcqRel) {
                report.slow_consumers += 1;
            }
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

impl Subscription {
    pub async fn recv(&mut self) -> Result<Message> {
        self.check_status()?;
        let message = self.receiver.recv().await.ok_or(Error::Closed)?;
        self.check_status()?;
        Ok(message)
    }

    pub fn try_recv(&mut self) -> Result<Option<Message>> {
        self.check_status()?;
        match self.receiver.try_recv() {
            Ok(message) => {
                self.check_status()?;
                Ok(Some(message))
            }
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(Error::Closed),
        }
    }

    fn check_status(&mut self) -> Result<()> {
        if self.closed {
            return Err(Error::Closed);
        }
        if self.slow.load(Ordering::Acquire) {
            self.close();
            return Err(Error::SlowConsumer);
        }
        Ok(())
    }

    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.receiver.close();
        if let Some(broker) = self.broker.upgrade() {
            let mut registry = broker.registry.write().unwrap_or_else(|p| p.into_inner());
            let removed = if self.pattern.contains(['*', '>']) {
                let position = registry.wildcard.iter().position(|e| e.id == self.id);
                position.map(|index| registry.wildcard.remove(index))
            } else if let Some(entries) = registry.exact.get_mut(&self.pattern) {
                let removed = entries
                    .iter()
                    .position(|e| e.id == self.id)
                    .map(|index| entries.remove(index));
                if entries.is_empty() {
                    registry.exact.remove(&self.pattern);
                }
                removed
            } else {
                None
            };
            // The entry goes before its queue group is checked for other members.
            let interest = removed.and_then(|entry| entry.interest.clone());
            registry.count -= 1;
            if let Some(name) = &self.queue_name
                && registry
                    .queues
                    .get(name)
                    .is_some_and(|queue| Arc::strong_count(queue) == 1)
            {
                registry.queues.remove(name);
            }
            if let Some(key) = interest {
                broker.change_interest(&mut registry, key, false);
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.close();
    }
}

pub(crate) fn validate_subject(subject: &str, wildcards: bool, max_bytes: usize) -> Result<()> {
    if subject.is_empty()
        || subject.len() > max_bytes
        || subject.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::InvalidSubject(subject.into()));
    }
    let mut tokens = subject.split('.').peekable();
    while let Some(token) = tokens.next() {
        if token.is_empty()
            || (token.contains(['*', '>']) && (!wildcards || (token != "*" && token != ">")))
            || (token == ">" && tokens.peek().is_some())
        {
            return Err(Error::InvalidSubject(subject.into()));
        }
    }
    Ok(())
}

pub(crate) fn subject_matches(pattern: &str, subject: &str) -> bool {
    let mut subject = subject.split('.');
    for token in pattern.split('.') {
        let Some(actual) = subject.next() else {
            return false;
        };
        if token == ">" {
            return true;
        }
        if token != "*" && token != actual {
            return false;
        }
    }
    subject.next().is_none()
}

#[cfg(test)]
mod federation_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[tokio::test]
    async fn exact_wildcard_and_shared_payload() {
        let broker = Broker::default();
        let mut exact = broker.subscribe("orders.created", None).unwrap();
        let mut star = broker.subscribe("orders.*", None).unwrap();
        let mut tail = broker.subscribe("orders.>", None).unwrap();
        let payload = Bytes::from(vec![42; 256]);
        assert_eq!(
            broker
                .publish("orders.created", payload.clone())
                .unwrap()
                .delivered,
            3
        );
        for message in [
            exact.recv().await.unwrap(),
            star.recv().await.unwrap(),
            tail.recv().await.unwrap(),
        ] {
            assert_eq!(message.payload.as_ptr(), payload.as_ptr());
        }
        assert_eq!(broker.publish("orders", Bytes::new()).unwrap().delivered, 0);
        assert_eq!(
            broker
                .publish("orders.created.local", Bytes::new())
                .unwrap()
                .delivered,
            1
        );
    }

    #[tokio::test]
    async fn queues_balance_and_fanout_receives_every_message() {
        let broker = Broker::default();
        let mut a = broker.subscribe("jobs.*", Some("workers")).unwrap();
        let mut b = broker.subscribe("jobs.run", Some("workers")).unwrap();
        let mut fanout = broker.subscribe("jobs.run", None).unwrap();
        for _ in 0..20 {
            assert_eq!(
                broker.publish("jobs.run", Bytes::new()).unwrap().delivered,
                2
            );
        }
        for _ in 0..10 {
            a.recv().await.unwrap();
            b.recv().await.unwrap();
        }
        assert!(a.try_recv().unwrap().is_none());
        assert!(b.try_recv().unwrap().is_none());
        for _ in 0..20 {
            fanout.recv().await.unwrap();
        }
    }

    #[test]
    fn interleaved_queue_groups_do_not_starve_workers() {
        let broker = Broker::default();
        let mut a = broker.subscribe("a", Some("group_a")).unwrap();
        let mut b = broker.subscribe("a", Some("group_a")).unwrap();
        let mut c = broker.subscribe("b", Some("group_a")).unwrap();
        let mut d = broker.subscribe("b", Some("group_a")).unwrap();
        for _ in 0..20 {
            broker.publish("a", Bytes::new()).unwrap();
            broker.publish("b", Bytes::new()).unwrap();
        }
        for sub in [&mut a, &mut b, &mut c, &mut d] {
            for _ in 0..10 {
                assert!(sub.try_recv().unwrap().is_some());
            }
            assert!(sub.try_recv().unwrap().is_none());
        }
        drop((a, b, c, d));
        assert!(broker.inner.registry.read().unwrap().queues.is_empty());
    }

    #[tokio::test]
    async fn slow_consumer_is_explicit_and_queue_can_fail_over() {
        let broker = Broker::new(BrokerConfig {
            subscription_capacity: 1,
            ..Default::default()
        })
        .unwrap();
        let mut slow = broker.subscribe("x", None).unwrap();
        broker.publish("x", Bytes::new()).unwrap();
        assert_eq!(broker.publish("x", Bytes::new()).unwrap().slow_consumers, 1);
        assert!(matches!(slow.recv().await, Err(Error::SlowConsumer)));
        assert!(matches!(slow.recv().await, Err(Error::Closed)));
        assert_eq!(broker.subscription_count(), 0);
        let mut a = broker.subscribe("q", Some("group")).unwrap();
        let mut b = broker.subscribe("q", Some("group")).unwrap();
        broker.publish("q", Bytes::new()).unwrap();
        broker.publish("q", Bytes::new()).unwrap();
        b.recv().await.unwrap();
        let report = broker.publish("q", Bytes::new()).unwrap();
        assert_eq!(
            report,
            PublishReport {
                delivered: 1,
                slow_consumers: 1,
                forwarded: 0,
            }
        );
        assert!(matches!(a.recv().await, Err(Error::SlowConsumer)));
        b.recv().await.unwrap();
    }

    #[tokio::test]
    async fn request_reply_timeout_and_inbox_cleanup() {
        let broker = Broker::default();
        let mut server = broker.subscribe("echo", None).unwrap();
        let responder = broker.clone();
        let task = tokio::spawn(async move {
            let request = server.recv().await.unwrap();
            responder
                .publish(request.reply.as_deref().unwrap(), request.payload)
                .unwrap();
        });
        assert_eq!(
            broker
                .request("echo", Bytes::from_static(b"hello"), Duration::from_secs(1))
                .await
                .unwrap()
                .payload,
            "hello"
        );
        task.await.unwrap();
        assert_eq!(broker.subscription_count(), 0);
        assert!(matches!(
            broker
                .request("absent", Bytes::new(), Duration::from_secs(1))
                .await,
            Err(Error::NoResponders)
        ));
        let _idle = broker.subscribe("idle", None).unwrap();
        assert!(matches!(
            broker
                .request("idle", Bytes::new(), Duration::from_millis(1))
                .await,
            Err(Error::Timeout)
        ));
        assert_eq!(broker.subscription_count(), 1);
    }

    #[tokio::test]
    async fn dropping_last_broker_wakes_waiting_subscriber() {
        let broker = Broker::default();
        let mut subscription = broker.subscribe("shutdown", None).unwrap();
        let task = tokio::spawn(async move { subscription.recv().await });
        tokio::task::yield_now().await;
        drop(broker);
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(Error::Closed)));
    }

    #[test]
    fn validation_limits_and_drop_cleanup() {
        assert!(
            Broker::new(BrokerConfig {
                subscription_capacity: usize::MAX,
                ..Default::default()
            })
            .is_err()
        );
        let broker = Broker::new(BrokerConfig {
            max_subscriptions: 1,
            max_payload_bytes: 2,
            ..Default::default()
        })
        .unwrap();
        for invalid in ["", ".x", "x.", "x..y", "x.>.y", "x.a*", "x y"] {
            assert!(broker.subscribe(invalid, None).is_err());
        }
        for invalid in ["x.*", "x.>"] {
            assert!(broker.publish(invalid, Bytes::new()).is_err());
        }
        assert!(broker.publish("x", Bytes::from_static(b"123")).is_err());
        let sub = broker.subscribe("x", None).unwrap();
        assert!(matches!(
            broker.subscribe("y", None),
            Err(Error::SubscriptionLimit)
        ));
        drop(sub);
        assert_eq!(broker.subscription_count(), 0);
        assert!(broker.subscribe("y", None).is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_publishers_deliver_each_message_once() {
        let broker = Broker::new(BrokerConfig {
            subscription_capacity: 4096,
            ..Default::default()
        })
        .unwrap();
        let mut sub = broker.subscribe("parallel", None).unwrap();
        let mut tasks = Vec::new();
        for publisher in 0..4 {
            let broker = broker.clone();
            tasks.push(tokio::spawn(async move {
                for sequence in 0..500 {
                    broker
                        .publish("parallel", Bytes::from(format!("{publisher}:{sequence}")))
                        .unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let mut seen = HashSet::new();
        for _ in 0..2000 {
            assert!(seen.insert(sub.recv().await.unwrap().payload));
        }
        assert_eq!(seen.len(), 2000);
    }
}
