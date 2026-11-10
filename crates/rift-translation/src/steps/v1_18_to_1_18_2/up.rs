//! A 1.18.2 client on a 1.18 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    codec::{HasShared, Shared},
    routed_step,
    step::{RouteBuilder, Routes},
    steps::Make,
};
use std::sync::OnceLock;

pub struct Up {
    shared: Shared,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Up {
    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use crate::State::Play;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Play, "login", |_, cx, state, packet| {
                    super::login(cx, state, packet, true)
                })
                .clientbound(Play, "respawn", |_, cx, state, packet| {
                    super::respawn(cx, state, packet, true)
                })
                .build()
        })
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| {
    Box::new(Up {
        shared: Shared::new(NEWER, OLDER),
    })
});
