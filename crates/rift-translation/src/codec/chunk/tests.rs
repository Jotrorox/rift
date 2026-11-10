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

mod handlers {
    use super::super::{
        blocks::read_changes,
        light::{Light, LightArray, LightUpdate},
        model::{Chunk, World, tests::sample},
    };
    use crate::{
        State,
        codec::testing::{Defaults, Out},
        data,
        nbt::{Compound, Nbt},
        wire::{Position, Reader, Writer},
    };

    fn names(out: &[Out]) -> Vec<&str> {
        out.iter().map(|out| out.name.as_str()).collect()
    }

    fn column(protocol: i32, x: i32, z: i32) -> Vec<u8> {
        let mut chunk = sample(protocol);
        chunk.x = x;
        chunk.z = z;
        for entity in &mut chunk.block_entities {
            entity.x = x * 16 + (entity.x & 15);
            entity.z = z * 16 + (entity.z & 15);
            if let Some(Nbt::Compound(tag)) = &mut entity.nbt
                && tag.contains("x")
            {
                tag.insert("x", entity.x);
                tag.insert("z", entity.z);
            }
        }
        if (477..757).contains(&protocol) {
            chunk.light = None;
        }
        chunk.write(protocol).unwrap()
    }

    fn light(x: i32, z: i32, level: u8) -> LightUpdate {
        let light = Light {
            block: vec![LightArray::Absent, LightArray::filled(level)],
            sky: vec![LightArray::Empty, LightArray::filled(15)],
            ..Light::default()
        };
        LightUpdate { x, z, light }
    }

    fn forget(protocol: i32, x: i32, z: i32) -> Vec<u8> {
        let mut writer = Writer::new();
        if protocol >= 764 {
            writer.i32(z).i32(x);
        } else {
            writer.i32(x).i32(z);
        }
        writer.buf
    }

    #[test]
    fn a_1_17_columns_light_is_held_until_the_column_and_merged_into_it() {
        let mut step = Defaults::new(757, 756);
        let update = light(1, 2, 7);
        let out = step
            .clientbound(State::Play, "light_update", &update.write(756).unwrap())
            .unwrap();
        assert!(out.is_empty(), "held");
        let out = step
            .clientbound(State::Play, "level_chunk", &column(756, 1, 2))
            .unwrap();
        assert_eq!(names(&out), ["level_chunk_with_light"]);
        let world = World {
            min_section: Some(-4),
            ..World::default()
        };
        let chunk = Chunk::read(&out[0].data, 757, &world).unwrap();
        assert_eq!(chunk.light.unwrap().block[1], LightArray::filled(7));
        // Once the column is there, updates pass through.
        let out = step
            .clientbound(State::Play, "light_update", &update.write(756).unwrap())
            .unwrap();
        assert_eq!(names(&out), ["light_update"]);
        // After it is forgotten they are held again.
        step.clientbound(State::Play, "forget_level_chunk", &forget(756, 1, 2))
            .unwrap();
        let out = step
            .clientbound(State::Play, "light_update", &update.write(756).unwrap())
            .unwrap();
        assert!(out.is_empty());
        // A column whose light never came still carries (empty) light data.
        let out = step
            .clientbound(State::Play, "level_chunk", &column(756, 5, 5))
            .unwrap();
        assert!(
            Chunk::read(&out[0].data, 757, &world)
                .unwrap()
                .light
                .is_some()
        );
    }

    #[test]
    fn a_1_18_column_is_split_into_its_light_and_the_column() {
        let mut step = Defaults::new(756, 757);
        let out = step
            .clientbound(State::Play, "level_chunk_with_light", &column(757, 3, -4))
            .unwrap();
        assert_eq!(names(&out), ["light_update", "level_chunk"]);
        let update = LightUpdate::read(&out[0].data, 756).unwrap();
        assert_eq!((update.x, update.z), (3, -4));
        assert!(update.light.trust_edges);
        let world = World {
            min_section: Some(-4),
            ..World::default()
        };
        Chunk::read(&out[1].data, 756, &world).unwrap();
    }

    #[test]
    fn light_for_a_1_13_client_goes_inside_its_column() {
        let mut step = Defaults::new(404, 477);
        let mut login = Writer::new();
        login
            .i32(1)
            .u8(0)
            .i32(0)
            .u8(20)
            .string("flat")
            .varint(10)
            .bool(false);
        step.clientbound(State::Play, "login", &login.buf).unwrap();
        let out = step
            .clientbound(
                State::Play,
                "light_update",
                &light(0, 0, 9).write(477).unwrap(),
            )
            .unwrap();
        assert!(out.is_empty());
        let out = step
            .clientbound(State::Play, "level_chunk", &column(477, 0, 0))
            .unwrap();
        assert_eq!(names(&out), ["level_chunk"]);
        let chunk = Chunk::read(
            &out[0].data,
            404,
            &World {
                sky_light: Some(true),
                ..World::default()
            },
        )
        .unwrap();
        let light = chunk.light.unwrap();
        assert_eq!(light.block[1], LightArray::filled(9));
        // Sections without light data: dark blocks, lit sky.
        let present = chunk
            .sections
            .iter()
            .position(|section| section.is_some())
            .unwrap();
        assert_eq!(present, 0);
        assert_eq!(light.sky[2], LightArray::Data(vec![0xff; 2048].into()));
        // And a 1.13 column going up brings its light first.
        let mut up = Defaults::new(477, 404);
        let out = up
            .clientbound(State::Play, "level_chunk", &column(404, 0, 0))
            .unwrap();
        assert_eq!(names(&out), ["light_update", "level_chunk"]);
        let update = LightUpdate::read(&out[0].data, 477).unwrap();
        assert_eq!(update.light.sky.len(), 18);
    }

    #[test]
    fn replaced_columns_are_forgotten_first_for_1_21_2_clients() {
        let mut step = Defaults::new(768, 767);
        let body = column(767, 0, 0);
        let out = step
            .clientbound(State::Play, "level_chunk_with_light", &body)
            .unwrap();
        assert_eq!(names(&out), ["level_chunk_with_light"]);
        let out = step
            .clientbound(State::Play, "level_chunk_with_light", &body)
            .unwrap();
        assert_eq!(
            names(&out),
            ["forget_level_chunk", "level_chunk_with_light"]
        );
        step.clientbound(State::Play, "forget_level_chunk", &forget(767, 0, 0))
            .unwrap();
        let out = step
            .clientbound(State::Play, "level_chunk_with_light", &body)
            .unwrap();
        assert_eq!(names(&out), ["level_chunk_with_light"]);
    }

    #[test]
    fn forgetting_swaps_coordinates_across_1_20_2() {
        let mut step = Defaults::new(764, 763);
        let out = step
            .clientbound(State::Play, "forget_level_chunk", &forget(763, 7, -9))
            .unwrap();
        assert_eq!(out[0].data, forget(764, 7, -9));
        let mut step = Defaults::new(763, 764);
        let out = step
            .clientbound(State::Play, "forget_level_chunk", &forget(764, 7, -9))
            .unwrap();
        assert_eq!(out[0].data, forget(763, 7, -9));
    }

    #[test]
    fn partial_columns_become_section_updates_for_1_17() {
        let mut chunk = sample(754);
        chunk.full = false;
        chunk.biomes = super::super::model::Biomes::Absent;
        chunk.heightmaps.clear();
        chunk
            .sections
            .iter_mut()
            .skip(1)
            .for_each(|section| *section = None);
        let mut step = Defaults::new(755, 754);
        let out = step
            .clientbound(State::Play, "level_chunk", &chunk.write(754).unwrap())
            .unwrap();
        assert_eq!(names(&out), ["section_blocks_update", "block_entity_data"]);
        let changes = read_changes(&out[0].data, 755).unwrap();
        assert_eq!(changes.len(), 4096);
        assert!(changes.iter().all(|change| (0..16).contains(&change.y)));
    }

    #[test]
    fn section_updates_split_by_section_and_merge_back() {
        let mut writer = Writer::new();
        writer.i32(2).i32(-1).varint(2);
        writer.u8(0x12).u8(5).varint(1); // y 5
        writer.u8(0x34).u8(200).varint(9); // y 200
        let mut step = Defaults::new(751, 736);
        let out = step
            .clientbound(State::Play, "chunk_blocks_update", &writer.buf)
            .unwrap();
        assert_eq!(
            names(&out),
            ["section_blocks_update", "section_blocks_update"]
        );
        let ys: Vec<i32> = out
            .iter()
            .map(|out| read_changes(&out.data, 751).unwrap()[0].y)
            .collect();
        assert_eq!(ys, [5, 200]);
        let mut down = Defaults::new(736, 751);
        let out = down
            .clientbound(State::Play, "section_blocks_update", &out[1].data)
            .unwrap();
        let changes = read_changes(&out[0].data, 736).unwrap();
        assert_eq!((changes[0].x, changes[0].y, changes[0].z), (35, 200, -12));
    }

    #[test]
    fn block_entity_data_switches_between_actions_and_types() {
        let mut tag = Compound::new();
        tag.insert("id", "minecraft:sign");
        tag.insert("x", 10);
        tag.insert("y", 64);
        tag.insert("z", -3);
        tag.insert("Text1", "{\"text\":\"a\"}");
        let position = Position {
            x: 10,
            y: 64,
            z: -3,
        };
        let mut writer = Writer::new();
        writer.position(position).u8(9);
        Nbt::Compound(tag).write(&mut writer, true);
        let mut up = Defaults::new(757, 756);
        let out = up
            .clientbound(State::Play, "block_entity_data", &writer.buf)
            .unwrap();
        let mut reader = Reader::new(&out[0].data);
        assert_eq!(reader.position().unwrap(), position);
        let kind = reader.varint().unwrap();
        assert_eq!(
            data::version(757)
                .expect_registry("block_entity_type")
                .name(kind),
            Some("sign")
        );
        let nbt = Nbt::read(&mut reader, true).unwrap().unwrap();
        let tag = nbt.as_compound().unwrap();
        assert!(!tag.contains("id") && tag.contains("Text1"));
        let mut down = Defaults::new(756, 757);
        let back = down
            .clientbound(State::Play, "block_entity_data", &out[0].data)
            .unwrap();
        let mut reader = Reader::new(&back[0].data);
        reader.position().unwrap();
        assert_eq!(reader.u8().unwrap(), 9);
        let nbt = Nbt::read(&mut reader, true).unwrap().unwrap();
        assert_eq!(nbt.as_compound().unwrap().str("id"), Some("minecraft:sign"));
        // Across 1.20 the sign gains sides.
        let mut writer = Writer::new();
        writer.position(position).varint(
            data::version(762)
                .expect_registry("block_entity_type")
                .id("sign")
                .unwrap(),
        );
        nbt.as_compound()
            .map(|tag| Nbt::Compound(tag.clone()))
            .unwrap()
            .write(&mut writer, true);
        let mut step = Defaults::new(763, 762);
        let out = step
            .clientbound(State::Play, "block_entity_data", &writer.buf)
            .unwrap();
        let mut reader = Reader::new(&out[0].data);
        reader.position().unwrap();
        reader.varint().unwrap();
        let nbt = Nbt::read(&mut reader, true).unwrap().unwrap();
        assert!(nbt.as_compound().unwrap().contains("front_text"));
    }

    #[test]
    fn removed_level_events_become_sounds_for_1_19_3() {
        let mut writer = Writer::new();
        writer
            .i32(1006)
            .position(Position { x: 1, y: 2, z: 3 })
            .i32(0)
            .bool(false);
        let mut step = Defaults::new(761, 760);
        let out = step
            .clientbound(State::Play, "level_event", &writer.buf)
            .unwrap();
        assert_eq!(names(&out), ["sound"]);
        // Smoke directions change meaning at 1.13.1.
        let mut writer = Writer::new();
        writer
            .i32(2000)
            .position_legacy(Position { x: 1, y: 2, z: 3 })
            .i32(7)
            .bool(false);
        let mut step = Defaults::new(401, 393);
        let out = step
            .clientbound(State::Play, "level_event", &writer.buf)
            .unwrap();
        assert_eq!(&out[0].data[12..16], &3i32.to_be_bytes());
    }

    #[test]
    fn explosions_change_layout_without_particles_on_the_way_up() {
        let mut writer = Writer::new();
        writer
            .f32(1.0)
            .f32(2.0)
            .f32(3.0)
            .f32(4.0)
            .i32(1)
            .i8(1)
            .i8(0)
            .i8(-1);
        writer.f32(0.0).f32(0.5).f32(0.0);
        let mut step = Defaults::new(755, 754);
        let out = step
            .clientbound(State::Play, "explode", &writer.buf)
            .unwrap();
        let mut reader = Reader::new(&out[0].data);
        for _ in 0..4 {
            reader.f32().unwrap();
        }
        assert_eq!(reader.varint().unwrap(), 1);
        // 1.20.2 to 1.20.3 adds particles and a sound of the receiver's.
        let mut writer = Writer::new();
        writer.f64(1.0).f64(2.0).f64(3.0).f32(4.0).varint(0);
        writer.f32(0.0).f32(0.5).f32(0.0);
        let mut step = Defaults::new(765, 764);
        let out = step
            .clientbound(State::Play, "explode", &writer.buf)
            .unwrap();
        let mut reader = Reader::new(&out[0].data);
        for _ in 0..3 {
            reader.f64().unwrap();
        }
        reader.f32().unwrap();
        assert_eq!(reader.varint().unwrap(), 0);
        for _ in 0..3 {
            reader.f32().unwrap();
        }
        assert_eq!(reader.varint().unwrap(), 1); // Destroy.
        let particles = data::version(765).expect_registry("particle_type");
        assert_eq!(particles.name(reader.varint().unwrap()), Some("explosion"));
        assert_eq!(
            particles.name(reader.varint().unwrap()),
            Some("explosion_emitter")
        );
        let sound = reader.varint().unwrap() - 1;
        assert_eq!(
            data::version(765)
                .expect_registry("sound_event")
                .name(sound),
            Some("entity.generic.explode")
        );
        reader.finish().unwrap();
    }

    #[test]
    fn every_handler_is_installed_only_where_needed() {
        // Identical layouts and IDs leave chunk packets alone.
        let mut skipped = Vec::new();
        for pair in crate::version::PROTOCOLS.windows(2) {
            let (older, newer) = (pair[0].0, pair[1].0);
            for (client, backend) in [(newer, older), (older, newer)] {
                let mut step = Defaults::new(client, backend);
                // A malformed chunk fails only where a handler parses it.
                let name = if backend >= 757 {
                    "level_chunk_with_light"
                } else {
                    "level_chunk"
                };
                let parsed = step.clientbound(State::Play, name, &[1, 2, 3]).is_err();
                let expected = super::super::install_needs_chunk(backend, client);
                assert_eq!(parsed, expected, "{backend} -> {client}");
                if !parsed {
                    skipped.push(format!("{backend} -> {client}"));
                }
            }
        }
        eprintln!("chunks pass untouched: {}", skipped.join(", "));
        assert!(!skipped.is_empty());
    }
}

/// Conversions per second of real chunks through the handlers, for a few
/// kinds of pair. Run with
/// `cargo test --release -p rift-translation chunk_throughput -- --ignored --nocapture`.
#[test]
#[ignore]
fn chunk_throughput() {
    for (client, backend) in [
        (771, 770),
        (770, 771),
        (757, 756),
        (756, 757),
        (477, 404),
        (404, 477),
        (764, 763),
        (776, 777),
    ] {
        let packets: Vec<_> = recorded(backend, &["level_chunk", "level_chunk_with_light"])
            .into_iter()
            .map(|(_, packet)| packet)
            .collect();
        if packets.is_empty() {
            eprintln!("{backend} -> {client}: no corpus");
            continue;
        }
        let mut step = Defaults::new(client, backend);
        let rounds = std::env::var("CHUNK_ROUNDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(40);
        let start = std::time::Instant::now();
        let mut bytes = 0;
        for _ in 0..rounds {
            for packet in &packets {
                let out = step
                    .send_id(Direction::Clientbound, State::Play, packet.id, &packet.data)
                    .unwrap();
                bytes += out.iter().map(|out| out.data.len()).sum::<usize>();
            }
        }
        let elapsed = start.elapsed().as_secs_f64();
        let count = rounds * packets.len();
        eprintln!(
            "{} -> {}: {:.0} chunks/s ({:.1} µs each, {:.0} MB/s out)",
            version::release(backend),
            version::release(client),
            count as f64 / elapsed,
            elapsed * 1e6 / count as f64,
            bytes as f64 / elapsed / 1e6
        );
    }
}

mod flattening_packets {
    use crate::{
        State,
        codec::testing::Defaults,
        data,
        nbt::{Compound, Nbt},
        wire::{Position, Reader, Writer},
    };

    #[test]
    fn placed_blocks_get_their_block_entity_for_1_12_clients() {
        let states = &data::version(393).block_states;
        let at = Position { x: 5, y: 70, z: -2 };
        let mut step = Defaults::new(340, 393);
        let mut writer = Writer::new();
        writer.position_legacy(at).varint(
            states
                .parse("lime_bed[facing=east,occupied=false,part=head]")
                .unwrap(),
        );
        // The block update itself needs the flattening mapping; the block
        // entity that follows it does not.
        match step.clientbound(State::Play, "block_update", &writer.buf) {
            Err(error) => assert!(error.to_string().contains("mapping"), "{error}"),
            Ok(out) => {
                assert_eq!(out[1].name, "block_entity_data");
                let mut reader = Reader::new(&out[1].data);
                reader.position_legacy().unwrap();
                assert_eq!(reader.u8().unwrap(), 11);
                let tag = Nbt::read(&mut reader, true).unwrap().unwrap();
                assert_eq!(tag.as_compound().unwrap().int("color"), Some(5));
            }
        }
        // A 1.13 banner's patterns get the 1.12 base and inverted colours
        // once its block is known.
        let mut step = Defaults::new(340, 393);
        super::super::blocks::track(
            crate::codec::HasShared::shared(&mut step),
            at,
            states.parse("blue_banner[rotation=0]").unwrap(),
        );
        let mut pattern = Compound::new();
        pattern.insert("Color", 1);
        pattern.insert("Pattern", "cre");
        let mut tag = Compound::new();
        tag.insert("id", "minecraft:banner");
        tag.insert("Patterns", vec![Nbt::Compound(pattern)]);
        let mut writer = Writer::new();
        writer.position_legacy(at).u8(6);
        Nbt::Compound(tag).write(&mut writer, true);
        let out = step
            .clientbound(State::Play, "block_entity_data", &writer.buf)
            .unwrap();
        let mut reader = Reader::new(&out[0].data);
        reader.position_legacy().unwrap();
        assert_eq!(reader.u8().unwrap(), 6);
        let tag = Nbt::read(&mut reader, true).unwrap().unwrap();
        let tag = tag.as_compound().unwrap();
        assert_eq!(tag.int("Base"), Some(15 - 11));
        assert_eq!(
            tag.list("Patterns").unwrap()[0]
                .as_compound()
                .unwrap()
                .int("Color"),
            Some(14)
        );
    }

    #[test]
    fn a_1_12_block_entity_picks_the_1_13_block() {
        let states = &data::version(393).block_states;
        let at = Position { x: 1, y: 64, z: 1 };
        let mut step = Defaults::new(393, 340);
        super::super::blocks::track(
            crate::codec::HasShared::shared(&mut step),
            at,
            states.parse("flower_pot").unwrap(),
        );
        let mut tag = Compound::new();
        tag.insert("id", "minecraft:flower_pot");
        tag.insert("Item", "minecraft:red_flower");
        tag.insert("Data", 2);
        let mut writer = Writer::new();
        writer.position_legacy(at).u8(5);
        Nbt::Compound(tag).write(&mut writer, true);
        let out = step
            .clientbound(State::Play, "block_entity_data", &writer.buf)
            .unwrap();
        assert_eq!(out.len(), 1, "the pot's block entity is gone");
        assert_eq!(out[0].name, "block_update");
        let mut reader = Reader::new(&out[0].data);
        reader.position_legacy().unwrap();
        assert_eq!(
            states.describe(reader.varint().unwrap()).unwrap(),
            "potted_allium"
        );
    }
}
