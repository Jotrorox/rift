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
