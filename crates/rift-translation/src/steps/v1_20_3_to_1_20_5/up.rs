//! A 1.20.5 client on a 1.20.3 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    CHANGE_GAME_MODE, CREATIVE, NEWER, OLDER, chat::LastSeen, codec_dimension_types, dimension_id,
    login_head, spawn_tail, text,
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{io, sync::OnceLock};

/// Vanilla's creative reach modifiers: their IDs and amounts.
const CREATIVE_BLOCK_RANGE: (u128, f64) = (0x736565d2_e1a7_403d_a3f8_1aeb3e302542, 0.5);
const CREATIVE_ENTITY_RANGE: (u128, f64) = (0x98491ef6_97b1_4584_ae82_71a8cc85cf73, 2.0);

pub struct Up {
    shared: Shared,
    /// The player's entity ID, from `login`.
    player: Option<i32>,
    /// Whether the backend enforces secure chat, once `server_data` said so.
    secure_chat: Option<bool>,
    last_seen: LastSeen,
    /// The backend's dimension types in network ID order.
    dimension_types: Vec<String>,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Up {
    fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            player: None,
            secure_chat: None,
            last_seen: LastSeen::default(),
            dimension_types: Vec::new(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::Clientbound;
            use State::{Configuration, Login, Play};
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Login, "hello", Self::hello)
                .clientbound(Login, "login_finished", Self::login_finished)
                // The backend never asks for cookies or known packs.
                .drop_serverbound(Login, "cookie_response")
                .drop_serverbound(Configuration, "cookie_response")
                .drop_serverbound(Configuration, "select_known_packs")
                .drop_serverbound(Play, "cookie_response")
                .drop_serverbound(Play, "debug_sample_subscription")
                .tap(
                    Clientbound,
                    Configuration,
                    "registry_data",
                    Self::follow_registries,
                )
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .tap(Clientbound, Play, "start_configuration", Self::leave_play)
                .clientbound(Play, "game_event", Self::game_event)
                .clientbound(Play, "server_data", Self::server_data)
                .clientbound(Play, "mount_screen_open", Self::mount_screen_open)
                .serverbound(Play, "chat", Self::chat)
                .serverbound(Play, "chat_ack", Self::chat_ack)
                .serverbound(Play, "chat_command", Self::chat_command)
                .serverbound(Play, "chat_command_signed", Self::chat_command_signed)
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

    /// An encryption request comes from an online-mode backend, whose
    /// client must authenticate with Mojang.
    fn hello(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.string(20)?;
        rw.byte_array()?;
        rw.byte_array()?;
        rw.w.bool(true);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// The client keeps going past packets it cannot handle, as 1.20.3
    /// clients did, rather than disconnecting on the first.
    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.uuid()?;
        rw.string(16)?;
        rw.list(|rw| {
            rw.str()?;
            rw.str()?;
            rw.optional(|rw| rw.str().map(drop))
        })?;
        rw.w.bool(false);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn follow_registries(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        // INTEGRATION: the registries codec numbers the client's dimension
        // types (`registries::id_of`); this mirrors its order: the backend's.
        self.dimension_types = codec_dimension_types(&packet.data)?;
        Ok(())
    }

    fn dimension(&self, rw: &mut Rewriter<'_>) -> io::Result<()> {
        let name = rw.r.str()?;
        rw.w.varint(dimension_id(&self.dimension_types, name)?);
        Ok(())
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let player = login_head(&mut rw)?;
        self.dimension(&mut rw)?;
        let game_mode = spawn_tail(&mut rw)?;
        // Only `server_data`, after this, says: assume a backend behind a
        // proxy does not enforce it until it does.
        rw.w.bool(self.secure_chat.unwrap_or(false));
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        self.player = Some(player);
        self.last_seen.reset();
        self.send_reach(cx, state, game_mode);
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        self.dimension(&mut rw)?;
        let game_mode = spawn_tail(&mut rw)?;
        rw.u8()?; // Data to keep.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        self.send_reach(cx, state, game_mode);
        Ok(())
    }

    fn leave_play(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.last_seen.reset();
        Ok(())
    }

    fn game_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let event = reader.u8()?;
        let value = reader.f32()?;
        reader.finish()?;
        cx.to_client(state, packet);
        if event == CHANGE_GAME_MODE {
            self.send_reach(cx, state, (value + 0.5).floor() as u8);
        }
        Ok(())
    }

    /// A 1.20.3 server's players reach as far as their client allows, which
    /// a 1.20.5 client takes from attributes: send vanilla's for the game
    /// mode, with the creative modifiers a 1.20.5 server would add.
    fn send_reach(&self, cx: &mut Cx<'_>, state: State, game_mode: u8) {
        let Some(player) = self.player else {
            return;
        };
        let attributes = Self::routes().client().expect_registry("attribute");
        let mut body = Writer::new();
        body.varint(player).varint(2);
        for (name, base, (uuid, amount)) in [
            ("player.block_interaction_range", 4.5, CREATIVE_BLOCK_RANGE),
            (
                "player.entity_interaction_range",
                3.0,
                CREATIVE_ENTITY_RANGE,
            ),
        ] {
            let id = attributes.id(name).expect("1.20.5 has reach attributes");
            body.varint(id).f64(base);
            if game_mode == CREATIVE {
                // Added to the base value.
                body.varint(1).uuid(uuid).f64(amount).varint(0);
            } else {
                body.varint(0);
            }
        }
        cx.to_client(
            state,
            Packet::new(Self::client_id(state, "update_attributes"), body.buf),
        );
    }

    /// `enforces_secure_chat` moved to `login`; remember it for the next.
    fn server_data(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        text(&mut self.shared, Direction::Clientbound, &mut rw)?;
        rw.optional(|rw| rw.byte_array().map(drop))?;
        self.secure_chat = Some(rw.r.bool()?);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// The armor slot left the horse's inventory for its equipment.
    fn mount_screen_open(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.u8()?;
        let size = rw.r.varint()?;
        rw.w.varint(size.saturating_sub(1).max(0));
        rw.i32()?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// Copy a chat message or signed command up to its last-seen update,
    /// then pass on the update with the offsets held back since the last.
    fn signed(
        &mut self,
        rw: &mut Rewriter<'_>,
        signatures: impl FnOnce(&mut Rewriter<'_>) -> io::Result<()>,
    ) -> io::Result<()> {
        rw.str()?;
        let timestamp = rw.i64()?;
        rw.i64()?; // Salt.
        signatures(rw)?;
        let offset = rw.r.varint()?;
        let acknowledged = rw.r.take(3)?.try_into().unwrap();
        let offset = self.last_seen.update(offset, acknowledged, timestamp);
        rw.w.varint(offset).bytes(&acknowledged);
        Ok(())
    }

    fn chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        self.signed(&mut rw, |rw| rw.optional(|rw| rw.copy(256).map(drop)))?;
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn chat_command_signed(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        self.signed(&mut rw, argument_signatures)?;
        cx.to_backend(
            state,
            Packet::new(Self::backend_id(state, "chat_command"), rw.finish()?),
        );
        Ok(())
    }

    /// A command without signed arguments: the backend still wants a
    /// timestamp, a salt, the (empty) signatures and a last-seen update.
    fn chat_command(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let command = reader.str()?;
        reader.finish()?;
        let (offset, acknowledged, timestamp) = self.last_seen.spoof();
        let mut body = Writer::new();
        body.string(command)
            .i64(timestamp)
            .i64(0)
            .varint(0)
            .varint(offset)
            .bytes(&acknowledged);
        cx.to_backend(state, Packet::new(packet.id, body.buf));
        Ok(())
    }

    fn chat_ack(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let offset = reader.varint()?;
        reader.finish()?;
        if let Some(offset) = self.last_seen.acknowledge(offset) {
            let mut body = Writer::new();
            body.varint(offset);
            cx.to_backend(state, Packet::new(packet.id, body.buf));
        }
        Ok(())
    }

    /// The container ID and button became VarInts; the backend reads bytes
    /// and gets the low eight bits, as from a 1.20.3 client.
    fn container_button_click(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let (container, button) = (reader.varint()?, reader.varint()?);
        reader.finish()?;
        let mut body = Writer::new();
        body.u8(container as u8).u8(button as u8);
        cx.to_backend(state, Packet::new(packet.id, body.buf));
        Ok(())
    }
}
routed_step!(Up);

/// The signatures of a command's signed arguments: name and signature each.
fn argument_signatures(rw: &mut Rewriter<'_>) -> io::Result<()> {
    let count = rw.count()?;
    if count > 8 {
        return Err(invalid("too many argument signatures"));
    }
    for _ in 0..count {
        rw.string(16)?;
        rw.copy(256)?;
    }
    Ok(())
}

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
