//! The client socket belongs to a session. Backends are replaceable attachments,
//! with their own framing, compression and protocol phases.
use crate::protocol::{
    self, Codec, ConnectionState, Direction, Handshake, NextState, Packet, PacketKind,
    PlayerIdentity, ProtocolVersion, Reader, State, read_varint, write_varint,
};
use crate::{
    auth::{AuthenticatedProfile, Authenticator, CryptoStream},
    forwarding::{self, Forwarding, LoginPlugins},
};
use std::{
    future::{Future, poll_fn},
    io,
    net::IpAddr,
    pin::Pin,
    sync::Arc,
    task::Poll,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

pub struct Connection<S> {
    pub io: S,
    pub state: ConnectionState,
    pub codec: Codec,
    reader: Reader,
    received: Option<Packet>,
    pending: Option<PendingWrite>,
    login_plugins: LoginPlugins,
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
            received: None,
            pending: None,
            login_plugins: LoginPlugins::default(),
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
                if let Some(packet) = self.received.take() {
                    return Poll::Ready(Ok(ConnectionEvent::Read(Some(packet))));
                }
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
    pub client: Connection<CryptoStream<C>>,
    backend: Option<Connection<B>>,
    pub handshake: Handshake,
    pub extension: Option<crate::extensions::ExtensionSession>,
    extension_commands: Vec<String>,
    version: Option<ProtocolVersion>,
    login_start: Option<Packet>,
    identity: Option<PlayerIdentity>,
    authenticated_profile: Option<AuthenticatedProfile>,
    forwarding: Option<Forwarding>,
    network: bool,
    joined: bool,
    pending_join_valid: bool,
    switch_in_progress: bool,
    backend_error: Option<io::Error>,
    client_information: Option<Packet>,
    client_brand: Option<Packet>,
    bundle_open: bool,
    initial_retry_safe: bool,
    client_eof: bool,
    read_chunk_size: usize,
    legacy_state: protocol::LegacyState,
}

impl<C: AsyncRead + AsyncWrite + Unpin, B: AsyncRead + AsyncWrite + Unpin> Session<C, B> {
    pub async fn accept(mut client: C) -> io::Result<Self> {
        let body = Reader::default()
            .read_frame(&mut client, 2048)
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing handshake"))?;
        let handshake = Handshake::decode(&Packet::from_body(&body)?)?;
        let mut client = Connection::new(CryptoStream::new(client), State::Handshake);
        client.state.accept_handshake(&handshake)?;
        Ok(Self {
            client,
            version: ProtocolVersion::new(handshake.protocol).ok(),
            backend: None,
            extension: None,
            extension_commands: Vec::new(),
            handshake,
            login_start: None,
            identity: None,
            authenticated_profile: None,
            forwarding: None,
            network: false,
            joined: false,
            pending_join_valid: false,
            switch_in_progress: false,
            backend_error: None,
            client_information: None,
            client_brand: None,
            bundle_open: false,
            initial_retry_safe: true,
            client_eof: false,
            read_chunk_size: 32 * 1024,
            legacy_state: protocol::LegacyState::default(),
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

    pub fn set_extension(&mut self, extension: crate::extensions::ExtensionSession) {
        self.extension_commands = extension.commands();
        self.extension = Some(extension);
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

    /// Mojang's account name after authentication, otherwise the client claim.
    pub fn player_name(&self) -> Option<&str> {
        if let Some(profile) = &self.authenticated_profile {
            return Some(&profile.name);
        }
        self.login_start
            .as_ref()
            .and_then(|packet| protocol::read_string(&mut packet.data.as_slice(), 16).ok())
    }

    pub fn switch_supported(&self) -> bool {
        self.version()
            .is_ok_and(ProtocolVersion::supports_switching)
    }

    pub fn enable_network(&mut self) -> io::Result<()> {
        if !self.switch_supported() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Network switching is unavailable for this Minecraft version.",
            ));
        }
        self.network = true;
        Ok(())
    }

    pub fn identity(&self) -> Option<PlayerIdentity> {
        self.identity.clone()
    }

    pub fn authenticated_profile(&self) -> Option<&AuthenticatedProfile> {
        self.authenticated_profile.as_ref()
    }

    /// Authenticate before opening any backend. The caller bounds the entire
    /// exchange with a deadline and disconnects on error or cancellation.
    pub async fn authenticate(&mut self, authenticator: &Authenticator) -> io::Result<()> {
        if self.backend.is_some()
            || self.authenticated_profile.is_some()
            || self.identity.is_some()
            || !self.client.state.settled(State::Login)
        {
            return Err(protocol::invalid(
                "authentication must precede backend login",
            ));
        }
        let requested = self.read_login_start().await?;
        let version = self.version()?;
        let profile = authenticator
            .authenticate(&mut self.client.io, version, &requested.name)
            .await?;
        self.login_start = Some(forwarding::login_start(&profile, version));
        self.authenticated_profile = Some(profile);
        Ok(())
    }

    pub fn set_forwarding(&mut self, secret: Arc<[u8]>, address: IpAddr) -> io::Result<()> {
        if self.backend.is_some() || self.identity.is_some() {
            return Err(protocol::invalid(
                "forwarding must be configured before backend login",
            ));
        }
        if self.version()?.number() < 761 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Velocity forwarding requires Minecraft 1.19.3 or newer in Rift.",
            ));
        }
        let profile = self.authenticated_profile.clone().ok_or_else(|| {
            protocol::invalid("Velocity forwarding requires an authenticated profile")
        })?;
        self.forwarding = Some(Forwarding::new(secret, address, profile)?);
        Ok(())
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
    /// client claim until authenticate() replaces it with Mojang's identity.
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
        if self.authenticated_profile.is_some() && self.forwarding.is_none() {
            return Err(protocol::invalid(
                "Authenticated backend login requires Velocity forwarding.",
            ));
        }
        if self.handshake.next_state == NextState::Status {
            return Err(protocol::invalid(
                "status sessions do not attach gameplay backends",
            ));
        }
        let replacing = self.identity.is_some();
        if replacing && !self.can_switch() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Backend switching requires a joined player on a switchable Minecraft version.",
            ));
        }
        if !replacing && (self.backend.is_some() || !self.client.state.settled(State::Login)) {
            return Err(protocol::invalid("backend login is already in progress"));
        }
        let mut backend = Connection::new(stream, State::Login);
        backend.reader.set_read_chunk_size(self.read_chunk_size);
        let mut handshake = self.handshake.clone();
        if replacing || self.authenticated_profile.is_some() {
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
            let forwarding = self.forwarding.clone();
            let legacy_join = {
                let login =
                    Self::login_replacement(&mut backend, version, &expected, forwarding.as_ref());
                tokio::pin!(login);
                loop {
                    tokio::select! {
                        result = &mut login => { break result?; }
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
            };
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
            let closed_bundle = self.bundle_open;
            if closed_bundle {
                self.send_client(&Packet::empty(
                    version.switching().unwrap().bundle_delimiter,
                ))
                .await?;
                self.bundle_open = false;
            }
            if let Some(join) = legacy_join {
                let caps = version.switching().unwrap();
                // A proxy-owned keepalive fences packets sent for the old world.
                let (request_id, reply_id) = caps.legacy_keepalive().unwrap();
                let mut random = [0; 8];
                aws_lc_rs::rand::fill(&mut random)
                    .map_err(|_| protocol::invalid("could not create transition barrier"))?;
                let challenge = if version.number() >= 340 {
                    random.to_vec()
                } else {
                    let mut data = Vec::new();
                    write_varint(
                        i32::from_be_bytes(random[..4].try_into().unwrap()),
                        &mut data,
                    );
                    data
                };
                self.send_client(&Packet::new(request_id, challenge.clone()))
                    .await?;
                loop {
                    let packet = self
                        .client
                        .reader
                        .read(&mut self.client.io, self.client.codec)
                        .await?
                        .ok_or_else(|| protocol::invalid("client closed during legacy switch"))?;
                    if packet.id == reply_id && packet.data == challenge {
                        break;
                    }
                    self.remember_client_packet(State::Play, &packet)?;
                    // A later busy-source rollback must not strand the old
                    // server's keepalive and disconnect an otherwise live player.
                    if packet.id == reply_id
                        && let Some(old) = self.backend.as_mut()
                    {
                        let result = async {
                            old.flush_pending().await?;
                            old.queue(version, Direction::Serverbound, &packet)?;
                            old.flush_pending().await
                        }
                        .await;
                        if let Err(error) = result {
                            self.backend_error = Some(error);
                            self.backend = None;
                        }
                    }
                }
                if let Err(error) = self.check_legacy_cutover().await {
                    // The keepalive did not change client state. A busy source
                    // can safely resume, including any cancel-safe queued frame.
                    if error.kind() == io::ErrorKind::WouldBlock && !closed_bundle {
                        self.switch_in_progress = false;
                    }
                    return Err(error);
                }
                for packet in self.legacy_state.reset(version)? {
                    self.send_client(&packet).await?;
                }
                let (join, respawn) = caps.legacy_world(&join)?;
                self.send_client(&join).await?;
                self.send_client(&respawn).await?;
                if let Some(settings) = &self.client_information {
                    backend.codec.write(&mut backend.io, settings).await?;
                }
                if let Some(brand) = &self.client_brand {
                    backend.codec.write(&mut backend.io, brand).await?;
                }
                self.backend = Some(backend);
                self.backend_error = None;
                self.joined = true;
                self.switch_in_progress = false;
                return Ok(());
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
            // Supported clients: ClientPacketListener.handleConfigurationStart /
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
            let pop = version.switching().unwrap().config_pack_pop;
            if pop >= 0 {
                self.send_client(&Packet::new(pop, vec![0])).await?;
            }
        }
        self.backend = Some(backend);
        self.backend_error = None;
        Ok(())
    }

    async fn check_legacy_cutover(&mut self) -> io::Result<()> {
        let version = self.version()?;
        let Some(old) = self.backend.as_mut() else {
            return Ok(());
        };
        // Nothing may be forwarded to the client after its barrier reply:
        // responses to those packets would belong to the retired backend.
        // Retain a late packet without observing it so rollback can relay it
        // exactly once, with its original compression and protocol state.
        let ready = if let Some(packet) = old.received.take() {
            Poll::Ready(Ok(Some(packet)))
        } else {
            let read = tokio::task::unconstrained(old.reader.read(&mut old.io, old.codec));
            tokio::pin!(read);
            poll_fn(|cx| Poll::Ready(read.as_mut().poll(cx))).await
        };
        match ready {
            Poll::Ready(Ok(Some(packet))) => {
                if version.kind(State::Play, Direction::Clientbound, packet.id)
                    == PacketKind::Disconnect
                {
                    old.state
                        .observe(version, Direction::Clientbound, &packet)?;
                    self.client
                        .queue(version, Direction::Clientbound, &packet)?;
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "The current backend disconnected the player.",
                    ));
                }
                old.received = Some(packet);
            }
            Poll::Ready(result) => {
                self.backend_error = result.err();
                self.backend = None;
                return Ok(());
            }
            Poll::Pending if !old.reader.has_partial_frame() => return Ok(()),
            Poll::Pending => {}
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "The current server is busy. Please retry the switch.",
        ))
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
        forwarding: Option<&Forwarding>,
    ) -> io::Result<Option<Packet>> {
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
                    backend
                        .login_plugins
                        .require_forwarded(forwarding.is_some())?;
                    if protocol::success_identity(version, &packet)? != *expected {
                        return Err(protocol::invalid(
                            "replacement backend changed the player identity",
                        ));
                    }
                    if !version.has_configuration() {
                        continue;
                    }
                    let acknowledged = Packet::empty(3);
                    backend
                        .state
                        .observe(version, Direction::Serverbound, &acknowledged)?;
                    backend.codec.write(&mut backend.io, &acknowledged).await?;
                    return Ok(None);
                }
                PacketKind::JoinGame if !version.has_configuration() => {
                    version
                        .switching()
                        .unwrap()
                        .validate_join(&packet, forwarding.is_some())?;
                    return Ok(Some(packet));
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
                    let response = if let Some(response) =
                        backend.login_plugins.request(&packet, forwarding)?
                    {
                        response
                    } else {
                        let id = read_varint(&mut packet.data.as_slice())?;
                        let mut data = Vec::new();
                        write_varint(id, &mut data);
                        data.push(0);
                        Packet::new(2, data)
                    };
                    backend.codec.write(&mut backend.io, &response).await?;
                }
                PacketKind::EncryptionRequest => return Err(encryption_unsupported()),
                _ => return Err(protocol::invalid("unsupported replacement login exchange")),
            }
        }
    }

    fn remember_client_packet(&mut self, state: State, packet: &Packet) -> io::Result<()> {
        // Administrator transfers also need client options, even when player
        // commands are disabled by omitting the network configuration.
        let Some(caps) = self.version()?.switching() else {
            return Ok(());
        };
        if (state == State::Configuration && packet.id == caps.config_information)
            || (state == State::Play && packet.id == caps.play_information)
        {
            self.client_information = Some(Packet::new(
                if self.version()?.has_configuration() {
                    caps.config_information
                } else {
                    caps.play_information
                },
                packet.data.clone(),
            ));
        }
        if (state == State::Configuration && packet.id == caps.config_payload)
            || (state == State::Play && packet.id == caps.play_payload)
        {
            let mut bytes = packet.data.as_slice();
            if matches!(
                protocol::read_string(&mut bytes, 32767)?,
                "minecraft:brand" | "MC|Brand"
            ) {
                // Avoid retaining arbitrary large mod payloads across backends.
                protocol::read_string(&mut bytes, 32767)?;
                if !bytes.is_empty() {
                    return Err(protocol::invalid("invalid client brand"));
                }
                self.client_brand = Some(Packet::new(
                    if self.version()?.has_configuration() {
                        caps.config_payload
                    } else {
                        caps.play_payload
                    },
                    packet.data.clone(),
                ));
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
            // Login plugin requests can queue proxy-owned backend writes. Do
            // not consume another request/success until that write completes.
            let read_backend = self.client.pending.is_none()
                && (backend.state.phase(Direction::Clientbound) != State::Login
                    || backend.pending.is_none());
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
                    self.joined = self.pending_join_valid;
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
                    if kind == PacketKind::LoginPluginResponse {
                        backend.login_plugins.client_response(&packet)?;
                    }
                    if kind == PacketKind::EncryptionResponse {
                        return Err(protocol::invalid("unexpected client encryption response"));
                    }
                    if self.network
                        && phase == State::Play
                        && self.joined
                        && let Some(caps) = version.switching()
                        && let Some((command, acknowledgement)) =
                            caps.proxy_command_with(&packet, &self.extension_commands)?
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
                    if kind == PacketKind::CookieRequest {
                        self.initial_retry_safe = false;
                    }
                    if phase == State::Play {
                        self.legacy_state.observe(version, &packet)?;
                    }
                    match kind {
                        PacketKind::LoginPluginRequest => {
                            if let Some(response) = backend
                                .login_plugins
                                .request(&packet, self.forwarding.as_ref())?
                            {
                                backend.queue(version, Direction::Serverbound, &response)?;
                            } else {
                                self.initial_retry_safe = false;
                                self.client
                                    .queue(version, Direction::Clientbound, &packet)?;
                            }
                        }
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
                            backend
                                .login_plugins
                                .require_forwarded(self.forwarding.is_some())?;
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
                            let packet = if let Some(profile) = &self.authenticated_profile {
                                if identity.uuid != Some(profile.uuid)
                                    || identity.name != profile.name
                                {
                                    return Err(protocol::invalid(
                                        "backend changed the authenticated player identity",
                                    ));
                                }
                                forwarding::login_success(profile, version, &packet)?
                            } else {
                                packet
                            };
                            self.identity = Some(identity);
                            self.client
                                .queue(version, Direction::Clientbound, &packet)?;
                        }
                        PacketKind::JoinGame => {
                            let validation = version
                                .switching()
                                .unwrap()
                                .validate_join(&packet, self.authenticated_profile.is_some());
                            self.pending_join_valid = validation.is_ok();
                            // Opaque ordinary relays keep forwarding, but only a
                            // valid supported world can become transfer-ready.
                            if self.network || self.switch_in_progress {
                                validation?;
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
                            let packet = if self.network
                                && phase == State::Play
                                && let Some(caps) = version.switching()
                                && packet.id == caps.commands
                            {
                                caps.network_commands_with(&packet, &self.extension_commands)?
                            } else {
                                packet
                            };
                            if phase == State::Play
                                && version
                                    .switching()
                                    .is_some_and(|caps| packet.id == caps.bundle_delimiter)
                            {
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
