use super::{NEW, OLD, same_nbt, same_patch};
use crate::{
    Direction,
    codec::testing::corpus,
    codec::{
        Shared,
        item::{ItemStack, Stack, convert::item_name, window},
    },
    nbt::{Compound, Nbt},
    wire::{Reader, Writer},
};

/// Every distinct non-empty stack the `protocol` server put in the player's
/// inventory in the recorded session, in order.
pub(super) fn given_stacks(protocol: i32) -> Option<Vec<ItemStack>> {
    let packets = corpus(protocol)?;
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
    Some(stacks)
}

/// The private corpus of many more components (1.20.5 only), when captured.
fn component_corpus() -> Option<Vec<ItemStack>> {
    let home = std::env::var_os("HOME")?;
    let directory = std::path::Path::new(&home).join(".cache/rift-via/components/corpus");
    if !directory.join("766.jsonl").exists() {
        return None;
    }
    // SAFETY: tests that read corpora do not run concurrently with this
    // variable's other users; it is restored right after.
    let previous = std::env::var_os("RIFT_TRANSLATION_CORPUS");
    unsafe { std::env::set_var("RIFT_TRANSLATION_CORPUS", &directory) };
    let stacks = given_stacks(NEW);
    match previous {
        Some(previous) => unsafe { std::env::set_var("RIFT_TRANSLATION_CORPUS", previous) },
        None => unsafe { std::env::remove_var("RIFT_TRANSLATION_CORPUS") },
    }
    stacks
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

fn same_components(a: &ItemStack, b: &ItemStack) -> bool {
    a.id == b.id
        && a.count == b.count
        && same_patch(
            a.components.as_ref().unwrap_or(&Default::default()),
            b.components.as_ref().unwrap_or(&Default::default()),
        )
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

/// Stacks survive the way to the other protocol and back.
#[test]
fn recorded_stacks_round_trip_across_the_bridge() {
    let mut checked = 0;
    for stack in given_stacks(OLD).unwrap_or_default() {
        for (there, back) in [
            (
                up as fn(&ItemStack) -> Stack,
                down as fn(&ItemStack) -> Stack,
            ),
            (up_serverbound, down_serverbound),
        ] {
            let back = back(&there(&stack).unwrap()).unwrap();
            assert!(
                same_stack_tag(&back, &stack),
                "{stack:?}\ncame back as\n{back:?}"
            );
        }
        checked += 1;
    }
    let new = given_stacks(NEW).unwrap_or_default();
    for stack in new
        .iter()
        .chain(component_corpus().unwrap_or_default().iter())
    {
        for (there, back) in [
            (
                down as fn(&ItemStack) -> Stack,
                up as fn(&ItemStack) -> Stack,
            ),
            (down_serverbound, up_serverbound),
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
