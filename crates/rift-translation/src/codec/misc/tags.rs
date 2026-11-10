//! `update_tags`: named lists of registry IDs, sent at login and on reload.
//!
//! 1.13 to 1.16.5 send fixed lists for blocks, items and fluids (and entity
//! types from 1.14); from 1.17 the packet is keyed by registry, and from
//! 1.20.2 it is sent during configuration too. Entries are network IDs of
//! their registry, so every list is mapped to the receiver's numbering.
//!
//! The client relies on vanilla tags its game logic and its registries name:
//! a newer client behind an older backend misses the tags the older server
//! never had, and an older client behind a newer one the tags it removed
//! (1.21.11 dropped `enchantable/sword`, which a 1.21.10 client's vanilla
//! enchantments name; it fails to load them without it). Every vanilla tag
//! of the receiver's version that the packet lacks is added, restricted to
//! entries the sender's registry has.
use super::registry::{self, VanillaTags};
use crate::{
    Direction, Packet, State,
    codec::{HasShared, Shared},
    data::strip,
    step::Cx,
    version,
    wire::{Reader, Writer, invalid},
};
use std::{collections::HashSet, io};

/// Registry-keyed tags from 1.17 (protocol 755); fixed lists before.
const KEYED: i32 = 755;

/// The registries of the fixed layout, in wire order.
const FIXED: [&str; 4] = [
    "minecraft:block",
    "minecraft:item",
    "minecraft:fluid",
    "minecraft:entity_type",
];

fn fixed(protocol: i32) -> &'static [&'static str] {
    if protocol < 477 { &FIXED[..3] } else { &FIXED }
}

/// Registries the data names by their newest name that older protocols
/// send under another: (newest name, older name, first protocol with the
/// newest). A client rejects tags of a registry it does not know by name.
const RENAMED: [(&str, &str, i32); 1] = [("painting_variant", "motive", 759)];

/// `registry` (namespaced or not) as `protocol` names it on the wire.
fn wire_name(protocol: i32, registry: &str) -> String {
    match RENAMED
        .iter()
        .find(|(newest, _, first)| protocol < *first && strip(registry) == *newest)
    {
        Some((_, older, _)) => format!("minecraft:{older}"),
        None => registry.to_owned(),
    }
}

/// A registry name from `protocol`'s wire as the data names it.
fn data_name(protocol: i32, registry: &str) -> String {
    match RENAMED
        .iter()
        .find(|(_, older, first)| protocol < *first && strip(registry) == *older)
    {
        Some((newest, _, _)) => format!("minecraft:{newest}"),
        None => registry.to_owned(),
    }
}

/// One registry's tags as sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryTags {
    /// The registry identifier; implied by position in the fixed layout,
    /// where it is one of [`FIXED`].
    pub registry: String,
    pub tags: Vec<(String, Vec<i32>)>,
}

/// The contents of one `update_tags` packet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TagSet {
    pub registries: Vec<RegistryTags>,
}

impl TagSet {
    fn registry_mut(&mut self, name: &str) -> Option<&mut RegistryTags> {
        let name = strip(name);
        self.registries
            .iter_mut()
            .find(|registry| strip(&registry.registry) == name)
    }
}

fn read_tags(reader: &mut Reader<'_>) -> io::Result<Vec<(String, Vec<i32>)>> {
    let count = reader.count()?;
    let mut tags = Vec::with_capacity(count);
    for _ in 0..count {
        let name = reader.str()?.to_owned();
        let size = reader.count()?;
        let mut ids = Vec::with_capacity(size);
        for _ in 0..size {
            ids.push(reader.varint()?);
        }
        tags.push((name, ids));
    }
    Ok(tags)
}

fn write_tags(writer: &mut Writer, tags: &[(String, Vec<i32>)]) {
    writer.varint(tags.len() as i32);
    for (name, ids) in tags {
        writer.string(name).varint(ids.len() as i32);
        for id in ids {
            writer.varint(*id);
        }
    }
}

/// Parse an `update_tags` body of `protocol`.
pub fn read(protocol: i32, data: &[u8]) -> io::Result<TagSet> {
    let mut reader = Reader::new(data);
    let mut set = TagSet::default();
    if protocol < KEYED {
        for registry in fixed(protocol) {
            set.registries.push(RegistryTags {
                registry: (*registry).to_owned(),
                tags: read_tags(&mut reader)?,
            });
        }
    } else {
        for _ in 0..reader.count()? {
            let registry = data_name(protocol, reader.str()?);
            set.registries.push(RegistryTags {
                registry,
                tags: read_tags(&mut reader)?,
            });
        }
    }
    reader.finish()?;
    Ok(set)
}

/// Serialize `set` in `protocol`'s layout. The fixed layout cannot carry
/// other registries than its own.
pub fn write(protocol: i32, set: &TagSet) -> io::Result<Vec<u8>> {
    let mut writer = Writer::new();
    if protocol < KEYED {
        if let Some(extra) = set.registries.iter().find(|registry| {
            !fixed(protocol)
                .iter()
                .any(|known| strip(known) == strip(&registry.registry))
        }) {
            return Err(invalid(&format!(
                "{} tags cannot be sent to protocol {protocol}",
                extra.registry
            )));
        }
        for known in fixed(protocol) {
            let tags = set
                .registries
                .iter()
                .find(|registry| strip(&registry.registry) == strip(known));
            write_tags(&mut writer, tags.map_or(&[][..], |registry| &registry.tags));
        }
    } else {
        writer.varint(set.registries.len() as i32);
        for registry in &set.registries {
            writer.string(&wire_name(protocol, &registry.registry));
            write_tags(&mut writer, &registry.tags);
        }
    }
    Ok(writer.into_inner())
}

/// Whether `protocol` can receive tags of `registry`, as far as it is known:
/// `None` for a registry unknown to the version's data (a custom one).
fn accepts(protocol: i32, registry: &str) -> Option<bool> {
    if protocol < KEYED {
        return Some(
            fixed(protocol)
                .iter()
                .any(|known| strip(known) == strip(registry)),
        );
    }
    registry::registry(protocol, registry).map(|_| true)
}

/// Convert a tag set from the sender's numbering to the receiver's: map
/// every list, drop registries the receiver lacks and add the receiver's
/// missing vanilla tags.
pub fn convert(shared: &mut Shared, direction: Direction, set: TagSet) -> TagSet {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let mut converted = TagSet::default();
    for mut tags in set.registries {
        let name = strip(&tags.registry).to_owned();
        let known_source = registry::registry(source, &name).is_some();
        match accepts(target, &name) {
            Some(false) => continue,
            // Unknown to both sides: a custom registry the step cannot renumber.
            None if !known_source => {
                converted.registries.push(tags);
                continue;
            }
            None => continue,
            Some(true) => {}
        }
        let mapping = registry::mapping(shared, direction, &name);
        let size = registry::registry(target, &name).map(|(_, registry)| registry.len());
        for (_, ids) in &mut tags.tags {
            let mut seen = HashSet::with_capacity(ids.len());
            ids.retain_mut(|id| {
                let mapped = match mapping {
                    Some(mapping) => match mapping.get(*id) {
                        Some(mapped) => mapped,
                        None => return false,
                    },
                    None => *id,
                };
                if size.is_some_and(|size| mapped < 0 || mapped as usize >= size) {
                    return false;
                }
                *id = mapped;
                seen.insert(mapped)
            });
        }
        converted.registries.push(tags);
    }
    if let Some(vanilla) = registry::vanilla_tags(target) {
        add_missing(shared, direction, vanilla, &mut converted);
    }
    converted
}

/// Add every vanilla tag of the receiver that `set` lacks.
fn add_missing(shared: &mut Shared, direction: Direction, vanilla: &VanillaTags, set: &mut TagSet) {
    let (source, target) = (shared.source(direction), shared.target(direction));
    for (name, tags) in &vanilla.registries {
        if accepts(target, name) != Some(true) {
            continue;
        }
        let Some((_, receiver)) = registry::registry(target, name) else {
            continue;
        };
        // Entries the sender's registry can produce, when it has the registry.
        let image = registry::registry(source, name).and_then(|(_, sender)| {
            let mapping = registry::mapping(shared, direction, name)?;
            let mut image = vec![false; receiver.len()];
            for id in 0..sender.len() as i32 {
                if let Some(slot) = mapping
                    .get(id)
                    .and_then(|mapped| image.get_mut(mapped as usize))
                {
                    *slot = true;
                }
            }
            Some(image)
        });
        if set.registry_mut(name).is_none() {
            set.registries.push(RegistryTags {
                registry: format!("minecraft:{name}"),
                tags: Vec::new(),
            });
        }
        let registry = set.registry_mut(name).expect("just added");
        let present: HashSet<String> = registry
            .tags
            .iter()
            .map(|(tag, _)| strip(tag).to_owned())
            .collect();
        for (tag, entries) in tags {
            if present.contains(*tag) {
                continue;
            }
            let ids = entries
                .iter()
                .filter_map(|entry| receiver.id(entry))
                .filter(|id| image.as_ref().is_none_or(|image| image[*id as usize]))
                .collect();
            registry.tags.push((format!("minecraft:{tag}"), ids));
        }
    }
}

/// The full vanilla tag set of `protocol` as an `update_tags` body, for a
/// step whose older side has no tags at all (a 1.13 client on a 1.12
/// backend needs one once it joins).
pub fn vanilla_update_tags(protocol: i32) -> io::Result<Vec<u8>> {
    let vanilla = registry::vanilla_tags(protocol)
        .ok_or_else(|| invalid(&format!("protocol {protocol} has no tags")))?;
    let mut set = TagSet::default();
    for (name, tags) in &vanilla.registries {
        let (_, receiver) = registry::registry(protocol, name)
            .ok_or_else(|| invalid(&format!("protocol {protocol} lacks the {name} registry")))?;
        set.registries.push(RegistryTags {
            registry: format!("minecraft:{name}"),
            tags: tags
                .iter()
                .map(|(tag, entries)| {
                    let ids = entries
                        .iter()
                        .filter_map(|entry| receiver.id(entry))
                        .collect();
                    (format!("minecraft:{tag}"), ids)
                })
                .collect(),
        });
    }
    write(protocol, &set)
}

/// Whether the pair must rewrite tags travelling from `source` to `target`.
pub fn needed(source: i32, target: i32) -> bool {
    if target < 393 || (source < KEYED) != (target < KEYED) {
        return true;
    }
    let (Some(from), Some(to)) = (
        registry::vanilla_tags(source),
        registry::vanilla_tags(target),
    ) else {
        return true;
    };
    let mut names: Vec<&str> = from
        .registries
        .iter()
        .chain(&to.registries)
        .map(|(name, _)| *name)
        .collect();
    if source >= 758 && target >= 758 {
        // Data packs may tag any built-in registry.
        names.extend(crate::data::version(source).registry_names());
        names.extend(crate::data::version(target).registry_names());
    }
    if names
        .iter()
        .any(|name| !registry::unchanged(source, target, name))
    {
        return true;
    }
    to.registries.iter().any(|(name, tags)| {
        let sent = from.registry(name).unwrap_or(&[]);
        tags.iter()
            .any(|(tag, _)| !sent.iter().any(|(known, _)| known == tag))
    })
}

/// The default `update_tags` handler.
pub fn update_tags<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let direction = Direction::Clientbound;
    let (source, target) = (shared.source(direction), shared.target(direction));
    if target < 393 {
        // 1.12 has no tags.
        return Ok(());
    }
    let set = read(source, &packet.data)?;
    let set = convert(shared, direction, set);
    let body = write(target, &set)?;
    if state == State::Configuration && !version::has_configuration(target) {
        // Before 1.20.2 tags arrive in play; the step that replaces the
        // configuration phase decides when the client gets them.
        let id = shared
            .target_data(direction)
            .expect_packet(State::Play, direction, "update_tags");
        cx.to_client(State::Play, Packet::new(id, body));
    } else {
        cx.to_client(state, Packet::new(packet.id, body));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::testing::{Defaults, corpus};

    fn names(protocol: i32, set: &TagSet) -> Vec<(String, String, Vec<String>)> {
        let mut result = Vec::new();
        for tags in &set.registries {
            let registry = strip(&tags.registry);
            let known = registry::registry(protocol, registry).map(|(_, known)| known);
            for (tag, ids) in &tags.tags {
                let mut entries: Vec<String> = ids
                    .iter()
                    .map(|id| {
                        known
                            .and_then(|known| known.name(*id))
                            .map_or_else(|| format!("#{id}"), str::to_owned)
                    })
                    .collect();
                entries.sort();
                result.push((registry.to_owned(), strip(tag).to_owned(), entries));
            }
        }
        result.sort();
        result
    }

    #[test]
    fn fixed_and_keyed_layouts_round_trip() {
        let set = TagSet {
            registries: vec![
                RegistryTags {
                    registry: "minecraft:block".into(),
                    tags: vec![("minecraft:logs".into(), vec![1, 2, 3])],
                },
                RegistryTags {
                    registry: "minecraft:item".into(),
                    tags: vec![],
                },
                RegistryTags {
                    registry: "minecraft:fluid".into(),
                    tags: vec![("minecraft:water".into(), vec![1, 2])],
                },
            ],
        };
        let bytes = write(393, &set).unwrap();
        assert_eq!(read(393, &bytes).unwrap(), set);
        // 1.14 adds an entity type list: absent means empty.
        let bytes = write(477, &set).unwrap();
        assert_eq!(read(477, &bytes).unwrap().registries.len(), 4);
        let bytes = write(755, &set).unwrap();
        assert_eq!(read(755, &bytes).unwrap(), set);
        let mut extra = set.clone();
        extra.registries.push(RegistryTags {
            registry: "minecraft:game_event".into(),
            tags: vec![],
        });
        assert!(write(754, &extra).is_err());
        // Trailing bytes, short input and absurd counts fail.
        let mut long = write(755, &set).unwrap();
        long.push(0);
        assert!(read(755, &long).is_err());
        assert!(read(755, &bytes[..bytes.len() - 1]).is_err());
        assert!(read(755, &[0xff, 0xff, 0xff, 0xff, 0x07]).is_err());
        assert!(read(393, &[]).is_err());
    }

    #[test]
    fn renamed_registries_go_by_the_receivers_name() {
        // 1.19 calls painting tags painting_variant; 1.18.2 knows the
        // registry only as motive and rejects tags of one it does not know.
        let body = vanilla_update_tags(759).unwrap();
        assert!(
            read(759, &body)
                .unwrap()
                .registry_mut("painting_variant")
                .is_some()
        );
        let mut step = Defaults::new(758, 759);
        let out = step.clientbound(State::Play, "update_tags", &body).unwrap();
        let wire: Vec<String> = {
            let mut reader = Reader::new(&out[0].data);
            (0..reader.count().unwrap())
                .map(|_| {
                    let name = reader.str().unwrap().to_owned();
                    read_tags(&mut reader).unwrap();
                    name
                })
                .collect()
        };
        assert!(wire.contains(&"minecraft:motive".to_owned()), "{wire:?}");
        assert!(!wire.iter().any(|name| name.contains("painting_variant")));
        // Read back, the data's name again; and a motive set goes up renamed.
        let paintings = |protocol: i32, body: &[u8]| {
            let mut set = read(protocol, body).unwrap();
            let paintings = set.registry_mut("painting_variant").unwrap().clone();
            names(
                protocol,
                &TagSet {
                    registries: vec![paintings],
                },
            )
        };
        let back = Defaults::new(759, 758)
            .clientbound(State::Play, "update_tags", &out[0].data)
            .unwrap();
        assert_eq!(paintings(759, &back[0].data), paintings(758, &out[0].data));
    }

    #[test]
    fn vanilla_sets_build_for_every_protocol_with_tags() {
        for &(protocol, _) in crate::version::PROTOCOLS {
            if protocol < 393 {
                assert!(vanilla_update_tags(protocol).is_err());
                continue;
            }
            let body = vanilla_update_tags(protocol).unwrap();
            let set = read(protocol, &body).unwrap();
            assert!(set.registries.iter().any(|tags| !tags.tags.is_empty()));
        }
    }

    #[test]
    fn upgrading_adds_missing_vanilla_tags_and_downgrading_drops_registries() {
        // A 1.17 client behind 1.16.5: game event tags appear, the fixed
        // lists become keyed.
        let body = vanilla_update_tags(754).unwrap();
        let mut step = Defaults::new(755, 754);
        let out = step.clientbound(State::Play, "update_tags", &body).unwrap();
        assert_eq!(out.len(), 1);
        let set = read(755, &out[0].data).unwrap();
        let game_events = set
            .registries
            .iter()
            .find(|tags| tags.registry == "minecraft:game_event")
            .unwrap();
        assert!(
            game_events
                .tags
                .iter()
                .any(|(tag, _)| tag == "minecraft:vibrations")
        );
        // Back down: 1.16.5 has no game event tags.
        let mut step = Defaults::new(754, 755);
        let down = step
            .clientbound(State::Play, "update_tags", &out[0].data)
            .unwrap();
        // The 1.17 tags 1.16.5 has a registry for stay (custom tags are
        // allowed); every original tag comes back as it was.
        let set = names(754, &read(754, &down[0].data).unwrap());
        for tag in names(754, &read(754, &body).unwrap()) {
            assert!(set.contains(&tag), "{tag:?}");
        }
        assert!(!set.iter().any(|(registry, _, _)| registry == "game_event"));
        // 1.12 has no tags at all.
        let mut step = Defaults::new(340, 393);
        let body = vanilla_update_tags(393).unwrap();
        assert!(
            step.clientbound(State::Play, "update_tags", &body)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn downgrading_restores_tags_the_newer_version_removed() {
        // 1.21.11 replaced enchantable/sword; a 1.21.10 client's vanilla
        // enchantments name it and fail to load without it.
        let body = vanilla_update_tags(774).unwrap();
        let mut step = Defaults::new(773, 774);
        let out = step.clientbound(State::Play, "update_tags", &body).unwrap();
        let set = read(773, &out[0].data).unwrap();
        let items = set
            .registries
            .iter()
            .find(|tags| strip(&tags.registry) == "item")
            .unwrap();
        let (_, swords) = items
            .tags
            .iter()
            .find(|(tag, _)| tag == "minecraft:enchantable/sword")
            .unwrap();
        let registry = crate::data::version(773).expect_registry("item");
        assert!(swords.contains(&registry.id("diamond_sword").unwrap()));
    }

    #[test]
    fn identical_neighbours_pass_tags_through() {
        assert!(!needed(498, 490));
        assert!(needed(754, 755));
        assert!(needed(755, 754));
        assert!(needed(393, 340));
    }

    /// Every update_tags packet of every recorded session parses and
    /// re-serialises to the same bytes.
    #[test]
    fn corpus_tags_round_trip() {
        let mut checked = 0;
        for &(protocol, _) in crate::version::PROTOCOLS {
            let packets: Vec<_> = crate::codec::misc::sessions(protocol)
                .into_iter()
                .flatten()
                .collect();
            for packet in packets
                .iter()
                .filter(|packet| packet.name(protocol) == "update_tags")
            {
                let set = read(protocol, &packet.data).unwrap();
                assert_eq!(write(protocol, &set).unwrap(), packet.data, "{protocol}");
                checked += 1;
            }
        }
        eprintln!("{checked} recorded update_tags packets round-tripped");
    }

    /// Converting a real session's tags to the next protocol up adds the
    /// tags that protocol's real server sends and the older one did not,
    /// with the same entries (as far as the older registry has them); back
    /// down, every entry names something the older version has.
    #[test]
    fn corpus_tags_convert_like_the_real_server() {
        let (mut checked, mut added) = (0, 0);
        for pair in crate::version::PROTOCOLS.windows(2) {
            let (older, newer) = (pair[0].0, pair[1].0);
            if older < 393 {
                continue;
            }
            let (Some(old), Some(new)) = (corpus(older), corpus(newer)) else {
                continue;
            };
            let find = |packets: &[crate::codec::testing::Recorded], protocol| {
                let packet = packets
                    .iter()
                    .find(|packet| packet.name(protocol) == "update_tags")
                    .unwrap();
                (packet.state, read(protocol, &packet.data).unwrap())
            };
            let ((old_state, old_set), (new_state, new_set)) =
                (find(&old, older), find(&new, newer));
            let old_body = write(older, &old_set).unwrap();
            let mut up = Defaults::new(newer, older);
            let out = up
                .send(Direction::Clientbound, old_state, "update_tags", &old_body)
                .unwrap();
            let converted = names(newer, &read(newer, &out[0].data).unwrap());
            // Tags the older server sent keep its contents. Every other tag
            // the newer server sends must have been added, with the entries
            // the older registry has.
            let sent: HashSet<(String, String)> = names(older, &old_set)
                .into_iter()
                .map(|(registry, tag, _)| (registry, tag))
                .collect();
            for expected in names(newer, &restrict(older, newer, &new_set)) {
                if sent.contains(&(expected.0.clone(), expected.1.clone())) {
                    continue;
                }
                assert!(
                    converted.contains(&expected),
                    "{older} -> {newer}: {expected:?}"
                );
                added += 1;
            }
            let new_body = write(newer, &new_set).unwrap();
            let mut down = Defaults::new(older, newer);
            let out = down
                .send(Direction::Clientbound, new_state, "update_tags", &new_body)
                .unwrap();
            let converted = read(older, &out[0].data).unwrap();
            for (registry, tag, entries) in names(older, &converted) {
                for entry in &entries {
                    assert!(
                        !entry.starts_with('#'),
                        "{newer} -> {older} {registry} {tag}"
                    );
                }
            }
            checked += 1;
        }
        eprintln!("{checked} recorded pairs converted, {added} added tags match the real server");
    }

    /// The real newer set with every entry the older version lacks removed.
    fn restrict(older: i32, newer: i32, set: &TagSet) -> TagSet {
        let mut shared = Shared::new(newer, older);
        let mut restricted = set.clone();
        for tags in &mut restricted.registries {
            let name = strip(&tags.registry).to_owned();
            let Some((_, sender)) = registry::registry(older, &name) else {
                continue;
            };
            let Some(mapping) = registry::mapping(&mut shared, Direction::Clientbound, &name)
            else {
                continue;
            };
            let image: HashSet<i32> = (0..sender.len() as i32)
                .filter_map(|id| mapping.get(id))
                .collect();
            for (_, ids) in &mut tags.tags {
                ids.retain(|id| image.contains(id));
            }
        }
        restricted
    }
}
