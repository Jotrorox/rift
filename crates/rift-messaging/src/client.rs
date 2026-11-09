use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use bytes::{BufMut, Bytes, BytesMut};
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::{sync::Mutex, time::timeout};

use crate::{ConsumerConfig, Delivery, Message, Record, Result, frame::*};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// Connect, verify the server certificate for `server_name`, and
    /// authenticate with `token`.
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

    /// The underlying QUIC connection, for raw streams and close detection.
    pub fn connection(&self) -> &Connection {
        &self.inner.connection
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
