//! A 1.12.2 client on a 1.13 backend: backend packets are downgraded for the
//! client and client packets upgraded for the backend.
//!
//! The shared codecs convert items, blocks, entities, text and the other
//! structures both protocols have. This step covers what 1.13 added or
//! turned into packets of its own, and what a 1.12.2 client still does
//! through plugin channels:
//!
//! - login plugin requests, which the client cannot see, are answered on its
//!   behalf as not understood, as a 1.13 client answers unknown channels;
//! - tab completion: chat completes player names from the tab list locally,
//!   commands are forwarded as 1.13 suggestion requests and the answer is
//!   rewritten into the replacements of the last word a 1.12 client expects;
//!   the command tree itself is dropped;
//! - plugin channels are renamed both ways (`minecraft:brand` and `MC|Brand`
//!   and so on); the `MC|*` channels the 1.12 client sends for books, anvils,
//!   trades, beacons, command and structure blocks and pick block become the
//!   1.13 packets, and `stop_sound` becomes `MC|StopSound`;
//! - the recipe book: 1.13 recipe names become 1.12's numeric recipe IDs and
//!   back (`down/recipes.rs`); tags and recipe definitions are dropped;
//! - `player_look_at` is emulated with a rotation-only teleport, and the
//!   pufferfish sting game event with the sound it plays;
//! - block entity and entity NBT queries, which a 1.12 client never makes,
//!   have their answers dropped;
//! - named sounds take 1.12 names (`custom_sound`).
//!
//! [`translations`] (1.13 translation keys a 1.12.2 client lacks) is
//! provided for the text codec. The block entity data 1.13 moved into block
//! states (bed colours, banner bases, skull kinds, potted plants) is the
//! chunk codec's (`codec::chunk::flattening`).
mod channels;
mod recipes;
mod suggestions;
#[cfg(test)]
mod tests;
pub(super) mod translations;

use super::{NEWER, OLDER};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    routed_step,
    step::{Cx, RouteBuilder, Routes},
    steps::Make,
    wire::{Reader, Writer, invalid},
};
use std::{
    collections::{HashMap, VecDeque},
    io,
    sync::OnceLock,
};

pub const STEP: Option<Make> = Some(|| Box::new(Down::new()));

/// The 1.13 game event a pufferfish's sting sends the stung player; 1.12
/// has no such event.
const PUFFER_FISH_STING: u8 = 9;

/// Rotation-only teleports this step sends carry IDs below zero, which a
/// 1.13 server never uses (it counts up from zero and wraps to zero).
const FIRST_OWN_TELEPORT: i32 = -1;

/// Unconfirmed rotation-only teleports kept; a client that never confirms
/// cannot grow the list further.
const MAX_OWN_TELEPORTS: usize = 64;

/// Where the player is, as far as this step has seen.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Position {
    x: f64,
    /// The feet.
    y: f64,
    z: f64,
}

pub struct Down {
    shared: Shared,
    /// Player names in the tab list, for completing chat.
    players: HashMap<u128, String>,
    /// The forwarded command completion still waiting for its answer.
    suggestion: Option<suggestions::Pending>,
    next_suggestion: i32,
    /// `None` until the backend first places the player, and again after a
    /// respawn.
    position: Option<Position>,
    /// IDs of the rotation-only teleports the client has yet to confirm.
    own_teleports: VecDeque<i32>,
    next_teleport: i32,
    /// 1.12 channel names by the 1.13 identifier they were sent on, so the
    /// backend's answers go back on the name the client knows.
    channels: HashMap<String, String>,
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
            players: HashMap::new(),
            suggestion: None,
            next_suggestion: 0,
            position: None,
            own_teleports: VecDeque::new(),
            next_teleport: FIRST_OWN_TELEPORT,
            channels: HashMap::new(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Down>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            use Direction::*;
            use State::*;
            RouteBuilder::new(OLDER, NEWER)
                .defaults()
                .clientbound(Login, "custom_query", Self::custom_query)
                // 1.12 completes against the server; the tree is only for 1.13.
                .drop_clientbound(Play, "commands")
                .clientbound(Play, "command_suggestions", Self::command_suggestions)
                .serverbound(Play, "command_suggestion", Self::command_suggestion)
                .clientbound(Play, "custom_payload", Self::clientbound_payload)
                .serverbound(Play, "custom_payload", Self::serverbound_payload)
                .clientbound(Play, "stop_sound", Self::stop_sound)
                .clientbound(Play, "custom_sound", Self::custom_sound)
                .clientbound(Play, "recipe", Self::recipe)
                .clientbound(Play, "place_ghost_recipe", Self::place_ghost_recipe)
                .serverbound(Play, "place_recipe", Self::place_recipe)
                .serverbound(Play, "recipe_book_update", Self::recipe_book_update)
                .drop_clientbound(Play, "update_recipes")
                .drop_clientbound(Play, "update_tags")
                // The answer to a block entity or entity NBT query, which only
                // a 1.13 client's debug keys make.
                .drop_clientbound(Play, "tag_query")
                .clientbound(Play, "player_look_at", Self::player_look_at)
                .clientbound(Play, "game_event", Self::game_event)
                .serverbound(Play, "accept_teleportation", Self::accept_teleportation)
                .tap(Clientbound, Play, "login", Self::forget_position)
                .tap(Clientbound, Play, "respawn", Self::forget_position)
                .tap(Clientbound, Play, "player_position", Self::follow_teleport)
                .tap(Serverbound, Play, "move_player_pos", Self::follow_move)
                .tap(Serverbound, Play, "move_player_pos_rot", Self::follow_move)
                .tap(Clientbound, Play, "player_info", Self::follow_players)
                .build()
        })
    }

    /// The ID of the client-side packet `name`.
    fn client_id(state: State, direction: Direction, name: &str) -> i32 {
        Self::routes()
            .client()
            .expect_packet(state, direction, name)
    }

    /// The ID of the backend-side packet `name`.
    fn backend_id(state: State, direction: Direction, name: &str) -> i32 {
        Self::routes()
            .backend()
            .expect_packet(state, direction, name)
    }

    /// A 1.12 client cannot receive login plugin requests; answer that the
    /// channel is not understood, which is what a 1.13 client says for any
    /// channel it does not know.
    fn custom_query(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let id = reader.varint()?;
        reader.string(32767)?;
        reader.rest();
        let mut answer = Writer::new();
        answer.varint(id).bool(false);
        cx.to_backend(
            state,
            Packet::new(
                Self::backend_id(State::Login, Direction::Serverbound, "custom_query_answer"),
                answer.into_inner(),
            ),
        );
        Ok(())
    }

    /// 1.13 named sounds use 1.13 sound names; ones 1.12 calls differently or
    /// lacks get its name or stand-in. Names outside the game's registry
    /// (resource pack sounds) pass unchanged.
    fn custom_sound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let name = reader.string(32767)?;
        let category = reader.varint()?;
        let position = reader.take(4 * 3)?;
        let (volume, pitch) = (reader.f32()?, reader.f32()?);
        reader.finish()?;
        let mapped = self.sound_name(name);
        let mut out = Writer::new();
        out.string(mapped.as_deref().unwrap_or(name))
            .varint(category)
            .bytes(position)
            .f32(volume)
            .f32(pitch);
        cx.to_client(state, Packet::new(packet.id, out.into_inner()));
        Ok(())
    }

    /// The 1.12 name of the 1.13 registry sound `name`, keeping its
    /// namespace; `None` when the name is not a 1.13 registry sound or has
    /// no 1.12 counterpart.
    fn sound_name(&mut self, name: &str) -> Option<String> {
        let path = name.strip_prefix("minecraft:").unwrap_or(name);
        if path.contains(':') {
            return None;
        }
        let id = Self::routes().backend().registry("sound_event")?.id(path)?;
        let mapped = self
            .shared
            .mapping(Direction::Clientbound, "sound_event")
            .get(id)?;
        let mapped = Self::routes()
            .client()
            .registry("sound_event")?
            .name(mapped)?;
        Some(if name.starts_with("minecraft:") {
            format!("minecraft:{mapped}")
        } else {
            mapped.to_owned()
        })
    }

    /// Weather, game mode and the other events pass; the pufferfish sting,
    /// new in 1.13, is played as the sound the 1.13 client would play.
    fn game_event(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let reason = reader.u8()?;
        reader.f32()?;
        reader.finish()?;
        if reason != PUFFER_FISH_STING {
            cx.to_client(state, packet);
            return Ok(());
        }
        let Some(position) = self.position else {
            return Ok(());
        };
        let Some(sting) = Self::routes()
            .backend()
            .registry("sound_event")
            .and_then(|sounds| sounds.id("entity.puffer_fish.sting"))
        else {
            return Ok(());
        };
        let Some(sound) = self
            .shared
            .mapping(Direction::Clientbound, "sound_event")
            .get(sting)
        else {
            return Ok(());
        };
        /// `SoundCategory.NEUTRAL`, which the 1.13 client plays the sting in.
        const NEUTRAL: i32 = 6;
        let fixed = |value: f64| (value * 8.0) as i32;
        let mut out = Writer::new();
        out.varint(sound)
            .varint(NEUTRAL)
            .i32(fixed(position.x))
            .i32(fixed(position.y))
            .i32(fixed(position.z))
            .f32(1.0)
            .f32(1.0);
        cx.to_client(
            state,
            Packet::new(
                Self::client_id(State::Play, Direction::Clientbound, "sound"),
                out.into_inner(),
            ),
        );
        Ok(())
    }

    /// Turn the player towards a point (1.13 commands such as `/teleport
    /// ... facing`) with a teleport that keeps the position and sets only
    /// the rotation. The client's confirmation of it is not passed on.
    fn player_look_at(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let anchor = reader.varint()?;
        let (x, y, z) = (reader.f64()?, reader.f64()?, reader.f64()?);
        if reader.bool()? {
            // The target entity and its anchor; x, y and z already hold where
            // the server saw it.
            reader.varint()?;
            reader.varint()?;
        }
        reader.finish()?;
        if !(0..=1).contains(&anchor) {
            return Err(invalid("invalid look-at anchor"));
        }
        let Some(position) = self.position else {
            return Ok(());
        };
        /// A standing player's eye height in 1.12 and 1.13.
        const EYES: f64 = 1.62;
        let eyes = position.y + if anchor == 1 { EYES } else { 0.0 };
        let (dx, dy, dz) = (x - position.x, y - eyes, z - position.z);
        let horizontal = (dx * dx + dz * dz).sqrt();
        let pitch = -dy.atan2(horizontal).to_degrees();
        let yaw = dz.atan2(dx).to_degrees() - 90.0;
        if !(yaw.is_finite() && pitch.is_finite()) {
            return Ok(());
        }
        /// Relative x, y and z with zero offsets: the position stays.
        const KEEP_POSITION: u8 = 0x01 | 0x02 | 0x04;
        let id = self.next_teleport;
        self.next_teleport = self
            .next_teleport
            .checked_sub(1)
            .unwrap_or(FIRST_OWN_TELEPORT);
        if self.own_teleports.len() == MAX_OWN_TELEPORTS {
            self.own_teleports.pop_front();
        }
        self.own_teleports.push_back(id);
        let mut out = Writer::new();
        out.f64(0.0)
            .f64(0.0)
            .f64(0.0)
            .f32(yaw as f32)
            .f32(pitch as f32)
            .u8(KEEP_POSITION)
            .varint(id);
        cx.to_client(
            state,
            Packet::new(
                Self::client_id(State::Play, Direction::Clientbound, "player_position"),
                out.into_inner(),
            ),
        );
        Ok(())
    }

    fn accept_teleportation(
        &mut self,
        cx: &mut Cx<'_>,
        state: State,
        packet: Packet,
    ) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let id = reader.varint()?;
        reader.finish()?;
        if let Some(index) = self.own_teleports.iter().position(|own| *own == id) {
            self.own_teleports.remove(index);
            return Ok(());
        }
        cx.to_backend(state, packet);
        Ok(())
    }

    fn forget_position(&mut self, _: State, _: &Packet) -> io::Result<()> {
        self.position = None;
        self.own_teleports.clear();
        Ok(())
    }

    /// Follow where the backend puts the player. The layout is the same in
    /// 1.12.2 and 1.13.
    fn follow_teleport(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let (x, y, z) = (reader.f64()?, reader.f64()?, reader.f64()?);
        reader.f32()?;
        reader.f32()?;
        let flags = reader.u8()?;
        reader.varint()?;
        reader.finish()?;
        let relative = |bit: u8, value: f64, old: Option<f64>| {
            if flags & bit == 0 {
                Some(value)
            } else {
                old.map(|old| old + value)
            }
        };
        let old = self.position;
        self.position = match (
            relative(0x01, x, old.map(|old| old.x)),
            relative(0x02, y, old.map(|old| old.y)),
            relative(0x04, z, old.map(|old| old.z)),
        ) {
            (Some(x), Some(y), Some(z)) => Some(Position { x, y, z }),
            _ => None,
        };
        Ok(())
    }

    /// Follow the client's own movement: `move_player_pos` and
    /// `move_player_pos_rot` both start with x, feet y and z and end with
    /// the on-ground flag, with yaw and pitch between in the latter.
    fn follow_move(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let (x, y, z) = (reader.f64()?, reader.f64()?, reader.f64()?);
        if !matches!(reader.remaining(), 1 | 9) {
            return Err(invalid("invalid player movement"));
        }
        self.position = Some(Position { x, y, z });
        Ok(())
    }

    /// Keep the tab list's names for completing chat locally: 1.13 servers
    /// leave that to the client.
    fn follow_players(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let action = reader.varint()?;
        for _ in 0..reader.count()? {
            let uuid = reader.uuid()?;
            match action {
                0 => {
                    let name = reader.string(16)?.to_owned();
                    for _ in 0..reader.count()? {
                        reader.string(32767)?;
                        reader.string(32767)?;
                        if reader.bool()? {
                            reader.string(32767)?;
                        }
                    }
                    reader.varint()?;
                    reader.varint()?;
                    if reader.bool()? {
                        reader.string(262144)?;
                    }
                    self.players.insert(uuid, name);
                }
                1 | 2 => drop(reader.varint()?),
                3 => {
                    if reader.bool()? {
                        reader.string(262144)?;
                    }
                }
                4 => drop(self.players.remove(&uuid)),
                _ => return Err(invalid("invalid player info action")),
            }
        }
        reader.finish()
    }
}

routed_step!(Down);
