//! A 1.16.4 client on a 1.16.3 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{MAIN_HAND, NEWER, OFF_HAND, OFF_HAND_SLOT, OLDER, edit_book_prefix};
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
                .serverbound(State::Play, "edit_book", Self::edit_book)
                .build()
        })
    }

    /// A book in the off-hand slot is in the off hand; any other is the one
    /// in the main hand, the only other place a client edits books from.
    fn edit_book(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let slot = edit_book_prefix(&mut rw)?;
        rw.w.varint(if slot == OFF_HAND_SLOT {
            OFF_HAND
        } else {
            MAIN_HAND
        });
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(Up::create);
