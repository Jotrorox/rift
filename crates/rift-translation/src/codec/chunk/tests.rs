//! Tests against the real-traffic corpus: every chunk, light and block
//! packet recorded from each official server must parse and re-serialise to
//! the same bytes, and converting it to each adjacent protocol (up and
//! down) must give packets the receiver's reader parses with the same
//! blocks (by name, allowing the mapping's stand-ins), biomes, light and
//! block entities. Protocols without a corpus on this machine are skipped.
use super::{
    blocks::read_changes,
    container::Container,
    light::{Light, LightArray, LightUpdate},
    model::{Biomes, Chunk, World},
    packets,
};
use crate::{
    Direction, State,
    codec::testing::{Defaults, Out, Recorded, corpus},
    data,
    mapping::{self, Kind},
    version::{self, PROTOCOLS},
    wire::Reader,
};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

/// The corpus of `protocol`, parsed once per test run.
fn cached(protocol: i32) -> Option<&'static [Recorded]> {
    type Cache = HashMap<i32, Option<&'static [Recorded]>>;
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    *CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .entry(protocol)
        .or_insert_with(|| corpus(protocol).map(|packets| &*Box::leak(packets.into_boxed_slice())))
}

fn recorded(protocol: i32, names: &[&str]) -> Vec<(&'static str, &'static Recorded)> {
    let Some(packets) = cached(protocol) else {
        return Vec::new();
    };
    packets
        .iter()
        .filter(|packet| packet.direction == Direction::Clientbound && packet.state == State::Play)
        .filter_map(|packet| {
            let name = packet.name(protocol);
            names.contains(&name).then_some((name, packet))
        })
        .collect()
}

const MINE: &[&str] = &[
    "login",
    "respawn",
    "level_chunk",
    "level_chunk_with_light",
    "light_update",
    "forget_level_chunk",
    "block_update",
    "section_blocks_update",
    "chunk_blocks_update",
    "block_entity_data",
    "block_event",
    "level_event",
    "explode",
    "chunks_biomes",
];

#[test]
fn corpus_chunks_and_light_round_trip() {
    let mut checked = 0;
    for &(protocol, release) in PROTOCOLS {
        let packets = recorded(
            protocol,
            &["level_chunk", "level_chunk_with_light", "light_update"],
        );
        if packets.is_empty() {
            eprintln!("no corpus for {release}: skipped");
            continue;
        }
        for (name, packet) in packets {
            let bytes = if name == "light_update" {
                let update = LightUpdate::read(&packet.data, protocol)
                    .unwrap_or_else(|error| panic!("{release} {name}: {error}"));
                update.write(protocol).unwrap()
            } else {
                let chunk = Chunk::read(&packet.data, protocol, &World::default())
                    .unwrap_or_else(|error| panic!("{release} {name}: {error}"));
                chunk.write(protocol).unwrap()
            };
            assert!(bytes == packet.data, "{release} {name} does not round trip");
            checked += 1;
        }
    }
    eprintln!("{checked} chunk and light packets round trip");
}

/// A column with its light, wherever the protocol keeps it.
struct Column {
    chunk: Chunk,
    light: Option<Light>,
}

/// The columns in a stream of packets of `protocol`, each with the light
/// sent for it before (1.14 to 1.17) or inside it.
fn columns<'a>(
    protocol: i32,
    world: World,
    packets: impl Iterator<Item = (&'a str, &'a [u8])>,
) -> Vec<Column> {
    let mut light: HashMap<(i32, i32), Light> = HashMap::new();
    let mut columns = Vec::new();
    for (name, data) in packets {
        match name {
            "light_update" => {
                let update = LightUpdate::read(data, protocol).unwrap();
                light.insert((update.x, update.z), update.light);
            }
            "level_chunk" | "level_chunk_with_light" => {
                let chunk = Chunk::read(data, protocol, &world)
                    .unwrap_or_else(|error| panic!("{}: {error}", version::release(protocol)));
                assert_eq!(
                    chunk.write(protocol).unwrap(),
                    data,
                    "converted chunk round trips"
                );
                let light = chunk
                    .light
                    .clone()
                    .or_else(|| light.remove(&(chunk.x, chunk.z)));
                columns.push(Column { chunk, light });
            }
            _ => {}
        }
    }
    columns
}

/// The 4096 states of the section at world section `y`; a section the
/// packet leaves out is air. `None` when the column does not reach it.
fn section_states(chunk: &Chunk, y: i32, scratch: &mut Vec<u32>) -> Option<Vec<i32>> {
    let section = chunk
        .sections
        .get(usize::try_from(y - chunk.min_section).ok()?)?;
    let mut values = Vec::new();
    match section {
        Some(section) => section.blocks.values(&mut values, scratch).unwrap(),
        None => values.resize(4096, 0),
    }
    Some(values)
}

fn biome_at(chunk: &Chunk, x: usize, y: i32, z: usize, scratch: &mut Vec<u32>) -> Option<i32> {
    match &chunk.biomes {
        Biomes::Absent => None,
        Biomes::Columns(columns) => Some(columns[z * 16 + x]),
        Biomes::Cells(cells) => {
            let layer = usize::try_from((y - chunk.min_section * 16) / 4).ok()?;
            cells.get(layer * 16 + (z / 4) * 4 + x / 4).copied()
        }
        Biomes::Sections => {
            let section = chunk
                .sections
                .get(usize::try_from(y.div_euclid(16) - chunk.min_section).ok()?)?
                .as_ref()?;
            let biomes: &Container = section.biomes.as_ref()?;
            let index = ((y.rem_euclid(16) as usize / 4) * 4 + z / 4) * 4 + x / 4;
            Some(biomes.get(index, scratch).unwrap())
        }
    }
}

fn light_at(light: &Light, sky: bool, index: usize) -> Option<&[u8]> {
    let arrays = if sky { &light.sky } else { &light.block };
    match arrays.get(index)? {
        LightArray::Data(bytes) => Some(bytes),
        _ => None,
    }
}

#[derive(Default, Debug)]
struct Tally {
    columns: usize,
    blocks: usize,
    stand_ins: usize,
    light_sections: usize,
}

/// Compare a converted column with its source.
fn compare(source: i32, target: i32, from: &Column, to: &Column, tally: &mut Tally) {
    let pair = format!(
        "{} -> {}",
        version::release(source),
        version::release(target)
    );
    let (a, b) = (&from.chunk, &to.chunk);
    assert_eq!((a.x, a.z), (b.x, b.z), "{pair}");
    let mapping = mapping::try_mapping(source, target, "block_state").unwrap();
    let (source_states, target_states) = (
        &data::version(source).block_states,
        &data::version(target).block_states,
    );
    let fallback = packets::block_fallback(target);
    let mut scratch = Vec::new();
    // Every world height both columns cover.
    let bottom = (a.min_section.max(b.min_section)) * 16;
    let top = ((a.min_section + a.sections.len() as i32)
        .min(b.min_section + b.sections.len() as i32))
        * 16;
    for section_y in (bottom..top).step_by(16) {
        let section = section_y / 16;
        let present = a
            .sections
            .get((section - a.min_section) as usize)
            .is_some_and(Option::is_some);
        if !present {
            continue;
        }
        let old = section_states(a, section, &mut scratch).unwrap();
        let new = section_states(b, section, &mut scratch)
            .unwrap_or_else(|| panic!("{pair}: section {section} outside the converted column"));
        for (index, (old, new)) in old.iter().zip(&new).enumerate() {
            let expected = mapping.get(*old).unwrap_or(fallback);
            assert_eq!(*new, expected, "{pair}: block {index} of section {section}");
            if mapping.kind(*old) == Some(Kind::Identical) {
                if !source_states.is_empty() {
                    assert_eq!(
                        source_states.describe(*old),
                        target_states.describe(*new),
                        "{pair}"
                    );
                }
            } else {
                tally.stand_ins += 1;
            }
            tally.blocks += 1;
        }
    }
    // Biomes at the centre of each cell at sea level, where every layout
    // agrees.
    let biomes = packets::biome_ids(source, target);
    for z in (2..16).step_by(4) {
        for x in (2..16).step_by(4) {
            let (Some(old), Some(new)) = (
                biome_at(a, x, 64, z, &mut scratch),
                biome_at(b, x, 64, z, &mut scratch),
            ) else {
                continue;
            };
            let expected = biomes.map_or(old, |mapping| mapping.get(old).unwrap_or(new));
            assert_eq!(new, expected, "{pair}: biome at {x} 64 {z}");
        }
    }
    // Light of every section both carry data for.
    if let (Some(old), Some(new)) = (&from.light, &to.light) {
        let shift = (a.min_section - b.min_section) as usize;
        for sky in [false, true] {
            for index in 0..old.sky.len().max(old.block.len()) {
                if let (Some(old), Some(new)) =
                    (light_at(old, sky, index), light_at(new, sky, index + shift))
                {
                    assert_eq!(old, new, "{pair}: light section {index}");
                    tally.light_sections += 1;
                }
            }
        }
    }
    assert!(
        to.light.is_some() || target < 757,
        "{pair}: 1.18 columns carry light"
    );
    // Block entities keep their positions where the receiver knows them.
    let positions = |chunk: &Chunk| -> Vec<(i32, i32, i32)> {
        let mut positions: Vec<_> = chunk
            .block_entities
            .iter()
            .map(|entity| (entity.x, entity.y, entity.z))
            .collect();
        positions.sort();
        positions
    };
    let (old, new) = (positions(a), positions(b));
    assert!(
        new.iter().all(|position| old.contains(position)),
        "{pair}: block entities moved"
    );
    if source >= 393 && target >= 393 {
        assert_eq!(old.len(), new.len(), "{pair}: block entities lost");
    }
    tally.columns += 1;
}

fn outputs(out: &[Out]) -> impl Iterator<Item = (&str, &[u8])> {
    out.iter()
        .map(|out| (out.name.as_str(), out.data.as_slice()))
}

/// Run `protocol`'s recorded packets through the step towards `client`.
fn convert_stream(client: i32, backend: i32) -> Option<(Vec<Column>, Vec<Column>, Vec<Out>)> {
    let packets = recorded(backend, MINE);
    if packets.is_empty() {
        return None;
    }
    let mut step = Defaults::new(client, backend);
    let mut out = Vec::new();
    let mut seen = Vec::new();
    for (name, packet) in &packets {
        let result = step.send_id(Direction::Clientbound, State::Play, packet.id, &packet.data);
        if matches!(*name, "login" | "respawn") {
            // Other codecs may not handle these yet; only the taps matter.
            out.extend(result.unwrap_or_default());
            continue;
        }
        let converted = result.unwrap_or_else(|error| {
            panic!(
                "{} -> {}: {name}: {error}",
                version::release(backend),
                version::release(client)
            )
        });
        out.extend(converted);
        seen.push((*name, packet.data.as_slice()));
    }
    let from = columns(backend, World::default(), seen.into_iter());
    // A 1.17+ client learns its world's height from the dimension.
    let world = World {
        min_section: (client >= 755 && backend >= 755)
            .then(|| from.first().map(|column| column.chunk.min_section))
            .flatten(),
        ..World::default()
    };
    let to = columns(client, world, outputs(&out));
    Some((from, to, out))
}

/// Pairs whose chunks cannot convert yet; see the INTEGRATION notes.
fn blocked(older: i32, newer: i32) -> bool {
    // The flattening mapping (`crate::legacy`) is not written yet.
    (older, newer) == (340, 393)
}

#[test]
fn corpus_chunks_convert_between_every_adjacent_pair() {
    let mut report = Vec::new();
    for pair in PROTOCOLS.windows(2) {
        let (older, newer) = (pair[0].0, pair[1].0);
        if blocked(older, newer) {
            report.push(format!(
                "{}/{}: blocked on the flattening mapping",
                pair[0].1, pair[1].1
            ));
            continue;
        }
        for (client, backend) in [(newer, older), (older, newer)] {
            let Some((from, to, out)) = convert_stream(client, backend) else {
                report.push(format!(
                    "{} -> {}: no corpus",
                    version::release(backend),
                    version::release(client)
                ));
                continue;
            };
            assert_eq!(
                from.len(),
                to.len(),
                "{} -> {}: column count",
                backend,
                client
            );
            let mut tally = Tally::default();
            // Debug builds compare a sample; release builds every column.
            let limit = if cfg!(debug_assertions) {
                4
            } else {
                usize::MAX
            };
            for (from, to) in from.iter().zip(&to).take(limit) {
                compare(backend, client, from, to, &mut tally);
            }
            check_block_packets(backend, client, &out);
            report.push(format!(
                "{} -> {}: {} columns, {} blocks ({} via stand-ins), {} light sections",
                version::release(backend),
                version::release(client),
                tally.columns,
                tally.blocks,
                tally.stand_ins,
                tally.light_sections
            ));
        }
    }
    eprintln!("{}", report.join("\n"));
}

/// The converted block packets parse in the receiver's layout.
fn check_block_packets(source: i32, target: i32, out: &[Out]) {
    let pair = format!(
        "{} -> {}",
        version::release(source),
        version::release(target)
    );
    let states = data::version(target).block_states.len();
    for packet in out {
        let mut reader = Reader::new(&packet.data);
        match packet.name.as_str() {
            "block_update" => {
                packets::read_position(&mut reader, target).unwrap();
                let state = reader.varint().unwrap();
                reader.finish().unwrap();
                if version::flattened(target) {
                    assert!(
                        (0..states).contains(&state),
                        "{pair}: block update state {state}"
                    );
                }
            }
            "section_blocks_update" | "chunk_blocks_update" => {
                for change in read_changes(&packet.data, target).unwrap() {
                    if version::flattened(target) {
                        assert!((0..states).contains(&change.state), "{pair}");
                    }
                }
            }
            "block_entity_data" => {
                packets::read_position(&mut reader, target).unwrap();
                if target >= 757 {
                    let kind = reader.varint().unwrap();
                    assert!(
                        data::version(target)
                            .expect_registry("block_entity_type")
                            .name(kind)
                            .is_some(),
                        "{pair}"
                    );
                } else {
                    reader.u8().unwrap();
                }
                crate::nbt::Nbt::read(&mut reader, !version::nameless_nbt(target)).unwrap();
                reader.finish().unwrap();
            }
            "level_event" => {
                reader.i32().unwrap();
                packets::read_position(&mut reader, target).unwrap();
                reader.i32().unwrap();
                reader.bool().unwrap();
                reader.finish().unwrap();
            }
            "block_event" => {
                packets::read_position(&mut reader, target).unwrap();
                reader.u8().unwrap();
                reader.u8().unwrap();
                let block = reader.varint().unwrap();
                reader.finish().unwrap();
                let blocks = data::version(target).expect_registry("block").len() as i32;
                assert!((0..blocks).contains(&block), "{pair}");
            }
            _ => {}
        }
    }
}
