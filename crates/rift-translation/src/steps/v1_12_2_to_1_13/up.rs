//! A 1.13 client on a 1.12.2 backend: backend packets are upgraded for the
//! client and client packets downgraded for the backend.
//!
//! The shared codecs flatten blocks and items, entity data, text and the
//! other structures both versions have. This step adds what only this pair
//! needs:
//!
//! - what the 1.13 client requires and a 1.12 server never sends: a command
//!   tree, the vanilla tags and the recipe list, right after `login`;
//! - plugin channels, which became namespaced identifiers, and the
//!   pre-1.13 `MC|...` channels that became real packets (both ways);
//! - tab completion, recipe IDs (numbers in 1.12, names in 1.13) and the
//!   recipe book;
//! - block states that 1.12 kept in block entities or left for its client to
//!   compute (bed colours, banners, skulls, flower pots, note blocks; fence,
//!   pane, wall, stairs, redstone... connections), on top of the chunk
//!   codec's flattened blocks;
//! - the 1.13 effect icon flag.
mod channels;
mod commands;
mod packets;
mod recipes;
mod serverbound;
mod tags;
#[cfg(test)]
mod tests;
pub mod translations;
pub mod world;

use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    step::{Cx, RouteBuilder, Routes, Step},
    steps::Make,
    wire::{Reader, Writer},
};
use std::{collections::VecDeque, io, sync::OnceLock};

pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));

/// The step's state for one connection.
pub struct Up {
    shared: Shared,
    /// Plugin channel names the backend used that 1.13 spells differently.
    channels: channels::Names,
    /// Tab completion requests awaiting the backend's answer, oldest first:
    /// transaction ID and the text the client sent.
    suggestions: VecDeque<(i32, String)>,
    /// The dimension the player is in: -1, 0 or 1.
    dimension: i32,
    world: world::World,
}

impl HasShared for Up {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

impl Up {
    pub fn new() -> Self {
        Self {
            shared: Shared::new(super::NEWER, super::OLDER),
            channels: channels::Names::default(),
            suggestions: VecDeque::new(),
            dimension: 0,
            world: world::World::default(),
        }
    }

    pub fn routes() -> &'static Routes<Self> {
        static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
        ROUTES.get_or_init(|| {
            let builder = RouteBuilder::new(super::NEWER, super::OLDER)
                .defaults()
                .clientbound(State::Play, "login", Self::login)
                .tap(
                    Direction::Clientbound,
                    State::Play,
                    "respawn",
                    Self::respawn,
                )
                .clientbound(State::Play, "update_mob_effect", Self::mob_effect)
                .drop_serverbound(State::Login, "custom_query_answer");
            let builder = channels::install(builder);
            let builder = commands::install(builder);
            let builder = recipes::install(builder);
            let builder = serverbound::install(builder);
            world::install(builder).build()
        })
    }

    /// Join game: the same layout in both versions. The 1.13 client also
    /// needs the recipes, tags and command tree a 1.13 server sends next.
    fn login(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        reader.i32()?; // Entity ID.
        reader.u8()?; // Game mode.
        let dimension = reader.i32()?;
        reader.u8()?; // Difficulty.
        reader.u8()?; // Maximum players.
        reader.string(16)?; // Level type.
        reader.bool()?; // Reduced debug info.
        reader.finish()?;
        // A new connection's world, whatever the dimension.
        self.world.clear();
        self.enter(dimension);
        cx.to_client(state, packet);
        let ids = packets::ids();
        cx.to_client(state, Packet::new(ids.update_recipes, recipes::declare()?));
        cx.to_client(state, Packet::new(ids.update_tags, tags::packet()?));
        cx.to_client(state, Packet::new(ids.commands, commands::tree()));
        Ok(())
    }

    fn respawn(&mut self, _: State, packet: &Packet) -> io::Result<()> {
        let dimension = Reader::new(&packet.data).i32()?;
        self.enter(dimension);
        Ok(())
    }

    /// Clients keep their world when respawning into the same dimension and
    /// replace it otherwise; what the step knows about blocks follows.
    fn enter(&mut self, dimension: i32) {
        // Only the three vanilla dimensions exist before 1.16; clients treat
        // anything else as the overworld.
        let dimension = if matches!(dimension, -1..=1) {
            dimension
        } else {
            0
        };
        if dimension != self.dimension {
            self.world.clear();
        }
        self.dimension = dimension;
    }

    /// 1.13 added the "show icon" flag; vanilla 1.13 shows the icon of every
    /// effect that shows particles, which is what 1.12 servers mean.
    fn mob_effect(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let mut reader = Reader::new(&packet.data);
        let entity = reader.varint()?;
        let effect = reader.i8()?;
        let amplifier = reader.i8()?;
        let duration = reader.varint()?;
        let flags = reader.u8()?;
        reader.finish()?;
        let mapping = self.shared.mapping(Direction::Clientbound, "mob_effect");
        let Some(effect) = mapping.get(i32::from(effect as u8)) else {
            return Ok(());
        };
        let mut writer = Writer::with_capacity(packet.data.len());
        writer
            .varint(entity)
            .i8(effect as i8)
            .i8(amplifier)
            .varint(duration)
            .u8(if flags & 0x02 != 0 {
                flags | 0x04
            } else {
                flags
            });
        cx.to_client(state, Packet::new(packet.id, writer.buf));
        Ok(())
    }
}

impl Default for Up {
    fn default() -> Self {
        Self::new()
    }
}

impl Step for Up {
    fn clientbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let start = cx.emitted.len();
        Self::routes().dispatch(self, cx, Direction::Clientbound, state, packet)?;
        // The codecs have flattened what they own; block states that need
        // block entity data or neighbours are completed here.
        world::complete(self, cx, start)
    }

    fn serverbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        Self::routes().dispatch(self, cx, Direction::Serverbound, state, packet)
    }
}
