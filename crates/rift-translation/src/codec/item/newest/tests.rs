//! Stacks recorded from official servers for the same `/give` in many
//! versions (see `tests/translation_corpus.py`): the newest server's stack,
//! converted down, must be what each older server sent.
use super::*;
use crate::{
    codec::item::{components::vectors::STACKS, read, window::SetSlot},
    wire::Reader,
};

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect()
}

fn decode(protocol: i32, bytes: &[u8]) -> Stack {
    let mut reader = Reader::new(bytes);
    let stack = read(protocol, &mut reader).unwrap();
    reader.finish().unwrap();
    stack
}

/// The stack of a recorded `container_set_slot`, and its bytes.
fn slot(protocol: i32, body: &str) -> (Stack, Vec<u8>) {
    let bytes = unhex(body);
    let mut reader = Reader::new(&bytes);
    let slot = SetSlot::read(protocol, &mut reader).unwrap();
    reader.finish().unwrap();
    let mut writer = Writer::new();
    write(protocol, slot.item.as_ref(), &mut writer).unwrap();
    (slot.item, writer.into_inner())
}

/// The recorded stacks of `protocol` for item `name` that set component
/// `kind`, as bytes.
fn recorded(protocol: i32, name: &str, kind: &str) -> Vec<Vec<u8>> {
    STACKS
        .iter()
        .filter(|(recorded, _)| *recorded == protocol)
        .map(|(_, hex)| unhex(hex))
        .filter(|bytes| {
            let stack = decode(protocol, bytes).unwrap();
            convert::item_name(protocol, stack.id) == Some(name)
                && stack.components.unwrap().get(kind).is_some()
        })
        .collect()
}

/// The one recorded 26.3 stack of `name` with component `kind`.
fn newest(name: &str, kind: &str) -> ItemStack {
    let mut found = recorded(NEWEST, name, kind);
    assert_eq!(found.len(), 1, "{name} {kind}");
    decode(NEWEST, &found.remove(0)).unwrap()
}

fn stack(name: &str, count: i32) -> ItemStack {
    ItemStack {
        id: item_id(name).unwrap(),
        count,
        damage: 0,
        tag: None,
        components: Some(Components::default()),
    }
}

/// Compounds compared regardless of entry order, and integers regardless of
/// width: the 1.12 `/give` typed `{id:16,lvl:5}` as ints where the game
/// writes shorts, and the client reads either.
fn sorted(value: &Nbt) -> Nbt {
    match value {
        Nbt::Compound(compound) => {
            let mut entries: Vec<(String, Nbt)> = compound
                .0
                .iter()
                .map(|(key, value)| (key.clone(), sorted(value)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Nbt::Compound(Compound(entries))
        }
        Nbt::List(values) => Nbt::List(values.iter().map(sorted).collect()),
        Nbt::Byte(value) => Nbt::Long(i64::from(*value)),
        Nbt::Short(value) => Nbt::Long(i64::from(*value)),
        Nbt::Int(value) => Nbt::Long(i64::from(*value)),
        other => other.clone(),
    }
}

fn component_protocols() -> impl Iterator<Item = i32> {
    PROTOCOLS
        .iter()
        .map(|(protocol, _)| *protocol)
        .filter(|protocol| version::item_components(*protocol))
}

#[test]
fn potions_and_heads_come_out_as_every_server_sent_them() {
    // 1.12.2 and 1.13 `container_set_slot`s of the same `/give`.
    const POTION_1_12: &str =
        "00002601750100000a0000080006506f74696f6e00136d696e6563726166743a73776966746e65737300";
    const HEAD_1_12: &str = "00002a018d0100030a000008000a536b756c6c4f776e657200054e6f74636800";
    const POTION_1_13: &str =
        "0000240270010a0000080006506f74696f6e00136d696e6563726166743a73776966746e65737300";
    const HEAD_1_13: &str = "00002702bc010a000008000a536b756c6c4f776e657200054e6f74636800";
    for (name, kind, legacy, flat) in [
        ("potion", "potion_contents", POTION_1_12, POTION_1_13),
        ("player_head", "profile", HEAD_1_12, HEAD_1_13),
    ] {
        let source = newest(name, kind);
        for protocol in component_protocols() {
            let ours = encode_from_newest(protocol, Some(source.clone())).unwrap();
            assert_eq!(
                recorded(protocol, name, kind),
                vec![ours],
                "{name} for {protocol}"
            );
        }
        for (protocol, body) in [(340, legacy), (393, flat)] {
            let ours = encode_from_newest(protocol, Some(source.clone())).unwrap();
            assert_eq!(ours, slot(protocol, body).1, "{name} for {protocol}");
        }
    }
}

#[test]
fn names_lore_and_enchantments_come_out_as_every_server_sent_them() {
    let source = newest("diamond_sword", "custom_name");
    // 1.21.5 and newer were given a red name, older servers a plain one.
    let mut plain = source.clone();
    let components = plain.components.as_mut().unwrap();
    let name = crate::codec::text::to_nbt(NEWEST, &crate::codec::text::Component::text("Blade"));
    components.insert(
        "custom_name",
        super::super::components::Value::Nbt(name.to_bytes(false)),
    );
    for protocol in component_protocols() {
        let given = if protocol >= 770 { &source } else { &plain };
        let ours = encode_from_newest(protocol, Some(given.clone())).unwrap();
        assert_eq!(
            recorded(protocol, "diamond_sword", "custom_name"),
            vec![ours],
            "{protocol}"
        );
    }
    // 1.12.2 writes the same tag with its entries in another order.
    const SWORD_1_12: &str = "00002401140100000a0000090004656e63680a000000010300036c766c00000005030002696400000010000a0007646973706c61790900044c6f726508000000020005666972737400067365636f6e640800044e616d650005426c6164650000";
    let (theirs, _) = slot(340, SWORD_1_12);
    let theirs = theirs.unwrap();
    let ours = from_newest(340, Some(plain)).unwrap().unwrap();
    assert_eq!(
        (ours.id, ours.count, ours.damage),
        (theirs.id, theirs.count, theirs.damage)
    );
    assert_eq!(
        ours.tag.as_ref().map(sorted),
        theirs.tag.as_ref().map(sorted)
    );
}

#[test]
fn old_clients_get_numeric_ids_with_data_values() {
    // `give @p red_wool 3` on 1.12.2 and 1.13.
    let (_, wool_1_12) = slot(340, "000025002303000e00");
    let (_, wool_1_13) = slot(393, "00002900600300");
    let wool = stack("red_wool", 3);
    assert_eq!(
        encode_from_newest(340, Some(wool.clone())).unwrap(),
        wool_1_12
    );
    assert_eq!(encode_from_newest(393, Some(wool)).unwrap(), wool_1_13);
    let converted = from_newest(335, Some(stack("minecraft:red_wool", 1)))
        .unwrap()
        .unwrap();
    assert_eq!((converted.id, converted.damage), (35, 14));
}

#[test]
fn items_a_client_lacks_become_named_stand_ins_without_stashes() {
    // 1.12 has no trident: stone named after it.
    let trident = from_newest(340, Some(stack("trident", 1)))
        .unwrap()
        .unwrap();
    assert_eq!((trident.id, trident.damage), (1, 0));
    let tag = trident.tag.unwrap();
    let tag = tag.as_compound().unwrap();
    assert_eq!(
        tag.compound("display").unwrap().str("Name"),
        Some("Trident")
    );
    assert!(tag.iter().all(|(key, _)| !key.starts_with(STASH_PREFIX)));

    // 26.2 has no straw bed: a yellow bed with the bed's name.
    let bed = from_newest(776, Some(stack("straw_bed", 1)))
        .unwrap()
        .unwrap();
    assert_eq!(convert::item_name(776, bed.id), Some("yellow_bed"));
    let patch = bed.components.unwrap();
    assert!(patch.get("item_name").is_some());
    assert!(patch.get("custom_data").is_none());

    // And all the way down, nothing the way back would need is left.
    for &(protocol, _) in PROTOCOLS {
        let bytes = encode_from_newest(protocol, Some(stack("straw_bed", 2))).unwrap();
        assert!(
            !bytes
                .windows(STASH_PREFIX.len())
                .any(|window| window == STASH_PREFIX.as_bytes()),
            "{protocol}"
        );
    }
}

#[test]
fn empty_stacks_and_unknown_protocols() {
    assert_eq!(encode_from_newest(340, None).unwrap(), [0xff, 0xff]);
    assert_eq!(encode_from_newest(404, None).unwrap(), [0]);
    assert_eq!(encode_from_newest(NEWEST, None).unwrap(), [0]);
    assert!(from_newest(47, Some(stack("stone", 1))).is_err());
    assert!(from_newest(778, None).is_err());
}

#[test]
fn items_are_found_by_name() {
    let stone = item_id("stone").unwrap();
    assert_eq!(item_id("minecraft:stone"), Some(stone));
    assert_eq!(item_name(stone), Some("stone"));
    assert_eq!(item_id("straw_bed"), item_id("minecraft:straw_bed"));
    for unknown in ["", "minecraft:", "no_such_item", "other:stone", "Stone"] {
        assert_eq!(item_id(unknown), None, "{unknown}");
    }
}
