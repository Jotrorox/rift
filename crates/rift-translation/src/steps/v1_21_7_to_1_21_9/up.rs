//! A 1.21.9 client on a 1.21.7 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

pub struct Up {
    shared: Shared,
    /// The dimension the player is in, from the last `login` or `respawn`:
    /// where a 1.21.7 server's spawn position lies.
    dimension: String,
    /// Client ticks since the last tick time subscription sent to the
    /// backend, while the client is subscribed.
    subscribed_ticks: Option<u32>,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Up {
    fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            dimension: super::DEFAULT_DIMENSION.to_owned(),
            subscribed_ticks: None,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::Clientbound;
            use State::{Configuration, Play};
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .tap(Clientbound, Play, "login", Self::follow_login)
                .tap(Clientbound, Play, "respawn", Self::follow_respawn)
                .clientbound(Play, "set_entity_motion", Self::set_entity_motion)
                .clientbound(Play, "player_rotation", Self::player_rotation)
                .clientbound(Play, "set_default_spawn_position", Self::spawn_position)
                // Only an answer to a code of conduct, which 1.21.7 never sends.
                .drop_serverbound(Configuration, "accept_code_of_conduct")
                .serverbound(Play, "debug_subscription_request", Self::debug_subscription)
                .serverbound(Play, "client_tick_end", Self::client_tick_end)
                .build()
        })
    }

    fn follow_login(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.dimension = super::spawn_dimension(&packet.data, false)?;
        Ok(())
    }

    fn follow_respawn(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.dimension = super::spawn_dimension(&packet.data, true)?;
        Ok(())
    }

    fn set_entity_motion(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let data = super::motion_to_vector(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn player_rotation(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = super::rotation_to_flagged(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn spawn_position(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = super::spawn_to_respawn_data(&packet.data, &self.dimension)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// Server tick times are the only debug data a 1.21.7 server streams;
    /// a request without them has nothing to ask for. A 1.21.9 client asks
    /// once, a 1.21.7 server forgets after ten seconds: see `client_tick_end`.
    fn debug_subscription(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let tick_time = Self::routes()
            .client()
            .expect_registry("debug_subscription")
            .id(super::TICK_TIME_SUBSCRIPTION)
            .expect("1.21.9 has a tick time debug subscription");
        if super::subscribes_to_tick_time(&packet.data, tick_time)? {
            self.subscribed_ticks = Some(0);
            Self::subscribe(cx, state);
        } else {
            self.subscribed_ticks = None;
        }
        Ok(())
    }

    /// Renew the client's subscription every `RENEW_AFTER_TICKS` client
    /// ticks, as a 1.21.7 client would every five seconds.
    fn client_tick_end(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        cx.to_backend(state, packet);
        if let Some(ticks) = &mut self.subscribed_ticks {
            *ticks += 1;
            if *ticks >= super::RENEW_AFTER_TICKS {
                *ticks = 0;
                Self::subscribe(cx, state);
            }
        }
        Ok(())
    }

    fn subscribe(cx: &mut Cx<'_>, state: State) {
        let id = Self::routes().backend().expect_packet(
            state,
            Direction::Serverbound,
            "debug_sample_subscription",
        );
        let mut body = crate::wire::Writer::with_capacity(1);
        body.varint(super::TICK_TIME_SAMPLE);
        cx.to_backend(state, Packet::new(id, body.into_inner()));
    }
}
routed_step!(Up);
