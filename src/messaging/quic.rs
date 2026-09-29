//! Authenticated QUIC transport for the shared broker and retained streams.
//!
//! Connections use TLS 1.3 and ALPN `rift-messaging/1`. See
//! `docs/messaging-protocol.md` for the language-independent wire protocol.
use std::{
    collections::{HashMap, HashSet},
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use hmac::{Hmac, Mac};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer,
    pem::{self, PemObject},
};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Mutex, Semaphore},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

use super::{
    Broker, Message,
    stream::{ConsumerConfig, Delivery, Record, Stream},
};

pub type Result<T> = io::Result<T>;
pub const ALPN: &[u8] = b"rift-messaging/1";
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = MAX_PAYLOAD_BYTES + 64 * 1024;
const MAX_STRING: usize = 4096;
const MAX_FETCH: usize = 256;
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
const AUTH: u8 = 1;
const OK: u8 = 2;
const ERROR: u8 = 3;
const PUBLISH: u8 = 4;
const PUBLISH_ACK: u8 = 5;
const SUBSCRIBE: u8 = 6;
const MESSAGE: u8 = 7;
const PING: u8 = 8;
const REQUEST: u8 = 9;
const STREAM_PUBLISH: u8 = 10;
const STORED: u8 = 11;
const CONSUMER_OPEN: u8 = 12;
const CONSUMER_FETCH: u8 = 13;
const DELIVERY: u8 = 14;
const ACK: u8 = 15;

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
    pub max_connections: usize,
    pub max_streams_per_connection: usize,
    pub handshake_timeout: Duration,
    pub io_timeout: Duration,
}
impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            identities: vec![],
            max_connections: 1024,
            max_streams_per_connection: 128,
            handshake_timeout: Duration::from_secs(5),
            io_timeout: Duration::from_secs(30),
        }
    }
}
impl AuthConfig {
    fn validate(&self) -> Result<()> {
        if self.max_connections == 0
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
    fn can_publish(&self, subject: &str, reply: Option<&str>) -> Result<()> {
        validate_literal(subject)?;
        if reserved(subject, "rift.events")
            || (!self.control && reserved(subject, "rift.control"))
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
        Ok(format!("remote_{:x}", digest.finalize()))
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
fn validate_pattern(pattern: &str) -> Result<()> {
    if pattern.is_empty() || pattern.len() > 1024 {
        return Err(invalid("invalid subject length"));
    }
    let mut parts = pattern.split('.').peekable();
    while let Some(part) = parts.next() {
        if part.is_empty()
            || part.chars().any(|c| c.is_whitespace() || c.is_control())
            || (part.contains(['*', '>']) && part != "*" && part != ">")
            || (part == ">" && parts.peek().is_some())
        {
            return Err(invalid("invalid subject pattern"));
        }
    }
    Ok(())
}
fn validate_literal(subject: &str) -> Result<()> {
    validate_pattern(subject)?;
    if subject.contains(['*', '>']) {
        return Err(invalid("publication subject must be literal"));
    }
    Ok(())
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

fn transport(streams: u32) -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();
    config.max_concurrent_uni_streams(0u32.into());
    config.max_concurrent_bidi_streams(streams.into());
    config.keep_alive_interval(Some(Duration::from_secs(10)));
    config.max_idle_timeout(Some(
        Duration::from_secs(60)
            .try_into()
            .expect("constant timeout"),
    ));
    config.stream_receive_window((2 * MAX_FRAME_BYTES as u32).into());
    config.receive_window((8 * MAX_FRAME_BYTES as u32).into());
    config.send_window(8 * MAX_FRAME_BYTES as u64);
    config.datagram_receive_buffer_size(None);
    config
}
/// Build TLS configuration from a PEM certificate chain and unencrypted PEM key.
pub fn tls_server_config(
    certificate: impl AsRef<Path>,
    private_key: impl AsRef<Path>,
) -> Result<quinn::ServerConfig> {
    let certs = read_certificates(certificate.as_ref())?;
    let private_key = private_key.as_ref();
    let key = PrivateKeyDer::from_pem_file(private_key)
        .map_err(|error| pem_error("private key", private_key, error))?;
    server_config_from_der(certs, key)
}
/// Build TLS configuration from DER material, also useful with a certificate manager.
pub fn server_config_from_der(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<quinn::ServerConfig> {
    let mut crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(other)?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map_err(other)?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    // 0-RTT is disabled: replayed publications/control commands are unacceptable.
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(crypto).map_err(other)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(Arc::new(transport(128)));
    Ok(config)
}
/// Trust the given PEM CA/server certificates; DNS/IP name verification stays enabled.
pub fn tls_client_config(ca_certificates: impl AsRef<Path>) -> Result<quinn::ClientConfig> {
    let certs = read_certificates(ca_certificates.as_ref())?;
    client_config_from_der(certs)
}
fn read_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|error| pem_error("certificate", path, error))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| pem_error("certificate", path, error))?;
    if certs.is_empty() {
        return Err(invalid(format!(
            "certificate PEM ({}): no certificates found",
            path.display()
        )));
    }
    Ok(certs)
}
fn pem_error(kind: &str, path: &Path, error: pem::Error) -> io::Error {
    let error_kind = match &error {
        pem::Error::Io(error) => error.kind(),
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(
        error_kind,
        format!("{kind} PEM ({}): {error}", path.display()),
    )
}
pub fn client_config_from_der(certs: Vec<CertificateDer<'static>>) -> Result<quinn::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots.add(cert).map_err(other)?;
    }
    let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(other)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).map_err(other)?,
    ));
    config.transport_config(Arc::new(transport(0)));
    Ok(config)
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
        tls.transport_config(Arc::new(transport(auth.max_streams_per_connection as u32)));
        let endpoint = Endpoint::server(tls, addr)?;
        let listener = endpoint.clone();
        let permits = Arc::new(Semaphore::new(auth.max_connections));
        let context = Arc::new(Context {
            broker,
            auth,
            streams,
        });
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    incoming = listener.accept() => {
                        let Some(incoming) = incoming else { break };
                        let Ok(permit) = permits.clone().try_acquire_owned() else { incoming.refuse(); continue };
                        let context = context.clone();
                        tasks.spawn(async move {
                            let _permit = permit;
                            if let Ok(Ok(connection)) = timeout(context.auth.handshake_timeout, incoming).await {
                                serve_connection(connection, context).await;
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
struct Context {
    broker: Broker,
    auth: AuthConfig,
    streams: HashMap<String, Stream>,
}
async fn serve_connection(connection: Connection, context: Arc<Context>) {
    let auth_result = timeout(context.auth.handshake_timeout, async {
        let (mut send, mut recv) = connection.accept_bi().await.map_err(other)?;
        let mut reader = FrameReader {
            max_frame_bytes: Some(MAX_STRING + 3),
            ..Default::default()
        };
        let mut frame = reader.read(&mut recv).await?;
        if take_u8(&mut frame)? != AUTH {
            return Err(denied("first stream must authenticate"));
        }
        let token = take_string(&mut frame)?;
        exhausted(&frame)?;
        match context.auth.authenticate(&token) {
            Ok(identity) => {
                write_simple(&mut send, OK).await?;
                send.finish().map_err(other)?;
                Ok(identity)
            }
            Err(error) => {
                let _ = write_error(&mut send, &error).await;
                let _ = send.finish();
                Err(error)
            }
        }
    })
    .await;
    let Ok(Ok(identity)) = auth_result else {
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
                    .publish_with_reply(&message.subject, message.reply.as_deref(), message.payload)
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
                    .request(&subject, frame, Duration::from_millis(wait as u64))
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

/// Cloneable asynchronous connection. Each subscription/RPC has its own QUIC stream.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}
struct ClientInner {
    endpoint: Endpoint,
    connection: Connection,
    publisher: Mutex<Option<Publisher>>,
}
impl Drop for ClientInner {
    fn drop(&mut self) {
        self.connection.close(0u32.into(), b"client closed");
    }
}
impl Client {
    pub async fn connect(
        addr: SocketAddr,
        server_name: &str,
        tls: quinn::ClientConfig,
        token: &str,
    ) -> Result<Self> {
        timeout(CLIENT_TIMEOUT, async {
            let bind = if addr.is_ipv4() {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            } else {
                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
            };
            let mut endpoint = Endpoint::client(SocketAddr::new(bind, 0))?;
            endpoint.set_default_client_config(tls);
            let connection = endpoint
                .connect(addr, server_name)
                .map_err(other)?
                .await
                .map_err(other)?;
            let (mut send, mut recv) = connection.open_bi().await.map_err(other)?;
            let mut auth = BytesMut::new();
            auth.put_u8(AUTH);
            put_string(&mut auth, token)?;
            write_frame(&mut send, auth.freeze()).await?;
            let _ = send.finish();
            expect_ok(FrameReader::default().read(&mut recv).await?)?;
            Ok(Self {
                inner: Arc::new(ClientInner {
                    endpoint,
                    connection,
                    publisher: Mutex::new(None),
                }),
            })
        })
        .await
        .map_err(timed_out)?
    }
    pub fn close(&self) {
        self.inner.connection.close(0u32.into(), b"client closed");
    }
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.endpoint.local_addr()
    }
    /// A dedicated ordered publisher. `publish` queues locally; `flush` confirms
    /// that all prior messages reached the broker. Keep it for hot loops.
    pub async fn publisher(&self) -> Result<Publisher> {
        let (send, recv) = timeout(CLIENT_TIMEOUT, self.inner.connection.open_bi())
            .await
            .map_err(timed_out)?
            .map_err(other)?;
        Ok(Publisher {
            send,
            recv,
            reader: FrameReader::default(),
            healthy: true,
            _client: Some(self.inner.clone()),
        })
    }
    /// Publish and wait for broker acceptance (one network round trip).
    pub async fn publish(&self, subject: &str, payload: Bytes) -> Result<()> {
        self.publish_with_reply(subject, None, payload).await
    }
    pub async fn publish_with_reply(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<()> {
        let mut slot = self.inner.publisher.lock().await;
        if slot.as_ref().is_none_or(|publisher| !publisher.healthy) {
            // The shared publisher must not keep its parent Arc alive in a cycle.
            let (send, recv) = timeout(CLIENT_TIMEOUT, self.inner.connection.open_bi())
                .await
                .map_err(timed_out)?
                .map_err(other)?;
            *slot = Some(Publisher {
                send,
                recv,
                reader: FrameReader::default(),
                healthy: true,
                _client: None,
            });
        }
        slot.as_mut()
            .expect("initialized publisher")
            .publish_confirmed(subject, reply, payload)
            .await
    }
    pub async fn subscribe(
        &self,
        subject: &str,
        queue: Option<&str>,
    ) -> Result<RemoteSubscription> {
        timeout(CLIENT_TIMEOUT, async {
            let (mut send, mut recv) = self.inner.connection.open_bi().await.map_err(other)?;
            let mut frame = BytesMut::new();
            frame.put_u8(SUBSCRIBE);
            put_string(&mut frame, subject)?;
            put_string(&mut frame, queue.unwrap_or_default())?;
            write_frame(&mut send, frame.freeze()).await?;
            let mut reader = FrameReader::default();
            expect_ok(reader.read(&mut recv).await?)?;
            Ok(RemoteSubscription {
                send,
                recv,
                reader,
                _client: self.inner.clone(),
            })
        })
        .await
        .map_err(timed_out)?
    }
    pub async fn request(&self, subject: &str, payload: Bytes, wait: Duration) -> Result<Message> {
        let millis =
            u32::try_from(wait.as_millis()).map_err(|_| invalid("request timeout too large"))?;
        if millis == 0 || millis > 120_000 {
            return Err(invalid("request timeout must be 1..120000 ms"));
        }
        let mut prefix = BytesMut::new();
        prefix.put_u8(REQUEST);
        prefix.put_u32(millis);
        put_string(&mut prefix, subject)?;
        check_payload(&payload)?;
        timeout(wait + Duration::from_secs(5), async {
            let (mut send, mut recv) = self.inner.connection.open_bi().await.map_err(other)?;
            write_parts(&mut send, prefix.freeze(), payload).await?;
            let _ = send.finish();
            let mut response = FrameReader::default().read(&mut recv).await?;
            expect_opcode(&mut response, MESSAGE)?;
            decode_message(response)
        })
        .await
        .map_err(timed_out)?
    }
    /// Store a message and await the stream's configured durability boundary.
    /// This does not also publish to live broker subscribers.
    pub async fn stream_publish(
        &self,
        stream: &str,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<Record> {
        let message = Message {
            subject: Arc::from(subject),
            reply: reply.map(Arc::from),
            payload,
        };
        let mut header = BytesMut::new();
        header.put_u8(STREAM_PUBLISH);
        put_string(&mut header, stream)?;
        message_header(&mut header, &message)?;
        let mut response = self
            .command(header.freeze(), message.payload.clone())
            .await?;
        expect_opcode(&mut response, STORED)?;
        let sequence = take_u64(&mut response)?;
        let timestamp_millis = take_u64(&mut response)?;
        exhausted(&response)?;
        Ok(Record {
            sequence,
            timestamp_millis,
            message,
        })
    }
    /// Create/reopen a durable consumer owned by the authenticated identity.
    pub async fn consumer(&self, stream: &str, name: &str, config: ConsumerConfig) -> Result<()> {
        let mut frame = BytesMut::new();
        frame.put_u8(CONSUMER_OPEN);
        put_string(&mut frame, stream)?;
        put_string(&mut frame, name)?;
        put_string(&mut frame, &config.filter_subject)?;
        frame.put_u64(config.start_sequence);
        frame.put_u32(
            u32::try_from(config.ack_wait.as_millis())
                .map_err(|_| invalid("ack wait too large"))?,
        );
        frame.put_u32(
            u32::try_from(config.max_ack_pending).map_err(|_| invalid("ack pending too large"))?,
        );
        expect_ok(self.command(frame.freeze(), Bytes::new()).await?)
    }
    pub async fn fetch(&self, stream: &str, name: &str, limit: usize) -> Result<Vec<Delivery>> {
        if limit == 0 || limit > MAX_FETCH {
            return Err(invalid("fetch limit must be 1..256"));
        }
        let mut frame = BytesMut::new();
        frame.put_u8(CONSUMER_FETCH);
        put_string(&mut frame, stream)?;
        put_string(&mut frame, name)?;
        frame.put_u32(limit as u32);
        timeout(CLIENT_TIMEOUT, async {
            let (mut send, mut recv) = self.inner.connection.open_bi().await.map_err(other)?;
            write_frame(&mut send, frame.freeze()).await?;
            let _ = send.finish();
            let mut reader = FrameReader::default();
            let mut records = Vec::new();
            loop {
                let mut response = reader.read(&mut recv).await?;
                if response.first() == Some(&OK) {
                    expect_ok(response)?;
                    return Ok(records);
                }
                if records.len() >= limit {
                    return Err(invalid("server exceeded fetch limit"));
                }
                expect_opcode(&mut response, DELIVERY)?;
                let sequence = take_u64(&mut response)?;
                let timestamp_millis = take_u64(&mut response)?;
                let delivery_count = take_u32(&mut response)?;
                let message = decode_message(response)?;
                records.push(Delivery {
                    record: Record {
                        sequence,
                        timestamp_millis,
                        message,
                    },
                    delivery_count,
                });
            }
        })
        .await
        .map_err(timed_out)?
    }
    pub async fn ack(&self, stream: &str, name: &str, sequence: u64) -> Result<()> {
        let mut frame = BytesMut::new();
        frame.put_u8(ACK);
        put_string(&mut frame, stream)?;
        put_string(&mut frame, name)?;
        frame.put_u64(sequence);
        expect_ok(self.command(frame.freeze(), Bytes::new()).await?)
    }
    async fn command(&self, header: Bytes, payload: Bytes) -> Result<Bytes> {
        timeout(CLIENT_TIMEOUT, async {
            let (mut send, mut recv) = self.inner.connection.open_bi().await.map_err(other)?;
            write_parts(&mut send, header, payload).await?;
            let _ = send.finish();
            FrameReader::default().read(&mut recv).await
        })
        .await
        .map_err(timed_out)?
    }
}

pub struct Publisher {
    send: SendStream,
    recv: RecvStream,
    reader: FrameReader,
    healthy: bool,
    _client: Option<Arc<ClientInner>>,
}
impl Publisher {
    /// Enqueue without an application round trip. Call `flush` to observe errors.
    pub async fn publish(
        &mut self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<()> {
        self.write_publication(PUBLISH, subject, reply, payload)
            .await
    }
    pub async fn publish_confirmed(
        &mut self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<()> {
        self.write_publication(PUBLISH_ACK, subject, reply, payload)
            .await
    }
    async fn write_publication(
        &mut self,
        opcode: u8,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<()> {
        if !self.healthy {
            return Err(other(
                "publisher closed or a previous operation was cancelled",
            ));
        }
        validate_literal(subject)?;
        if let Some(reply) = reply {
            validate_literal(reply)?;
        }
        check_payload(&payload)?;
        let mut header = BytesMut::new();
        header.put_u8(opcode);
        put_string(&mut header, subject)?;
        put_string(&mut header, reply.unwrap_or_default())?;
        self.healthy = false;
        timeout(CLIENT_TIMEOUT, async {
            write_parts(&mut self.send, header.freeze(), payload).await?;
            if opcode == PUBLISH_ACK {
                expect_ok(self.reader.read(&mut self.recv).await?)?;
            }
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(timed_out)??;
        self.healthy = true;
        Ok(())
    }
    /// Barrier: confirms all previous publications on this publisher were processed.
    pub async fn flush(&mut self) -> Result<()> {
        if !self.healthy {
            return Err(other(
                "publisher closed or a previous operation was cancelled",
            ));
        }
        self.healthy = false;
        timeout(CLIENT_TIMEOUT, async {
            write_simple(&mut self.send, PING).await?;
            expect_ok(self.reader.read(&mut self.recv).await?)
        })
        .await
        .map_err(timed_out)??;
        self.healthy = true;
        Ok(())
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        let _ = self.send.reset(0u32.into());
        let _ = self.recv.stop(0u32.into());
    }
}
/// Dropping a subscription immediately cancels its remote stream. `recv` is
/// cancellation safe, including when a network frame arrives in several chunks.
pub struct RemoteSubscription {
    send: SendStream,
    recv: RecvStream,
    reader: FrameReader,
    _client: Arc<ClientInner>,
}
impl RemoteSubscription {
    pub async fn recv(&mut self) -> Result<Message> {
        let mut frame = self.reader.read(&mut self.recv).await?;
        expect_opcode(&mut frame, MESSAGE)?;
        decode_message(frame)
    }
}
impl Drop for RemoteSubscription {
    fn drop(&mut self) {
        let _ = self.recv.stop(0u32.into());
        let _ = self.send.reset(0u32.into());
    }
}

#[derive(Default)]
struct FrameReader {
    max_frame_bytes: Option<usize>,
    header: [u8; 4],
    header_read: usize,
    body: BytesMut,
    first_chunk: Option<Bytes>,
    body_read: usize,
}
impl FrameReader {
    async fn read_bounded(&mut self, recv: &mut RecvStream, duration: Duration) -> Result<Bytes> {
        // Idle publishers remain usable; once a frame starts, partial-frame reads
        // have a deadline so a peer cannot trickle bytes indefinitely.
        if self.header_read == 0 {
            let n = recv
                .read(&mut self.header[..1])
                .await
                .map_err(other)?
                .ok_or_else(eof)?;
            self.header_read = n;
        }
        timeout(duration, self.read(recv))
            .await
            .map_err(timed_out)?
    }
    async fn read(&mut self, recv: &mut RecvStream) -> Result<Bytes> {
        while self.header_read < 4 {
            let n = recv
                .read(&mut self.header[self.header_read..])
                .await
                .map_err(other)?
                .ok_or_else(eof)?;
            self.header_read += n;
        }
        let size = u32::from_be_bytes(self.header) as usize;
        if size == 0 || size > self.max_frame_bytes.unwrap_or(MAX_FRAME_BYTES) {
            return Err(invalid("frame length exceeds limit or is zero"));
        }
        while self.body_read < size {
            let chunk = recv
                .read_chunk(size - self.body_read, true)
                .await
                .map_err(other)?
                .ok_or_else(eof)?
                .bytes;
            if self.body_read == 0 {
                if chunk.len() == size {
                    self.header_read = 0;
                    return Ok(chunk);
                }
                self.body_read = chunk.len();
                self.first_chunk = Some(chunk);
                continue;
            }
            if let Some(first) = self.first_chunk.take() {
                self.body.reserve(size);
                self.body.extend_from_slice(&first);
            }
            self.body_read += chunk.len();
            self.body.extend_from_slice(&chunk);
        }
        self.header_read = 0;
        self.body_read = 0;
        Ok(self.body.split().freeze())
    }
}
async fn write_simple(send: &mut SendStream, opcode: u8) -> Result<()> {
    write_frame(send, Bytes::copy_from_slice(&[opcode])).await
}
async fn write_frame(send: &mut SendStream, body: Bytes) -> Result<()> {
    write_parts(send, body, Bytes::new()).await
}
async fn write_parts(send: &mut SendStream, header: Bytes, payload: Bytes) -> Result<()> {
    let size = header
        .len()
        .checked_add(payload.len())
        .filter(|s| *s > 0 && *s <= MAX_FRAME_BYTES)
        .ok_or_else(|| invalid("frame exceeds limit"))?;
    let mut prefix = BytesMut::with_capacity(4 + header.len());
    prefix.put_u32(size as u32);
    prefix.extend_from_slice(&header);
    // Quinn takes ownership of Bytes: payload data is not copied into a second
    // application buffer and header/payload enter the transport in one call.
    send.write_all_chunks(&mut [prefix.freeze(), payload])
        .await
        .map_err(other)
}
async fn write_message(
    send: &mut SendStream,
    opcode: u8,
    message: &Message,
    extra: &[u8],
) -> Result<()> {
    let mut header = BytesMut::new();
    header.put_u8(opcode);
    header.extend_from_slice(extra);
    message_header(&mut header, message)?;
    write_parts(send, header.freeze(), message.payload.clone()).await
}
fn message_header(header: &mut BytesMut, message: &Message) -> Result<()> {
    validate_literal(&message.subject)?;
    if let Some(reply) = &message.reply {
        validate_literal(reply)?;
    }
    check_payload(&message.payload)?;
    put_string(header, &message.subject)?;
    put_string(header, message.reply.as_deref().unwrap_or_default())
}
fn decode_message(mut frame: Bytes) -> Result<Message> {
    let subject = take_string(&mut frame)?;
    let reply = take_string(&mut frame)?;
    validate_literal(&subject)?;
    if !reply.is_empty() {
        validate_literal(&reply)?;
    }
    check_payload(&frame)?;
    Ok(Message {
        subject: Arc::from(subject),
        reply: nonempty(&reply).map(Arc::from),
        payload: frame,
    })
}
async fn write_error(send: &mut SendStream, error: &io::Error) -> Result<()> {
    let mut frame = BytesMut::new();
    frame.put_u8(ERROR);
    frame.put_u16(if error.kind() == io::ErrorKind::PermissionDenied {
        1
    } else {
        2
    });
    let message = error.to_string();
    // Avoid unbounded peer-visible diagnostic strings from underlying libraries.
    let end = message
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= 1024)
        .last()
        .unwrap_or(0);
    put_string(
        &mut frame,
        if message.len() > 1024 {
            &message[..end]
        } else {
            &message
        },
    )?;
    write_frame(send, frame.freeze()).await
}
fn expect_opcode(frame: &mut Bytes, expected: u8) -> Result<()> {
    let opcode = take_u8(frame)?;
    if opcode == ERROR {
        let code = take_u16(frame)?;
        let message = take_string(frame)?;
        exhausted(frame)?;
        return Err(io::Error::new(
            if code == 1 {
                io::ErrorKind::PermissionDenied
            } else {
                io::ErrorKind::InvalidData
            },
            message,
        ));
    }
    if opcode != expected {
        return Err(invalid("unexpected response opcode"));
    }
    Ok(())
}
fn expect_ok(mut frame: Bytes) -> Result<()> {
    expect_opcode(&mut frame, OK)?;
    exhausted(&frame)
}
fn check_payload(payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        Err(invalid("payload exceeds 1 MiB transport limit"))
    } else {
        Ok(())
    }
}
fn put_string(buffer: &mut BytesMut, value: &str) -> Result<()> {
    if value.len() > MAX_STRING {
        return Err(invalid("string exceeds protocol limit"));
    }
    buffer.put_u16(value.len() as u16);
    buffer.extend_from_slice(value.as_bytes());
    Ok(())
}
fn take_string(frame: &mut Bytes) -> Result<String> {
    let len = take_u16(frame)? as usize;
    if len > MAX_STRING || frame.len() < len {
        return Err(invalid("invalid string length"));
    }
    String::from_utf8(frame.split_to(len).to_vec()).map_err(|_| invalid("string must be UTF-8"))
}
fn take_u8(frame: &mut Bytes) -> Result<u8> {
    if frame.is_empty() {
        Err(invalid("truncated frame"))
    } else {
        Ok(frame.get_u8())
    }
}
fn take_u16(frame: &mut Bytes) -> Result<u16> {
    if frame.len() < 2 {
        Err(invalid("truncated frame"))
    } else {
        Ok(frame.get_u16())
    }
}
fn take_u32(frame: &mut Bytes) -> Result<u32> {
    if frame.len() < 4 {
        Err(invalid("truncated frame"))
    } else {
        Ok(frame.get_u32())
    }
}
fn take_u64(frame: &mut Bytes) -> Result<u64> {
    if frame.len() < 8 {
        Err(invalid("truncated frame"))
    } else {
        Ok(frame.get_u64())
    }
}
fn exhausted(frame: &Bytes) -> Result<()> {
    if frame.is_empty() {
        Ok(())
    } else {
        Err(invalid("unexpected trailing frame bytes"))
    }
}
fn nonempty(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}
fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn denied(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
fn other(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::other(error)
}
fn timed_out(_: tokio::time::error::Elapsed) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "QUIC operation timed out")
}
fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "QUIC stream closed")
}

#[cfg(test)]
mod tests;
