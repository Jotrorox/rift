//! A 1.19 client on a 1.19.1 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared, registries::Side},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{
        Make,
        v1_18_2_to_1_19::{
            chat::{self, ChatTypes},
            id, join,
            login::{Hello, clamp_velocity_forwarding},
            messages::{Placement, PlayerChat, Sent, ServerData, SystemChat},
        },
    },
};
use std::{io, sync::OnceLock};

pub(super) struct Down {
    shared: Shared,
    /// The backend's chat types, from its `login`.
    chat_types: ChatTypes,
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
            chat_types: ChatTypes::default(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .serverbound(Login, "hello", Self::hello)
                .clientbound(Login, "custom_query", Self::custom_query)
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "player_chat", Self::player_chat)
                .clientbound(Play, "system_chat", Self::system_chat)
                .clientbound(Play, "server_data", Self::server_data)
                // Headers only matter to clients that verify signatures; a
                // message to delete was never shown as signed; 1.19 has no
                // chat completions beyond player names.
                .drop_clientbound(Play, "player_chat_header")
                .drop_clientbound(Play, "delete_chat")
                .drop_clientbound(Play, "custom_chat_completions")
                .serverbound(Play, "chat", Self::chat)
                .serverbound(Play, "chat_command", Self::chat_command)
                .build()
        })
    }

    fn hello(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let hello = Hello::read(OLDER, &packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, hello.write(NEWER)));
        Ok(())
    }

    fn custom_query(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = clamp_velocity_forwarding(&packet.data)?.unwrap_or(packet.data);
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// The registries codec converts the chat types for the client, adding
    /// the `system` and `game_info` types system chat uses; the backend's
    /// are kept for decoration.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = join::convert_login(&mut self.shared, Direction::Clientbound, &packet.data)?;
        self.chat_types = ChatTypes::from_shared(&self.shared, Side::Backend);
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn player_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let message = PlayerChat::read(NEWER, &packet.data)?;
        let Some(display) = self.chat_types.display(
            message.chat_type,
            &message.sender_name,
            message.target_name.as_ref(),
            &message.content,
        ) else {
            return Ok(());
        };
        let system = SystemChat {
            json: display.to_string(),
            placement: Placement::ChatType(chat::system_type(&self.shared, Side::Client, false)),
            sender: 0,
        };
        let id = id(OLDER, state, Direction::Clientbound, "system_chat");
        cx.to_client(state, Packet::new(id, system.write(OLDER)?));
        Ok(())
    }

    fn system_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut message = SystemChat::read(NEWER, &packet.data)?;
        if let Placement::Overlay(overlay) = message.placement {
            message.placement =
                Placement::ChatType(chat::system_type(&self.shared, Side::Client, overlay));
        }
        cx.to_client(state, Packet::new(packet.id, message.write(OLDER)?));
        Ok(())
    }

    fn server_data(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = ServerData::read(NEWER, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data.write(OLDER)));
        Ok(())
    }

    fn chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let message = Sent::read_chat(OLDER, &packet.data)?;
        let timestamp = message.timestamp.unwrap_or_default();
        cx.to_backend(
            state,
            Packet::new(packet.id, message.write_chat(NEWER, timestamp, 0)),
        );
        Ok(())
    }

    fn chat_command(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let command = Sent::read_command(OLDER, &packet.data)?;
        let timestamp = command.timestamp.unwrap_or_default();
        let data = command.write_command(NEWER, timestamp, 0);
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
