//! A 1.21.9 client on a 1.21.11 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{Border, NEWER, OLDER};
use crate::{
    Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

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
                .clientbound(State::Play, "initialize_border", Self::initialize_border)
                .clientbound(State::Play, "set_border_lerp_size", Self::border_lerp_size)
                .clientbound(
                    State::Play,
                    "set_border_warning_delay",
                    Self::border_warning,
                )
                .build()
        })
    }

    fn initialize_border(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let data = Border::FromTicks.initialize(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn border_lerp_size(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let data = Border::FromTicks.lerp_size(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn border_warning(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = Border::FromTicks.warning_delay(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }
}
routed_step!(Down);
