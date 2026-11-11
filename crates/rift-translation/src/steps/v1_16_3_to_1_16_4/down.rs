//! A 1.16.3 client on a 1.16.4 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{MAIN_HAND, NEWER, OFF_HAND, OFF_HAND_SLOT, OLDER, check_hotbar, edit_book_prefix};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, invalid},
};
use std::{io, sync::OnceLock};

pub struct Down {
    shared: Shared,
    /// The selected hotbar slot, as the client and the backend last set it:
    /// a book edited in the main hand is in that slot.
    selected: i32,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Down {
    fn create() -> Box<dyn crate::step::Step> {
        Box::new(Self {
            shared: Shared::new(OLDER, NEWER),
            selected: 0,
        })
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .tap(
                    Direction::Clientbound,
                    State::Play,
                    "set_held_slot",
                    Self::held_slot,
                )
                .tap(
                    Direction::Serverbound,
                    State::Play,
                    "set_carried_item",
                    Self::carried_item,
                )
                .serverbound(State::Play, "edit_book", Self::edit_book)
                .build()
        })
    }

    fn held_slot(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        // Out-of-range values are ignored, as the client ignores them.
        if let Ok(slot) = check_hotbar(i32::from(reader.i8()?)) {
            self.selected = slot;
        }
        Ok(())
    }

    fn carried_item(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        if let Ok(slot) = check_hotbar(i32::from(reader.i16()?)) {
            self.selected = slot;
        }
        Ok(())
    }

    fn edit_book(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let slot = match edit_book_prefix(&mut rw)? {
            MAIN_HAND => self.selected,
            OFF_HAND => OFF_HAND_SLOT,
            _ => return Err(invalid("invalid hand")),
        };
        rw.w.varint(slot);
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(Down::create);
