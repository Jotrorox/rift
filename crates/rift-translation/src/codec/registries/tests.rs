use super::*;
use crate::{
    State as Phase,
    codec::testing::{self, Defaults},
    nbt::Nbt,
    version::PROTOCOLS,
    wire::Rewriter,
};
use form::Sync;

/// Every protocol that synchronises registries, oldest first.
fn synced_protocols() -> impl Iterator<Item = i32> {
    PROTOCOLS
        .iter()
        .map(|(protocol, _)| *protocol)
        .filter(|protocol| *protocol >= 735)
}

/// Adjacent pairs (older, newer) of synchronising protocols.
fn pairs() -> Vec<(i32, i32)> {
    let protocols: Vec<i32> = synced_protocols().collect();
    protocols
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .collect()
}

/// The registry packets of a recorded session: (name, body).
fn registry_packets(protocol: i32) -> Option<Vec<(&'static str, Vec<u8>)>> {
    let packets = testing::corpus(protocol)?;
    Some(
        packets
            .into_iter()
            .filter(|packet| packet.direction == Direction::Clientbound)
            .filter_map(|packet| {
                let name = packet.name(protocol);
                matches!(name, "registry_data" | "login" | "respawn").then_some((name, packet.data))
            })
            .collect(),
    )
}

/// The registries a recorded session's server synchronised.
fn recorded(protocol: i32) -> Option<Vec<Registry>> {
    let packets = registry_packets(protocol)?;
    let mut registries = Vec::new();
    for (name, body) in packets {
        match (form::sync(protocol), name) {
            (Sync::DimensionList | Sync::LoginCodec, "login") => {
                return login::read_login(protocol, &mut Reader::new(&body))
                    .unwrap()
                    .codec;
            }
            (Sync::ConfigurationCodec, "registry_data") => {
                return Some(form::read_codec_packet(protocol, &body).unwrap());
            }
            (Sync::Packets, "registry_data") => registries.push(form::read_packet(&body).unwrap()),
            _ => {}
        }
    }
    Some(registries)
}

fn skip(protocol: i32) {
    eprintln!("no corpus for {protocol}: skipped");
}

#[test]
fn generated_defaults_equal_what_each_vanilla_server_sends() {
    for protocol in synced_protocols() {
        let Some(recorded) = recorded(protocol) else {
            skip(protocol);
            continue;
        };
        let expected = defaults::registries(protocol);
        assert_eq!(
            recorded.iter().map(|r| &r.name).collect::<Vec<_>>(),
            expected.iter().map(|r| &r.name).collect::<Vec<_>>(),
            "{protocol} registries"
        );
        for (recorded, expected) in recorded.iter().zip(&expected) {
            assert_eq!(recorded, expected, "{protocol} {}", recorded.name);
        }
    }
}

#[test]
fn registry_parts_of_every_recorded_packet_round_trip() {
    for protocol in synced_protocols() {
        let Some(packets) = registry_packets(protocol) else {
            skip(protocol);
            continue;
        };
        let mut seen = 0;
        for (name, body) in packets {
            // A step between a protocol and itself converts nothing, so the
            // readers and writers must reproduce every byte.
            let mut shared = Shared::new(protocol, protocol);
            let rewritten = match name {
                "login" => {
                    let mut rw = Rewriter::new(&body);
                    login::transcode_login(&mut shared, Direction::Clientbound, &mut rw).unwrap();
                    rw.copy_rest();
                    rw.finish().unwrap()
                }
                "respawn" => {
                    let mut rw = Rewriter::new(&body);
                    login::transcode_respawn(&mut shared, Direction::Clientbound, &mut rw).unwrap();
                    rw.copy_rest();
                    rw.finish().unwrap()
                }
                _ if form::sync(protocol) == Sync::ConfigurationCodec => {
                    let registries = form::read_codec_packet(protocol, &body).unwrap();
                    form::write_codec_packet(protocol, &registries).unwrap()
                }
                _ => {
                    let registry = form::read_packet(&body).unwrap();
                    let vanilla = defaults::get(protocol, &registry.name).unwrap();
                    assert_eq!(form::write_packet(&vanilla.to_registry()), body);
                    form::write_packet(&registry)
                }
            };
            assert_eq!(rewritten, body, "{protocol} {name}");
            seen += 1;
        }
        assert!(seen > 0, "{protocol} has registry packets");
    }
}

#[test]
fn logins_and_respawns_of_every_corpus_are_followed() {
    for protocol in PROTOCOLS.iter().map(|(protocol, _)| *protocol) {
        let Some(packets) = testing::corpus(protocol) else {
            skip(protocol);
            continue;
        };
        let mut step = Defaults::new(protocol, protocol);
        for packet in packets {
            let name = packet.name(protocol);
            if packet.direction == Direction::Clientbound
                && (name == "login" || name == "registry_data" || name == "respawn")
            {
                step.send_id(packet.direction, packet.state, packet.id, &packet.data)
                    .unwrap();
            }
        }
        let shared = step.shared();
        let dimension = dimension(shared, Side::Client).expect("login recorded a dimension");
        assert_eq!(
            dimension.legacy_id, 0,
            "{protocol}: the corpus plays in the overworld"
        );
        let (min_y, height) = if protocol >= 757 {
            (-64, 384)
        } else {
            (0, 256)
        };
        assert_eq!(
            (dimension.min_y, dimension.height),
            (min_y, height),
            "{protocol}"
        );
        if protocol >= 735 {
            assert_eq!(world(shared, Side::Backend), Some("minecraft:overworld"));
        }
    }
}

/// `element` converted without the vanilla shortcut, as a datapack's own
/// entry would be.
fn structurally(registry: &str, from: i32, to: i32, name: &str, element: &Nbt) -> Nbt {
    let mut element = element.clone();
    if let Nbt::Compound(compound) = &mut element {
        transform::apply(registry, from, to, name, compound).unwrap();
        convert::conform(registry, to, name, compound);
    }
    element
}

fn check_receiver(registry: &str, to: i32, name: &str, element: &Nbt, context: &str) {
    let Some(compound) = element.as_compound() else {
        return;
    };
    let problems = convert::problems(registry, to, compound);
    assert!(
        problems.is_empty(),
        "{context}: {registry} {name} for {to}: {problems:?}\n{}",
        snbt::write(element)
    );
}

#[test]
fn every_vanilla_registry_converts_to_each_neighbour_in_both_directions() {
    for (older, newer) in pairs() {
        for (from, to) in [(older, newer), (newer, older)] {
            let sent = defaults::registries(from);
            let converted = convert::registries(from, to, &sent).unwrap();
            let expected = defaults::all(to);
            assert_eq!(converted.len(), expected.len(), "{from}->{to}");
            for (registry, vanilla) in converted.iter().zip(expected) {
                assert_eq!(registry.name, vanilla.name, "{from}->{to}");
                for entry in &registry.entries {
                    let element = entry.element.as_ref().unwrap();
                    if let Some(expected) = vanilla.get(&entry.name) {
                        assert_eq!(element, &*expected.element, "{from}->{to} {}", entry.name);
                    }
                    check_receiver(&registry.name, to, &entry.name, element, "converted");
                }
                for entry in &vanilla.entries {
                    assert!(
                        registry.entry(&entry.name).is_some(),
                        "{to} needs {}",
                        entry.name
                    );
                }
                if !form::has_codec(to) {
                    for (index, entry) in registry.entries.iter().enumerate() {
                        assert_eq!(entry.id, index as i32);
                    }
                }
            }
        }
    }
}

/// Trim materials and patterns name items; one the receiver lacks makes a
/// real client reject the whole registry (the 1.21.4 resin trim on a
/// 1.21.3 client), so every converted entry names an item it has.
#[test]
fn converted_trims_name_items_the_receiver_has() {
    for (older, newer) in pairs() {
        for (from, to) in [(older, newer), (newer, older)] {
            let Some(items) = crate::data::version(to).registry("item") else {
                continue;
            };
            let converted = convert::registries(from, to, &defaults::registries(from)).unwrap();
            for registry in &converted {
                let key = match registry.name.as_str() {
                    "trim_material" => "ingredient",
                    "trim_pattern" => "template_item",
                    _ => continue,
                };
                for entry in &registry.entries {
                    let element = entry.element.as_ref().unwrap().as_compound().unwrap();
                    if let Some(item) = element.get(key).and_then(Nbt::as_str) {
                        assert!(
                            items.id(crate::data::strip(item)).is_some(),
                            "{from}->{to} {} {}: {item}",
                            registry.name,
                            entry.name
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn custom_entries_built_from_real_ones_satisfy_the_neighbours_schema() {
    for (older, newer) in pairs() {
        for (from, to) in [(older, newer), (newer, older)] {
            for registry in defaults::all(from) {
                if !defaults::syncs(to, registry.name) {
                    continue;
                }
                for entry in &registry.entries {
                    let converted =
                        structurally(registry.name, from, to, &entry.name, &entry.element);
                    check_receiver(
                        registry.name,
                        to,
                        &entry.name,
                        &converted,
                        &format!("{from}->{to}"),
                    );
                    let (Some(vanilla), Some(converted)) = (
                        defaults::element(to, registry.name, &entry.name),
                        converted.as_compound(),
                    ) else {
                        continue;
                    };
                    // What clients act on survives the field-by-field path.
                    match registry.name {
                        "dimension_type" => {
                            let ours = DimensionType::from_element(Some(&entry.name), converted);
                            let theirs = DimensionType::from_element(Some(&entry.name), vanilla);
                            let changed = |a: &DimensionType, b: &DimensionType| {
                                (a.min_y, a.height, a.has_ceiling, a.legacy_id)
                                    != (b.min_y, b.height, b.has_ceiling, b.legacy_id)
                            };
                            // 1.18 raised the overworld; 1.21.9 lit the end.
                            let moved = (from.min(to) == 756 && to.max(from) == 757)
                                || (from.min(to) == 772 && to.max(from) == 773);
                            assert!(
                                moved || !changed(&ours, &theirs),
                                "{from}->{to} {}: {ours:?} {theirs:?}",
                                entry.name
                            );
                        }
                        "worldgen/biome" => {
                            for key in [
                                "temperature",
                                "downfall",
                                "has_precipitation",
                                "precipitation",
                            ] {
                                if let (Some(ours), Some(theirs)) =
                                    (converted.get(key), vanilla.get(key))
                                    && from.min(to) != 756
                                {
                                    assert_eq!(ours, theirs, "{from}->{to} {} {key}", entry.name);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// A datapack dimension type and biome of `protocol`'s format, built from
/// the vanilla overworld and plains with the names changed.
fn custom(protocol: i32) -> Vec<Registry> {
    let mut registries = defaults::registries(protocol);
    for registry in &mut registries {
        let template = match registry.name.as_str() {
            "dimension_type" => "minecraft:overworld",
            "worldgen/biome" => "minecraft:plains",
            _ => continue,
        };
        let mut entry = registry.entry(template).unwrap().clone();
        entry.name = "mypack:void".to_owned();
        entry.id = registry.entries.iter().map(|entry| entry.id).max().unwrap() + 1;
        let element = entry.element.as_mut().unwrap().as_compound_mut().unwrap();
        if element.contains("name") {
            element.insert("name", "mypack:void");
        }
        if element.contains("temperature") {
            element.insert("temperature", 0.25f32);
        }
        if element.contains("height") {
            element.insert("min_y", -32i32);
            element.insert("height", 320i32);
            element.insert("logical_height", 320i32);
        }
        registry.entries.push(entry);
    }
    registries
}

#[test]
fn custom_dimension_and_biome_survive_every_conversion() {
    let protocols: Vec<i32> = synced_protocols().collect();
    let upwards = |first: i32| -> Vec<i32> {
        protocols
            .iter()
            .copied()
            .filter(|protocol| *protocol >= first)
            .collect()
    };
    let downwards: Vec<i32> = protocols.iter().rev().copied().collect();
    for chain in [upwards(735), upwards(751), upwards(755), downwards] {
        let mut registries = custom(chain[0]);
        let had_biome = chain[0] >= 751;
        let had_heights = chain[0] >= 755;
        for window in chain.windows(2) {
            let (from, to) = (window[0], window[1]);
            registries = convert::registries(from, to, &registries).unwrap();
            for registry in &registries {
                for entry in &registry.entries {
                    check_receiver(
                        &registry.name,
                        to,
                        &entry.name,
                        entry.element.as_ref().unwrap(),
                        &format!("chain {from}->{to}"),
                    );
                }
            }
            let dimensions = registries
                .iter()
                .find(|r| r.name == "dimension_type")
                .unwrap();
            let void = dimensions
                .entry("mypack:void")
                .expect("custom dimension kept");
            let void = void.element.as_ref().unwrap().as_compound().unwrap();
            let properties = DimensionType::from_element(Some("mypack:void"), void);
            if to >= 755 && had_heights {
                assert_eq!(
                    (properties.min_y, properties.height),
                    (-32, 320),
                    "{from}->{to}"
                );
            }
            assert_eq!(properties.legacy_id, 0);
            if to >= 751 {
                let biomes = registries
                    .iter()
                    .find(|r| r.name == "worldgen/biome")
                    .unwrap();
                match biomes.entry("mypack:void") {
                    Some(void) => {
                        let void = void.element.as_ref().unwrap().as_compound().unwrap();
                        assert_eq!(void.get("temperature"), Some(&Nbt::Float(0.25)));
                    }
                    None => assert!(!had_biome, "{from}->{to} lost the custom biome"),
                }
            }
        }
    }
}

fn registry_data(registry: &Registry) -> Vec<u8> {
    form::write_packet(registry)
}

#[test]
fn ids_follow_synthesis_and_dropping_between_1_21_4_and_1_21_5() {
    // A 1.21.5 backend syncs cat variants; a 1.21.4 client has them built in.
    let mut down = Defaults::new(769, 770);
    let mut outputs = Vec::new();
    for vanilla in defaults::all(770) {
        let body = registry_data(&vanilla.to_registry());
        outputs.extend(
            down.clientbound(Phase::Configuration, "registry_data", &body)
                .unwrap(),
        );
    }
    let names: Vec<String> = outputs
        .iter()
        .map(|out| form::read_packet(&out.data).unwrap().name)
        .collect();
    assert!(!names.contains(&"cat_variant".to_owned()));
    assert_eq!(names.len(), defaults::all(769).len());
    let shared = down.shared();
    let builtin = data::version(769).expect_registry("cat_variant");
    let red = defaults::get(770, "cat_variant")
        .unwrap()
        .get("red")
        .unwrap()
        .id;
    assert_eq!(
        map(shared, Direction::Clientbound, "cat_variant", red),
        builtin.id("red")
    );
    // ...and going up the client gets the vanilla 1.21.5 registries first.
    let mut up = Defaults::new(770, 769);
    let first = registry_data(&defaults::get(769, "worldgen/biome").unwrap().to_registry());
    let out = up
        .clientbound(Phase::Configuration, "registry_data", &first)
        .unwrap();
    let sent: Vec<String> = out
        .iter()
        .map(|out| form::read_packet(&out.data).unwrap().name)
        .collect();
    for name in [
        "wolf_sound_variant",
        "cat_variant",
        "test_instance",
        "worldgen/biome",
    ] {
        assert!(sent.contains(&name.to_owned()), "{name} in {sent:?}");
    }
    let shared = up.shared();
    let ocelot_like = builtin.id("all_black").unwrap();
    assert_eq!(
        map(
            shared,
            Direction::Serverbound,
            "cat_variant",
            id_of(shared, Side::Client, "cat_variant", "all_black").unwrap()
        ),
        Some(ocelot_like)
    );
}

#[test]
fn biome_ids_survive_custom_entries_and_appended_vanilla() {
    // A 1.21.5 backend with a custom biome first, talking to a 1.21.4 client.
    let mut biomes = defaults::get(770, "worldgen/biome").unwrap().to_registry();
    let mut custom = biomes.entries[0].clone();
    custom.name = "mypack:void".into();
    biomes.entries.insert(0, custom);
    biomes
        .entries
        .retain(|entry| entry.name != "minecraft:plains");
    biomes.renumber();
    let mut step = Defaults::new(769, 770);
    let out = step
        .clientbound(
            Phase::Configuration,
            "registry_data",
            &form::write_packet(&biomes),
        )
        .unwrap();
    let sent = out
        .iter()
        .map(|out| form::read_packet(&out.data).unwrap())
        .find(|registry| registry.name == "worldgen/biome")
        .unwrap();
    assert_eq!(sent.entries[0].name, "mypack:void");
    assert_eq!(sent.entries.last().unwrap().name, "minecraft:plains");
    let shared = step.shared();
    for (id, entry) in biomes.entries.iter().enumerate() {
        assert_eq!(
            map(shared, Direction::Clientbound, "worldgen/biome", id as i32),
            Some(id as i32),
            "{}",
            entry.name
        );
    }
    assert_eq!(
        size(shared, Side::Client, "worldgen/biome"),
        biomes.entries.len() + 1
    );
    assert_eq!(
        id_of(shared, Side::Client, "worldgen/biome", "plains"),
        Some(biomes.entries.len() as i32)
    );
    assert_eq!(
        id_of(shared, Side::Backend, "worldgen/biome", "plains"),
        None
    );
}

#[test]
fn the_one_codec_splits_and_joins_across_1_20_5() {
    let mut up = Defaults::new(766, 765);
    let codec = form::write_codec_packet(765, &defaults::registries(765)).unwrap();
    let out = up
        .clientbound(Phase::Configuration, "registry_data", &codec)
        .unwrap();
    let sent: Vec<Registry> = out
        .iter()
        .map(|out| form::read_packet(&out.data).unwrap())
        .collect();
    same_vanilla_contents(&sent, 766);

    let mut down = Defaults::new(765, 766);
    let mut out = Vec::new();
    for registry in defaults::registries(766) {
        out.extend(
            down.clientbound(
                Phase::Configuration,
                "registry_data",
                &form::write_packet(&registry),
            )
            .unwrap(),
        );
    }
    assert_eq!(
        out.len(),
        1,
        "the codec goes out once, after the last registry"
    );
    let joined = form::read_codec_packet(765, &out[0].data).unwrap();
    same_vanilla_contents(&joined, 765);
}

/// Every vanilla registry and entry of `protocol` with its vanilla element;
/// entries the sender had beyond them may follow.
fn same_vanilla_contents(registries: &[Registry], protocol: i32) {
    let names: Vec<&str> = registries
        .iter()
        .map(|registry| registry.name.as_str())
        .collect();
    let expected: Vec<&str> = defaults::all(protocol)
        .iter()
        .map(|vanilla| vanilla.name)
        .collect();
    assert_eq!(names, expected);
    for (registry, vanilla) in registries.iter().zip(defaults::all(protocol)) {
        for entry in &vanilla.entries {
            let sent = registry.entry(&entry.name).expect("vanilla entry sent");
            assert_eq!(
                sent.element.as_ref(),
                Some(&*entry.element),
                "{}",
                entry.name
            );
        }
    }
}

#[test]
fn a_1_20_1_client_gets_the_configuration_codec_in_login() {
    let mut down = Defaults::new(763, 764);
    let codec = form::write_codec_packet(764, &defaults::registries(764)).unwrap();
    assert!(
        down.clientbound(Phase::Configuration, "registry_data", &codec)
            .unwrap()
            .is_empty()
    );
    let pending = take_pending(down.shared());
    assert_eq!(pending, defaults::registries(763));
}

#[test]
fn dimensions_convert_between_every_form() {
    // 1.20.4 names, 1.20.5 IDs.
    let mut step = Defaults::new(766, 765);
    let codec = form::write_codec_packet(765, &defaults::registries(765)).unwrap();
    step.clientbound(Phase::Configuration, "registry_data", &codec)
        .unwrap();
    let shared = step.shared();
    let converted = login::convert_dimension(
        shared,
        Direction::Clientbound,
        Dimension::Name("minecraft:the_nether".into()),
    )
    .unwrap();
    let nether = id_of(shared, Side::Client, "dimension_type", "the_nether").unwrap();
    assert_eq!(converted, Dimension::Id(nether));
    // 1.18.2 elements, 1.19 names.
    let shared = &mut Shared::new(759, 758);
    for registry in defaults::registries(758) {
        record(shared, Side::Backend, &registry);
    }
    let end = defaults::element(758, "dimension_type", "the_end")
        .unwrap()
        .clone();
    let converted =
        login::convert_dimension(shared, Direction::Clientbound, Dimension::Element(end)).unwrap();
    assert_eq!(converted, Dimension::Name("minecraft:the_end".into()));
    // Legacy numbers both ways across 1.16.
    let shared = &Shared::new(735, 578);
    let converted =
        login::convert_dimension(shared, Direction::Clientbound, Dimension::Legacy(-1)).unwrap();
    assert_eq!(converted, Dimension::Name("minecraft:the_nether".into()));
    let shared = &Shared::new(578, 735);
    let converted = login::convert_dimension(
        shared,
        Direction::Clientbound,
        Dimension::Name("minecraft:the_end".into()),
    )
    .unwrap();
    assert_eq!(converted, Dimension::Legacy(1));
    assert!(DimensionType::legacy(-1).has_ceiling);
    // A 1.16 client of a 1.15 backend needs a codec it was never sent.
    let codec = login::default_codec(735).unwrap();
    assert_eq!(
        form::read_codec(735, &codec).unwrap(),
        defaults::registries(735)
    );
}

#[test]
fn malformed_registry_data_fails_without_panicking() {
    let good = form::write_packet(&defaults::get(777, "dimension_type").unwrap().to_registry());
    for cut in 0..good.len() {
        assert!(form::read_packet(&good[..cut]).is_err(), "cut at {cut}");
    }
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(form::read_packet(&trailing).is_err());
    let mut step = Defaults::new(776, 777);
    for body in [
        &good[..good.len() / 2],
        &[0xff, 0xff, 0xff, 0xff, 0x0f][..],
        &[],
    ] {
        assert!(
            step.clientbound(Phase::Configuration, "registry_data", body)
                .is_err()
        );
    }
    // Codecs: wrong shapes are errors.
    let codec = form::write_codec(763, &defaults::registries(763)).unwrap();
    let Nbt::Compound(mut root) = codec else {
        panic!()
    };
    root.compound_mut("minecraft:chat_type")
        .unwrap()
        .insert("type", "minecraft:wrong");
    assert!(form::read_codec(763, &Nbt::Compound(root)).is_err());
    assert!(form::read_codec(763, &Nbt::Int(3)).is_err());
    assert!(form::read_codec_packet(765, &[10, 1, 0, 1]).is_err());
    let mut rw_input = vec![0u8; 4];
    rw_input.extend([9, 0, 0]);
    assert!(login::read_login(763, &mut Reader::new(&rw_input)).is_err());
}

#[test]
fn every_pair_installs_without_route_problems() {
    for pair in crate::steps::PAIRS {
        for (client, backend) in [(pair.newer, pair.older), (pair.older, pair.newer)] {
            let routes = RouteBuilder::<Defaults>::new(client, backend)
                .defaults()
                .build();
            let problems: Vec<&String> = routes
                .problems()
                .iter()
                .filter(|problem| problem.contains("registry_data"))
                .collect();
            assert!(problems.is_empty(), "{client}<-{backend}: {problems:?}");
        }
    }
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
        .collect()
}

/// A real 1.20.5 `login` (vanilla 1.20.6, overworld) and 1.15.2 `login`.
const LOGIN_766: &str = "000000010003136d696e6563726166743a6f766572776f726c64146d696e6563726166743a7468655f6e6574686572116d696e6563726166743a7468655f656e6414020200010000136d696e6563726166743a6f766572776f726c64d39468421171e08501ff0001000000";
const LOGIN_578: &str = "000000010100000000c7feab42c557da511404666c6174020001";

#[test]
fn real_logins_convert_their_registry_parts() {
    let body = hex(LOGIN_766);
    for client in [766, 767] {
        let mut shared = Shared::new(client, 766);
        let mut rw = Rewriter::new(&body);
        login::transcode_login(&mut shared, Direction::Clientbound, &mut rw).unwrap();
        rw.copy_rest();
        assert_eq!(rw.finish().unwrap(), body, "{client}");
        let overworld = dimension(&shared, Side::Client).unwrap();
        assert_eq!(
            (overworld.min_y, overworld.height, overworld.has_skylight),
            (-64, 384, true)
        );
        assert_eq!(world(&shared, Side::Client), Some("minecraft:overworld"));
    }
    let parsed = login::read_login(578, &mut Reader::new(&hex(LOGIN_578))).unwrap();
    assert_eq!(parsed.dimension, Dimension::Legacy(0));
    assert_eq!(parsed.world, None);
    let shared = &Shared::new(735, 578);
    assert_eq!(
        login::convert_dimension(shared, Direction::Clientbound, parsed.dimension).unwrap(),
        Dimension::Name("minecraft:overworld".into())
    );
    assert!(!login::transcodes_login(shared));
}

#[test]
fn entries_without_data_take_the_senders_vanilla_element() {
    let mut chat = defaults::get(773, "chat_type").unwrap().to_registry();
    for entry in &mut chat.entries {
        entry.element = None;
    }
    let mut step = Defaults::new(772, 773);
    let out = step
        .clientbound(
            Phase::Configuration,
            "registry_data",
            &form::write_packet(&chat),
        )
        .unwrap();
    let sent = out
        .iter()
        .map(|out| form::read_packet(&out.data).unwrap())
        .find(|registry| registry.name == "chat_type")
        .unwrap();
    assert_eq!(sent, defaults::get(772, "chat_type").unwrap().to_registry());
    chat.entries[0].name = "mypack:shout".into();
    let mut step = Defaults::new(772, 773);
    let body = form::write_packet(&chat);
    assert!(
        step.clientbound(Phase::Configuration, "registry_data", &body)
            .is_err()
    );
}

#[test]
fn environment_attributes_the_receiver_lacks_are_dropped() {
    // The official 26.3 server's nether attributes, two of them new in 26.1
    // and one in 26.3: a 1.21.11 client fails to load the whole registry
    // over an attribute key it does not know.
    let attributes = || {
        let mut attributes = Compound::new();
        attributes.insert("minecraft:gameplay/fast_lava", 1i8);
        attributes.insert("minecraft:visual/ambient_light_color", "#302821");
        attributes.insert("minecraft:visual/block_light_tint", "#ffffff");
        attributes.insert("minecraft:gameplay/natural_mob_spawns", 1i8);
        attributes
    };
    let keys = |element: &Compound, field: &str| -> Vec<String> {
        element
            .compound(field)
            .unwrap()
            .0
            .iter()
            .map(|(key, _)| key.clone())
            .collect()
    };
    let mut element = Compound::new();
    element.insert("attributes", attributes());
    transform::apply("dimension_type", 777, 776, "the_nether", &mut element).unwrap();
    assert_eq!(
        keys(&element, "attributes"),
        [
            "minecraft:gameplay/fast_lava",
            "minecraft:visual/ambient_light_color",
            "minecraft:visual/block_light_tint"
        ]
    );
    let mut element = Compound::new();
    element.insert("attributes", attributes());
    transform::apply("worldgen/biome", 775, 774, "plains", &mut element).unwrap();
    assert_eq!(
        keys(&element, "attributes"),
        ["minecraft:gameplay/fast_lava"]
    );
    let mut element = Compound::new();
    element.insert("tracks", attributes());
    transform::apply("timeline", 775, 774, "day", &mut element).unwrap();
    assert_eq!(keys(&element, "tracks"), ["minecraft:gameplay/fast_lava"]);
}

/// The parameters of every chat decoration of a converted registry.
fn decoration_parameters(registry: &Registry) -> Vec<String> {
    let mut found = Vec::new();
    for entry in &registry.entries {
        let element = entry.element.as_ref().and_then(Nbt::as_compound).unwrap();
        for part in ["chat", "narration"] {
            let Some(part) = element.compound(part) else {
                continue;
            };
            let decoration = part.compound("decoration").unwrap_or(part);
            for parameter in decoration.list("parameters").unwrap_or_default() {
                found.push(parameter.as_str().unwrap().to_owned());
            }
        }
    }
    found
}

#[test]
fn chat_parameters_take_each_versions_names() {
    // 1.19's team and private message types name their target `team_name`;
    // a 1.19.1 client rejects the registry with that name, and back.
    let sent = defaults::get(759, "chat_type").unwrap().to_registry();
    assert!(decoration_parameters(&sent).contains(&"team_name".to_owned()));
    let up = convert::registry(759, 760, &sent).unwrap().unwrap();
    for parameter in decoration_parameters(&up) {
        assert!(
            ["sender", "target", "content"].contains(&parameter.as_str()),
            "{parameter}"
        );
    }
    let sent = defaults::get(760, "chat_type").unwrap().to_registry();
    let down = convert::registry(760, 759, &sent).unwrap().unwrap();
    for parameter in decoration_parameters(&down) {
        assert!(
            ["sender", "team_name", "content"].contains(&parameter.as_str()),
            "{parameter}"
        );
    }
}
