//! Bound HTTP resources before a request reaches Axum middleware.
//!
//! Header reads and response writes happen outside middleware. Wrap the actual
//! socket so slow peers cannot outlive its deadline or the shutdown grace period.
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    time::{Instant, Sleep, sleep},
};

const MAX_CONNECTIONS: usize = 64;
/// A socket that moved fewer than [`WINDOW_BYTES`] in one window is closed:
/// idle keep-alive connections and trickled headers last one window at most.
const SOCKET_WINDOW: Duration = Duration::from_secs(10);
/// 256 KiB per window (about 26 KiB/s) keeps a transfer alive, so the largest
/// upload, 64 MiB, has roughly 43 minutes on the slowest accepted link.
const WINDOW_BYTES: u64 = 256 * 1024;
/// No socket outlives this, however steadily it transfers.
const SOCKET_LIFETIME: Duration = Duration::from_secs(60 * 60);

pub struct Listener {
    socket: TcpListener,
    slots: Arc<Semaphore>,
    force: watch::Receiver<bool>,
}

impl Listener {
    pub fn new(socket: TcpListener, force: watch::Receiver<bool>) -> Self {
        Self {
            socket,
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            force,
        }
    }
}

impl axum::serve::Listener for Listener {
    type Io = Stream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Stream, SocketAddr) {
        loop {
            match self.socket.accept().await {
                Ok((socket, address)) => {
                    let Ok(permit) = self.slots.clone().try_acquire_owned() else {
                        // Admission precedes HTTP header parsing. Never queue
                        // accepted sockets while all connection slots are used.
                        drop(socket);
                        tokio::task::yield_now().await;
                        continue;
                    };
                    return (
                        Stream::new(socket, permit, self.force.clone(), Limits::default()),
                        address,
                    );
                }
                Err(error) => {
                    eprintln!("rift: HTTP accept: {error}");
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

#[derive(Clone, Copy)]
struct Limits {
    window: Duration,
    window_bytes: u64,
    lifetime: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            window: SOCKET_WINDOW,
            window_bytes: WINDOW_BYTES,
            lifetime: SOCKET_LIFETIME,
        }
    }
}

pub struct Stream {
    socket: TcpStream,
    _permit: OwnedSemaphorePermit,
    limits: Limits,
    /// End of the current window.
    deadline: Pin<Box<Sleep>>,
    /// Bytes read and written in the current window.
    moved: u64,
    expires: Instant,
    forced: Pin<Box<dyn Future<Output = ()> + Send>>,
    closed: bool,
}

impl Stream {
    fn new(
        socket: TcpStream,
        permit: OwnedSemaphorePermit,
        mut force: watch::Receiver<bool>,
        limits: Limits,
    ) -> Self {
        Self {
            socket,
            _permit: permit,
            limits,
            deadline: Box::pin(sleep(limits.window)),
            moved: 0,
            expires: Instant::now() + limits.lifetime,
            forced: Box::pin(async move {
                while !*force.borrow_and_update() {
                    if force.changed().await.is_err() {
                        break;
                    }
                }
            }),
            closed: false,
        }
    }

    fn available(&mut self, context: &mut Context<'_>) -> io::Result<()> {
        // A window that moved enough data starts the next one; polling the
        // reset timer again registers this task for its new deadline.
        while !self.closed && self.deadline.as_mut().poll(context).is_ready() {
            let now = Instant::now();
            if self.moved < self.limits.window_bytes || now >= self.expires {
                self.closed = true;
                break;
            }
            self.moved = 0;
            let next = (now + self.limits.window).min(self.expires);
            self.deadline.as_mut().reset(next);
        }
        if self.closed || self.forced.as_mut().poll(context).is_ready() {
            self.closed = true;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP socket stalled, or its lifetime or shutdown deadline was exceeded",
            ));
        }
        Ok(())
    }

    fn moved(&mut self, bytes: usize) {
        self.moved = self.moved.saturating_add(bytes as u64);
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.available(context)?;
        let before = buffer.filled().len();
        let result = Pin::new(&mut this.socket).poll_read(context, buffer);
        this.moved(buffer.filled().len() - before);
        result
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.available(context)?;
        let result = Pin::new(&mut this.socket).poll_write(context, buffer);
        if let Poll::Ready(Ok(written)) = &result {
            this.moved(*written);
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.available(context)?;
        Pin::new(&mut this.socket).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.available(context)?;
        Pin::new(&mut this.socket).poll_shutdown(context)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.available(context)?;
        let result = Pin::new(&mut this.socket).poll_write_vectored(context, buffers);
        if let Poll::Ready(Ok(written)) = &result {
            this.moved(*written);
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.socket.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::serve::Listener as _;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        time::timeout,
    };

    async fn pair(window: Duration) -> (Stream, TcpStream, watch::Sender<bool>, Arc<Semaphore>) {
        pair_with(Limits {
            window,
            ..Limits::default()
        })
        .await
    }

    async fn pair_with(limits: Limits) -> (Stream, TcpStream, watch::Sender<bool>, Arc<Semaphore>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().try_acquire_owned().unwrap();
        let (force, forced) = watch::channel(false);
        (
            Stream::new(socket, permit, forced, limits),
            peer,
            force,
            slots,
        )
    }

    #[tokio::test]
    async fn idle_header_reads_expire_and_release_socket_capacity() {
        let (mut stream, _peer, _force, slots) = pair(Duration::from_millis(20)).await;
        assert_eq!(slots.available_permits(), 0);
        let error = timeout(Duration::from_secs(2), stream.read(&mut [0]))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        // Repeated polls must return errors, never re-poll a completed future.
        assert_eq!(
            stream.read(&mut [0]).await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        drop(stream);
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn steady_transfers_outlive_a_window_but_trickles_and_long_lives_do_not() {
        let limits = Limits {
            window: Duration::from_millis(250),
            window_bytes: 16,
            lifetime: Duration::from_secs(60),
        };
        // Sixteen bytes per window keep the socket open across many windows.
        let (mut stream, mut peer, _force, _slots) = pair_with(limits).await;
        let sender = tokio::spawn(async move {
            for _ in 0..40 {
                if peer.write_all(&[7; 16]).await.is_err() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
            peer
        });
        let mut received = [0; 40 * 16];
        timeout(Duration::from_secs(5), stream.read_exact(&mut received))
            .await
            .unwrap()
            .unwrap();
        // The same socket is closed one window after the transfer slows to a trickle.
        let mut peer = sender.await.unwrap();
        peer.write_all(b"x").await.unwrap();
        let mut rest = Vec::new();
        let error = timeout(Duration::from_secs(2), stream.read_to_end(&mut rest))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        // The absolute lifetime ends even a transfer that is fast enough.
        let (mut stream, mut peer, _force, _slots) = pair_with(Limits {
            lifetime: Duration::from_millis(600),
            ..limits
        })
        .await;
        tokio::spawn(async move {
            while peer.write_all(&[7; 64]).await.is_ok() {
                sleep(Duration::from_millis(5)).await;
            }
        });
        let error = timeout(Duration::from_secs(5), async {
            loop {
                if let Err(error) = stream.read(&mut [0; 64]).await {
                    return error;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn force_shutdown_wakes_pending_reads_and_closes_writes() {
        let (mut stream, mut peer, force, _slots) = pair(Duration::from_secs(60)).await;
        peer.write_all(b"hello").await.unwrap();
        let mut buffer = [0; 5];
        stream.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"hello");
        let task = tokio::spawn(async move {
            let result = stream.read(&mut [0]).await;
            (result, stream)
        });
        tokio::task::yield_now().await;
        force.send_replace(true);
        let (result, mut stream) = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            stream.write(b"late response").await.unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test]
    async fn admission_drops_excess_connections_before_reading_headers() {
        let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let (_force, forced) = watch::channel(false);
        let mut listener = Listener::new(socket, forced);
        listener.slots = Arc::new(Semaphore::new(1));
        let first_peer = TcpStream::connect(address).await.unwrap();
        let (first, _) = listener.accept().await;
        let mut excess = TcpStream::connect(address).await.unwrap();
        let accepting = tokio::spawn(async move { listener.accept().await });
        assert_eq!(
            timeout(Duration::from_secs(2), excess.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        drop(first);
        drop(first_peer);
        let mut next_peer = TcpStream::connect(address).await.unwrap();
        let (mut next, _) = timeout(Duration::from_secs(2), accepting)
            .await
            .unwrap()
            .unwrap();
        next_peer.write_all(b"x").await.unwrap();
        assert_eq!(next.read_u8().await.unwrap(), b'x');
    }

    #[tokio::test]
    async fn force_shutdown_terminates_stalled_response_writes() {
        let (mut stream, _peer, force, _slots) = pair(Duration::from_secs(60)).await;
        let task = tokio::spawn(async move {
            let bytes = vec![0; 64 * 1024];
            loop {
                if let Err(error) = stream.write_all(&bytes).await {
                    return error;
                }
            }
        });
        tokio::task::yield_now().await;
        force.send_replace(true);
        let error = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
