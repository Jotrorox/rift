//! A 26.3 client on a 26.2 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{
    ABORT_DESTROY_BLOCK, BREWING_STAND_EXTRA_DATA, CHANGE_DESTROY_DIRECTION, DESTROY_PROGRESS,
    DESTROY_PROGRESS_AND_SOUND, INSTANT_SPLASH_POTION, INSTANT_SPLASH_POTION_SOUND, LevelEvent,
    NEWER, OLDER, SPLASH_POTION, SPLASH_POTION_SOUND, START_DESTROY_BLOCK, STOP_DESTROY_BLOCK,
    SWING_DURATION, SWING_WHACK, WAX_ON, animate, brewing_stand_screen, container_data,
    copy_sign_lines,
    movement::{self, Move},
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::{Make, v1_21_11_to_26_1::join},
    wire::{Position, Reader, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

/// Ticks after which a block the player still seems to be mining stops
/// showing progress: a server that never said the block is gone.
const MAX_DESTROY_TICKS: u32 = 6000;
const CREATIVE: i32 = 1;

pub struct Up {
    shared: Shared,
    game_mode: i32,
    /// The block the player is mining, for the progress level events a 26.3
    /// client expects from the server every tick.
    destroying: Option<Destroying>,
    /// The open brewing stand's container ID, and whether it still needs
    /// the data slots 26.2 does not send.
    brewing_stand: Option<(i32, bool)>,
}

#[derive(Clone, Copy, Debug)]
struct Destroying {
    position: i64,
    direction: u8,
    ticks: u32,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

struct Ids {
    swing_animation: i32,
    level_event: i32,
    sound: i32,
    move_player_pos_rot: i32,
    swing: i32,
    /// The client's `item.honeycomb.wax_on` sound event.
    wax_on_sound: Option<i32>,
}

impl Up {
    fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            game_mode: 0,
            destroying: None,
            brewing_stand: None,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::*;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Play, "login", Self::login)
                .clientbound(Play, "respawn", Self::respawn)
                .tap(Clientbound, Play, "game_event", Self::game_event)
                .clientbound(Play, "move_entity_pos", Self::move_pos)
                .clientbound(Play, "move_entity_pos_rot", Self::move_pos_rot)
                .clientbound(Play, "move_entity_rot", Self::move_rot)
                .clientbound(Play, "entity_position_sync", Self::position_sync)
                .clientbound(Play, "animate", Self::animate)
                .clientbound(Play, "open_sign_editor", Self::open_sign_editor)
                .clientbound(Play, "level_event", Self::level_event)
                .clientbound(Play, "container_set_data", Self::container_set_data)
                .tap(Clientbound, Play, "open_screen", Self::open_screen)
                .tap(Clientbound, Play, "container_close", Self::container_closed)
                .tap(Serverbound, Play, "container_close", Self::container_closed)
                .tap(Clientbound, Play, "block_update", Self::block_update)
                .tap(
                    Clientbound,
                    Play,
                    "section_blocks_update",
                    Self::section_update,
                )
                .serverbound(Play, "accept_teleportation", Self::accept_teleportation)
                .serverbound(Play, "punch", Self::punch)
                .serverbound(Play, "player_action", Self::player_action)
                .serverbound(Play, "sign_update", Self::sign_update)
                .serverbound(Play, "client_tick_end", Self::client_tick_end)
                .build()
        })
    }

    fn ids() -> &'static Ids {
        static IDS: OnceLock<Ids> = OnceLock::new();
        IDS.get_or_init(|| {
            let routes = Self::routes();
            let (client, backend) = (routes.client(), routes.backend());
            let to_client = |name| client.expect_packet(State::Play, Direction::Clientbound, name);
            let to_backend =
                |name| backend.expect_packet(State::Play, Direction::Serverbound, name);
            Ids {
                swing_animation: to_client("swing_animation"),
                level_event: to_client("level_event"),
                sound: to_client("sound"),
                move_player_pos_rot: to_backend("move_player_pos_rot"),
                swing: to_backend("swing"),
                wax_on_sound: client
                    .expect_registry("sound_event")
                    .id("item.honeycomb.wax_on"),
            }
        })
    }

    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut login = join::Login::parse(OLDER, &packet.data)?;
        login
            .spawn
            .convert(&mut self.shared, Direction::Clientbound);
        self.spawned(login.spawn.game_mode);
        cx.to_client(state, Packet::new(packet.id, login.to_bytes(NEWER)));
        Ok(())
    }

    fn respawn(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut respawn = join::Respawn::parse(OLDER, &packet.data)?;
        respawn
            .spawn
            .convert(&mut self.shared, Direction::Clientbound);
        self.spawned(respawn.spawn.game_mode);
        cx.to_client(state, Packet::new(packet.id, respawn.to_bytes(NEWER)));
        Ok(())
    }

    fn spawned(&mut self, game_mode: i32) {
        self.game_mode = game_mode;
        self.destroying = None;
        self.brewing_stand = None;
    }

    fn game_event(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        if let Some(mode) = join::game_event_mode(&packet.data) {
            self.game_mode = mode;
            if mode == CREATIVE {
                self.destroying = None;
            }
        }
        Ok(())
    }

    fn movement(cx: &mut Cx<'_>, state: State, packet: Packet, kind: Move) -> io::Result<()> {
        let data = movement::upgrade(kind, &packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    fn move_pos(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        Self::movement(cx, state, packet, Move::Pos)
    }

    fn move_pos_rot(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        Self::movement(cx, state, packet, Move::PosRot)
    }

    fn move_rot(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        Self::movement(cx, state, packet, Move::Rot)
    }

    fn position_sync(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let data = movement::upgrade_sync(&packet.data)?;
        cx.to_client(state, Packet::new(packet.id, data));
        Ok(())
    }

    /// Arm swings become `swing_animation` with the default swing; the other
    /// actions take their 26.3 numbers. The unused action 1 is dropped.
    fn animate(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let action = r.u8()?;
        r.finish()?;
        let mut w = Writer::new();
        w.varint(entity);
        let hand = match action {
            animate::SWING_MAIN_HAND => 0,
            animate::SWING_OFF_HAND => 1,
            _ => {
                let Some(newer) = animate::NEWER.iter().position(|known| *known == action) else {
                    return Ok(());
                };
                w.u8(newer as u8);
                cx.to_client(state, Packet::new(packet.id, w.into_inner()));
                return Ok(());
            }
        };
        w.varint(hand).varint(SWING_WHACK).varint(SWING_DURATION);
        cx.to_client(
            state,
            Packet::new(Self::ids().swing_animation, w.into_inner()),
        );
        Ok(())
    }

    fn open_sign_editor(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let position = r.i64()?;
        let front = r.bool()?;
        r.finish()?;
        let mut w = Writer::with_capacity(9);
        w.i64(position).varint(i32::from(front));
        cx.to_client(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

    /// Block states are mapped; a potion splash also gets the sound event
    /// 26.3 plays apart from the particles, and waxing its sound.
    fn level_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let event = LevelEvent::parse(&packet.data)?;
        let Some(event) = event.convert(&mut self.shared, Direction::Clientbound) else {
            return Ok(());
        };
        cx.to_client(state, Packet::new(packet.id, event.to_bytes()));
        let sound = match event.event {
            SPLASH_POTION => SPLASH_POTION_SOUND,
            INSTANT_SPLASH_POTION => INSTANT_SPLASH_POTION_SOUND,
            WAX_ON => {
                if let Some(sound) = Self::ids().wax_on_sound {
                    cx.to_client(state, block_sound(sound, event.position));
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        let sound = LevelEvent {
            event: sound,
            position: event.position,
            data: 0,
            global: false,
        };
        cx.to_client(state, Packet::new(packet.id, sound.to_bytes()));
        Ok(())
    }

    fn open_screen(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.brewing_stand = brewing_stand_screen(OLDER, &packet.data)?.map(|id| (id, true));
        Ok(())
    }

    fn container_closed(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.brewing_stand = None;
        Ok(())
    }

    /// After a brewing stand's first data, add the totals 26.2 implied.
    fn container_set_data(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let (container, _) = container_data(&packet.data)?;
        let id = packet.id;
        cx.to_client(state, packet);
        if let Some((open, pending @ true)) = &mut self.brewing_stand
            && *open == container
        {
            *pending = false;
            for (slot, value) in BREWING_STAND_EXTRA_DATA {
                let mut w = Writer::new();
                w.varint(container).i16(slot).i16(value);
                cx.to_client(state, Packet::new(id, w.into_inner()));
            }
        }
        Ok(())
    }

    fn block_update(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        if let Ok(position) = Reader::new(&packet.data).i64()
            && self
                .destroying
                .is_some_and(|block| block.position == position)
        {
            self.destroying = None;
        }
        Ok(())
    }

    fn section_update(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let Some(block) = self.destroying else {
            return Ok(());
        };
        let mut r = Reader::new(&packet.data);
        let (Ok(section), Ok(count)) = (r.i64(), r.count()) else {
            return Ok(());
        };
        let target = Position::from_modern(block.position);
        let (sx, sy, sz) = (section >> 42, section << 44 >> 44, section << 22 >> 42);
        if (sx, sy, sz)
            != (
                i64::from(target.x >> 4),
                i64::from(target.y >> 4),
                i64::from(target.z >> 4),
            )
        {
            return Ok(());
        }
        let local = (target.x & 15) << 8 | (target.z & 15) << 4 | (target.y & 15);
        for _ in 0..count {
            let Ok(entry) = r.varlong() else {
                return Ok(());
            };
            if (entry & 0xfff) as i32 == local {
                self.destroying = None;
                return Ok(());
            }
        }
        Ok(())
    }

    /// 26.3 needs no movement packet after accepting a teleport; 26.2
    /// servers expect the one 26.2 clients send right after.
    fn accept_teleportation(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let id = r.varint()?;
        let mut w = Writer::with_capacity(33);
        for _ in 0..3 {
            w.f64(r.f64()?);
        }
        w.f32(r.f32()?).f32(r.f32()?).u8(0);
        r.finish()?;
        let mut accept = Writer::new();
        accept.varint(id);
        cx.to_backend(state, Packet::new(packet.id, accept.into_inner()));
        cx.to_backend(
            state,
            Packet::new(Self::ids().move_player_pos_rot, w.into_inner()),
        );
        Ok(())
    }

    fn punch(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        if !packet.data.is_empty() {
            return Err(invalid("trailing punch data"));
        }
        let mut w = Writer::new();
        w.varint(0);
        cx.to_backend(state, Packet::new(Self::ids().swing, w.into_inner()));
        Ok(())
    }

    fn player_action(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let action = r.varint()?;
        let position = r.i64()?;
        let direction = r.u8()?;
        let sequence = r.varint()?;
        r.finish()?;
        if !(0..=8).contains(&action) || direction > 5 {
            return Err(invalid("invalid player action"));
        }
        match action {
            START_DESTROY_BLOCK if self.game_mode != CREATIVE => {
                self.destroying = Some(Destroying {
                    position,
                    direction,
                    ticks: 0,
                });
            }
            CHANGE_DESTROY_DIRECTION => {
                if let Some(block) = &mut self.destroying {
                    block.direction = direction;
                }
                return Ok(());
            }
            ABORT_DESTROY_BLOCK | STOP_DESTROY_BLOCK => self.destroying = None,
            _ => {}
        }
        let mut w = Writer::with_capacity(packet.data.len());
        w.varint(if action > CHANGE_DESTROY_DIRECTION {
            action - 1
        } else {
            action
        })
        .i64(position)
        .u8(direction)
        .varint(sequence);
        cx.to_backend(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

    fn sign_update(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let position = r.i64()?;
        let mut lines = Writer::new();
        copy_sign_lines(&mut r, &mut lines)?;
        let slot = r.varint()?;
        r.finish()?;
        let mut w = Writer::with_capacity(packet.data.len());
        w.i64(position)
            .bool(super::sign_side_flag(slot))
            .bytes(&lines.buf);
        cx.to_backend(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

    /// Every client tick while mining, the progress event a 26.3 server
    /// sends: with the sound every fourth tick.
    fn client_tick_end(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        if !packet.data.is_empty() {
            return Err(invalid("trailing client tick end data"));
        }
        cx.to_backend(state, packet);
        if let Some(block) = &mut self.destroying {
            block.ticks += 1;
            if block.ticks > MAX_DESTROY_TICKS {
                self.destroying = None;
                return Ok(());
            }
            let event = LevelEvent {
                event: if block.ticks % 4 == 0 {
                    DESTROY_PROGRESS_AND_SOUND
                } else {
                    DESTROY_PROGRESS
                },
                position: block.position,
                data: i32::from(block.direction),
                global: false,
            };
            cx.to_client(
                state,
                Packet::new(Self::ids().level_event, event.to_bytes()),
            );
        }
        Ok(())
    }
}
routed_step!(Up);

/// A block sound at the centre of a block, as the server plays it.
fn block_sound(sound: i32, position: i64) -> Packet {
    const BLOCKS: i32 = 4;
    let position = Position::from_modern(position);
    let mut w = Writer::with_capacity(32);
    w.varint(sound + 1).varint(BLOCKS);
    for coordinate in [position.x, position.y, position.z] {
        w.i32(coordinate.wrapping_mul(8).wrapping_add(4));
    }
    w.f32(1.0).f32(1.0).i64(0);
    Packet::new(Up::ids().sound, w.into_inner())
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_packet_has_a_route() {
        assert_eq!(super::Up::routes().problems(), &[] as &[String]);
    }
}
