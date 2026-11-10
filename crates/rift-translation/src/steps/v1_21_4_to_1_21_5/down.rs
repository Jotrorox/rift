//! A 1.21.4 client on a 1.21.5 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data, routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
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
    fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            Self::builder()
                .clientbound(State::Play, "add_entity", Self::add_entity)
                .clientbound(State::Play, "player_chat", Self::player_chat)
                // Test results only show in the 1.21.5 test block screens.
                .drop_clientbound(State::Play, "test_instance_block_status")
                .serverbound(State::Play, "chat", Self::chat)
                .serverbound(
                    State::Play,
                    "chat_command_signed",
                    Self::chat_command_signed,
                )
                .build()
        })
    }

    fn builder() -> RouteBuilder<Self> {
        RouteBuilder::new(OLDER, NEWER).defaults()
    }

    /// The shared codecs' routes alone, for the packets this step takes
    /// over only in part. Dispatching through them runs the codecs' taps
    /// for the packet a second time.
    fn defaults() -> &'static Routes<Self> {
        static DEFAULTS: OnceLock<Routes<Down>> = OnceLock::new();
        DEFAULTS.get_or_init(|| Self::builder().build())
    }

    /// Orbs go back to their own packet; every other entity is the entity
    /// codec's.
    fn add_entity(&mut self, cx: &mut Cx<'_>, state: State, mut packet: Packet) -> io::Result<()> {
        let Some((orb, motion)) = super::entity_to_orb(&packet.data)? else {
            // INTEGRATION: the entity codec's `add_entity` default (or a
            // pass-through until it has one) handles the rest.
            packet.id =
                data::version(NEWER).expect_packet(state, Direction::Clientbound, "add_entity");
            return Self::defaults().dispatch(self, cx, Direction::Clientbound, state, packet);
        };
        let client = data::version(OLDER);
        let id = |name| client.expect_packet(state, Direction::Clientbound, name);
        cx.to_client(state, Packet::new(id("add_experience_orb"), orb));
        if let Some(motion) = motion {
            cx.to_client(state, Packet::new(id("set_entity_motion"), motion));
        }
        Ok(())
    }

    fn player_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::with_text(&mut self.shared, Direction::Clientbound, |text| {
            super::player_chat(&packet.data, None, text)
        })?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::chat_checksum(&packet.data, false, true)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn chat_command_signed(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::chat_checksum(&packet.data, true, true)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
