//! A 26.1 client on a 1.21.11 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
use super::{ATTACK, INTERACT, INTERACT_AT, NEWER, OLDER, REQUEST_GAME_RULE_VALUES, join};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Writer},
};
use std::{io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

pub struct Up {
    shared: Shared,
    /// Whether the player holds sneak, for the secondary-action flag 1.21.11
    /// attacks carry and 26.1's `attack` lacks.
    sneaking: bool,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

/// Backend-side IDs of packets sent under another name.
struct Ids {
    interact: i32,
    chat_command: i32,
}

impl Up {
    fn new() -> Self {
        Self {
            shared: Shared::new(NEWER, OLDER),
            sneaking: false,
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::*;
            RouteBuilder::new(NEWER, OLDER)
                .defaults()
                .clientbound(Play, "set_time", Self::set_time)
                .tap(Clientbound, Play, "respawn", Self::respawned)
                .tap(Serverbound, Play, "player_input", Self::player_input)
                .serverbound(Play, "interact", Self::interact)
                .serverbound(Play, "attack", Self::attack)
                .serverbound(Play, "spectator_action", Self::attack)
                .serverbound(Play, "set_game_rule", Self::set_game_rule)
                .serverbound(Play, "client_command", Self::client_command)
                .build()
        })
    }

    fn ids() -> &'static Ids {
        static IDS: OnceLock<Ids> = OnceLock::new();
        IDS.get_or_init(|| {
            let backend = Self::routes().backend();
            let id = |name| backend.expect_packet(State::Play, Direction::Serverbound, name);
            Ids {
                interact: id("interact"),
                chat_command: id("chat_command"),
            }
        })
    }

    /// One day time and a flag become the overworld and end clocks, the two
    /// clocks a 1.21.11 world implies.
    fn set_time(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let game_time = r.i64()?;
        let day_time = r.i64()?;
        let advances = r.bool()?;
        r.finish()?;
        let clocks = world_clocks(&mut self.shared);
        let mut w = Writer::with_capacity(9 + clocks.len() * 20);
        w.i64(game_time).varint(clocks.len() as i32);
        for clock in clocks {
            w.varint(clock)
                .varlong(day_time)
                .f32(0.0)
                .f32(if advances { 1.0 } else { 0.0 });
        }
        cx.to_client(state, Packet::new(packet.id, w.into_inner()));
        Ok(())
    }

    fn respawned(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.sneaking = false;
        Ok(())
    }

    fn player_input(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        if let [flags] = packet.data[..] {
            self.sneaking = flags & 0x20 != 0;
        }
        Ok(())
    }

    /// 26.1's one interaction becomes what a 1.21.11 client sends for it:
    /// `interact_at` with the hit location, then the plain `interact`.
    fn interact(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        let hand = join::hand(r.varint()?)?;
        let [x, y, z] = join::read_lp_vec3(&mut r)?;
        let secondary = r.bool()?;
        r.finish()?;
        self.sneaking = secondary;
        let mut at = Writer::new();
        at.varint(entity)
            .varint(INTERACT_AT)
            .f32(x as f32)
            .f32(y as f32)
            .f32(z as f32)
            .varint(hand)
            .bool(secondary);
        let mut plain = Writer::new();
        plain
            .varint(entity)
            .varint(INTERACT)
            .varint(hand)
            .bool(secondary);
        cx.to_backend(state, Packet::new(packet.id, at.into_inner()));
        cx.to_backend(state, Packet::new(packet.id, plain.into_inner()));
        Ok(())
    }

    /// `attack` and `spectator_action` (an entity ID each) are both an
    /// attack `interact` in 1.21.11; the server tells them apart by the
    /// player's game mode.
    fn attack(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let entity = r.varint()?;
        r.finish()?;
        let mut w = Writer::new();
        w.varint(entity).varint(ATTACK).bool(self.sneaking);
        cx.to_backend(state, Packet::new(Self::ids().interact, w.into_inner()));
        Ok(())
    }

    /// The game rule screen's changes become `/gamerule` commands, which a
    /// 1.21.11 server accepts by full identifier with the same permission.
    fn set_game_rule(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let mut commands = Vec::new();
        for _ in 0..r.count()? {
            let rule = r.str()?;
            let value = r.str()?;
            let plain = |text: &str, extra: &str| {
                !text.is_empty()
                    && text
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
            };
            // Anything else could not have come from a real game rule.
            if plain(rule, "_-.:/") && plain(value, "_-.+") {
                commands.push(format!("gamerule {rule} {value}"));
            }
        }
        r.finish()?;
        for command in commands {
            let mut w = Writer::new();
            w.string(&command);
            cx.to_backend(state, Packet::new(Self::ids().chat_command, w.into_inner()));
        }
        Ok(())
    }

    /// 1.21.11 has no game rule values to send back: drop that request.
    fn client_command(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut r = Reader::new(&packet.data);
        let action = r.varint()?;
        r.finish()?;
        if action != REQUEST_GAME_RULE_VALUES {
            cx.to_backend(state, packet);
        }
        Ok(())
    }
}
routed_step!(Up);

/// The client's `world_clock` network IDs of the overworld and end clocks.
// INTEGRATION: the registries codec fills in the `world_clock` registry a
// 26.1 client needs from a 1.21.11 backend; take the IDs it gave the client
// from `codec::registries::id_of(shared, Side::Client, "world_clock",
// "overworld")` (and `"the_end"`). Until then: vanilla's order.
fn world_clocks(shared: &mut Shared) -> [i32; 2] {
    let _ = shared;
    [0, 1]
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_packet_has_a_route() {
        assert_eq!(super::Up::routes().problems(), &[] as &[String]);
    }
}
