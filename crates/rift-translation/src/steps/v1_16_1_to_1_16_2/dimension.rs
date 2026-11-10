//! The dimension codec as 1.16.1 lists dimension types and as 1.16.2 makes
//! them a registry beside the biomes.
//!
//! INTEGRATION: the registries codec is to provide the registry parts of
//! `login` and `respawn`; until it does, this module converts them itself.
use crate::{
    nbt::{Compound, Nbt},
    wire::invalid,
};
use std::{io, io::Read, sync::OnceLock};

/// The codec a vanilla 1.16.2 server sends in `login`, as recorded from the
/// official server (gzip of a named root compound). Its biome registry is
/// what 1.16.2 clients get from a 1.16.1 backend: vanilla 1.16.2 numbers its
/// biomes as 1.16.1 does, so chunk biome IDs stay valid.
const VANILLA: &[u8] = include_bytes!("dimension_codec.nbt.gz");

pub(super) const DIMENSION_TYPE: &str = "minecraft:dimension_type";
pub(super) const BIOME: &str = "minecraft:worldgen/biome";
const OVERWORLD: &str = "minecraft:overworld";

fn vanilla() -> &'static Compound {
    static CODEC: OnceLock<Compound> = OnceLock::new();
    CODEC.get_or_init(|| {
        let mut bytes = Vec::new();
        flate2::read::GzDecoder::new(VANILLA)
            .read_to_end(&mut bytes)
            .expect("the embedded 1.16.2 codec is gzip");
        match Nbt::from_bytes(&bytes, true) {
            Ok(Some(Nbt::Compound(codec))) => codec,
            _ => panic!("the embedded 1.16.2 codec is a compound"),
        }
    })
}

/// The registry `name` of a 1.16.2 codec: its entries' names, IDs and
/// elements.
fn entries<'a>(codec: &'a Compound, name: &str) -> io::Result<&'a [Nbt]> {
    codec
        .compound(name)
        .and_then(|registry| registry.list("value"))
        .ok_or_else(|| invalid("dimension codec without a registry"))
}

/// Whether two tags hold the same data, whatever order compounds list their
/// keys in.
fn same(a: &Nbt, b: &Nbt) -> bool {
    match (a, b) {
        (Nbt::Compound(a), Nbt::Compound(b)) => same_compound(a, b),
        (Nbt::List(a), Nbt::List(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same(a, b))
        }
        _ => a == b,
    }
}

fn same_compound(a: &Compound, b: &Compound) -> bool {
    a.len() == b.len()
        && a.iter()
            .all(|(key, value)| b.get(key).is_some_and(|other| same(value, other)))
}

/// The dimension types of one codec, by name, as 1.16.2 elements.
#[derive(Debug, Default)]
pub(super) struct Types {
    types: Vec<(String, Compound)>,
}

impl Types {
    /// The element 1.16.2 sends for the dimension type `name`; the first type
    /// when the codec has no such type.
    pub(super) fn element(&self, name: &str) -> io::Result<&Compound> {
        self.types
            .iter()
            .find(|(known, _)| known == name)
            .or_else(|| self.types.first())
            .map(|(_, element)| element)
            .ok_or_else(|| invalid("dimension codec without dimension types"))
    }

    /// The name of the type whose element is `element`: an exact match, else
    /// a type with the same effects, else the first type.
    pub(super) fn name(&self, element: &Compound) -> io::Result<&str> {
        if let Some(name) = self.exact(element) {
            return Ok(name);
        }
        let effects = element.str("effects").unwrap_or(OVERWORLD);
        self.types
            .iter()
            .find(|(name, _)| name == effects)
            .or_else(|| {
                self.types
                    .iter()
                    .find(|(_, known)| known.str("effects") == Some(effects))
            })
            .or_else(|| self.types.first())
            .map(|(name, _)| name.as_str())
            .ok_or_else(|| invalid("dimension codec without dimension types"))
    }

    /// The name of the type whose element is exactly `element`.
    pub(super) fn exact(&self, element: &Compound) -> Option<&str> {
        self.types
            .iter()
            .find(|(_, known)| same_compound(known, element))
            .map(|(name, _)| name.as_str())
    }

    /// The types of a 1.16.2 codec.
    pub(super) fn from_registry(codec: &Compound) -> io::Result<Self> {
        let mut types = Vec::new();
        for entry in entries(codec, DIMENSION_TYPE)? {
            let entry = entry
                .as_compound()
                .ok_or_else(|| invalid("dimension type entry is not a compound"))?;
            let name = entry
                .str("name")
                .ok_or_else(|| invalid("dimension type without a name"))?;
            let element = entry
                .compound("element")
                .ok_or_else(|| invalid("dimension type without an element"))?;
            types.push((name.to_owned(), element.clone()));
        }
        Ok(Self { types })
    }

    /// Add a type for an element the codec lacks (a login's own dimension
    /// that no registry entry describes) and return its name.
    pub(super) fn add(&mut self, element: Compound) -> String {
        let name = format!("rift:dimension_{}", self.types.len());
        self.types.push((name.clone(), element));
        name
    }
}

/// The special effects 1.16.1 gave a dimension type by its name.
fn effects(name: &str) -> &str {
    match name {
        "minecraft:the_nether" | "minecraft:the_end" => name,
        _ => OVERWORLD,
    }
}

/// A 1.16.1 codec (`{dimension: [type...]}`, each type with its `name` and
/// `shrunk`) as the 1.16.2 codec: the types as a registry with `effects` and
/// `coordinate_scale`, and vanilla's biomes.
pub(super) fn upgrade(codec: &Nbt) -> io::Result<(Compound, Types)> {
    let list = codec
        .as_compound()
        .and_then(|codec| codec.list("dimension"))
        .ok_or_else(|| invalid("dimension codec without dimensions"))?;
    let mut types = Types::default();
    let mut entries = Vec::with_capacity(list.len());
    for (id, old) in list.iter().enumerate() {
        let old = old
            .as_compound()
            .ok_or_else(|| invalid("dimension type is not a compound"))?;
        let name = old
            .str("name")
            .ok_or_else(|| invalid("dimension type without a name"))?
            .to_owned();
        // In vanilla's key order: effects after bed_works, the scale where
        // `shrunk` was.
        let mut element = Compound::new();
        for (key, value) in old.iter() {
            match key {
                "name" => {}
                "shrunk" => {
                    let shrunk = value.as_i64().is_some_and(|value| value != 0);
                    element.insert("coordinate_scale", if shrunk { 8.0 } else { 1.0 });
                }
                _ => {
                    element.insert(key, value.clone());
                    if key == "bed_works" {
                        element.insert("effects", effects(&name));
                    }
                }
            }
        }
        if !element.contains("effects") {
            element.insert("effects", effects(&name));
        }
        if !element.contains("coordinate_scale") {
            element.insert("coordinate_scale", 1.0);
        }
        let mut entry = Compound::new();
        entry.insert("name", name.as_str());
        entry.insert("id", id as i32);
        entry.insert("element", element.clone());
        entries.push(Nbt::Compound(entry));
        types.types.push((name, element));
    }
    let mut registry = Compound::new();
    registry.insert("type", DIMENSION_TYPE);
    registry.insert("value", entries);
    let biomes = vanilla()
        .get(BIOME)
        .cloned()
        .ok_or_else(|| invalid("embedded codec without biomes"))?;
    let mut upgraded = Compound::new();
    upgraded.insert(DIMENSION_TYPE, registry);
    upgraded.insert(BIOME, biomes);
    Ok((upgraded, types))
}

/// The 1.16.1 codec listing `types`. Biomes are not part of it: 1.16.1
/// clients number them as vanilla does.
///
/// INTEGRATION: the chunk codec needs the backend's biome registry
/// (`minecraft:worldgen/biome` entries, `name` and `id`) to give 1.16.1
/// clients only biome IDs they know; this step does not keep it.
pub(super) fn list(types: &Types) -> Nbt {
    let mut list = Vec::with_capacity(types.types.len());
    for (name, element) in &types.types {
        // In vanilla's key order: the name before logical_height, `shrunk`
        // where the scale was.
        let mut old = Compound::new();
        for (key, value) in element.iter() {
            match key {
                "effects" => {}
                "coordinate_scale" => {
                    let shrunk = value.as_f64().is_some_and(|scale| scale > 1.0);
                    old.insert("shrunk", shrunk);
                }
                _ => {
                    if key == "logical_height" {
                        old.insert("name", name.as_str());
                    }
                    old.insert(key, value.clone());
                }
            }
        }
        if !old.contains("name") {
            old.insert("name", name.as_str());
        }
        if !old.contains("shrunk") {
            old.insert("shrunk", false);
        }
        list.push(Nbt::Compound(old));
    }
    let mut codec = Compound::new();
    codec.insert("dimension", list);
    Nbt::Compound(codec)
}

#[cfg(test)]
pub(super) fn vanilla_codec() -> &'static Compound {
    vanilla()
}
