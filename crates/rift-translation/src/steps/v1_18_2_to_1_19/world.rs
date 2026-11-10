//! The registry codec and dimension of `login` and `respawn` between 1.18.2
//! and 1.19: 1.19 names the dimension type by its registry key instead of
//! sending its element, requires monster spawning fields in dimension types
//! and a chat type registry, and no longer has biome categories.
// INTEGRATION: the registries codec's helpers for the registry parts of
// `login` and `respawn` should replace these conversions once they exist.
use super::{
    chat::{self, ChatTypes, registry_entries, registry_entries_mut},
    join::{Dimension, Login, Respawn, same_compound},
};
use crate::{
    nbt::{Compound, Nbt},
    wire::invalid,
};
use std::io;

const DIMENSION_TYPES: &str = "minecraft:dimension_type";

/// The dimension types of the last `login`, by key, as the backend sent
/// them.
#[derive(Debug, Default)]
pub struct DimensionTypes {
    types: Vec<(String, Compound)>,
}

impl DimensionTypes {
    fn from_codec(codec: &Compound) -> Self {
        let types = registry_entries(codec, DIMENSION_TYPES)
            .unwrap_or_default()
            .iter()
            .filter_map(|entry| {
                let entry = entry.as_compound()?;
                Some((
                    entry.str("name")?.to_owned(),
                    entry.compound("element")?.clone(),
                ))
            })
            .collect();
        Self { types }
    }

    /// The key of a 1.18.2 dimension element: the type with the same
    /// entries, else the one sharing most entries among those of the same
    /// height and minimum Y (a backend that altered a type after `login`).
    fn key(&self, element: &Compound) -> io::Result<String> {
        if let Some((key, _)) = self
            .types
            .iter()
            .find(|(_, known)| same_compound(known, element))
        {
            return Ok(key.clone());
        }
        let shared = |known: &Compound| {
            element
                .iter()
                .filter(|(name, value)| known.get(name) == Some(value))
                .count()
        };
        self.types
            .iter()
            .filter(|(_, known)| {
                ["min_y", "height"]
                    .iter()
                    .all(|name| known.get(name) == element.get(name))
            })
            .max_by_key(|(_, known)| shared(known))
            .map(|(key, _)| key.clone())
            .ok_or_else(|| invalid("dimension type matches none the backend announced in login"))
    }

    fn element(&self, key: &str) -> io::Result<Compound> {
        let key = key.strip_prefix("minecraft:").unwrap_or(key);
        self.types
            .iter()
            .find(|(known, _)| known.strip_prefix("minecraft:").unwrap_or(known) == key)
            .map(|(_, element)| element.clone())
            .ok_or_else(|| invalid("dimension type the backend never announced in login"))
    }
}

/// 1.18.2 `login` to 1.19: chat types and monster spawning fields added,
/// the dimension named by key.
pub fn login_up(data: &[u8], types: &mut DimensionTypes) -> io::Result<Vec<u8>> {
    let mut login = Login::read(758, data)?;
    *types = DimensionTypes::from_codec(&login.codec);
    if let Dimension::Element(element) = &login.dimension {
        login.dimension = Dimension::Key(types.key(element)?);
    }
    if let Some(entries) = registry_entries_mut(&mut login.codec, DIMENSION_TYPES) {
        for entry in entries {
            if let Some(element) = entry
                .as_compound_mut()
                .and_then(|entry| entry.compound_mut("element"))
            {
                // Only the server spawns monsters: any valid values do.
                if !element.contains("monster_spawn_light_level") {
                    element.insert("monster_spawn_light_level", Nbt::Int(7));
                }
                if !element.contains("monster_spawn_block_light_limit") {
                    element.insert("monster_spawn_block_light_limit", Nbt::Int(0));
                }
            }
        }
    }
    login
        .codec
        .0
        .retain(|(name, _)| name != "minecraft:chat_type");
    login.codec.0.insert(
        0,
        (
            "minecraft:chat_type".into(),
            Nbt::Compound(chat::vanilla_1_19()),
        ),
    );
    login.write(759)
}

/// 1.19 `login` to 1.18.2: the dimension's element sent in full, chat types
/// kept for decoration and removed, biome categories restored.
pub fn login_down(
    data: &[u8],
    types: &mut DimensionTypes,
    chat_types: &mut ChatTypes,
) -> io::Result<Vec<u8>> {
    let mut login = Login::read(759, data)?;
    *types = DimensionTypes::from_codec(&login.codec);
    *chat_types = ChatTypes::from_codec(&login.codec);
    if let Dimension::Key(key) = &login.dimension {
        login.dimension = Dimension::Element(types.element(key)?);
    }
    login.codec.remove("minecraft:chat_type");
    if let Some(biomes) = registry_entries_mut(&mut login.codec, "minecraft:worldgen/biome") {
        for biome in biomes {
            if let Some(element) = biome
                .as_compound_mut()
                .and_then(|biome| biome.compound_mut("element"))
                && !element.contains("category")
            {
                // 1.18.2 requires one; the client uses it for nothing visible.
                element.insert("category", "none");
            }
        }
    }
    login.death = None;
    login.write(758)
}

pub fn respawn_up(data: &[u8], types: &DimensionTypes) -> io::Result<Vec<u8>> {
    let mut respawn = Respawn::read(758, data)?;
    if let Dimension::Element(element) = &respawn.dimension {
        respawn.dimension = Dimension::Key(types.key(element)?);
    }
    respawn.write(759)
}

pub fn respawn_down(data: &[u8], types: &DimensionTypes) -> io::Result<Vec<u8>> {
    let mut respawn = Respawn::read(759, data)?;
    if let Dimension::Key(key) = &respawn.dimension {
        respawn.dimension = Dimension::Element(types.element(key)?);
    }
    respawn.death = None;
    respawn.write(758)
}
