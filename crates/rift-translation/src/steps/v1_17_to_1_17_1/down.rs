//! A 1.17 client on a 1.17.1 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER, book};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{Make, v1_16_4_to_1_17::common},
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub struct Down {
    shared: Shared,
    /// The last state number the backend sent per window; clicks must carry
    /// it or the backend resends the whole menu. The inventory and at most
    /// one container.
    states: Vec<(u8, i32)>,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Down {
    pub(super) fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            states: Vec::new(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::Play;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Play, "container_set_content", Self::set_content)
                .clientbound(Play, "container_set_slot", Self::set_slot)
                .clientbound(Play, "remove_entities", Self::remove_entities)
                .serverbound(Play, "container_click", Self::container_click)
                .serverbound(Play, "edit_book", Self::edit_book)
                .build()
        })
    }

    fn remember(&mut self, window: u8, state_id: i32) {
        match self.states.iter_mut().find(|(known, _)| *known == window) {
            Some((_, known)) => *known = state_id,
            None => {
                if window != 0 {
                    self.states.retain(|(known, _)| *known == 0);
                }
                self.states.push((window, state_id));
            }
        }
    }

    fn set_content(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let window = rw.u8()?;
        let state_id = rw.r.varint()?;
        self.remember(window, state_id);
        let count = rw.r.count()?;
        rw.w.i16(i16::try_from(count).map_err(|_| invalid("too many slots for 1.17"))?);
        for _ in 0..count {
            common::item(&mut self.shared, Direction::Clientbound, &mut rw)?;
        }
        // 1.17 sets the cursor with a slot packet of its own.
        let mut carried = Rewriter::new(rw.r.rest());
        carried.w.i8(-1).i16(-1);
        common::item(&mut self.shared, Direction::Clientbound, &mut carried)?;
        let carried = carried.finish()?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        common::to_client(cx, OLDER, state, "container_set_slot", carried);
        Ok(())
    }

    fn set_slot(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let window = rw.i8()?;
        let state_id = rw.r.varint()?;
        if let Ok(window) = u8::try_from(window) {
            self.remember(window, state_id);
        }
        rw.i16()?;
        common::item(&mut self.shared, Direction::Clientbound, &mut rw)?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn remove_entities(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let ids = (0..r.count()?)
            .map(|_| r.varint())
            .collect::<io::Result<Vec<_>>>()?;
        r.finish()?;
        for id in ids {
            let mut w = Writer::new();
            w.varint(id);
            common::to_client(cx, OLDER, state, "remove_entity", w.into_inner());
        }
        Ok(())
    }

    fn container_click(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let window = rw.u8()?;
        let state_id = self
            .states
            .iter()
            .find(|(known, _)| *known == window)
            .map_or(0, |(_, state_id)| *state_id);
        rw.w.varint(state_id);
        rw.i16()?;
        rw.i8()?;
        rw.varint()?;
        for _ in 0..rw.count()? {
            rw.i16()?;
            common::item(&mut self.shared, Direction::Serverbound, &mut rw)?;
        }
        common::item(&mut self.shared, Direction::Serverbound, &mut rw)?;
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn edit_book(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_backend(state, Packet::new(packet.id, book::upgrade(&packet.data)?));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
