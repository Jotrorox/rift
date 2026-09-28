//! The client socket belongs to a session. Backends are replaceable attachments,
//! with their own framing, compression and protocol phases.
use crate::protocol::{
    self, Codec, ConnectionState, Direction, Handshake, NextState, Packet, PacketKind,
    PlayerIdentity, ProtocolVersion, Reader, State, read_varint, write_varint,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    Packet,
    ClientClosed,
    BackendClosed,
    BackendFailed,
    ProxyCommand(String),
    Disconnected,
}

pub struct Session<C, B> {
    pub client: Connection<C>,
    backend: Option<Connection<B>>,
    pub handshake: Handshake,
    version: Option<ProtocolVersion>,
    login_start: Option<Packet>,
    identity: Option<PlayerIdentity>,
    network: bool,
    joined: bool,
    switch_in_progress: bool,
    backend_error: Option<io::Error>,
    client_information: Option<Packet>,
    client_brand: Option<Packet>,
    bundle_open: bool,
    initial_retry_safe: bool,
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
            network: false,
            joined: false,
            switch_in_progress: false,
            backend_error: None,
            client_information: None,
            client_brand: None,
            bundle_open: false,
            initial_retry_safe: true,
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

    pub fn switch_supported(&self) -> bool {
        self.handshake.protocol == 774
    }

    pub fn enable_network(&mut self) -> io::Result<()> {
        if !self.switch_supported() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Network switching requires Minecraft 1.21.11.",
            ));
        }
        self.network = true;
        Ok(())
    }

    pub fn identity(&self) -> Option<PlayerIdentity> {
        self.identity.clone()
    }

    pub fn can_switch(&self) -> bool {
        self.switch_supported()
            && self.joined
            && !self.switch_in_progress
            && !self.client_eof
            && self.client.state.settled(State::Play)
    }

    pub fn switch_in_progress(&self) -> bool {
        self.switch_in_progress
    }

    pub fn last_backend_error(&self) -> Option<&io::Error> {
        self.backend_error.as_ref()
    }

    /// Read Login Start before choosing a backend. The UUID is an untrusted
    /// client claim; the confirmed offline backend identity arrives at success.
    pub async fn read_login_start(&mut self) -> io::Result<PlayerIdentity> {
        let version = self.version()?;
        if let Some(packet) = &self.login_start {
            return protocol::start_identity(version, packet);
        }
        if !self.client.state.settled(State::Login) {
            return Err(protocol::invalid("not in login"));
        }
        let packet = self
            .client
            .reader
            .read(&mut self.client.io, self.client.codec)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing login start"))?;
        if self
            .client
            .state
            .observe(version, Direction::Serverbound, &packet)?
            != PacketKind::LoginStart
        {
            return Err(protocol::invalid("expected Login Start"));
        }
        let identity = protocol::start_identity(version, &packet)?;
        self.login_start = Some(packet);
        Ok(identity)
    }

    /// Only a transport failure before Login Success can retry initial login.
    /// A backend's explicit Disconnect closes client state and is never retried.
    pub fn reset_initial_backend(&mut self) -> bool {
        if self.identity.is_none()
            && !self.client_eof
            && self.initial_retry_safe
            && self.client.state.settled(State::Login)
            && !self.switch_in_progress
        {
            self.backend = None;
            true
        } else {
            false
        }
    }

    /// Preflight replacement login while preserving the current backend. A
    /// timeout before switch_in_progress() becomes true can safely resume play.
    /// Once the transition starts the owner must disconnect on cancellation:
    /// direct transition writes may be partial. Callers must supply a deadline.
    pub async fn connect_backend(&mut self, stream: B) -> io::Result<()> {
        let version = self.version()?;
        if self.handshake.next_state == NextState::Status {
            return Err(protocol::invalid(
                "status sessions do not attach gameplay backends",
            ));
        }
        let replacing = self.identity.is_some();
        if replacing && !self.can_switch() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Backend switching requires a joined Minecraft 1.21.11 player.",
            ));
        }
        if !replacing && (self.backend.is_some() || !self.client.state.settled(State::Login)) {
            return Err(protocol::invalid("backend login is already in progress"));
        }
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
        if replacing {
            let expected = self.identity.clone().unwrap();
            {
                let login = Self::login_replacement(&mut backend, version, &expected);
                tokio::pin!(login);
                loop {
                    tokio::select! {
                        result = &mut login => { result?; break; }
                        event = self.forward(), if self.backend.is_some() => match event? {
                            SessionEvent::Packet | SessionEvent::ProxyCommand(_) => {}
                            SessionEvent::BackendClosed | SessionEvent::BackendFailed => {}
                            SessionEvent::ClientClosed => return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof, "client closed during replacement login")),
                            SessionEvent::Disconnected => return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied, "The current backend disconnected the player.")),
                        }
                    }
                }
            }
            // A ready Login Success must not win the select and discard an
            // already queued kick on the old connection. Establish a bounded
            // quiescent cutover before changing the client's protocol state.
            self.drain_before_switch().await?;
            // The old server can initiate its own configuration while login is
            // in flight; never splice a second transition into that exchange.
            if !self.can_switch() {
                return Err(protocol::invalid(
                    "client state changed during replacement login",
                ));
            }
            // All errors above leave the old attachment and client world intact.
            self.switch_in_progress = true;
            self.client.flush_pending().await?;
            // A failed backend may have left an unfinished bundle on the wire.
            // Close it before the unbundled configuration transition.
            if self.bundle_open {
                self.send_client(&Packet::empty(0)).await?;
                self.bundle_open = false;
            }
            self.send_client(&Packet::empty(version.start_configuration().unwrap()))
                .await?;
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
                // All old-world packets, including chat/session updates and
                // acknowledgements, belong to the retired backend. Never replay
                // them to a new chat chain. Cache only persistent client options.
                self.remember_client_packet(State::Play, &packet)?;
            }
            // New play listener + Join Game reset entities, chunks, scoreboard,
            // tab list, bossbars, signed-message encoder and last-seen tracker.
            // Mojang 1.21.11 ClientPacketListener.handleConfigurationStart /
            // handleLogin; ClientConfigurationPacketListenerImpl.finish.
            self.joined = false;
            if let Some(settings) = &self.client_information {
                backend.codec.write(&mut backend.io, settings).await?;
            }
            if let Some(brand) = &self.client_brand {
                backend.codec.write(&mut backend.io, brand).await?;
            }
            // Resource packs are common-listener state and survive a fresh play
            // listener. Pop the old server's stack before new configuration.
            self.send_client(&Packet::new(0x08, vec![0])).await?;
        }
        self.backend = Some(backend);
        self.backend_error = None;
        Ok(())
    }

    async fn drain_before_switch(&mut self) -> io::Result<()> {
        // Bound both work and CPU time. A continuously busy source keeps its
        // attachment instead of starving preflight or hiding a queued ban.
        const MAX_EVENTS: usize = 128;
        let started = std::time::Instant::now();
        for _ in 0..MAX_EVENTS {
            if self.backend.is_none() {
                return Ok(());
            }
            if started.elapsed() >= std::time::Duration::from_millis(20) {
                break;
            }
            let ready = {
                // Tokio's cooperative budget can return Pending with socket
                // data still ready. It is not evidence of quiescence. This
                // single bounded poll supplies its own work/time budget.
                let forward = tokio::task::unconstrained(self.forward());
                tokio::pin!(forward);
                poll_fn(|cx| Poll::Ready(forward.as_mut().poll(cx))).await
            };
            match ready {
                Poll::Ready(Ok(SessionEvent::Packet | SessionEvent::ProxyCommand(_))) => {}
                Poll::Ready(Ok(SessionEvent::BackendClosed | SessionEvent::BackendFailed)) => {
                    return Ok(());
                }
                Poll::Ready(Ok(SessionEvent::Disconnected)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "The current backend disconnected the player.",
                    ));
                }
                Poll::Ready(Ok(SessionEvent::ClientClosed)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "client closed during replacement login",
                    ));
                }
                Poll::Ready(Err(error)) => return Err(error),
                Poll::Pending => {
                    if self
                        .client
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.kind == PacketKind::Disconnect)
                    {
                        // The decoded ban is authoritative even if delivering
                        // it is backpressured. The owner will flush it on close.
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "The current backend disconnected the player.",
                        ));
                    }
                    if self.client.pending.is_some() {
                        // Backpressure disables backend reads while this frame
                        // is pending. Flush it, then inspect the backend again;
                        // the caller's deadline bounds this cancel-safe write.
                        self.client.flush_pending().await?;
                        continue;
                    }
                    if self
                        .backend
                        .as_ref()
                        .is_some_and(|backend| backend.reader.has_partial_frame())
                    {
                        // A partially received packet can itself be a ban. Leave
                        // it with the old attachment until the next attempt.
                        break;
                    }
                    return Ok(());
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "The current server is busy. Please retry the switch.",
        ))
    }

    async fn login_replacement(
        backend: &mut Connection<B>,
        version: ProtocolVersion,
        expected: &PlayerIdentity,
    ) -> io::Result<()> {
        loop {
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
                    if protocol::success_identity(version, &packet)? != *expected {
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
                PacketKind::Disconnect => {
                    // A ban/whitelist rejection is authoritative. Preserve its
                    // text for the requesting player and do not try another hub.
                    let reason = protocol::read_string(&mut packet.data.as_slice(), 32767)
                        .unwrap_or("The destination server denied access.")
                        .to_owned();
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
                }
                PacketKind::LoginPluginRequest => {
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
                _ => return Err(protocol::invalid("unsupported replacement login exchange")),
            }
        }
    }

    fn remember_client_packet(&mut self, state: State, packet: &Packet) -> io::Result<()> {
        if !self.network {
            return Ok(());
        }
        if matches!(
            (state, packet.id),
            (State::Configuration, 0) | (State::Play, 0x0d)
        ) {
            self.client_information = Some(Packet::new(0, packet.data.clone()));
        }
        if matches!(
            (state, packet.id),
            (State::Configuration, 2) | (State::Play, 0x15)
        ) {
            let mut bytes = packet.data.as_slice();
            if protocol::read_string(&mut bytes, 32767)? == "minecraft:brand" {
                // Avoid retaining arbitrary large mod payloads across backends.
                protocol::read_string(&mut bytes, 32767)?;
                if !bytes.is_empty() {
                    return Err(protocol::invalid("invalid client brand"));
                }
                self.client_brand = Some(Packet::new(2, packet.data.clone()));
            }
        }
        Ok(())
    }

    pub async fn send_system_message(&mut self, message: &str) -> io::Result<()> {
        self.client.flush_pending().await?;
        if !self.client.state.settled(State::Play) {
            return Err(protocol::invalid("system messages require play state"));
        }
        self.send_client(&protocol::system_message(self.version()?, message)?)
            .await
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
                Client(io::Result<ConnectionEvent>),
                Backend(io::Result<ConnectionEvent>),
            }
            let read_client = !self.client_eof && backend.pending.is_none();
            let read_backend = self.client.pending.is_none();
            let incoming = tokio::select! {
                event = self.client.next(read_client) => Incoming::Client(event),
                event = backend.next(read_backend) => Incoming::Backend(event),
            };
            match incoming {
                Incoming::Client(Err(error)) => return Err(error),
                Incoming::Backend(Err(error)) => {
                    self.backend_error = Some(error);
                    self.backend = None;
                    return Ok(SessionEvent::BackendFailed);
                }
                Incoming::Client(Ok(ConnectionEvent::Written(PacketKind::Disconnect))) => {
                    self.backend = None;
                    return Ok(SessionEvent::Disconnected);
                }
                Incoming::Client(Ok(ConnectionEvent::Written(PacketKind::JoinGame))) => {
                    self.joined = true;
                    self.switch_in_progress = false;
                    return Ok(SessionEvent::Packet);
                }
                Incoming::Client(Ok(ConnectionEvent::Written(_)))
                | Incoming::Backend(Ok(ConnectionEvent::Written(_))) => {
                    return Ok(SessionEvent::Packet);
                }
                Incoming::Client(Ok(ConnectionEvent::Read(None))) => {
                    self.client_eof = true;
                    let _ = backend.io.shutdown().await;
                    return Ok(SessionEvent::ClientClosed);
                }
                Incoming::Backend(Ok(ConnectionEvent::Read(None))) => {
                    self.backend = None;
                    return Ok(SessionEvent::BackendClosed);
                }
                Incoming::Client(Ok(ConnectionEvent::Read(Some(packet)))) => {
                    let phase = self.client.state.phase(Direction::Serverbound);
                    let kind =
                        self.client
                            .state
                            .observe(version, Direction::Serverbound, &packet)?;
                    if kind == PacketKind::LoginStart {
                        self.login_start = Some(packet.clone());
                    }
                    self.remember_client_packet(phase, &packet)?;
                    let backend = self.backend.as_mut().unwrap();
                    if self.network
                        && phase == State::Play
                        && self.joined
                        && let Some((command, acknowledgement)) = protocol::proxy_command(&packet)?
                    {
                        if let Some(acknowledgement) = acknowledgement {
                            backend.queue(version, Direction::Serverbound, &acknowledgement)?;
                        }
                        return Ok(SessionEvent::ProxyCommand(command));
                    }
                    backend.queue(version, Direction::Serverbound, &packet)?;
                }
                Incoming::Backend(Ok(ConnectionEvent::Read(Some(packet)))) => {
                    let phase = backend.state.phase(Direction::Clientbound);
                    let kind = backend
                        .state
                        .observe(version, Direction::Clientbound, &packet)?;
                    if matches!(
                        kind,
                        PacketKind::LoginPluginRequest | PacketKind::CookieRequest
                    ) {
                        self.initial_retry_safe = false;
                    }
                    match kind {
                        PacketKind::SetCompression => {
                            let threshold = read_varint(&mut packet.data.as_slice())?;
                            backend.codec.set_compression(threshold);
                            // Initial retries may use another compression threshold.
                            // The client's negotiated codec belongs to the session.
                            if self.client.codec.threshold().is_none() {
                                self.client
                                    .queue(version, Direction::Clientbound, &packet)?;
                                self.client.codec.set_compression(threshold);
                            } else {
                                return Ok(SessionEvent::Packet);
                            }
                        }
                        PacketKind::EncryptionRequest => return Err(encryption_unsupported()),
                        PacketKind::LoginSuccess => {
                            let identity = protocol::success_identity(version, &packet)?;
                            let requested = protocol::start_identity(
                                version,
                                self.login_start
                                    .as_ref()
                                    .ok_or_else(|| protocol::invalid("missing login start"))?,
                            )?;
                            if identity.name != requested.name {
                                return Err(protocol::invalid("backend changed the player name"));
                            }
                            self.identity = Some(identity);
                            self.client
                                .queue(version, Direction::Clientbound, &packet)?;
                        }
                        PacketKind::JoinGame => {
                            if self.network {
                                protocol::validate_network_join(&packet)?;
                            }
                            self.client
                                .queue(version, Direction::Clientbound, &packet)?;
                        }
                        PacketKind::StartConfiguration => {
                            self.joined = false;
                            self.client
                                .queue(version, Direction::Clientbound, &packet)?;
                        }
                        _ => {
                            let packet =
                                if self.network && phase == State::Play && packet.id == 0x10 {
                                    protocol::network_commands(&packet)?
                                } else {
                                    packet
                                };
                            if self.network && phase == State::Play && packet.id == 0 {
                                if !packet.data.is_empty() {
                                    return Err(protocol::invalid("invalid bundle delimiter"));
                                }
                                self.bundle_open = !self.bundle_open;
                            }
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
