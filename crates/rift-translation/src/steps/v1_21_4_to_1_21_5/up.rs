//! A 1.21.5 client on a 1.21.4 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data, routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::Reader,
};
use std::{io, sync::OnceLock};

pub struct Up {
    shared: Shared,
    /// The global index of the next `player_chat` the client gets; it
    /// counts from 0 in every play phase, like the 1.21.5 server's.
    chat_index: i32,
    random: u64,
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
            chat_index: 0,
            random: 0x9e37_79b9_7f4a_7c15,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        use Direction::*;
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .tap(
                    Clientbound,
                    State::Configuration,
                    "finish_configuration",
                    Self::new_play_phase,
                )
                .clientbound(State::Play, "add_experience_orb", Self::add_experience_orb)
                .clientbound(State::Play, "player_chat", Self::player_chat)
                .clientbound(State::Play, "entity_event", Self::entity_event)
                .serverbound(State::Play, "chat", Self::chat)
                .serverbound(
                    State::Play,
                    "chat_command_signed",
                    Self::chat_command_signed,
                )
                // Test blocks are new in 1.21.5; a 1.21.4 server has none.
                .drop_serverbound(State::Play, "set_test_block")
                .drop_serverbound(State::Play, "test_instance_block_action")
                .build()
        })
    }

    fn new_play_phase(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.chat_index = 0;
        Ok(())
    }

    fn add_experience_orb(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let (spawn, entity_data) = super::orb_to_entity(&packet.data)?;
        let client = data::version(NEWER);
        let id = |name| client.expect_packet(state, Direction::Clientbound, name);
        cx.to_client(state, Packet::new(id("add_entity"), spawn));
        cx.to_client(state, Packet::new(id("set_entity_data"), entity_data));
        Ok(())
    }

    fn player_chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let index = Some(self.chat_index);
        let body = super::with_text(&mut self.shared, Direction::Clientbound, |text| {
            super::player_chat(&packet.data, index, text)
        })?;
        self.chat_index = self.chat_index.wrapping_add(1);
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// The shield events become the sounds 1.21.4 clients played for them.
    fn entity_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.i32()?;
        let event = reader.i8()?;
        reader.finish()?;
        let Some((sound, volume)) = super::shield_sound(event) else {
            cx.to_client(state, packet);
            return Ok(());
        };
        let random = super::next_random(&mut self.random);
        let body = super::entity_sound(entity, sound, volume, random);
        let id = data::version(NEWER).expect_packet(state, Direction::Clientbound, "sound_entity");
        cx.to_client(state, Packet::new(id, body));
        Ok(())
    }

    fn chat(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = super::chat_checksum(&packet.data, false, false)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }

    fn chat_command_signed(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = super::chat_checksum(&packet.data, true, false)?;
        cx.to_backend(state, Packet::new(packet.id, body));
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
