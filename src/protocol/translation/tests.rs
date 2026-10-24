use super::data::RegistryDiff;
use super::*;
use crate::protocol::{write_string, write_varint};

const OLD: i32 = 771;
const NEW: i32 = 772;
// Built-in IDs from the generated 1.21.6 -> 1.21.8 diff.
const LAVA_CHICKEN: i32 = 1256; // 1.21.8 item; absent from 1.21.6.
const CREATOR_DISC: i32 = 1253; // Its stand-in, unchanged in both.
const LAVA_CHICKEN_SOUND: i32 = 939;
const CREATOR_SOUND: i32 = 948; // Before insertion; 949 in 1.21.8.

fn version(number: i32) -> ProtocolVersion {
    ProtocolVersion::new(number).unwrap()
}

fn translator(client: i32, backend: i32) -> Translator {
    Translator::new(version(client), version(backend))
        .unwrap()
        .unwrap()
}

/// An upgrading translator: a 1.21.8 client on a 1.21.6 backend.
fn upgrading() -> Translator {
    translator(NEW, OLD)
}

fn varint(value: i32) -> Vec<u8> {
    let mut data = Vec::new();
    write_varint(value, &mut data);
    data
}

fn component(name: &str) -> i32 {
    data::version(V1_21_6)
        .registry("minecraft:data_component_type")
        .iter()
        .position(|entry| entry == &format!("minecraft:{name}"))
        .unwrap() as i32
}

fn packet_id(state: &str, name: &str) -> i32 {
    data::version(V1_21_6)
        .packets(state)
        .iter()
        .position(|entry| entry == &format!("minecraft:{name}"))
        .unwrap() as i32
}

/// An item stack with already encoded added components.
fn stack(item: i32, components: &[(i32, Vec<u8>)]) -> Vec<u8> {
    let mut data = varint(1);
    data.extend(varint(item));
    data.extend(varint(components.len() as i32));
    data.extend(varint(0));
    for (kind, value) in components {
        data.extend(varint(*kind));
        data.extend(value);
    }
    data
}

/// The same stack with each component length-prefixed, as creative clients send.
fn untrusted_stack(item: i32, components: &[(i32, Vec<u8>)]) -> Vec<u8> {
    let mut data = varint(1);
    data.extend(varint(item));
    data.extend(varint(components.len() as i32));
    data.extend(varint(0));
    for (kind, value) in components {
        data.extend(varint(*kind));
        data.extend(varint(value.len() as i32));
        data.extend(value);
    }
    data
}

fn container(stacks: &[Vec<u8>]) -> (i32, Vec<u8>) {
    let mut value = varint(stacks.len() as i32);
    for stack in stacks {
        value.extend(stack);
    }
    (component("container"), value)
}

fn set_slot(item: Vec<u8>) -> Packet {
    let mut data = varint(0); // Container.
    data.extend(varint(7)); // State ID.
    data.extend(36i16.to_be_bytes());
    data.extend(item);
    Packet::new(packet_id("play/clientbound", "container_set_slot"), data)
}

fn play(translator: &mut Translator, packet: Packet) -> io::Result<Packet> {
    translator.clientbound(State::Play, packet)
}

#[test]
fn generated_data_builds_one_step_with_identical_layouts() {
    assert_eq!(steps().len(), 1);
    assert!(supported(version(OLD), version(NEW)));
    assert!(supported(version(NEW), version(OLD)));
    assert!(supported(version(774), version(774)));
    assert!(!supported(version(NEW), version(774)));
    assert!(!supported(version(770), version(OLD)));
    assert!(
        Translator::new(version(OLD), version(OLD))
            .unwrap()
            .is_none()
    );
    let error = Translator::new(version(774), version(OLD)).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
}

#[test]
fn registry_diffs_shift_ids_and_map_missing_entries_to_stand_ins() {
    // Old [a, b, c, d]; new [a, X, b, d, Y]: c removed, X and Y inserted.
    let diff = RegistryDiff {
        old_size: 4,
        new_size: 5,
        inserted: vec![
            (1, "x".into(), 3, "d".into()),
            (4, "y".into(), 0, "a".into()),
        ],
        removed: vec![(2, "c".into(), 3, "d".into())],
    };
    assert_eq!(diff.upgrade(), [0, 2, 3, 3]);
    assert_eq!(diff.downgrade(), [0, 3, 1, 3, 0]);

    let step = &steps()[0];
    let (up, down) = (
        step.up.item.as_ref().unwrap(),
        step.down.item.as_ref().unwrap(),
    );
    assert_eq!((up.len(), down.len()), (1415, 1416));
    assert_eq!(up[1255], 1255);
    assert_eq!(up[1256], 1257);
    assert_eq!(up[1414], 1415);
    assert_eq!(down[LAVA_CHICKEN as usize], CREATOR_DISC);
    assert_eq!(down[1257], 1256);
    let (up, down) = (
        step.up.sound.as_ref().unwrap(),
        step.down.sound.as_ref().unwrap(),
    );
    assert_eq!(up[938], 938);
    assert_eq!(up[939], 940);
    assert_eq!(down[LAVA_CHICKEN_SOUND as usize], CREATOR_SOUND);
    assert_eq!(
        step.down.missing_items,
        [(
            "minecraft:music_disc_lava_chicken".to_owned(),
            "minecraft:music_disc_creator".to_owned()
        )]
    );
    assert!(step.up.missing_items.is_empty() && step.up.missing_sounds.is_empty());
}

#[test]
fn nested_items_in_data_components_are_renumbered() {
    let mut pot = varint(4);
    for item in [1300, 1, 1300, 1] {
        pot.extend(varint(item));
    }
    let mut repairable = varint(3); // A holder set of two items.
    repairable.extend(varint(1256));
    repairable.extend(varint(5));
    let shulker = stack(
        700,
        &[
            container(&[stack(1256, &[]), varint(0), stack(1255, &[])]),
            (component("pot_decorations"), pot),
            (component("repairable"), repairable),
            (component("damage"), varint(1256)), // Not an item: unchanged.
        ],
    );
    let translated = play(&mut upgrading(), set_slot(shulker)).unwrap();
    let mut pot = varint(4);
    for item in [1301, 1, 1301, 1] {
        pot.extend(varint(item));
    }
    let mut repairable = varint(3);
    repairable.extend(varint(1257));
    repairable.extend(varint(5));
    let expected = stack(
        700,
        &[
            container(&[stack(1257, &[]), varint(0), stack(1255, &[])]),
            (component("pot_decorations"), pot),
            (component("repairable"), repairable),
            (component("damage"), varint(1256)),
        ],
    );
    assert_eq!(translated, set_slot(expected));
}

#[test]
fn sounds_are_renumbered_and_inline_sounds_are_kept() {
    let sound = |holder: Vec<u8>| {
        let mut data = holder;
        data.extend(varint(0)); // Source.
        data.extend([0; 12 + 4 + 4 + 8]);
        Packet::new(packet_id("play/clientbound", "sound"), data)
    };
    let mut translator = upgrading();
    assert_eq!(
        play(&mut translator, sound(varint(LAVA_CHICKEN_SOUND + 1))).unwrap(),
        sound(varint(LAVA_CHICKEN_SOUND + 2))
    );
    let mut inline = varint(0);
    write_string("minecraft:custom.sound", &mut inline);
    inline.extend([1, 0x41, 0, 0, 0]); // Fixed range present.
    assert_eq!(
        play(&mut translator, sound(inline.clone())).unwrap(),
        sound(inline)
    );
}

#[test]
fn only_item_and_sound_tags_are_renumbered() {
    let tags = |blocks: [i32; 2], items: [i32; 2]| {
        let mut data = varint(2);
        for (registry, ids) in [("minecraft:block", blocks), ("minecraft:item", items)] {
            write_string(registry, &mut data);
            data.extend(varint(1));
            write_string("minecraft:test", &mut data);
            data.extend(varint(2));
            for id in ids {
                data.extend(varint(id));
            }
        }
        Packet::new(packet_id("configuration/clientbound", "update_tags"), data)
    };
    let translated = upgrading()
        .clientbound(State::Configuration, tags([1256, 3], [1256, 3]))
        .unwrap();
    assert_eq!(translated, tags([1256, 3], [1257, 3]));
}

#[test]
fn creative_items_are_downgraded_with_recomputed_length_prefixes() {
    let id = packet_id("play/serverbound", "set_creative_mode_slot");
    let creative = |item: Vec<u8>| {
        let mut data = 44i16.to_be_bytes().to_vec();
        data.extend(item);
        Packet::new(id, data)
    };
    let sent = untrusted_stack(
        700,
        &[container(&[stack(LAVA_CHICKEN, &[]), stack(1257, &[])])],
    );
    let received = untrusted_stack(
        700,
        &[container(&[stack(CREATOR_DISC, &[]), stack(1256, &[])])],
    );
    assert_eq!(
        upgrading()
            .serverbound(State::Play, creative(sent))
            .unwrap(),
        creative(received)
    );
}

#[test]
fn length_prefixes_follow_varint_size_changes() {
    let layout = &steps()[0].layout;
    // Map item 1 to 300, which needs one more VarInt byte.
    let table: Vec<i32> = (0..400).map(|id| if id == 1 { 300 } else { id }).collect();
    let ids = v1_21_6::Ids {
        item: Some(&table),
        sound: None,
        missing_items: &[],
        missing_sounds: &[],
    };
    let id = packet_id("play/serverbound", "set_creative_mode_slot");
    let creative = |item: Vec<u8>| {
        let mut data = 0i16.to_be_bytes().to_vec();
        data.extend(item);
        Packet::new(id, data)
    };
    let packet = creative(untrusted_stack(2, &[container(&[stack(1, &[])])]));
    let translated = layout
        .rewrite(State::Play, Direction::Serverbound, packet, &ids)
        .unwrap();
    assert_eq!(
        translated,
        creative(untrusted_stack(2, &[container(&[stack(300, &[])])]))
    );
}

#[test]
fn hashed_container_clicks_carry_renumbered_items() {
    let id = packet_id("play/serverbound", "container_click");
    let click = |item: i32| {
        let mut data = varint(0);
        data.extend(varint(3)); // State ID.
        data.extend(36i16.to_be_bytes());
        data.extend([0]); // Button.
        data.extend(varint(0)); // Pickup.
        data.extend(varint(1)); // One changed slot, now empty.
        data.extend(36i16.to_be_bytes());
        data.push(0);
        data.push(1); // Carried stack present.
        data.extend(varint(item));
        data.extend(varint(1));
        data.extend(varint(1)); // One added component hash.
        data.extend(varint(component("damage")));
        data.extend(0x1234_5678i32.to_be_bytes());
        data.extend(varint(0));
        Packet::new(id, data)
    };
    assert_eq!(
        upgrading().serverbound(State::Play, click(1257)).unwrap(),
        click(1256)
    );
}

#[test]
fn entity_metadata_items_particles_and_painting_variants_are_parsed() {
    let particle_type = |name: &str| {
        data::version(V1_21_6)
            .registry("minecraft:particle_type")
            .iter()
            .position(|entry| entry == &format!("minecraft:{name}"))
            .unwrap() as i32
    };
    let metadata = |item: i32| {
        let mut data = varint(42); // Entity.
        data.extend([8]); // Index.
        data.extend(varint(7)); // Item stack.
        data.extend(stack(item, &[]));
        data.extend([9]);
        data.extend(varint(18)); // Particle list.
        data.extend(varint(2));
        data.extend(varint(particle_type("item")));
        data.extend(stack(item, &[]));
        data.extend(varint(particle_type("dust")));
        data.extend([0; 8]);
        data.extend([10]);
        data.extend(varint(30)); // Inline painting variant.
        data.extend(varint(0));
        data.extend(varint(2));
        data.extend(varint(1));
        write_string("minecraft:test", &mut data);
        data.extend([1, 8, 0, 1, b'x', 0]); // Title as a string tag; no author.
        data.extend([11]);
        data.extend(varint(2)); // VarLong.
        data.extend([0xff, 0xff, 0x01]);
        data.push(0xff);
        Packet::new(packet_id("play/clientbound", "set_entity_data"), data)
    };
    assert_eq!(
        play(&mut upgrading(), metadata(1300)).unwrap(),
        metadata(1301)
    );
}

#[test]
fn registry_data_names_missing_entries_only_for_older_receivers() {
    let id = packet_id("configuration/clientbound", "registry_data");
    let nbt_string = |value: &str, data: &mut Vec<u8>| {
        data.extend((value.len() as u16).to_be_bytes());
        data.extend(value.as_bytes());
    };
    let entry = |sound: &[u8], item: &str| {
        let mut data = Vec::new();
        write_string("minecraft:jukebox_song", &mut data);
        data.extend(varint(1));
        write_string("minecraft:lava_chicken", &mut data);
        data.push(1); // Data present.
        data.push(10); // Root compound.
        data.extend(sound);
        data.push(9); // A list of item names.
        nbt_string("items", &mut data);
        data.push(8);
        data.extend(2i32.to_be_bytes());
        nbt_string(item, &mut data);
        nbt_string("minecraft:stick", &mut data);
        data.push(0);
        Packet::new(id, data)
    };
    let mut reference = vec![8];
    nbt_string("sound_event", &mut reference);
    nbt_string("minecraft:music_disc.lava_chicken", &mut reference);
    let original = entry(&reference, "minecraft:music_disc_lava_chicken");

    // A 1.21.8 backend and a 1.21.6 client: inline the sound, stand in the item.
    let mut inline = vec![10];
    nbt_string("sound_event", &mut inline);
    inline.push(8);
    nbt_string("sound_id", &mut inline);
    nbt_string("minecraft:music_disc.lava_chicken", &mut inline);
    inline.push(0);
    let downgraded = translator(OLD, NEW)
        .clientbound(State::Configuration, original.clone())
        .unwrap();
    assert_eq!(downgraded, entry(&inline, "minecraft:music_disc_creator"));
    // A newer receiver already has every entry: bytes are unchanged.
    assert_eq!(
        upgrading()
            .clientbound(State::Configuration, original.clone())
            .unwrap(),
        original
    );
}

#[test]
fn unrelated_packets_and_states_pass_through_unchanged() {
    let mut translator = upgrading();
    let chunk = Packet::new(
        packet_id("play/clientbound", "level_chunk_with_light"),
        vec![0xff; 9],
    );
    assert_eq!(play(&mut translator, chunk.clone()).unwrap(), chunk);
    let login = Packet::new(2, vec![1, 2, 3]);
    assert_eq!(
        translator.clientbound(State::Login, login.clone()).unwrap(),
        login
    );
    let chat = Packet::new(packet_id("play/serverbound", "chat"), vec![0xff; 4]);
    assert_eq!(
        translator.serverbound(State::Play, chat.clone()).unwrap(),
        chat
    );
}

#[test]
fn malformed_packets_fail_instead_of_panicking_or_passing() {
    let valid = set_slot(stack(
        700,
        &[container(&[stack(
            1256,
            &[(component("lore"), vec![1, 8, 0, 1, b'x'])],
        )])],
    ));
    let mut translator = upgrading();
    assert!(play(&mut translator, valid.clone()).is_ok());
    for length in 0..valid.data.len() {
        let truncated = Packet::new(valid.id, valid.data[..length].to_vec());
        assert!(play(&mut translator, truncated).is_err(), "prefix {length}");
    }
    let mut trailing = valid.clone();
    trailing.data.push(0);
    assert!(play(&mut translator, trailing).is_err());
    // Unknown component types and items outside the registry are rejected.
    assert!(play(&mut translator, set_slot(stack(700, &[(500, vec![])]))).is_err());
    assert!(play(&mut translator, set_slot(stack(1415, &[]))).is_err());
    // Deeply nested bundles exhaust the depth budget.
    let mut nested = stack(1, &[]);
    for _ in 0..40 {
        nested = stack(
            1,
            &[(component("bundle_contents"), [varint(1), nested].concat())],
        );
    }
    assert!(play(&mut translator, set_slot(nested)).is_err());
}
