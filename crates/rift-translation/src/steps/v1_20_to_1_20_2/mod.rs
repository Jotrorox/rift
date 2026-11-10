//! Minecraft 1.20/1.20.1 (protocol 763) and 1.20.2 (protocol 764): the pair
//! where the configuration phase appears.
//!
//! A 1.20.1 server goes from login straight to play; a 1.20.2 one runs a
//! configuration phase in between (registries, feature flags, tags, resource
//! packs) and may return to it from play. Each step therefore runs the phase
//! one side lacks on that side's behalf:
//!
//! - [`up`] (a 1.20.2 client on a 1.20.1 backend) holds back the backend's
//!   play packets while it feeds the client a configuration built from the
//!   backend's Join Game, and re-enters configuration when the backend sends
//!   another Join Game.
//! - [`down`] (a 1.20.1 client on a 1.20.2 backend) answers the backend's
//!   configuration itself and folds what it learnt into the Join Game the
//!   old client needs.
//!
//! Besides phases, 1.20.2 made the login UUID mandatory, introduced chunk
//! batches and play-phase pings, merged Spawn Player into Add Entity, made
//! network NBT roots nameless, moved Respawn's kept-data flags, widened the
//! display-objective slot to a VarInt and renumbered mob effects. Shared
//! codecs handle the rest of what changed (chunks, entity data, items,
//! advancements, recipes, tags); see `crate::codec`.
mod down;
mod layout;
#[cfg(test)]
mod tests;
mod up;

use super::Make;
use crate::{
    Direction, Packet, State,
    step::{Cx, Emitted, Routes},
    wire::invalid,
};
use std::{collections::VecDeque, io};

pub const OLDER: i32 = 763;
pub const NEWER: i32 = 764;

/// A 1.20.2 client on a 1.20 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.20 client on a 1.20.2 backend.
pub const DOWN: Option<Make> = down::STEP;

/// Run `packet` through a step's routes and collect what comes out instead
/// of sending it: for packets that leave in another phase than the routes
/// produce them in.
fn convert<S>(
    routes: &Routes<S>,
    step: &mut S,
    cx: &Cx<'_>,
    direction: Direction,
    state: State,
    packet: Packet,
) -> io::Result<VecDeque<Emitted>> {
    let mut emitted = VecDeque::new();
    let mut scratch = Cx {
        emitted: &mut emitted,
        client_protocol: cx.client_protocol,
        backend_protocol: cx.backend_protocol,
    };
    routes.dispatch(step, &mut scratch, direction, state, packet)?;
    Ok(emitted)
}

/// Packets held back while the receiver is in another phase, bounded so a
/// peer that never finishes the phase cannot exhaust memory.
#[derive(Default)]
struct Held {
    packets: VecDeque<Packet>,
    bytes: usize,
}

impl Held {
    /// More than a full join at a large view distance.
    const LIMIT: usize = 64 << 20;

    fn push(&mut self, packet: Packet) -> io::Result<()> {
        self.bytes += packet.data.len() + 8;
        if self.bytes > Self::LIMIT {
            return Err(invalid(
                "too many packets held back during the configuration phase",
            ));
        }
        self.packets.push_back(packet);
        Ok(())
    }

    fn take(&mut self) -> VecDeque<Packet> {
        self.bytes = 0;
        std::mem::take(&mut self.packets)
    }
}
