//! A 1.21.7 client on a 1.21.9 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
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

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

pub struct Down {
    shared: Shared,
    /// The player's rotation as the client last reported it, for 1.21.9
    /// rotations relative to it.
    yaw: f32,
    pitch: f32,
    /// Client ticks since the client last renewed its tick time
    /// subscription, while it is subscribed.
    subscribed_ticks: Option<u32>,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Down {
    fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            yaw: 0.0,
            pitch: 0.0,
            subscribed_ticks: None,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::Serverbound;
            use State::{Configuration, Play};
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Configuration, "code_of_conduct", Self::code_of_conduct)
                .clientbound(Play, "set_entity_motion", Self::set_entity_motion)
                .clientbound(Play, "player_rotation", Self::player_rotation)
                .clientbound(Play, "set_default_spawn_position", Self::spawn_position)
                // Debug renderers a 1.21.7 client draws from data it never asks for.
                .drop_clientbound(Play, "debug/block_value")
                .drop_clientbound(Play, "debug/chunk_value")
                .drop_clientbound(Play, "debug/entity_value")
                .drop_clientbound(Play, "debug/event")
                .drop_clientbound(Play, "game_test_highlight_pos")
                .tap(Serverbound, Play, "move_player_rot", Self::follow_rotation)
                .tap(
                    Serverbound,
                    Play,
                    "move_player_pos_rot",
                    Self::follow_position_rotation,
                )
                .serverbound(
                    Play,
                    "debug_sample_subscription",
                    Self::debug_sample_subscription,
                )
                .serverbound(Play, "client_tick_end", Self::client_tick_end)
                .build()
        })
    }

    /// A 1.21.7 client cannot show a code of conduct, so the step accepts it
    /// on the player's behalf; the server holds configuration until then.
    fn code_of_conduct(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        r.str()?;
        r.finish()?;
        let id = Self::routes().backend().expect_packet(
            state,
            Direction::Serverbound,
            "accept_code_of_conduct",
        );
        cx.to_backend(state, Packet::empty(id));
        Ok(())
    }

    fn set_entity_motion(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let data = super::motion_to_shorts(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// Relative angles are resolved against the rotation the client last
    /// reported, as a 1.21.9 client resolves them against its own.
    fn player_rotation(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let ((yaw, relative_yaw), (pitch, relative_pitch)) =
            super::read_flagged_rotation(&packet.data)?;
        if relative_yaw {
            self.yaw += yaw;
        } else {
            self.yaw = yaw;
        }
        if relative_pitch {
            self.pitch += pitch;
        } else {
            self.pitch = pitch;
        }
        let mut body = Writer::with_capacity(8);
        body.f32(self.yaw).f32(self.pitch);
        cx.to_client(state, Packet::new(packet.id, body.into_inner()));
        Ok(())
    }

    fn spawn_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = super::respawn_data_to_spawn(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn follow_rotation(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        (self.yaw, self.pitch) = (r.f32()?, r.f32()?);
        Ok(())
    }

    fn follow_position_rotation(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        r.take(24)?; // Position.
        (self.yaw, self.pitch) = (r.f32()?, r.f32()?);
        Ok(())
    }

    fn debug_sample_subscription(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        super::check_tick_time_sample(&packet.data)?;
        // Only a first subscription needs telling: renewals keep it alive.
        if self.subscribed_ticks.replace(0).is_none() {
            let tick_time = Self::routes()
                .backend()
                .expect_registry("debug_subscription")
                .id(super::TICK_TIME_SUBSCRIPTION)
                .expect("1.21.9 has a tick time debug subscription");
            Self::request(cx, state, &[tick_time]);
        }
        Ok(())
    }

    /// End the subscription once the client has stopped renewing it, as a
    /// 1.21.7 server would after ten seconds.
    fn client_tick_end(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_backend(state, packet);
        if let Some(ticks) = &mut self.subscribed_ticks {
            *ticks += 1;
            if *ticks >= super::EXPIRE_AFTER_TICKS {
                self.subscribed_ticks = None;
                Self::request(cx, state, &[]);
            }
        }
        Ok(())
    }

    fn request(cx: &mut Cx<'_>, state: State, subscriptions: &[i32]) {
        let id = Self::routes().backend().expect_packet(
            state,
            Direction::Serverbound,
            "debug_subscription_request",
        );
        let mut body = Writer::with_capacity(1 + subscriptions.len());
        body.varint(subscriptions.len() as i32);
        for subscription in subscriptions {
            body.varint(*subscription);
        }
        cx.to_backend(state, Packet::new(id, body.into_inner()));
    }
}
routed_step!(Down);
