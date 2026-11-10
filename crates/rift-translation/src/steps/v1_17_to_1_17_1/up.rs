//! A 1.17.1 client on a 1.17 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER, book};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{
        Make,
        v1_16_4_to_1_17::common::{self, EMPTY_ITEM},
    },
    wire::{Reader, Rewriter, invalid},
};
use std::{io, sync::OnceLock};

pub struct Up {
    shared: Shared,
    /// The cursor stack as the backend last set it, in the client's format:
    /// 1.17.1's whole-contents packet carries it, 1.17's does not.
    carried: Vec<u8>,
    /// The state number given to the next slot packet. The client only
    /// echoes it in clicks, which a 1.17 backend does not want.
    state_id: i32,
    /// The player's name, for the author of a book being signed.
    name: String,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Up {
    pub(super) fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            carried: EMPTY_ITEM.to_vec(),
            state_id: 0,
            name: String::new(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::{Login, Play};
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Play, "container_set_content", Self::set_content)
                .clientbound(Play, "container_set_slot", Self::set_slot)
                .clientbound(Play, "remove_entity", Self::remove_entity)
                .serverbound(Play, "container_click", Self::container_click)
                .serverbound(Play, "edit_book", Self::edit_book)
                .tap(
                    Direction::Clientbound,
                    Login,
                    "login_finished",
                    |step, _, packet| {
                        let mut r = Reader::new(&packet.data);
                        r.uuid()?;
                        step.name = r.string(16)?.to_owned();
                        Ok(())
                    },
                )
                .build()
        })
    }

    fn next_state(&mut self) -> i32 {
        self.state_id = (self.state_id + 1) & 0x7fff;
        self.state_id
    }

    fn set_content(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.u8()?;
        let state_id = self.next_state();
        rw.w.varint(state_id);
        let count = rw.r.i16()?;
        let count = usize::try_from(count).map_err(|_| invalid("negative slot count"))?;
        rw.w.varint(count as i32);
        for _ in 0..count {
            common::item(&mut self.shared, Direction::Clientbound, &mut rw)?;
        }
        rw.w.bytes(&self.carried);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn set_slot(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let window = rw.i8()?;
        let state_id = self.next_state();
        rw.w.varint(state_id);
        let slot = rw.i16()?;
        let start = rw.w.len();
        common::item(&mut self.shared, Direction::Clientbound, &mut rw)?;
        if (window, slot) == (-1, -1) {
            self.carried = rw.w.buf[start..].to_vec();
        }
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn remove_entity(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.w.varint(1);
        rw.varint()?;
        common::to_client(cx, NEWER, state, "remove_entities", rw.finish()?);
        Ok(())
    }

    /// The state number goes; the mode was always a VarInt on the wire.
    fn container_click(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.u8()?;
        rw.r.varint()?;
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
        let data = book::downgrade(&packet.data, &self.name)?;
        cx.to_backend(state, Packet::new(packet.id, data));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
