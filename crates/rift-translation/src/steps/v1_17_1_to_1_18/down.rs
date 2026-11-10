//! A 1.17.1 client on a 1.18 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER, chunk};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{
        Make,
        v1_16_4_to_1_17::common::{self, Login},
    },
    wire::{Reader, Rewriter, Writer},
};
use std::{io, sync::OnceLock};

pub struct Down {
    shared: Shared,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// 1.17.1 clients read objective names of at most 16 characters.
const MAX_OBJECTIVE: usize = 16;

impl Down {
    pub(super) fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::Clientbound;
            use State::Play;
            RouteBuilder::new(OLDER, NEWER)
                // The chunk codec's own handling replaces this once it lands.
                .default_handler(
                    Clientbound,
                    Play,
                    "level_chunk_with_light",
                    Self::level_chunk,
                )
                .defaults()
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "set_display_objective", Self::display_objective)
                // A 1.17.1 client neither knows nor needs it.
                .drop_clientbound(Play, "set_simulation_distance")
                .serverbound(Play, "client_information", Self::client_information)
                .build()
        })
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut login = Login::read(NEWER, &packet.data)?;
        // INTEGRATION: the registries codec's conversion of the dimension codec.
        login.edit_biomes(|biome| {
            common::add_terrain_shape(biome)?;
            common::known_category(biome, common::CATEGORIES_1_17)
        })?;
        cx.to_client(state, Packet::new(packet.id, login.write(OLDER)));
        Ok(())
    }

    /// The light goes first, as 1.17.1 servers send it.
    fn level_chunk(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let (x, z) = (r.i32()?, r.i32()?);
        let column = chunk::column_1_18(&mut r)?;
        let light = chunk::light(&mut r)?;
        r.finish()?;
        let column = chunk::column(&mut self.shared, Direction::Clientbound, x, z, column)?;
        let mut update = Writer::with_capacity(light.len() + 10);
        update.varint(x).varint(z).bytes(light);
        common::to_client(cx, OLDER, state, "light_update", update.into_inner());
        let mut w = Writer::with_capacity(column.len() + 8);
        w.i32(x).i32(z).bytes(&column);
        common::to_client(cx, OLDER, state, "level_chunk", w.into_inner());
        Ok(())
    }

    /// Longer names are cut: the name only has to match the objective's,
    /// which the scoreboard packets cut the same way.
    fn display_objective(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let position = r.i8()?;
        let name = r.str()?;
        r.finish()?;
        let mut w = Writer::new();
        w.i8(position).string(&truncate(name, MAX_OBJECTIVE));
        cx.to_client(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

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
        // Listed in the server list, as vanilla clients are by default.
        rw.w.bool(true);
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Down);

/// The first `max` UTF-16 units of `text`, without splitting a character.
pub(super) fn truncate(text: &str, max: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect()
}

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
