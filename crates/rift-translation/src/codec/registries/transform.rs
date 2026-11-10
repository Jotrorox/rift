//! The format changes of registry elements at each version boundary, for
//! entries that are not simply vanilla (see [`super::convert`]). Each rule
//! names the first protocol with the newer format; a step crosses at most
//! one boundary, in either direction. Values a newer format needs and an
//! older entry cannot supply are vanilla's own defaults; anything still
//! missing afterwards is filled by `convert::conform`.
use super::defaults;
use crate::{
    data,
    nbt::{Compound, Nbt},
};
use std::{
    collections::{HashMap, HashSet},
    io,
    sync::{Mutex, OnceLock},
};

/// The first protocol of every format change below: a pair whose newer
/// protocol is one of these converts custom entries even when the vanilla
/// contents happen to be the same.
pub const BOUNDARIES: &[i32] = &[
    751, 755, 757, 758, 759, 760, 761, 762, 764, 766, 768, 769, 770, 771, 774, 775, 777,
];

#[derive(Clone, Copy)]
struct Boundary {
    from: i32,
    to: i32,
}

impl Boundary {
    fn up(self) -> bool {
        self.to > self.from
    }

    /// Whether the step crosses the change that `first` introduced.
    fn crosses(self, first: i32) -> bool {
        self.from.min(self.to) < first && self.from.max(self.to) >= first
    }
}

/// Apply every format change between `from` and the adjacent `to` to one
/// element of `registry` named `name`.
pub fn apply(
    registry: &str,
    from: i32,
    to: i32,
    name: &str,
    element: &mut Compound,
) -> io::Result<()> {
    let step = Boundary { from, to };
    match registry {
        "dimension_type" => dimension_type(step, name, element),
        "worldgen/biome" => biome(step, element),
        "chat_type" => chat_type(step, element),
        "trim_pattern" => trim_pattern(step, element),
        "trim_material" => trim_material(step, element),
        "wolf_variant" => wolf_variant(step, element),
        "wolf_sound_variant" => sound_variant(step, element, Some("minecraft:entity.wolf.step")),
        "pig_variant" | "cat_variant" | "cow_variant" | "chicken_variant" => {
            baby_asset(step, element)
        }
        "enchantment" => enchantment(step, element),
        "timeline" => timeline(step, element),
        "jukebox_song" | "instrument" => {
            if let Some(sound) = element.get_mut("sound_event") {
                sound_reference(to, sound);
            }
        }
        _ => {}
    }
    let attributes = match registry {
        "dimension_type" | "worldgen/biome" => element.compound_mut("attributes"),
        "timeline" => element.compound_mut("tracks"),
        _ => None,
    };
    if let Some(attributes) = attributes {
        known_attributes(to, attributes);
    }
    Ok(())
}

/// Drop environment attributes the receiver lacks: its client fails to load
/// a registry naming an attribute key it does not know (26.1 added light
/// colours, 26.3 spawning and bed rules).
fn known_attributes(protocol: i32, attributes: &mut Compound) {
    let Some(known) = data::version(protocol).registry("environment_attribute") else {
        return;
    };
    attributes
        .0
        .retain(|(key, _)| known.id(data::strip(key)).is_some());
}

/// Fields that pick the vanilla template of a registry (see
/// `defaults::template`): an entry of the same kind fills what is missing.
pub fn template_keys(registry: &str) -> &'static [&'static str] {
    match registry {
        "dimension_type" => &["effects", "skybox", "cardinal_light", "timelines"],
        _ => &[],
    }
}

fn take_str(element: &mut Compound, key: &str) -> Option<String> {
    match element.remove(key)? {
        Nbt::String(value) => Some(value),
        _ => None,
    }
}

fn take_bool(element: &mut Compound, key: &str) -> Option<bool> {
    element.remove(key)?.as_i64().map(|value| value != 0)
}

fn flag(element: &Compound, key: &str) -> Option<bool> {
    element.get(key)?.as_i64().map(|value| value != 0)
}

/// `#rrggbb` for an RGB int, as 1.21.11 writes colours.
fn hex_rgb(color: i64) -> String {
    format!("#{:06x}", color & 0xff_ffff)
}

/// An RGB int from `#rrggbb` or `#aarrggbb` (alpha dropped), or an int.
fn rgb(value: &Nbt) -> Option<i32> {
    match value {
        Nbt::String(text) => {
            let digits = text.strip_prefix('#')?;
            let parsed = u32::from_str_radix(digits, 16).ok()?;
            Some((parsed & 0xff_ffff) as i32)
        }
        other => other.as_i64().map(|value| (value & 0xff_ffff) as i32),
    }
}

/// The dimension effects (`minecraft:overworld`, `the_nether`, `the_end`)
/// a dimension type looks like in any format.
fn dimension_effects(name: &str, element: &Compound) -> &'static str {
    let effects = element
        .str("effects")
        .map(data::strip)
        .or_else(|| match element.str("skybox") {
            Some("end") => Some("the_end"),
            _ if element.str("cardinal_light") == Some("nether") => Some("the_nether"),
            _ => match element.str("timelines").map(data::strip) {
                Some("#in_nether") | Some("#minecraft:in_nether") => Some("the_nether"),
                Some("#in_end") | Some("#minecraft:in_end") => Some("the_end"),
                _ => None,
            },
        })
        .unwrap_or(match data::strip(name) {
            "the_nether" => "the_nether",
            "the_end" => "the_end",
            _ => "overworld",
        });
    match effects {
        "the_nether" => "minecraft:the_nether",
        "the_end" => "minecraft:the_end",
        _ => "minecraft:overworld",
    }
}

fn dimension_type(step: Boundary, name: &str, element: &mut Compound) {
    let up = step.up();
    if step.crosses(751) {
        if up {
            let shrunk = take_bool(element, "shrunk").unwrap_or(false);
            element.remove("name");
            let effects = dimension_effects(name, element);
            element.insert("effects", effects);
            element.insert("coordinate_scale", if shrunk { 8.0f64 } else { 1.0f64 });
        } else {
            element.remove("effects");
            let scale = element.remove("coordinate_scale").and_then(|v| v.as_f64());
            element.insert("name", super::form::namespaced(name));
            element.insert("shrunk", scale.is_some_and(|scale| scale > 1.0));
        }
    }
    if step.crosses(755) {
        if up {
            if !element.contains("min_y") {
                element.insert("min_y", 0i32);
            }
            if !element.contains("height") {
                element.insert("height", 256i32);
            }
        } else {
            element.remove("min_y");
            element.remove("height");
            if let Some(logical) = element.int("logical_height") {
                element.insert("logical_height", logical.clamp(0, 256));
            }
        }
    }
    if step.crosses(758)
        && let Some(Nbt::String(infiniburn)) = element.get_mut("infiniburn")
    {
        if up && !infiniburn.starts_with('#') {
            infiniburn.insert(0, '#');
        } else if !up && let Some(tag) = infiniburn.strip_prefix('#') {
            *infiniburn = tag.to_owned();
        }
    }
    if step.crosses(759) {
        if up {
            if !element.contains("monster_spawn_light_level") {
                let mut range = Compound::new();
                range.insert("min_inclusive", 0i32);
                range.insert("max_inclusive", 7i32);
                let mut level = Compound::new();
                level.insert("type", "minecraft:uniform");
                level.insert("value", range);
                element.insert("monster_spawn_light_level", level);
            }
            if !element.contains("monster_spawn_block_light_limit") {
                element.insert("monster_spawn_block_light_limit", 0i32);
            }
        } else {
            element.remove("monster_spawn_light_level");
            element.remove("monster_spawn_block_light_limit");
        }
    }
    if step.crosses(766)
        && let Some(Nbt::Compound(level)) = element.get_mut("monster_spawn_light_level")
    {
        int_provider(level, up);
    }
    if step.crosses(771) {
        if up {
            if dimension_effects(name, element) == "minecraft:overworld" {
                element.insert("cloud_height", 192i32);
            }
        } else {
            element.remove("cloud_height");
        }
    }
    if step.crosses(774) {
        if up {
            dimension_attributes_up(name, element);
        } else {
            dimension_attributes_down(name, element);
        }
    }
    if step.crosses(775) {
        if up {
            let effects = dimension_effects(name, element);
            element.insert("has_ender_dragon_fight", effects == "minecraft:the_end");
            match effects {
                "minecraft:the_end" => drop(element.insert("default_clock", "minecraft:the_end")),
                "minecraft:overworld" => {
                    drop(element.insert("default_clock", "minecraft:overworld"))
                }
                _ => {}
            }
        } else {
            element.remove("has_ender_dragon_fight");
            element.remove("default_clock");
        }
    }
}

/// 1.20.5 inlined the fields of an int provider next to its `type`; before
/// they sat in a `value` compound (a constant's `value` is an int in both).
fn int_provider(provider: &mut Compound, up: bool) {
    if up {
        if let Some(Nbt::Compound(_)) = provider.get("value")
            && let Some(Nbt::Compound(inner)) = provider.remove("value")
        {
            for (key, value) in inner.0 {
                provider.insert(key, value);
            }
        }
    } else if !provider.contains("value") {
        let kind = provider.remove("type");
        let inner = std::mem::take(provider);
        if let Some(kind) = kind {
            provider.insert("type", kind);
        }
        provider.insert("value", inner);
    }
}

/// 1.21.11 moved a dimension's look and several rules into environment
/// attributes and timelines.
fn dimension_attributes_up(name: &str, element: &mut Compound) {
    let effects = dimension_effects(name, element);
    for key in [
        "effects",
        "piglin_safe",
        "natural",
        "respawn_anchor_works",
        "bed_works",
        "has_raids",
        "ultrawarm",
    ] {
        element.remove(key);
    }
    let cloud_height = element.remove("cloud_height").and_then(|v| v.as_f64());
    if element.remove("fixed_time").is_some() {
        element.insert("has_fixed_time", true);
    }
    let timelines = match effects {
        "minecraft:the_nether" => {
            element.insert("cardinal_light", "nether");
            element.insert("skybox", "none");
            "#minecraft:in_nether"
        }
        "minecraft:the_end" => {
            element.insert("skybox", "end");
            "#minecraft:in_end"
        }
        _ => "#minecraft:in_overworld",
    };
    element.insert("timelines", timelines);
    let mut attributes = defaults::element(774, "dimension_type", effects)
        .and_then(|vanilla| vanilla.compound("attributes"))
        .cloned()
        .unwrap_or_default();
    match cloud_height {
        Some(height) => drop(attributes.insert("minecraft:visual/cloud_height", height as f32)),
        None => drop(attributes.remove("minecraft:visual/cloud_height")),
    }
    element.insert("attributes", attributes);
}

fn dimension_attributes_down(name: &str, element: &mut Compound) {
    let effects = dimension_effects(name, element);
    let attributes = match element.remove("attributes") {
        Some(Nbt::Compound(attributes)) => attributes,
        _ => Compound::new(),
    };
    for key in ["skybox", "cardinal_light", "timelines"] {
        element.remove(key);
    }
    let fixed = take_bool(element, "has_fixed_time").unwrap_or(false);
    let attribute = |key: &str| attributes.get(key).and_then(Nbt::as_i64);
    element.insert("effects", effects);
    element.insert(
        "piglin_safe",
        attribute("minecraft:gameplay/piglins_zombify") == Some(0),
    );
    element.insert(
        "ultrawarm",
        attribute("minecraft:gameplay/water_evaporates") == Some(1),
    );
    let overworld = effects == "minecraft:overworld";
    let nether = effects == "minecraft:the_nether";
    element.insert(
        "natural",
        overworld && flag(element, "has_skylight").unwrap_or(true),
    );
    element.insert("bed_works", overworld);
    element.insert("respawn_anchor_works", nether);
    element.insert("has_raids", !nether);
    if fixed {
        element.insert("fixed_time", if nether { 18000i64 } else { 6000i64 });
    }
    if let Some(height) = attributes
        .get("minecraft:visual/cloud_height")
        .and_then(Nbt::as_f64)
    {
        element.insert("cloud_height", height.floor() as i32);
    }
}

/// Sound fields of a biome's effects (before 1.21.11).
const BIOME_SOUNDS: &[&[&str]] = &[
    &["mood_sound", "sound"],
    &["music", "sound"],
    &["ambient_sound"],
    &["additions_sound", "sound"],
];

fn nested_mut<'a>(element: &'a mut Compound, path: &[&str]) -> Option<&'a mut Nbt> {
    let (last, parents) = path.split_last()?;
    let mut current = element;
    for key in parents {
        current = current.compound_mut(key)?;
    }
    current.get_mut(last)
}

/// A sound event reference for `protocol`: a name up to 1.19.2, always the
/// direct `{sound_id}` form in 1.19.3, and from 1.19.4 a name when the
/// receiver has that sound built in or the direct form when it does not.
pub fn sound_reference(protocol: i32, sound: &mut Nbt) {
    let name = match sound {
        Nbt::String(name) => name.clone(),
        Nbt::Compound(direct) => match direct.str("sound_id") {
            Some(name) => name.to_owned(),
            None => return,
        },
        _ => return,
    };
    let built_in = data::version(protocol)
        .registry("sound_event")
        .is_some_and(|sounds| sounds.id(&name).is_some());
    if protocol < 761 || (protocol >= 762 && built_in) {
        *sound = Nbt::String(name);
    } else if let Nbt::String(_) = sound {
        let mut direct = Compound::new();
        direct.insert("sound_id", name);
        *sound = Nbt::Compound(direct);
    }
}

/// A built-in sound the receiver lacks becomes its stand-in from the pair's
/// mapping, where there is one, so the receiver plays something it knows.
fn stand_in_sound(step: Boundary, sound: &mut Nbt) {
    let name = match sound {
        Nbt::String(name) => name,
        Nbt::Compound(direct) => match direct.get_mut("sound_id") {
            Some(Nbt::String(name)) => name,
            _ => return,
        },
        _ => return,
    };
    let (Some(sender), Some(receiver)) = (
        data::version(step.from).registry("sound_event"),
        data::version(step.to).registry("sound_event"),
    ) else {
        return;
    };
    if receiver.id(name).is_some() {
        return;
    }
    let stand_in = sender
        .id(name)
        .and_then(|id| crate::mapping::mapping(step.from, step.to, "sound_event").get(id))
        .and_then(|id| receiver.name(id));
    if let Some(stand_in) = stand_in {
        *name = super::form::namespaced(stand_in);
    }
}

fn biome(step: Boundary, element: &mut Compound) {
    let up = step.up();
    if step.crosses(757) {
        if up {
            element.remove("depth");
            element.remove("scale");
        } else {
            element.insert("depth", 0.125f32);
            element.insert("scale", 0.05f32);
        }
    }
    if step.crosses(759) {
        if up {
            element.remove("category");
        } else if !element.contains("category") {
            element.insert("category", "none");
        }
    }
    if step.crosses(762) {
        if up {
            let precipitation = take_str(element, "precipitation");
            element.insert(
                "has_precipitation",
                precipitation.is_some_and(|value| value != "none"),
            );
        } else {
            let falls = take_bool(element, "has_precipitation").unwrap_or(false);
            let cold = element
                .get("temperature")
                .and_then(Nbt::as_f64)
                .is_some_and(|temperature| temperature < 0.15);
            let kind = match (falls, cold) {
                (false, _) => "none",
                (true, true) => "snow",
                (true, false) => "rain",
            };
            element.insert("precipitation", kind);
        }
    }
    if step.crosses(769)
        && let Some(effects) = element.compound_mut("effects")
    {
        if up {
            if let Some(Nbt::Compound(music)) = effects.remove("music") {
                let mut weighted = Compound::new();
                weighted.insert("data", music);
                weighted.insert("weight", 1i32);
                effects.insert("music", vec![Nbt::Compound(weighted)]);
            }
        } else {
            effects.remove("music_volume");
            if let Some(Nbt::List(choices)) = effects.remove("music")
                && let Some(Nbt::Compound(first)) = choices.into_iter().next()
                && let Some(Nbt::Compound(music)) = first.get("data")
            {
                effects.insert("music", music.clone());
            }
        }
    }
    if step.crosses(774) {
        if up {
            biome_attributes_up(element);
        } else {
            biome_attributes_down(element);
        }
    }
    if step.crosses(777)
        && !up
        && let Some(attributes) = element.compound_mut("attributes")
    {
        unwrap_modifiers(attributes);
    }
    if step.to < 774 {
        if let Some(effects) = element.compound_mut("effects") {
            for path in BIOME_SOUNDS {
                if let Some(sound) = nested_mut(effects, path) {
                    stand_in_sound(step, sound);
                    sound_reference(step.to, sound);
                }
            }
            if let Some(Nbt::Compound(particle)) = effects.get("particle")
                && !particle_survives(step, particle.compound("options"))
            {
                effects.remove("particle");
            }
        }
    } else if let Some(attributes) = element.compound_mut("attributes") {
        biome_attribute_sounds(step.to, attributes);
    }
}

/// Whether a biome's ambient particle can be sent to `step.to`: its type
/// exists there, and across 1.20.5 (new particle option formats) it has no
/// options beyond its type.
fn particle_survives(step: Boundary, options: Option<&Compound>) -> bool {
    let Some(options) = options else {
        return false;
    };
    let Some(kind) = options.str("type") else {
        return false;
    };
    let known = data::version(step.to)
        .registry("particle_type")
        .is_some_and(|particles| particles.id(kind).is_some());
    known && (!step.crosses(766) || options.len() == 1)
}

fn biome_attribute_sounds(protocol: i32, attributes: &mut Compound) {
    if let Some(Nbt::Compound(music)) = attributes.get_mut("minecraft:audio/background_music") {
        for (_, choice) in music.0.iter_mut() {
            if let Nbt::Compound(choice) = choice
                && let Some(sound) = choice.get_mut("sound")
            {
                sound_reference(protocol, sound);
            }
        }
    }
    if let Some(Nbt::Compound(ambient)) = attributes.get_mut("minecraft:audio/ambient_sounds") {
        for path in [&["mood", "sound"][..], &["additions", "sound"], &["loop"]] {
            if let Some(sound) = nested_mut(ambient, path) {
                sound_reference(protocol, sound);
            }
        }
    }
}

/// Before 26.3 an environment attribute is a plain value; 26.3 also allows
/// `{argument, modifier}`. Keep what an older client can take as a value.
fn unwrap_modifiers(attributes: &mut Compound) {
    attributes.0.retain_mut(|(_, value)| {
        let Nbt::Compound(modified) = value else {
            return true;
        };
        if modified.len() != 2 || !modified.contains("argument") {
            return true;
        }
        match modified.str("modifier") {
            Some("override" | "append") => {
                *value = modified.get("argument").cloned().unwrap_or(Nbt::Byte(0));
                true
            }
            _ => false,
        }
    });
}

/// 1.21.11 replaced most biome effects with environment attributes.
fn biome_attributes_up(element: &mut Compound) {
    let mut effects = match element.remove("effects") {
        Some(Nbt::Compound(effects)) => effects,
        _ => Compound::new(),
    };
    let mut attributes = Compound::new();
    let mut new_effects = Compound::new();
    for (from, to) in [
        ("sky_color", "minecraft:visual/sky_color"),
        ("fog_color", "minecraft:visual/fog_color"),
        ("water_fog_color", "minecraft:visual/water_fog_color"),
    ] {
        if let Some(color) = effects.remove(from).and_then(|value| value.as_i64()) {
            attributes.insert(to, hex_rgb(color));
        }
    }
    for key in [
        "water_color",
        "foliage_color",
        "grass_color",
        "dry_foliage_color",
    ] {
        if let Some(color) = effects.remove(key).and_then(|value| value.as_i64()) {
            new_effects.insert(key, hex_rgb(color));
        }
    }
    if !new_effects.contains("water_color") {
        new_effects.insert("water_color", "#3f76e4");
    }
    if let Some(modifier) = effects.remove("grass_color_modifier") {
        new_effects.insert("grass_color_modifier", modifier);
    }
    if let Some(Nbt::List(choices)) = effects.remove("music")
        && let Some(Nbt::Compound(first)) = choices.into_iter().next()
        && let Some(Nbt::Compound(music)) = first.get("data")
    {
        let mut music = music.clone();
        if flag(&music, "replace_current_music") == Some(false) {
            music.remove("replace_current_music");
        }
        let mut background = Compound::new();
        background.insert("default", music);
        attributes.insert("minecraft:audio/background_music", background);
    }
    if let Some(volume) = effects
        .remove("music_volume")
        .and_then(|value| value.as_f64())
        && volume != 1.0
    {
        attributes.insert("minecraft:audio/music_volume", volume as f32);
    }
    let mut ambient = Compound::new();
    if let Some(mood) = effects.remove("mood_sound") {
        ambient.insert("mood", mood);
    }
    if let Some(additions) = effects.remove("additions_sound") {
        ambient.insert("additions", additions);
    }
    if let Some(sound) = effects.remove("ambient_sound") {
        ambient.insert("loop", sound);
    }
    if !ambient.is_empty() {
        attributes.insert("minecraft:audio/ambient_sounds", ambient);
    }
    if let Some(Nbt::Compound(particle)) = effects.remove("particle")
        && let (Some(options), Some(probability)) =
            (particle.get("options"), particle.get("probability"))
    {
        let mut ambient_particle = Compound::new();
        ambient_particle.insert("probability", probability.clone());
        ambient_particle.insert("particle", options.clone());
        attributes.insert(
            "minecraft:visual/ambient_particles",
            vec![Nbt::Compound(ambient_particle)],
        );
    }
    element.insert("effects", new_effects);
    element.insert("attributes", attributes);
}

fn biome_attributes_down(element: &mut Compound) {
    let mut attributes = match element.remove("attributes") {
        Some(Nbt::Compound(attributes)) => attributes,
        _ => Compound::new(),
    };
    let old = match element.remove("effects") {
        Some(Nbt::Compound(effects)) => effects,
        _ => Compound::new(),
    };
    let mut effects = Compound::new();
    // Colours a biome leaves to its dimension in 1.21.11 take the
    // overworld's values, which 1.21.10 biomes spell out.
    for (attribute, key, fallback) in [
        ("minecraft:visual/sky_color", "sky_color", 7907327),
        ("minecraft:visual/fog_color", "fog_color", 12638463),
        (
            "minecraft:visual/water_fog_color",
            "water_fog_color",
            329011,
        ),
    ] {
        let color = attributes.get(attribute).and_then(rgb).unwrap_or(fallback);
        effects.insert(key, color);
    }
    effects.insert(
        "water_color",
        old.get("water_color").and_then(rgb).unwrap_or(4159204),
    );
    for key in ["foliage_color", "grass_color", "dry_foliage_color"] {
        if let Some(color) = old.get(key).and_then(rgb) {
            effects.insert(key, color);
        }
    }
    if let Some(modifier) = old.get("grass_color_modifier") {
        effects.insert("grass_color_modifier", modifier.clone());
    }
    if let Some(Nbt::Compound(background)) = attributes.remove("minecraft:audio/background_music")
        && let Some(Nbt::Compound(music)) = background.get("default")
    {
        let mut music = music.clone();
        if !music.contains("replace_current_music") {
            music.insert("replace_current_music", false);
        }
        let mut weighted = Compound::new();
        weighted.insert("data", music);
        weighted.insert("weight", 1i32);
        effects.insert("music", vec![Nbt::Compound(weighted)]);
    }
    let volume = attributes
        .remove("minecraft:audio/music_volume")
        .and_then(|value| value.as_f64())
        .unwrap_or(1.0);
    effects.insert("music_volume", volume as f32);
    if let Some(Nbt::Compound(ambient)) = attributes.remove("minecraft:audio/ambient_sounds") {
        if let Some(mood) = ambient.get("mood") {
            effects.insert("mood_sound", mood.clone());
        }
        if let Some(additions) = ambient.get("additions") {
            effects.insert("additions_sound", additions.clone());
        }
        if let Some(sound) = ambient.get("loop") {
            effects.insert("ambient_sound", sound.clone());
        }
    }
    if let Some(Nbt::List(particles)) = attributes.remove("minecraft:visual/ambient_particles")
        && let Some(Nbt::Compound(first)) = particles.into_iter().next()
        && let (Some(particle), Some(probability)) =
            (first.get("particle"), first.get("probability"))
    {
        let mut old_particle = Compound::new();
        old_particle.insert("probability", probability.clone());
        old_particle.insert("options", particle.clone());
        effects.insert("particle", old_particle);
    }
    element.insert("effects", effects);
}

fn chat_type(step: Boundary, element: &mut Compound) {
    if !step.crosses(760) {
        return;
    }
    if step.up() {
        // 1.19: {chat: {decoration: {...}}, narration: {decoration, priority}, overlay}
        let decoration = |part: Option<Nbt>| -> Compound {
            let mut decoration = part
                .and_then(|part| match part {
                    Nbt::Compound(mut part) => match part.remove("decoration") {
                        Some(Nbt::Compound(decoration)) => Some(decoration),
                        _ => None,
                    },
                    _ => None,
                })
                .unwrap_or_else(|| {
                    let mut plain = Compound::new();
                    plain.insert("translation_key", "%s");
                    plain.insert("parameters", vec![Nbt::from("content")]);
                    plain
                });
            if let Some(Nbt::Compound(style)) = decoration.get("style")
                && style.is_empty()
            {
                decoration.remove("style");
            }
            decoration
        };
        let mut chat = decoration(element.remove("chat"));
        let mut narration = decoration(element.remove("narration"));
        narration.remove("style");
        rename_parameter(&mut chat, "team_name", "target");
        rename_parameter(&mut narration, "team_name", "target");
        element.0.clear();
        element.insert("chat", chat);
        element.insert("narration", narration);
    } else {
        let wrap = |part: Option<Nbt>, narration: bool| -> Compound {
            let mut decoration = match part {
                Some(Nbt::Compound(part)) => part,
                _ => Compound::new(),
            };
            rename_parameter(&mut decoration, "target", "team_name");
            if !decoration.contains("style") {
                decoration.insert("style", Compound::new());
            }
            let mut wrapped = Compound::new();
            wrapped.insert("decoration", decoration);
            if narration {
                wrapped.insert("priority", "chat");
            }
            wrapped
        };
        let chat = wrap(element.remove("chat"), false);
        let narration = wrap(element.remove("narration"), true);
        element.0.clear();
        element.insert("chat", chat);
        element.insert("narration", narration);
    }
}

/// Rename a chat decoration parameter: 1.19 calls the target of team and
/// private messages `team_name`, 1.19.1 and later `target`, and each
/// rejects the other's name.
fn rename_parameter(decoration: &mut Compound, from: &str, to: &str) {
    if let Some(Nbt::List(parameters)) = decoration.get_mut("parameters") {
        for parameter in parameters {
            if parameter.as_str() == Some(from) {
                *parameter = Nbt::from(to);
            }
        }
    }
}

fn trim_pattern(step: Boundary, element: &mut Compound) {
    if step.crosses(764) {
        if step.up() {
            if !element.contains("decal") {
                element.insert("decal", false);
            }
        } else {
            element.remove("decal");
        }
    }
    if step.crosses(770) && step.up() {
        element.remove("template_item");
    }
}

fn trim_material(step: Boundary, element: &mut Compound) {
    let up = step.up();
    if step.crosses(766) {
        for key in ["override_armor_materials", "override_armor_assets"] {
            if let Some(overrides) = element.compound_mut(key) {
                for (material, _) in overrides.0.iter_mut() {
                    *material = if up {
                        super::form::namespaced(material)
                    } else {
                        data::strip(material).to_owned()
                    };
                }
            }
        }
    }
    if step.crosses(769) {
        let (from, to) = if up {
            ("override_armor_materials", "override_armor_assets")
        } else {
            ("override_armor_assets", "override_armor_materials")
        };
        if let Some(overrides) = element.remove(from) {
            element.insert(to, overrides);
        }
        if up {
            element.remove("item_model_index");
        }
    }
    if step.crosses(770) && up {
        element.remove("ingredient");
    }
    if step.crosses(777) {
        if up {
            if let Some(asset) = take_str(element, "asset_name") {
                let (namespace, path) = asset.split_once(':').unwrap_or(("minecraft", &asset));
                element.insert("palette_id", format!("{namespace}:trim/{path}"));
            }
            element.remove("override_armor_assets");
        } else if let Some(palette) = take_str(element, "palette_id") {
            let path = data::strip(&palette);
            let path = path.rsplit('/').next().unwrap_or(path);
            element.insert("asset_name", path);
        }
    }
}

fn wolf_variant(step: Boundary, element: &mut Compound) {
    let up = step.up();
    if step.crosses(770) {
        if up {
            let mut assets = Compound::new();
            for (from, to) in [
                ("wild_texture", "wild"),
                ("tame_texture", "tame"),
                ("angry_texture", "angry"),
            ] {
                if let Some(texture) = element.remove(from) {
                    assets.insert(to, texture);
                }
            }
            element.remove("biomes");
            element.insert("assets", assets);
        } else if let Some(Nbt::Compound(assets)) = element.remove("assets") {
            for (from, to) in [
                ("wild", "wild_texture"),
                ("tame", "tame_texture"),
                ("angry", "angry_texture"),
            ] {
                if let Some(texture) = assets.get(from) {
                    element.insert(to, texture.clone());
                }
            }
        }
    }
    if step.crosses(775) {
        if up {
            if let Some(assets) = element.get("assets").cloned() {
                element.insert("baby_assets", assets);
            }
        } else {
            element.remove("baby_assets");
        }
    }
}

/// 26.1 split mob sound variants into adult and baby sounds and gave adults
/// a step sound.
fn sound_variant(step: Boundary, element: &mut Compound, step_sound: Option<&str>) {
    if step.crosses(775) {
        if step.up() {
            let mut adult = std::mem::take(element);
            if let Some(sound) = step_sound
                && !adult.contains("step_sound")
            {
                adult.insert("step_sound", sound);
            }
            element.insert("adult_sounds", adult.clone());
            element.insert("baby_sounds", adult);
        } else if let Some(Nbt::Compound(mut adult)) = element.remove("adult_sounds") {
            adult.remove("step_sound");
            *element = adult;
        }
    }
    let to = step.to;
    for (_, value) in element.0.iter_mut() {
        match value {
            Nbt::Compound(sounds) => {
                for (_, sound) in sounds.0.iter_mut() {
                    sound_reference(to, sound);
                }
            }
            sound => sound_reference(to, sound),
        }
    }
}

fn baby_asset(step: Boundary, element: &mut Compound) {
    if step.crosses(775) {
        if step.up() {
            if let Some(asset) = element.get("asset_id").cloned() {
                element.insert("baby_asset_id", asset);
            }
        } else {
            element.remove("baby_asset_id");
        }
    }
}

/// Effect component types vanilla enchantments of `protocol` use.
fn enchantment_effects(protocol: i32) -> HashSet<String> {
    static CACHE: OnceLock<Mutex<HashMap<i32, HashSet<String>>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    cache
        .entry(protocol)
        .or_insert_with(|| {
            defaults::get(protocol, "enchantment")
                .map(|defaults| {
                    defaults
                        .entries
                        .iter()
                        .filter_map(|entry| entry.element.as_compound()?.compound("effects"))
                        .flat_map(|effects| effects.iter().map(|(key, _)| key.to_owned()))
                        .collect()
                })
                .unwrap_or_default()
        })
        .clone()
}

/// Rename attribute IDs inside enchantment effects across 1.21.2, which
/// dropped the `generic.`, `player.` and `zombie.` prefixes.
fn rename_attributes(value: &mut Nbt, to: i32, up: bool) {
    match value {
        Nbt::Compound(compound) => {
            for (key, value) in compound.0.iter_mut() {
                if key == "attribute"
                    && let Nbt::String(attribute) = value
                {
                    let bare = data::strip(attribute).to_owned();
                    if up {
                        if let Some((_, rest)) = bare.split_once('.') {
                            *attribute = format!("minecraft:{rest}");
                        }
                    } else if let Some(attributes) = data::version(to).registry("attribute") {
                        for prefix in ["generic.", "player.", "zombie."] {
                            if attributes.id(&format!("{prefix}{bare}")).is_some() {
                                *attribute = format!("minecraft:{prefix}{bare}");
                                break;
                            }
                        }
                    }
                } else {
                    rename_attributes(value, to, up);
                }
            }
        }
        Nbt::List(values) => {
            for value in values {
                rename_attributes(value, to, up);
            }
        }
        _ => {}
    }
}

fn enchantment(step: Boundary, element: &mut Compound) {
    if step.crosses(768)
        && let Some(effects) = element.get_mut("effects")
    {
        rename_attributes(effects, step.to, step.up());
    }
    if step.crosses(777)
        && let Some(effects) = element.get_mut("effects")
    {
        effect_requirements(effects, step.up());
    }
    let known = enchantment_effects(step.to);
    if let Some(effects) = element.compound_mut("effects") {
        effects.0.retain(|(key, _)| known.contains(key));
    }
}

/// Every `requirements` loot condition inside enchantment effects.
fn effect_requirements(value: &mut Nbt, up: bool) {
    match value {
        Nbt::Compound(compound) => {
            for (key, value) in compound.0.iter_mut() {
                if key == "requirements" {
                    loot_condition(value, up);
                } else {
                    effect_requirements(value, up);
                }
            }
        }
        Nbt::List(list) => list
            .iter_mut()
            .for_each(|value| effect_requirements(value, up)),
        _ => {}
    }
}

/// 26.3 names a loot condition's kind `type` where older versions name it
/// `condition`; `inverted`, `all_of` and `any_of` nest more conditions.
fn loot_condition(value: &mut Nbt, up: bool) {
    match value {
        Nbt::Compound(condition) => {
            let (from, to) = if up {
                ("condition", "type")
            } else {
                ("type", "condition")
            };
            for (key, _) in condition.0.iter_mut() {
                if key == from {
                    *key = to.to_owned();
                }
            }
            for (key, value) in condition.0.iter_mut() {
                if key == "term" || key == "terms" {
                    loot_condition(value, up);
                }
            }
        }
        Nbt::List(list) => list.iter_mut().for_each(|value| loot_condition(value, up)),
        _ => {}
    }
}

fn timeline(step: Boundary, element: &mut Compound) {
    if step.crosses(775) {
        if step.up() {
            if !element.contains("clock") {
                element.insert("clock", "minecraft:overworld");
            }
        } else {
            element.remove("clock");
            element.remove("time_markers");
        }
    }
    if step.crosses(777)
        && let Some(Nbt::Compound(track)) = element
            .compound_mut("tracks")
            .and_then(|tracks| tracks.get_mut("minecraft:visual/cloud_color"))
        && let Some(Nbt::List(keyframes)) = track.get_mut("keyframes")
    {
        for keyframe in keyframes {
            if let Nbt::Compound(keyframe) = keyframe
                && let Some(value) = keyframe.get_mut("value")
            {
                match (step.up(), &*value) {
                    (true, Nbt::Int(argb)) => {
                        *value = Nbt::String(format!("#{:08x}", *argb as u32));
                    }
                    (false, Nbt::String(text)) => {
                        if let Some(argb) = text
                            .strip_prefix('#')
                            .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                        {
                            *value = Nbt::Int(argb as i32);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
