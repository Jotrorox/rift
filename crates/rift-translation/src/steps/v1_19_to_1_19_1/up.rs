//! A 1.19.1 client on a 1.19 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{
        Make,
        v1_18_2_to_1_19::{
            chat::{self, ChatTypes},
            id,
            join::Login,
            login::{Hello, clamp_velocity_forwarding},
            messages::{Placement, PlayerChat, Sent, ServerData, SystemChat},
        },
    },
};
use std::{io, sync::OnceLock};

pub(super) struct Up {
    shared: Shared,
    /// The backend's chat types, from its `login`.
    chat_types: ChatTypes,
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
            chat_types: ChatTypes::default(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::*;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .serverbound(Login, "hello", Self::hello)
                .clientbound(Login, "custom_query", Self::custom_query)
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "player_chat", Self::player_chat)
                .clientbound(Play, "system_chat", Self::system_chat)
                .clientbound(Play, "server_data", Self::server_data)
                .serverbound(Play, "chat", Self::chat)
                .serverbound(Play, "chat_command", Self::chat_command)
                // The backend checks no acknowledgements.
                .drop_serverbound(Play, "chat_ack")
                .build()
        })
    }

    fn hello(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let hello = Hello::read(NEWER, &packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, hello.write(OLDER)));
        Ok(())
    }

    fn custom_query(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = clamp_velocity_forwarding(&packet.data)?.unwrap_or(packet.data);
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// Keep the backend's chat types for decoration and give the client
    /// 1.19.1's own.
    // INTEGRATION: the registries codec's helpers for the registry parts of
    // `login` should replace the chat type handling here once they exist.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut login = Login::read(OLDER, &packet.data)?;
        self.chat_types = ChatTypes::from_codec(&login.codec);
        login
            .codec
            .insert("minecraft:chat_type", chat::vanilla_1_19_1());
        cx.to_client(state, Packet::new(packet.id, login.write(NEWER)?));
        Ok(())
    }

    fn player_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let message = PlayerChat::read(OLDER, &packet.data)?;
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
            placement: Placement::Overlay(self.chat_types.is_overlay(message.chat_type)),
            sender: 0,
        };
        let id = id(NEWER, state, Direction::Clientbound, "system_chat");
        cx.to_client(state, Packet::new(id, system.write(NEWER)?));
        Ok(())
    }

    fn system_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut message = SystemChat::read(OLDER, &packet.data)?;
        if let Placement::ChatType(chat_type) = message.placement {
            message.placement = Placement::Overlay(self.chat_types.is_overlay(chat_type));
        }
        cx.to_client(state, Packet::new(packet.id, message.write(NEWER)?));
        Ok(())
    }

    fn server_data(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = ServerData::read(OLDER, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data.write(NEWER)));
        Ok(())
    }

    fn chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let message = Sent::read_chat(NEWER, &packet.data)?;
        let timestamp = message.timestamp.unwrap_or_default();
        cx.to_backend(
            state,
            Packet::new(packet.id, message.write_chat(OLDER, timestamp, 0)),
        );
        Ok(())
    }

    fn chat_command(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let command = Sent::read_command(NEWER, &packet.data)?;
        let timestamp = command.timestamp.unwrap_or_default();
        let data = command.write_command(OLDER, timestamp, 0);
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
