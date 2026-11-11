//! A 1.12.1 client on a 1.12 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
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
                .serverbound(State::Play, "place_recipe", Self::place_recipe)
                .build()
        })
    }

    /// The 1.12 backend cannot place a recipe it is only told the name of.
    /// Show the recipe as a ghost in the grid instead, as a 1.12.1 server
    /// does when the player lacks ingredients: the player sees what the
    /// recipe needs and can place it by hand.
    fn place_recipe(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let window = reader.i8()?;
        let recipe = reader.varint()?;
        reader.bool()?; // Make all.
        reader.finish()?;
        let mut ghost = Writer::new();
        ghost.i8(window).varint(recipe);
        let id = Self::routes().client().expect_packet(
            State::Play,
            Direction::Clientbound,
            "place_ghost_recipe",
        );
        cx.to_client(state, Packet::new(id, ghost.into_inner()));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
