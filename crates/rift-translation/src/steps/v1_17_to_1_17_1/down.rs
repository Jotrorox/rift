//! A 1.17 client on a 1.17.1 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend. Everything that
//! changed belongs to the shared codecs (see `up`).
use super::{NEWER, OLDER};
use crate::{
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
    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| RouteBuilder::new(OLDER, NEWER).defaults().build())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| {
    Box::new(Down {
        shared: Shared::new(OLDER, NEWER),
    })
});
