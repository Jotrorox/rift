//! A 1.16.1 client on a 1.16.2 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{
    BOOKS, HARDCORE, NEWER, OLDER, SETTINGS, SHOWN,
    dimension::{self, Types},
    unpack_section,
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

pub struct Down {
    shared: Shared,
    /// The backend's dimension types, to name the elements it sends.
    types: Types,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Down {
    fn create() -> Box<dyn crate::step::Step> {
        Box::new(Self {
            shared: Shared::new(OLDER, NEWER),
            types: Types::default(),
        })
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(State::Play, "login", Self::login)
                .clientbound(State::Play, "respawn", Self::respawn)
                .clientbound(
                    State::Play,
                    "section_blocks_update",
                    Self::section_blocks_update,
                )
                .serverbound(State::Play, "recipe_book_update", Self::recipe_book_update)
                .build()
        })
    }

    fn element(reader: &mut Reader<'_>) -> io::Result<crate::nbt::Compound> {
        match Nbt::read(reader, true)? {
            Some(Nbt::Compound(element)) => Ok(element),
            _ => Err(invalid("dimension type is not a compound")),
        }
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i32()?; // Entity ID.
        let hardcore = rw.r.bool()?;
        let game_mode = rw.r.u8()?;
        rw.w.u8(if hardcore {
            game_mode | HARDCORE
        } else {
            game_mode
        });
        rw.u8()?; // Previous game mode.
        for _ in 0..rw.count()? {
            rw.str()?; // World names.
        }
        let codec = match Nbt::read(&mut rw.r, true)? {
            Some(Nbt::Compound(codec)) => codec,
            _ => return Err(invalid("missing dimension codec")),
        };
        self.types = Types::from_registry(&codec)?;
        let element = Self::element(&mut rw.r)?;
        // A dimension no registry entry describes is listed under a name of
        // its own: 1.16.1 clients look the login's dimension up by name.
        let name = match self.types.exact(&element) {
            Some(name) => name.to_owned(),
            None => self.types.add(element),
        };
        dimension::list(&self.types).write(&mut rw.w, true);
        rw.w.string(&name);
        rw.str()?; // World name.
        rw.i64()?; // Hashed seed.
        let max_players = rw.r.varint()?;
        rw.w.u8(max_players.clamp(0, 255) as u8);
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
        let element = Self::element(&mut rw.r)?;
        rw.w.string(self.types.name(&element)?);
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

    /// Back to the chunk column with absolute heights. Sections outside the
    /// 0 to 255 a 1.16.1 client knows cannot be expressed and are dropped.
    fn section_blocks_update(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let (x, section, z) = unpack_section(reader.i64()?);
        reader.bool()?; // Suppress light updates.
        let mapping = self.shared.mapping(Direction::Clientbound, "block_state");
        let client = Self::routes().client();
        let stone = client.block_states.parse("stone").unwrap_or(0);
        let mut records = Writer::new();
        let mut count = 0;
        for _ in 0..reader.count()? {
            let record = reader.varlong()?;
            let y = section * 16 + (record & 15) as i32;
            let Ok(y) = u8::try_from(y) else {
                continue;
            };
            let state = i32::try_from(record >> 12).map_err(|_| invalid("invalid block state"))?;
            records
                .u8((record >> 4) as u8)
                .u8(y)
                .varint(mapping.get_or(state, stone));
            count += 1;
        }
        reader.finish()?;
        if count == 0 {
            return Ok(());
        }
        let mut writer = Writer::with_capacity(9 + records.len());
        writer.i32(x).i32(z).varint(count).bytes(&records.buf);
        let id = client.expect_packet(State::Play, Direction::Clientbound, "chunk_blocks_update");
        cx.to_client(state, Packet::new(id, writer.buf));
        Ok(())
    }

    /// The recipe shown, or all four books' settings, one packet per book.
    fn recipe_book_update(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let backend = Self::routes().backend();
        let mut reader = Reader::new(&packet.data);
        match reader.varint()? {
            SHOWN => {
                let mut writer = Writer::new();
                writer.string(reader.str()?);
                reader.finish()?;
                let id = backend.expect_packet(
                    State::Play,
                    Direction::Serverbound,
                    "recipe_book_seen_recipe",
                );
                cx.to_backend(state, Packet::new(id, writer.buf));
            }
            SETTINGS => {
                let mut packets = Vec::with_capacity(BOOKS as usize);
                for book in 0..BOOKS {
                    let mut writer = Writer::with_capacity(3);
                    writer
                        .varint(book)
                        .bool(reader.bool()?)
                        .bool(reader.bool()?);
                    packets.push(writer.buf);
                }
                reader.finish()?;
                let id = backend.expect_packet(
                    State::Play,
                    Direction::Serverbound,
                    "recipe_book_change_settings",
                );
                for body in packets {
                    cx.to_backend(state, Packet::new(id, body));
                }
            }
            _ => return Err(invalid("invalid recipe book update")),
        }
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(Down::create);
