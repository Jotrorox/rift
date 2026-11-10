//! Converting one stack between the two adjacent protocols of a step.
use super::{ItemStack, Stack, bridge, components, flatten, tags};
use crate::{
    Direction,
    codec::{Shared, text},
    data,
    mapping::Kind,
    nbt::{Compound, Nbt},
    version,
    wire::invalid,
};
use std::io;

pub(super) fn stack(
    shared: &mut Shared,
    direction: Direction,
    stack: &mut Stack,
) -> io::Result<()> {
    if stack.is_none() {
        return Ok(());
    }
    let (source, target) = (shared.source(direction), shared.target(direction));
    match (
        version::item_components(source),
        version::item_components(target),
    ) {
        (true, true) => components_era(shared, direction, stack),
        (false, false) => {
            let item = stack.as_mut().expect("checked above");
            match (version::flattened(source), version::flattened(target)) {
                (false, true) => flatten::up(shared, direction, item)?,
                (true, false) => flatten::down(shared, direction, item)?,
                _ => tag_era(shared, direction, item)?,
            }
            // A stack that became air is the empty stack.
            if version::flattened(target)
                && data::version(target)
                    .registry("item")
                    .and_then(|r| r.name(item.id))
                    == Some("air")
            {
                *stack = None;
            }
            Ok(())
        }
        _ => bridge::convert(shared, direction, stack),
    }
}

/// The key under which a conversion out of `protocol` stashes what the
/// receiver cannot hold. The conversion back into `protocol` restores it.
pub(super) fn stash_key(protocol: i32) -> String {
    format!("rift:{protocol}")
}

/// An item ID in the receiver's terms, and the sender's item name when the
/// receiver lacks the item and got a stand-in.
pub(super) struct Mapped {
    pub id: i32,
    pub stand_in_for: Option<&'static str>,
}

/// The name of item `id` in `protocol`, without namespace.
pub(super) fn item_name(protocol: i32, id: i32) -> Option<&'static str> {
    data::version(protocol).registry("item")?.name(id)
}

pub(super) fn item_id(protocol: i32, name: &str) -> Option<i32> {
    data::version(protocol).registry("item")?.id(name)
}

/// Map an item ID through the step's item mapping. Without a counterpart
/// or stand-in the receiver gets stone, which every protocol has.
pub(super) fn map_item(shared: &mut Shared, direction: Direction, id: i32) -> io::Result<Mapped> {
    let source = shared.source(direction);
    let target = shared.target(direction);
    let name = item_name(source, id).ok_or_else(|| invalid(&format!("unknown item ID {id}")))?;
    let mapping = shared.mapping(direction, "item");
    match mapping.get(id) {
        Some(mapped) => Ok(Mapped {
            id: mapped,
            stand_in_for: (mapping.kind(id) == Some(Kind::StandIn)).then_some(name),
        }),
        None => Ok(Mapped {
            id: item_id(target, "stone").ok_or_else(|| invalid("no stone to stand in"))?,
            stand_in_for: Some(name),
        }),
    }
}

/// "cherry_planks" as "Cherry Planks", for stand-in names.
pub(super) fn readable(name: &str) -> String {
    data::strip(name)
        .split(['_', '/'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Give a stand-in the original item's name, unless it has a custom name.
/// Returns whether a name was added.
pub(super) fn name_stand_in(tag: &mut Compound, protocol: i32, original: &str) -> bool {
    let display = match tag.get_mut("display") {
        Some(Nbt::Compound(display)) => display,
        Some(_) => return false,
        None => {
            tag.insert("display", Compound::new());
            tag.compound_mut("display").expect("just inserted")
        }
    };
    if display.contains("Name") {
        return false;
    }
    let name = readable(original);
    let name = if version::flattened(protocol) {
        serde_json::json!({"text": name, "italic": false}).to_string()
    } else {
        name
    };
    display.insert("Name", name);
    true
}

/// Remove a name [`name_stand_in`] added.
pub(super) fn unname_stand_in(tag: &mut Compound) {
    if let Some(display) = tag.compound_mut("display") {
        display.remove("Name");
        if display.is_empty() {
            tag.remove("display");
        }
    }
}

/// The stack's tag as a compound, created when absent. A tag that is not a
/// compound (never sent by vanilla) is left alone: `None`.
pub(super) fn tag_compound(item: &mut ItemStack) -> Option<&mut Compound> {
    if item.tag.is_none() {
        item.tag = Some(Nbt::Compound(Compound::new()));
    }
    item.tag.as_mut()?.as_compound_mut()
}

/// Drop an empty tag again, as vanilla sends tagless stacks.
pub(super) fn tidy(item: &mut ItemStack) {
    if matches!(&item.tag, Some(Nbt::Compound(tag)) if tag.is_empty()) {
        item.tag = None;
    }
}

/// Both protocols keep an NBT tag, both flattened or both not.
fn tag_era(shared: &mut Shared, direction: Direction, item: &mut ItemStack) -> io::Result<()> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let restore = item
        .tag
        .as_mut()
        .and_then(Nbt::as_compound_mut)
        .and_then(|tag| tag.remove(&stash_key(target)))
        .and_then(|stash| match stash {
            Nbt::Compound(stash) => Some(stash),
            _ => None,
        });
    if let Some(stash) = &restore
        && let Some(original) = stash.str("id").and_then(|name| item_id(target, name))
    {
        // The stack was a stand-in for an item the receiver has: restore it.
        item.id = original;
        if let Some(Nbt::Compound(tag)) = stash.get("tag") {
            item.tag = Some(Nbt::Compound(tag.clone()));
        } else if let Some(tag) = item.tag.as_mut().and_then(Nbt::as_compound_mut)
            && stash.int("named") == Some(1)
        {
            unname_stand_in(tag);
        }
        tidy(item);
        return Ok(());
    }
    let source_name = item_name(source, item.id).unwrap_or("stone");
    let mapped = map_item(shared, direction, item.id)?;
    item.id = mapped.id;
    let original = item.tag.clone();
    let name = item_name(target, item.id).unwrap_or("stone");
    let mut lossy = false;
    if let Some(tag) = item.tag.as_mut().and_then(Nbt::as_compound_mut) {
        lossy = tags::convert(shared, direction, name, tag)?;
    }
    if (lossy || mapped.stand_in_for.is_some())
        && let Some(tag) = tag_compound(item)
    {
        let named = match mapped.stand_in_for {
            Some(stand_in_for) => name_stand_in(tag, target, stand_in_for),
            None => false,
        };
        let mut stash = Compound::new();
        stash.insert("id", format!("minecraft:{source_name}"));
        stash.insert("named", i8::from(named));
        if let Some(original) = original {
            stash.insert("tag", original);
        }
        tag.insert(stash_key(source), stash);
    }
    tidy(item);
    Ok(())
}

/// Both protocols have data components: map the item and let the component
/// codec convert the patch. A stand-in gets the original's name as
/// `item_name` and the original stashed in `custom_data` under
/// `rift:<protocol>`, so it is restored when the stack comes back.
fn components_era(shared: &mut Shared, direction: Direction, stack: &mut Stack) -> io::Result<()> {
    let (source, target) = (shared.source(direction), shared.target(direction));
    let item = stack.as_mut().expect("caller checked");
    let components = item.components.get_or_insert_with(Default::default);
    let restore = bridge::take_custom_data(components, &stash_key(target))?;
    if let Some(stash) = &restore
        && let Some(original) = stash
            .str("id")
            .and_then(|name| item_id(target, data::strip(name)))
    {
        // Back to the protocol the stand-in was made for: the patch is the
        // original's, converted on the way out and back.
        let named = stash.int("named") == Some(1);
        components::convert(shared, direction, original, components)?;
        if named {
            components.take("item_name");
        }
        item.id = original;
        return Ok(());
    }
    let mapped = map_item(shared, direction, item.id)?;
    item.id = mapped.id;
    components::convert(shared, direction, mapped.id, components)?;
    if let Some(original) = mapped.stand_in_for {
        let mut stash = Compound::new();
        stash.insert("id", format!("minecraft:{original}"));
        if components.get("item_name").is_none() {
            let name = text::to_nbt(target, &text::Component::text(readable(original)));
            components.insert("item_name", components::Value::Nbt(name.to_bytes(false)));
            stash.insert("named", Nbt::Byte(1));
        }
        bridge::put_custom_data(components, &stash_key(source), Nbt::Compound(stash))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_stand_ins_are_named_and_restored() {
        // 1.21 added this disc; a 1.20.6 client gets a stand-in.
        let original = item_id(767, "music_disc_creator").unwrap();
        let mut shared = Shared::new(766, 767);
        let mut converted = Some(ItemStack {
            id: original,
            count: 1,
            damage: 0,
            tag: None,
            components: Some(Default::default()),
        });
        stack(&mut shared, Direction::Clientbound, &mut converted).unwrap();
        let there = converted.clone().unwrap();
        assert_ne!(item_name(766, there.id), Some("music_disc_creator"));
        let patch = there.components.as_ref().unwrap();
        assert!(patch.get("item_name").is_some());
        assert!(patch.get("custom_data").is_some());
        // The creative client returns it: the backend gets its disc back.
        stack(&mut shared, Direction::Serverbound, &mut converted).unwrap();
        let back = converted.unwrap();
        assert_eq!(back.id, original);
        assert!(back.components.unwrap().is_empty());
    }

    #[test]
    fn readable_names_are_title_case() {
        assert_eq!(readable("minecraft:cherry_planks"), "Cherry Planks");
        assert_eq!(readable("trident"), "Trident");
    }

    #[test]
    fn stand_in_names_respect_custom_names_and_protocol_format() {
        let mut tag = Compound::new();
        assert!(name_stand_in(&mut tag, 393, "trident"));
        assert_eq!(
            tag.compound("display").unwrap().str("Name"),
            Some(r#"{"italic":false,"text":"Trident"}"#)
        );
        assert!(!name_stand_in(&mut tag, 393, "other"));
        unname_stand_in(&mut tag);
        assert!(tag.is_empty());
        assert!(name_stand_in(&mut tag, 340, "trident"));
        assert_eq!(
            tag.compound("display").unwrap().str("Name"),
            Some("Trident")
        );
    }
}
