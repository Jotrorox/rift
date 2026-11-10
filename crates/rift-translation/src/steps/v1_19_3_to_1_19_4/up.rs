//! A 1.19.4 client on a 1.19.3 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    ANIMATE_HURT, DAMAGE_TYPES, HURT_EVENTS, NEWER, OLDER,
    play::{self, PlayerPosition},
    registries::{self, DamageType},
};
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
    /// The player's entity ID, from join game.
    player: Option<i32>,
    /// The vehicle the player rides and all its passengers, to dismount
    /// the player when a teleport asks for it.
    vehicle: Option<(i32, Vec<i32>)>,
    /// The client's damage type ID for each 1.19.3 hurt event.
    hurt_types: Vec<(i8, i32)>,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

fn damage_types() -> &'static [DamageType] {
    static TYPES: OnceLock<Vec<DamageType>> = OnceLock::new();
    TYPES.get_or_init(|| registries::parse_damage_types(DAMAGE_TYPES))
}

impl Up {
    pub(super) fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            player: None,
            vehicle: None,
            hurt_types: Vec::new(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(State::Play, "login", Self::login)
                .clientbound(State::Play, "player_position", Self::player_position)
                .clientbound(State::Play, "entity_event", Self::entity_event)
                .clientbound(State::Play, "animate", Self::animate)
                .tap(
                    Direction::Clientbound,
                    State::Play,
                    "set_passengers",
                    Self::passengers,
                )
                .tap(
                    Direction::Clientbound,
                    State::Play,
                    "remove_entities",
                    Self::removed,
                )
                .tap(
                    Direction::Clientbound,
                    State::Play,
                    "respawn",
                    Self::respawned,
                )
                .build()
        })
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let direction = Direction::Clientbound;
        let mut hurt_types = Vec::new();
        let login = play::rewrite_login(&packet.data, OLDER, NEWER, |codec| {
            // INTEGRATION: registry codec conversion (see `registries`).
            registries::convert_biomes(&mut self.shared, direction, codec)?;
            registries::ensure_damage_types(codec, damage_types())?;
            registries::ensure_registry(codec, "trim_pattern");
            registries::ensure_registry(codec, "trim_material");
            let names = registries::damage_type_names(codec);
            for (event, kind) in HURT_EVENTS {
                if let Some((id, _)) = names.iter().find(|(_, name)| name == kind) {
                    hurt_types.push((event, *id));
                }
            }
            Ok(())
        })?;
        self.player = Some(login.player_id);
        self.vehicle = None;
        self.hurt_types = hurt_types;
        cx.to_client(state, Packet::new(packet.id, login.body));
        Ok(())
    }

    /// 1.19.4 has no "dismount first" flag: dismount the player with the
    /// passenger list it would have after leaving the vehicle.
    fn player_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let position = play::read_player_position(&packet.data, OLDER)?;
        if position.dismount
            && let (Some(player), Some((vehicle, passengers))) = (self.player, self.vehicle.take())
        {
            let remaining: Vec<i32> = passengers
                .into_iter()
                .filter(|passenger| *passenger != player)
                .collect();
            let id = Self::routes().client().expect_packet(
                state,
                Direction::Clientbound,
                "set_passengers",
            );
            cx.to_client(
                state,
                Packet::new(id, play::write_passengers(vehicle, &remaining)),
            );
        }
        let position = PlayerPosition {
            dismount: false,
            ..position
        };
        cx.to_client(
            state,
            Packet::new(packet.id, play::write_player_position(&position, NEWER)),
        );
        Ok(())
    }

    /// Hurt entity events become damage events naming the matching type,
    /// without a source.
    fn entity_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.i32()?;
        let event = reader.i8()?;
        reader.finish()?;
        let Some((_, damage_type)) = self.hurt_types.iter().find(|(hurt, _)| *hurt == event) else {
            cx.to_client(state, packet);
            return Ok(());
        };
        let mut writer = Writer::with_capacity(8);
        writer
            .varint(entity)
            .varint(*damage_type)
            .varint(0) // no cause entity (IDs are sent plus one)
            .varint(0) // no direct entity
            .bool(false); // no source position
        let id =
            Self::routes()
                .client()
                .expect_packet(state, Direction::Clientbound, "damage_event");
        cx.to_client(state, Packet::new(id, writer.into_inner()));
        Ok(())
    }

    fn animate(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        let action = reader.u8()?;
        reader.finish()?;
        if action != ANIMATE_HURT {
            cx.to_client(state, packet);
            return Ok(());
        }
        let mut writer = Writer::with_capacity(8);
        writer.varint(entity).f32(0.0); // yaw: 1.19.3 had no direction
        let id =
            Self::routes()
                .client()
                .expect_packet(state, Direction::Clientbound, "hurt_animation");
        cx.to_client(state, Packet::new(id, writer.into_inner()));
        Ok(())
    }

    fn passengers(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let Some(player) = self.player else {
            return Ok(());
        };
        let (vehicle, passengers) = play::read_passengers(&packet.data)?;
        if passengers.contains(&player) {
            self.vehicle = Some((vehicle, passengers));
        } else if self
            .vehicle
            .as_ref()
            .is_some_and(|(riding, _)| *riding == vehicle)
        {
            self.vehicle = None;
        }
        Ok(())
    }

    fn removed(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        if let Some((vehicle, _)) = self.vehicle
            && play::read_removed(&packet.data)?.contains(&vehicle)
        {
            self.vehicle = None;
        }
        Ok(())
    }

    fn respawned(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.vehicle = None;
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
