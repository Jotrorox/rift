//! A 1.12.2 client on a 1.12.1 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Writer},
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
    fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(State::Play, "keep_alive", Self::keep_alive_request)
                .serverbound(State::Play, "keep_alive", Self::keep_alive_answer)
                .build()
        })
    }

    fn keep_alive_request(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let id = reader.varint()?;
        reader.finish()?;
        let mut body = Writer::new();
        body.i64(id.into());
        cx.to_client(state, Packet::new(packet.id, body.into_inner()));
        Ok(())
    }

    /// The 1.12.1 backend only ever sends VarInt IDs, so an answer that does
    /// not fit one answers nothing it sent and is dropped.
    fn keep_alive_answer(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let id = reader.i64()?;
        reader.finish()?;
        if let Ok(id) = i32::try_from(id) {
            let mut body = Writer::new();
            body.varint(id);
            cx.to_backend(state, Packet::new(packet.id, body.into_inner()));
        }
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
