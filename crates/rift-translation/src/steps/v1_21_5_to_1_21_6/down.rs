//! A 1.21.5 client on a 1.21.6 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data, routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
};
use std::{io, sync::OnceLock};

pub struct Down {
    shared: Shared,
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
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(
                    State::Play,
                    "change_difficulty",
                    Self::clientbound_difficulty,
                )
                .clientbound(State::Play, "show_dialog", Self::show_dialog)
                // A 1.21.5 client has no dialog screens to show or close, and
                // in configuration no chat to fall back on. A 1.21.6 server
                // waits for no answer to a dialog.
                .drop_clientbound(State::Configuration, "show_dialog")
                .drop_clientbound(State::Configuration, "clear_dialog")
                .drop_clientbound(State::Play, "clear_dialog")
                // The locator bar is new in 1.21.6.
                .drop_clientbound(State::Play, "waypoint")
                .serverbound(
                    State::Play,
                    "change_difficulty",
                    Self::serverbound_difficulty,
                )
                .serverbound(State::Play, "player_command", Self::player_command)
                .build()
        })
    }

    fn clientbound_difficulty(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::clientbound_difficulty(&packet.data, false)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// A dialog the server spells out shows as a chat message with its title
    /// and text; buttons and inputs cannot be offered.
    fn show_dialog(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        if let Some(message) = super::dialog_as_chat(&packet.data)? {
            let id =
                data::version(OLDER).expect_packet(state, Direction::Clientbound, "system_chat");
            cx.to_client(state, Packet::new(id, super::system_chat(&message)));
        }
        Ok(())
    }

    fn serverbound_difficulty(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::serverbound_difficulty(&packet.data, true)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn player_command(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        if let Some(body) = super::command_down_to_1_21_6(&packet.data)? {
            cx.to_backend(state, Packet::new(packet.id, body));
        }
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
