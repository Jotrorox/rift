//! Codecs for what many packets and versions share: item stacks, entity
//! data, particles, chunks, text components, tags, commands and synchronised
//! registries.
//!
//! Each domain module knows its structures and packets in every protocol from
//! 1.12 to 26.3 and installs default routes that convert them between the
//! two protocols of a step, layout and registry IDs alike. A step that calls
//! [`RouteBuilder::defaults`] therefore only has to handle what is specific
//! to its pair: packets that appear, disappear or change meaning. A step's
//! own rule for a packet replaces the default.
//!
//! Which module owns which packet (by its newest Mojang name; older names
//! of the same packet are covered too):
//!
//! - [`item`]: `container_set_content`, `container_set_slot`,
//!   `set_equipment`, `merchant_offers`, `update_advancements`,
//!   `update_recipes`, `recipe_book_add`, `recipe_book_remove`,
//!   `recipe_book_settings`, `recipe`, `place_ghost_recipe`,
//!   `set_cursor_item`, `set_player_inventory`, `cooldown`; serverbound
//!   `container_click`, `set_creative_mode_slot`, `edit_book`.
//! - [`entity`]: `add_entity` and the older spawn packets, `set_entity_data`,
//!   `remove_entities`, `level_particles`, `update_mob_effect`,
//!   `remove_mob_effect`, `update_attributes`, `damage_event`.
//! - [`chunk`]: `level_chunk_with_light` and the older chunk packets,
//!   `light_update`, `forget_level_chunk`, `block_update`,
//!   `section_blocks_update`, `block_entity_data`, `block_event`,
//!   `level_event`, `explode`, `chunks_biomes`.
//! - [`text`]: `system_chat`, `player_chat`, `disguised_chat` and the older
//!   chat packet, `disconnect` (configuration and play), the title packets,
//!   `boss_event`, `tab_list`, `player_info_update` and the older player
//!   list packet, `set_objective`, `set_score`, `reset_score`,
//!   `set_player_team`, `player_combat_kill`, `resource_pack_push`,
//!   `server_data`, `map_item_data`, `command_suggestions`.
//! - [`misc`]: `update_tags`, `award_stats`, `commands`, `sound`,
//!   `sound_entity`, `stop_sound`, `open_screen`.
//! - [`registries`]: `registry_data`, and the registry parts of `login` and
//!   `respawn` for the steps that own those packets.
//!
//! Every other packet belongs to the step.
pub mod chunk;
pub mod entity;
pub mod item;
pub mod misc;
pub mod registries;
#[cfg(test)]
pub mod testing;
pub mod text;

use crate::{
    Direction,
    data::{self, VersionData},
    mapping::{self, Mapping},
    step::RouteBuilder,
};

/// What the shared codecs keep for one step of one connection.
pub struct Shared {
    /// The step's client-side protocol.
    pub client: i32,
    /// The step's backend-side protocol.
    pub backend: i32,
    /// Registry mappings in use, per direction; few enough for a scan.
    mappings: [Vec<(&'static str, &'static Mapping)>; 2],
    pub item: item::State,
    pub entity: entity::State,
    pub chunk: chunk::State,
    pub text: text::State,
    pub misc: misc::State,
    pub registries: registries::State,
}

impl Shared {
    /// For a step between the adjacent protocols `client` and `backend`.
    pub fn new(client: i32, backend: i32) -> Self {
        Self {
            client,
            backend,
            mappings: Default::default(),
            item: Default::default(),
            entity: Default::default(),
            chunk: Default::default(),
            text: Default::default(),
            misc: Default::default(),
            registries: Default::default(),
        }
    }

    /// The protocol packets travelling in `direction` arrive in.
    pub fn source(&self, direction: Direction) -> i32 {
        match direction {
            Direction::Clientbound => self.backend,
            Direction::Serverbound => self.client,
        }
    }

    /// The protocol packets travelling in `direction` leave in.
    pub fn target(&self, direction: Direction) -> i32 {
        match direction {
            Direction::Clientbound => self.client,
            Direction::Serverbound => self.backend,
        }
    }

    pub fn source_data(&self, direction: Direction) -> &'static VersionData {
        data::version(self.source(direction))
    }

    pub fn target_data(&self, direction: Direction) -> &'static VersionData {
        data::version(self.target(direction))
    }

    /// The IDs of `registry` (such as `"item"` or `"block_state"`) for
    /// packets travelling in `direction`: sender's ID to receiver's ID.
    /// Hold on to the result when mapping many IDs.
    pub fn mapping(&mut self, direction: Direction, registry: &'static str) -> &'static Mapping {
        let cache = &mut self.mappings[direction as usize];
        if let Some((_, mapping)) = cache.iter().find(|(name, _)| *name == registry) {
            return mapping;
        }
        let (source, target) = match direction {
            Direction::Clientbound => (self.backend, self.client),
            Direction::Serverbound => (self.client, self.backend),
        };
        let mapping = mapping::mapping(source, target, registry);
        cache.push((registry, mapping));
        mapping
    }
}

/// A step that uses the shared codecs keeps a [`Shared`] and hands it out.
pub trait HasShared {
    fn shared(&mut self) -> &mut Shared;
}

impl<S: HasShared + 'static> RouteBuilder<S> {
    /// Install every shared codec's default routes for this pair.
    pub fn defaults(self) -> Self {
        let builder = registries::install(self);
        let builder = misc::install(builder);
        let builder = text::install(builder);
        let builder = chunk::install(builder);
        let builder = entity::install(builder);
        item::install(builder)
    }
}
