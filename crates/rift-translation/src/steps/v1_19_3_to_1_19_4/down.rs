//! A 1.19.3 client on a 1.19.4 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{
    ANIMATE_HURT, NEWER, OLDER, hurt_event,
    play::{self, PlayerPosition},
};
use crate::{
    Direction, Packet, State,
    codec::{
        HasShared, Shared,
        registries::{self, Side},
    },
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{Make, v1_18_2_to_1_19::join},
    wire::{Reader, Writer},
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

impl Down {
    pub(super) fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(State::Play, "login", Self::login)
                .clientbound(State::Play, "player_position", Self::player_position)
                .clientbound(State::Play, "damage_event", Self::damage_event)
                .clientbound(State::Play, "hurt_animation", Self::hurt_animation)
                .drop_clientbound(State::Play, "bundle_delimiter")
                // Re-sending whole chunks would mean caching every chunk.
                .drop_clientbound(State::Play, "chunks_biomes")
                .build()
        })
    }

    /// The registries codec drops the registries 1.19.3 does not
    /// synchronise; the rest of the layout is the same.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = join::convert_login(&mut self.shared, Direction::Clientbound, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn player_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let position = play::read_player_position(&packet.data, NEWER)?;
        let position = PlayerPosition {
            dismount: false,
            ..position
        };
        cx.to_client(
            state,
            Packet::new(packet.id, play::write_player_position(&position, OLDER)),
        );
        Ok(())
    }

    /// A damage event becomes the hurt entity event of its damage type, which
    /// plays the matching hurt animation and sound.
    fn damage_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        let damage_type = reader.varint()?;
        reader.varint()?; // cause entity, plus one
        reader.varint()?; // direct entity, plus one
        if reader.bool()? {
            for _ in 0..3 {
                reader.f64()?; // source position
            }
        }
        reader.finish()?;
        let event = registries::name_of(&self.shared, Side::Backend, "damage_type", damage_type)
            .map_or(2, hurt_event);
        let mut writer = Writer::with_capacity(5);
        writer.i32(entity).i8(event);
        let id =
            Self::routes()
                .client()
                .expect_packet(state, Direction::Clientbound, "entity_event");
        cx.to_client(state, Packet::new(id, writer.into_inner()));
        Ok(())
    }

    /// The hurt tilt becomes the "take damage" animation; 1.19.3 tilts the
    /// same way whatever the direction.
    fn hurt_animation(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        reader.f32()?; // yaw
        reader.finish()?;
        let mut writer = Writer::with_capacity(6);
        writer.varint(entity).u8(ANIMATE_HURT);
        let id = Self::routes()
            .client()
            .expect_packet(state, Direction::Clientbound, "animate");
        cx.to_client(state, Packet::new(id, writer.into_inner()));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
