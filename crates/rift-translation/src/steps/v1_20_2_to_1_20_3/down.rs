//! A 1.20.2 client on a 1.20.3 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{
    LEVEL_CHUNKS_LOAD_START, NEWER, OLDER, OlderScore, push_rest, skip_score_format,
    status_for_newer,
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub struct Down {
    shared: Shared,
    /// The pack the backend pushed last: the one the client answers for.
    resource_pack: Option<u128>,
    /// The body of the backend's last `set_default_spawn_position`.
    spawn: Option<[u8; 12]>,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Down {
    fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            resource_pack: None,
            spawn: None,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::{Configuration, Play};
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(
                    Configuration,
                    "resource_pack_push",
                    Self::resource_pack_push,
                )
                .clientbound(Play, "resource_pack_push", Self::resource_pack_push)
                // The client cannot unload a server pack; the next push
                // replaces it.
                .drop_clientbound(Configuration, "resource_pack_pop")
                .drop_clientbound(Play, "resource_pack_pop")
                .clientbound(Configuration, "update_enabled_features", Self::features)
                .serverbound(Configuration, "resource_pack", Self::resource_pack)
                .serverbound(Play, "resource_pack", Self::resource_pack)
                .clientbound(Play, "set_score", Self::set_score)
                .clientbound(Play, "reset_score", Self::reset_score)
                .tap(
                    Direction::Clientbound,
                    Play,
                    "set_default_spawn_position",
                    Self::follow_spawn,
                )
                .clientbound(Play, "game_event", Self::game_event)
                .serverbound(Play, "set_jigsaw_block", Self::set_jigsaw_block)
                // The client always runs at 20 ticks per second.
                .drop_clientbound(Play, "ticking_state")
                .drop_clientbound(Play, "ticking_step")
                .build()
        })
    }

    fn client_id(state: State, name: &str) -> i32 {
        Self::routes()
            .client()
            .expect_packet(state, Direction::Clientbound, name)
    }

    fn resource_pack_push(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let id = rw.r.uuid()?;
        push_rest(&mut self.shared, Direction::Clientbound, &mut rw)?;
        let body = rw.finish()?;
        self.resource_pack = Some(id);
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// 1.20.3 put the coming 1.21 content behind a flag 1.20.2 lacks.
    fn features(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::super::v1_20_5_to_1_21::rewrite_features(&packet.data, OLDER, &[])?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn resource_pack(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let status = status_for_newer(reader.varint()?)?;
        reader.finish()?;
        let mut body = Writer::new();
        body.uuid(self.resource_pack.unwrap_or(0)).varint(status);
        cx.to_backend(state, Packet::new(packet.id, body.buf));
        Ok(())
    }

    fn set_score(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let score = OlderScore::Set {
            owner: reader.str()?,
            objective: reader.str()?,
            value: reader.varint()?,
        };
        // The client shows the objective's numbers, unformatted.
        skip_score_format(&mut reader)?;
        reader.finish()?;
        cx.to_client(state, Packet::new(packet.id, score.write()));
        Ok(())
    }

    fn reset_score(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let owner = reader.str()?;
        let objective = if reader.bool()? { reader.str()? } else { "" };
        reader.finish()?;
        let score = OlderScore::Reset { owner, objective };
        cx.to_client(
            state,
            Packet::new(Self::client_id(state, "set_score"), score.write()),
        );
        Ok(())
    }

    fn follow_spawn(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let spawn = packet
            .data
            .as_slice()
            .try_into()
            .map_err(|_| invalid("set_default_spawn_position is not 12 bytes"))?;
        self.spawn = Some(spawn);
        Ok(())
    }

    /// The client closes its "Loading terrain" screen when the spawn
    /// position arrives, not on the event that tells newer clients to: send
    /// the spawn position again in its place.
    fn game_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let event = reader.u8()?;
        reader.f32()?;
        reader.finish()?;
        if event != LEVEL_CHUNKS_LOAD_START {
            cx.to_client(state, packet);
        } else if let Some(spawn) = self.spawn {
            let id = Self::client_id(state, "set_default_spawn_position");
            cx.to_client(state, Packet::new(id, spawn.to_vec()));
        }
        Ok(())
    }

    /// The client's jigsaw blocks have no priorities: send the defaults.
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
        rw.w.varint(0).varint(0);
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
