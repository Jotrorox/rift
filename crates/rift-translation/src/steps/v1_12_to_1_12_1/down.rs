//! A 1.12 client on a 1.12.1 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
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
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                // The 1.12 client shows ghost recipes on its own, and never
                // sends the placement request that would make a server do it.
                .drop_clientbound(State::Play, "place_ghost_recipe")
                // A list of item moves the client chose; the 1.12.1 backend
                // only takes a recipe ID, which the moves do not carry. The
                // client's grid stays as the backend has it: clicking a
                // recipe does nothing.
                .drop_serverbound(State::Play, "crafting_recipe_placement")
                .build()
        })
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
