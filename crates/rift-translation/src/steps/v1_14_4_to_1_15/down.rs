//! A 1.14.4 client on a 1.15 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{IMMEDIATE_RESPAWN, NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub struct Down {
    shared: Shared,
    /// The backend's `doImmediateRespawn` game rule: 1.14 clients always
    /// show the death screen, so the step asks to respawn for them.
    immediate_respawn: bool,
    /// The player's entity ID, from `login`.
    player: i32,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// Vanilla's `SoundSource.BLOCKS`, which explosions play in.
const BLOCKS: i32 = 4;
/// `ServerboundClientCommandPacket`: perform respawn.
const PERFORM_RESPAWN: i32 = 0;
/// `entity_event`: a living entity died.
const DEATH: i8 = 3;

impl Down {
    fn create() -> Box<dyn crate::step::Step> {
        Box::new(Self {
            shared: Shared::new(OLDER, NEWER),
            immediate_respawn: false,
            player: 0,
        })
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(State::Play, "login", Self::login)
                .clientbound(State::Play, "respawn", Self::respawn)
                .clientbound(State::Play, "game_event", Self::game_event)
                .clientbound(State::Play, "set_health", Self::set_health)
                .clientbound(State::Play, "explode", Self::explode)
                .clientbound(State::Play, "entity_event", Self::entity_event)
                .build()
        })
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        self.player = rw.i32()?;
        rw.u8()?; // Game mode.
        rw.i32()?; // Dimension.
        rw.r.i64()?; // Hashed seed.
        rw.u8()?; // Max players.
        rw.string(16)?; // Level type.
        rw.varint()?; // View distance.
        rw.bool()?; // Reduced debug info.
        self.immediate_respawn = !rw.r.bool()?;
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut rw = Rewriter::new(&packet.data);
        rw.i32()?; // Dimension.
        rw.r.i64()?; // Hashed seed.
        rw.u8()?; // Game mode.
        rw.string(16)?; // Level type.
        cx.to_client(state, Packet::new(packet.id, rw.finish()?));
        Ok(())
    }

    /// Reason 11 is new: follow it instead of passing on an event the
    /// client does not know.
    fn game_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let reason = reader.u8()?;
        let value = reader.f32()?;
        reader.finish()?;
        if reason == IMMEDIATE_RESPAWN {
            self.immediate_respawn = value == 1.0;
            return Ok(());
        }
        cx.to_client(state, packet);
        Ok(())
    }

    /// With immediate respawn on, a dead player respawns at once: the
    /// client still shows its death screen until the respawn arrives.
    fn set_health(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let health = reader.f32()?;
        reader.varint()?; // Food.
        reader.f32()?; // Saturation.
        reader.finish()?;
        cx.to_client(state, packet);
        if health <= 0.0 && self.immediate_respawn {
            let id = Self::routes().backend().expect_packet(
                State::Play,
                Direction::Serverbound,
                "client_command",
            );
            let mut body = Writer::new();
            body.varint(PERFORM_RESPAWN);
            cx.to_backend(state, Packet::new(id, body.buf));
        }
        Ok(())
    }

    /// 1.15 servers tell the dying player itself that it died; a 1.14
    /// client handles that as for any other entity and drops its loot,
    /// which needs the integrated server and fails.
    fn entity_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.i32()?;
        let event = reader.i8()?;
        reader.finish()?;
        if !(entity == self.player && event == DEATH) {
            cx.to_client(state, packet);
        }
        Ok(())
    }

    /// 1.15 clients play the explosion sound themselves; 1.15 servers no
    /// longer send it, so the step does as a 1.14 server would.
    fn explode(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let (x, y, z) = (reader.f32()?, reader.f32()?, reader.f32()?);
        reader.f32()?; // Power.
        let records = reader.i32()?;
        let records = usize::try_from(records)
            .ok()
            .and_then(|records| records.checked_mul(3))
            .ok_or_else(|| invalid("invalid explosion record count"))?;
        reader.take(records)?;
        reader.f32()?; // Player knockback.
        reader.f32()?;
        reader.f32()?;
        reader.finish()?;
        cx.to_client(state, packet);
        let client = Self::routes().client();
        let sound = client
            .expect_registry("sound_event")
            .id("entity.generic.explode")
            .ok_or_else(|| invalid("no explosion sound"))?;
        let mut body = Writer::new();
        body.varint(sound)
            .varint(BLOCKS)
            .i32((x * 8.0) as i32)
            .i32((y * 8.0) as i32)
            .i32((z * 8.0) as i32)
            .f32(4.0)
            .f32(0.7);
        let id = client.expect_packet(State::Play, Direction::Clientbound, "sound");
        cx.to_client(state, Packet::new(id, body.buf));
        Ok(())
    }
}
routed_step!(Down);

pub const STEP: Option<Make> = Some(Down::create);
