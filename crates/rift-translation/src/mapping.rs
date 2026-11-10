//! Registry ID mappings between adjacent protocol versions, derived from the
//! per-version registries by entry name plus each pair's rename and stand-in
//! rules.
//!
//! # Rules
//!
//! `data/rules/<older>-<newer>.json` holds the rules of one adjacent pair,
//! per registry (`"block_state"` for block states):
//!
//! ```json
//! {"item": {"renames": {"scute": "turtle_scute"},
//!           "up": {"removed_in_newer": "newer_stand_in"},
//!           "down": {"added_in_newer": "older_stand_in"},
//!           "up_default": "stone", "down_default": "stone"}}
//! ```
//!
//! * `renames`: an entry of the older version that the newer one calls
//!   differently. Applies in both directions.
//! * `up`: for mappings from the older to the newer version, a stand-in in
//!   the newer version for an older entry it lacks. `down` is the reverse:
//!   older stand-ins for newer entries.
//! * `up_default` / `down_default`: the stand-in for anything else that has
//!   no counterpart. Without one such entries stay unmapped (`None`), which
//!   [`unmapped`] lists and `examples/mapping_report.rs` prints.
//!
//! Keys are in the source version's names, values in the target's. Rename
//! targets must be distinct: express a merge as one rename plus stand-ins.
//!
//! # Mapping an entry
//!
//! For a source entry: its rename if it has one, else the same name if the
//! target has it, else its stand-in, else the default, else unmapped.
//!
//! # Block states
//!
//! In `block_state` rules a name is `block` (every state of the block) or
//! `block[property=value,...]` (the states with those values). A source
//! state picks its target block by the first of:
//!
//! 1. the matching rule (rename or stand-in of this direction) that names
//!    the most properties, if any names properties; ties go to renames, then
//!    to the alphabetically first key;
//! 2. the whole-block rename;
//! 3. the block of the same name, if the target has it;
//! 4. the whole-block stand-in;
//! 5. the default.
//!
//! The target state is then the target block's state where every property
//! takes the value the rule's target names, else the source state's value
//! for a property of the same name if the target property has that value,
//! else the target block's default state's value. That is the state of the
//! target block matching the most source properties, ties broken towards
//! the default state, so stairs keep their facing, half and waterlogging.
//!
//! Before 1.13 block states are numeric (`id << 4 | metadata`) and stay
//! put within 1.12.x. Between 1.12.2 and 1.13 `block_state`, `block` and
//! `item` go through [`crate::legacy`]; their rules there name 1.12 states
//! `name:metadata` and items `name` or `name:damage`, and renames do not
//! apply.
use crate::{
    data::{self, Block, BlockStates, Registry},
    version,
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Mutex, OnceLock},
};

/// How a source entry found its target, for reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// The same name (and for block states the same property values).
    Identical,
    /// A rename rule.
    Renamed,
    /// A stand-in or default rule.
    StandIn,
    /// A block state of the same block whose properties differ.
    Closest,
    /// No counterpart and no rule.
    Unmapped,
    /// The source has no entry with this ID.
    Gap,
}

/// Source network ID to target network ID for one registry.
#[derive(Debug, Default)]
pub struct Mapping {
    table: Vec<i32>,
    kinds: Vec<Kind>,
}

/// How many source entries mapped which way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub identical: usize,
    pub renamed: usize,
    pub stand_in: usize,
    pub closest: usize,
    pub unmapped: usize,
}

impl Mapping {
    /// A mapping from a plain table: negative entries are unmapped.
    pub fn new(table: Vec<i32>) -> Self {
        let kinds = table
            .iter()
            .enumerate()
            .map(|(id, mapped)| match *mapped {
                mapped if mapped < 0 => Kind::Unmapped,
                mapped if mapped == id as i32 => Kind::Identical,
                _ => Kind::Renamed,
            })
            .collect();
        Self { table, kinds }
    }

    pub(crate) fn from_parts(table: Vec<i32>, kinds: Vec<Kind>) -> Self {
        debug_assert_eq!(table.len(), kinds.len());
        Self { table, kinds }
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

    /// How the source ID found its target; `None` when out of range.
    pub fn kind(&self, id: i32) -> Option<Kind> {
        self.kinds.get(usize::try_from(id).ok()?).copied()
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
            .zip(&self.kinds)
            .enumerate()
            .all(|(id, (mapped, kind))| id as i32 == *mapped || *kind == Kind::Gap)
    }

    pub fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for kind in &self.kinds {
            match kind {
                Kind::Identical => counts.identical += 1,
                Kind::Renamed => counts.renamed += 1,
                Kind::StandIn => counts.stand_in += 1,
                Kind::Closest => counts.closest += 1,
                Kind::Unmapped => counts.unmapped += 1,
                Kind::Gap => {}
            }
        }
        counts
    }

    /// Source IDs that have an entry but no target.
    pub fn unmapped_ids(&self) -> impl Iterator<Item = i32> + '_ {
        self.kinds
            .iter()
            .enumerate()
            .filter(|(_, kind)| **kind == Kind::Unmapped)
            .map(|(id, _)| id as i32)
    }
}

/// The mapping of `registry` from protocol `from` to the adjacent protocol
/// `to`, built on first use. `registry` is a registry name such as `"item"`,
/// or `"block_state"`. Panics when the protocols are not adjacent or either
/// lacks the registry; see [`try_mapping`].
pub fn mapping(from: i32, to: i32, registry: &str) -> &'static Mapping {
    try_mapping(from, to, registry).unwrap_or_else(|| {
        panic!(
            "no {registry} mapping from {} to {}: not adjacent protocols, or one lacks the registry",
            version::release(from),
            version::release(to)
        )
    })
}

/// The mapping of `registry` between adjacent protocols, or `None` when
/// they are not adjacent or either lacks the registry.
pub fn try_mapping(from: i32, to: i32, registry: &str) -> Option<&'static Mapping> {
    type Key = (i32, i32, String);
    static CACHE: OnceLock<Mutex<HashMap<Key, Option<&'static Mapping>>>> = OnceLock::new();
    let registry = data::strip(registry);
    // Built outside the lock: the flattening mapping asks for others.
    if let Some(found) =
        CACHE
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .get(&(from, to, registry.to_owned()))
    {
        return *found;
    }
    let built = build(from, to, registry).map(|mapping| &*Box::leak(Box::new(mapping)));
    *CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry((from, to, registry.to_owned()))
        .or_insert(built)
}

/// Whether `a` and `b` are neighbours in [`version::PROTOCOLS`].
pub fn adjacent(a: i32, b: i32) -> bool {
    match (version::index(a), version::index(b)) {
        (Some(a), Some(b)) => a.abs_diff(b) == 1,
        _ => false,
    }
}

/// The name of entry `id` of `registry` in `protocol`: a registry name, or
/// for `block_state` the `name[property=value,...]` form (legacy states as
/// `name:metadata`).
pub fn entry_name(protocol: i32, registry: &str, id: i32) -> Option<String> {
    let data = data::try_version(protocol)?;
    if data::strip(registry) == "block_state" {
        if version::flattened(protocol) {
            return data.block_states.describe(id);
        }
        let name = data.registry("block")?.name(id >> 4)?;
        return Some(format!("{name}:{}", id & 15));
    }
    data.registry(registry)?.name(id).map(str::to_owned)
}

/// The source entries of a mapping without a target, by ID and name.
pub fn unmapped(from: i32, to: i32, registry: &str) -> Vec<(i32, String)> {
    let Some(mapping) = try_mapping(from, to, registry) else {
        return Vec::new();
    };
    mapping
        .unmapped_ids()
        .map(|id| {
            let name = entry_name(from, registry, id).unwrap_or_else(|| id.to_string());
            (id, name)
        })
        .collect()
}

/// The registries both protocols have, plus `block_state`.
pub fn shared_registries(a: i32, b: i32) -> Vec<String> {
    let (Some(first), Some(second)) = (data::try_version(a), data::try_version(b)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = first
        .registry_names()
        .filter(|name| second.registry(name).is_some())
        .map(str::to_owned)
        .collect();
    names.push("block_state".to_owned());
    names.sort();
    names
}

/// The rules of one registry in a pair's rules file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryRules {
    #[serde(default)]
    pub renames: BTreeMap<String, String>,
    #[serde(default)]
    pub up: BTreeMap<String, String>,
    #[serde(default)]
    pub down: BTreeMap<String, String>,
    #[serde(default)]
    pub up_default: Option<String>,
    #[serde(default)]
    pub down_default: Option<String>,
}

/// Every registry's rules for the pair `older`-`newer`, empty without a
/// rules file. Panics when the file does not parse.
pub fn rules(older: i32, newer: i32) -> &'static HashMap<String, RegistryRules> {
    try_rules(older, newer).unwrap_or_else(|error| panic!("{error}"))
}

fn try_rules(older: i32, newer: i32) -> Result<&'static HashMap<String, RegistryRules>, String> {
    type Parsed = Result<&'static HashMap<String, RegistryRules>, String>;
    static RULES: OnceLock<Mutex<HashMap<(i32, i32), Parsed>>> = OnceLock::new();
    let mut parsed = RULES.get_or_init(Default::default).lock().unwrap();
    parsed
        .entry((older, newer))
        .or_insert_with(|| {
            let stem = format!("rules/{older}-{newer}");
            let Some(text) = data::text(&stem) else {
                return Ok(Box::leak(Box::default()));
            };
            serde_json::from_str::<HashMap<String, RegistryRules>>(&text)
                .map(|rules| &*Box::leak(Box::new(rules)))
                .map_err(|error| format!("data/{stem}.json: {error}"))
        })
        .clone()
}

/// The rules of one direction, in source names: source name to target name.
struct Directed<'a> {
    renames: BTreeMap<&'a str, &'a str>,
    stand_ins: &'a BTreeMap<String, String>,
    default: Option<&'a str>,
}

fn directed(rules: &RegistryRules, up: bool) -> Directed<'_> {
    let renames = if up {
        rules
            .renames
            .iter()
            .map(|(old, new)| (old.as_str(), new.as_str()))
            .collect()
    } else {
        rules
            .renames
            .iter()
            .map(|(old, new)| (new.as_str(), old.as_str()))
            .collect()
    };
    Directed {
        renames,
        stand_ins: if up { &rules.up } else { &rules.down },
        default: if up {
            rules.up_default.as_deref()
        } else {
            rules.down_default.as_deref()
        },
    }
}

fn build(from: i32, to: i32, registry: &str) -> Option<Mapping> {
    if from == to {
        // A protocol maps onto itself unchanged; codecs rely on this when a
        // structure is read and written in one protocol.
        let version = data::try_version(from)?;
        let size = if registry == "block_state" && version::flattened(from) {
            version.block_states.len() as usize
        } else if registry == "block_state" {
            version.registry("block")?.len() * 16
        } else {
            version.registry(registry)?.len()
        };
        return Some(Mapping::new((0..size as i32).collect()));
    }
    if !adjacent(from, to) {
        return None;
    }
    let (source, target) = (data::try_version(from)?, data::try_version(to)?);
    let up = from < to;
    let (older, newer) = if up { (from, to) } else { (to, from) };
    let empty = RegistryRules::default();
    let rules = try_rules(older, newer)
        .ok()?
        .get(registry)
        .unwrap_or(&empty);
    let rules = directed(rules, up);
    if registry == "block_state" {
        return match (version::flattened(from), version::flattened(to)) {
            (true, true) => Some(block_states(
                &source.block_states,
                &target.block_states,
                &rules,
            )),
            (false, false) => {
                // Numeric states stay put within 1.12.x.
                let count = source
                    .registry("block")?
                    .len()
                    .min(target.registry("block")?.len());
                Some(Mapping::new((0..(count as i32) << 4).collect()))
            }
            _ => crate::legacy::block_state_mapping(from, to),
        };
    }
    if version::flattened(from) != version::flattened(to) {
        match registry {
            "item" => return crate::legacy::item_mapping(from, to),
            "block" => return crate::legacy::block_mapping(from, to),
            _ => {}
        }
    }
    Some(by_name(
        source.registry(registry)?,
        target.registry(registry)?,
        &rules,
    ))
}

fn by_name(source: &Registry, target: &Registry, rules: &Directed<'_>) -> Mapping {
    let mut table = Vec::with_capacity(source.len());
    let mut kinds = Vec::with_capacity(source.len());
    for id in 0..source.len() as i32 {
        let Some(name) = source.name(id) else {
            table.push(-1);
            kinds.push(Kind::Gap);
            continue;
        };
        let (mapped, kind) = if let Some(renamed) = rules.renames.get(name) {
            (target.id(renamed), Kind::Renamed)
        } else if let Some(same) = target.id(name) {
            (Some(same), Kind::Identical)
        } else if let Some(stand_in) = rules.stand_ins.get(name) {
            (target.id(stand_in), Kind::StandIn)
        } else if let Some(default) = rules.default {
            (target.id(default), Kind::StandIn)
        } else {
            (None, Kind::Unmapped)
        };
        match mapped {
            Some(mapped) => {
                table.push(mapped);
                kinds.push(kind);
            }
            None => {
                table.push(-1);
                kinds.push(Kind::Unmapped);
            }
        }
    }
    Mapping::from_parts(table, kinds)
}

/// `name` or `name[property=value,...]`, split; `None` when malformed.
pub fn parse_pattern(pattern: &str) -> Option<(&str, Vec<(&str, &str)>)> {
    let (name, properties) = match pattern.split_once('[') {
        Some((name, rest)) => (name, rest.strip_suffix(']')?),
        None => (pattern, ""),
    };
    let properties = properties
        .split(',')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('='))
        .collect::<Option<Vec<_>>>()?;
    (!name.is_empty()).then_some((data::strip(name), properties))
}

struct StateRule<'a> {
    properties: Vec<(&'a str, &'a str)>,
    target: (&'a str, Vec<(&'a str, &'a str)>),
    renamed: bool,
    key: &'a str,
}

fn block_states(source: &BlockStates, target: &BlockStates, rules: &Directed<'_>) -> Mapping {
    let mut by_block: HashMap<&str, Vec<StateRule<'_>>> = HashMap::new();
    let renames = rules
        .renames
        .iter()
        .map(|(key, value)| (*key, *value, true));
    let stand_ins = rules
        .stand_ins
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str(), false));
    for (key, value, renamed) in renames.chain(stand_ins) {
        let (Some((name, properties)), Some(target)) = (parse_pattern(key), parse_pattern(value))
        else {
            continue;
        };
        by_block.entry(name).or_default().push(StateRule {
            properties,
            target,
            renamed,
            key,
        });
    }
    let default = rules.default.and_then(parse_pattern);
    let total = source.len() as usize;
    let mut table = Vec::with_capacity(total);
    let mut kinds = Vec::with_capacity(total);
    let none = Vec::new();
    for block in &source.blocks {
        let block_rules = by_block.get(&*block.name).unwrap_or(&none);
        let whole = |renamed: bool| {
            block_rules
                .iter()
                .find(|rule| rule.properties.is_empty() && rule.renamed == renamed)
        };
        for offset in 0..block.state_count() {
            let values = block.values(offset);
            let specific = block_rules
                .iter()
                .filter(|rule| {
                    !rule.properties.is_empty()
                        && rule.properties.iter().all(|pair| values.contains(pair))
                })
                .min_by(|a, b| {
                    b.properties
                        .len()
                        .cmp(&a.properties.len())
                        .then(b.renamed.cmp(&a.renamed))
                        .then(a.key.cmp(b.key))
                });
            let (chosen, kind) = if let Some(rule) = specific {
                let kind = if rule.renamed {
                    Kind::Renamed
                } else {
                    Kind::StandIn
                };
                (Some((rule.target.0, &rule.target.1[..])), kind)
            } else if let Some(rule) = whole(true) {
                (Some((rule.target.0, &rule.target.1[..])), Kind::Renamed)
            } else if target.block(&block.name).is_some() {
                (Some((&*block.name, &[][..])), Kind::Identical)
            } else if let Some(rule) = whole(false) {
                (Some((rule.target.0, &rule.target.1[..])), Kind::StandIn)
            } else if let Some((name, properties)) = &default {
                (Some((*name, &properties[..])), Kind::StandIn)
            } else {
                (None, Kind::Unmapped)
            };
            let mapped = chosen.and_then(|(name, explicit)| {
                let block = target.block(name)?;
                Some(block.first_state + closest(block, explicit, &values)?)
            });
            match mapped {
                Some(mapped) => {
                    let kind = if kind == Kind::Identical
                        && !same_values(target, mapped, block, &values)
                    {
                        Kind::Closest
                    } else {
                        kind
                    };
                    table.push(mapped);
                    kinds.push(kind);
                }
                None => {
                    table.push(-1);
                    kinds.push(Kind::Unmapped);
                }
            }
        }
    }
    Mapping::from_parts(table, kinds)
}

/// The offset in `block` of the state with the `explicit` values, else the
/// source's value of a same-named property when valid, else the default
/// state's value. `None` when an explicit value is invalid.
pub fn closest(block: &Block, explicit: &[(&str, &str)], source: &[(&str, &str)]) -> Option<i32> {
    let wanted: Vec<(&str, &str)> = block
        .properties
        .iter()
        .filter_map(|(name, values)| {
            if let Some(pair) = explicit.iter().find(|(property, _)| *property == &**name) {
                return Some(*pair);
            }
            source
                .iter()
                .find(|(property, value)| {
                    *property == &**name && values.iter().any(|known| &**known == *value)
                })
                .copied()
        })
        .collect();
    if explicit
        .iter()
        .any(|(property, _)| !wanted.iter().any(|(name, _)| name == property))
    {
        return None;
    }
    block.offset(&wanted)
}

fn same_values(target: &BlockStates, id: i32, source: &Block, values: &[(&str, &str)]) -> bool {
    let Some((block, offset)) = target.locate(id) else {
        return false;
    };
    block.name == source.name && block.values(offset) == values
}

/// Every problem with the rules files: unparsable files, files for pairs
/// that are not adjacent, registries a version lacks, and names that do not
/// exist where a rule says they should. Empty when all rules are sound.
pub fn check_rules() -> Vec<String> {
    let mut problems = Vec::new();
    for stem in data::stems() {
        let Some(pair) = stem.strip_prefix("rules/") else {
            continue;
        };
        let parsed = pair.split_once('-').and_then(|(older, newer)| {
            Some((older.parse::<i32>().ok()?, newer.parse::<i32>().ok()?))
        });
        let Some((older, newer)) =
            parsed.filter(|(older, newer)| older < newer && adjacent(*older, *newer))
        else {
            problems.push(format!(
                "data/{stem}.json: not named <older>-<newer> for adjacent protocols"
            ));
            continue;
        };
        match try_rules(older, newer) {
            Ok(rules) => {
                let mut registries: Vec<_> = rules.iter().collect();
                registries.sort_by_key(|(name, _)| name.as_str());
                for (registry, rules) in registries {
                    check_registry_rules(older, newer, registry, rules, &mut problems);
                }
            }
            Err(error) => problems.push(error),
        }
    }
    problems
}

/// Whether `name` is an entry of `registry` in `protocol`; for block states
/// a pattern whose block, properties and values exist.
fn exists(protocol: i32, registry: &str, name: &str) -> bool {
    let Some(data) = data::try_version(protocol) else {
        return false;
    };
    if !version::flattened(protocol) {
        // 1.12 names states `name:metadata` and item variants `name:damage`.
        match registry {
            "block_state" => return crate::legacy::parse_block(protocol, name).is_some(),
            "item" => return crate::legacy::parse_item(protocol, name).is_some(),
            _ => {}
        }
    }
    if registry == "block_state" {
        let Some((block, properties)) = parse_pattern(name) else {
            return false;
        };
        let Some(block) = data.block_states.block(block) else {
            return false;
        };
        return properties.iter().all(|(property, value)| {
            block.properties.iter().any(|(known, values)| {
                &**known == *property && values.iter().any(|known| &**known == *value)
            })
        });
    }
    data.registry(registry)
        .is_some_and(|entries| entries.id(name).is_some())
}

fn check_registry_rules(
    older: i32,
    newer: i32,
    registry: &str,
    rules: &RegistryRules,
    problems: &mut Vec<String>,
) {
    let file = format!("data/rules/{older}-{newer}.json {registry}");
    let pair_has = |protocol: i32| {
        registry == "block_state"
            || data::try_version(protocol).is_some_and(|data| data.registry(registry).is_some())
    };
    if !pair_has(older) || !pair_has(newer) {
        problems.push(format!(
            "{file}: {} and {} do not both have this registry",
            version::release(older),
            version::release(newer)
        ));
        return;
    }
    let (old, new) = (version::release(older), version::release(newer));
    let flattening = version::flattened(older) != version::flattened(newer);
    if flattening && ["block_state", "block", "item"].contains(&registry) {
        if !rules.renames.is_empty() {
            problems.push(format!(
                "{file}: renames do not apply across the flattening; use `up` and `down`"
            ));
        }
        if registry == "block" {
            problems.push(format!(
                "{file}: the block registry follows the block_state rules across the flattening"
            ));
        }
    }
    let check = |section: &str, name: &str, protocol: i32, release: &str| {
        (!exists(protocol, registry, name))
            .then(|| format!("{file} {section}: {name} does not exist in {release}"))
    };
    let mut targets: HashMap<&str, &str> = HashMap::new();
    for (from, to) in &rules.renames {
        problems.extend(check("renames", from, older, old));
        problems.extend(check("renames", to, newer, new));
        // A rename names something new: an entry the older version already
        // has under the new name would make the rename ambiguous downwards.
        let whole = registry != "block_state" || !to.contains('[');
        if whole && exists(older, registry, to) {
            problems.push(format!(
                "{file} renames: {to} already exists in {old}, so {from} is not renamed to it"
            ));
        }
        if let Some(other) = targets.insert(to, from) {
            problems.push(format!(
                "{file} renames: {other} and {from} both rename to {to}; rename one and give the other an `up` stand-in"
            ));
        }
    }
    for (from, to) in &rules.up {
        problems.extend(check("up", from, older, old));
        problems.extend(check("up", to, newer, new));
    }
    for (from, to) in &rules.down {
        problems.extend(check("down", from, newer, new));
        problems.extend(check("down", to, older, old));
    }
    if let Some(default) = &rules.up_default {
        problems.extend(check("up_default", default, newer, new));
    }
    if let Some(default) = &rules.down_default {
        problems.extend(check("down_default", default, older, old));
    }
    // A whole-entry stand-in for an entry the other side already has is
    // dead: the same name wins.
    if registry != "block_state" {
        for (section, stand_ins, other) in [("up", &rules.up, newer), ("down", &rules.down, older)]
        {
            for from in stand_ins.keys() {
                if exists(other, registry, from) {
                    problems.push(format!(
                        "{file} {section}: {from} exists on both sides, so the stand-in is never used"
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::PROTOCOLS;

    #[test]
    fn rules_files_are_sound() {
        let problems = check_rules();
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn every_pair_and_registry_maps_without_panicking() {
        for pair in PROTOCOLS.windows(2) {
            let (older, newer) = (pair[0].0, pair[1].0);
            for registry in shared_registries(older, newer) {
                for (from, to) in [(older, newer), (newer, older)] {
                    let mapping = try_mapping(from, to, &registry);
                    assert!(
                        mapping.is_some(),
                        "no {registry} mapping from {from} to {to}"
                    );
                }
            }
        }
        assert!(try_mapping(335, 393, "item").is_none());
        assert!(try_mapping(754, 755, "no_such_registry").is_none());
    }

    #[test]
    fn identical_registries_map_to_themselves() {
        let mapping = mapping(772, 771, "entity_type");
        assert!(mapping.is_identity());
        assert_eq!(mapping.counts().unmapped, 0);
        let states = mapping_states(772, 771);
        assert!(states.is_identity());
    }

    fn mapping_states(from: i32, to: i32) -> &'static Mapping {
        mapping(from, to, "block_state")
    }

    #[test]
    fn items_map_by_name_in_both_directions() {
        let (old, new) = (data::version(754), data::version(755));
        let up = mapping(754, 755, "item");
        let down = mapping(755, 754, "item");
        let netherite = old.expect_registry("item").id("netherite_ingot").unwrap();
        let mapped = up.get(netherite).unwrap();
        assert_eq!(
            new.expect_registry("item").name(mapped),
            Some("netherite_ingot")
        );
        assert_eq!(down.get(mapped), Some(netherite));
        // Round trip for everything the older version has.
        for (id, _) in old.expect_registry("item").names() {
            assert_eq!(up.get(id).and_then(|new| down.get(new)), Some(id));
        }
    }

    #[test]
    fn renames_apply_in_both_directions() {
        let rules = RegistryRules {
            renames: [("grass_path".to_owned(), "dirt_path".to_owned())].into(),
            up: [("removed".to_owned(), "stone".to_owned())].into(),
            down_default: Some("air".to_owned()),
            ..Default::default()
        };
        let older = Registry::new(
            ["air", "stone", "grass_path", "removed"]
                .map(|name| Some(name.to_owned()))
                .into(),
        );
        let newer = Registry::new(
            ["air", "dirt_path", "stone", "added"]
                .map(|name| Some(name.to_owned()))
                .into(),
        );
        let up = by_name(&older, &newer, &directed(&rules, true));
        assert_eq!(
            (0..4).map(|id| up.get(id)).collect::<Vec<_>>(),
            [Some(0), Some(2), Some(1), Some(2)]
        );
        assert_eq!(up.kind(2), Some(Kind::Renamed));
        assert_eq!(up.kind(3), Some(Kind::StandIn));
        let down = by_name(&newer, &older, &directed(&rules, false));
        assert_eq!(
            (0..4).map(|id| down.get(id)).collect::<Vec<_>>(),
            [Some(0), Some(2), Some(1), Some(0)]
        );
        let no_default = by_name(&newer, &older, &directed(&RegistryRules::default(), false));
        assert_eq!(no_default.get(1), None);
        assert_eq!(no_default.counts().unmapped, 2);
    }

    /// A test block: name, properties with their values, default offset.
    type TestBlock<'a> = (&'a str, &'a [(&'a str, &'a [&'a str])], i32);

    fn states(blocks: &[TestBlock<'_>]) -> BlockStates {
        BlockStates::from_raw(
            blocks
                .iter()
                .map(|(name, properties, default)| {
                    (
                        name.to_string(),
                        properties
                            .iter()
                            .map(|(name, values)| {
                                (
                                    name.to_string(),
                                    values.iter().map(|value| value.to_string()).collect(),
                                )
                            })
                            .collect(),
                        *default,
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn block_states_pick_the_closest_state() {
        let older = states(&[
            ("air", &[], 0),
            (
                "oak_stairs",
                &[
                    ("facing", &["north", "south"]),
                    ("half", &["top", "bottom"]),
                    ("shape", &["straight", "inner"]),
                ],
                2,
            ),
            ("grass_path", &[], 0),
            (
                "old_slab",
                &[
                    ("type", &["top", "bottom"]),
                    ("waterlogged", &["true", "false"]),
                ],
                3,
            ),
        ]);
        let newer = states(&[
            ("air", &[], 0),
            (
                "oak_stairs",
                &[
                    ("facing", &["north", "south"]),
                    ("half", &["top", "bottom"]),
                    ("shape", &["straight", "outer"]),
                    ("waterlogged", &["true", "false"]),
                ],
                5,
            ),
            ("dirt_path", &[], 0),
            (
                "stone_slab",
                &[
                    ("type", &["top", "bottom", "double"]),
                    ("waterlogged", &["true", "false"]),
                ],
                3,
            ),
        ]);
        let rules = RegistryRules {
            renames: [("grass_path".to_owned(), "dirt_path".to_owned())].into(),
            up: [
                ("old_slab".to_owned(), "stone_slab".to_owned()),
                (
                    "old_slab[type=top]".to_owned(),
                    "stone_slab[type=double]".to_owned(),
                ),
            ]
            .into(),
            ..Default::default()
        };
        let up = block_states(&older, &newer, &directed(&rules, true));
        let describe = |id| {
            newer
                .describe(up.get(older.parse(id).unwrap()).unwrap())
                .unwrap()
        };
        // Waterlogged is new: the default (false) is picked.
        assert_eq!(
            describe("oak_stairs[facing=south,half=top,shape=straight]"),
            "oak_stairs[facing=south,half=top,shape=straight,waterlogged=false]"
        );
        // `inner` is gone: the default state's shape, other properties kept.
        assert_eq!(
            describe("oak_stairs[facing=south,half=bottom,shape=inner]"),
            "oak_stairs[facing=south,half=bottom,shape=straight,waterlogged=false]"
        );
        assert_eq!(
            up.kind(
                older
                    .parse("oak_stairs[facing=south,half=bottom,shape=inner]")
                    .unwrap()
            ),
            Some(Kind::Closest)
        );
        assert_eq!(describe("grass_path"), "dirt_path");
        // A whole-block stand-in carries same-named properties over; a
        // state-specific one wins where it matches.
        assert_eq!(
            describe("old_slab[type=bottom,waterlogged=true]"),
            "stone_slab[type=bottom,waterlogged=true]"
        );
        assert_eq!(
            describe("old_slab[type=top,waterlogged=true]"),
            "stone_slab[type=double,waterlogged=true]"
        );
        let down = block_states(&newer, &older, &directed(&rules, false));
        let back = |id| older.describe(down.get(newer.parse(id).unwrap())?);
        assert_eq!(back("dirt_path").as_deref(), Some("grass_path"));
        assert_eq!(
            back("oak_stairs[facing=north,half=top,shape=outer,waterlogged=true]").as_deref(),
            Some("oak_stairs[facing=north,half=top,shape=straight]")
        );
        // No stand-in back down for the stone slab.
        assert_eq!(back("stone_slab[type=double,waterlogged=false]"), None);
    }

    #[test]
    fn real_block_states_round_trip_between_close_versions() {
        let up = mapping(754, 755, "block_state");
        let down = mapping(755, 754, "block_state");
        let old = &data::version(754).block_states;
        let new = &data::version(755).block_states;
        let id = old
            .parse("oak_stairs[facing=east,half=top,shape=outer_left,waterlogged=true]")
            .unwrap();
        let mapped = up.get(id).unwrap();
        assert_eq!(
            new.describe(mapped).unwrap(),
            "oak_stairs[facing=east,half=top,shape=outer_left,waterlogged=true]"
        );
        assert_eq!(down.get(mapped), Some(id));
        for id in 0..old.len() {
            if up.kind(id) == Some(Kind::Identical) {
                assert_eq!(down.get(up.get(id).unwrap()), Some(id));
            }
        }
    }

    #[test]
    fn patterns_parse() {
        assert_eq!(parse_pattern("minecraft:stone"), Some(("stone", vec![])));
        assert_eq!(
            parse_pattern("lever[face=wall,powered=true]"),
            Some(("lever", vec![("face", "wall"), ("powered", "true")]))
        );
        assert_eq!(parse_pattern("lever[face]"), None);
        assert_eq!(parse_pattern("lever[face=wall"), None);
        assert_eq!(parse_pattern(""), None);
    }
}
