//! A 1.16.1 client on a 1.16.2 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{BOOKS, HARDCORE, NEWER, OLDER, SETTINGS, SHOWN};
use crate::{
    Direction, Packet, State,
    codec::{
        HasShared, Shared,
        registries::{
            self, Dimension, Registry, Side, convert,
            form::{Entry, namespaced},
            login,
        },
    },
    nbt::{Compound, Nbt},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
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

const DIMENSION_TYPE: &str = "dimension_type";

impl Down {
    fn create() -> Box<dyn crate::step::Step> {
        Box::new(Self {
            shared: Shared::new(OLDER, NEWER),
        })
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(State::Play, "login", Self::login)
                .clientbound(State::Play, "respawn", Self::respawn)
                .serverbound(State::Play, "recipe_book_update", Self::recipe_book_update)
                .build()
        })
    }

    /// The codec becomes the dimension list (1.16.1 clients number biomes
    /// themselves), and the login's dimension type the name of its entry.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let shared = &mut self.shared;
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
        rw.list(|rw| rw.str().map(drop))?; // World names.
        let sent = login::read_codec(NEWER, &mut rw.r)?;
        let mut converted = login::convert_codec(shared, Direction::Clientbound, &sent)?;
        let Dimension::Element(element) = login::read_dimension(NEWER, &mut rw.r)? else {
            return Err(invalid("dimension type is not a compound"));
        };
        let world = rw.r.str()?.to_owned();
        let name = match registries::find_element(shared, Side::Backend, DIMENSION_TYPE, &element) {
            Some(name) => namespaced(name),
            None => add_type(shared, &sent, &mut converted, &element)?,
        };
        login::write_codec(OLDER, &converted, &mut rw.w)?;
        rw.w.string(&name).string(&world);
        registries::record_dimension(
            shared,
            Side::Backend,
            Dimension::Element(element),
            Some(world.clone()),
        );
        registries::record_dimension(shared, Side::Client, Dimension::Name(name), Some(world));
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

    /// The element names the entry it equals, else the vanilla type with
    /// its effects: 1.16.1 clients know only the types `login` listed.
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
/// A login dimension no registry entry describes gets an entry of its own
/// on both sides: 1.16.1 clients look the login's dimension up by name, and
/// a later respawn into the same element finds it again. Returns its name.
fn add_type(
    shared: &mut Shared,
    sent: &[Registry],
    converted: &mut [Registry],
    element: &Compound,
) -> io::Result<String> {
    let list = converted
        .iter_mut()
        .find(|registry| registry.name == DIMENSION_TYPE)
        .ok_or_else(|| invalid("codec without dimension types"))?;
    let name = format!("rift:dimension_{}", list.entries.len());
    let next = |registry: &Registry| {
        registry
            .entries
            .iter()
            .map(|entry| entry.id + 1)
            .max()
            .unwrap_or(0)
    };
    let mut backend = sent
        .iter()
        .find(|registry| registry.name == DIMENSION_TYPE)
        .cloned()
        .unwrap_or_else(|| Registry::new(DIMENSION_TYPE));
    backend.entries.push(Entry {
        name: name.clone(),
        id: next(&backend),
        element: Some(Nbt::Compound(element.clone())),
    });
    registries::record(shared, Side::Backend, &backend);
    let client_element = convert::element_for(
        DIMENSION_TYPE,
        NEWER,
        OLDER,
        &name,
        Nbt::Compound(element.clone()),
    )?;
    list.entries.push(Entry {
        name: name.clone(),
        id: next(list),
        element: Some(client_element),
    });
    registries::record(shared, Side::Client, list);
    Ok(name)
}

routed_step!(Down);

pub const STEP: Option<Make> = Some(Down::create);
