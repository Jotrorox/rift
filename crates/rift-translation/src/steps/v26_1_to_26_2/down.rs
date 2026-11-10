//! A 26.1 client on a 26.2 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER, copy_profile};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{Make, v1_21_11_to_26_1::join},
    wire::{Reader, Writer},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

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
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Login, "login_finished", Self::login_finished)
                .clientbound(Play, "login", Self::login)
                .serverbound(Play, "spectator_action", Self::spectator_action)
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
