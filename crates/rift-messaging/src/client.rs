use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex as StdMutex, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use bytes::{BufMut, Bytes, BytesMut};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::{
    sync::{Mutex, watch},
    task::JoinHandle,
    time::timeout,
};

use crate::{
    ConsumerConfig, Delivery, Message, Record, Result, ServerAddress, SubscriptionScope, frame::*,
};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Pause after every address of a failover client failed once, doubling up
/// to [`MAX_RECONNECT_DELAY`].
const MIN_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(10);

/// Cloneable asynchronous connection. Each subscription/RPC has its own QUIC stream.
///
/// A client made by [`Client::connect_any`] reconnects by itself when its
/// connection ends, trying the next address; see there.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    link: RwLock<Link>,
    /// The generation of the current link, so failover subscriptions can
    /// wait for the next connection.
    generation: watch::Sender<u64>,
    publisher: Mutex<Option<Publisher>>,
    /// Whether the client reconnects by itself ([`Client::connect_any`]).
    failover: bool,
    closed: AtomicBool,
    reconnector: StdMutex<Option<JoinHandle<()>>>,
}

/// One authenticated connection and the endpoint it runs on.
#[derive(Clone)]
struct Link {
    endpoint: Endpoint,
    connection: Connection,
    generation: u64,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        if let Some(task) = self
            .reconnector
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
        self.link
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .connection
            .close(0u32.into(), b"client closed");
    }
}

impl ClientInner {
    fn link(&self) -> Link {
        self.link.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn connection(&self) -> Connection {
        self.link
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .connection
            .clone()
    }

    /// Make a new connection current and wake failover subscriptions.
    fn install(&self, endpoint: Endpoint, connection: Connection) {
        let generation = {
            let mut link = self.link.write().unwrap_or_else(|e| e.into_inner());
            let generation = link.generation + 1;
            let previous = std::mem::replace(
                &mut *link,
                Link {
                    endpoint,
                    connection,
                    generation,
                },
            );
            previous
                .connection
                .close(0u32.into(), b"replaced by a new connection");
            generation
        };
        self.generation.send_replace(generation);
    }
}

/// Connect to `addr`, verify its certificate for `server_name` and
/// authenticate with `token`.
async fn open(
    addr: SocketAddr,
    server_name: &str,
    tls: quinn::ClientConfig,
    token: &str,
) -> Result<(Endpoint, Connection)> {
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
        Ok((endpoint, connection))
    })
    .await
    .map_err(timed_out)?
}

/// A failover client's background task: whenever the connection ends,
/// connect to the next address in turn until one accepts.
async fn reconnect(
    inner: Weak<ClientInner>,
    mut connection: Connection,
    addresses: Vec<ServerAddress>,
    mut index: usize,
    tls: quinn::ClientConfig,
    token: String,
) {
    loop {
        connection.closed().await;
        let mut delay = MIN_RECONNECT_DELAY;
        let mut failures = 0usize;
        loop {
            match inner.upgrade() {
                Some(client) if !client.closed.load(Ordering::Acquire) => {}
                _ => return,
            }
            index = (index + 1) % addresses.len();
            let server = &addresses[index];
            match open(server.address, &server.server_name, tls.clone(), &token).await {
                Ok((endpoint, next)) => {
                    let client = match inner.upgrade() {
                        Some(client) if !client.closed.load(Ordering::Acquire) => client,
                        _ => {
                            next.close(0u32.into(), b"client closed");
                            return;
                        }
                    };
                    client.install(endpoint, next.clone());
                    connection = next;
                    break;
                }
                Err(_) => {
                    failures += 1;
                    if failures.is_multiple_of(addresses.len()) {
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(MAX_RECONNECT_DELAY);
                    }
                }
            }
        }
    }
}

impl Client {
    /// Connect, verify the server certificate for `server_name`, and
    /// authenticate with `token`. The client does not reconnect: once the
    /// connection ends, every operation fails.
    pub async fn connect(
        addr: SocketAddr,
        server_name: &str,
        tls: quinn::ClientConfig,
        token: &str,
    ) -> Result<Self> {
        let (endpoint, connection) = open(addr, server_name, tls, token).await?;
        Ok(Self::with_link(endpoint, connection, false))
    }

    /// Connect to the first of `addresses` that accepts, such as the peers
    /// of a Rift cluster, verifying each certificate for its own
    /// [`ServerAddress::server_name`].
    ///
    /// The client then stays connected: when its connection ends, it
    /// connects to the next address in turn (wrapping around), pausing with
    /// growing delays after every address failed once. Operations fail
    /// while no connection is up and are never retried, since a failed
    /// publication may have reached the broker. Subscriptions are
    /// re-established on the new connection by their next
    /// [`RemoteSubscription::recv`] (a waiting `recv` does so at once);
    /// messages published in between are not delivered. Publishers from
    /// [`Client::publisher`] belong to one connection: create a new one
    /// after a failure.
    pub async fn connect_any(
        addresses: &[ServerAddress],
        tls: quinn::ClientConfig,
        token: &str,
    ) -> Result<Self> {
        let mut last = invalid("no server addresses");
        for (index, server) in addresses.iter().enumerate() {
            match open(server.address, &server.server_name, tls.clone(), token).await {
                Ok((endpoint, connection)) => {
                    let client = Self::with_link(endpoint, connection.clone(), true);
                    let task = tokio::spawn(reconnect(
                        Arc::downgrade(&client.inner),
                        connection,
                        addresses.to_vec(),
                        index,
                        tls,
                        token.to_owned(),
                    ));
                    *client
                        .inner
                        .reconnector
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(task);
                    return Ok(client);
                }
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn with_link(endpoint: Endpoint, connection: Connection, failover: bool) -> Self {
        Self {
            inner: Arc::new(ClientInner {
                link: RwLock::new(Link {
                    endpoint,
                    connection,
                    generation: 0,
                }),
                generation: watch::channel(0).0,
                publisher: Mutex::new(None),
                failover,
                closed: AtomicBool::new(false),
                reconnector: StdMutex::new(None),
            }),
        }
    }

    /// Close the connection; a failover client also stops reconnecting.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
        self.inner.connection().close(0u32.into(), b"client closed");
        // Wake subscriptions waiting for a reconnect, so they fail.
        self.inner.generation.send_modify(|_| {});
    }

    /// The local address of the current connection.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.inner.link().endpoint.local_addr()
    }

    /// The current QUIC connection, for raw streams and close detection.
    /// A failover client replaces it after a reconnect.
    pub fn connection(&self) -> Connection {
        self.inner.connection()
    }

    /// The address the current connection was made to.
    pub fn remote_address(&self) -> SocketAddr {
        self.inner.connection().remote_address()
    }

    /// A dedicated ordered publisher. `publish` queues locally; `flush` confirms
    /// that all prior messages reached the broker. Keep it for hot loops.
    pub async fn publisher(&self) -> Result<Publisher> {
        let (send, recv) = timeout(CLIENT_TIMEOUT, self.inner.connection().open_bi())
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
            let (send, recv) = timeout(CLIENT_TIMEOUT, self.inner.connection().open_bi())
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

    /// Subscribe with [`SubscriptionScope::Cluster`], the default.
    pub async fn subscribe(
        &self,
        subject: &str,
        queue: Option<&str>,
    ) -> Result<RemoteSubscription> {
        self.subscribe_scoped(subject, queue, SubscriptionScope::Cluster)
            .await
    }

    /// Subscribe to `subject` (a pattern) in queue group `queue`, hearing
    /// messages from the peers `scope` names. A server from before scopes
    /// refuses [`SubscriptionScope::Local`].
    pub async fn subscribe_scoped(
        &self,
        subject: &str,
        queue: Option<&str>,
        scope: SubscriptionScope,
    ) -> Result<RemoteSubscription> {
        let generation = self.inner.generation.subscribe();
        let link = self.inner.link();
        let active = open_subscription(&link.connection, subject, queue, scope).await?;
        Ok(RemoteSubscription {
            active: Some(active),
            subject: subject.to_owned(),
            queue: queue.map(str::to_owned),
            scope,
            generation,
            client: self.inner.clone(),
        })
    }

    /// Send a request through a private broker inbox and wait up to `wait`
    /// (1 ms to 120 s) for the first reply.
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
            let (mut send, mut recv) = self.inner.connection().open_bi().await.map_err(other)?;
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
            sender: None,
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
            let (mut send, mut recv) = self.inner.connection().open_bi().await.map_err(other)?;
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

    /// Delete a durable consumer owned by the authenticated identity,
    /// discarding its position and pending deliveries.
    pub async fn delete_consumer(&self, stream: &str, name: &str) -> Result<()> {
        let mut frame = BytesMut::new();
        frame.put_u8(CONSUMER_DELETE);
        put_string(&mut frame, stream)?;
        put_string(&mut frame, name)?;
        expect_ok(self.command(frame.freeze(), Bytes::new()).await?)
    }

    async fn command(&self, header: Bytes, payload: Bytes) -> Result<Bytes> {
        timeout(CLIENT_TIMEOUT, async {
            let (mut send, mut recv) = self.inner.connection().open_bi().await.map_err(other)?;
            write_parts(&mut send, header, payload).await?;
            let _ = send.finish();
            FrameReader::default().read(&mut recv).await
        })
        .await
        .map_err(timed_out)?
    }
}

/// An ordered publication stream; see [`Client::publisher`].
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

/// Send SUBSCRIBE on a new stream of `connection` and wait for its OK.
async fn open_subscription(
    connection: &Connection,
    subject: &str,
    queue: Option<&str>,
    scope: SubscriptionScope,
) -> Result<Active> {
    timeout(CLIENT_TIMEOUT, async {
        let (mut send, mut recv) = connection.open_bi().await.map_err(other)?;
        let mut frame = BytesMut::new();
        frame.put_u8(SUBSCRIBE);
        put_string(&mut frame, subject)?;
        put_string(&mut frame, queue.unwrap_or_default())?;
        // The default is left out, so servers from before scopes accept it.
        if scope == SubscriptionScope::Local {
            frame.put_u8(SCOPE_LOCAL);
        }
        write_frame(&mut send, frame.freeze()).await?;
        let mut reader = FrameReader::default();
        expect_ok(reader.read(&mut recv).await?)?;
        Ok(Active {
            send,
            recv,
            reader,
            connection: connection.clone(),
        })
    })
    .await
    .map_err(timed_out)?
}

/// Dropping a subscription immediately cancels its remote stream. `recv` is
/// cancellation safe, including when a network frame arrives in several chunks.
///
/// On a client from [`Client::connect_any`], `recv` waits through a lost
/// connection and subscribes again on the next one.
pub struct RemoteSubscription {
    /// `None` while a failover client has no connection to subscribe on.
    active: Option<Active>,
    subject: String,
    queue: Option<String>,
    scope: SubscriptionScope,
    generation: watch::Receiver<u64>,
    client: Arc<ClientInner>,
}

/// The delivery stream of one established subscription.
struct Active {
    send: SendStream,
    recv: RecvStream,
    reader: FrameReader,
    connection: Connection,
}

impl Drop for Active {
    fn drop(&mut self) {
        let _ = self.recv.stop(0u32.into());
        let _ = self.send.reset(0u32.into());
    }
}

impl RemoteSubscription {
    pub async fn recv(&mut self) -> Result<Message> {
        loop {
            if let Some(active) = &mut self.active {
                let result: Result<Message> = async {
                    let mut frame = active.reader.read(&mut active.recv).await?;
                    expect_opcode(&mut frame, MESSAGE)?;
                    decode_message(frame)
                }
                .await;
                match result {
                    Ok(message) => return Ok(message),
                    // The server ended the stream on a live connection (or
                    // the client does not fail over): the caller decides.
                    Err(error)
                        if !self.client.failover || active.connection.close_reason().is_none() =>
                    {
                        return Err(error);
                    }
                    Err(_) => self.active = None,
                }
            }
            self.resubscribe().await?;
        }
    }

    /// Subscribe again on the newest connection, waiting for the failover
    /// task to make one when the current connection is gone.
    async fn resubscribe(&mut self) -> Result<()> {
        loop {
            if self.client.closed.load(Ordering::Acquire) {
                return Err(other("client closed"));
            }
            let link = self.client.link();
            if link.connection.close_reason().is_none() {
                match open_subscription(
                    &link.connection,
                    &self.subject,
                    self.queue.as_deref(),
                    self.scope,
                )
                .await
                {
                    Ok(active) => {
                        self.active = Some(active);
                        return Ok(());
                    }
                    // Refused by a live server, for example a permission.
                    Err(error) if link.connection.close_reason().is_none() => return Err(error),
                    Err(_) => {}
                }
            }
            loop {
                let current = *self.generation.borrow_and_update();
                if current > link.generation || self.client.closed.load(Ordering::Acquire) {
                    break;
                }
                self.generation
                    .changed()
                    .await
                    .map_err(|_| other("client closed"))?;
            }
        }
    }
}
