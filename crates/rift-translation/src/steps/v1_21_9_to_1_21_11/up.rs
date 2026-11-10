//! A 1.21.11 client on a 1.21.9 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{Border, NEWER, OLDER};
use crate::{
    Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

pub struct Up {
    shared: Shared,
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
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(State::Play, "initialize_border", Self::initialize_border)
                .clientbound(State::Play, "set_border_lerp_size", Self::border_lerp_size)
                .clientbound(
                    State::Play,
                    "set_border_warning_delay",
                    Self::border_warning,
                )
                .serverbound(State::Play, "player_action", Self::player_action)
                .build()
        })
    }

    fn initialize_border(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let data = Border::ToTicks.initialize(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn border_lerp_size(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let data = Border::ToTicks.lerp_size(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn border_warning(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = Border::ToTicks.warning_delay(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// A stab is the spear's attack, which a 1.21.10 server has no item
    /// for; it would disconnect the player for the unknown action.
    fn player_action(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        if !super::is_stab(&packet.data)? {
            cx.to_backend(state, packet);
        }
        Ok(())
    }
}
routed_step!(Up);
