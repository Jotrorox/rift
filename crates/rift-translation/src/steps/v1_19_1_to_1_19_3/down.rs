//! A 1.19.1 client on a 1.19.3 backend: backend packets are downgraded for
//! the client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared, registries::Side},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{
        Make,
        v1_18_2_to_1_19::{
            chat::ChatTypes,
            id, join,
            login::{Hello, Key, clamp_velocity_forwarding},
            messages::{Placement, PlayerChat, Sent, ServerData, SystemChat, read_disguised_chat},
        },
    },
    wire::{Reader, Writer},
};
use std::{io, sync::OnceLock};

/// The vanilla client acknowledges once this many signed messages are
/// pending; the backend disconnects far later (at 4096).
const ACKNOWLEDGE_AFTER: i32 = 64;

pub(super) struct Down {
    shared: Shared,
    /// The backend's chat types, from its `login`.
    chat_types: ChatTypes,
    /// Signed messages the backend sent since the last acknowledgement. The
    /// client never acknowledges them: it only sees system chat.
    unacknowledged: i32,
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
            unacknowledged: 0,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .serverbound(Login, "hello", Self::hello)
                .serverbound(Login, "key", Self::key)
                .clientbound(Login, "custom_query", Self::custom_query)
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "player_chat", Self::player_chat)
                .clientbound(Play, "disguised_chat", Self::disguised_chat)
                .clientbound(Play, "server_data", Self::server_data)
                .clientbound(Play, "sound", Self::sound)
                // 1.19.1 knows no experimental features.
                .drop_clientbound(Play, "update_enabled_features")
                .serverbound(Play, "chat", Self::chat)
                .serverbound(Play, "chat_command", Self::chat_command)
                // The step acknowledges for the client; previews stay off.
                .drop_serverbound(Play, "chat_ack")
                .drop_serverbound(Play, "chat_preview")
                .build()
        })
    }

    fn hello(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let hello = Hello::read(OLDER, &packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, hello.write(NEWER)));
        Ok(())
    }

    fn key(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let key = Key::read(OLDER, &packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, key.write(NEWER)?));
        Ok(())
    }

    fn custom_query(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = clamp_velocity_forwarding(&packet.data)?.unwrap_or(packet.data);
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// The registries codec converts the registries (biome sounds become
    /// names, the only form 1.19.1 reads); the backend's chat types are
    /// kept for decoration.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = join::convert_login(&mut self.shared, Direction::Clientbound, &packet.data)?;
        self.chat_types = ChatTypes::from_shared(&self.shared, Side::Backend);
        self.unacknowledged = 0;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = join::convert_respawn(&mut self.shared, Direction::Clientbound, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn player_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let message = PlayerChat::read(NEWER, &packet.data)?;
        if message.signed {
            self.unacknowledged += 1;
            if self.unacknowledged > ACKNOWLEDGE_AFTER {
                let mut ack = Writer::new();
                ack.varint(std::mem::take(&mut self.unacknowledged));
                let id = id(NEWER, state, Direction::Serverbound, "chat_ack");
                cx.to_backend(state, Packet::new(id, ack.buf));
            }
        }
        self.show(cx, state, &message)
    }

    fn disguised_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let message = read_disguised_chat(&packet.data)?;
        self.show(cx, state, &message)
    }

    /// Player chat as system chat, decorated as the client would have.
    fn show(&mut self, cx: &mut Cx<'_>, state: State, message: &PlayerChat) -> io::Result<()> {
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
            placement: Placement::Overlay(false),
            sender: 0,
        };
        let id = id(OLDER, state, Direction::Clientbound, "system_chat");
        cx.to_client(state, Packet::new(id, system.write(OLDER)?));
        Ok(())
    }

    fn server_data(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = ServerData::read(NEWER, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data.write(OLDER)));
        Ok(())
    }

    /// A sound from the registry stays `sound`; one given inline by name
    /// becomes `custom_sound`, which has no fixed range.
    fn sound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let mut w = Writer::with_capacity(packet.data.len());
        let holder = r.varint()?;
        let name = if holder == 0 {
            let name = r.str()?;
            if r.bool()? {
                r.f32()?;
            }
            w.string(name);
            "custom_sound"
        } else {
            let mapping = self.shared.mapping(Direction::Clientbound, "sound_event");
            let Some(sound) = mapping.get(holder - 1) else {
                return Ok(()); // 1.19.1 has no such sound and no stand-in.
            };
            w.varint(sound);
            "sound"
        };
        w.varint(r.varint()?); // Source.
        w.bytes(r.take(3 * 4 + 2 * 4 + 8)?); // Position, volume, pitch, seed.
        r.finish()?;
        let id = id(OLDER, state, Direction::Clientbound, name);
        cx.to_client(state, Packet::new(id, w.buf));
        Ok(())
    }

    fn chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let message = Sent::read_chat(OLDER, &packet.data)?;
        let timestamp = message.timestamp.unwrap_or_default();
        let acknowledged = std::mem::take(&mut self.unacknowledged);
        let data = message.write_chat(NEWER, timestamp, acknowledged);
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn chat_command(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let command = Sent::read_command(OLDER, &packet.data)?;
        let timestamp = command.timestamp.unwrap_or_default();
        let acknowledged = std::mem::take(&mut self.unacknowledged);
        let data = command.write_command(NEWER, timestamp, acknowledged);
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
