//! The client socket belongs to a session. Backends are replaceable attachments,
//! with their own framing, compression and protocol phases.
use crate::protocol::{
    self, Codec, ConnectionState, Direction, Handshake, NextState, Packet, PacketKind,
    ProtocolVersion, Reader, State, read_varint, write_varint,
};
use std::{
    future::{Future, poll_fn},
    io,
    pin::Pin,
    task::Poll,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub struct Connection<S> {
    pub io: S,
    pub state: ConnectionState,
    pub codec: Codec,
    reader: Reader,
    pending: Option<PendingWrite>,
}

struct PendingWrite {
    frame: Vec<u8>,
    offset: usize,
    kind: PacketKind,
}

enum ConnectionEvent {
    Read(Option<Packet>),
    Written(PacketKind),
}

impl<S> Connection<S> {
    fn new(io: S, state: State) -> Self {
        Self {
            io,
            state: ConnectionState::new(state),
            codec: Codec::default(),
            reader: Reader::default(),
            pending: None,
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Connection<S> {
    fn queue(
        &mut self,
        version: ProtocolVersion,
        direction: Direction,
        packet: &Packet,
    ) -> io::Result<PacketKind> {
        debug_assert!(self.pending.is_none());
        let mut state = self.state.clone();
        let kind = state.observe(version, direction, packet)?;
        let frame = self.codec.encode(packet)?;
        self.pending = Some(PendingWrite {
            frame,
            offset: 0,
            kind,
        });
        self.state = state;
        Ok(kind)
    }

    // Poll writes and reads on the same socket without holding one direction
    // hostage to the other's backpressure. Each endpoint buffers one write.
    async fn next(&mut self, read: bool) -> io::Result<ConnectionEvent> {
        poll_fn(|cx| {
            if let Some(pending) = &mut self.pending {
                while pending.offset < pending.frame.len() {
                    match Pin::new(&mut self.io).poll_write(cx, &pending.frame[pending.offset..]) {
                        Poll::Ready(Ok(0)) => {
                            return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                        }
                        Poll::Ready(Ok(size)) => pending.offset += size,
                        Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                        Poll::Pending => break,
                    }
                }
                if pending.offset == pending.frame.len() {
                    let kind = self.pending.take().unwrap().kind;
                    return Poll::Ready(Ok(ConnectionEvent::Written(kind)));
                }
            }
            // Control changes become visible to incoming packets only after
            // their frame is delivered, including compression and phase changes.
            if read
                && self
                    .pending
                    .as_ref()
                    .is_none_or(|pending| pending.kind == PacketKind::Unknown)
            {
                let read = self.reader.read(&mut self.io, self.codec);
                return std::pin::pin!(read)
                    .poll(cx)
                    .map(|result| result.map(ConnectionEvent::Read));
            }
            Poll::Pending
        })
        .await
    }

    async fn flush_pending(&mut self) -> io::Result<()> {
        if self.pending.is_some() {
            self.next(false).await?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    Packet,
    ClientClosed,
    BackendClosed,
    Disconnected,
}

pub struct Session<C, B> {
    pub client: Connection<C>,
    backend: Option<Connection<B>>,
    pub handshake: Handshake,
    version: Option<ProtocolVersion>,
    login_start: Option<Packet>,
    identity: Option<Vec<u8>>,
    client_eof: bool,
    read_chunk_size: usize,
}

impl<C: AsyncRead + AsyncWrite + Unpin, B: AsyncRead + AsyncWrite + Unpin> Session<C, B> {
    pub async fn accept(mut client: C) -> io::Result<Self> {
        let body = Reader::default()
            .read_frame(&mut client, 2048)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing handshake"))?;
        let handshake = Handshake::decode(&Packet::from_body(&body)?)?;
        let mut client = Connection::new(client, State::Handshake);
        client.state.accept_handshake(&handshake)?;
        Ok(Self {
            client,
            version: ProtocolVersion::new(handshake.protocol).ok(),
            backend: None,
            handshake,
            login_start: None,
            identity: None,
            client_eof: false,
            read_chunk_size: 32 * 1024,
        })
    }

    /// Tune socket read granularity without changing the packet size limits.
    pub fn set_read_chunk_size(&mut self, size: usize) {
        self.read_chunk_size = size;
        self.client.reader.set_read_chunk_size(size);
        if let Some(backend) = &mut self.backend {
            backend.reader.set_read_chunk_size(size);
        }
    }

    pub fn version(&self) -> io::Result<ProtocolVersion> {
        let version = ProtocolVersion::new(self.handshake.protocol)?;
        if self.handshake.next_state == NextState::Transfer && !version.has_transfer() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Transfer requires Minecraft 1.20.5 or newer.",
            ));
        }
        Ok(version)
    }

    pub fn backend(&self) -> Option<&Connection<B>> {
        self.backend.as_ref()
    }

    /// The validated login name, for operator display; it is not authenticated.
    pub fn player_name(&self) -> Option<&str> {
        self.login_start
            .as_ref()
            .and_then(|packet| protocol::read_string(&mut packet.data.as_slice(), 16).ok())
    }

    /// Attach a connected backend without surrendering ownership of the client.
    /// A replacement logs in independently while the client re-enters
    /// configuration. Callers must bound this operation with a deadline; on a
    /// timeout the session must be disconnected, since a write may be partial.
    pub async fn connect_backend(&mut self, stream: B) -> io::Result<()> {
        let version = self.version()?;
        if self.handshake.next_state == NextState::Status {
            return Err(protocol::invalid(
                "status sessions do not attach gameplay backends",
            ));
        }
        let replacing = self.identity.is_some();
        if replacing {
            if !version.has_configuration()
                || !self.client.state.settled(State::Play)
                || self.client_eof
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Backend switching requires a client in play on Minecraft 1.20.2 or newer.",
                ));
            }
            self.client.flush_pending().await?;
            let start = Packet::empty(version.start_configuration().unwrap());
            self.send_client(&start).await?;
            loop {
                let packet = self
                    .client
                    .reader
                    .read(&mut self.client.io, self.client.codec)
                    .await?
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "client closed during backend switch",
                        )
                    })?;
                let kind = self
                    .client
                    .state
                    .observe(version, Direction::Serverbound, &packet)?;
                if kind == PacketKind::ConfigurationAcknowledged {
                    break;
                }
                // Drain old-world packets up to the transition acknowledgement.
            }
        } else if self.backend.is_some() || !self.client.state.settled(State::Login) {
            return Err(protocol::invalid("backend login is already in progress"));
        }
        // Dropping a backend never drops, resets or replaces the client codec.
        self.backend = None;
        let mut backend = Connection::new(stream, State::Login);
        backend.reader.set_read_chunk_size(self.read_chunk_size);
        let mut handshake = self.handshake.clone();
        if replacing {
            handshake.next_state = NextState::Login;
        }
        backend
            .codec
            .write(&mut backend.io, &handshake.packet())
            .await?;
        if let Some(start) = &self.login_start {
            backend
                .state
                .observe(version, Direction::Serverbound, start)?;
            backend.codec.write(&mut backend.io, start).await?;
        }
        self.backend = Some(backend);
        if replacing {
            self.login_replacement(version).await?;
        }
        Ok(())
    }

    async fn login_replacement(&mut self, version: ProtocolVersion) -> io::Result<()> {
        loop {
            let backend = self.backend.as_mut().unwrap();
            let packet = backend
                .reader
                .read(&mut backend.io, backend.codec)
                .await?
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "replacement backend closed during login",
                    )
                })?;
            match backend
                .state
                .observe(version, Direction::Clientbound, &packet)?
            {
                PacketKind::SetCompression => backend
                    .codec
                    .set_compression(read_varint(&mut packet.data.as_slice())?),
                PacketKind::LoginSuccess => {
                    if Some(protocol::login_identity(version, &packet)?) != self.identity {
                        return Err(protocol::invalid(
                            "replacement backend changed the player identity",
                        ));
                    }
                    let acknowledged = Packet::empty(3);
                    backend
                        .state
                        .observe(version, Direction::Serverbound, &acknowledged)?;
                    backend.codec.write(&mut backend.io, &acknowledged).await?;
                    return Ok(());
                }
                PacketKind::LoginPluginRequest => {
                    // Login plugins cannot be forwarded to a client already in
                    // configuration. Explicitly report unsupported to the backend.
                    let id = read_varint(&mut packet.data.as_slice())?;
                    let mut data = Vec::new();
                    write_varint(id, &mut data);
                    data.push(0);
                    backend
                        .codec
                        .write(&mut backend.io, &Packet::new(2, data))
                        .await?;
                }
                PacketKind::EncryptionRequest => return Err(encryption_unsupported()),
                _ => {
                    return Err(protocol::invalid(
                        "replacement backend rejected login or requested an unsupported login exchange",
                    ));
                }
            }
        }
    }

    async fn send_client(&mut self, packet: &Packet) -> io::Result<()> {
        let version = self.version()?;
        // Commit the state only after the complete packet has been written.
        let mut state = self.client.state.clone();
        state.observe(version, Direction::Clientbound, packet)?;
        self.client.codec.write(&mut self.client.io, packet).await?;
        self.client.state = state;
        Ok(())
    }

    /// Complete one forwarded packet. Partial reads and writes survive cancellation.
    /// A backend EOF returns to the owner to attach another backend or disconnect.
    pub async fn forward(&mut self) -> io::Result<SessionEvent> {
        let version = self.version()?;
        loop {
            let backend = self
                .backend
                .as_mut()
                .ok_or_else(|| protocol::invalid("no backend attached"))?;
            enum Incoming {
                Client(ConnectionEvent),
                Backend(ConnectionEvent),
            }
            let read_client = !self.client_eof && backend.pending.is_none();
            let read_backend = self.client.pending.is_none();
            let incoming = tokio::select! {
                event = self.client.next(read_client) => Incoming::Client(event?),
                event = backend.next(read_backend) => Incoming::Backend(event?),
            };
            match incoming {
                Incoming::Client(ConnectionEvent::Written(PacketKind::Disconnect)) => {
                    self.backend = None;
                    return Ok(SessionEvent::Disconnected);
                }
                Incoming::Client(ConnectionEvent::Written(_))
                | Incoming::Backend(ConnectionEvent::Written(_)) => {
                    return Ok(SessionEvent::Packet);
                }
                Incoming::Client(ConnectionEvent::Read(None)) => {
                    self.client_eof = true;
                    backend.io.shutdown().await?;
                    return Ok(SessionEvent::ClientClosed);
                }
                Incoming::Backend(ConnectionEvent::Read(None)) => {
                    self.backend = None;
                    return Ok(SessionEvent::BackendClosed);
                }
                Incoming::Client(ConnectionEvent::Read(Some(packet))) => {
                    let kind =
                        self.client
                            .state
                            .observe(version, Direction::Serverbound, &packet)?;
                    if kind == PacketKind::LoginStart {
                        self.login_start = Some(packet.clone());
                    }
                    backend.queue(version, Direction::Serverbound, &packet)?;
                }
                Incoming::Backend(ConnectionEvent::Read(Some(packet))) => {
                    let kind = backend
                        .state
                        .observe(version, Direction::Clientbound, &packet)?;
                    match kind {
                        PacketKind::SetCompression => {
                            let threshold = read_varint(&mut packet.data.as_slice())?;
                            backend.codec.set_compression(threshold);
                            // Negotiate once on initial login. Later backends have
                            // independent settings and are transcoded to this codec.
                            self.client
                                .queue(version, Direction::Clientbound, &packet)?;
                            self.client.codec.set_compression(threshold);
                        }
                        PacketKind::EncryptionRequest => return Err(encryption_unsupported()),
                        PacketKind::LoginSuccess => {
                            self.identity = Some(protocol::login_identity(version, &packet)?);
                            self.client
                                .queue(version, Direction::Clientbound, &packet)?;
                        }
                        _ => {
                            self.client
                                .queue(version, Direction::Clientbound, &packet)?;
                        }
                    }
                }
            }
        }
    }

    pub async fn run(&mut self) -> io::Result<SessionEvent> {
        loop {
            match self.forward().await? {
                SessionEvent::Packet => {}
                event => return Ok(event),
            }
        }
    }

    pub async fn disconnect(&mut self, reason: &str) -> io::Result<()> {
        self.client.flush_pending().await?;
        let state = self.client.state.phase(Direction::Clientbound);
        if state != State::Closed {
            let packet = protocol::disconnect(self.version, state, reason)?;
            self.client
                .codec
                .write(&mut self.client.io, &packet)
                .await?;
            self.client.state.close();
        }
        self.backend = None;
        self.client.io.shutdown().await
    }

    pub async fn status_request(&mut self) -> io::Result<()> {
        if !self.client.state.settled(State::Status) {
            return Err(protocol::invalid("not a status session"));
        }
        let frame = self
            .client
            .reader
            .read_frame(&mut self.client.io, 5)
            .await?
            .ok_or_else(|| protocol::invalid("missing status request"))?;
        let packet = Packet::from_body(&frame)?;
        if packet.id != 0 || !packet.data.is_empty() {
            return Err(protocol::invalid("expected empty status request"));
        }
        Ok(())
    }

    /// Answer a server-list request locally. A ping is echoed on this connection,
    /// independently of any backend probe or cached status document.
    pub async fn respond_status(&mut self, response: &Packet) -> io::Result<()> {
        if !self.client.state.settled(State::Status) {
            return Err(protocol::invalid("not a status session"));
        }
        self.client
            .codec
            .write(&mut self.client.io, response)
            .await?;
        if let Some(frame) = self
            .client
            .reader
            .read_frame(&mut self.client.io, 13)
            .await?
        {
            let ping = Packet::from_body(&frame)?;
            if ping.id != 1 || ping.data.len() != 8 {
                return Err(protocol::invalid("expected status ping"));
            }
            self.client.codec.write(&mut self.client.io, &ping).await?;
        }
        self.client.state.close();
        self.client.io.shutdown().await
    }
}

fn encryption_unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Backend encryption is unsupported. Rift requires an offline-mode backend.",
    )
}

#[cfg(test)]
mod tests;
