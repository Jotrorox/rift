//! A 1.18 client on a 1.17.1 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    NEWER, OLDER,
    chunk::{self, Pending},
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{
        Make,
        v1_16_4_to_1_17::common::{self, Login, Respawn},
    },
    wire::{Reader, Rewriter, Writer},
};
use std::{io, sync::OnceLock};

pub struct Up {
    shared: Shared,
    pending: Pending,
    /// The world the player is in: leaving it unloads its chunks.
    world: String,
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
            pending: Pending::default(),
            world: String::new(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::Clientbound;
            use State::Play;
            RouteBuilder::new(NEWER, OLDER)
                // The chunk codec's own handling of these replaces the
                // sequencing below once it lands.
                .default_handler(Clientbound, Play, "level_chunk", Self::level_chunk)
                .default_handler(Clientbound, Play, "light_update", Self::light_update)
                .defaults()
                .clientbound(Play, "login", Self::login)
                .serverbound(Play, "client_information", Self::client_information)
                .tap(Clientbound, Play, "respawn", |step, _, packet| {
                    let respawn = Respawn::read(&packet.data)?;
                    if respawn.world != step.world {
                        step.pending.clear();
                        step.world = respawn.world;
                    }
                    Ok(())
                })
                .tap(
                    Clientbound,
                    Play,
                    "forget_level_chunk",
                    |step, _, packet| {
                        let mut r = Reader::new(&packet.data);
                        step.pending.forget((r.i32()?, r.i32()?));
                        Ok(())
                    },
                )
                .build()
        })
    }

    /// 1.18 adds the simulation distance; a 1.17.1 server simulates as far
    /// as it sends chunks. Biomes lose their terrain shape.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut login = Login::read(OLDER, &packet.data)?;
        // INTEGRATION: the registries codec's conversion of the dimension codec.
        login.edit_biomes(common::remove_terrain_shape)?;
        self.pending.clear();
        self.world.clone_from(&login.world);
        cx.to_client(state, Packet::new(packet.id, login.write(NEWER)));
        Ok(())
    }

    fn level_chunk(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let (x, z) = (r.i32()?, r.i32()?);
        let column = chunk::column_1_17(&mut r)?;
        r.finish()?;
        let column = chunk::column(&mut self.shared, Direction::Clientbound, x, z, column)?;
        let light = self.pending.load((x, z)).unwrap_or_else(chunk::no_light);
        let mut w = Writer::with_capacity(column.len() + light.len() + 8);
        w.i32(x).i32(z).bytes(&column).bytes(&light);
        common::to_client(cx, NEWER, state, "level_chunk_with_light", w.into_inner());
        Ok(())
    }

    /// Light for a chunk the client has updates it as before; light for a
    /// chunk still to come waits to travel inside it.
    fn light_update(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let chunk = (r.varint()?, r.varint()?);
        let light = chunk::light(&mut r)?;
        r.finish()?;
        if self.pending.is_loaded(chunk) {
            cx.to_client(state, packet);
        } else {
            self.pending.hold(chunk, light.to_vec());
        }
        Ok(())
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

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
