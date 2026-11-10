//! A 1.17.1 client on a 1.18 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{Make, v1_16_4_to_1_17::common},
    wire::{Reader, Rewriter, Writer, invalid},
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

/// The longest objective and team names a 1.17.1 client reads.
const MAX_NAME: usize = 16;
/// The longest score holder a 1.17.1 client reads in `set_score`.
const MAX_HOLDER: usize = 40;

impl Down {
    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::Play;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Play, "login", common::login::<Self>)
                .clientbound(Play, "respawn", common::respawn::<Self>)
                .clientbound(Play, "set_objective", Self::set_objective)
                .clientbound(Play, "set_display_objective", Self::display_objective)
                .clientbound(Play, "set_score", Self::set_score)
                .clientbound(Play, "set_player_team", Self::set_player_team)
                // A 1.17.1 client neither knows nor needs it.
                .drop_clientbound(Play, "set_simulation_distance")
                .serverbound(Play, "client_information", Self::client_information)
                .build()
        })
    }

    /// 1.18 lifted the length limits of scoreboard names; a 1.17.1 client
    /// fails on a longer one. Names are cut the same way in every packet,
    /// so they still refer to each other.
    fn set_objective(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.w.string(&truncate(rw.r.str()?, MAX_NAME));
        match rw.i8()? {
            0 | 2 => {
                rw.str()?;
                rw.varint()?;
            }
            1 => {}
            _ => return Err(invalid("unknown objective action")),
        }
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

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
        w.i8(position).string(&truncate(name, MAX_NAME));
        cx.to_client(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

    fn set_score(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.w.string(&truncate(rw.r.str()?, MAX_HOLDER));
        let action = rw.varint()?;
        rw.w.string(&truncate(rw.r.str()?, MAX_NAME));
        match action {
            0 => {
                rw.varint()?;
            }
            1 => {}
            _ => return Err(invalid("unknown score action")),
        }
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// Only the team's name is limited; members' names never were.
    fn set_player_team(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.w.string(&truncate(rw.r.str()?, MAX_NAME));
        let mode = rw.i8()?;
        if matches!(mode, 0 | 2) {
            // Display name, options, name tag visibility, collision rule,
            // colour, prefix and suffix.
            rw.str()?;
            rw.i8()?;
            rw.string(40)?;
            rw.string(40)?;
            rw.varint()?;
            rw.str()?;
            rw.str()?;
        }
        match mode {
            0 | 3 | 4 => rw.list(|rw| rw.str().map(drop))?,
            1 | 2 => {}
            _ => return Err(invalid("unknown team action")),
        }
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
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

pub const STEP: Option<Make> = Some(|| {
    Box::new(Down {
        shared: Shared::new(OLDER, NEWER),
    })
});
