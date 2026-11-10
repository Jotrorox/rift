//! Entity data, entity types, particles, and the packets that carry them.
//!
//! The per-protocol tables (entity types, their entity data fields, the
//! serializers, particle options) are generated from the server jars by
//! `scripts/translation_entities.py`; see [`tables`]. [`State`] follows
//! which entity is of which type, fed by the spawn packets, because entity
//! data can only be converted knowing the entity's type.
pub mod common;
pub mod metadata;
mod packets;
pub mod particle;
mod spawn;
pub mod tables;
#[cfg(test)]
mod tests;

use super::{HasShared, Shared};
use crate::{
    Direction, Packet, State as Phase, data,
    step::{Cx, RouteBuilder},
    wire::Reader,
};
use std::{collections::HashMap, io};

/// An entity the backend spawned, by its type name on both sides of the
/// step (the client side's is a stand-in when the client lacks the type).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tracked {
    pub backend: &'static str,
    pub client: &'static str,
}

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {
    entities: HashMap<i32, Tracked>,
    /// The player's own entity, from `login`.
    player: Option<i32>,
    /// Paintings sent with `add_painting` from an `add_entity` (1.19+)
    /// whose variant arrives later in entity data: block and 3D facing.
    paintings: HashMap<i32, (crate::wire::Position, i32)>,
}

impl State {
    /// The type of entity `id` as the backend side of the step knows it.
    pub fn backend_type(&self, id: i32) -> Option<&'static str> {
        self.entities.get(&id).map(|tracked| tracked.backend)
    }

    /// The type of entity `id` as the client side of the step knows it.
    pub fn client_type(&self, id: i32) -> Option<&'static str> {
        self.entities.get(&id).map(|tracked| tracked.client)
    }

    pub fn get(&self, id: i32) -> Option<Tracked> {
        self.entities.get(&id).copied()
    }

    /// The player's own entity ID, once `login` was seen.
    pub fn player(&self) -> Option<i32> {
        self.player
    }

    /// Follow a spawned entity.
    pub fn track(&mut self, id: i32, tracked: Tracked) {
        self.entities.insert(id, tracked);
    }

    pub fn untrack(&mut self, id: i32) {
        self.entities.remove(&id);
        self.paintings.remove(&id);
    }

    /// Forget every entity but the player, as a respawn or world change does.
    fn reset(&mut self) {
        self.entities.clear();
        self.paintings.clear();
        if let Some(player) = self.player {
            self.entities.insert(
                player,
                Tracked {
                    backend: "player",
                    client: "player",
                },
            );
        }
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
}

/// The type name of entity type `id` in `protocol`'s registry.
pub fn type_name(protocol: i32, id: i32) -> Option<&'static str> {
    data::version(protocol)
        .expect_registry("entity_type")
        .name(id)
}

/// The receiver's name for an entity type of the sender's, by the type
/// registry mapping and, failing that, the renames the data files record.
/// `None` when the receiver has no such type and no stand-in.
pub fn map_type(shared: &mut Shared, direction: Direction, name: &str) -> Option<&'static str> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    let target = data::version(to).expect_registry("entity_type");
    if name == "player" {
        return Some("player");
    }
    if from == to {
        return target.id(name).and_then(|id| target.name(id));
    }
    if let Some(id) = data::version(from).expect_registry("entity_type").id(name)
        && let Some(mapped) = shared.mapping(direction, "entity_type").get(id)
    {
        return target.name(mapped);
    }
    // INTEGRATION: the mapping engine's rules should carry these renames;
    // the entity tables record them meanwhile.
    let newer = from.max(to);
    let renamed = &tables::tables(newer).renamed;
    let other = if to == newer {
        renamed
            .iter()
            .find(|(_, old)| *old == name)
            .map(|(new, _)| *new)
    } else {
        renamed
            .iter()
            .find(|(new, _)| *new == name)
            .map(|(_, old)| *old)
    };
    let other = other?;
    target.id(other).and_then(|id| target.name(id))
}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    let (client, backend) = (builder.client().protocol, builder.backend().protocol);
    let builder = builder
        .tap(Direction::Clientbound, Phase::Play, "login", login::<S>)
        .tap(Direction::Clientbound, Phase::Play, "respawn", respawn::<S>);
    let builder = spawn::install(builder, client, backend);
    packets::install(builder, client, backend)
}

/// Whether the step for this pair needs to know entity types: its entity
/// data conversion depends on them.
fn tracks(client: i32, backend: i32) -> bool {
    metadata::differs(backend, client) || metadata::differs(client, backend)
}

/// `login` starts a world: every protocol sends the player's entity ID first.
fn login<S: HasShared>(step: &mut S, state: Phase, packet: &Packet) -> io::Result<()> {
    let _ = state;
    let id = Reader::new(&packet.data).i32()?;
    let entity = &mut step.shared().entity;
    entity.player = Some(id);
    entity.reset();
    Ok(())
}

fn respawn<S: HasShared>(step: &mut S, _: Phase, _: &Packet) -> io::Result<()> {
    step.shared().entity.reset();
    Ok(())
}

/// Send a play packet by name towards the receiver of `direction`.
fn emit(
    cx: &mut Cx<'_>,
    shared: &Shared,
    direction: Direction,
    name: &str,
    body: Vec<u8>,
) -> io::Result<()> {
    let id = shared
        .target_data(direction)
        .packet_id(Phase::Play, direction, name)
        .ok_or_else(|| {
            crate::wire::invalid(&format!(
                "{} has no {name} packet",
                shared.target_data(direction).name
            ))
        })?;
    let packet = Packet::new(id, body);
    match direction {
        Direction::Clientbound => cx.to_client(Phase::Play, packet),
        Direction::Serverbound => cx.to_backend(Phase::Play, packet),
    }
    Ok(())
}
