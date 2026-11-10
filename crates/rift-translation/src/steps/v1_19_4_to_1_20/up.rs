//! A 1.20 client on a 1.19.4 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{DAMAGE_TYPES, NEWER, OLDER, UPDATE_1_20};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{
        Make,
        v1_19_3_to_1_19_4::{
            play,
            registries::{self, DamageType},
        },
    },
    wire::Rewriter,
};
use std::{io, sync::OnceLock};

pub struct Up {
    shared: Shared,
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
    fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(NEWER, OLDER)
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

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let direction = Direction::Clientbound;
        let login = play::rewrite_login(&packet.data, OLDER, NEWER, |codec| {
            // INTEGRATION: registry codec conversion (see `registries`).
            registries::convert_biomes(&mut self.shared, direction, codec)?;
            registries::ensure_damage_types(codec, damage_types())?;
            registries::ensure_registry(codec, "trim_pattern");
            registries::ensure_registry(codec, "trim_material");
            registries::convert_trims(&mut self.shared, direction, codec)
        })?;
        cx.to_client(state, Packet::new(packet.id, login.body));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let body = play::rewrite_respawn(&packet.data, OLDER, NEWER)?;
        cx.to_client(state, Packet::new(packet.id, body));
        Ok(())
    }

    /// A 1.19.4 sign has only the side 1.20 calls the front.
    fn open_sign_editor(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i64()?; // position
        rw.w.bool(true);
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn combat_end(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.varint()?; // duration
        rw.r.i32()?; // killer, unused
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn features(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut features = play::read_features(&packet.data)?;
        features.retain(|feature| *feature != UPDATE_1_20 && *feature != "update_1_20");
        cx.to_client(
            state,
            Packet::new(packet.id, play::write_features(&features)),
        );
        Ok(())
    }

    /// Only the front exists on the backend; it never asks for the back.
    fn sign_update(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i64()?; // position
        let front = rw.r.bool()?;
        for _ in 0..4 {
            rw.string(384)?;
        }
        let body = rw.finish()?;
        if front {
            cx.to_backend(state, Packet::new(packet.id, body));
        }
        Ok(())
    }
}
routed_step!(Up);

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
