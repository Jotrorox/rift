//! Registry contents from one protocol to an adjacent one.
//!
//! An entry that is exactly the sender's vanilla entry becomes the
//! receiver's vanilla entry of the same name, which covers every field
//! change for the common case. Anything else (a datapack's own dimension or
//! biome, a modified vanilla entry, a vanilla entry the receiver's version
//! lacks) is converted field by field: the format changes of each version
//! boundary are applied, then the element is conformed to the receiver's
//! vanilla entries (see [`conform`]), so it parses even where a change has
//! no dedicated rule.
//!
//! The receiver gets the sender's entries in the sender's order, then every
//! vanilla entry of its own version the sender lacks (a vanilla client takes
//! some for granted, such as damage types and default mob variants). A
//! registry the receiver does not synchronise is dropped; one it expects but
//! the sender's version does not send is the receiver's vanilla registry.
use super::{
    defaults,
    form::{Registry, has_codec},
    transform,
};
use crate::{
    nbt::{Compound, Nbt},
    wire::invalid,
};
use std::{
    collections::HashMap,
    io,
    sync::{Mutex, OnceLock},
};

/// One sender registry for the adjacent protocol `to`, or `None` when `to`
/// does not synchronise it. Entries without data (1.20.5+ known packs) take
/// the sender version's vanilla element.
pub fn registry(from: i32, to: i32, sent: &Registry) -> io::Result<Option<Registry>> {
    if !defaults::syncs(to, &sent.name) {
        return Ok(None);
    }
    let mut entries = sent.entries.clone();
    if has_codec(from) && !has_codec(to) {
        // Explicit IDs become positions.
        entries.sort_by_key(|entry| entry.id);
    }
    for entry in &mut entries {
        let element = match entry.element.take() {
            Some(element) => element,
            None => defaults::tag(from, &sent.name, &entry.name)
                .cloned()
                .ok_or_else(|| {
                    invalid(&format!(
                        "{} {} has no data and is not vanilla in {}",
                        sent.name,
                        entry.name,
                        crate::version::release(from)
                    ))
                })?,
        };
        entry.element = Some(element_for(&sent.name, from, to, &entry.name, element)?);
    }
    entries.retain(|entry| {
        entry
            .element
            .as_ref()
            .is_none_or(|element| !names_unknown_item(&sent.name, to, element))
    });
    let mut registry = Registry {
        name: sent.name.clone(),
        entries,
    };
    append_missing(to, &mut registry);
    if !has_codec(to) {
        registry.renumber();
    }
    Ok(Some(registry))
}

/// The receiver's vanilla registries that the sender's version does not
/// synchronise: what to send in addition to everything converted.
pub fn missing(from: i32, to: i32) -> Vec<Registry> {
    defaults::all(to)
        .iter()
        .filter(|defaults| !defaults::syncs(from, defaults.name))
        .map(defaults::Defaults::to_registry)
        .collect()
}

/// Everything a sender synchronised (a whole codec) for the receiver, in
/// the receiver's vanilla registry order, with what it lacks filled in.
pub fn registries(from: i32, to: i32, sent: &[Registry]) -> io::Result<Vec<Registry>> {
    let mut converted = Vec::new();
    for sent in sent {
        if let Some(converted_one) = registry(from, to, sent)? {
            converted.push(converted_one);
        }
    }
    Ok(assemble(to, converted))
}

/// Converted registries in `to`'s vanilla registry order, with `to`'s
/// vanilla registry for each one missing: a complete set for a codec.
pub fn assemble(to: i32, mut converted: Vec<Registry>) -> Vec<Registry> {
    let mut result = Vec::new();
    for defaults in defaults::all(to) {
        match converted
            .iter()
            .position(|registry| registry.name == defaults.name)
        {
            Some(index) => result.push(converted.swap_remove(index)),
            None => result.push(defaults.to_registry()),
        }
    }
    result
}

/// Add the vanilla entries of `to` the registry lacks, after the others.
fn append_missing(to: i32, registry: &mut Registry) {
    let Some(defaults) = defaults::get(to, &registry.name) else {
        return;
    };
    let mut next = registry
        .entries
        .iter()
        .map(|entry| entry.id + 1)
        .max()
        .unwrap_or(0);
    for vanilla in &defaults.entries {
        if registry.entry(&vanilla.name).is_none() {
            registry.entries.push(super::form::Entry {
                name: vanilla.name.to_string(),
                id: next,
                element: Some((*vanilla.element).clone()),
            });
            next += 1;
        }
    }
}

/// One element of `registry` from `from` to the adjacent `to`.
pub fn element_for(
    registry: &str,
    from: i32,
    to: i32,
    name: &str,
    element: Nbt,
) -> io::Result<Nbt> {
    if from == to {
        return Ok(element);
    }
    if let Some(sender) = defaults::get(from, registry)
        && sender.is_vanilla(name, &element)
        && let Some(vanilla) = defaults::tag(to, registry, name)
    {
        let mut vanilla = vanilla.clone();
        if registry == "dimension_type" {
            keep_extent(&element, &mut vanilla, to);
        }
        return Ok(vanilla);
    }
    let mut element = element;
    // INTEGRATION: text components inside datapack entries (descriptions of
    // trims, enchantments, jukebox songs and instruments, painting titles,
    // dialog texts) pass unconverted. Vanilla's only use `translate` and
    // `color`, which every version reads alike; once `text::convert_nbt`
    // works, the registry_data handlers should run it on those fields of
    // non-vanilla entries when both protocols are 1.20.3+ (it needs
    // `&mut Shared`, which this pure function does not have).
    if let Nbt::Compound(compound) = &mut element {
        transform::apply(registry, from, to, name, compound)?;
        conform(registry, to, name, compound);
    }
    Ok(element)
}

/// Fields of registry elements that name an item, by registry.
const ITEM_FIELDS: &[(&str, &str)] = &[
    ("trim_material", "ingredient"),
    ("trim_pattern", "template_item"),
];

/// Whether `element` names an item `protocol` lacks (the 1.21.4 resin trim
/// for older clients): a client rejects the whole registry over one, so
/// such entries are left out.
fn names_unknown_item(registry: &str, protocol: i32, element: &Nbt) -> bool {
    let Some(items) = crate::data::version(protocol).registry("item") else {
        return false;
    };
    ITEM_FIELDS
        .iter()
        .filter(|(name, _)| *name == registry)
        .filter_map(|(_, key)| element.as_compound()?.get(key)?.as_str())
        .any(|item| items.id(crate::data::strip(item)).is_none())
}

/// The backend's chunks span its dimension's height, so a client that can
/// show any height (1.17 on) keeps the sender's extent even where its own
/// vanilla type differs: 1.18's overworld runs from -64 to 320, 1.17's
/// from 0 to 256.
fn keep_extent(sent: &Nbt, vanilla: &mut Nbt, to: i32) {
    if to < 755 {
        return;
    }
    let (Some(sent), Some(vanilla)) = (sent.as_compound(), vanilla.as_compound_mut()) else {
        return;
    };
    for key in ["min_y", "height", "logical_height"] {
        if let Some(value) = sent.get(key) {
            vanilla.insert(key, value.clone());
        }
    }
}

/// Fields of a registry's elements as the vanilla entries of one protocol
/// have them: which NBT kinds each takes, whether every entry has it, and
/// for a few nested structures the same one level down.
#[derive(Debug, Default)]
pub struct Schema {
    fields: Vec<(String, Field)>,
}

#[derive(Debug, Default)]
struct Field {
    /// Bit `kind` set for every NBT kind seen.
    kinds: u16,
    /// Every vanilla entry has it.
    required: bool,
    nested: Option<Schema>,
}

/// Registries whose elements are records with fixed fields, and the nested
/// records inside them. Others (dialogs, timelines, test definitions, block
/// transformers...) are dispatched by a type field or are maps, where the
/// vanilla entries do not show every legal field; they are left as sent.
const RECORDS: &[(&str, &[&str])] = &[
    ("dimension_type", &[]),
    ("worldgen/biome", &["effects"]),
    ("chat_type", &["chat", "narration"]),
    ("damage_type", &[]),
    ("trim_pattern", &[]),
    ("trim_material", &[]),
    ("banner_pattern", &[]),
    ("wolf_variant", &["assets", "baby_assets"]),
    ("wolf_sound_variant", &["adult_sounds", "baby_sounds"]),
    ("pig_variant", &[]),
    ("pig_sound_variant", &["adult_sounds", "baby_sounds"]),
    ("cat_variant", &[]),
    ("cat_sound_variant", &["adult_sounds", "baby_sounds"]),
    ("cow_variant", &[]),
    ("cow_sound_variant", &[]),
    ("chicken_variant", &[]),
    ("chicken_sound_variant", &["adult_sounds", "baby_sounds"]),
    ("frog_variant", &[]),
    ("zombie_nautilus_variant", &[]),
    ("painting_variant", &[]),
    ("enchantment", &[]),
    ("jukebox_song", &[]),
    ("instrument", &[]),
    ("decorated_pot_pattern", &[]),
];

/// The schema of `registry` in `protocol`, or `None` when it is not a
/// record (see [`RECORDS`]) or not synchronised.
pub fn schema(protocol: i32, registry: &str) -> Option<&'static Schema> {
    type Cache = HashMap<(i32, String), Option<&'static Schema>>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    *cache
        .entry((protocol, registry.to_owned()))
        .or_insert_with(|| {
            let (_, nested) = RECORDS.iter().find(|(name, _)| *name == registry)?;
            let defaults = defaults::get(protocol, registry)?;
            if defaults.entries.is_empty() {
                return None;
            }
            let elements: Vec<&Compound> = defaults
                .entries
                .iter()
                .filter_map(|entry| entry.element.as_compound())
                .collect();
            Some(&*Box::leak(Box::new(derive(&elements, nested))))
        })
}

fn derive(elements: &[&Compound], nested: &[&str]) -> Schema {
    let mut schema = Schema::default();
    for element in elements {
        for (key, value) in element.iter() {
            let index = match schema.fields.iter().position(|(known, _)| known == key) {
                Some(index) => index,
                None => {
                    schema.fields.push((key.to_owned(), Field::default()));
                    schema.fields.len() - 1
                }
            };
            schema.fields[index].1.kinds |= 1 << value.kind();
        }
    }
    for (key, field) in &mut schema.fields {
        field.required = elements.iter().all(|element| element.contains(key));
        if nested.contains(&key.as_str()) {
            let inner: Vec<&Compound> = elements
                .iter()
                .filter_map(|element| element.compound(key))
                .collect();
            if !inner.is_empty() {
                field.nested = Some(derive(&inner, &[]));
            }
        }
    }
    schema
}

/// Make `element` parse as `registry` in `protocol`: drop fields the
/// protocol's vanilla entries never have, drop or replace fields of a kind
/// they never use, and fill fields every vanilla entry has from the most
/// similar vanilla entry. Registries that are not records are untouched.
pub fn conform(registry: &str, protocol: i32, name: &str, element: &mut Compound) {
    let Some(schema) = schema(protocol, registry) else {
        return;
    };
    let template = defaults::template(
        protocol,
        registry,
        name,
        element,
        transform::template_keys(registry),
    );
    conform_with(schema, template, element);
}

fn conform_with(schema: &Schema, template: Option<&Compound>, element: &mut Compound) {
    element.0.retain(|(key, value)| {
        schema
            .fields
            .iter()
            .find(|(known, _)| known == key)
            .is_some_and(|(_, field)| field.kinds & (1 << value.kind()) != 0 || field.required)
    });
    for (key, field) in &schema.fields {
        let from_template = template.and_then(|template| template.get(key));
        match element.get_mut(key) {
            Some(value) if field.kinds & (1 << value.kind()) == 0 => match from_template {
                Some(replacement) => *value = replacement.clone(),
                None => drop(element.remove(key)),
            },
            Some(Nbt::Compound(inner)) => {
                if let Some(nested) = &field.nested {
                    let inner_template = from_template.and_then(Nbt::as_compound);
                    conform_with(nested, inner_template, inner);
                }
            }
            Some(_) => {}
            None if field.required => {
                if let Some(value) = from_template {
                    element.insert(key.clone(), value.clone());
                }
            }
            None => {}
        }
    }
}

/// Whether `element` has every field the protocol requires with a kind it
/// accepts and nothing it never has, for tests and diagnostics: the
/// problems, empty when it conforms.
pub fn problems(registry: &str, protocol: i32, element: &Compound) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(schema) = schema(protocol, registry) {
        check(schema, element, "", &mut problems);
    }
    problems
}

fn check(schema: &Schema, element: &Compound, path: &str, problems: &mut Vec<String>) {
    for (key, value) in element.iter() {
        match schema.fields.iter().find(|(known, _)| known == key) {
            None => problems.push(format!("{path}{key} is unknown")),
            Some((_, field)) => {
                if field.kinds & (1 << value.kind()) == 0 {
                    problems.push(format!("{path}{key} has kind {}", value.kind()));
                } else if let (Some(nested), Nbt::Compound(inner)) = (&field.nested, value) {
                    check(nested, inner, &format!("{path}{key}."), problems);
                }
            }
        }
    }
    for (key, field) in &schema.fields {
        if field.required && !element.contains(key) {
            problems.push(format!("{path}{key} is missing"));
        }
    }
}
