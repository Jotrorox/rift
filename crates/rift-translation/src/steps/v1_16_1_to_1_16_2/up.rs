//! A 1.16.2 client on a 1.16.1 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    BOOKS, Change, HARDCORE, NEWER, OLDER, SETTINGS, SHOWN,
    dimension::{self, Types},
    pack_section,
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    nbt::Nbt,
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub struct Up {
    shared: Shared,
    /// The backend's dimension types, as 1.16.2 elements.
    types: Types,
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
            types: Types::default(),
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
                .clientbound(
                    State::Play,
                    "chunk_blocks_update",
                    Self::chunk_blocks_update,
                )
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

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i32()?; // Entity ID.
        let game_mode = rw.r.u8()?;
        rw.w.bool(game_mode & HARDCORE != 0)
            .u8(game_mode & !HARDCORE);
        rw.u8()?; // Previous game mode.
        for _ in 0..rw.count()? {
            rw.str()?; // World names.
        }
        let codec =
            Nbt::read(&mut rw.r, true)?.ok_or_else(|| invalid("missing dimension codec"))?;
        let (codec, types) = dimension::upgrade(&codec)?;
        self.types = types;
        Nbt::Compound(codec).write(&mut rw.w, true);
        let element = self.types.element(rw.r.str()?)?;
        Nbt::Compound(element.clone()).write(&mut rw.w, true);
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
        let element = self.types.element(rw.r.str()?)?;
        Nbt::Compound(element.clone()).write(&mut rw.w, true);
        rw.str()?; // World name.
        rw.i64()?; // Hashed seed.
        rw.u8()?; // Game mode.
        rw.u8()?; // Previous game mode.
        rw.bool()?; // Debug.
        rw.bool()?; // Flat.
        rw.bool()?; // Keep attributes.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// One packet per section the changes touch.
    fn chunk_blocks_update(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let (x, z) = (reader.i32()?, reader.i32()?);
        let mapping = self.shared.mapping(Direction::Clientbound, "block_state");
        let client = Self::routes().client();
        let stone = client.block_states.parse("stone").unwrap_or(0);
        let mut sections: Vec<(i32, Vec<Change>)> = Vec::new();
        for _ in 0..reader.count()? {
            let horizontal = reader.u8()?;
            let y = reader.u8()?;
            let state = mapping.get_or(reader.varint()?, stone);
            let section = i32::from(y >> 4);
            let change = Change {
                x: horizontal >> 4,
                y: y & 15,
                z: horizontal & 15,
                state,
            };
            match sections.iter_mut().find(|(known, _)| *known == section) {
                Some((_, changes)) => changes.push(change),
                None => sections.push((section, vec![change])),
            }
        }
        reader.finish()?;
        sections.sort_by_key(|(section, _)| *section);
        let id = client.expect_packet(State::Play, Direction::Clientbound, "section_blocks_update");
        for (section, changes) in sections {
            let mut writer = Writer::with_capacity(12 + changes.len() * 4);
            writer
                .i64(pack_section(x, section, z))
                .bool(false) // Light updates are not suppressed.
                .varint(changes.len() as i32);
            for change in changes {
                let position =
                    i64::from(change.x) << 8 | i64::from(change.z) << 4 | i64::from(change.y);
                writer.varlong(i64::from(change.state) << 12 | position);
            }
            cx.to_client(state, Packet::new(id, writer.buf));
        }
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
