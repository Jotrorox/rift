//! A 1.13.2 client on a 1.13.1 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    State,
    codec::{HasShared, Shared},
    routed_step,
    step::{RouteBuilder, Routes},
    steps::{Make, v1_13_to_1_13_1},
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
                .clientbound(
                    State::Play,
                    "custom_payload",
                    v1_13_to_1_13_1::custom_payload::<Self>,
                )
                .build()
        })
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
