//! A 1.16 client on a 1.15.2 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    EnchantmentMenu, NEWER, OLDER,
    dimension::{self, VANILLA_WORLDS},
    parse_uuid,
};
use crate::{
    Direction, Packet, State,
    codec::{
        HasShared, Shared,
        registries::{Dimension, login},
    },
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub struct Up {
    shared: Shared,
    menu: EnchantmentMenu,
    /// The speeds the backend last gave the player, which 1.15 servers
    /// expect back in serverbound `player_abilities`.
    flying_speed: f32,
    walking_speed: f32,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// 1.16's `previous game mode` when there is none.
const NO_GAME_MODE: u8 = 0xff;

impl Up {
    fn create() -> Box<dyn crate::step::Step> {
        Box::new(Self {
            shared: Shared::new(NEWER, OLDER),
            menu: EnchantmentMenu::default(),
            flying_speed: 0.05,
            walking_speed: 0.1,
        })
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::Clientbound;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(State::Login, "login_finished", Self::login_finished)
                .clientbound(State::Play, "login", Self::login)
                .clientbound(State::Play, "respawn", Self::respawn)
                .clientbound(State::Play, "container_set_data", Self::container_set_data)
                .tap(Clientbound, State::Play, "open_screen", Self::open_screen)
                .tap(
                    Clientbound,
                    State::Play,
                    "player_abilities",
                    Self::abilities,
                )
                .serverbound(State::Play, "interact", Self::interact)
                .serverbound(State::Play, "player_abilities", Self::player_abilities)
                .serverbound(State::Play, "set_jigsaw_block", Self::set_jigsaw_block)
                .drop_serverbound(State::Play, "jigsaw_generate")
                .build()
        })
    }

    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let uuid = parse_uuid(rw.r.string(36)?)?;
        rw.w.uuid(uuid);
        rw.string(16)?; // Name.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// The vanilla dimension codec and world list: a 1.15 server has the
    /// three vanilla dimensions and nothing else.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let shared = &mut self.shared;
        let mut rw = Rewriter::new(&packet.data);
        rw.i32()?; // Entity ID.
        rw.u8()?; // Game mode, with the hardcore bit.
        rw.w.u8(NO_GAME_MODE);
        rw.w.varint(VANILLA_WORLDS.len() as i32);
        for world in VANILLA_WORLDS {
            rw.w.string(world);
        }
        let codec = login::convert_codec(shared, Direction::Clientbound, &[])?;
        login::write_codec(NEWER, &codec, &mut rw.w)?;
        let dimension = Self::dimension(shared, rw.r.i32()?)?;
        rw.w.string(&dimension).string(&dimension);
        rw.i64()?; // Hashed seed.
        rw.u8()?; // Max players.
        let (debug, flat) = dimension::flags(rw.r.string(16)?);
        rw.varint()?; // View distance.
        rw.bool()?; // Reduced debug info.
        rw.bool()?; // Respawn screen.
        rw.w.bool(debug).bool(flat);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// The dimension type, and world, of a 1.15 dimension ID (the
    /// registries codec has recorded it for the client already).
    fn dimension(shared: &Shared, legacy: i32) -> io::Result<String> {
        match login::convert_dimension(shared, Direction::Clientbound, Dimension::Legacy(legacy))? {
            Dimension::Name(name) => Ok(name),
            _ => Err(invalid("1.16 names dimensions")),
        }
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let dimension = Self::dimension(&self.shared, reader.i32()?)?;
        let seed = reader.i64()?;
        let game_mode = reader.u8()?;
        let (debug, flat) = dimension::flags(reader.string(16)?);
        reader.finish()?;
        let mut writer = Writer::new();
        writer
            .string(&dimension)
            .string(&dimension)
            .i64(seed)
            .u8(game_mode)
            .u8(NO_GAME_MODE)
            .bool(debug)
            .bool(flat)
            // 1.15 servers expect clients to keep the player's attributes
            // over a respawn; 1.14 and older ones resend them.
            .bool(cx.backend_protocol >= 573);
        cx.to_client(state, Packet::new(packet.id, writer.buf));
        Ok(())
    }
    fn open_screen(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.menu.observe(OLDER, packet)
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

    fn abilities(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        reader.i8()?; // Flags.
        self.flying_speed = reader.f32()?;
        self.walking_speed = reader.f32()?;
        Ok(())
    }

    /// The sneaking flag 1.15 servers do not read.
    fn interact(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.varint()?; // Entity ID.
        let action = rw.varint()?;
        match action {
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
        rw.r.bool()?;
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn player_abilities(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i8()?; // Flags: only "flying" still means anything.
        rw.w.f32(self.flying_speed).f32(self.walking_speed);
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// 1.15's attachment type is 1.16's target name; the jigsaw's own name
    /// and its joint type are new.
    fn set_jigsaw_block(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.copy(8)?; // Position.
        rw.r.str()?; // Name.
        rw.str()?; // Target, the attachment type.
        rw.str()?; // Pool.
        rw.str()?; // Final state.
        rw.r.str()?; // Joint type.
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(Up::create);
