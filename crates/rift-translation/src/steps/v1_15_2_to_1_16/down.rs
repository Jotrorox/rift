//! A 1.15.2 client on a 1.16 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{
    EnchantmentMenu, NEWER, OLDER,
    dimension::{self, Types},
    format_uuid, legacy_attribute, legacy_map_color,
};
use crate::{
    Direction, Packet, State,
    codec::{self, HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub struct Down {
    shared: Shared,
    menu: EnchantmentMenu,
    /// Whether the client last said it started or stopped sneaking; 1.16
    /// servers want it with every `interact`.
    sneaking: bool,
    dimensions: Types,
    /// The world and 1.15 dimension the client is in.
    world: String,
    dimension: i32,
    /// The player's entity ID and its attributes as the backend last sent
    /// them, to restore after a respawn that keeps them: 1.15 clients
    /// always start over with default attributes.
    player: i32,
    attributes: Vec<Attribute>,
}

/// One attribute of the player, as `update_attributes` carries it.
struct Attribute {
    name: String,
    base: f64,
    /// UUID, amount, operation.
    modifiers: Vec<(u128, f64, i8)>,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// `add_global_entity`'s type for a lightning bolt.
const LIGHTNING: i8 = 1;
/// Vanilla 1.16's default jigsaw name and joint, for what 1.15 cannot say.
const EMPTY_JIGSAW_NAME: &str = "minecraft:empty";
const ROLLABLE: &str = "rollable";

impl Down {
    fn create() -> Box<dyn crate::step::Step> {
        Box::new(Self {
            shared: Shared::new(OLDER, NEWER),
            menu: EnchantmentMenu::default(),
            sneaking: false,
            dimensions: Types::default(),
            world: String::new(),
            dimension: dimension::OVERWORLD,
            player: 0,
            attributes: Vec::new(),
        })
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::{Clientbound, Serverbound};
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(State::Login, "login_finished", Self::login_finished)
                .clientbound(State::Play, "login", Self::login)
                .clientbound(State::Play, "respawn", Self::respawn)
                .clientbound(State::Play, "add_entity", Self::add_entity)
                .clientbound(State::Play, "set_equipment", Self::set_equipment)
                .clientbound(State::Play, "container_set_data", Self::container_set_data)
                .clientbound(State::Play, "map_item_data", Self::map_item_data)
                .tap(Clientbound, State::Play, "open_screen", Self::open_screen)
                .tap(
                    Clientbound,
                    State::Play,
                    "update_attributes",
                    Self::attributes,
                )
                .tap(
                    Serverbound,
                    State::Play,
                    "player_command",
                    Self::player_command,
                )
                .serverbound(State::Play, "interact", Self::interact)
                .serverbound(State::Play, "player_abilities", Self::player_abilities)
                .serverbound(State::Play, "set_jigsaw_block", Self::set_jigsaw_block)
                .build()
        })
    }

    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let uuid = rw.r.uuid()?;
        rw.w.string(&format_uuid(uuid));
        rw.string(16)?; // Name.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        self.player = rw.i32()?;
        rw.u8()?; // Game mode, with the hardcore bit.
        rw.r.u8()?; // Previous game mode.
        for _ in 0..rw.r.count()? {
            rw.r.str()?; // World names.
        }
        self.dimensions = Types::read(&mut rw.r)?;
        self.dimension = self.dimensions.legacy(rw.r.str()?);
        self.world = rw.r.str()?.to_owned();
        rw.w.i32(self.dimension);
        rw.i64()?; // Hashed seed.
        rw.u8()?; // Max players.
        let mut tail = Writer::new();
        tail.varint(rw.r.varint()?) // View distance.
            .bool(rw.r.bool()?) // Reduced debug info.
            .bool(rw.r.bool()?); // Respawn screen.
        let (debug, flat) = (rw.r.bool()?, rw.r.bool()?);
        rw.w.string(dimension::level_type(debug, flat));
        rw.w.bytes(&tail.buf);
        self.attributes.clear();
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// A 1.15 client only reloads its world when the dimension changes, so
    /// moving between two worlds of the same dimension goes through
    /// another dimension first.
    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let dimension = self.dimensions.legacy(reader.str()?);
        let world = reader.str()?;
        let seed = reader.i64()?;
        let game_mode = reader.u8()?;
        reader.u8()?; // Previous game mode.
        let (debug, flat) = (reader.bool()?, reader.bool()?);
        let keep_attributes = reader.bool()?;
        reader.finish()?;
        let respawn = |dimension: i32, level_type: &str| {
            let mut writer = Writer::new();
            writer
                .i32(dimension)
                .i64(seed)
                .u8(game_mode)
                .string(level_type);
            Packet::new(packet.id, writer.buf)
        };
        if dimension == self.dimension && world != self.world {
            let elsewhere = if dimension == dimension::OVERWORLD {
                dimension::NETHER
            } else {
                dimension::OVERWORLD
            };
            cx.to_client(state, respawn(elsewhere, "default"));
        }
        cx.to_client(
            state,
            respawn(dimension, dimension::level_type(debug, flat)),
        );
        self.dimension = dimension;
        self.world = world.to_owned();
        if !keep_attributes {
            self.attributes.clear();
        } else if !self.attributes.is_empty() {
            let mut writer = Writer::new();
            writer.varint(self.player).i32(self.attributes.len() as i32);
            for attribute in &self.attributes {
                writer
                    .string(&legacy_attribute(&attribute.name))
                    .f64(attribute.base)
                    .varint(attribute.modifiers.len() as i32);
                for (uuid, amount, operation) in &attribute.modifiers {
                    writer.uuid(*uuid).f64(*amount).i8(*operation);
                }
            }
            let id = Self::routes().client().expect_packet(
                State::Play,
                Direction::Clientbound,
                "update_attributes",
            );
            cx.to_client(state, Packet::new(id, writer.buf));
        }
        Ok(())
    }

    fn attributes(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        if reader.varint()? != self.player {
            return Ok(());
        }
        let count = reader.i32()?;
        for _ in 0..count.max(0) {
            let name = reader.str()?.to_owned();
            let base = reader.f64()?;
            let mut modifiers = Vec::new();
            for _ in 0..reader.count()? {
                modifiers.push((reader.uuid()?, reader.f64()?, reader.i8()?));
            }
            let attribute = Attribute {
                name,
                base,
                modifiers,
            };
            match self
                .attributes
                .iter_mut()
                .find(|known| known.name == attribute.name)
            {
                Some(known) => *known = attribute,
                None => self.attributes.push(attribute),
            }
        }
        reader.finish()
    }

    /// Lightning goes back to its own packet. Everything else is the
    /// entity codec's.
    fn add_entity(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let backend = Self::routes().backend();
        let types = backend.expect_registry("entity_type");
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        reader.uuid()?;
        let kind = reader.varint()?;
        if Some(kind) != types.id("lightning_bolt") {
            return self.other_entity(
                cx,
                state,
                packet,
                kind == types.id("falling_block").unwrap_or(-1),
            );
        }
        let (x, y, z) = (reader.f64()?, reader.f64()?, reader.f64()?);
        reader.take(2 + 4 + 6)?; // Rotation, data, velocity.
        reader.finish()?;
        let mut writer = Writer::new();
        writer.varint(entity).i8(LIGHTNING).f64(x).f64(y).f64(z);
        let id = Self::routes().client().expect_packet(
            State::Play,
            Direction::Clientbound,
            "add_global_entity",
        );
        cx.to_client(state, Packet::new(id, writer.buf));
        Ok(())
    }

    /// INTEGRATION: hand the packet to the entity codec's `add_entity`
    /// conversion once it is public. Until then: the entity type and a
    /// falling block's state, the only registry IDs the packet carries.
    fn other_entity(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
        falling_block: bool,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.varint()?; // Entity ID.
        rw.copy(16)?; // UUID.
        let kind = rw.r.varint()?;
        let Some(kind) = self
            .shared
            .mapping(Direction::Clientbound, "entity_type")
            .get(kind)
        else {
            return Ok(());
        };
        rw.w.varint(kind);
        rw.copy(3 * 8 + 2)?; // Position, rotation.
        let data = rw.r.i32()?;
        rw.w.i32(if falling_block {
            let stone = Self::routes()
                .client()
                .block_states
                .parse("stone")
                .unwrap_or(0);
            self.shared
                .mapping(Direction::Clientbound, "block_state")
                .get_or(data, stone)
        } else {
            data
        });
        rw.copy(6)?; // Velocity.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// One packet per slot.
    fn set_equipment(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let id = Self::routes().client().expect_packet(
            State::Play,
            Direction::Clientbound,
            "set_equipped_item",
        );
        let mut rw = Rewriter::new(&packet.data);
        let entity = rw.r.varint()?;
        let mut packets = Vec::new();
        loop {
            let slot = rw.r.i8()?;
            let index = i32::from(slot & 0x7f);
            if index > 5 {
                return Err(invalid("invalid equipment slot"));
            }
            let start = rw.w.buf.len();
            // INTEGRATION: the item codec's stack conversion.
            codec::item::transcode(&mut self.shared, Direction::Clientbound, &mut rw)?;
            let mut writer = Writer::new();
            writer
                .varint(entity)
                .varint(index)
                .bytes(&rw.w.buf[start..]);
            packets.push(Packet::new(id, writer.buf));
            if slot >= 0 {
                break;
            }
        }
        rw.finish()?;
        for packet in packets {
            cx.to_client(state, packet);
        }
        Ok(())
    }

    fn open_screen(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.menu.observe(NEWER, packet)
    }

    fn container_set_data(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = self
            .menu
            .rewrite(&mut self.shared, Direction::Clientbound, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// Nether wood colours on maps.
    fn map_item_data(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.varint()?; // Map ID.
        rw.i8()?; // Scale.
        rw.bool()?; // Tracking position.
        rw.bool()?; // Locked.
        for _ in 0..rw.count()? {
            rw.varint()?; // Type.
            rw.copy(3)?; // X, Z, rotation.
            if rw.bool()? {
                // INTEGRATION: the text codec's component conversion.
                codec::text::transcode(&mut self.shared, Direction::Clientbound, &mut rw)?;
            }
        }
        if rw.u8()? > 0 {
            rw.copy(3)?; // Rows, X, Z.
            let colors = rw.r.byte_array()?;
            let colors: Vec<u8> = colors
                .iter()
                .map(|color| legacy_map_color(*color))
                .collect();
            rw.w.byte_array(&colors);
        }
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn player_command(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        reader.varint()?; // Entity ID.
        match reader.varint()? {
            0 => self.sneaking = true,
            1 => self.sneaking = false,
            _ => {}
        }
        Ok(())
    }

    fn interact(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.varint()?; // Entity ID.
        match rw.varint()? {
            0 => drop(rw.varint()?),
            1 => {}
            2 => {
                rw.f32()?;
                rw.f32()?;
                rw.f32()?;
                rw.varint()?; // Hand.
            }
            _ => return Err(invalid("invalid interaction")),
        }
        rw.w.bool(self.sneaking);
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// Only "flying" is left, and the speeds go.
    fn player_abilities(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let flags = reader.i8()?;
        reader.f32()?;
        reader.f32()?;
        reader.finish()?;
        cx.to_backend(state, Packet::new(packet.id, vec![(flags & 0x02) as u8]));
        Ok(())
    }

    fn set_jigsaw_block(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.copy(8)?; // Position.
        let target = rw.r.str()?;
        rw.w.string(EMPTY_JIGSAW_NAME).string(target);
        rw.str()?; // Pool.
        rw.str()?; // Final state.
        rw.w.string(ROLLABLE);
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(Down::create);
