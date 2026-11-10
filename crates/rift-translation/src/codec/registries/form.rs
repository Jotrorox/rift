//! How each protocol puts synchronised registries on the wire, and readers
//! and writers for every form: the 1.16 dimension list, the 1.16.2–1.20.4
//! registry codec (in `login`, then in one configuration `registry_data`),
//! and the per-registry `registry_data` packets of 1.20.5 on.
use crate::{
    nbt::{Compound, Nbt},
    wire::{Reader, Writer, invalid},
};
use std::io;

/// One entry of a synchronised registry as a packet carries it.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The entry's name as sent, normally namespaced (`minecraft:plains`).
    pub name: String,
    /// The network ID: explicit in a codec (and sparse in 1.16.2–1.17.1
    /// biomes), the position in a 1.20.5+ packet.
    pub id: i32,
    /// `None` in a 1.20.5+ packet for an entry the client takes from a pack
    /// it said it knows. Nearly always a compound; 26.3 block transformers
    /// are lists.
    pub element: Option<Nbt>,
}

/// One synchronised registry.
#[derive(Clone, Debug, PartialEq)]
pub struct Registry {
    /// Without the `minecraft:` namespace, as `crate::data` names registries:
    /// `dimension_type`, `worldgen/biome`, `chat_type`...
    pub name: String,
    pub entries: Vec<Entry>,
}

impl Registry {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entries: Vec::new(),
        }
    }

    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|entry| same_name(&entry.name, name))
    }

    /// The entry with network ID `id`.
    pub fn by_id(&self, id: i32) -> Option<&Entry> {
        match self.entries.get(usize::try_from(id).ok()?) {
            Some(entry) if entry.id == id => Some(entry),
            _ => self.entries.iter().find(|entry| entry.id == id),
        }
    }

    /// Renumber entries 0, 1, 2... in their order, as 1.20.5+ packets imply.
    pub fn renumber(&mut self) {
        for (index, entry) in self.entries.iter_mut().enumerate() {
            entry.id = index as i32;
        }
    }
}

/// Resource locations compare without the default namespace.
pub fn same_name(a: &str, b: &str) -> bool {
    crate::data::strip(a) == crate::data::strip(b)
}

/// `name` with the default namespace when it has none, as the game writes
/// registry and entry names.
pub fn namespaced(name: &str) -> String {
    if name.contains(':') {
        name.to_owned()
    } else {
        format!("minecraft:{name}")
    }
}

/// How a protocol synchronises registries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sync {
    /// Before 1.16: nothing; the dimension is a number (-1, 0, 1).
    None,
    /// 1.16 and 1.16.1: a list of dimension types in `login`.
    DimensionList,
    /// 1.16.2 to 1.20.1: a registry codec in `login`.
    LoginCodec,
    /// 1.20.2 to 1.20.4: the same codec in one configuration `registry_data`.
    ConfigurationCodec,
    /// 1.20.5 on: one configuration `registry_data` per registry.
    Packets,
}

pub fn sync(protocol: i32) -> Sync {
    match protocol {
        ..735 => Sync::None,
        735..751 => Sync::DimensionList,
        751..764 => Sync::LoginCodec,
        764..766 => Sync::ConfigurationCodec,
        _ => Sync::Packets,
    }
}

/// Whether `protocol` sends registries as one codec NBT (1.16 to 1.20.4).
pub fn has_codec(protocol: i32) -> bool {
    matches!(
        sync(protocol),
        Sync::DimensionList | Sync::LoginCodec | Sync::ConfigurationCodec
    )
}

fn compound_of(value: &Nbt) -> io::Result<&Compound> {
    value
        .as_compound()
        .ok_or_else(|| invalid("registry codec entry is not a compound"))
}

fn exactly(compound: &Compound, keys: &[&str]) -> io::Result<()> {
    if compound.len() != keys.len() || keys.iter().any(|key| !compound.contains(key)) {
        return Err(invalid("registry codec entry has unexpected fields"));
    }
    Ok(())
}

/// The registries in a codec of `protocol` (the dimension list for 1.16 and
/// 1.16.1, whose elements keep their `name` field).
pub fn read_codec(protocol: i32, codec: &Nbt) -> io::Result<Vec<Registry>> {
    let root = compound_of(codec)?;
    if sync(protocol) == Sync::DimensionList {
        exactly(root, &["dimension"])?;
        let list = root
            .list("dimension")
            .ok_or_else(|| invalid("dimension list is not a list"))?;
        let mut registry = Registry::new("dimension_type");
        for (index, element) in list.iter().enumerate() {
            let element = compound_of(element)?;
            let name = element
                .str("name")
                .ok_or_else(|| invalid("dimension without a name"))?;
            registry.entries.push(Entry {
                name: name.to_owned(),
                id: index as i32,
                element: Some(Nbt::Compound(element.clone())),
            });
        }
        return Ok(vec![registry]);
    }
    let mut registries = Vec::with_capacity(root.len());
    for (key, value) in root.iter() {
        let body = compound_of(value)?;
        exactly(body, &["type", "value"])?;
        if body.str("type") != Some(key) {
            return Err(invalid("registry codec type does not match its key"));
        }
        let list = body
            .list("value")
            .ok_or_else(|| invalid("registry codec value is not a list"))?;
        let mut registry = Registry::new(crate::data::strip(key));
        for entry in list {
            let entry = compound_of(entry)?;
            exactly(entry, &["name", "id", "element"])?;
            let (Some(name), Some(Nbt::Int(id)), Some(element)) =
                (entry.str("name"), entry.get("id"), entry.get("element"))
            else {
                return Err(invalid("malformed registry codec entry"));
            };
            registry.entries.push(Entry {
                name: name.to_owned(),
                id: *id,
                element: Some(element.clone()),
            });
        }
        registries.push(registry);
    }
    Ok(registries)
}

/// The codec of `protocol` holding `registries`. Entries need elements.
pub fn write_codec(protocol: i32, registries: &[Registry]) -> io::Result<Nbt> {
    let element = |entry: &Entry| {
        entry
            .element
            .clone()
            .ok_or_else(|| invalid("a registry codec entry needs its element"))
    };
    let mut root = Compound::new();
    if sync(protocol) == Sync::DimensionList {
        let mut list = Vec::new();
        for registry in registries {
            if registry.name == "dimension_type" {
                for entry in &registry.entries {
                    list.push(element(entry)?);
                }
            }
        }
        root.insert("dimension", list);
        return Ok(Nbt::Compound(root));
    }
    for registry in registries {
        let key = namespaced(&registry.name);
        let mut entries = Vec::with_capacity(registry.entries.len());
        for entry in &registry.entries {
            let mut wrapped = Compound::new();
            wrapped.insert("name", entry.name.as_str());
            wrapped.insert("id", entry.id);
            wrapped.insert("element", element(entry)?);
            entries.push(Nbt::Compound(wrapped));
        }
        let mut body = Compound::new();
        body.insert("type", key.as_str());
        body.insert("value", entries);
        root.insert(key, body);
    }
    Ok(Nbt::Compound(root))
}

/// The registry in one 1.20.5+ `registry_data` packet body.
pub fn read_packet(body: &[u8]) -> io::Result<Registry> {
    let mut reader = Reader::new(body);
    let name = reader.string(32767)?;
    let count = reader.count()?;
    let mut registry = Registry::new(crate::data::strip(name));
    registry.entries.reserve(count.min(reader.remaining()));
    for id in 0..count {
        let name = reader.string(32767)?.to_owned();
        let element = if reader.bool()? {
            match Nbt::read(&mut reader, false)? {
                Some(element) => Some(element),
                None => return Err(invalid("registry entry data is empty")),
            }
        } else {
            None
        };
        registry.entries.push(Entry {
            name,
            id: id as i32,
            element,
        });
    }
    reader.finish()?;
    Ok(registry)
}

/// A 1.20.5+ `registry_data` packet body. Entry IDs are their positions.
pub fn write_packet(registry: &Registry) -> Vec<u8> {
    let mut writer = Writer::new();
    writer
        .string(&namespaced(&registry.name))
        .varint(registry.entries.len() as i32);
    for entry in &registry.entries {
        writer.string(&entry.name);
        match &entry.element {
            Some(element) => {
                writer.bool(true);
                element.write(&mut writer, false);
            }
            None => {
                writer.bool(false);
            }
        }
    }
    writer.into_inner()
}

/// The body of a 1.20.2–1.20.4 `registry_data` packet: the codec, nameless.
pub fn read_codec_packet(protocol: i32, body: &[u8]) -> io::Result<Vec<Registry>> {
    match Nbt::from_bytes(body, false)? {
        Some(codec) => read_codec(protocol, &codec),
        None => Err(invalid("registry_data without a codec")),
    }
}

pub fn write_codec_packet(protocol: i32, registries: &[Registry]) -> io::Result<Vec<u8>> {
    Ok(write_codec(protocol, registries)?.to_bytes(false))
}
