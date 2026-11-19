//! What peer links need from the network: connections that carry one
//! message per stream, each optionally answered.
//!
//! [`Links`](super::link::Links) runs the handshake, the stream kinds,
//! liveness and reconnects over any [`Transport`]. Production uses QUIC
//! ([`super::quic`]); tests use [`memory`], an in-process network that can
//! partition, delay and drop, so the cluster logic is tested without sockets.
use bytes::Bytes;
use std::{future::Future, io, pin::Pin, sync::Arc};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Dials and accepts connections to peers.
pub trait Transport: Send + Sync + 'static {
    /// Open a connection to `address` (`host:port`). TLS, if any, is done
    /// when this returns; the link handshake runs on top of it.
    fn connect(&self, address: &str) -> BoxFuture<'_, io::Result<Arc<dyn Connection>>>;
    /// The next inbound connection; `None` once the transport is closed.
    fn accept(&self) -> BoxFuture<'_, Option<Arc<dyn Connection>>>;
    /// Stop accepting and close every connection.
    fn close(&self);
}

/// One connection between two peers.
pub trait Connection: Send + Sync + 'static {
    /// Send `message` on a new stream. With `reply`, wait for the answer
    /// (at most `limit` bytes) and return it.
    fn exchange(
        &self,
        message: Bytes,
        reply: bool,
        limit: usize,
    ) -> BoxFuture<'_, io::Result<Option<Bytes>>>;
    /// The next stream the other side opened; `None` once closed.
    fn next(&self) -> BoxFuture<'_, Option<Incoming>>;
    /// 32 bytes both ends derive from this connection's secrets, so a token
    /// proof made for one connection is worthless on another.
    fn binding(&self) -> io::Result<[u8; 32]>;
    /// The other end's socket address, for diagnostics.
    fn remote(&self) -> String;
    fn close(&self, reason: &str);
    /// Resolves once the connection is closed, by either side.
    fn closed(&self) -> BoxFuture<'_, ()>;
}

/// A stream the other side opened: its message once read, and where the
/// answer goes. Reading happens in [`Incoming::read`], off the accept loop.
pub struct Incoming {
    pub read: BoxFuture<'static, io::Result<(Bytes, ReplySink)>>,
}

/// Sends the answer to one incoming stream. Dropping it unanswered closes
/// the stream; the other side then sees an error.
pub struct ReplySink(Box<dyn FnOnce(Bytes) -> BoxFuture<'static, ()> + Send>);

impl ReplySink {
    pub fn new<F, Fut>(send: F) -> Self
    where
        F: FnOnce(Bytes) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self(Box::new(move |reply| Box::pin(send(reply))))
    }

    pub async fn send(self, reply: Bytes) {
        (self.0)(reply).await
    }
}

/// A transport that reaches nobody. `rift cluster init` uses it to commit
/// the first entries of a one-voter cluster without opening the peer port.
pub struct Offline {
    closed: tokio::sync::watch::Sender<bool>,
}

impl Default for Offline {
    fn default() -> Self {
        Self {
            closed: tokio::sync::watch::channel(false).0,
        }
    }
}

impl Transport for Offline {
    fn connect(&self, address: &str) -> BoxFuture<'_, io::Result<Arc<dyn Connection>>> {
        let error = io::Error::new(
            io::ErrorKind::NotConnected,
            format!("{address}: this peer is offline"),
        );
        Box::pin(async move { Err(error) })
    }

    fn accept(&self) -> BoxFuture<'_, Option<Arc<dyn Connection>>> {
        Box::pin(async move {
            let mut closed = self.closed.subscribe();
            stopped(&mut closed).await;
            None
        })
    }

    fn close(&self) {
        self.closed.send_replace(true);
    }
}

/// Resolves once `stop` holds `true` or its sender is gone. Never holds the
/// watch's borrow across an await, so the future stays `Send`.
pub async fn stopped(stop: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
pub mod memory {
    //! An in-process network with faults, for the simulation harness.
    //!
    //! Endpoints are named by address. Faults are decided per message from
    //! the network's state: a partition between two addresses fails every
    //! exchange between them (both ways), `delay` holds each message on the
    //! tokio clock (paused in tests, so delays cost no real time), and
    //! `drop_every` fails every n-th message, counted network-wide so a run
    //! is reproducible.
    use super::*;
    use std::{
        collections::{BTreeSet, HashMap},
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::{mpsc, oneshot, watch};

    type Accepts = mpsc::UnboundedSender<Arc<dyn Connection>>;

    #[derive(Default)]
    struct Faults {
        partitions: BTreeSet<(String, String)>,
        delay: Duration,
        drop_every: u64,
    }

    #[derive(Clone, Default)]
    pub struct Network {
        inner: Arc<NetworkInner>,
    }

    #[derive(Default)]
    struct NetworkInner {
        endpoints: Mutex<HashMap<String, Accepts>>,
        faults: Mutex<Faults>,
        messages: AtomicU64,
        next_connection: AtomicU64,
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        if a <= b {
            (a.to_owned(), b.to_owned())
        } else {
            (b.to_owned(), a.to_owned())
        }
    }

    impl Network {
        /// A transport listening on `address` in this network.
        pub fn bind(&self, address: &str) -> Arc<MemoryTransport> {
            let (sender, receiver) = mpsc::unbounded_channel();
            self.inner
                .endpoints
                .lock()
                .unwrap()
                .insert(address.to_owned(), sender);
            Arc::new(MemoryTransport {
                network: self.clone(),
                address: address.to_owned(),
                accepts: tokio::sync::Mutex::new(receiver),
                connections: Mutex::new(Vec::new()),
                closed: AtomicBool::new(false),
            })
        }

        /// Cut every message between `a` and `b`, both ways.
        pub fn partition(&self, a: &str, b: &str) {
            self.inner
                .faults
                .lock()
                .unwrap()
                .partitions
                .insert(pair(a, b));
        }

        /// Isolate `address` from every other endpoint in `all`.
        pub fn isolate(&self, address: &str, all: &[String]) {
            for other in all {
                if other != address {
                    self.partition(address, other);
                }
            }
        }

        pub fn heal(&self) {
            self.inner.faults.lock().unwrap().partitions.clear();
        }

        pub fn set_delay(&self, delay: Duration) {
            self.inner.faults.lock().unwrap().delay = delay;
        }

        /// Fail every `n`-th message (0 turns dropping off).
        pub fn set_drop_every(&self, n: u64) {
            self.inner.faults.lock().unwrap().drop_every = n;
        }

        fn fault(&self, from: &str, to: &str) -> (bool, Duration) {
            let faults = self.inner.faults.lock().unwrap();
            let count = self.inner.messages.fetch_add(1, Ordering::Relaxed) + 1;
            let cut = faults.partitions.contains(&pair(from, to))
                || (faults.drop_every > 0 && count.is_multiple_of(faults.drop_every));
            (cut, faults.delay)
        }
    }

    pub struct MemoryTransport {
        network: Network,
        address: String,
        accepts: tokio::sync::Mutex<mpsc::UnboundedReceiver<Arc<dyn Connection>>>,
        /// Both dialed and accepted ones, closed with the transport.
        connections: Mutex<Vec<Arc<dyn Connection>>>,
        closed: AtomicBool,
    }

    impl Transport for MemoryTransport {
        fn connect(&self, address: &str) -> BoxFuture<'_, io::Result<Arc<dyn Connection>>> {
            let address = address.to_owned();
            Box::pin(async move {
                if self.closed.load(Ordering::SeqCst) {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "transport closed",
                    ));
                }
                let (cut, delay) = self.network.fault(&self.address, &address);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                if cut {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "partitioned"));
                }
                let accepts = self
                    .network
                    .inner
                    .endpoints
                    .lock()
                    .unwrap()
                    .get(&address)
                    .cloned()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::ConnectionRefused, "no such endpoint")
                    })?;
                let id = self
                    .network
                    .inner
                    .next_connection
                    .fetch_add(1, Ordering::Relaxed);
                let mut binding = [0u8; 32];
                binding[..8].copy_from_slice(&id.to_le_bytes());
                let (closed, _) = watch::channel(false);
                let closed = Arc::new(closed);
                let (to_remote, remote_inbox) = mpsc::unbounded_channel();
                let (to_local, local_inbox) = mpsc::unbounded_channel();
                let local = Arc::new(MemoryConnection {
                    network: self.network.clone(),
                    local: self.address.clone(),
                    remote: address.clone(),
                    outbox: to_remote,
                    inbox: tokio::sync::Mutex::new(local_inbox),
                    binding,
                    closed: closed.clone(),
                });
                let remote = Arc::new(MemoryConnection {
                    network: self.network.clone(),
                    local: address.clone(),
                    remote: self.address.clone(),
                    outbox: to_local,
                    inbox: tokio::sync::Mutex::new(remote_inbox),
                    binding,
                    closed,
                });
                accepts.send(remote as Arc<dyn Connection>).map_err(|_| {
                    io::Error::new(io::ErrorKind::ConnectionRefused, "endpoint closed")
                })?;
                let local = local as Arc<dyn Connection>;
                self.connections.lock().unwrap().push(local.clone());
                Ok(local)
            })
        }

        fn accept(&self) -> BoxFuture<'_, Option<Arc<dyn Connection>>> {
            Box::pin(async move {
                let connection = self.accepts.lock().await.recv().await?;
                if self.closed.load(Ordering::SeqCst) {
                    connection.close("transport closed");
                    return None;
                }
                self.connections.lock().unwrap().push(connection.clone());
                Some(connection)
            })
        }

        fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
            self.network
                .inner
                .endpoints
                .lock()
                .unwrap()
                .remove(&self.address);
            for connection in self.connections.lock().unwrap().drain(..) {
                connection.close("transport closed");
            }
        }
    }

    type Delivery = (Bytes, oneshot::Sender<Bytes>);

    pub struct MemoryConnection {
        network: Network,
        local: String,
        remote: String,
        outbox: mpsc::UnboundedSender<Delivery>,
        inbox: tokio::sync::Mutex<mpsc::UnboundedReceiver<Delivery>>,
        binding: [u8; 32],
        /// Shared by both ends: closing one closes the other.
        closed: Arc<watch::Sender<bool>>,
    }

    impl MemoryConnection {
        fn is_closed(&self) -> bool {
            *self.closed.borrow()
        }
    }

    impl Connection for MemoryConnection {
        fn exchange(
            &self,
            message: Bytes,
            reply: bool,
            limit: usize,
        ) -> BoxFuture<'_, io::Result<Option<Bytes>>> {
            Box::pin(async move {
                if self.is_closed() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "connection closed",
                    ));
                }
                if message.len() > limit {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "message too large",
                    ));
                }
                let (cut, delay) = self.network.fault(&self.local, &self.remote);
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                if cut {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "partitioned"));
                }
                let (answer, answered) = oneshot::channel();
                self.outbox.send((message, answer)).map_err(|_| {
                    io::Error::new(io::ErrorKind::NotConnected, "connection closed")
                })?;
                if !reply {
                    return Ok(None);
                }
                let mut closed = self.closed.subscribe();
                tokio::select! {
                    answer = answered => {
                        let answer = answer.map_err(|_| {
                            io::Error::new(io::ErrorKind::ConnectionReset, "stream reset")
                        })?;
                        // The answer crosses the network too.
                        let (cut, delay) = self.network.fault(&self.remote, &self.local);
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        if cut {
                            return Err(io::Error::new(io::ErrorKind::TimedOut, "partitioned"));
                        }
                        Ok(Some(answer))
                    }
                    _ = stopped(&mut closed) => {
                        Err(io::Error::new(io::ErrorKind::ConnectionReset, "connection closed"))
                    }
                }
            })
        }

        fn next(&self) -> BoxFuture<'_, Option<Incoming>> {
            Box::pin(async move {
                let mut closed = self.closed.subscribe();
                let mut inbox = self.inbox.lock().await;
                let (message, answer) = tokio::select! {
                    delivery = inbox.recv() => delivery?,
                    _ = stopped(&mut closed) => return None,
                };
                Some(Incoming {
                    read: Box::pin(async move {
                        Ok((
                            message,
                            ReplySink::new(move |reply| async move {
                                let _ = answer.send(reply);
                            }),
                        ))
                    }),
                })
            })
        }

        fn binding(&self) -> io::Result<[u8; 32]> {
            Ok(self.binding)
        }

        fn remote(&self) -> String {
            self.remote.clone()
        }

        fn close(&self, _reason: &str) {
            self.closed.send_replace(true);
        }

        fn closed(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                let mut closed = self.closed.subscribe();
                stopped(&mut closed).await;
            })
        }
    }
}
