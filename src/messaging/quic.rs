//! Authenticated QUIC transport for the shared broker and retained streams.
//!
//! Connections use TLS 1.3 and ALPN `rift-messaging/1`. See
//! `docs/messaging-protocol.md` for the language-independent wire protocol.
//! The client, the frame codec and the TLS setup live in the `rift-messaging`
//! crate, so backend plugins can use them without this crate; they are
//! re-exported here.
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    io,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::{BufMut, BytesMut};
use hmac::{Hmac, KeyInit, Mac};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rift_messaging::frame::*;
pub use rift_messaging::{
    ALPN, Client, MAX_FRAME_BYTES, MAX_PAYLOAD_BYTES, Publisher, RemoteSubscription, Result,
    client_config_from_der, read_certificates, server_config_from_der, tls_client_config,
    tls_server_config,
};
use sha2::{Digest, Sha256};
use tokio::{
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
    time::timeout,
};

use super::{
    Broker,
    stream::{ConsumerConfig, Stream},
};

/// Authenticated plugin identity. Tokens should come from environment variables.
/// Tokens are deliberately excluded from Debug output.
#[derive(Clone)]
pub struct Identity {
    pub name: String,
    pub token: String,
    pub publish: Vec<String>,
    pub subscribe: Vec<String>,
    pub control: bool,
}
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("name", &self.name)
            .field("publish", &self.publish)
            .field("subscribe", &self.subscribe)
            .field("control", &self.control)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub identities: Vec<Identity>,
    /// Authenticated connections of all identities except node agents, whose
    /// one session each never competes with plugins for a slot.
    pub max_connections: usize,
    /// Connections from one IPv4 address or IPv6 /64, at any stage.
    pub max_connections_per_address: usize,
    /// Authenticated connections of one identity other than a node agent.
    pub max_connections_per_identity: usize,
    pub max_streams_per_connection: usize,
    pub handshake_timeout: Duration,
    pub io_timeout: Duration,
}
impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            identities: vec![],
            max_connections: 1024,
            max_connections_per_address: 256,
            max_connections_per_identity: 256,
            max_streams_per_connection: 128,
            handshake_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(30),
        }
    }
}
impl AuthConfig {
    fn validate(&self) -> Result<()> {
        if self.max_connections == 0
            || self.max_connections_per_address == 0
            || self.max_connections_per_identity == 0
            || self.max_streams_per_connection == 0
            || self.max_streams_per_connection > 65535
            || self.handshake_timeout.is_zero()
            || self.io_timeout.is_zero()
        {
            return Err(invalid("invalid QUIC resource limits"));
        }
        let mut names = HashSet::new();
        let mut tokens = HashSet::new();
        for identity in &self.identities {
            if !valid_name(&identity.name)
                || !names.insert(&identity.name)
                || identity.token.is_empty()
                || identity.token.len() > MAX_STRING
                || !tokens.insert(&identity.token)
            {
                return Err(invalid(
                    "identity names and tokens must be nonempty and unique",
                ));
            }
            for pattern in identity.publish.iter().chain(&identity.subscribe) {
                validate_pattern(pattern)?;
            }
        }
        Ok(())
    }
    fn authenticate(&self, token: &str) -> Result<Identity> {
        // HMAC verification compares fixed-size tags in constant time, without
        // exposing tokens or leaking their matching prefix through comparison.
        let tag = token_mac(token).finalize().into_bytes();
        let mut found = None;
        for identity in &self.identities {
            if token_mac(&identity.token).verify_slice(&tag).is_ok() {
                found = Some(identity.clone());
            }
        }
        found.ok_or_else(|| denied("authentication failed"))
    }
}
fn token_mac(token: &str) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes())
        .expect("HMAC accepts arbitrary key lengths");
    mac.update(b"rift-messaging/1 authentication");
    mac
}
impl Identity {
    /// `rift.node.<n>.*` belongs to the agent authenticated as `node.<n>`;
    /// commands come from the in-process controller only.
    fn node_scope(&self, pattern: &str) -> bool {
        self.name
            .strip_prefix("node.")
            .is_some_and(|node| covers(&format!("rift.node.{node}.>"), pattern))
    }
    fn can_publish(&self, subject: &str, reply: Option<&str>) -> Result<()> {
        validate_literal(subject)?;
        if reserved(subject, "rift.events")
            // Only the proxy publishes permission updates and the change log.
            || reserved(subject, "rift.perms.update")
            || reserved(subject, rift_permissions::wire::CHANGES)
            || (!self.control && reserved(subject, "rift.control"))
            || (reserved(subject, "rift.node") && !self.node_scope(subject))
            || !self.publish.iter().any(|p| covers(p, subject))
        {
            return Err(denied("publication not permitted"));
        }
        if let Some(reply) = reply {
            validate_literal(reply)?;
            if !reply.starts_with("_INBOX.") || !self.subscribe.iter().any(|p| covers(p, reply)) {
                return Err(denied("reply must be an authorized _INBOX subject"));
            }
        }
        Ok(())
    }
    fn can_subscribe(&self, pattern: &str) -> Result<()> {
        validate_pattern(pattern)?;
        if (!self.control
            && (overlaps(pattern, "rift.control.>") || covers(pattern, "rift.control")))
            // Node commands carry secrets such as the forwarding secret.
            || (!self.control
                && !self.node_scope(pattern)
                && (overlaps(pattern, "rift.node.>") || covers(pattern, "rift.node")))
            // Permission requests name players and carry `/perms` commands;
            // only the proxy (or a control principal) may receive them.
            || (!self.control
                && rift_permissions::wire::REQUESTS
                    .iter()
                    .any(|subject| overlaps(pattern, subject)))
            || !self.subscribe.iter().any(|p| covers(p, pattern))
        {
            return Err(denied("subscription not permitted"));
        }
        Ok(())
    }
    fn consumer_name(&self, name: &str) -> Result<String> {
        if !valid_name(name) {
            return Err(invalid("invalid consumer name"));
        }
        // Hash length-delimited components to fit the durable store's 128-byte
        // name limit while isolating identically named consumers per principal.
        let mut digest = Sha256::new();
        digest.update((self.name.len() as u64).to_be_bytes());
        digest.update(self.name.as_bytes());
        digest.update(name.as_bytes());
        let hex: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(format!("remote_{hex}"))
    }
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 96
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
fn reserved(subject: &str, prefix: &str) -> bool {
    subject == prefix
        || subject
            .strip_prefix(prefix)
            .is_some_and(|s| s.starts_with('.'))
}
/// Whether every subject matched by requested is also matched by allowed.
fn covers(allowed: &str, requested: &str) -> bool {
    let mut a = allowed.split('.');
    let mut r = requested.split('.');
    loop {
        match (a.next(), r.next()) {
            (None, None) => return true,
            (Some(">"), Some(_)) => return true,
            (Some("*"), Some(x)) if x != ">" => {}
            (Some(x), Some(y)) if x == y && y != ">" => {}
            _ => return false,
        }
    }
}
/// Whether two valid patterns have at least one common literal subject.
fn overlaps(left: &str, right: &str) -> bool {
    let mut l = left.split('.');
    let mut r = right.split('.');
    loop {
        match (l.next(), r.next()) {
            (None, None) => return true,
            (Some(">"), Some(_)) | (Some(_), Some(">")) => return true,
            (Some(a), Some(b)) if a == "*" || b == "*" || a == b => {}
            _ => return false,
        }
    }
}

pub struct Server {
    endpoint: Endpoint,
    task: JoinHandle<()>,
}
impl Server {
    pub fn bind(
        addr: SocketAddr,
        tls: quinn::ServerConfig,
        broker: Broker,
        auth: AuthConfig,
    ) -> Result<Self> {
        Self::bind_with_streams(addr, tls, broker, auth, HashMap::new())
    }
    pub fn bind_with_streams(
        addr: SocketAddr,
        mut tls: quinn::ServerConfig,
        broker: Broker,
        auth: AuthConfig,
        streams: HashMap<String, Stream>,
    ) -> Result<Self> {
        auth.validate()?;
        for name in streams.keys() {
            if !valid_name(name) {
                return Err(invalid("invalid stream name"));
            }
        }
        tls.transport_config(Arc::new(rift_messaging::transport_config(
            auth.max_streams_per_connection as u32,
        )));
        let endpoint = Endpoint::server(tls, addr)?;
        let listener = endpoint.clone();
        let context = Arc::new(Context {
            broker,
            sessions: Arc::new(Semaphore::new(auth.max_connections)),
            pending: Arc::new(Semaphore::new(MAX_PENDING)),
            by_address: Tally::default(),
            pending_by_address: Tally::default(),
            by_identity: Tally::default(),
            auth,
            streams,
            node_sessions: std::sync::Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(0),
        });
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    incoming = listener.accept() => {
                        let Some(incoming) = incoming else { break };
                        // An unvalidated source address may be spoofed. Answer
                        // with a stateless Retry, which only a real peer can
                        // complete, before the connection holds any slot.
                        if !incoming.remote_address_validated() {
                            if incoming.may_retry() {
                                let _ = incoming.retry();
                            } else {
                                incoming.ignore();
                            }
                            continue;
                        }
                        let Some(admission) = context.admit(incoming.remote_address().ip()) else {
                            incoming.refuse();
                            continue;
                        };
                        let context = context.clone();
                        tasks.spawn(async move {
                            if let Ok(Ok(connection)) = timeout(context.auth.handshake_timeout, incoming).await {
                                serve_connection(connection, context, admission).await;
                            }
                        });
                    }
                    _ = tasks.join_next(), if !tasks.is_empty() => {}
                }
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        });
        Ok(Self { endpoint, task })
    }
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.local_addr()
    }
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
    pub async fn shutdown(&self) {
        self.endpoint.close(0u32.into(), b"server shutdown");
        self.task.abort();
        let _ = timeout(Duration::from_secs(2), self.endpoint.wait_idle()).await;
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.endpoint.close(0u32.into(), b"server shutdown");
        self.task.abort();
    }
}
/// Connections still in their TLS handshake or authentication, in total and
/// per address. They have their own pool, so authenticated plugins filling
/// `max_connections` never stop a node agent from authenticating.
const MAX_PENDING: usize = 256;
const MAX_PENDING_PER_ADDRESS: usize = 16;

struct Context {
    broker: Broker,
    auth: AuthConfig,
    streams: HashMap<String, Stream>,
    /// Authenticated connections of identities other than node agents.
    sessions: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    by_address: Tally<IpAddr>,
    pending_by_address: Tally<IpAddr>,
    by_identity: Tally<String>,
    /// The one live connection of each `node.*` principal, by session id.
    /// Node commands are addressed to the principal, so a second session must
    /// never receive them alongside the first.
    node_sessions: std::sync::Mutex<HashMap<String, (u64, Connection)>>,
    next_session: AtomicU64,
}
/// Slots held by a connection from its validated first packet on.
struct Admission {
    _address: Claim<IpAddr>,
    /// Released once the connection authenticates.
    pending: (tokio::sync::OwnedSemaphorePermit, Claim<IpAddr>),
}
/// Slots held by an authenticated connection of an identity other than a node.
type Session = (tokio::sync::OwnedSemaphorePermit, Claim<String>);
impl Context {
    fn admit(&self, ip: IpAddr) -> Option<Admission> {
        let address = address_key(ip);
        let connection = self
            .by_address
            .claim(address, self.auth.max_connections_per_address)?;
        let pending_address = self
            .pending_by_address
            .claim(address, MAX_PENDING_PER_ADDRESS)?;
        let pending = self.pending.clone().try_acquire_owned().ok()?;
        Some(Admission {
            _address: connection,
            pending: (pending, pending_address),
        })
    }
    fn admit_session(&self, identity: &str) -> Result<Session> {
        let claim = self
            .by_identity
            .claim(identity.to_owned(), self.auth.max_connections_per_identity)
            .ok_or_else(|| denied("too many connections for this identity"))?;
        let permit = self
            .sessions
            .clone()
            .try_acquire_owned()
            .map_err(|_| denied("too many connections"))?;
        Ok((permit, claim))
    }
    /// Register `connection` as the only session of node principal `name`,
    /// closing an older one: after a silent network drop the agent must be
    /// able to reconnect before the old connection's idle timeout.
    fn claim_node_session(self: &Arc<Self>, name: &str, connection: &Connection) -> NodeSession {
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let previous = self
            .node_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned(), (id, connection.clone()));
        if let Some((_, previous)) = previous {
            previous.close(4u32.into(), b"replaced by a newer session of this node");
        }
        NodeSession {
            context: self.clone(),
            name: name.to_owned(),
            id,
        }
    }
}
/// Releases a node principal's session slot when its connection ends.
struct NodeSession {
    context: Arc<Context>,
    name: String,
    id: u64,
}
impl Drop for NodeSession {
    fn drop(&mut self) {
        let mut sessions = self
            .context
            .node_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if sessions
            .get(&self.name)
            .is_some_and(|(id, _)| *id == self.id)
        {
            sessions.remove(&self.name);
        }
    }
}
/// Live connections by key, each held by a [`Claim`].
struct Tally<K: Eq + Hash + Clone>(Arc<std::sync::Mutex<HashMap<K, usize>>>);
impl<K: Eq + Hash + Clone> Default for Tally<K> {
    fn default() -> Self {
        Self(Arc::default())
    }
}
impl<K: Eq + Hash + Clone> Tally<K> {
    /// Count one more connection for `key`, unless it already has `limit`.
    fn claim(&self, key: K, limit: usize) -> Option<Claim<K>> {
        let mut counts = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let count = counts.entry(key.clone()).or_default();
        if *count >= limit {
            return None;
        }
        *count += 1;
        Some(Claim {
            counts: self.0.clone(),
            key,
        })
    }
}
struct Claim<K: Eq + Hash + Clone> {
    counts: Arc<std::sync::Mutex<HashMap<K, usize>>>,
    key: K,
}
impl<K: Eq + Hash + Clone> Drop for Claim<K> {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = counts.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.key);
            }
        }
    }
}
/// The key connection limits count by. An IPv6 host usually controls a
/// whole /64, so the /64 counts as one address.
fn address_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(ip) => IpAddr::V6(Ipv6Addr::from_bits(ip.to_bits() & !u128::from(u64::MAX))),
        ip => ip,
    }
}
async fn serve_connection(connection: Connection, context: Arc<Context>, admission: Admission) {
    let Admission { _address, pending } = admission;
    let auth_result = timeout(context.auth.handshake_timeout, async {
        let (mut send, mut recv) = connection.accept_bi().await.map_err(other)?;
        let mut reader = FrameReader::with_limit(MAX_STRING + 3);
        let mut frame = reader.read(&mut recv).await?;
        if take_u8(&mut frame)? != AUTH {
            return Err(denied("first stream must authenticate"));
        }
        let token = take_string(&mut frame)?;
        exhausted(&frame)?;
        let admitted = context.auth.authenticate(&token).and_then(|identity| {
            // A node agent has at most one session, replaced on reconnect, so
            // it needs no shared slot and plugins can never crowd it out.
            let session = if identity.name.starts_with("node.") {
                None
            } else {
                Some(context.admit_session(&identity.name)?)
            };
            Ok((identity, session))
        });
        match admitted {
            Ok((identity, session)) => {
                let node_session = session
                    .is_none()
                    .then(|| context.claim_node_session(&identity.name, &connection));
                write_simple(&mut send, OK).await?;
                send.finish().map_err(other)?;
                Ok((identity, node_session, session))
            }
            Err(error) => {
                let _ = write_error(&mut send, &error).await;
                let _ = send.finish();
                // Let the client read why before the connection closes.
                let _ = timeout(Duration::from_secs(1), send.stopped()).await;
                Err(error)
            }
        }
    })
    .await;
    drop(pending);
    let Ok(Ok((identity, _node_session, _session))) = auth_result else {
        connection.close(1u32.into(), b"authentication failed");
        return;
    };
    let identity = Arc::new(identity);
    let permits = Arc::new(Semaphore::new(context.auth.max_streams_per_connection));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            incoming = connection.accept_bi() => {
                let Ok((mut send, mut recv)) = incoming else { break };
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    let _ = send.reset(2u32.into()); let _ = recv.stop(2u32.into()); continue;
                };
                let context = context.clone(); let identity = identity.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve_stream(&mut send, &mut recv, context.clone(), identity).await {
                        let _ = timeout(context.auth.io_timeout, write_error(&mut send, &error)).await;
                    }
                    let _ = send.finish(); let _ = recv.stop(0u32.into());
                });
            }
            _ = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}
async fn serve_stream(
    send: &mut SendStream,
    recv: &mut RecvStream,
    context: Arc<Context>,
    identity: Arc<Identity>,
) -> Result<()> {
    let mut reader = FrameReader::default();
    let mut first = true;
    loop {
        let mut frame = if first {
            first = false;
            timeout(context.auth.handshake_timeout, reader.read(recv))
                .await
                .map_err(timed_out)??
        } else {
            reader.read_bounded(recv, context.auth.io_timeout).await?
        };
        let opcode = take_u8(&mut frame)?;
        match opcode {
            PUBLISH | PUBLISH_ACK => {
                let message = decode_message(frame)?;
                identity.can_publish(&message.subject, message.reply.as_deref())?;
                context
                    .broker
                    .publish_as(
                        &message.subject,
                        message.reply.as_deref(),
                        message.payload,
                        Some(&identity.name),
                    )
                    .map_err(other)?;
                if opcode == PUBLISH_ACK {
                    timeout(context.auth.io_timeout, write_simple(send, OK))
                        .await
                        .map_err(timed_out)??;
                }
            }
            PING => {
                exhausted(&frame)?;
                timeout(context.auth.io_timeout, write_simple(send, OK))
                    .await
                    .map_err(timed_out)??;
            }
            SUBSCRIBE => {
                let subject = take_string(&mut frame)?;
                let queue = take_string(&mut frame)?;
                exhausted(&frame)?;
                identity.can_subscribe(&subject)?;
                if !queue.is_empty() && !valid_name(&queue) {
                    return Err(invalid("invalid queue group"));
                }
                let mut subscription = context
                    .broker
                    .subscribe(&subject, nonempty(&queue))
                    .map_err(other)?;
                timeout(context.auth.io_timeout, write_simple(send, OK))
                    .await
                    .map_err(timed_out)??;
                loop {
                    tokio::select! {
                        _ = send.stopped() => return Ok(()),
                        message = subscription.recv() => {
                            let message = message.map_err(other)?;
                            timeout(context.auth.io_timeout, write_message(send, MESSAGE, &message, &[])).await.map_err(timed_out)??;
                        }
                    }
                }
            }
            REQUEST => {
                let wait = take_u32(&mut frame)?;
                if wait == 0 || wait > 120_000 {
                    return Err(invalid("request timeout must be 1..120000 ms"));
                }
                let subject = take_string(&mut frame)?;
                identity.can_publish(&subject, None)?;
                check_payload(&frame)?;
                let message = context
                    .broker
                    .request_as(
                        &subject,
                        frame,
                        Duration::from_millis(wait as u64),
                        Some(&identity.name),
                    )
                    .await
                    .map_err(other)?;
                timeout(
                    context.auth.io_timeout,
                    write_message(send, MESSAGE, &message, &[]),
                )
                .await
                .map_err(timed_out)??;
                return Ok(());
            }
            STREAM_PUBLISH => {
                let stream_name = take_string(&mut frame)?;
                let message = decode_message(frame)?;
                identity.can_publish(&message.subject, message.reply.as_deref())?;
                let stream = context
                    .streams
                    .get(&stream_name)
                    .ok_or_else(|| invalid("unknown stream"))?;
                let record = stream
                    .publish(&message.subject, message.reply.as_deref(), message.payload)
                    .await
                    .map_err(other)?;
                let mut response = BytesMut::with_capacity(17);
                response.put_u8(STORED);
                response.put_u64(record.sequence);
                response.put_u64(record.timestamp_millis);
                timeout(
                    context.auth.io_timeout,
                    write_frame(send, response.freeze()),
                )
                .await
                .map_err(timed_out)??;
                return Ok(());
            }
            CONSUMER_OPEN => {
                let stream_name = take_string(&mut frame)?;
                let name = identity.consumer_name(&take_string(&mut frame)?)?;
                let filter_subject = take_string(&mut frame)?;
                let start_sequence = take_u64(&mut frame)?;
                let ack_wait_ms = take_u32(&mut frame)?;
                let max_ack_pending = take_u32(&mut frame)? as usize;
                exhausted(&frame)?;
                identity.can_subscribe(&filter_subject)?;
                if ack_wait_ms == 0
                    || ack_wait_ms > 3_600_000
                    || max_ack_pending == 0
                    || max_ack_pending > 65536
                {
                    return Err(invalid("invalid consumer limits"));
                }
                context
                    .streams
                    .get(&stream_name)
                    .ok_or_else(|| invalid("unknown stream"))?
                    .consumer(
                        &name,
                        ConsumerConfig {
                            filter_subject,
                            start_sequence,
                            ack_wait: Duration::from_millis(ack_wait_ms as u64),
                            max_ack_pending,
                        },
                    )
                    .await
                    .map_err(other)?;
                timeout(context.auth.io_timeout, write_simple(send, OK))
                    .await
                    .map_err(timed_out)??;
                return Ok(());
            }
            CONSUMER_FETCH => {
                let stream_name = take_string(&mut frame)?;
                let name = identity.consumer_name(&take_string(&mut frame)?)?;
                let limit = take_u32(&mut frame)? as usize;
                exhausted(&frame)?;
                if limit == 0 || limit > MAX_FETCH {
                    return Err(invalid("fetch limit must be 1..256"));
                }
                let consumer = context
                    .streams
                    .get(&stream_name)
                    .ok_or_else(|| invalid("unknown stream"))?
                    .get_consumer(&name)
                    .await
                    .map_err(other)?;
                let deliveries = consumer.fetch(limit).await.map_err(other)?;
                for delivery in deliveries {
                    identity.can_subscribe(&delivery.record.message.subject)?;
                    let mut prefix = BytesMut::with_capacity(20);
                    prefix.put_u64(delivery.record.sequence);
                    prefix.put_u64(delivery.record.timestamp_millis);
                    prefix.put_u32(delivery.delivery_count);
                    timeout(
                        context.auth.io_timeout,
                        write_message(send, DELIVERY, &delivery.record.message, &prefix),
                    )
                    .await
                    .map_err(timed_out)??;
                }
                timeout(context.auth.io_timeout, write_simple(send, OK))
                    .await
                    .map_err(timed_out)??;
                return Ok(());
            }
            ACK => {
                let stream_name = take_string(&mut frame)?;
                let name = identity.consumer_name(&take_string(&mut frame)?)?;
                let sequence = take_u64(&mut frame)?;
                exhausted(&frame)?;
                context
                    .streams
                    .get(&stream_name)
                    .ok_or_else(|| invalid("unknown stream"))?
                    .get_consumer(&name)
                    .await
                    .map_err(other)?
                    .ack(sequence)
                    .await
                    .map_err(other)?;
                timeout(context.auth.io_timeout, write_simple(send, OK))
                    .await
                    .map_err(timed_out)??;
                return Ok(());
            }
            _ => return Err(invalid("unknown opcode")),
        }
    }
}

fn denied(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[cfg(test)]
mod tests;
