//! A 1.13 client on a 1.13.1 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
//!
//! Tab completion needs no help this way round: a 1.13.1 server takes a
//! command without its slash as it is, so its ranges fit the 1.13 client.
use super::{NEWER, OLDER, packets};
use crate::{
    State,
    codec::{HasShared, Shared},
    routed_step,
    step::{RouteBuilder, Routes},
    steps::Make,
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
            use State::Play;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Play, "boss_event", packets::boss_event::<Self>)
                .clientbound(Play, "level_event", packets::level_event::<Self>)
                .clientbound(Play, "custom_payload", packets::custom_payload::<Self>)
                .serverbound(Play, "edit_book", packets::edit_book::<Self>)
                .build()
        })
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
