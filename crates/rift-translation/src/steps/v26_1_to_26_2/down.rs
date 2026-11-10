//! A 26.1 client on a 26.2 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER, beds, copy_profile};
use crate::{
    Direction, Packet, State,
    codec::{
        HasShared, Shared,
        chunk::{self, model::Chunk, packets::read_position},
    },
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{Make, v1_21_11_to_26_1::join},
    wire::{Reader, Writer},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

pub struct Down {
    shared: Shared,
    /// Unpacked section data, reused between columns.
    scratch: Vec<u32>,
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
            scratch: Vec::new(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Login, "login_finished", Self::login_finished)
                .clientbound(Play, "login", Self::login)
                .serverbound(Play, "spectator_action", Self::spectator_action)
                .clientbound(Play, "level_chunk_with_light", Self::level_chunk)
                .clientbound(Play, "block_update", Self::block_update)
                .clientbound(Play, "section_blocks_update", Self::section_blocks_update)
                .build()
        })
    }

    /// The session UUID has nowhere to go in 26.1.
    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let mut w = Writer::with_capacity(packet.data.len());
        copy_profile(&mut r, &mut w)?;
        r.uuid()?;
        r.finish()?;
        cx.to_client(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut login = join::Login::parse(NEWER, &packet.data)?;
        login
            .spawn
            .convert(&mut self.shared, Direction::Clientbound);
        login.online_mode = None;
        cx.to_client(state, Packet::new(packet.id, login.to_bytes(OLDER)));
        Ok(())
    }

    /// The chunk codec's conversion, plus the block entities of beds.
    fn level_chunk(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut chunk = Chunk::read(&packet.data, NEWER, &self.shared.chunk.world)?;
        chunk::packets::convert_chunk(&mut self.shared, &mut chunk)?;
        beds::add_to_chunk(&mut chunk, &mut self.scratch)?;
        cx.to_client(state, Packet::new(packet.id, chunk.write(OLDER)?));
        Ok(())
    }

    fn block_update(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let position = read_position(&mut r, NEWER)?;
        let block = r.varint()?;
        chunk::blocks::block_update(self, cx, state, packet)?;
        beds::after_block_update(&mut self.shared, cx, state, position, block)
    }

    fn section_blocks_update(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let body = packet.data.clone();
        chunk::blocks::section_blocks_update(self, cx, state, packet)?;
        beds::after_section_update(&mut self.shared, cx, state, &body, NEWER)
    }

    fn spectator_action(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let target = r.varint()?;
        r.finish()?;
        let mut w = Writer::new();
        w.varint(target.wrapping_add(1));
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
