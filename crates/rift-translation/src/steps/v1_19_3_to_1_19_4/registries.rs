//! The registry codec in join game, as 1.19.3 to 1.20 send it: synchronised
//! registries by name (`minecraft:worldgen/biome`, `minecraft:damage_type`...),
//! each a compound with its `type` and a `value` list of entries with a
//! `name`, an `id` and an `element`.
//!
//! INTEGRATION: the registries codec (`codec::registries`) is to own the
//! registry parts of join game for every protocol. Until it offers helpers
//! for them, the 1.19.3 to 1.20 steps convert what their pairs change here:
//! the damage types, trim patterns and trim materials 1.19.4 added, rain and
//! snow becoming one flag in 1.19.4, biome sounds becoming registry
//! references in 1.19.4, and built-in registry names inside entries.
use crate::{
    Direction,
    codec::Shared,
    nbt::{Compound, Nbt},
    wire::invalid,
};
use serde::Deserialize;
use std::io;

const NAMESPACE: &str = "minecraft:";

/// Biomes from 1.19.4 no longer name their precipitation: whether it rains
/// or snows follows from the temperature alone.
const PRECIPITATION_FLAG: i32 = 762;
/// From 1.19.4 biome sounds are registry references; 1.19.3 only reads them
/// inline (`{sound_id: ...}`).
const SOUND_REFERENCES: i32 = 762;
/// Below this temperature it snows instead of raining, in every version.
const SNOW_BELOW: f64 = 0.15;

/// The entries of `registry` (such as `"damage_type"`), if the codec has it.
pub fn entries_mut<'a>(codec: &'a mut Compound, registry: &str) -> Option<&'a mut Vec<Nbt>> {
    codec
        .compound_mut(&format!("{NAMESPACE}{registry}"))?
        .get_mut("value")?
        .as_list_mut()
}

/// Add `registry` without entries unless the codec has it, as 1.19.4 sends
/// the trim registries when the 1.20 experiment is off.
pub fn ensure_registry(codec: &mut Compound, registry: &str) {
    let key = format!("{NAMESPACE}{registry}");
    if !codec.contains(&key) {
        codec.insert(key.clone(), registry_compound(&key, Vec::new()));
    }
}

fn registry_compound(key: &str, entries: Vec<Nbt>) -> Compound {
    let mut registry = Compound::new();
    registry.insert("type", key);
    registry.insert("value", entries);
    registry
}

/// The receiver's name for the built-in registry entry `name` of the
/// sender, through the pair's mapping; `None` when it has no counterpart or
/// the sender does not know the name.
pub fn map_name(
    shared: &mut Shared,
    direction: Direction,
    registry: &'static str,
    name: &str,
) -> Option<String> {
    let id = shared.source_data(direction).registry(registry)?.id(name)?;
    let mapped = shared.mapping(direction, registry).get(id)?;
    let target = shared
        .target_data(direction)
        .registry(registry)?
        .name(mapped)?;
    Some(format!("{NAMESPACE}{target}"))
}

/// One vanilla damage type, as `data/damage_types.json` lists them in ID
/// order (taken from the join game of the official server).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DamageType {
    pub name: String,
    #[serde(default)]
    pub effects: Option<String>,
    pub scaling: String,
    pub exhaustion: f32,
    pub message_id: String,
    #[serde(default)]
    pub death_message_type: Option<String>,
}

impl DamageType {
    fn entry(&self, id: i32) -> Nbt {
        let mut element = Compound::new();
        if let Some(effects) = &self.effects {
            element.insert("effects", effects.as_str());
        }
        element.insert("scaling", self.scaling.as_str());
        element.insert("exhaustion", self.exhaustion);
        element.insert("message_id", self.message_id.as_str());
        if let Some(kind) = &self.death_message_type {
            element.insert("death_message_type", kind.as_str());
        }
        let mut entry = Compound::new();
        entry.insert("name", format!("{NAMESPACE}{}", self.name));
        entry.insert("id", id);
        entry.insert("element", element);
        Nbt::Compound(entry)
    }
}

pub fn parse_damage_types(json: &str) -> Vec<DamageType> {
    serde_json::from_str(json).expect("damage type table parses")
}

/// Make sure the damage type registry has every vanilla type of the
/// receiving version: clients from 1.19.4 look several up by name and fail
/// without them. Missing types get IDs after the highest present one, so
/// IDs the sender already uses keep their meaning.
pub fn ensure_damage_types(codec: &mut Compound, vanilla: &[DamageType]) -> io::Result<()> {
    let key = format!("{NAMESPACE}damage_type");
    let Some(entries) = entries_mut(codec, "damage_type") else {
        let entries = vanilla
            .iter()
            .enumerate()
            .map(|(id, kind)| kind.entry(id as i32))
            .collect();
        codec.insert(key.clone(), registry_compound(&key, entries));
        return Ok(());
    };
    let mut next = 0;
    let mut present = Vec::with_capacity(entries.len());
    for entry in entries.iter() {
        let entry = entry
            .as_compound()
            .ok_or_else(|| invalid("damage type entry is not a compound"))?;
        let id = entry
            .int("id")
            .ok_or_else(|| invalid("damage type entry without an ID"))?;
        next = next.max(id.saturating_add(1));
        present.push(entry.str("name").unwrap_or_default().to_owned());
    }
    for kind in vanilla {
        let name = format!("{NAMESPACE}{}", kind.name);
        if !present.contains(&name) {
            entries.push(kind.entry(next));
            next += 1;
        }
    }
    Ok(())
}

/// The damage type names by ID, for packets that carry damage type IDs.
pub fn damage_type_names(codec: &Compound) -> Vec<(i32, String)> {
    let Some(entries) = codec
        .compound(&format!("{NAMESPACE}damage_type"))
        .and_then(|registry| registry.list("value"))
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(Nbt::as_compound)
        .filter_map(|entry| {
            let name = entry.str("name")?;
            let name = name.strip_prefix(NAMESPACE).unwrap_or(name);
            Some((entry.int("id")?, name.to_owned()))
        })
        .collect()
}

/// Convert every biome for the receiver: precipitation, and the sounds and
/// particle of its effects.
pub fn convert_biomes(
    shared: &mut Shared,
    direction: Direction,
    codec: &mut Compound,
) -> io::Result<()> {
    let target = shared.target(direction);
    let Some(biomes) = entries_mut(codec, "worldgen/biome") else {
        return Err(invalid("registry codec without biomes"));
    };
    for biome in biomes {
        let element = biome
            .as_compound_mut()
            .and_then(|biome| biome.compound_mut("element"))
            .ok_or_else(|| invalid("biome entry without an element"))?;
        precipitation(element, target)?;
        if let Some(effects) = element.compound_mut("effects") {
            biome_effects(shared, direction, effects)?;
        }
    }
    Ok(())
}

fn precipitation(element: &mut Compound, target: i32) -> io::Result<()> {
    if target >= PRECIPITATION_FLAG {
        if let Some(kind) = element.remove("precipitation") {
            let kind = kind
                .as_str()
                .ok_or_else(|| invalid("biome precipitation is not a string"))?;
            element.insert("has_precipitation", kind != "none");
        }
    } else if let Some(flag) = element.remove("has_precipitation") {
        let flag = flag
            .as_i64()
            .ok_or_else(|| invalid("biome precipitation flag is not a number"))?;
        let temperature = element
            .get("temperature")
            .and_then(Nbt::as_f64)
            .ok_or_else(|| invalid("biome without a temperature"))?;
        let kind = match (flag != 0, temperature < SNOW_BELOW) {
            (false, _) => "none",
            (true, true) => "snow",
            (true, false) => "rain",
        };
        element.insert("precipitation", kind);
    }
    Ok(())
}

fn biome_effects(
    shared: &mut Shared,
    direction: Direction,
    effects: &mut Compound,
) -> io::Result<()> {
    if let Some(sound) = effects.get_mut("ambient_sound") {
        convert_sound(shared, direction, sound)?;
    }
    for key in ["mood_sound", "additions_sound", "music"] {
        if let Some(sound) = effects
            .compound_mut(key)
            .and_then(|holder| holder.get_mut("sound"))
        {
            convert_sound(shared, direction, sound)?;
        }
    }
    let particle = effects
        .compound("particle")
        .and_then(|particle| particle.compound("options"))
        .and_then(|options| options.str("type"))
        .map(str::to_owned);
    if let Some(kind) = particle {
        let known = shared
            .source_data(direction)
            .registry("particle_type")
            .and_then(|registry| registry.id(&kind))
            .is_some();
        match map_name(shared, direction, "particle_type", &kind) {
            Some(mapped) => {
                if let Some(options) = effects
                    .compound_mut("particle")
                    .and_then(|particle| particle.compound_mut("options"))
                {
                    options.insert("type", mapped);
                }
            }
            // A known particle the receiver lacks: no ambient particles.
            None if known => drop(effects.remove("particle")),
            None => {}
        }
    }
    Ok(())
}

/// A biome sound: a sound event name (1.19.4 on) or an inline sound event
/// with a `sound_id` and an optional fixed `range`.
fn convert_sound(shared: &mut Shared, direction: Direction, sound: &mut Nbt) -> io::Result<()> {
    let (name, range) = match &*sound {
        Nbt::String(name) => (name.clone(), None),
        Nbt::Compound(inline) => (
            inline
                .str("sound_id")
                .ok_or_else(|| invalid("inline sound event without a sound_id"))?
                .to_owned(),
            inline.get("range").cloned(),
        ),
        _ => return Err(invalid("biome sound is neither a name nor a sound event")),
    };
    let mapped = map_name(shared, direction, "sound_event", &name);
    *sound = match (mapped, range) {
        (Some(mapped), None) if shared.target(direction) >= SOUND_REFERENCES => Nbt::String(mapped),
        (mapped, range) => {
            // Inline sound events need not be registered, so an unknown one
            // stays as it is rather than failing the receiver's decoder.
            let mut inline = Compound::new();
            inline.insert("sound_id", mapped.unwrap_or(name));
            if let Some(range) = range {
                inline.insert("range", range);
            }
            Nbt::Compound(inline)
        }
    };
    Ok(())
}

/// Map the items trim patterns and materials name to the receiver's, and
/// drop entries whose item it lacks: its decoder rejects unknown items.
pub fn convert_trims(
    shared: &mut Shared,
    direction: Direction,
    codec: &mut Compound,
) -> io::Result<()> {
    for (registry, field) in [
        ("trim_pattern", "template_item"),
        ("trim_material", "ingredient"),
    ] {
        let Some(entries) = entries_mut(codec, registry) else {
            continue;
        };
        let mut kept = Vec::with_capacity(entries.len());
        for mut entry in std::mem::take(entries) {
            let element = entry
                .as_compound_mut()
                .and_then(|entry| entry.compound_mut("element"))
                .ok_or_else(|| invalid("trim entry without an element"))?;
            let item = element
                .str(field)
                .ok_or_else(|| invalid("trim entry without its item"))?;
            if let Some(mapped) = map_name(shared, direction, "item", item) {
                element.insert(field, mapped);
                kept.push(entry);
            }
        }
        *entries = kept;
    }
    Ok(())
}
