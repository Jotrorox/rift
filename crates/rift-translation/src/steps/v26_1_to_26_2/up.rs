//! A 26.2 client on a 26.1 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{NEWER, OLDER, copy_profile};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{Make, v1_21_11_to_26_1::join},
    wire::{Reader, Writer},
};
use std::{
    hash::{BuildHasher, RandomState},
    io,
    sync::OnceLock,
};

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

pub struct Up {
    shared: Shared,
    /// The session UUID this connection's 26.2 client is told; a 26.1
    /// server has none.
    session: u128,
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
            session: random_uuid(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use State::*;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Login, "login_finished", Self::login_finished)
                .clientbound(Play, "login", Self::login)
                .serverbound(Play, "spectator_action", Self::spectator_action)
                .build()
        })
    }

    fn login_finished(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let mut w = Writer::with_capacity(packet.data.len() + 16);
        copy_profile(&mut r, &mut w)?;
        r.finish()?;
        w.uuid(self.session);
        cx.to_client(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

    /// Join game gains the online-mode flag, set: 26.1 clients always
    /// prepared their chat signing key, which a backend that enforces
    /// secure chat needs.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut login = join::Login::parse(OLDER, &packet.data)?;
        login
            .spawn
            .convert(&mut self.shared, Direction::Clientbound);
        login.online_mode = Some(true);
        cx.to_client(state, Packet::new(packet.id, login.to_bytes(NEWER)));
        Ok(())
    }

    /// A click on an entity keeps its ID; a click on nothing only resets the
    /// idle timer on a 26.2 server and has no 26.1 equivalent.
    fn spectator_action(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let target = r.varint()?;
        r.finish()?;
        if target != 0 {
            let mut w = Writer::new();
            w.varint(target - 1);
            cx.to_backend(state, Packet::new(packet.id, w.into_inner()));
        }
        Ok(())
    }
}
routed_step!(Up);

/// A version 4 UUID from the standard library's randomly seeded hasher.
fn random_uuid() -> u128 {
    let state = RandomState::new();
    let high = u128::from(state.hash_one(0x5249_4654u32));
    let low = u128::from(state.hash_one(0x3236_2e32u32));
    let uuid = high << 64 | low;
    uuid & !(0xf000 << 64 | 0xc000 << 48) | 0x4000 << 64 | 0x8000 << 48
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_packet_has_a_route() {
        assert_eq!(super::Up::routes().problems(), &[] as &[String]);
    }

    #[test]
    fn session_uuids_are_version_4() {
        let uuid = super::random_uuid();
        assert_eq!(uuid >> 76 & 0xf, 4);
        assert_eq!(uuid >> 62 & 0x3, 2);
        assert_ne!(uuid, super::random_uuid());
    }
}
