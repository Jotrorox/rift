//! A 1.17.1 client on a 1.17 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend. Everything that
//! changed belongs to the shared codecs: menu state numbers, the cursor in
//! the whole contents and book edits to the item codec, entity removal to
//! the entity codec.
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
        ROUTES.get_or_init(|| RouteBuilder::new(NEWER, OLDER).defaults().build())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| {
    Box::new(Up {
        shared: Shared::new(NEWER, OLDER),
    })
});
