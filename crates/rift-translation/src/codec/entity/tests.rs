use super::{metadata, packets, particle, spawn, tables};
use crate::{
    Direction, Packet, State as Phase,
    codec::{
        HasShared, Shared,
        testing::{Defaults, Recorded, corpus},
    },
    data,
    step::{Cx, Emitted},
    version::PROTOCOLS,
    wire::{Reader, Writer},
};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io,
};

const SPAWNS: &[&str] = &[
    "add_entity",
    "add_mob",
    "add_player",
    "add_painting",
    "add_experience_orb",
    "add_global_entity",
];

const OWN: &[&str] = &[
    "add_entity",
    "add_mob",
    "add_player",
    "add_painting",
    "add_experience_orb",
    "add_global_entity",
    "set_entity_data",
    "remove_entities",
    "remove_entity",
    "level_particles",
    "update_mob_effect",
    "remove_mob_effect",
    "update_attributes",
];

struct Probe {
    shared: Shared,
}

impl HasShared for Probe {
    fn shared(&mut self) -> &mut Shared {
        &mut self.shared
    }
}

type Handler = fn(&mut Probe, &mut Cx<'_>, Phase, Packet) -> io::Result<()>;

fn handler(name: &str) -> Option<Handler> {
    Some(match name {
        "set_entity_data" => packets::set_entity_data::<Probe>,
        "remove_entities" => packets::remove_entities::<Probe>,
        "remove_entity" => packets::remove_entity::<Probe>,
        "level_particles" => packets::level_particles::<Probe>,
        "update_mob_effect" => packets::update_mob_effect::<Probe>,
        "remove_mob_effect" => packets::remove_mob_effect::<Probe>,
        "update_attributes" => packets::update_attributes::<Probe>,
        _ => return None,
    })
}

/// Read a clientbound packet of `protocol` with this codec's reader and
/// write it back in the same protocol.
fn round_trip(
    protocol: i32,
    types: &Types,
    name: &str,
    body: &[u8],
) -> io::Result<Vec<(String, Vec<u8>)>> {
    let mut probe = Probe {
        shared: Shared::new(protocol, protocol),
    };
    for (id, kind) in &types.0 {
        probe.shared.entity.track(
            *id,
            super::Tracked {
                backend: kind,
                client: kind,
            },
        );
    }
    if SPAWNS.contains(&name) {
        let read = spawn::read(protocol, name, body)?;
        return Ok(
            spawn::encode(&mut probe.shared, Direction::Clientbound, read)?
                .into_iter()
                .map(|(name, body)| (name.to_owned(), body))
                .collect(),
        );
    }
    let handler = handler(name).expect("own packet");
    let mut emitted = VecDeque::new();
    let mut cx = Cx {
        emitted: &mut emitted,
        client_protocol: protocol,
        backend_protocol: protocol,
    };
    let id = data::version(protocol).expect_packet(Phase::Play, Direction::Clientbound, name);
    handler(
        &mut probe,
        &mut cx,
        Phase::Play,
        Packet::new(id, body.to_vec()),
    )?;
    Ok(emitted
        .into_iter()
        .map(|Emitted { packet, .. }| {
            (
                data::version(protocol)
                    .packet_name(Phase::Play, Direction::Clientbound, packet.id)
                    .unwrap_or("?")
                    .to_owned(),
                packet.data,
            )
        })
        .collect())
}

/// Errors that come from codecs other than this one not being written yet.
fn foreign(error: &io::Error) -> bool {
    error.to_string().contains("not implemented")
}

fn clientbound_play(packets: &[Recorded]) -> impl Iterator<Item = &Recorded> {
    packets
        .iter()
        .filter(|packet| packet.direction == Direction::Clientbound && packet.state == Phase::Play)
}

/// Follows entity types through a session's spawns, as a client would.
#[derive(Default)]
struct Types(HashMap<i32, &'static str>);

impl Types {
    fn observe(&mut self, protocol: i32, name: &str, body: &[u8]) {
        if name == "login" {
            self.0.clear();
            self.0.insert(Reader::new(body).i32().unwrap(), "player");
        } else if name == "respawn" {
            self.0.retain(|_, kind| *kind == "player");
        } else if SPAWNS.contains(&name)
            && let Ok(read) = spawn::read(protocol, name, body)
        {
            self.0.insert(read.id, read.kind);
        }
    }
}

/// Check one packet's entity data against the tables; the number of
/// entries checked, `None` when the entity's type is unknown.
fn check_data(protocol: i32, types: &Types, name: &str, body: &[u8]) -> io::Result<Option<usize>> {
    let mut reader = Reader::new(body);
    let kind = if name == "set_entity_data" {
        let id = reader.varint()?;
        types.0.get(&id).copied()
    } else {
        let read = spawn::read(protocol, name, body)?;
        let Some(raw) = read.metadata else {
            return Ok(Some(0));
        };
        let mut reader = Reader::new(&raw);
        let count = metadata::check(protocol, read.kind, &mut reader)?;
        reader.finish()?;
        return Ok(Some(count));
    };
    let Some(kind) = kind else {
        return Ok(None);
    };
    let count = metadata::check(protocol, kind, &mut reader)?;
    reader.finish()?;
    Ok(Some(count))
}

#[test]
fn tables_cover_every_protocol_and_entity_type() {
    for &(protocol, name) in PROTOCOLS {
        let tables = tables::tables(protocol);
        let registry = data::version(protocol).expect_registry("entity_type");
        for (_, kind) in registry.names() {
            let fields = tables
                .fields(kind)
                .unwrap_or_else(|| panic!("{name}: no tables for {kind}"));
            assert!(fields.starts_with(tables.root()), "{name} {kind}");
            assert!(fields.len() < 255, "{name} {kind}");
            for field in fields {
                assert!(
                    tables.serializer_id(field.kind).is_some(),
                    "{name} {kind} {}",
                    field.key
                );
            }
        }
        let player = tables.fields("player").expect("player");
        assert!(
            player
                .iter()
                .any(|field| field.key == "LivingEntity.DATA_HEALTH_ID")
        );
        assert_eq!(tables.root()[0].key, "Entity.DATA_SHARED_FLAGS_ID");
    }
}

#[test]
fn known_indexes_match_the_protocol_documentation() {
    // Facts from each version's protocol documentation.
    let index = |protocol: i32, kind: &str, key: &str| {
        tables::tables(protocol)
            .fields(kind)
            .unwrap()
            .iter()
            .position(|field| field.key == key)
    };
    // 1.12: health 7, creeper state 12; 1.14 added pose (6) and the bed.
    assert_eq!(
        index(340, "creeper", "LivingEntity.DATA_HEALTH_ID"),
        Some(7)
    );
    assert_eq!(index(340, "creeper", "Creeper.DATA_SWELL_DIR"), Some(12));
    assert_eq!(index(477, "creeper", "Entity.DATA_POSE"), Some(6));
    assert_eq!(index(477, "creeper", "Creeper.DATA_SWELL_DIR"), Some(14));
    // 1.17 added ticks frozen at 7; 1.20.5 effect particles stay at 10.
    assert_eq!(index(755, "zombie", "Entity.DATA_TICKS_FROZEN"), Some(7));
    assert_eq!(
        index(766, "zombie", "LivingEntity.DATA_EFFECT_PARTICLES"),
        Some(10)
    );
    // The item of an item entity, every version.
    assert_eq!(index(340, "item", "ItemEntity.DATA_ITEM"), Some(6));
    assert_eq!(index(777, "item", "ItemEntity.DATA_ITEM"), Some(8));
    // Serializer IDs: 1.12 has no optional component; 1.19.3 added long.
    let serializer = |protocol: i32, kind| tables::tables(protocol).serializer_id(kind);
    assert_eq!(serializer(340, tables::Kind::ItemStack), Some(5));
    assert_eq!(serializer(393, tables::Kind::ItemStack), Some(6));
    assert_eq!(serializer(761, tables::Kind::Long), Some(2));
    assert_eq!(serializer(766, tables::Kind::Particles), Some(18));
    assert_eq!(serializer(773, tables::Kind::CompoundTag), None);
}

#[test]
fn every_pair_installs_without_problems() {
    for pair in PROTOCOLS.windows(2) {
        let (older, newer) = (pair[0].0, pair[1].0);
        for (client, backend) in [(newer, older), (older, newer)] {
            let routes = crate::step::RouteBuilder::<Probe>::new(client, backend)
                .defaults()
                .build();
            let ours: Vec<&String> = routes
                .problems()
                .iter()
                .filter(|problem| {
                    OWN.iter()
                        .any(|name| problem.contains(&format!(" {name} ")))
                })
                .collect();
            assert!(ours.is_empty(), "{client} <- {backend}: {ours:?}");
        }
    }
}

#[test]
fn low_precision_vectors_round_trip() {
    for vector in [
        [0.0, 0.0, 0.0],
        [0.1, -0.2, 0.3],
        [1.5, 0.0, -3.25],
        [-7.0, 12.0, 0.004],
        [100.0, -250.5, 3.0],
    ] {
        let mut writer = Writer::new();
        spawn::write_lp_vec3(vector, &mut writer);
        let decoded = spawn::read_lp_vec3(&mut Reader::new(&writer.buf)).unwrap();
        let scale = vector
            .iter()
            .fold(1.0f64, |max, axis| max.max(axis.abs().ceil()));
        for (axis, back) in vector.iter().zip(decoded) {
            assert!(
                (axis - back).abs() <= scale / 16000.0,
                "{vector:?} -> {decoded:?}"
            );
        }
        let mut again = Writer::new();
        spawn::write_lp_vec3(decoded, &mut again);
        assert_eq!(again.buf, writer.buf, "{vector:?}");
    }
    let mut zero = Writer::new();
    spawn::write_lp_vec3([0.0; 3], &mut zero);
    assert_eq!(zero.buf, [0]);
}

#[test]
fn attribute_names_and_modifiers_convert() {
    use packets::{canonical, modifier_id, modifier_uuid};
    assert_eq!(canonical("generic.maxHealth"), "max_health");
    assert_eq!(
        canonical("minecraft:generic.movement_speed"),
        "movement_speed"
    );
    assert_eq!(
        canonical("player.block_interaction_range"),
        "block_interaction_range"
    );
    assert_eq!(canonical("horse.jump_strength"), "jump_strength");
    assert_eq!(canonical("scale"), "scale");
    let sprinting = 0x662a6b8d_da3e_4c1c_8813_96ea6097278d;
    assert_eq!(modifier_id(sprinting), "minecraft:sprinting");
    assert_eq!(modifier_uuid("minecraft:sprinting"), sprinting);
    assert_eq!(modifier_uuid(&modifier_id(42)), 42);
    let derived = modifier_uuid("minecraft:effect.speed");
    assert_eq!(derived, modifier_uuid("minecraft:effect.speed"));
    assert_eq!((derived >> 76) & 0xf, 3);
}

/// Real packets read and written back unchanged, for every protocol whose
/// session was recorded. Skips protocols without a recording.
#[test]
fn corpus_round_trips() {
    let mut report = BTreeMap::new();
    let mut failures = Vec::new();
    for &(protocol, release) in PROTOCOLS {
        let Some(packets) = corpus(protocol) else {
            eprintln!("corpus for {release} ({protocol}) is not recorded here; skipped");
            continue;
        };
        let (mut checked, mut skipped) = (0, 0);
        let mut types = Types::default();
        for packet in clientbound_play(&packets) {
            let name = packet.name(protocol);
            types.observe(protocol, name, &packet.data);
            if !OWN.contains(&name) {
                continue;
            }
            match round_trip(protocol, &types, name, &packet.data) {
                Ok(out) if out == [(name.to_owned(), packet.data.clone())] => checked += 1,
                Ok(out) => failures.push(format!(
                    "{release} {name}: {} -> {:?}",
                    hex(&packet.data),
                    out.iter()
                        .map(|(name, body)| format!("{name} {}", hex(body)))
                        .collect::<Vec<_>>()
                )),
                Err(error) if foreign(&error) => skipped += 1,
                Err(error) => {
                    failures.push(format!("{release} {name}: {error}: {}", hex(&packet.data)))
                }
            }
        }
        report.insert(protocol, (checked, skipped));
    }
    eprintln!("round trips per protocol (checked, waiting on other codecs): {report:?}");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Every entity data entry of every recorded session is explained by the
/// tables: the index exists for the entity's type and the serializer
/// matches.
#[test]
fn corpus_entity_data_matches_tables() {
    let mut report = BTreeMap::new();
    let mut failures = Vec::new();
    for &(protocol, release) in PROTOCOLS {
        let Some(packets) = corpus(protocol) else {
            continue;
        };
        let mut types = Types::default();
        let (mut entities, mut entries, mut untracked, mut skipped) = (0, 0, 0, 0);
        for packet in clientbound_play(&packets) {
            let name = packet.name(protocol);
            types.observe(protocol, name, &packet.data);
            if SPAWNS.contains(&name) {
                entities += 1;
            }
            if name != "set_entity_data" && !matches!(name, "add_mob" | "add_player") {
                continue;
            }
            match check_data(protocol, &types, name, &packet.data) {
                Ok(Some(count)) => entries += count,
                Ok(None) => untracked += 1,
                Err(error) if foreign(&error) => skipped += 1,
                Err(error) => {
                    failures.push(format!("{release} {name}: {error}: {}", hex(&packet.data)))
                }
            }
        }
        report.insert(protocol, (entities, entries, untracked, skipped));
    }
    eprintln!(
        "entity data per protocol (entities spawned, entries checked, packets for untracked entities, waiting on other codecs): {report:?}"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Real packets converted to each adjacent protocol, both ways, come out as
/// packets this codec reads in the receiver's protocol, with entity data
/// that fits the receiver's tables.
#[test]
fn corpus_converts_between_adjacent_protocols() {
    let mut report = BTreeMap::new();
    let mut failures = Vec::new();
    // With RIFT_ENTITY_DUMP set, the converted packets are written there for
    // `scripts/translation_entities.py --validate` to decode with the
    // minecraft-data schemas.
    let mut dump = std::env::var_os("RIFT_ENTITY_DUMP")
        .map(|path| std::io::BufWriter::new(std::fs::File::create(path).expect("dump file")));
    for pair in PROTOCOLS.windows(2) {
        let (older, newer) = (pair[0].0, pair[1].0);
        for (client, backend) in [(newer, older), (older, newer)] {
            let Some(packets) = corpus(backend) else {
                continue;
            };
            let mut step = Defaults::new(client, backend);
            let mut types = Types::default();
            let (mut converted, mut skipped) = (0, 0);
            for packet in clientbound_play(&packets) {
                let name = packet.name(backend);
                let own = OWN.contains(&name);
                let out = match step.send_id(
                    Direction::Clientbound,
                    Phase::Play,
                    packet.id,
                    &packet.data,
                ) {
                    Ok(out) => out,
                    Err(error) if own && foreign(&error) => {
                        skipped += 1;
                        continue;
                    }
                    Err(error) if own => {
                        failures.push(format!(
                            "{backend}->{client} {name}: {error}: {}",
                            hex(&packet.data)
                        ));
                        continue;
                    }
                    Err(_) => continue,
                };
                for sent in out {
                    types.observe(client, &sent.name, &sent.data);
                    if !OWN.contains(&sent.name.as_str()) {
                        continue;
                    }
                    converted += 1;
                    if let Some(dump) = &mut dump {
                        use std::io::Write;
                        let id = data::version(client).expect_packet(
                            Phase::Play,
                            Direction::Clientbound,
                            &sent.name,
                        );
                        writeln!(
                            dump,
                            r#"{{"from":{backend},"protocol":{client},"id":{id},"name":"{}","hex":"{}"}}"#,
                            sent.name,
                            hex(&sent.data)
                        )
                        .expect("dump written");
                    }
                    let result =
                        round_trip(client, &types, &sent.name, &sent.data).and_then(|back| {
                            if back != [(sent.name.clone(), sent.data.clone())] {
                                return Err(io::Error::other(format!(
                                    "does not round trip: {back:?}"
                                )));
                            }
                            if sent.name == "set_entity_data"
                                || matches!(sent.name.as_str(), "add_mob" | "add_player")
                            {
                                check_data(client, &types, &sent.name, &sent.data)?;
                            }
                            Ok(())
                        });
                    match result {
                        Ok(()) => {}
                        Err(error) if foreign(&error) => skipped += 1,
                        Err(error) => failures.push(format!(
                            "{backend}->{client} {name} as {}: {error}: {} from {}",
                            sent.name,
                            hex(&sent.data),
                            hex(&packet.data)
                        )),
                    }
                }
            }
            report.insert(format!("{backend}->{client}"), (converted, skipped));
        }
    }
    eprintln!("converted packets per step (checked, waiting on other codecs): {report:?}");
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Truncated and corrupted real packets fail or convert, never panic.
#[test]
fn malformed_input_does_not_panic() {
    for &(protocol, _) in PROTOCOLS {
        let Some(packets) = corpus(protocol) else {
            continue;
        };
        let mut seen = 0;
        for packet in clientbound_play(&packets) {
            let name = packet.name(protocol);
            if !OWN.contains(&name) || seen > 40 {
                continue;
            }
            seen += 1;
            for cut in 0..packet.data.len() {
                let _ = round_trip(protocol, &Types::default(), name, &packet.data[..cut]);
            }
            for position in 0..packet.data.len().min(24) {
                let mut corrupt = packet.data.clone();
                corrupt[position] ^= 0xa5;
                let _ = round_trip(protocol, &Types::default(), name, &corrupt);
            }
        }
    }
    // Hand-made garbage for every own packet in a few protocols.
    for protocol in [335, 393, 477, 759, 766, 773, 777] {
        for name in OWN {
            if data::version(protocol)
                .packet_id(Phase::Play, Direction::Clientbound, name)
                .is_none()
            {
                continue;
            }
            for body in [
                &[][..],
                &[0xff; 9],
                &[0x80; 32],
                &[1, 2, 3, 0xff, 0x7f, 0, 0, 0],
            ] {
                let _ = round_trip(protocol, &Types::default(), name, body);
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

#[test]
fn particle_names_cross_1_13() {
    let reddust = data::version(340)
        .expect_registry("particle_type")
        .id("reddust")
        .unwrap();
    let dust = particle::upgrade_legacy(reddust, [1.0, 0.0, 0.5]).unwrap();
    assert_eq!(
        data::version(393)
            .expect_registry("particle_type")
            .name(dust.id),
        Some("dust")
    );
    assert_eq!(
        particle::downgrade_legacy(&dust),
        Some((reddust, Some([1.0, 0.0, 0.5])))
    );
    let _ = unhex("00");
}

// Boundaries, with hand-made and recorded packets, so that they are covered
// without the corpus.

fn names(out: &[crate::codec::testing::Out]) -> Vec<&str> {
    out.iter().map(|out| out.name.as_str()).collect()
}

/// The `login` the official server of `protocol` sent in the recorded
/// session (player entity 1, survival, overworld).
fn login(step: &mut Defaults, protocol: i32) {
    let recorded = match protocol {
        767 => {
            "000000010003136d696e6563726166743a6f766572776f726c64116d696e6563726166743a7468655f656e\
             64146d696e6563726166743a7468655f6e657468657214020200010000136d696e6563726166743a6f7665\
             72776f726c643947aa214a8fe62d01ff0001000000"
        }
        other => panic!("no recorded login for protocol {other}"),
    };
    step.clientbound(Phase::Play, "login", &unhex(recorded))
        .unwrap();
}

#[test]
fn custom_names_become_components_in_1_13() {
    let mut body = Writer::new();
    body.varint(5).u8(2).varint(3).string("Bob").u8(0xff);
    let out = Defaults::new(393, 340)
        .clientbound(Phase::Play, "set_entity_data", &body.buf)
        .unwrap();
    let mut expected = Writer::new();
    expected
        .varint(5)
        .u8(2)
        .varint(5)
        .bool(true)
        .string(r#"{"text":"Bob"}"#)
        .u8(0xff);
    assert_eq!(names(&out), ["set_entity_data"]);
    assert_eq!(out[0].data, expected.buf);
    // And back: the component's text becomes the name.
    let back = Defaults::new(340, 393)
        .clientbound(Phase::Play, "set_entity_data", &expected.buf)
        .unwrap();
    assert_eq!(back[0].data, body.buf);
}

#[test]
fn spawn_data_moves_out_of_spawn_packets_in_1_15() {
    // A recorded 1.14.4 villager spawn with its entity data.
    let recorded = unhex(
        "1270e7c5ac39794046b514b283624381c65440659000000000004010000000000000c064d0000000000000000000000000000000000001010002050003070004070005070006120007000008\
         0241a000000901000a07000b01000c0a000d00010e07000f01001010020001ff",
    );
    let mut step = Defaults::new(573, 498);
    let out = step.clientbound(Phase::Play, "add_mob", &recorded).unwrap();
    assert_eq!(names(&out), ["add_mob", "set_entity_data"]);
    let mut types = Types::default();
    types.observe(573, "add_mob", &out[0].data);
    assert_eq!(types.0.values().next(), Some(&"villager"));
    // Every entry fits 1.15's villager, which gained the stinger count.
    assert!(
        check_data(573, &types, "set_entity_data", &out[1].data)
            .unwrap()
            .unwrap()
            > 10
    );
    // Back to 1.14.4: the spawn carries an empty list, data follows apart.
    let back = Defaults::new(498, 573)
        .clientbound(Phase::Play, "add_mob", &out[0].data)
        .unwrap();
    assert_eq!(names(&back), ["add_mob"]);
    assert_eq!(back[0].data.last(), Some(&0xff));
}

#[test]
fn mobs_and_paintings_merge_into_add_entity_in_1_19() {
    let recorded = unhex(
        "0e6b0e8ff560e243e496518f25e20c6e1d6bc016000000000000c04e000000000000401e000000000000000000000000000000",
    );
    let out = Defaults::new(759, 758)
        .clientbound(Phase::Play, "add_mob", &recorded)
        .unwrap();
    assert_eq!(names(&out), ["add_entity"]);
    let spawn = spawn::read(759, "add_entity", &out[0].data).unwrap();
    assert_eq!(spawn.id, 14);
    // A painting's motive moves to entity data.
    let painting = unhex("1c978547e80bf04eb89ffdfa2eb85f2aac00fffffd4000004fc500");
    let mut step = Defaults::new(759, 758);
    let out = step
        .clientbound(Phase::Play, "add_painting", &painting)
        .unwrap();
    assert_eq!(names(&out), ["add_entity", "set_entity_data"]);
    // Down again: a placeholder motive, then the real one from the data.
    let mut down = Defaults::new(758, 759);
    let first = down
        .clientbound(Phase::Play, "add_entity", &out[0].data)
        .unwrap();
    assert_eq!(names(&first), ["add_painting"]);
    let second = down
        .clientbound(Phase::Play, "set_entity_data", &out[1].data)
        .unwrap();
    assert_eq!(names(&second), ["remove_entities", "add_painting"]);
    assert_eq!(second[1].data[..1], painting[..1]);
    let original = spawn::read(758, "add_painting", &painting).unwrap();
    let repainted = spawn::read(758, "add_painting", &second[1].data).unwrap();
    assert_eq!(original.id, repainted.id);
}

#[test]
fn players_and_experience_orbs_merge_into_add_entity() {
    let mut body = Writer::new();
    body.varint(7)
        .uuid(42)
        .f64(1.0)
        .f64(64.0)
        .f64(-3.5)
        .i8(10)
        .i8(-5);
    let out = Defaults::new(764, 763)
        .clientbound(Phase::Play, "add_player", &body.buf)
        .unwrap();
    assert_eq!(names(&out), ["add_entity"]);
    assert_eq!(
        spawn::read(764, "add_entity", &out[0].data).unwrap().kind,
        "player"
    );
    let back = Defaults::new(763, 764)
        .clientbound(Phase::Play, "add_entity", &out[0].data)
        .unwrap();
    assert_eq!(names(&back), ["add_player"]);
    assert_eq!(back[0].data, body.buf);

    let orb = unhex("1cc012000000000000c04e00000000000040160000000000000005");
    let out = Defaults::new(770, 769)
        .clientbound(Phase::Play, "add_experience_orb", &orb)
        .unwrap();
    assert_eq!(names(&out), ["add_entity", "set_entity_data"]);
    let mut types = Types::default();
    types.observe(770, "add_entity", &out[0].data);
    assert_eq!(
        check_data(770, &types, "set_entity_data", &out[1].data).unwrap(),
        Some(1)
    );
}

#[test]
fn velocity_becomes_a_low_precision_vector_in_1_21_9() {
    let mut body = Writer::new();
    let zombie = data::version(772)
        .expect_registry("entity_type")
        .id("zombie")
        .unwrap();
    body.varint(3)
        .uuid(9)
        .varint(zombie)
        .f64(0.5)
        .f64(70.0)
        .f64(0.5);
    body.i8(0)
        .i8(64)
        .i8(64)
        .varint(0)
        .i16(800)
        .i16(-1600)
        .i16(0);
    let out = Defaults::new(773, 772)
        .clientbound(Phase::Play, "add_entity", &body.buf)
        .unwrap();
    let mut reader = Reader::new(&out[0].data);
    reader.varint().unwrap();
    reader.uuid().unwrap();
    reader.varint().unwrap();
    reader.take(24).unwrap();
    let velocity = spawn::read_lp_vec3(&mut reader).unwrap();
    assert!((velocity[0] - 0.1).abs() < 1e-3 && (velocity[1] + 0.2).abs() < 1e-3);
    let back = Defaults::new(772, 773)
        .clientbound(Phase::Play, "add_entity", &out[0].data)
        .unwrap();
    assert_eq!(back[0].data, body.buf);
    // A recorded 1.21.9 spawn reads and writes back.
    let recorded = unhex(
        "028e0bf2b591cc492ea23f29b20f366a9146400c000000000000c04d570a3d8000004025000000000000e900a6eb10ea00a20000",
    );
    let read = round_trip(773, &Types::default(), "add_entity", &recorded).unwrap();
    assert_eq!(read, [("add_entity".to_owned(), recorded)]);
}

#[test]
fn poses_follow_their_names() {
    // 1.19.3 inserted SITTING before ROARING.
    let roaring = |protocol: i32| {
        tables::tables(protocol)
            .constants(tables::Kind::Pose)
            .iter()
            .position(|name| *name == "ROARING")
            .unwrap() as i32
    };
    let (pose, serializer) = (
        6,
        tables::tables(760)
            .serializer_id(tables::Kind::Pose)
            .unwrap(),
    );
    let mut body = Writer::new();
    body.varint(5)
        .u8(pose)
        .varint(serializer)
        .varint(roaring(760))
        .u8(0xff);
    let out = Defaults::new(761, 760)
        .clientbound(Phase::Play, "set_entity_data", &body.buf)
        .unwrap();
    let mut expected = Writer::new();
    expected
        .varint(5)
        .u8(pose)
        .varint(
            tables::tables(761)
                .serializer_id(tables::Kind::Pose)
                .unwrap(),
        )
        .varint(roaring(761))
        .u8(0xff);
    assert_eq!(out[0].data, expected.buf);
    assert_ne!(roaring(760), roaring(761));
}

#[test]
fn dust_colours_become_ints_in_1_21_2() {
    let dust = |protocol: i32| {
        data::version(protocol)
            .expect_registry("particle_type")
            .id("dust")
            .unwrap()
    };
    let mut body = Writer::new();
    body.bool(false).f64(0.0).f64(64.0).f64(0.0);
    body.f32(0.0).f32(0.0).f32(0.0).f32(0.0).i32(1);
    body.varint(dust(767)).f32(1.0).f32(0.0).f32(0.0).f32(2.0);
    let out = Defaults::new(768, 767)
        .clientbound(Phase::Play, "level_particles", &body.buf)
        .unwrap();
    let mut expected = Writer::new();
    expected.bool(false).f64(0.0).f64(64.0).f64(0.0);
    expected.f32(0.0).f32(0.0).f32(0.0).f32(0.0).i32(1);
    expected.varint(dust(768)).i32(0xff0000).f32(2.0);
    assert_eq!(out[0].data, expected.buf);
    // A recorded 26.3 packet (particle first, speeds per axis) reads back.
    let recorded = unhex(
        "15ffff00003f8000000000bfe0000000000000c04d800000000000400c0000000000000000000000000000000000003f8000003f8000003f8000000500",
    );
    assert_eq!(
        round_trip(777, &Types::default(), "level_particles", &recorded).unwrap(),
        [("level_particles".to_owned(), recorded.clone())]
    );
    let down = Defaults::new(776, 777)
        .clientbound(Phase::Play, "level_particles", &recorded)
        .unwrap();
    assert_eq!(names(&down), ["level_particles"]);
}

#[test]
fn item_particles_cross_the_26_1_item_template() {
    // Recorded diamond particles: 1.21.11 sends a stack (count, item 893,
    // empty patch), 26.1 a template (item 894, count, empty patch).
    let head = "00003ff8000000000000c04d80000000000040250000000000000000000000000000000000003f80000000000005";
    let older = unhex(&format!("{head}2f01fd060000"));
    let newer = unhex(&format!("{head}2ffe06010000"));
    let down = Defaults::new(774, 775)
        .clientbound(Phase::Play, "level_particles", &newer)
        .unwrap();
    assert_eq!(hex(&down[0].data), hex(&older));
    let up = Defaults::new(775, 774)
        .clientbound(Phase::Play, "level_particles", &older)
        .unwrap();
    assert_eq!(hex(&up[0].data), hex(&newer));
}

#[test]
fn attributes_and_effects_follow_their_layouts() {
    // 1.16 namespaced and renamed attribute keys.
    let mut body = Writer::new();
    body.varint(1)
        .i32(1)
        .string("generic.maxHealth")
        .f64(20.0)
        .varint(0);
    let out = Defaults::new(735, 578)
        .clientbound(Phase::Play, "update_attributes", &body.buf)
        .unwrap();
    let mut expected = Writer::new();
    expected
        .varint(1)
        .i32(1)
        .string("minecraft:generic.max_health")
        .f64(20.0)
        .varint(0);
    assert_eq!(out[0].data, expected.buf);
    // 1.21 identified modifiers; a recorded 1.20.5 packet converts.
    let recorded = unhex(
        "0103113fb99999a0000000000740080000000000000198491ef697b14584ae8271a8cc85cf7340000000000000000006401200000000000001736565d2e1a7403da3f81aeb3e3025423fe000000000000000",
    );
    let out = Defaults::new(767, 766)
        .clientbound(Phase::Play, "update_attributes", &recorded)
        .unwrap();
    let back = Defaults::new(766, 767)
        .clientbound(Phase::Play, "update_attributes", &out[0].data)
        .unwrap();
    assert_eq!(back[0].data, recorded);
    // 1.20.5 widened the amplifier and dropped the factor data.
    let mut effect = Writer::new();
    effect
        .varint(1)
        .varint(0)
        .u8(1)
        .varint(600)
        .u8(6)
        .bool(false);
    let out = Defaults::new(766, 765)
        .clientbound(Phase::Play, "update_mob_effect", &effect.buf)
        .unwrap();
    let mut expected = Writer::new();
    expected.varint(1).varint(0).varint(1).varint(600).u8(6);
    assert_eq!(out[0].data, expected.buf);
}

#[test]
fn boats_split_by_wood_in_1_21_2() {
    let registry = |protocol: i32, name: &str| {
        data::version(protocol)
            .expect_registry("entity_type")
            .id(name)
            .unwrap()
    };
    let mut birch = Writer::new();
    birch.varint(8).uuid(1).varint(registry(768, "birch_boat"));
    birch
        .f64(0.0)
        .f64(63.0)
        .f64(0.0)
        .i8(0)
        .i8(0)
        .i8(0)
        .varint(0);
    birch.i16(0).i16(0).i16(0);
    let mut down = Defaults::new(767, 768);
    let out = down
        .clientbound(Phase::Play, "add_entity", &birch.buf)
        .unwrap();
    assert_eq!(names(&out), ["add_entity", "set_entity_data"]);
    assert_eq!(
        spawn::read(767, "add_entity", &out[0].data).unwrap().kind,
        "boat"
    );
    // Up again: the spawn is an oak boat until the wood arrives.
    let mut up = Defaults::new(768, 767);
    login(&mut up, 767);
    let first = up
        .clientbound(Phase::Play, "add_entity", &out[0].data)
        .unwrap();
    assert_eq!(
        spawn::read(768, "add_entity", &first[0].data).unwrap().kind,
        "oak_boat"
    );
    let second = up
        .clientbound(Phase::Play, "set_entity_data", &out[1].data)
        .unwrap();
    assert_eq!(names(&second), ["remove_entities", "add_entity"]);
    assert_eq!(second[1].data, birch.buf);
}

#[test]
fn removals_split_for_1_17() {
    let mut body = Writer::new();
    body.varint(2).varint(4).varint(5);
    let out = Defaults::new(755, 754)
        .clientbound(Phase::Play, "remove_entities", &body.buf)
        .unwrap();
    assert_eq!(names(&out), ["remove_entity", "remove_entity"]);
    let back = Defaults::new(754, 755)
        .clientbound(Phase::Play, "remove_entity", &out[1].data)
        .unwrap();
    assert_eq!(back[0].data, [1, 5]);
}
