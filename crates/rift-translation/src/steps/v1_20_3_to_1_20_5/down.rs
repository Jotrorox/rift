//! A 1.20.3 client on a 1.20.5 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER, dimension_name, login_head, packet_dimension_types, spawn_tail, text};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    nbt::Nbt,
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{io, sync::OnceLock};

/// The largest cookie a 1.20.5 client stores.
const MAX_COOKIE: usize = 5120;
/// How many cookies the step keeps for the client.
const MAX_COOKIES: usize = 64;

pub struct Down {
    shared: Shared,
    /// Whether the backend enforces secure chat, from its last `login`.
    secure_chat: bool,
    /// What the backend stored on the client, which cannot store it itself.
    cookies: Vec<(String, Vec<u8>)>,
    /// The backend's dimension types in network ID order.
    dimension_types: Vec<String>,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Down {
    fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            secure_chat: false,
            cookies: Vec::new(),
            dimension_types: Vec::new(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::Clientbound;
            use State::{Configuration, Login, Play};
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Login, "hello", Self::hello)
                .clientbound(Login, "login_finished", Self::login_finished)
                .clientbound(Login, "cookie_request", Self::cookie_request)
                .clientbound(Configuration, "cookie_request", Self::cookie_request)
                .clientbound(Play, "cookie_request", Self::cookie_request)
                .clientbound(Configuration, "store_cookie", Self::store_cookie)
                .clientbound(Play, "store_cookie", Self::store_cookie)
                .clientbound(Configuration, "transfer", Self::transfer)
                .clientbound(Play, "transfer", Self::transfer)
                // The client clears its chat when configuration starts.
                .drop_clientbound(Configuration, "reset_chat")
                .clientbound(
                    Configuration,
                    "select_known_packs",
                    Self::select_known_packs,
                )
                .tap(
                    Clientbound,
                    Configuration,
                    "registry_data",
                    Self::follow_registries,
                )
                .clientbound(
                    Configuration,
                    "finish_configuration",
                    Self::finish_configuration,
                )
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "server_data", Self::server_data)
                .clientbound(Play, "mount_screen_open", Self::mount_screen_open)
                // Debug screen samples and projectile acceleration have no
                // place in the client.
                .drop_clientbound(Play, "debug_sample")
                .drop_clientbound(Play, "projectile_power")
                // Signed even without signatures, as the client sent it: an
                // unsigned command would break the backend's message chain.
                .rename_serverbound(Play, "chat_command", "chat_command_signed")
                .serverbound(Play, "container_button_click", Self::container_button_click)
                .build()
        })
    }

    fn client_id(state: State, name: &str) -> i32 {
        Self::routes()
            .client()
            .expect_packet(state, Direction::Clientbound, name)
    }

    fn backend_id(state: State, name: &str) -> i32 {
        Self::routes()
            .backend()
            .expect_packet(state, Direction::Serverbound, name)
    }

    /// The client always authenticates when asked to encrypt.
    fn hello(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.string(20)?;
        rw.byte_array()?;
        rw.byte_array()?;
        rw.r.bool()?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.uuid()?;
        rw.string(16)?;
        rw.list(|rw| {
            rw.str()?;
            rw.str()?;
            rw.optional(|rw| rw.str().map(drop))
        })?;
        rw.r.bool()?; // Strict error handling.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// Answer for the client from what the backend stored on it.
    fn cookie_request(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let key = reader.str()?;
        reader.finish()?;
        let mut body = Writer::new();
        body.string(key);
        match self.cookies.iter().find(|(known, _)| known == key) {
            Some((_, payload)) => body.bool(true).byte_array(payload),
            None => body.bool(false),
        };
        cx.to_backend(
            state,
            Packet::new(Self::backend_id(state, "cookie_response"), body.buf),
        );
        Ok(())
    }

    fn store_cookie(&mut self, _: &mut Cx<'_>, _: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let key = reader.str()?.to_owned();
        let payload = reader.byte_array()?;
        reader.finish()?;
        if payload.len() > MAX_COOKIE {
            return Err(invalid("cookie too large"));
        }
        self.cookies.retain(|(known, _)| *known != key);
        if self.cookies.len() == MAX_COOKIES {
            self.cookies.remove(0);
        }
        self.cookies.push((key, payload.to_vec()));
        Ok(())
    }

    /// The client cannot follow a transfer: disconnect it with the reason,
    /// as it would otherwise sit on a server that has let it go.
    fn transfer(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let host = reader.str()?;
        let port = reader.varint()?;
        reader.finish()?;
        let reason = format!(
            "The server sent you to {host}:{port}, but Minecraft {} cannot be transferred.",
            crate::version::release(OLDER)
        );
        let body = Nbt::String(reason).to_bytes(false);
        cx.to_client(
            state,
            Packet::new(Self::client_id(state, "disconnect"), body),
        );
        Ok(())
    }

    /// Know no packs, so the backend sends every registry entry in full.
    fn select_known_packs(&mut self, cx: &mut Cx<'_>, state: State, _: Packet) -> io::Result<()> {
        let mut body = Writer::new();
        body.varint(0);
        cx.to_backend(
            state,
            Packet::new(Self::backend_id(state, "select_known_packs"), body.buf),
        );
        Ok(())
    }

    fn follow_registries(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        // INTEGRATION: the registries codec names the backend's dimension
        // types (`registries::name_of`); until then they are followed here.
        if let Some(names) = packet_dimension_types(&packet.data)? {
            self.dimension_types = names;
        }
        Ok(())
    }

    /// The client needs every registry in one codec before it may play.
    fn finish_configuration(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        if let Some(codec) = pending_registry_codec(&mut self.shared)? {
            cx.to_client(
                state,
                Packet::new(Self::client_id(state, "registry_data"), codec),
            );
        }
        cx.to_client(state, packet);
        Ok(())
    }

    fn dimension(&self, rw: &mut Rewriter<'_>) -> io::Result<()> {
        let id = rw.r.varint()?;
        rw.w.string(&dimension_name(&self.dimension_types, id)?);
        Ok(())
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        login_head(&mut rw)?;
        self.dimension(&mut rw)?;
        spawn_tail(&mut rw)?;
        // Moved back to `server_data`.
        self.secure_chat = rw.r.bool()?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        self.dimension(&mut rw)?;
        spawn_tail(&mut rw)?;
        rw.u8()?; // Data to keep.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn server_data(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        text(&mut self.shared, Direction::Clientbound, &mut rw)?;
        rw.optional(|rw| rw.byte_array().map(drop))?;
        rw.w.bool(self.secure_chat);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// The client's horse inventories still hold the armor slot.
    fn mount_screen_open(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.u8()?;
        let size = rw.r.varint()?;
        rw.w.varint(size.saturating_add(1));
        rw.i32()?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn container_button_click(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let (container, button) = (reader.i8()?, reader.i8()?);
        reader.finish()?;
        let mut body = Writer::new();
        body.varint(container.into()).varint(button.into());
        cx.to_backend(state, Packet::new(packet.id, body.buf));
        Ok(())
    }
}
routed_step!(Down);

/// The registries a 1.20.5 backend sent so far, as a 1.20.3 codec, when the
/// registries codec still holds them because the backend's version has
/// registries it did not send.
// INTEGRATION: `codec::registries::take_pending`, assembled for the client
// (`registries::convert::assemble`) and written as a codec packet
// (`registries::form::write_codec_packet`). Nothing is held until then.
fn pending_registry_codec(shared: &mut Shared) -> io::Result<Option<Vec<u8>>> {
    let _ = shared;
    Ok(None)
}

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
