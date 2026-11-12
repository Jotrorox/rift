//! A 1.19.3 client on a 1.19.1 backend: backend packets are upgraded for the
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
            id, join,
            login::{Hello, Key, clamp_velocity_forwarding},
            messages::{PlayerChat, Sent, ServerData, write_disguised_chat},
        },
    },
    wire::{Reader, Writer},
};
use std::{io, sync::OnceLock};

pub(super) struct Up {
    shared: Shared,
    /// The player's last health, food and saturation.
    health: Option<(f32, i32, f32)>,
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
            health: None,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::Clientbound;
            use State::*;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .serverbound(Login, "hello", Self::hello)
                .serverbound(Login, "key", Self::key)
                .clientbound(Login, "custom_query", Self::custom_query)
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .tap(Clientbound, Play, "set_health", Self::track_health)
                .clientbound(Play, "player_chat", Self::player_chat)
                .clientbound(Play, "server_data", Self::server_data)
                .clientbound(Play, "custom_sound", Self::custom_sound)
                // Chat is delivered unsigned: headers, deletions by signature
                // and previews have nothing to act on.
                .drop_clientbound(Play, "player_chat_header")
                .drop_clientbound(Play, "delete_chat")
                .drop_clientbound(Play, "chat_preview")
                .drop_clientbound(Play, "set_display_chat_preview")
                .serverbound(Play, "chat", Self::chat)
                .serverbound(Play, "chat_command", Self::chat_command)
                .drop_serverbound(Play, "chat_ack")
                .drop_serverbound(Play, "chat_session_update")
                .build()
        })
    }

    fn hello(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let hello = Hello::read(NEWER, &packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, hello.write(OLDER)));
        Ok(())
    }

    fn key(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let key = Key::read(NEWER, &packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, key.write(OLDER)?));
        Ok(())
    }

    fn custom_query(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = clamp_velocity_forwarding(&packet.data)?.unwrap_or(packet.data);
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// The layout is unchanged and 1.19.3 reads 1.19.1's registries. Its
    /// vanilla server follows `login` with the enabled feature flags.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        self.health = None;
        let data = join::convert_login(&mut self.shared, Direction::Clientbound, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        let mut features = Writer::new();
        features.varint(1).string("minecraft:vanilla");
        let id = id(
            NEWER,
            state,
            Direction::Clientbound,
            "update_enabled_features",
        );
        cx.to_client(state, Packet::new(id, features.buf));
        Ok(())
    }

    /// 1.19.1 clients always keep the player's entity data across a respawn,
    /// so 1.19.3 clients are asked to as well. A dead player's kept health
    /// of zero would reopen the death screen at once, so a dead player
    /// respawns with full health until the backend says otherwise.
    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = join::convert_respawn(&mut self.shared, Direction::Clientbound, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        if let Some((health, food, saturation)) = self.health
            && health <= 0.0
        {
            let mut alive = Writer::new();
            alive.f32(20.0).varint(food).f32(saturation);
            let id = id(NEWER, state, Direction::Clientbound, "set_health");
            cx.to_client(state, Packet::new(id, alive.buf));
            self.health = Some((20.0, food, saturation));
        }
        Ok(())
    }

    fn track_health(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        self.health = Some((r.f32()?, r.varint()?, r.f32()?));
        Ok(())
    }

    /// Unsigned player chat that the client decorates with the backend's
    /// chat types, which it received unchanged in `login`.
    fn player_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let message = PlayerChat::read(OLDER, &packet.data)?;
        let id = id(NEWER, state, Direction::Clientbound, "disguised_chat");
        cx.to_client(state, Packet::new(id, write_disguised_chat(&message)));
        Ok(())
    }

    fn server_data(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = ServerData::read(OLDER, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data.write(NEWER)));
        Ok(())
    }

    /// A sound by name becomes `sound` with an inline sound event.
    fn custom_sound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let mut w = Writer::with_capacity(packet.data.len() + 2);
        let name = r.str()?;
        w.varint(0).string(name).bool(false); // Inline, no fixed range.
        w.varint(r.varint()?); // Source.
        w.bytes(r.take(3 * 4 + 2 * 4 + 8)?); // Position, volume, pitch, seed.
        r.finish()?;
        let id = id(NEWER, state, Direction::Clientbound, "sound");
        cx.to_client(state, Packet::new(id, w.buf));
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
