//! A 1.18 client on a 1.17.1 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{Make, v1_16_4_to_1_17::common},
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
    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::Play;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Play, "login", common::login::<Self>)
                .clientbound(Play, "respawn", common::respawn::<Self>)
                .serverbound(Play, "client_information", Self::client_information)
                .build()
        })
    }

    /// 1.18 lets players refuse to appear in the server list.
    fn client_information(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.string(16)?;
        rw.i8()?;
        rw.varint()?;
        rw.bool()?;
        rw.u8()?;
        rw.varint()?;
        rw.bool()?;
        rw.r.bool()?;
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| {
    Box::new(Up {
        shared: Shared::new(NEWER, OLDER),
    })
});
