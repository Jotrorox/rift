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
    time::{Sleep, sleep},
};

const MAX_CONNECTIONS: usize = 64;
const SOCKET_LIFETIME: Duration = Duration::from_secs(10);

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
                        Stream::new(socket, permit, self.force.clone(), SOCKET_LIFETIME),
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

pub struct Stream {
    socket: TcpStream,
    _permit: OwnedSemaphorePermit,
    deadline: Pin<Box<Sleep>>,
    forced: Pin<Box<dyn Future<Output = ()> + Send>>,
    closed: bool,
}

impl Stream {
    fn new(
        socket: TcpStream,
        permit: OwnedSemaphorePermit,
        mut force: watch::Receiver<bool>,
        lifetime: Duration,
    ) -> Self {
        Self {
            socket,
            _permit: permit,
            deadline: Box::pin(sleep(lifetime)),
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
        if self.closed
            || self.deadline.as_mut().poll(context).is_ready()
            || self.forced.as_mut().poll(context).is_ready()
        {
            self.closed = true;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP socket lifetime or shutdown deadline exceeded",
            ));
        }
        Ok(())
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
        Pin::new(&mut this.socket).poll_read(context, buffer)
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
        Pin::new(&mut this.socket).poll_write(context, buffer)
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
        Pin::new(&mut this.socket).poll_write_vectored(context, buffers)
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

    async fn pair(lifetime: Duration) -> (Stream, TcpStream, watch::Sender<bool>, Arc<Semaphore>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().try_acquire_owned().unwrap();
        let (force, forced) = watch::channel(false);
        (
            Stream::new(socket, permit, forced, lifetime),
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
