//! A 1.20.3 client on a 1.20.2 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    LEVEL_CHUNKS_LOAD_START, NEWER, OLDER, OlderScore, pack_uuid, push_rest, status_for_older,
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer},
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
            use State::{Configuration, Play};
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(
                    Configuration,
                    "resource_pack_push",
                    Self::resource_pack_push,
                )
                .clientbound(Play, "resource_pack_push", Self::resource_pack_push)
                .serverbound(Configuration, "resource_pack", Self::resource_pack)
                .serverbound(Play, "resource_pack", Self::resource_pack)
                .clientbound(Play, "set_score", Self::set_score)
                .clientbound(Play, "login", Self::start_level_loading)
                .clientbound(Play, "respawn", Self::start_level_loading)
                .serverbound(Play, "set_jigsaw_block", Self::set_jigsaw_block)
                // Only crafters have toggleable slots, and the backend has none.
                .drop_serverbound(Play, "container_slot_state_changed")
                .build()
        })
    }

    fn client_id(state: State, name: &str) -> i32 {
        Self::routes()
            .client()
            .expect_packet(state, Direction::Clientbound, name)
    }

    /// The backend's one pack replaces whatever it pushed before: drop every
    /// pack first, then push this one under an ID derived from its URL.
    fn resource_pack_push(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let url = rw.r.peek();
        let url = Reader::new(url).string(super::MAX_URL)?;
        rw.w.uuid(pack_uuid(url));
        push_rest(&mut self.shared, Direction::Clientbound, &mut rw)?;
        let body = rw.finish()?;
        let mut pop = Writer::new();
        pop.bool(false);
        cx.to_client(
            state,
            Packet::new(Self::client_id(state, "resource_pack_pop"), pop.buf),
        );
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn resource_pack(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        reader.uuid()?;
        let status = reader.varint()?;
        reader.finish()?;
        if let Some(status) = status_for_older(status)? {
            let mut body = Writer::new();
            body.varint(status);
            cx.to_backend(state, Packet::new(packet.id, body.buf));
        }
        Ok(())
    }

    fn set_score(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut body = Writer::new();
        let id = match OlderScore::read(&packet.data)? {
            OlderScore::Set {
                owner,
                objective,
                value,
            } => {
                // No display name and the objective's own number format.
                body.string(owner)
                    .string(objective)
                    .varint(value)
                    .bool(false)
                    .bool(false);
                packet.id
            }
            OlderScore::Reset { owner, objective } => {
                body.string(owner).bool(!objective.is_empty());
                if !objective.is_empty() {
                    body.string(objective);
                }
                Self::client_id(state, "reset_score")
            }
        };
        cx.to_client(state, Packet::new(id, body.buf));
        Ok(())
    }

    /// A 1.20.3 client shows "Loading terrain" until the server says the
    /// level's chunks are coming, which a 1.20.2 server never does: say it
    /// right after the packet that opens the screen.
    fn start_level_loading(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        cx.to_client(state, packet);
        let mut body = Writer::new();
        body.u8(LEVEL_CHUNKS_LOAD_START).f32(0.0);
        cx.to_client(
            state,
            Packet::new(Self::client_id(state, "game_event"), body.buf),
        );
        Ok(())
    }

    /// The backend's jigsaw blocks have no selection and placement priorities.
    fn set_jigsaw_block(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i64()?; // Position.
        for _ in 0..5 {
            // Name, target, pool, final state, joint type.
            rw.str()?;
        }
        rw.r.varint()?;
        rw.r.varint()?;
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
