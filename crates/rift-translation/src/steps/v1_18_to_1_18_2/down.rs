//! A 1.18 client on a 1.18.2 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    codec::{HasShared, Shared},
    routed_step,
    step::{RouteBuilder, Routes},
    steps::{Make, v1_16_4_to_1_17::common},
};
use std::sync::OnceLock;

pub struct Down {
    shared: Shared,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Down {
    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use crate::State::Play;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Play, "login", common::login::<Self>)
                .clientbound(Play, "respawn", common::respawn::<Self>)
                .build()
        })
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| {
    Box::new(Down {
        shared: Shared::new(OLDER, NEWER),
    })
});
