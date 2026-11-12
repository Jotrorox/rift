//! A 1.13.1 client on a 1.13.2 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    State,
    codec::{HasShared, Shared},
    routed_step,
    step::{RouteBuilder, Routes},
    steps::{Make, v1_13_to_1_13_1},
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
                    "custom_payload",
                    v1_13_to_1_13_1::custom_payload::<Self>,
                )
                .build()
        })
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
