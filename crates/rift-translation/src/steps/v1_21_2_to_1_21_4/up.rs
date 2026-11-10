//! A 1.21.4 client on a 1.21.2 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
};
use std::{io, sync::OnceLock};

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
                .clientbound(State::Play, "set_held_slot", Self::set_held_slot)
                .clientbound(
                    State::Configuration,
                    "update_enabled_features",
                    Self::enabled_features,
                )
                .serverbound(State::Play, "move_vehicle", Self::move_vehicle)
                // A 1.21.2 server picks from an inventory slot the client
                // names; the block or entity a 1.21.4 client names would need
                // the world and the inventory to resolve.
                .drop_serverbound(State::Play, "pick_item_from_block")
                .drop_serverbound(State::Play, "pick_item_from_entity")
                // A 1.21.2 server treats the player as loaded from the start.
                .drop_serverbound(State::Play, "player_loaded")
                .build()
        })
    }

    fn set_held_slot(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::held_slot(&packet.data, true)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn enabled_features(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::enabled_features(&packet.data, true)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn move_vehicle(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::move_vehicle(&packet.data, false)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
