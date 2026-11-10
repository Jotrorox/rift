//! A 1.19.4 client on a 1.20 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{NEWER, OLDER, UPDATE_1_20};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{Make, v1_18_2_to_1_19::join, v1_19_3_to_1_19_4::play},
    wire::Rewriter,
};
use std::{io, sync::OnceLock};

pub struct Down {
    shared: Shared,
    /// Where the backend last asked to edit the back of a sign, until the
    /// client answers: a 1.19.4 client has a single side to edit.
    back_sign: Option<i64>,
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
            back_sign: None,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(State::Play, "login", Self::login)
                .clientbound(State::Play, "respawn", Self::respawn)
                .clientbound(State::Play, "open_sign_editor", Self::open_sign_editor)
                .clientbound(State::Play, "player_combat_end", Self::combat_end)
                .clientbound(State::Play, "update_enabled_features", Self::features)
                .serverbound(State::Play, "sign_update", Self::sign_update)
                .build()
        })
    }

    /// The registries codec converts the registries (damage types, trim
    /// patterns whose template the receiver has); the portal cooldown comes
    /// or goes.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = join::convert_login(&mut self.shared, Direction::Clientbound, &packet.data)?;
        self.back_sign = None;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = join::convert_respawn(&mut self.shared, Direction::Clientbound, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn open_sign_editor(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let position = rw.i64()?;
        let front = rw.r.bool()?;
        self.back_sign = (!front).then_some(position);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn combat_end(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.varint()?; // duration
        rw.w.i32(-1); // killer, never read
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn features(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut features = play::read_features(&packet.data)?;
        if !features.contains(&UPDATE_1_20) {
            features.push(UPDATE_1_20);
        }
        cx.to_client(
            state,
            Packet::new(packet.id, play::write_features(&features)),
        );
        Ok(())
    }

    /// The edit is of the side the backend asked for at that position.
    fn sign_update(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        let position = rw.i64()?;
        let back = self.back_sign.take() == Some(position);
        rw.w.bool(!back);
        for _ in 0..4 {
            rw.string(384)?;
        }
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));
