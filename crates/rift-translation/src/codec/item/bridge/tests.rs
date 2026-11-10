use super::{NEW, OLD, same_nbt, same_patch};
use crate::codec::{
    item::components::{Component, Components, schema::Value},
    text::{self, component::HoverEvent, component::ItemInfo},
};
use crate::{
    Direction,
    codec::testing::{Recorded, corpus, corpus_in},
    codec::{
        Shared,
        item::{ItemStack, Stack, convert::item_name, window},
    },
    nbt::{Compound, Nbt},
    wire::{Reader, Writer},
};

/// Every distinct non-empty stack a server put in the player's inventory
/// in a recorded session, in order.
fn stacks_in(protocol: i32, packets: &[Recorded]) -> Vec<ItemStack> {
    let mut stacks: Vec<ItemStack> = Vec::new();
    for packet in packets
        .iter()
        .filter(|packet| packet.direction == Direction::Clientbound)
    {
        let mut reader = Reader::new(&packet.data);
        let found = match packet.name(protocol) {
            "container_set_slot" => {
                vec![window::SetSlot::read(protocol, &mut reader).unwrap().item]
            }
            "container_set_content" => {
                window::SetContent::read(protocol, &mut reader)
                    .unwrap()
                    .items
            }
            _ => continue,
        };
        for stack in found.into_iter().flatten() {
            if !stacks.contains(&stack) {
                stacks.push(stack);
            }
        }
    }
    stacks
}

/// The stacks of the shared corpus's session of `protocol`.
pub(super) fn given_stacks(protocol: i32) -> Option<Vec<ItemStack>> {
    Some(stacks_in(protocol, &corpus(protocol)?))
}

fn private(directory: &str, protocol: i32) -> Option<Vec<ItemStack>> {
    let home = std::env::var_os("HOME")?;
    let directory = std::path::Path::new(&home).join(directory);
    Some(stacks_in(protocol, &corpus_in(&directory, protocol)?))
}

/// The private corpus of many more components (1.20.5 only), when captured.
fn component_corpus() -> Option<Vec<ItemStack>> {
    private(".cache/rift-via/components/corpus", NEW)
}

/// The bridge's private corpus (`~/.cache/rift-via/bridge/capture.py`): the
/// same stacks given in 1.20.4 tag syntax and 1.20.5 component syntax, each
/// marked with `rift_pair` (in the tag, or in `custom_data`).
fn paired_corpus() -> Option<Vec<(i32, ItemStack, ItemStack)>> {
    let marker = |stack: &ItemStack| -> Option<i32> {
        let tag = match &stack.components {
            Some(components) => components.get("custom_data")?.as_nbt().ok()??,
            None => stack.tag.clone()?,
        };
        tag.as_compound()?.int("rift_pair")
    };
    let directory = ".cache/rift-via/bridge/corpus";
    let old = private(directory, OLD)?;
    let new = private(directory, NEW)?;
    let mut pairs = Vec::new();
    for stack in &old {
        let Some(index) = marker(stack) else { continue };
        if pairs.iter().any(|(known, _, _)| *known == index) {
            continue;
        }
        // A command one of the servers rejected has no pair.
        let Some(other) = new.iter().find(|other| marker(other) == Some(index)) else {
            eprintln!("pair {index} only on 1.20.4");
            continue;
        };
        pairs.push((index, stack.clone(), other.clone()));
    }
    Some(pairs)
}

/// Convert across the bridge in `direction` on a step for `client`.
fn across(client: i32, backend: i32, direction: Direction, stack: &ItemStack) -> Stack {
    let mut shared = Shared::new(client, backend);
    let mut converted = Some(stack.clone());
    super::convert(&mut shared, direction, &mut converted)
        .unwrap_or_else(|error| panic!("{stack:?}: {error}"));
    // What comes out is a valid stack of the receiver's protocol.
    let target = shared.target(direction);
    let mut writer = Writer::new();
    crate::codec::item::write(target, converted.as_ref(), &mut writer).unwrap();
    let mut reader = Reader::new(&writer.buf);
    let read = crate::codec::item::read(target, &mut reader).unwrap();
    reader.finish().unwrap();
    assert!(read.is_some() == converted.is_some());
    converted
}

fn up(stack: &ItemStack) -> Stack {
    across(NEW, OLD, Direction::Clientbound, stack)
}

fn down(stack: &ItemStack) -> Stack {
    across(OLD, NEW, Direction::Clientbound, stack)
}

fn up_serverbound(stack: &ItemStack) -> Stack {
    across(OLD, NEW, Direction::Serverbound, stack)
}

fn down_serverbound(stack: &ItemStack) -> Stack {
    across(NEW, OLD, Direction::Serverbound, stack)
}

fn name(protocol: i32, stack: &ItemStack) -> &'static str {
    item_name(protocol, stack.id).unwrap()
}

/// NBT as vanilla's `NbtOps` rewrites it when a 1.20.5 server decodes a
/// component: lists of bytes, ints and longs become arrays; compound keys
/// sorted for comparison.
fn normalized_nbt(nbt: &Nbt) -> Nbt {
    match nbt {
        Nbt::Compound(compound) => {
            let mut entries: Vec<_> = compound
                .iter()
                .map(|(key, value)| (key.to_owned(), normalized_nbt(value)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Nbt::Compound(Compound(entries))
        }
        Nbt::List(list) if !list.is_empty() && list.iter().all(|e| matches!(e, Nbt::Int(_))) => {
            Nbt::IntArray(
                list.iter()
                    .filter_map(|e| e.as_i64().map(|v| v as i32))
                    .collect(),
            )
        }
        Nbt::List(list) if !list.is_empty() && list.iter().all(|e| matches!(e, Nbt::Long(_))) => {
            Nbt::LongArray(list.iter().filter_map(Nbt::as_i64).collect())
        }
        Nbt::List(list) if !list.is_empty() && list.iter().all(|e| matches!(e, Nbt::Byte(_))) => {
            Nbt::ByteArray(
                list.iter()
                    .filter_map(|e| e.as_i64().map(|v| v as i8 as u8))
                    .collect(),
            )
        }
        Nbt::List(list) => Nbt::List(list.iter().map(normalized_nbt).collect()),
        other => other.clone(),
    }
}

/// A patch with what carries no meaning taken out: map-like lists sorted
/// (enchantment levels and block state properties are maps in 1.20.5,
/// sent in hash order) and NBT normalised as above.
fn normalized_patch(patch: &Components) -> Components {
    fn value(kind: &str, original: &Value) -> Value {
        match (kind, original) {
            (_, Value::Nbt(_)) => match original.as_nbt() {
                Ok(Some(nbt)) => Value::nbt(&normalized_nbt(&nbt)),
                _ => original.clone(),
            },
            ("enchantments" | "stored_enchantments", Value::Struct(fields)) => {
                let mut fields = fields.clone();
                if let Value::List(levels) = &mut fields[0] {
                    levels.sort_by_key(|level| format!("{level:?}"));
                }
                Value::Struct(fields)
            }
            ("block_state", Value::List(properties)) => {
                let mut properties = properties.clone();
                properties.sort_by_key(|property| format!("{property:?}"));
                Value::List(properties)
            }
            (_, Value::List(items)) => {
                Value::List(items.iter().map(|item| value("", item)).collect())
            }
            (_, Value::Stack(Some(stack))) => {
                let mut stack = stack.clone();
                stack.components = stack.components.as_ref().map(normalized_patch);
                Value::Stack(Some(stack))
            }
            _ => original.clone(),
        }
    }
    Components {
        added: patch
            .added
            .iter()
            .map(|component| Component {
                kind: component.kind,
                value: value(component.kind, &component.value),
            })
            .collect(),
        removed: patch.removed.clone(),
    }
}

/// Effect flags at their defaults dropped (vanilla writes them all, `/give`
/// keeps what it was given, and they mean the same), and container items in
/// slot order (1.20.5 keeps them by slot).
fn round_trip_normalized(nbt: &mut Nbt) {
    match nbt {
        Nbt::Compound(compound) => {
            for (key, value) in compound.0.iter_mut() {
                round_trip_normalized(value);
                if let ("Items", Nbt::List(items)) = (key.as_str(), value) {
                    items.sort_by_key(|item| item.as_compound().and_then(|item| item.int("Slot")));
                }
            }
            if compound.contains("duration") && compound.contains("id") {
                let particles = compound.get("show_particles").and_then(Nbt::as_i64);
                if compound.get("show_icon").and_then(Nbt::as_i64) == particles.or(Some(1)) {
                    compound.remove("show_icon");
                }
                if compound.get("ambient").and_then(Nbt::as_i64) == Some(0) {
                    compound.remove("ambient");
                }
                if particles == Some(1) {
                    compound.remove("show_particles");
                }
            }
        }
        Nbt::List(list) => list.iter_mut().for_each(round_trip_normalized),
        _ => {}
    }
}

/// A 1.20.4 tag with what carries no meaning taken out: enchantment lists
/// and container items sorted, default effect flags dropped, NBT
/// normalised as above.
fn normalized_tag(nbt: &Nbt) -> Nbt {
    fn sort(nbt: &mut Nbt) {
        match nbt {
            Nbt::Compound(compound) => {
                for (key, value) in compound.0.iter_mut() {
                    sort(value);
                    if let ("Enchantments" | "StoredEnchantments" | "Items", Nbt::List(list)) =
                        (key.as_str(), value)
                    {
                        list.sort_by_key(|entry| format!("{entry:?}"));
                    }
                }
            }
            Nbt::List(list) => list.iter_mut().for_each(sort),
            _ => {}
        }
    }
    let mut nbt = normalized_nbt(nbt);
    sort(&mut nbt);
    round_trip_normalized(&mut nbt);
    nbt
}

fn same_components(a: &ItemStack, b: &ItemStack) -> bool {
    a.id == b.id
        && a.count == b.count
        && same_patch(
            a.components.as_ref().unwrap_or(&Default::default()),
            b.components.as_ref().unwrap_or(&Default::default()),
        )
}

/// Whether a 1.20.4 stack came back from 1.20.5 as it went: the same tag
/// after [`round_trip_normalized`].
fn came_back(back: &ItemStack, original: &ItemStack) -> bool {
    let normalized = |stack: &ItemStack| {
        let mut stack = stack.clone();
        if let Some(tag) = &mut stack.tag {
            round_trip_normalized(tag);
        }
        stack
    };
    same_stack_tag(&normalized(back), &normalized(original))
}

/// [`same_components`] after [`normalized_patch`].
fn same_meaning(a: &ItemStack, b: &ItemStack) -> bool {
    let normalized = |stack: &ItemStack| ItemStack {
        components: stack.components.as_ref().map(normalized_patch),
        ..stack.clone()
    };
    same_components(&normalized(a), &normalized(b))
}

/// [`same_stack_tag`] after [`normalized_tag`].
fn same_tag_meaning(a: &ItemStack, b: &ItemStack) -> bool {
    let normalized = |stack: &ItemStack| ItemStack {
        tag: stack.tag.as_ref().map(normalized_tag),
        ..stack.clone()
    };
    same_stack_tag(&normalized(a), &normalized(b))
}

/// Tags that mean the same: numbers by value (1.20.4 reads them leniently),
/// JSON text by meaning, compounds in any order.
fn same_tag(a: &Nbt, b: &Nbt) -> bool {
    match (a, b) {
        (Nbt::Compound(x), Nbt::Compound(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, value)| y.get(key).is_some_and(|other| same_tag(value, other)))
        }
        (Nbt::List(x), Nbt::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_tag(x, y))
        }
        (Nbt::String(x), Nbt::String(y)) => {
            x == y
                || matches!(
                    (crate::codec::text::from_json_str(OLD, x), crate::codec::text::from_json_str(OLD, y)),
                    (Ok(x), Ok(y)) if x == y
                )
        }
        (Nbt::IntArray(x), Nbt::IntArray(y)) => x == y,
        _ => match (a.as_i64(), b.as_i64()) {
            (Some(x), Some(y)) if a.kind() <= 4 && b.kind() <= 4 => x == y,
            _ => match (a.as_f64(), b.as_f64()) {
                (Some(x), Some(y)) => x == y,
                _ => same_nbt(a, b),
            },
        },
    }
}

fn same_stack_tag(a: &ItemStack, b: &ItemStack) -> bool {
    let empty = Nbt::Compound(Compound::new());
    a.id == b.id
        && a.count == b.count
        && same_tag(
            a.tag.as_ref().unwrap_or(&empty),
            b.tag.as_ref().unwrap_or(&empty),
        )
}

/// What the 1.20.4 server sent for each `/give` of the scenario, converted,
/// is what the 1.20.5 server sent for the same command, and the reverse.
#[test]
fn recorded_stacks_convert_to_what_the_other_server_sends() {
    let (Some(old), Some(new)) = (given_stacks(OLD), given_stacks(NEW)) else {
        eprintln!("corpus for {OLD} or {NEW} not captured: skipped");
        return;
    };
    let mut compared = 0;
    for stack in &old {
        let converted = up(stack).unwrap();
        let candidates: Vec<_> = new
            .iter()
            .filter(|other| name(NEW, other) == name(OLD, stack))
            .collect();
        assert!(!candidates.is_empty(), "no 1.20.5 {}", name(OLD, stack));
        assert!(
            candidates
                .iter()
                .any(|other| same_components(&converted, other)),
            "{} converted to\n{converted:?}\nbut 1.20.5 sent\n{candidates:?}",
            name(OLD, stack)
        );
        compared += 1;
    }
    for stack in &new {
        let candidates: Vec<_> = old
            .iter()
            .filter(|other| name(OLD, other) == name(NEW, stack))
            .collect();
        if candidates.is_empty() {
            // Only given with component syntax (food, attribute modifiers
            // with identifiers, charged projectiles).
            continue;
        }
        let converted = down(stack).unwrap();
        assert!(
            // Two `/give`s of the same book stack on 1.20.5: counts may differ.
            candidates.iter().any(|other| same_stack_tag(
                &converted,
                &ItemStack {
                    count: converted.count,
                    ..(*other).clone()
                }
            )),
            "{} converted to\n{converted:?}\nbut 1.20.4 sent\n{candidates:?}",
            name(NEW, stack)
        );
        compared += 1;
    }
    eprintln!("{compared} stacks compared with what the other server sent");
}

/// Stacks survive the way to the other protocol and back on one
/// connection: to a client and back from it (creative inventory, clicks),
/// or from a client to the server and back.
#[test]
fn recorded_stacks_round_trip_across_the_bridge() {
    type Convert = fn(&ItemStack) -> Stack;
    let pairs = paired_corpus().unwrap_or_default();
    let mut checked = 0;
    let old = given_stacks(OLD).unwrap_or_default();
    for stack in old.iter().chain(pairs.iter().map(|(_, old, _)| old)) {
        for (there, back) in [
            (up as Convert, down_serverbound as Convert),
            (up_serverbound, down),
        ] {
            let back = back(&there(stack).unwrap()).unwrap();
            assert!(came_back(&back, stack), "{stack:?}\ncame back as\n{back:?}");
        }
        checked += 1;
    }
    let new = given_stacks(NEW).unwrap_or_default();
    let components = component_corpus().unwrap_or_default();
    for stack in new
        .iter()
        .chain(&components)
        .chain(pairs.iter().map(|(_, _, new)| new))
    {
        for (there, back) in [
            (down as Convert, up_serverbound as Convert),
            (down_serverbound, up),
        ] {
            let back = back(&there(stack).unwrap()).unwrap();
            assert!(
                same_components(&back, stack),
                "{stack:?}\ncame back as\n{back:?}"
            );
        }
        checked += 1;
    }
    eprintln!("{checked} recorded stacks round-tripped");
}

#[test]
#[ignore = "prints the recorded stacks and their conversions"]
fn dump_given_stacks() {
    for stack in given_stacks(OLD).unwrap_or_default() {
        println!("765 {} x{} {:?}", name(OLD, &stack), stack.count, stack.tag);
        println!("  -> {:?}", up(&stack));
    }
    for stack in given_stacks(NEW)
        .unwrap_or_default()
        .iter()
        .chain(component_corpus().unwrap_or_default().iter())
    {
        println!(
            "766 {} x{} {:?}",
            name(NEW, stack),
            stack.count,
            stack.components
        );
        println!("  -> {:?}", down(stack));
    }
}

#[test]
#[ignore = "prints recorded 1.20.5 stacks as hex"]
fn dump_hex() {
    for stack in given_stacks(NEW)
        .unwrap_or_default()
        .iter()
        .chain(component_corpus().unwrap_or_default().iter())
    {
        let mut writer = Writer::new();
        crate::codec::item::write(NEW, Some(stack), &mut writer).unwrap();
        let hex: String = writer.buf.iter().map(|b| format!("{b:02x}")).collect();
        println!("{} {hex}", name(NEW, stack));
    }
}

/// Each pair of the bridge's private corpus: the 1.20.4 server's stack
/// converted up is the 1.20.5 server's, and the reverse.
#[test]
fn paired_stacks_convert_to_what_the_other_server_sends() {
    let Some(pairs) = paired_corpus() else {
        eprintln!("bridge corpus not captured: skipped");
        return;
    };
    let mut failures = Vec::new();
    for (index, old, new) in &pairs {
        let converted = up(old).unwrap();
        if !same_meaning(&converted, new) {
            failures.push(format!(
                "pair {index} up: {}\n  1.20.4 {:?}\n  got    {:?}\n  1.20.5 {:?}",
                name(OLD, old),
                old.tag,
                converted.components,
                new.components
            ));
        }
        let converted = down(new).unwrap();
        if !same_tag_meaning(&converted, old) {
            failures.push(format!(
                "pair {index} down: {}\n  1.20.5 {:?}\n  got    {:?}\n  1.20.4 {:?}",
                name(NEW, new),
                new.components,
                converted.tag,
                old.tag
            ));
        }
    }
    eprintln!("{} pairs compared", pairs.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
#[ignore = "prints the bridge corpus pairs as hex"]
fn dump_pairs_hex() {
    let hex = |protocol, stack: &ItemStack| {
        let mut writer = Writer::new();
        crate::codec::item::write(protocol, Some(stack), &mut writer).unwrap();
        writer
            .buf
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    for (index, old, new) in paired_corpus().unwrap_or_default() {
        println!(
            "{index} {} {} {}",
            name(OLD, &old),
            hex(OLD, &old),
            hex(NEW, &new)
        );
    }
}

/// Stacks the vanilla 1.20.4 and 1.20.6 servers sent for the same `/give`
/// (from the bridge corpus): item, 1.20.4 stack, 1.20.5 stack, as hex.
const PAIRS: &[(&str, &str, &str)] = &[
    (
        "diamond_sword",
        "01c206010a03000644616d6167650000000003000948696465466c61677300000001030009726966745f706169720000000409000c456e6368616e746d656e74730a000000020200036c766c0003080002696400136d696e6563726166743a73686172706e657373000200036c766c0002080002696400146d696e6563726166743a756e627265616b696e670000",
        "01c5060200090216020d0300000a030009726966745f706169720000000400",
    ),
    (
        "stick",
        "01cc06010a030009726966745f70616972000000070a0007646973706c61790900044c6f726508000000020003226122001b7b2274657874223a2262222c226974616c6963223a66616c73657d0800044e616d65001a7b2274657874223a224e222c22636f6c6f72223a22726564227d0000",
        "01cf060300050a080005636f6c6f7200037265640800047465787400014e000702080001610a080004746578740001620100066974616c69630000000a030009726966745f706169720000000700",
    ),
    (
        "stick",
        "01cc06010a030009726966745f706169720000000d0900124174747269627574654d6f646966696572730a00000001060006416d6f756e743ff0000000000000080004536c6f7400046865616408000d4174747269627574654e616d6500176d696e6563726166743a67656e657269632e61726d6f720300094f7065726174696f6e000000000b00045555494400000004000000010000000200000003000000040800044e616d650004726966740000",
        "01cf0602000c01000000000100000002000000030000000404726966743ff0000000000000000701000a030009726966745f706169720000000d00",
    ),
    (
        "iron_chestplate",
        "01de06010a03000644616d616765000000000a00045472696d0800086d6174657269616c000e6d696e6563726166743a676f6c640800077061747465726e000f6d696e6563726166743a636f6173740003000948696465466c61677300000080030009726966745f706169720000000f00",
        "01e106020023050100000a030009726966745f706169720000000f00",
    ),
    (
        "crossbow",
        "019e09010a0900124368617267656450726f6a656374696c65730a000000010800026964000f6d696e6563726166743a6172726f77010005436f756e74010003000644616d616765000000000100074368617267656401030009726966745f706169720000001200",
        "01a50902001d0101a1060000000a030009726966745f706169720000001200",
    ),
    (
        "written_book",
        "01be08010a03000a67656e65726174696f6e0000000109000570616765730800000002000d7b2274657874223a227031227d000422703222030009726966745f7061697200000017080006617574686f720001410800057469746c650001540100087265736f6c7665640100",
        "01c4080200220154000141010208000270310008000270320001000a030009726966745f706169720000001700",
    ),
    (
        "firework_rocket",
        "01d108010a030009726966745f706169720000001d0a000946697265776f726b73010006466c696768740209000a4578706c6f73696f6e730a0000000101000454797065040b0006436f6c6f7273000000020000000100000002010005547261696c010b000a46616465436f6c6f72730000000100000003010007466c69636b657201000000",
        "01d80802002d02010402000000010000000201000000030101000a030009726966745f706169720000001d00",
    ),
    (
        "player_head",
        "01ca08010a030009726966745f706169720000001f0a000a536b756c6c4f776e65720b0002496400000004000000010000000200000003000000040a000a50726f7065727469657309000874657874757265730a0000000108000556616c756500046533303d00000800044e616d650004526966740000",
        "01d10802002e010452696674010000000100000002000000030000000401087465787475726573046533303d00000a030009726966745f706169720000001f00",
    ),
    (
        "potion",
        "01e307010a090015637573746f6d5f706f74696f6e5f656666656374730a000000010300086475726174696f6e00000064010009616d706c69666965720101000973686f775f69636f6e000800026964000f6d696e6563726166743a737065656400030011437573746f6d506f74696f6e436f6c6f72000000ff080006506f74696f6e00126d696e6563726166743a737472656e677468030009726966745f706169720000002000",
        "01e60702001f012201000000ff0100016400010000000a030009726966745f706169720000002000",
    ),
    (
        "chest",
        "01aa02010a030009726966745f70616972000000270a000e426c6f636b456e746974795461670900054974656d730a00000002010004536c6f74010800026964000f6d696e6563726166743a73746f6e65010005436f756e740300010004536c6f7400080002696400176d696e6563726166743a6469616d6f6e645f73776f7264010005436f756e74010a000374616703000644616d6167650000000300000000",
        "01ab020200330201c5060100030303010000000a030009726966745f706169720000002700",
    ),
    (
        "shield",
        "018309010a03000644616d61676500000000030009726966745f706169720000002a0a000e426c6f636b456e746974795461670900085061747465726e730a000000010800075061747465726e00027473030005436f6c6f720000000b00030004426173650000000e0000",
        "018a090300310e3001250b000a030009726966745f706169720000002a00",
    ),
    (
        "diamond_sword",
        "01c206010a01000b556e627265616b61626c650103000644616d6167650000000003000948696465466c61677300000027030009726966745f706169720000003509000c456e6368616e746d656e74730a000000010200036c766c0001080002696400136d696e6563726166743a73686172706e6573730000",
        "01c506050009010d010004000e0c0202cb3f55d3645c4f38a4979c13a33db5cf0f576561706f6e206d6f6469666965724018000000000000000104fa233e1c41804865b01bbcce9785aca30f576561706f6e206d6f646966696572c003333340000000000100000a030009726966745f706169720000003500",
    ),
    (
        "diamond_sword",
        "01c206010a03000644616d6167650000000003000948696465466c61677300000002030009726966745f706169720000003d00",
        "01c50602000c0202cb3f55d3645c4f38a4979c13a33db5cf0f576561706f6e206d6f6469666965724018000000000000000104fa233e1c41804865b01bbcce9785aca30f576561706f6e206d6f646966696572c003333340000000000100000a030009726966745f706169720000003d00",
    ),
];

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect()
}

fn stack(protocol: i32, hex: &str) -> ItemStack {
    let bytes = unhex(hex);
    let mut reader = Reader::new(&bytes);
    let stack = crate::codec::item::read(protocol, &mut reader)
        .unwrap()
        .unwrap();
    reader.finish().unwrap();
    stack
}

#[test]
fn inline_pairs_convert_both_ways_and_back() {
    for (item, old, new) in PAIRS {
        let (old, new) = (stack(OLD, old), stack(NEW, new));
        assert!(same_meaning(&up(&old).unwrap(), &new), "{item} up");
        assert!(same_tag_meaning(&down(&new).unwrap(), &old), "{item} down");
        let back = down_serverbound(&up(&old).unwrap()).unwrap();
        assert!(came_back(&back, &old), "{item}: {back:?}");
        let back = up_serverbound(&down(&new).unwrap()).unwrap();
        assert!(same_components(&back, &new), "{item}: {back:?}");
    }
}

fn old_stack(item: &str, snbt: &str) -> ItemStack {
    ItemStack {
        id: crate::codec::item::convert::item_id(OLD, item).unwrap(),
        count: 1,
        damage: 0,
        tag: Some(crate::codec::text::snbt::parse(OLD, snbt).unwrap()),
        components: None,
    }
}

fn components_of(stack: &Stack) -> &Components {
    stack.as_ref().unwrap().components.as_ref().unwrap()
}

#[test]
fn hide_flags_that_hide_nothing_travel_in_custom_data() {
    let stack = old_stack("stick", "{HideFlags:196}");
    let there = up(&stack);
    let data = components_of(&there)
        .get("custom_data")
        .unwrap()
        .as_nbt()
        .unwrap();
    assert_eq!(
        data.unwrap().as_compound().unwrap().int("HideFlags"),
        Some(196)
    );
    assert!(components_of(&there).get("dyed_color").is_none());
    assert!(came_back(
        &down_serverbound(&there.unwrap()).unwrap(),
        &stack
    ));
}

#[test]
fn hidden_default_modifiers_keep_the_prototype() {
    // Weapons: the prototype's modifiers, hidden; an empty 1.20.5 list
    // would take a sword's attack damage away.
    let sword = old_stack("diamond_sword", "{Damage:0,HideFlags:2}");
    let there = up(&sword);
    let modifiers = components_of(&there).get("attribute_modifiers").unwrap();
    assert_eq!(modifiers.fields().unwrap()[0].items().unwrap().len(), 2);
    assert_eq!(modifiers.fields().unwrap()[1], Value::Bool(false));
    assert!(came_back(
        &down_serverbound(&there.unwrap()).unwrap(),
        &sword
    ));
    // Armour: an empty list falls back to the armour's own modifiers.
    let armour = old_stack("iron_chestplate", "{Damage:0,HideFlags:2}");
    let there = up(&armour);
    let modifiers = components_of(&there).get("attribute_modifiers").unwrap();
    assert!(modifiers.fields().unwrap()[0].items().unwrap().is_empty());
    assert!(came_back(
        &down_serverbound(&there.unwrap()).unwrap(),
        &armour
    ));
    // No modifiers on armour cannot be said in 1.20.5: kept in custom_data.
    let bare = old_stack("iron_chestplate", "{Damage:0,AttributeModifiers:[]}");
    let there = up(&bare);
    assert!(components_of(&there).get("attribute_modifiers").is_none());
    assert!(came_back(
        &down_serverbound(&there.unwrap()).unwrap(),
        &bare
    ));
}

#[test]
fn entity_data_gets_an_id_only_for_servers() {
    let egg = old_stack("armor_stand", "{EntityTag:{Small:1b}}");
    let entity = |stack: &Stack| {
        let data = components_of(stack)
            .get("entity_data")
            .unwrap()
            .as_nbt()
            .unwrap();
        data.unwrap()
            .as_compound()
            .unwrap()
            .str("id")
            .map(str::to_owned)
    };
    assert_eq!(entity(&up(&egg)), None);
    let to_server = up_serverbound(&egg);
    assert_eq!(entity(&to_server).as_deref(), Some("minecraft:armor_stand"));
    let back = down(&to_server.unwrap()).unwrap();
    let tag = back.tag.unwrap();
    let entity_tag = tag.as_compound().unwrap().compound("EntityTag").unwrap();
    assert_eq!(entity_tag.int("Small"), Some(1));
}

#[test]
fn malformed_tags_fail_or_travel_unchanged() {
    let mut not_compound = old_stack("stick", "{}");
    not_compound.tag = Some(Nbt::Int(1));
    let mut shared = Shared::new(NEW, OLD);
    assert!(super::convert(&mut shared, Direction::Clientbound, &mut Some(not_compound)).is_err());
    // Keys of the wrong type are no component: they ride in custom_data.
    for snbt in [
        "{Enchantments:5,display:{Name:7,Lore:\"x\"},Damage:\"a\",Trim:{material:1}}",
        "{CanDestroy:[1],AttributeModifiers:[{AttributeName:\"minecraft:nope\"}]}",
        "{SkullOwner:5,BlockEntityTag:{Items:\"x\"}}",
    ] {
        // A stick: a sword always carries `Damage` in 1.20.4 and gets it.
        let stack = old_stack("stick", snbt);
        let there = up(&stack);
        assert!(components_of(&there).get("custom_data").is_some(), "{snbt}");
        assert!(
            came_back(&down_serverbound(&there.unwrap()).unwrap(), &stack),
            "{snbt}"
        );
    }
    // Bundles nested past the limit fail rather than recurse without end.
    let mut tag = Compound::new();
    for _ in 0..100 {
        let mut inner = Compound::new();
        inner.insert("id", "minecraft:bundle");
        inner.insert("Count", Nbt::Byte(1));
        inner.insert("tag", tag);
        tag = Compound::new();
        tag.insert("Items", Nbt::List(vec![Nbt::Compound(inner)]));
    }
    let mut stack = old_stack("bundle", "{}");
    stack.tag = Some(Nbt::Compound(tag));
    let mut shared = Shared::new(NEW, OLD);
    assert!(super::convert(&mut shared, Direction::Clientbound, &mut Some(stack)).is_err());
}

/// The `show_item` hovers of the bridge corpus's `tellraw hover <n>`
/// messages, by `n`.
fn hovers(protocol: i32) -> Option<Vec<(String, ItemInfo)>> {
    let home = std::env::var_os("HOME")?;
    let directory = std::path::Path::new(&home).join(".cache/rift-via/bridge/corpus");
    let mut found = Vec::new();
    for packet in corpus_in(&directory, protocol)? {
        if packet.direction != Direction::Clientbound || packet.name(protocol) != "system_chat" {
            continue;
        }
        let mut reader = Reader::new(&packet.data);
        let Ok(Some(nbt)) = Nbt::read(&mut reader, false) else {
            continue;
        };
        let Ok(component) = text::from_nbt(protocol, &nbt) else {
            continue;
        };
        if let (text::component::Content::Text(label), Some(HoverEvent::ShowItem(item))) =
            (&component.content, &component.style.hover)
            && label.starts_with("hover ")
        {
            found.push((label.clone(), item.clone()));
        }
    }
    Some(found)
}

/// Each hover of the bridge corpus converted across the bridge is what the
/// other server sent for the same `tellraw`.
#[test]
fn hover_stacks_convert_to_what_the_other_server_sends() {
    let (Some(old), Some(new)) = (hovers(OLD), hovers(NEW)) else {
        eprintln!("bridge corpus not captured: skipped");
        return;
    };
    assert!(
        !old.is_empty() && old.len() == new.len(),
        "{} {}",
        old.len(),
        new.len()
    );
    let mut failures = Vec::new();
    for ((label, old), (other, new)) in old.iter().zip(&new) {
        assert_eq!(label, other);
        let mut up = old.clone();
        let mut shared = Shared::new(NEW, OLD);
        let converted = crate::codec::item::convert_hover_stack(
            &mut shared,
            Direction::Clientbound,
            &mut up.id,
            &mut up.count,
            &mut up.tag,
            &mut up.components,
        );
        assert!(converted.unwrap(), "{label}");
        let empty = Nbt::Compound(Compound::new());
        let as_nbt = |c: &Option<Compound>| c.clone().map(Nbt::Compound).unwrap_or(empty.clone());
        if up.id != new.id
            || !same_tag(
                &normalized_nbt(&as_nbt(&up.components)),
                &normalized_nbt(&as_nbt(&new.components)),
            )
        {
            failures.push(format!(
                "{label} up:\n  got    {:?}\n  1.20.5 {:?}",
                up.components, new.components
            ));
        }
        let mut down = new.clone();
        let mut shared = Shared::new(OLD, NEW);
        let converted = crate::codec::item::convert_hover_stack(
            &mut shared,
            Direction::Clientbound,
            &mut down.id,
            &mut down.count,
            &mut down.tag,
            &mut down.components,
        );
        assert!(converted.unwrap(), "{label}");
        if down.id != old.id
            || !same_tag(
                &normalized_tag(&as_nbt(&down.tag)),
                &normalized_tag(&as_nbt(&old.tag)),
            )
        {
            failures.push(format!(
                "{label} down:\n  got    {:?}\n  1.20.4 {:?}",
                down.tag, old.tag
            ));
        }
    }
    eprintln!("{} hovers compared", old.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// `tellraw` with a `show_item` hover of an enchanted, named sword, as the
/// vanilla 1.20.4 and 1.20.6 servers sent it (`system_chat`).
const HOVER_OLD: &str = "0a0a000a686f7665724576656e740a0008636f6e74656e7473080002696400176d696e6563726166743a6469616d6f6e645f73776f726408000374616700787b44616d6167653a332c456e6368616e746d656e74733a5b7b69643a226d696e6563726166743a73686172706e657373222c6c766c3a32737d5d2c646973706c61793a7b4c6f72653a5b27226122275d2c4e616d653a277b2274657874223a22426c616465222c22636f6c6f72223a22726564227d277d7d00080006616374696f6e000973686f775f6974656d00080004746578740007686f76657220300000";
const HOVER_NEW: &str = "0a0a000a686f7665724576656e740a0008636f6e74656e74730a000a636f6d706f6e656e747309000e6d696e6563726166743a6c6f7265080000000100032261220a00166d696e6563726166743a656e6368616e746d656e74730a00066c6576656c730300136d696e6563726166743a73686172706e6573730000000200000300106d696e6563726166743a64616d616765000000030800156d696e6563726166743a637573746f6d5f6e616d65001e7b22636f6c6f72223a22726564222c2274657874223a22426c616465227d00030005636f756e7400000001080002696400176d696e6563726166743a6469616d6f6e645f73776f726400080006616374696f6e000973686f775f6974656d00080004746578740007686f76657220300000";

fn hover_of(protocol: i32, body: &[u8]) -> ItemInfo {
    let mut reader = Reader::new(body);
    let nbt = Nbt::read(&mut reader, false).unwrap().unwrap();
    match text::from_nbt(protocol, &nbt).unwrap().style.hover {
        Some(HoverEvent::ShowItem(item)) => item,
        other => panic!("{other:?}"),
    }
}

/// The whole path: the text codec converts a real `system_chat` with a
/// `show_item` hover through the bridge, both ways.
#[test]
fn chat_hovers_cross_the_bridge() {
    use crate::{State, codec::testing::Defaults};
    let empty = Nbt::Compound(Compound::new());
    for (client, backend, from, to) in [
        (NEW, OLD, HOVER_OLD, HOVER_NEW),
        (OLD, NEW, HOVER_NEW, HOVER_OLD),
    ] {
        let mut step = Defaults::new(client, backend);
        let outs = step
            .clientbound(State::Play, "system_chat", &unhex(from))
            .unwrap();
        let got = hover_of(client, &outs[0].data);
        let want = hover_of(client, &unhex(to));
        assert_eq!((&got.id, got.count), (&want.id, want.count));
        let nbt = |c: &Option<Compound>| c.clone().map(Nbt::Compound).unwrap_or(empty.clone());
        assert!(
            same_tag(
                &normalized_tag(&nbt(&got.tag)),
                &normalized_tag(&nbt(&want.tag))
            ),
            "{got:?}"
        );
        assert!(
            same_tag(
                &normalized_nbt(&nbt(&got.components)),
                &normalized_nbt(&nbt(&want.components))
            ),
            "{got:?}"
        );
    }
}

/// What the persistent encoder writes, it reads back, for every recorded
/// 1.20.5 stack (components it does not cover are left out).
#[test]
fn persistent_components_round_trip() {
    let new = given_stacks(NEW).unwrap_or_default();
    let components = component_corpus().unwrap_or_default();
    let pairs = paired_corpus().unwrap_or_default();
    let mut checked = 0;
    let mut shared = Shared::new(NEW, OLD);
    let mut bridge = super::Bridge::new(&mut shared, Direction::Clientbound);
    for stack in new
        .iter()
        .chain(&components)
        .chain(pairs.iter().map(|(_, _, new)| new))
    {
        let patch = stack.components.clone().unwrap_or_default();
        let encoded = super::persistent::encode(&mut bridge, &patch);
        let decoded = super::persistent::decode(&mut bridge, &encoded);
        let covered = Components {
            added: patch
                .added
                .iter()
                .filter(|component| encoded.contains(&format!("minecraft:{}", component.kind)))
                .cloned()
                .collect(),
            removed: patch.removed.clone(),
        };
        assert!(
            same_meaning(
                &ItemStack {
                    components: Some(decoded),
                    ..stack.clone()
                },
                &ItemStack {
                    components: Some(covered),
                    ..stack.clone()
                },
            ),
            "{stack:?}\n{encoded:?}"
        );
        checked += 1;
    }
    eprintln!("{checked} stacks through the persistent form");
}
