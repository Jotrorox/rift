//! A 1.16.2 client on a 1.16.1 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{BOOKS, HARDCORE, NEWER, OLDER, SETTINGS, SHOWN};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared, registries::login},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub struct Up {
    shared: Shared,
    /// Open and filtering, per recipe book: 1.16.2 clients change one book
    /// at a time, 1.16.1 servers want all four at once.
    books: [(bool, bool); BOOKS as usize],
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Up {
    fn create() -> Box<dyn crate::step::Step> {
        Box::new(Self {
            shared: Shared::new(NEWER, OLDER),
            books: Default::default(),
        })
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(State::Play, "login", Self::login)
                .clientbound(State::Play, "respawn", Self::respawn)
                .tap(Direction::Clientbound, State::Play, "recipe", Self::recipe)
                .serverbound(
                    State::Play,
                    "recipe_book_change_settings",
                    Self::recipe_book_change_settings,
                )
                .serverbound(
                    State::Play,
                    "recipe_book_seen_recipe",
                    Self::recipe_book_seen_recipe,
                )
                .build()
        })
    }

    /// The dimension list becomes the codec of dimension types and the
    /// biomes the 1.16.1 backend numbers as vanilla does; the login's
    /// dimension becomes its type's element.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let shared = &mut self.shared;
        let mut rw = Rewriter::new(&packet.data);
        rw.i32()?; // Entity ID.
        let game_mode = rw.r.u8()?;
        rw.w.bool(game_mode & HARDCORE != 0)
            .u8(game_mode & !HARDCORE);
        rw.u8()?; // Previous game mode.
        rw.list(|rw| rw.str().map(drop))?; // World names.
        login::transcode_codec(shared, Direction::Clientbound, &mut rw)?;
        let mut ahead = rw.r;
        ahead.str()?;
        let world = ahead.str()?.to_owned();
        login::transcode_dimension(shared, Direction::Clientbound, &mut rw, Some(&world))?;
        rw.str()?; // World name.
        rw.i64()?; // Hashed seed.
        let max_players = rw.r.u8()?;
        rw.w.varint(i32::from(max_players));
        rw.varint()?; // View distance.
        rw.bool()?; // Reduced debug info.
        rw.bool()?; // Respawn screen.
        rw.bool()?; // Debug.
        rw.bool()?; // Flat.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        login::transcode_respawn(&mut self.shared, Direction::Clientbound, &mut rw)?;
        rw.i64()?; // Hashed seed.
        rw.u8()?; // Game mode.
        rw.u8()?; // Previous game mode.
        rw.bool()?; // Debug.
        rw.bool()?; // Flat.
        rw.bool()?; // Keep attributes.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// The crafting and furnace books' state as the backend sets it.
    fn recipe(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        reader.varint()?; // Action.
        for book in &mut self.books[..2] {
            *book = (reader.bool()?, reader.bool()?);
        }
        Ok(())
    }

    fn recipe_book_change_settings(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let book = reader.varint()?;
        let settings = (reader.bool()?, reader.bool()?);
        reader.finish()?;
        if !(0..BOOKS).contains(&book) {
            return Err(invalid("invalid recipe book"));
        }
        self.books[book as usize] = settings;
        let mut writer = Writer::with_capacity(9);
        writer.varint(SETTINGS);
        for (open, filtering) in self.books {
            writer.bool(open).bool(filtering);
        }
        cx.to_backend(state, Packet::new(Self::recipe_book_update(), writer.buf));
        Ok(())
    }

    fn recipe_book_seen_recipe(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.w.varint(SHOWN);
        rw.str()?; // Recipe.
        cx.to_backend(state, Packet::new(Self::recipe_book_update(), rw.finish()?));
        Ok(())
    }

    fn recipe_book_update() -> i32 {
        Self::routes().backend().expect_packet(
            State::Play,
            Direction::Serverbound,
            "recipe_book_update",
        )
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(Up::create);
