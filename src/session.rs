//! The client socket belongs to a session. Backends are replaceable attachments,
//! with their own framing, compression and protocol phases.
use crate::protocol::{
    self, Codec, ConnectionState, Direction, Handshake, NextState, Packet, PacketKind,
    ProtocolVersion, Reader, State, read_varint, write_varint,
};
use std::io;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub struct Connection<S> {
    pub io: S,
    pub state: ConnectionState,
    pub codec: Codec,
    reader: Reader,
}

impl<S> Connection<S> {
    fn new(io: S, state: State) -> Self {
        Self {
            io,
            state: ConnectionState::new(state),
            codec: Codec::default(),
            reader: Reader::default(),
        }
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

    /// Forward one packet. Partial reads survive cancellation. A backend EOF is
    /// returned to the owner so it can attach another backend or disconnect.
    /// Do not cancel this future during writes and then resume the session.
    pub async fn forward(&mut self) -> io::Result<SessionEvent> {
        let version = self.version()?;
        let backend = self
            .backend
            .as_mut()
            .ok_or_else(|| protocol::invalid("no backend attached"))?;
        enum Incoming {
            Client(Option<Packet>),
            Backend(Option<Packet>),
        }
        let incoming = tokio::select! {
            packet = self.client.reader.read(&mut self.client.io, self.client.codec), if !self.client_eof => Incoming::Client(packet?),
            packet = backend.reader.read(&mut backend.io, backend.codec) => Incoming::Backend(packet?),
        };
        match incoming {
            Incoming::Client(None) => {
                self.client_eof = true;
                backend.io.shutdown().await?;
                Ok(SessionEvent::ClientClosed)
            }
            Incoming::Backend(None) => {
                self.backend = None;
                Ok(SessionEvent::BackendClosed)
            }
            Incoming::Client(Some(packet)) => {
                let kind = self
                    .client
                    .state
                    .observe(version, Direction::Serverbound, &packet)?;
                if kind == PacketKind::LoginStart {
                    self.login_start = Some(packet.clone());
                }
                backend
                    .state
                    .observe(version, Direction::Serverbound, &packet)?;
                backend.codec.write(&mut backend.io, &packet).await?;
                Ok(SessionEvent::Packet)
            }
            Incoming::Backend(Some(packet)) => {
                let kind = backend
                    .state
                    .observe(version, Direction::Clientbound, &packet)?;
                match kind {
                    PacketKind::SetCompression => {
                        let threshold = read_varint(&mut packet.data.as_slice())?;
                        backend.codec.set_compression(threshold);
                        // Negotiate once on initial login. Later backends have
                        // independent settings and are transcoded to this codec.
                        self.send_client(&packet).await?;
                        self.client.codec.set_compression(threshold);
                    }
                    PacketKind::EncryptionRequest => return Err(encryption_unsupported()),
                    PacketKind::LoginSuccess => {
                        self.identity = Some(protocol::login_identity(version, &packet)?);
                        self.send_client(&packet).await?;
                    }
                    PacketKind::Disconnect => {
                        self.send_client(&packet).await?;
                        self.backend = None;
                        return Ok(SessionEvent::Disconnected);
                    }
                    _ => self.send_client(&packet).await?,
                }
                Ok(SessionEvent::Packet)
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
