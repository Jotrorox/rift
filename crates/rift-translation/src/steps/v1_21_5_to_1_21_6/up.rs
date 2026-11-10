//! A 1.21.6 client on a 1.21.5 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data, routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::Reader,
};
use std::{io, sync::OnceLock};

pub struct Up {
    shared: Shared,
    /// The player's entity ID, which a 1.21.5 `player_command` names.
    player: i32,
    /// Whether the backend was last told shift is held.
    sneaking: bool,
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
            player: 0,
            sneaking: false,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        use Direction::*;
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .tap(Clientbound, State::Play, "login", Self::joined)
                .tap(Clientbound, State::Play, "respawn", Self::respawned)
                .clientbound(
                    State::Play,
                    "change_difficulty",
                    Self::clientbound_difficulty,
                )
                .serverbound(
                    State::Play,
                    "change_difficulty",
                    Self::serverbound_difficulty,
                )
                .serverbound(State::Play, "player_command", Self::player_command)
                .serverbound(State::Play, "player_input", Self::player_input)
                .serverbound(State::Play, "change_game_mode", Self::change_game_mode)
                // Only dialogs send these, and a 1.21.5 server shows none.
                .drop_serverbound(State::Configuration, "custom_click_action")
                .drop_serverbound(State::Play, "custom_click_action")
                .build()
        })
    }

    fn joined(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.player = Reader::new(&packet.data).i32()?;
        self.sneaking = false;
        Ok(())
    }

    /// A respawned player is a new entity that is not sneaking.
    fn respawned(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.sneaking = false;
        Ok(())
    }

    fn clientbound_difficulty(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::clientbound_difficulty(&packet.data, true)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn serverbound_difficulty(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::serverbound_difficulty(&packet.data, false)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn player_command(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::command_up_to_1_21_5(&packet.data)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// A 1.21.5 server learns of sneaking from `player_command`; a 1.21.6
    /// client only sends its input, which the server still gets as well.
    fn player_input(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let sneaking = super::input_flags(&packet.data)? & super::SHIFT != 0;
        cx.to_backend(state, packet);
        if sneaking != self.sneaking {
            self.sneaking = sneaking;
            let id =
                data::version(OLDER).expect_packet(state, Direction::Serverbound, "player_command");
            let body = super::shift_command(self.player, sneaking);
            cx.to_backend(state, Packet::new(id, body));
        }
        Ok(())
    }

    fn change_game_mode(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::game_mode_command(&packet.data)?;
        let id = data::version(OLDER).expect_packet(state, Direction::Serverbound, "chat_command");
        cx.to_backend(state, Packet::new(id, body));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
