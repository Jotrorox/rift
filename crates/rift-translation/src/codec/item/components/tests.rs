//! Tests of the component layouts and conversions: completeness of the
//! tables, a generated value of every layout through every pair, real
//! stacks from the recorded corpus (byte-exact round trips and conversions
//! both ways), and hand-built cases for what the rules and the stash do.
use super::{
    convert::tests::TEXT_PASSTHROUGH,
    convert::{self, convert_value},
    layouts,
    layouts::{FIRST, Layout},
    schema::{self, Node},
    *,
};
use crate::{
    Direction, State,
    codec::{item::ItemStack, testing},
    data,
    nbt::{Compound, Nbt},
    version::PROTOCOLS,
    wire::{Reader, Writer},
};
use std::collections::BTreeMap;

/// Protocols with data components, oldest first.
fn protocols() -> impl Iterator<Item = i32> {
    PROTOCOLS
        .iter()
        .map(|(protocol, _)| *protocol)
        .filter(|protocol| *protocol >= FIRST)
}

/// A step that carries packets from `from` to the adjacent `to`, clientbound.
fn step(from: i32, to: i32) -> Shared {
    Shared::new(to, from)
}

fn passthrough_text() {
    TEXT_PASSTHROUGH.with(|flag| flag.set(true));
}

#[test]
fn every_component_type_has_a_layout() {
    for protocol in protocols() {
        let layout = Layout::of(protocol).unwrap();
        assert_eq!(
            layout.missing(),
            Vec::<String>::new(),
            "protocol {protocol} has component types without a layout"
        );
    }
}

/// Every registry a layout names exists in some protocol's data or is one
/// of the synchronised registries.
#[test]
fn layouts_name_known_registries() {
    const SYNCED: &[&str] = &[
        "trim_material",
        "trim_pattern",
        "banner_pattern",
        "jukebox_song",
        "damage_type",
        "wolf_variant",
        "wolf_sound_variant",
        "pig_variant",
        "pig_sound_variant",
        "cow_variant",
        "cow_sound_variant",
        "chicken_variant",
        "chicken_sound_variant",
        "cat_sound_variant",
        "zombie_nautilus_variant",
        "block_transformer",
    ];
    for protocol in protocols() {
        let layout = Layout::of(protocol).unwrap();
        let registry = data::version(protocol).expect_registry("data_component_type");
        for id in 0..registry.len() as i32 {
            let (kind, node) = layout.get(id).unwrap();
            schema::registries(node, &mut |name| {
                let known = SYNCED.contains(&name)
                    || protocols().any(|p| data::version(p).registry(name).is_some());
                assert!(known, "{kind} names unknown registry {name}");
            });
        }
    }
}

/// Which shapes [`sample`] builds: holders inline or by reference, the
/// right or left side of eithers, which variant of dispatches.
#[derive(Clone, Copy)]
struct Flavour {
    inline: bool,
    pick: usize,
}

/// A value of `node` that exercises it: every optional present, one element
/// per list, the flavour's choices elsewhere.
fn sample(protocol: i32, node: &Node, depth: usize, flavour: Flavour) -> Value {
    use Node::*;
    let item = |protocol: i32| {
        let registry = data::version(protocol).expect_registry("item");
        registry.id("stone").unwrap()
    };
    let next = |node: &Node| sample(protocol, node, depth + 1, flavour);
    match node {
        Unit => Value::Unit,
        Bool => Value::Bool(false),
        Byte => Value::Byte(1),
        VarInt | Int => Value::Int(1),
        VarLong | Long | Position => Value::Long(1),
        Float => Value::Float(1.0),
        Double => Value::Double(1.0),
        Str(_) => Value::String("a".into()),
        Identifier => Value::String("minecraft:stone".into()),
        Uuid => Value::Uuid(1),
        Tag | Compound => Value::nbt(&Nbt::Compound(Default::default())),
        Text => Value::nbt(&Nbt::String("text".into())),
        Holder(_, inline) if flavour.inline && depth < 6 => Value::Direct(Box::new(next(inline))),
        Id(registry) | Holder(registry, _) => Value::Id(match *registry {
            "item" => item(protocol),
            _ => 0,
        }),
        HolderSet(_) if flavour.inline => Value::Ids(vec![0]),
        HolderSet(_) => Value::Tag("minecraft:planks".into()),
        Enum(names) => Value::Enum(names[flavour.pick % names.len()]),
        Optional(inner) if depth < 6 => Value::some(next(inner)),
        Optional(_) => Value::none(),
        List(element, _) if depth < 6 => Value::List(vec![next(element)]),
        List(_, _) => Value::List(Vec::new()),
        Array(element, count) => Value::List((0..*count).map(|_| next(element)).collect()),
        Struct(fields) => Value::Struct(fields.iter().map(|(_, field)| next(field)).collect()),
        Either(left, right) => {
            let right_side = flavour.inline && depth < 6;
            Value::Either(
                !right_side,
                Box::new(next(if right_side { right } else { left })),
            )
        }
        Dispatch(_, variants) | Union(variants) => {
            // The deepest variants nest displays; stop at a leaf one.
            let (name, variant) = if depth < 4 {
                &variants[flavour.pick % variants.len()]
            } else {
                &variants[0]
            };
            Value::Variant(name, Box::new(next(variant)))
        }
        Stack | Template => Value::Stack(Some(Box::new(ItemStack {
            id: item(protocol),
            count: 1,
            damage: 0,
            tag: None,
            components: Some(Components::default()),
        }))),
        Kind => Value::Kind("damage"),
        Typed => Value::Typed(Box::new(Component {
            kind: "damage",
            value: Value::Int(3),
        })),
        Ref(inner) => sample(protocol, inner, depth + 1, flavour),
    }
}

fn encode(protocol: i32, components: &Components) -> Vec<u8> {
    let mut writer = Writer::new();
    write(protocol, components, &mut writer).unwrap();
    writer.buf
}

fn decode(protocol: i32, bytes: &[u8]) -> Components {
    let mut reader = Reader::new(bytes);
    let components = read(protocol, &mut reader).unwrap();
    reader.finish().unwrap();
    components
}

/// Generated values of every component of every protocol, in several
/// flavours, survive their own writer and reader, convert to both
/// neighbours into something the neighbour's reader takes, and come back
/// as they were (what the neighbour cannot express rides in the stash).
#[test]
fn every_layout_converts_to_both_neighbours_and_back() {
    passthrough_text();
    let all: Vec<i32> = protocols().collect();
    for inline in [false, true] {
        for pick in 0..12 {
            let flavour = Flavour { inline, pick };
            for (index, &protocol) in all.iter().enumerate() {
                let layout = Layout::of(protocol).unwrap();
                let registry = data::version(protocol).expect_registry("data_component_type");
                for id in 0..registry.len() as i32 {
                    let (kind, node) = layout.get(id).unwrap();
                    let mut components = Components::default();
                    components.insert(kind, sample(protocol, node, 0, flavour));
                    let bytes = encode(protocol, &components);
                    assert_eq!(decode(protocol, &bytes), components, "{kind} in {protocol}");
                    let neighbours = [index.checked_sub(1), Some(index + 1)];
                    for to in neighbours.into_iter().flatten().filter_map(|i| all.get(i)) {
                        let what = format!("{kind} from {protocol} to {to} ({inline}, {pick})");
                        let mut converted = components.clone();
                        convert(
                            &mut step(protocol, *to),
                            Direction::Clientbound,
                            1,
                            &mut converted,
                        )
                        .unwrap_or_else(|error| panic!("{what}: {error}"));
                        let out = encode(*to, &converted);
                        assert_eq!(decode(*to, &out), converted, "{what}");
                        let mut back = converted.clone();
                        convert(
                            &mut step(*to, protocol),
                            Direction::Clientbound,
                            1,
                            &mut back,
                        )
                        .unwrap_or_else(|error| panic!("{what} and back: {error}"));
                        assert_eq!(
                            normalized_patch(back),
                            normalized_patch(components.clone()),
                            "{what} and back"
                        );
                    }
                }
            }
        }
    }
}

/// The stacks of one recorded packet, as byte ranges of its body, for the
/// packets whose layout around stacks is short: `None` for other packets.
fn stack_ranges(protocol: i32, name: &str, body: &[u8]) -> Option<Vec<(usize, usize)>> {
    let mut reader = Reader::new(body);
    let mut ranges = Vec::new();
    let offset = |reader: &Reader| body.len() - reader.remaining();
    let mut stack = |reader: &mut Reader| {
        let start = offset(reader);
        schema::read_stack(protocol, reader, 0)
            .unwrap_or_else(|error| panic!("{name} in {protocol}: {error}"));
        ranges.push((start, offset(reader)));
    };
    let container = |reader: &mut Reader, signed: bool| {
        if protocol >= 768 {
            reader.varint().map(drop)
        } else if signed {
            reader.i8().map(drop)
        } else {
            reader.u8().map(drop)
        }
    };
    match name {
        "container_set_content" => {
            container(&mut reader, false).ok()?;
            reader.varint().ok()?;
            for _ in 0..reader.count().ok()? {
                stack(&mut reader);
            }
            stack(&mut reader);
        }
        "container_set_slot" => {
            container(&mut reader, true).ok()?;
            reader.varint().ok()?;
            reader.i16().ok()?;
            stack(&mut reader);
        }
        "set_equipment" => {
            reader.varint().ok()?;
            loop {
                let slot = reader.u8().ok()?;
                stack(&mut reader);
                if slot & 0x80 == 0 {
                    break;
                }
            }
        }
        "set_cursor_item" => stack(&mut reader),
        "set_player_inventory" => {
            reader.varint().ok()?;
            stack(&mut reader);
        }
        _ => return None,
    }
    assert!(reader.is_empty(), "{name} in {protocol} has trailing data");
    Some(ranges)
}

/// Every non-empty stack the corpus of `protocol` holds, as recorded.
fn corpus_stacks(protocol: i32) -> Option<Vec<Vec<u8>>> {
    let packets = testing::corpus(protocol)?;
    let mut stacks = Vec::new();
    for packet in packets {
        if packet.direction != Direction::Clientbound || packet.state != State::Play {
            continue;
        }
        let name = packet.name(protocol);
        if let Some(ranges) = stack_ranges(protocol, name, &packet.data) {
            for (start, end) in ranges {
                let bytes = &packet.data[start..end];
                if bytes != [0] && !stacks.iter().any(|known: &Vec<u8>| known == bytes) {
                    stacks.push(bytes.to_vec());
                }
            }
        }
    }
    Some(stacks)
}

fn read_stack(protocol: i32, bytes: &[u8]) -> Option<ItemStack> {
    let mut reader = Reader::new(bytes);
    let stack = schema::read_stack(protocol, &mut reader, 0).unwrap();
    reader.finish().unwrap();
    stack
}

fn write_stack(protocol: i32, stack: Option<&ItemStack>) -> Vec<u8> {
    let mut writer = Writer::new();
    schema::write_stack(protocol, stack, &mut writer).unwrap();
    writer.buf
}

/// Convert a top-level stack as the item codec will: map the item, then
/// the patch.
/// A stack through the item codec, which also names and stashes stand-ins
/// for items the receiver lacks.
fn convert_stack(from: i32, to: i32, stack: &ItemStack) -> Option<ItemStack> {
    let mut shared = step(from, to);
    let mut converted = Some(stack.clone());
    crate::codec::item::convert(&mut shared, Direction::Clientbound, &mut converted)
        .unwrap_or_else(|error| panic!("stack from {from} to {to}: {error}"));
    converted
}

#[test]
fn corpus_stacks_round_trip_byte_for_byte() {
    let mut report = Vec::new();
    for protocol in protocols() {
        let Some(stacks) = corpus_stacks(protocol) else {
            report.push(format!("{protocol}: no corpus"));
            continue;
        };
        let mut kinds = BTreeMap::new();
        for bytes in &stacks {
            let stack = read_stack(protocol, bytes);
            assert_eq!(
                &write_stack(protocol, stack.as_ref()),
                bytes,
                "stack in {protocol}"
            );
            for component in &stack.unwrap().components.unwrap().added {
                *kinds.entry(component.kind).or_insert(0) += 1;
            }
        }
        let registry = data::version(protocol).expect_registry("data_component_type");
        let unseen: Vec<_> = registry
            .names()
            .map(|(_, name)| name)
            .filter(|name| !kinds.contains_key(name))
            .collect();
        report.push(format!(
            "{protocol}: {} stacks, {} component types; not in the corpus: {}",
            stacks.len(),
            kinds.len(),
            unseen.join(" ")
        ));
    }
    eprintln!("{}", report.join("\n"));
}

/// Real stacks go to each neighbour and back: the neighbour's reader takes
/// what comes out, and coming back gives the original (whatever the
/// neighbour cannot express travels in the stash).
#[test]
fn corpus_stacks_convert_to_both_neighbours_and_back() {
    passthrough_text();
    let all: Vec<i32> = protocols().collect();
    let mut report = Vec::new();
    for (index, &protocol) in all.iter().enumerate() {
        let Some(stacks) = corpus_stacks(protocol) else {
            continue;
        };
        let neighbours = [index.checked_sub(1), Some(index + 1)];
        for to in neighbours.into_iter().flatten().filter_map(|i| all.get(i)) {
            let (mut converted, mut exact) = (0, 0);
            for bytes in &stacks {
                let stack = read_stack(protocol, bytes).unwrap();
                let Some(there) = convert_stack(protocol, *to, &stack) else {
                    continue;
                };
                let out = write_stack(*to, Some(&there));
                assert_eq!(read_stack(*to, &out).as_ref(), Some(&there));
                converted += 1;
                let back = convert_stack(*to, protocol, &there).unwrap();
                assert_eq!(
                    normalized(&back),
                    normalized(&stack),
                    "stack from {protocol} to {to} and back"
                );
                if write_stack(protocol, Some(&back)) == *bytes {
                    exact += 1;
                }
            }
            report.push(format!(
                "{protocol} -> {to} -> {protocol}: {converted} stacks, {exact} byte-identical"
            ));
        }
    }
    eprintln!("{}", report.join("\n"));
}

/// A stack with its components in a fixed order, for comparisons where a
/// conversion may reorder them.
fn normalized(stack: &ItemStack) -> ItemStack {
    let mut stack = stack.clone();
    if let Some(components) = &mut stack.components {
        components.added.sort_by_key(|component| component.kind);
        components.removed.sort();
    }
    stack
}

fn patch(protocol: i32, components: &[(&'static str, Value)]) -> Components {
    let mut patch = Components::default();
    for (kind, value) in components {
        patch.insert(kind, value.clone());
    }
    let _ = protocol;
    patch
}

fn converted(from: i32, to: i32, components: &Components) -> Components {
    let mut shared = step(from, to);
    let mut components = components.clone();
    convert(&mut shared, Direction::Clientbound, 1, &mut components).unwrap();
    // What comes out is valid for the receiver.
    let bytes = encode(to, &components);
    assert_eq!(decode(to, &bytes), components);
    components
}

fn stash_entries(components: &Components) -> Vec<Nbt> {
    let Some(value) = components.get("custom_data") else {
        return Vec::new();
    };
    let Some(Nbt::Compound(data)) = value.as_nbt().unwrap() else {
        panic!("custom_data is not a compound");
    };
    data.list(stash::KEY)
        .map(<[Nbt]>::to_vec)
        .unwrap_or_default()
}

#[test]
fn components_the_receiver_lacks_are_stashed_and_come_back() {
    let weapon = Value::Struct(vec![Value::Int(2), Value::Float(5.0)]);
    let original = patch(
        770,
        &[("weapon", weapon.clone()), ("max_damage", Value::Int(9))],
    );
    let down = converted(770, 769, &original);
    assert!(down.get("weapon").is_none());
    assert_eq!(down.get("max_damage"), Some(&Value::Int(9)));
    let entries = stash_entries(&down);
    assert_eq!(entries.len(), 1);
    let entry = entries[0].as_compound().unwrap();
    assert_eq!(entry.int("protocol"), Some(770));
    assert_eq!(entry.str("type"), Some("weapon"));
    // Two steps further down, the entry for 770 rides along.
    let further = converted(768, 767, &converted(769, 768, &down));
    assert_eq!(stash_entries(&further).len(), 1);
    let back = converted(767, 768, &further);
    let back = converted(768, 769, &back);
    let back = converted(769, 770, &back);
    assert_eq!(back.get("weapon"), Some(&weapon));
    assert!(back.get("custom_data").is_none(), "the stash is emptied");
    assert_eq!(back.get("max_damage"), Some(&Value::Int(9)));
}

#[test]
fn stash_keeps_custom_data_of_its_own() {
    let mut data = Compound::new();
    data.insert("mine", Nbt::Int(7));
    let custom = Value::nbt(&Nbt::Compound(data));
    let original = patch(
        770,
        &[
            ("custom_data", custom.clone()),
            ("glider", Value::Unit),
            (
                "weapon",
                Value::Struct(vec![Value::Int(1), Value::Float(0.0)]),
            ),
        ],
    );
    let down = converted(770, 769, &original);
    assert_eq!(stash_entries(&down).len(), 1);
    let back = converted(769, 770, &down);
    assert_eq!(back.get("custom_data"), Some(&custom));
    assert_eq!(normalized_patch(back), normalized_patch(original));
}

fn normalized_patch(mut components: Components) -> Components {
    components.added.sort_by_key(|component| component.kind);
    components.removed.sort();
    components
}

#[test]
fn removed_types_the_receiver_lacks_come_back() {
    let mut original = Components::default();
    original.removed.push("weapon");
    let down = converted(770, 769, &original);
    assert!(down.removed.is_empty());
    let back = converted(769, 770, &down);
    assert_eq!(back, original);
}

#[test]
fn tooltip_flags_become_tooltip_display_and_back() {
    let levels = Value::List(Vec::new());
    let original = patch(
        769,
        &[
            (
                "enchantments",
                Value::Struct(vec![levels.clone(), Value::Bool(false)]),
            ),
            ("unbreakable", Value::Bool(true)),
            ("hide_tooltip", Value::Unit),
        ],
    );
    let up = converted(769, 770, &original);
    assert_eq!(up.get("enchantments"), Some(&levels));
    assert_eq!(up.get("unbreakable"), Some(&Value::Unit));
    assert_eq!(
        up.get("tooltip_display"),
        Some(&Value::Struct(vec![
            Value::Bool(true),
            Value::List(vec![Value::Kind("enchantments")]),
        ]))
    );
    assert!(stash_entries(&up).is_empty(), "nothing is lost going up");
    let back = converted(770, 769, &up);
    assert_eq!(normalized_patch(back), normalized_patch(original));
}

#[test]
fn food_splits_into_consumable_and_merges_back() {
    let effect = Value::Struct(vec![
        Value::Id(1),
        Value::Struct(vec![
            Value::Int(0),
            Value::Int(100),
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(true),
            Value::none(),
        ]),
    ]);
    let food = Value::Struct(vec![
        Value::Int(4),
        Value::Float(2.4),
        Value::Bool(false),
        Value::Float(1.6),
        Value::none(),
        Value::List(vec![Value::Struct(vec![effect, Value::Float(0.5)])]),
    ]);
    let original = patch(767, &[("food", food)]);
    let up = converted(767, 768, &original);
    assert!(up.get("consumable").is_some());
    assert_eq!(up.get("food").unwrap().fields().unwrap().len(), 3);
    assert!(stash_entries(&up).is_empty());
    let back = converted(768, 767, &up);
    assert_eq!(back, original);
}

#[test]
fn entity_data_moves_its_type_out_and_back() {
    let mut data = Compound::new();
    data.insert("id", Nbt::String("minecraft:pig".into()));
    data.insert("NoAI", Nbt::Byte(1));
    let original = patch(772, &[("entity_data", Value::nbt(&Nbt::Compound(data)))]);
    let up = converted(772, 773, &original);
    let fields = up.get("entity_data").unwrap().fields().unwrap();
    let pig = data::version(773)
        .expect_registry("entity_type")
        .id("pig")
        .unwrap();
    assert_eq!(fields[0], Value::Id(pig));
    let back = converted(773, 772, &up);
    assert_eq!(back, original);
}

#[test]
fn swing_animation_splits_and_merges() {
    let swing = Value::Struct(vec![Value::Int(1), Value::Int(6)]);
    let original = patch(776, &[("swing_animation", swing.clone())]);
    let up = converted(776, 777, &original);
    assert_eq!(up.get("attack_animation"), Some(&swing));
    assert_eq!(up.get("interact_animation"), Some(&swing));
    assert_eq!(converted(777, 776, &up), original);
}

#[test]
fn hashed_stacks_map_types_and_keep_hashes() {
    use super::hashed::{HashedStack, read as read_hashed, write as write_hashed};
    let stack = HashedStack {
        item: data::version(776)
            .expect_registry("item")
            .id("stone")
            .unwrap(),
        count: 3,
        added: vec![("swing_animation", 0x1234), ("damage", -5)],
        removed: vec!["map_color"],
    };
    let mut writer = Writer::new();
    write_hashed(776, Some(&stack), &mut writer).unwrap();
    let mut reader = Reader::new(&writer.buf);
    assert_eq!(
        read_hashed(776, &mut reader).unwrap().as_ref(),
        Some(&stack)
    );
    let mut converted = Some(stack.clone());
    let mut shared = step(776, 777);
    hashed::convert(&mut shared, Direction::Clientbound, &mut converted).unwrap();
    let converted = converted.unwrap();
    assert_eq!(
        converted.added,
        vec![
            ("attack_animation", 0x1234),
            ("interact_animation", 0x1234),
            ("damage", -5)
        ]
    );
    assert!(converted.removed.is_empty(), "26.3 has no map_color");
}

#[test]
fn delimited_patches_round_trip() {
    let original = patch(771, &[("damage", Value::Int(3)), ("glider", Value::Unit)]);
    let mut writer = Writer::new();
    write_delimited(771, &original, &mut writer).unwrap();
    let mut reader = Reader::new(&writer.buf);
    assert_eq!(read_delimited(771, &mut reader).unwrap(), original);
    reader.finish().unwrap();
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect()
}

/// Recorded stacks embedded in the source (see [`super::vectors`]), so CI
/// checks round trips and conversions of real data without the corpus.
#[test]
fn recorded_stacks_round_trip_and_convert() {
    passthrough_text();
    let all: Vec<i32> = protocols().collect();
    for (protocol, hex) in super::vectors::STACKS {
        let bytes = unhex(hex);
        let stack = read_stack(*protocol, &bytes).unwrap();
        assert_eq!(
            write_stack(*protocol, Some(&stack)),
            bytes,
            "vector in {protocol}"
        );
        let index = all.iter().position(|known| known == protocol).unwrap();
        let neighbours = [index.checked_sub(1), Some(index + 1)];
        for to in neighbours.into_iter().flatten().filter_map(|i| all.get(i)) {
            let Some(there) = convert_stack(*protocol, *to, &stack) else {
                continue;
            };
            let out = write_stack(*to, Some(&there));
            assert_eq!(read_stack(*to, &out).as_ref(), Some(&there));
            let back = convert_stack(*to, *protocol, &there).unwrap();
            assert_eq!(
                normalized(&back),
                normalized(&stack),
                "vector from {protocol} to {to} and back"
            );
        }
    }
}

/// Write `vectors.rs` from the corpus: per protocol, recorded stacks that
/// together hold every component type the corpus has (greedy cover).
/// `RIFT_COMPONENT_VECTORS=path cargo test -- --ignored write_vectors`.
#[test]
#[ignore]
fn write_vectors() {
    let path = std::env::var("RIFT_COMPONENT_VECTORS").expect("RIFT_COMPONENT_VECTORS");
    let mut out = String::from(
        "//! Recorded item stacks from official servers, one set per protocol that\n\
         //! together holds every component type the capture produced. Generated\n\
         //! by the `write_vectors` test from a corpus captured with extra\n\
         //! `/give` commands; do not edit.\n\n\
         /// (protocol, stack as hex) in `ItemStack.OPTIONAL_STREAM_CODEC`.\n\
         pub const STACKS: &[(i32, &str)] = &[\n",
    );
    for protocol in protocols() {
        let Some(stacks) = corpus_stacks(protocol) else {
            continue;
        };
        let kinds = |bytes: &Vec<u8>| -> Vec<&'static str> {
            let stack = read_stack(protocol, bytes).unwrap();
            stack
                .components
                .unwrap()
                .added
                .iter()
                .map(|c| c.kind)
                .collect()
        };
        let mut covered: Vec<&str> = Vec::new();
        let mut candidates: Vec<(Vec<u8>, Vec<&str>)> = stacks
            .iter()
            .map(|bytes| (bytes.clone(), kinds(bytes)))
            .collect();
        loop {
            let best = candidates
                .iter()
                .enumerate()
                .map(|(index, (bytes, kinds))| {
                    let new = kinds.iter().filter(|kind| !covered.contains(kind)).count();
                    (new, std::cmp::Reverse(bytes.len()), index)
                })
                .max();
            let Some((new, _, index)) = best.filter(|(new, _, _)| *new > 0) else {
                break;
            };
            let _ = new;
            let (bytes, kinds) = candidates.remove(index);
            covered.extend(kinds);
            let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            out.push_str(&format!("    ({protocol}, \"{hex}\"),\n"));
        }
    }
    out.push_str("];\n");
    std::fs::write(path, out).unwrap();
}

/// `recipe_book_add` and `update_recipes` (1.21.2+) around one protocol's
/// displays: the packets are the same in every protocol that has them.
macro_rules! recipe_packets {
    ($add:ident, $update:ident, $recipe:path, $slot:path) => {
        static $add: Node = Node::Struct(&[
            (
                "entries",
                Node::List(
                    &Node::Struct(&[
                        ("id", Node::VarInt),
                        ("display", Node::Ref(&$recipe)),
                        ("group", Node::VarInt),
                        ("category", Node::Id("recipe_book_category")),
                        (
                            "requirements",
                            Node::Optional(&Node::List(&Node::HolderSet("item"), usize::MAX)),
                        ),
                        ("flags", Node::Byte),
                    ]),
                    usize::MAX,
                ),
            ),
            ("replace", Node::Bool),
        ]);
        static $update: Node = Node::Struct(&[
            (
                "item_sets",
                Node::List(
                    &Node::Struct(&[
                        ("key", Node::Identifier),
                        ("items", Node::List(&Node::Id("item"), usize::MAX)),
                    ]),
                    usize::MAX,
                ),
            ),
            (
                "stonecutter",
                Node::List(
                    &Node::Struct(&[
                        ("input", Node::HolderSet("item")),
                        ("display", Node::Ref(&$slot)),
                    ]),
                    usize::MAX,
                ),
            ),
        ]);
    };
}
recipe_packets!(
    ADD_768,
    UPDATE_768,
    layouts::RECIPE_DISPLAY_768,
    layouts::SLOT_DISPLAY_768
);
recipe_packets!(
    ADD_770,
    UPDATE_770,
    layouts::RECIPE_DISPLAY_770,
    layouts::SLOT_DISPLAY_770
);
recipe_packets!(
    ADD_775,
    UPDATE_775,
    layouts::RECIPE_DISPLAY_775,
    layouts::SLOT_DISPLAY_775
);
recipe_packets!(
    ADD_777,
    UPDATE_777,
    layouts::RECIPE_DISPLAY_777,
    layouts::SLOT_DISPLAY_777
);

fn recipe_packet(protocol: i32, name: &str) -> Option<&'static Node> {
    let index = match protocol {
        768..770 => 0,
        770..775 => 1,
        775..777 => 2,
        777.. => 3,
        _ => return None,
    };
    match name {
        "recipe_book_add" => Some([&ADD_768, &ADD_770, &ADD_775, &ADD_777][index]),
        "update_recipes" => Some([&UPDATE_768, &UPDATE_770, &UPDATE_775, &UPDATE_777][index]),
        _ => None,
    }
}

/// Recorded recipe packets parse completely and write back unchanged, and
/// their displays convert to both neighbours and back.
#[test]
fn corpus_recipe_displays_round_trip_and_convert() {
    passthrough_text();
    let all: Vec<i32> = protocols().collect();
    let mut report = Vec::new();
    for (index, &protocol) in all.iter().enumerate() {
        let Some(packets) = testing::corpus(protocol) else {
            continue;
        };
        let (mut seen, mut lossy_count) = (0, 0);
        for packet in packets.iter().filter(|packet| packet.state == State::Play) {
            let name = packet.name(protocol);
            let Some(node) = recipe_packet(protocol, name) else {
                continue;
            };
            let mut reader = Reader::new(&packet.data);
            let value = schema::read(protocol, node, &mut reader, 0)
                .unwrap_or_else(|error| panic!("{name} in {protocol}: {error}"));
            reader.finish().unwrap();
            let mut writer = Writer::new();
            schema::write(protocol, node, &value, &mut writer).unwrap();
            assert_eq!(writer.buf, packet.data, "{name} in {protocol}");
            seen += 1;
            let neighbours = [index.checked_sub(1), Some(index + 1)];
            for to in neighbours.into_iter().flatten().filter_map(|i| all.get(i)) {
                let Some(target) = recipe_packet(*to, name) else {
                    continue;
                };
                let mut shared = step(protocol, *to);
                let (there, lossy) = convert::with(&mut shared, Direction::Clientbound, |cx| {
                    convert_value(cx, node, target, &value).map(|there| (there, cx.lossy))
                })
                .unwrap()
                .unwrap();
                let mut writer = Writer::new();
                schema::write(*to, target, &there, &mut writer).unwrap();
                let mut reader = Reader::new(&writer.buf);
                schema::read(*to, target, &mut reader, 0).unwrap();
                reader.finish().unwrap();
                let mut shared = step(*to, protocol);
                let back = convert::with(&mut shared, Direction::Clientbound, |cx| {
                    convert_value(cx, target, node, &there)
                })
                .unwrap()
                .unwrap();
                if lossy {
                    // Entries the neighbour lacks (newer items) were left out.
                    lossy_count += 1;
                } else {
                    assert_eq!(back, value, "{name} from {protocol} to {to} and back");
                }
            }
        }
        report.push(format!(
            "{protocol}: {seen} recipe packets, {lossy_count} conversions left something out"
        ));
    }
    eprintln!("{}", report.join("\n"));
}

/// Hashes computed by the official 26.3 server's `HashOps.CRC32C_INSTANCE`.
#[test]
fn hashes_match_the_official_server() {
    use super::hashed::hash_component;
    let hash = |kind, value| hash_component(&Component { kind, value }).unwrap();
    assert_eq!(hash("damage", Value::Int(5)), 645064431);
    assert_eq!(hash("damage", Value::Int(-7)), -204940306);
    assert_eq!(
        hash("potion_duration_scale", Value::Float(1.5)),
        -1709970540
    );
    assert_eq!(
        hash("enchantment_glint_override", Value::Bool(true)),
        -1019818302
    );
    assert_eq!(hash("glider", Value::Unit), -982207288);
    assert_eq!(
        hash("item_model", Value::String("minecraft:diamond".into())),
        58140582
    );
    assert_eq!(hash("rarity", Value::Int(2)), -1420566726);
    let nbt = "0a03000161000000010c00056c6f6e677300000001000000000000000601000162030200017300090600016440040000000000000b0004696e747300000003000000010000000200000003050001663e80000007000562797465730000000204050800046e616d6500085269667420e29c930400016c0000011f71fb04cb0900046c69737408000000020001780001790a0005696e6e65720100017a010000";
    let bytes: Vec<u8> = (0..nbt.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&nbt[index..index + 2], 16).unwrap())
        .collect();
    assert_eq!(hash("custom_data", Value::Nbt(bytes)), -2028272530);
}

/// A patch that removes one type, for every type of every protocol, goes to
/// both neighbours and comes back the same.
#[test]
fn every_removed_type_converts_to_both_neighbours_and_back() {
    let all: Vec<i32> = protocols().collect();
    for (index, &protocol) in all.iter().enumerate() {
        let layout = Layout::of(protocol).unwrap();
        let registry = data::version(protocol).expect_registry("data_component_type");
        for id in 0..registry.len() as i32 {
            let (kind, _) = layout.get(id).unwrap();
            let components = Components {
                added: Vec::new(),
                removed: vec![kind],
            };
            let neighbours = [index.checked_sub(1), Some(index + 1)];
            for to in neighbours.into_iter().flatten().filter_map(|i| all.get(i)) {
                let mut converted = components.clone();
                convert(
                    &mut step(protocol, *to),
                    Direction::Clientbound,
                    1,
                    &mut converted,
                )
                .unwrap();
                assert_eq!(decode(*to, &encode(*to, &converted)), converted);
                let mut back = converted;
                convert(
                    &mut step(*to, protocol),
                    Direction::Clientbound,
                    1,
                    &mut back,
                )
                .unwrap();
                assert_eq!(
                    back, components,
                    "removing {kind} from {protocol} to {to} and back"
                );
            }
        }
    }
}

/// Only pairs without any change to components report them identical.
#[test]
fn identical_pairs_have_nothing_to_convert() {
    let all: Vec<i32> = protocols().collect();
    let identical: Vec<_> = all
        .windows(2)
        .filter(|pair| super::identical(pair[0], pair[1]))
        .map(|pair| pair[0])
        .collect();
    eprintln!("pairs with identical components: {identical:?}");
    for older in &identical {
        assert!(!super::rules::PAIRS.contains(older));
    }
}
