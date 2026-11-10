//! A 26.2 client on a 26.3 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{
    CHANGE_DESTROY_DIRECTION, DESTROY_BLOCK, DESTROY_BLOCK_PARTICLES, LevelEvent, NEWER,
    NEWER_ONLY_EVENTS, OLDER, SWING_NONE, animate, brewing_stand_screen, container_data,
    copy_sign_lines,
    movement::{self, Move},
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{Make, v1_21_11_to_26_1::join},
    wire::{Reader, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

pub struct Down {
    shared: Shared,
    brewing_stand: Option<i32>,
    /// The rotation the client last reported.
    rotation: Option<[f32; 2]>,
    /// A teleport the client accepted, completed by the movement packet a
    /// 26.2 client always sends right after accepting.
    accepted: Option<i32>,
    /// That movement packet, held until the end of the client tick (see
    /// `moved`).
    deferred: Option<Packet>,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

struct Ids {
    animate: i32,
    punch: i32,
    accept_teleportation: i32,
}

impl Down {
    fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            brewing_stand: None,
            rotation: None,
            accepted: None,
            deferred: None,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .drop_clientbound(Configuration, "post_effects")
                .drop_clientbound(Play, "post_effects")
                .drop_clientbound(Play, "add_transient_block")
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .clientbound(Play, "move_entity_pos", Self::move_pos)
                .clientbound(Play, "move_entity_pos_rot", Self::move_pos_rot)
                .clientbound(Play, "move_entity_rot", Self::move_rot)
                .clientbound(Play, "entity_position_sync", Self::position_sync)
                .clientbound(Play, "animate", Self::animate)
                .clientbound(Play, "swing_animation", Self::swing_animation)
                .clientbound(Play, "open_sign_editor", Self::open_sign_editor)
                .clientbound(Play, "level_event", Self::level_event)
                .clientbound(Play, "container_set_data", Self::container_set_data)
                .tap(Clientbound, Play, "open_screen", Self::open_screen)
                .tap(Clientbound, Play, "container_close", Self::container_closed)
                .tap(Serverbound, Play, "container_close", Self::container_closed)
                .serverbound(Play, "accept_teleportation", Self::accept_teleportation)
                .serverbound(Play, "move_player_pos", Self::move_player_pos)
                .serverbound(Play, "move_player_pos_rot", Self::move_player_pos_rot)
                .tap(Serverbound, Play, "move_player_rot", Self::move_player_rot)
                .serverbound(Play, "swing", Self::swing)
                .serverbound(Play, "player_action", Self::player_action)
                .serverbound(Play, "sign_update", Self::sign_update)
                .serverbound(Play, "client_tick_end", Self::client_tick_end)
                .build()
        })
    }

    fn ids() -> &'static Ids {
        static IDS: OnceLock<Ids> = OnceLock::new();
        IDS.get_or_init(|| {
            let routes = Self::routes();
            let (client, backend) = (routes.client(), routes.backend());
            Ids {
                animate: client.expect_packet(State::Play, Direction::Clientbound, "animate"),
                punch: backend.expect_packet(State::Play, Direction::Serverbound, "punch"),
                accept_teleportation: backend.expect_packet(
                    State::Play,
                    Direction::Serverbound,
                    "accept_teleportation",
                ),
            }
        })
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut login = join::Login::parse(NEWER, &packet.data)?;
        login
            .spawn
            .convert(&mut self.shared, Direction::Clientbound);
        self.spawned();
        cx.to_client(state, Packet::new(packet.id, login.to_bytes(OLDER)));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut respawn = join::Respawn::parse(NEWER, &packet.data)?;
        respawn
            .spawn
            .convert(&mut self.shared, Direction::Clientbound);
        self.spawned();
        cx.to_client(state, Packet::new(packet.id, respawn.to_bytes(OLDER)));
        Ok(())
    }

    fn spawned(&mut self) {
        self.deferred = None;
        self.brewing_stand = None;
    }

    fn movement(cx: &mut Cx<'_>, state: State, packet: Packet, kind: Move) -> io::Result<()> {
        let parts = movement::downgrade(kind, &packet.data)?;
        let pos_id = Self::routes().client().expect_packet(
            State::Play,
            Direction::Clientbound,
            "move_entity_pos",
        );
        for (part, data) in parts {
            let id = if part == kind { packet.id } else { pos_id };
            cx.to_client(state, Packet::new(id, data));
        }
        Ok(())
    }

    fn move_pos(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        Self::movement(cx, state, packet, Move::Pos)
    }

    fn move_pos_rot(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        Self::movement(cx, state, packet, Move::PosRot)
    }

    fn move_rot(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        Self::movement(cx, state, packet, Move::Rot)
    }

    fn position_sync(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = movement::downgrade_sync(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn animate(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let action = r.u8()?;
        r.finish()?;
        if let Some(older) = animate::NEWER.get(usize::from(action)) {
            let mut w = Writer::new();
            w.varint(entity).u8(*older);
            cx.to_client(state, Packet::new(packet.id, w.into_inner()));
        }
        Ok(())
    }

    /// A swing becomes the arm swing of its hand; its type and duration
    /// are the 26.2 client's own business. No swing is no packet.
    fn swing_animation(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let hand = join::hand(r.varint()?)?;
        let kind = r.varint()?;
        r.varint()?;
        r.finish()?;
        if kind != SWING_NONE {
            let mut w = Writer::new();
            w.varint(entity).u8(if hand == 0 {
                animate::SWING_MAIN_HAND
            } else {
                animate::SWING_OFF_HAND
            });
            cx.to_client(state, Packet::new(Self::ids().animate, w.into_inner()));
        }
        Ok(())
    }

    fn open_sign_editor(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let position = r.i64()?;
        let slot = r.varint()?;
        r.finish()?;
        let mut w = Writer::with_capacity(9);
        w.i64(position).bool(super::sign_side_flag(slot));
        cx.to_client(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

    /// Events 26.2 does not know are dropped (their sounds and particles
    /// came with other events or from the client itself); particle-only
    /// block breaking becomes the breaking event with its sound.
    fn level_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut event = LevelEvent::parse(&packet.data)?;
        if NEWER_ONLY_EVENTS.contains(&event.event) {
            return Ok(());
        }
        if event.event == DESTROY_BLOCK_PARTICLES {
            event.event = DESTROY_BLOCK;
        }
        if let Some(event) = event.convert(&mut self.shared, Direction::Clientbound) {
            cx.to_client(state, Packet::new(packet.id, event.to_bytes()));
        }
        Ok(())
    }

    fn open_screen(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.brewing_stand = brewing_stand_screen(NEWER, &packet.data)?;
        Ok(())
    }

    fn container_closed(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.brewing_stand = None;
        Ok(())
    }

    /// A 26.2 brewing stand has two data slots: more would crash the client.
    fn container_set_data(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let (container, slot) = container_data(&packet.data)?;
        if self.brewing_stand != Some(container) || slot < 2 {
            cx.to_client(state, packet);
        }
        Ok(())
    }

    /// 26.3 accepts a teleport with the position and rotation the client
    /// took: a 26.2 client's acceptance and the movement packet it sends
    /// right after become one 26.3 acceptance.
    fn accept_teleportation(&mut self, _: &mut Cx<'_>, _: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let id = r.varint()?;
        r.finish()?;
        self.accepted = Some(id);
        Ok(())
    }

    /// Complete a pending acceptance with this movement, or pass it on.
    ///
    /// The movement packet still goes to the server, which may rely on it
    /// (Pumpkin updates the player's chunks only from movement packets),
    /// but only when the client sends no other position in that tick: a
    /// 26.3 server disconnects a player for two positions in one client
    /// tick. It is held until the tick ends, or dropped for the client's
    /// own movement of that tick.
    fn moved(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
        position: [f64; 3],
        rotation: Option<[f32; 2]>,
    ) {
        if rotation.is_some() {
            self.rotation = rotation;
        }
        let Some(id) = self.accepted.take() else {
            self.deferred = None;
            cx.to_backend(state, packet);
            return;
        };
        self.deferred = Some(packet.clone());
        let rotation = self.rotation.unwrap_or_default();
        let mut w = Writer::with_capacity(37);
        w.varint(id);
        for value in position {
            w.f64(value);
        }
        w.f32(rotation[0]).f32(rotation[1]);
        cx.to_backend(
            state,
            Packet::new(Self::ids().accept_teleportation, w.into_inner()),
        );
    }

    fn client_tick_end(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        if let Some(movement) = self.deferred.take() {
            cx.to_backend(state, movement);
        }
        cx.to_backend(state, packet);
        Ok(())
    }

    fn move_player_pos(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let position = [r.f64()?, r.f64()?, r.f64()?];
        r.u8()?;
        r.finish()?;
        self.moved(cx, state, packet, position, None);
        Ok(())
    }

    fn move_player_pos_rot(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let position = [r.f64()?, r.f64()?, r.f64()?];
        let rotation = [r.f32()?, r.f32()?];
        r.u8()?;
        r.finish()?;
        self.moved(cx, state, packet, position, Some(rotation));
        Ok(())
    }

    fn move_player_rot(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        if let (Ok(y_rot), Ok(x_rot)) = (r.f32(), r.f32()) {
            self.rotation = Some([y_rot, x_rot]);
        }
        Ok(())
    }

    /// A main-hand swing is a punch; 26.3 servers decide other swings
    /// themselves.
    fn swing(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let hand = join::hand(r.varint()?)?;
        r.finish()?;
        if hand == 0 {
            cx.to_backend(state, Packet::empty(Self::ids().punch));
        }
        Ok(())
    }

    fn player_action(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let action = r.varint()?;
        if !(0..=7).contains(&action) {
            return Err(invalid("invalid player action"));
        }
        let mut w = Writer::with_capacity(packet.data.len());
        w.varint(if action >= CHANGE_DESTROY_DIRECTION {
            action + 1
        } else {
            action
        });
        w.i64(r.i64()?).u8(r.u8()?).varint(r.varint()?);
        r.finish()?;
        cx.to_backend(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

    fn sign_update(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let position = r.i64()?;
        let front = r.bool()?;
        let mut w = Writer::with_capacity(packet.data.len() + 1);
        w.i64(position);
        copy_sign_lines(&mut r, &mut w)?;
        r.finish()?;
        w.varint(i32::from(front));
        cx.to_backend(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }
}
routed_step!(Down);

#[cfg(test)]
mod tests {
    #[test]
    fn every_packet_has_a_route() {
        assert_eq!(super::Down::routes().problems(), &[] as &[String]);
    }
}
