//! Registry ID mappings between adjacent protocol versions, derived from the
//! per-version registries by name, plus each step's rename and stand-in rules.
use crate::data;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

/// Source network ID to target network ID for one registry.
#[derive(Debug, Default)]
pub struct Mapping {
    table: Vec<i32>,
}

impl Mapping {
    pub fn new(table: Vec<i32>) -> Self {
        Self { table }
    }

    /// The target ID, or `None` when the source ID is out of range or has no
    /// equivalent and no stand-in.
    pub fn get(&self, id: i32) -> Option<i32> {
        let mapped = *self.table.get(usize::try_from(id).ok()?)?;
        (mapped >= 0).then_some(mapped)
    }

    /// The target ID, or `fallback` when there is none.
    pub fn get_or(&self, id: i32, fallback: i32) -> i32 {
        self.get(id).unwrap_or(fallback)
    }

    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    /// Whether every ID maps to itself, so rewriting can be skipped.
    pub fn is_identity(&self) -> bool {
        self.table
            .iter()
            .enumerate()
            .all(|(id, mapped)| id as i32 == *mapped)
    }
}

/// The mapping of `registry` from protocol `from` to the adjacent protocol
/// `to`, built on first use. `registry` is a registry name such as `"item"`,
/// or `"block_state"`.
pub fn mapping(from: i32, to: i32, registry: &str) -> &'static Mapping {
    type Key = (i32, i32, String);
    static CACHE: OnceLock<Mutex<HashMap<Key, &'static Mapping>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    cache
        .entry((from, to, registry.to_owned()))
        .or_insert_with(|| Box::leak(Box::new(build(from, to, registry))))
}

fn build(from: i32, to: i32, registry: &str) -> Mapping {
    let (source, target) = (data::version(from), data::version(to));
    let (source, target) = (
        source.expect_registry(registry),
        target.expect_registry(registry),
    );
    Mapping::new(
        (0..source.len() as i32)
            .map(|id| {
                source
                    .name(id)
                    .and_then(|name| target.id(name))
                    .unwrap_or(-1)
            })
            .collect(),
    )
}
