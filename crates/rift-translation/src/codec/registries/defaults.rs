//! The vanilla contents of every synchronised registry per protocol, as the
//! official server of that version sends them (generated into
//! `data/registries/` by `scripts/translation_registries.py`).
use super::{
    form::{Entry, Registry},
    snbt,
};
use crate::{
    data,
    nbt::{Compound, Nbt},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

/// One vanilla entry. Elements are shared between the versions that send
/// the same one.
#[derive(Debug)]
pub struct Vanilla {
    pub name: Box<str>,
    pub id: i32,
    pub element: Arc<Nbt>,
}

/// One vanilla registry of one protocol, in network-ID order.
#[derive(Debug)]
pub struct Defaults {
    pub name: &'static str,
    pub entries: Vec<Vanilla>,
    by_name: HashMap<Box<str>, usize>,
}

impl Defaults {
    /// The vanilla entry `name`, with or without its namespace.
    pub fn get(&self, name: &str) -> Option<&Vanilla> {
        self.by_name
            .get(data::strip(name))
            .map(|index| &self.entries[*index])
    }

    /// The vanilla entry with network ID `id`.
    pub fn by_id(&self, id: i32) -> Option<&Vanilla> {
        match self.entries.get(usize::try_from(id).ok()?) {
            Some(entry) if entry.id == id => Some(entry),
            _ => self.entries.iter().find(|entry| entry.id == id),
        }
    }

    /// Whether `element` is exactly the vanilla element of `name`.
    pub fn is_vanilla(&self, name: &str, element: &Nbt) -> bool {
        self.get(name)
            .is_some_and(|entry| *entry.element == *element)
    }

    /// The registry as a packet would carry it, every entry with its data.
    pub fn to_registry(&self) -> Registry {
        Registry {
            name: self.name.to_owned(),
            entries: self
                .entries
                .iter()
                .map(|entry| Entry {
                    name: entry.name.to_string(),
                    id: entry.id,
                    element: Some((*entry.element).clone()),
                })
                .collect(),
        }
    }
}

/// Every registry `protocol` synchronises, in the order its server sends
/// them; empty before 1.16.
pub fn all(protocol: i32) -> &'static [Defaults] {
    static CACHE: OnceLock<Mutex<HashMap<i32, &'static [Defaults]>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    cache
        .entry(protocol)
        .or_insert_with(|| Box::leak(load(protocol).into_boxed_slice()))
}

/// The vanilla contents of `registry` in `protocol`, if it synchronises it.
pub fn get(protocol: i32, registry: &str) -> Option<&'static Defaults> {
    let registry = data::strip(registry);
    all(protocol).iter().find(|known| known.name == registry)
}

/// Whether `protocol` synchronises `registry`.
pub fn syncs(protocol: i32, registry: &str) -> bool {
    get(protocol, registry).is_some()
}

/// The registries a client of `protocol` needs, with full data.
pub fn registries(protocol: i32) -> Vec<Registry> {
    all(protocol).iter().map(Defaults::to_registry).collect()
}

/// The registry names of `protocol` (`registries/index.json`).
fn names(protocol: i32) -> Vec<&'static str> {
    static INDEX: OnceLock<HashMap<i32, Vec<&'static str>>> = OnceLock::new();
    INDEX
        .get_or_init(|| {
            let index = data::file("registries/index").expect("registries/index.json exists");
            index["protocols"]
                .as_object()
                .expect("index lists protocols")
                .iter()
                .map(|(protocol, names)| {
                    let names = names
                        .as_array()
                        .expect("index lists registry names")
                        .iter()
                        .map(|name| -> &'static str {
                            Box::leak(name.as_str().expect("registry name").into())
                        })
                        .collect();
                    (protocol.parse().expect("protocol number"), names)
                })
                .collect()
        })
        .get(&protocol)
        .cloned()
        .unwrap_or_default()
}

/// A registry file's element pool, parsed once.
fn pool(registry: &str, file: &'static Value) -> &'static [Arc<Nbt>] {
    type Pools = HashMap<String, &'static [Arc<Nbt>]>;
    static POOLS: OnceLock<Mutex<Pools>> = OnceLock::new();
    let mut pools = POOLS.get_or_init(Default::default).lock().unwrap();
    pools.entry(registry.to_owned()).or_insert_with(|| {
        let parsed: Vec<Arc<Nbt>> = file["entries"]
            .as_array()
            .expect("registry file has entries")
            .iter()
            .map(|text| {
                let text = text.as_str().expect("registry entry is SNBT text");
                Arc::new(snbt::parse(text).unwrap_or_else(|error| {
                    panic!("registries/{registry}: invalid SNBT {text}: {error}")
                }))
            })
            .collect();
        Box::leak(parsed.into_boxed_slice())
    })
}

fn load(protocol: i32) -> Vec<Defaults> {
    names(protocol)
        .into_iter()
        .map(|name| {
            let file = data::file(&format!("registries/{name}"))
                .unwrap_or_else(|| panic!("registries/{name}.json exists"));
            let pool = pool(name, file);
            let run = file["versions"]
                .as_array()
                .expect("registry file has versions")
                .iter()
                .find(|run| {
                    run["protocols"]
                        .as_array()
                        .is_some_and(|protocols| protocols.iter().any(|p| p == protocol))
                })
                .unwrap_or_else(|| panic!("registries/{name} has no run for {protocol}"));
            let strings = |key: &str| -> Vec<&str> {
                run[key]
                    .as_array()
                    .map(|values| values.iter().map(|value| value.as_str().unwrap()).collect())
                    .unwrap_or_default()
            };
            let numbers = |key: &str| -> Option<Vec<i64>> {
                run[key]
                    .as_array()
                    .map(|values| values.iter().map(|value| value.as_i64().unwrap()).collect())
            };
            let names = strings("names");
            let elements = numbers("elements").expect("registry run has elements");
            let ids = numbers("ids");
            let entries: Vec<Vanilla> = names
                .iter()
                .enumerate()
                .map(|(index, entry)| Vanilla {
                    name: (*entry).into(),
                    id: ids.as_ref().map_or(index as i32, |ids| ids[index] as i32),
                    element: pool[elements[index] as usize].clone(),
                })
                .collect();
            let by_name = entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (data::strip(&entry.name).into(), index))
                .collect();
            Defaults {
                name,
                entries,
                by_name,
            }
        })
        .collect()
}

/// The vanilla element of `name` in `registry` for `protocol`.
/// A compound element; see [`tag`] for any element.
pub fn element(protocol: i32, registry: &str, name: &str) -> Option<&'static Compound> {
    tag(protocol, registry, name)?.as_compound()
}

/// The vanilla element of `name` in `registry` for `protocol`, any tag.
pub fn tag(protocol: i32, registry: &str, name: &str) -> Option<&'static Nbt> {
    Some(&get(protocol, registry)?.get(name)?.element)
}

/// The vanilla entry of `registry` in `protocol` that is most like
/// `element`, to fill in fields a conversion cannot derive: the entry of the
/// same name, else the first entry whose `keys` all match, else the first.
pub fn template(
    protocol: i32,
    registry: &str,
    name: &str,
    element: &Compound,
    keys: &[&str],
) -> Option<&'static Compound> {
    let defaults = get(protocol, registry)?;
    if let Some(element) = defaults
        .get(name)
        .and_then(|entry| entry.element.as_compound())
    {
        return Some(element);
    }
    if !keys.is_empty() {
        // Only the keys this element has; at least one must.
        let present: Vec<&&str> = keys.iter().filter(|key| element.contains(key)).collect();
        if !present.is_empty() {
            let matching = defaults.entries.iter().find_map(|entry| {
                let candidate = entry.element.as_compound()?;
                present
                    .iter()
                    .all(|key| candidate.get(key) == element.get(key))
                    .then_some(candidate)
            });
            if matching.is_some() {
                return matching;
            }
        }
    }
    defaults.entries.first()?.element.as_compound()
}
