//! A 1.14.3 client on a 1.14.2 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    codec::{HasShared, Shared},
    routed_step,
    step::{RouteBuilder, Routes},
    steps::Make,
};
use std::sync::OnceLock;

pub const STEP: Option<Make> = Some(|| {
    Box::new(Up {
        shared: Shared::new(NEWER, OLDER),
    })
});

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

#[cfg(test)]
mod tests {
    #[test]
    fn every_packet_has_a_route() {
        assert_eq!(super::Up::routes().problems(), &[] as &[String]);
    }
}
