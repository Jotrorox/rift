//! A 1.15 client on a 1.14.4 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::Rewriter,
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
    fn create() -> Box<dyn crate::step::Step> {
        Box::new(Self {
            shared: Shared::new(NEWER, OLDER),
        })
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(State::Play, "login", Self::login)
                .clientbound(State::Play, "respawn", Self::respawn)
                .build()
        })
    }

    /// The hashed seed (unknown: 0) after the dimension, and the respawn
    /// screen shown, as 1.14 always did.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i32()?; // Entity ID.
        rw.u8()?; // Game mode.
        rw.i32()?; // Dimension.
        rw.w.i64(0);
        rw.u8()?; // Max players.
        rw.string(16)?; // Level type.
        rw.varint()?; // View distance.
        rw.bool()?; // Reduced debug info.
        rw.w.bool(true);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i32()?; // Dimension.
        rw.w.i64(0);
        rw.u8()?; // Game mode.
        rw.string(16)?; // Level type.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(Up::create);
