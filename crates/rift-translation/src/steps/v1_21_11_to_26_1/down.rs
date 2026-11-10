//! A 1.21.11 client on a 26.1 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
use super::{
    ATTACK, INTERACT, INTERACT_AT, NEWER, OLDER, OVERWORLD_CLOCK, WORLD_CLOCK_REGISTRY, join,
};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Writer, invalid},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

pub struct Down {
    shared: Shared,
    /// The player's game mode: 1.21.11 spectators click entities with an
    /// attack, which 26.1 calls `spectator_action`.
    game_mode: i32,
    /// The entity and hand of the last `interact_at`, whose plain `interact`
    /// follow-up 26.1 does not need.
    interacted_at: Option<(i32, i32)>,
    clock: Clock,
}

/// The overworld clock, as 1.21.11's single day time.
#[derive(Default)]
struct Clock {
    /// The overworld clock's ID in the backend's `world_clock` registry.
    overworld: Option<i32>,
    game_time: i64,
    day_time: i64,
    advances: bool,
}

impl HasShared for Down {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

struct Ids {
    attack: i32,
    spectator_action: i32,
}

impl Down {
    fn new() -> Self {
        Self {
            shared: Shared::new(OLDER, NEWER),
            game_mode: 0,
            interacted_at: None,
            clock: Clock::default(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .tap(
                    Clientbound,
                    Configuration,
                    "registry_data",
                    Self::registry_data,
                )
                .tap(Clientbound, Play, "login", Self::login)
                .tap(Clientbound, Play, "respawn", Self::respawn)
                .tap(Clientbound, Play, "game_event", Self::game_event)
                .clientbound(Play, "set_time", Self::set_time)
                .drop_clientbound(Play, "game_rule_values")
                .drop_clientbound(Play, "low_disk_space_warning")
                .serverbound(Play, "interact", Self::interact)
                .build()
        })
    }

    fn ids() -> &'static Ids {
        static IDS: OnceLock<Ids> = OnceLock::new();
        IDS.get_or_init(|| {
            let backend = Self::routes().backend();
            let id = |name| backend.expect_packet(State::Play, Direction::Serverbound, name);
            Ids {
                attack: id("attack"),
                spectator_action: id("spectator_action"),
            }
        })
    }

    /// Learn which clock is the overworld's. Only an observer: the
    /// registries codec checks the packet itself.
    // INTEGRATION: `codec::registries::id_of(shared, Side::Backend,
    // "world_clock", "overworld")` answers this once that codec is merged.
    fn registry_data(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        if let Ok(Some(index)) = registry_entry(&packet.data, WORLD_CLOCK_REGISTRY, OVERWORLD_CLOCK)
        {
            self.clock.overworld = Some(index);
        }
        Ok(())
    }

    fn login(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.game_mode = join::Login::parse(NEWER, &packet.data)?.spawn.game_mode;
        Ok(())
    }

    fn respawn(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        self.game_mode = join::Respawn::parse(NEWER, &packet.data)?.spawn.game_mode;
        Ok(())
    }

    fn game_event(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        if let Some(mode) = join::game_event_mode(&packet.data) {
            self.game_mode = mode;
        }
        Ok(())
    }

    /// The overworld clock becomes the day time. 26.1 sends clocks only when
    /// they change; in between the day time follows the game time while
    /// the clock runs, as the 1.21.11 client would advance it itself.
    fn set_time(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let game_time = r.i64()?;
        let count = r.count()?;
        let overworld = self.clock.overworld.unwrap_or(0);
        let mut found = None;
        for _ in 0..count {
            let clock = r.varint()?;
            let total_ticks = r.varlong()?;
            let _partial_tick = r.f32()?;
            let rate = r.f32()?;
            if clock == overworld {
                found = Some((total_ticks, rate != 0.0));
            }
        }
        r.finish()?;
        let clock = &mut self.clock;
        match found {
            Some((day_time, advances)) => {
                clock.day_time = day_time;
                clock.advances = advances;
            }
            None if clock.advances => {
                clock.day_time = clock
                    .day_time
                    .wrapping_add(game_time.wrapping_sub(clock.game_time));
            }
            None => {}
        }
        clock.game_time = game_time;
        let mut w = Writer::with_capacity(17);
        w.i64(game_time).i64(clock.day_time).bool(clock.advances);
        cx.to_client(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

    /// 1.21.11's three interact actions become 26.1's three packets. A
    /// vanilla client sends `interact_at` first and the plain `interact` only
    /// when that did not succeed on its side; 26.1 servers try both from the
    /// one packet, so the follow-up is dropped.
    fn interact(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let action = r.varint()?;
        let mut w = Writer::new();
        let id = match action {
            INTERACT => {
                let hand = join::hand(r.varint()?)?;
                let secondary = r.bool()?;
                r.finish()?;
                if self.interacted_at.take() == Some((entity, hand)) {
                    return Ok(());
                }
                w.varint(entity).varint(hand);
                join::write_lp_vec3(&mut w, [0.0; 3]);
                w.bool(secondary);
                packet.id
            }
            ATTACK => {
                r.bool()?;
                r.finish()?;
                w.varint(entity);
                if self.game_mode == join::SPECTATOR {
                    Self::ids().spectator_action
                } else {
                    Self::ids().attack
                }
            }
            INTERACT_AT => {
                let location = [r.f32()?, r.f32()?, r.f32()?].map(f64::from);
                let hand = join::hand(r.varint()?)?;
                let secondary = r.bool()?;
                r.finish()?;
                self.interacted_at = Some((entity, hand));
                w.varint(entity).varint(hand);
                join::write_lp_vec3(&mut w, location);
                w.bool(secondary);
                packet.id
            }
            _ => return Err(invalid("invalid interact action")),
        };
        cx.to_backend(state, Packet::new(id, w.into_inner()));
        Ok(())
    }
}
routed_step!(Down);

/// The index of `entry` in a 1.20.5+ `registry_data` body for `registry`;
/// `None` when the packet is another registry's or lacks the entry.
pub(super) fn registry_entry(data: &[u8], registry: &str, entry: &str) -> io::Result<Option<i32>> {
    let mut r = Reader::new(data);
    if r.str()? != registry {
        return Ok(None);
    }
    for index in 0..r.count()? {
        let name = r.str()?;
        if r.bool()? {
            r.nbt_bytes(false)?;
        }
        if name == entry {
            return Ok(Some(index as i32));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_packet_has_a_route() {
        assert_eq!(super::Down::routes().problems(), &[] as &[String]);
    }
}
