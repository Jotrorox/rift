//! Between 1.20.4 (NBT tags) and 1.20.5 (data components).
//!
//! A 1.20.4 stack carries an NBT tag; a 1.20.5 stack a patch of data
//! components over its item's defaults. [`up`] turns a tag into a patch as
//! vanilla's own `ItemStackComponentizationFix` describes it (for
//! protocol 766's network layouts), and [`down`] is its inverse. Tag keys
//! with no component go to `custom_data` and come back out of it, so the
//! tag of a 1.20.4 stack survives the trip to a 1.20.5 client and back.
//!
//! The way down cannot express everything (food, tools, rarity, removed
//! defaults...). After converting a patch to a tag, the tag is converted
//! back up and whatever does not come out as it went in is stashed in the
//! tag under `rift:766`, entry by entry in the component stash's format
//! (`components::stash`), and restored on the way up. Stand-ins for items
//! the receiver lacks are named after the original and the original's ID is
//! stashed the same way (`rift:766` in tags, `rift:765` in `custom_data`).
mod down;
mod persistent;
mod tables;
mod up;

use super::{
    ItemStack, Stack,
    components::{
        Component, Components,
        schema::Value,
        stash::{self, Entry},
    },
    convert::{item_id, item_name, map_item, name_stand_in, readable, stash_key, unname_stand_in},
};
use crate::{
    Direction,
    codec::{
        Shared,
        registries::{self, Side},
        text,
    },
    data,
    nbt::{Compound, Nbt},
    version,
    wire::{MAX_DEPTH, invalid},
};
use std::io;

/// The last protocol with NBT item tags and the first with components.
const OLD: i32 = 765;
const NEW: i32 = 766;

pub(super) fn convert(
    shared: &mut Shared,
    direction: Direction,
    stack: &mut Stack,
) -> io::Result<()> {
    let Some(item) = stack.take() else {
        return Ok(());
    };
    let (source, target) = (shared.source(direction), shared.target(direction));
    if (source, target) != (OLD, NEW) && (source, target) != (NEW, OLD) {
        return Err(invalid("the item bridge only joins 1.20.4 and 1.20.5"));
    }
    let mut bridge = Bridge::new(shared, direction);
    *stack = if version::item_components(target) {
        bridge.up(item)?
    } else {
        bridge.down(item)?
    };
    Ok(())
}

/// Convert the stack of a `show_item` hover event across the bridge: the
/// 1.20.4 tag and 1.20.5 persistent components (see `persistent`).
/// `Ok(false)` when the item does not exist on the sender's side.
pub(super) fn hover(
    shared: &mut Shared,
    direction: Direction,
    id: &mut String,
    count: &mut i32,
    tag: &mut Option<Compound>,
    components: &mut Option<Compound>,
) -> io::Result<bool> {
    let upward = shared.target(direction) == NEW;
    let mut bridge = Bridge::new(shared, direction);
    let converted = persistent::hover(&mut bridge, upward, id, count, tag, components)?;
    // Nothing comes back from a hover: no stash. And a 1.20.4 hover holds
    // the tag as given, without the `Damage` stacks always carry.
    if let Some(tag) = tag {
        tag.remove(&stash_key(NEW));
        if tag.get("Damage").and_then(Nbt::as_i64) == Some(0) {
            tag.remove("Damage");
        }
    }
    Ok(converted)
}

/// One conversion across the bridge, in either direction.
pub(super) struct Bridge<'a> {
    shared: &'a mut Shared,
    /// The direction stacks travel from 1.20.4 to 1.20.5 in.
    upward: Direction,
    depth: usize,
}

fn opposite(direction: Direction) -> Direction {
    match direction {
        Direction::Clientbound => Direction::Serverbound,
        Direction::Serverbound => Direction::Clientbound,
    }
}

impl<'a> Bridge<'a> {
    fn new(shared: &'a mut Shared, direction: Direction) -> Self {
        let upward = if shared.target(direction) == NEW {
            direction
        } else {
            opposite(direction)
        };
        Bridge {
            shared,
            upward,
            depth: 0,
        }
    }

    fn downward(&self) -> Direction {
        opposite(self.upward)
    }

    /// The side of the step that speaks 1.20.5.
    fn new_side(&self) -> Side {
        Side::receiver(self.upward)
    }

    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        if self.depth >= MAX_DEPTH {
            return Err(invalid("item stack nesting limit exceeded"));
        }
        self.depth += 1;
        let result = f(self);
        self.depth -= 1;
        result
    }

    /// The 1.20.5 ID of the 1.20.4 entry `name` of a built-in registry,
    /// through the step's mapping (renames, stand-ins).
    fn new_id(&mut self, registry: &'static str, name: &str) -> Option<i32> {
        let name = data::strip(name);
        if let Some(synced) = self.synced_id(registry, name) {
            return synced;
        }
        match data::version(OLD)
            .registry(registry)
            .and_then(|r| r.id(name))
        {
            Some(id) => self.shared.mapping(self.upward, registry).get(id),
            None => data::version(NEW).registry(registry)?.id(name),
        }
    }

    /// The 1.20.4 name (namespaced) of the 1.20.5 entry `id`.
    fn old_name(&mut self, registry: &'static str, id: i32) -> Option<String> {
        if !self.builtin(registry) {
            let name = registries::name_of(self.shared, self.new_side(), registry, id)?;
            return Some(format!("minecraft:{name}"));
        }
        let mapped = self.shared.mapping(self.downward(), registry).get(id)?;
        let name = data::version(OLD).registry(registry)?.name(mapped)?;
        Some(format!("minecraft:{name}"))
    }

    /// The 1.20.5 name (without namespace) of the 1.20.5 entry `id`.
    fn new_name(&mut self, registry: &'static str, id: i32) -> Option<String> {
        if self.builtin(registry) {
            return data::version(NEW)
                .registry(registry)?
                .name(id)
                .map(str::to_owned);
        }
        registries::name_of(self.shared, self.new_side(), registry, id).map(str::to_owned)
    }

    /// The 1.20.5 ID of the 1.20.5 entry `name`.
    fn new_id_by_name(&mut self, registry: &'static str, name: &str) -> Option<i32> {
        let name = data::strip(name);
        if self.builtin(registry) {
            return data::version(NEW).registry(registry)?.id(name);
        }
        registries::id_of(self.shared, self.new_side(), registry, name)
    }

    fn builtin(&self, registry: &str) -> bool {
        data::version(NEW).registry(registry).is_some()
    }

    /// For registries 1.20.5 synchronises: the ID the 1.20.5 side gave
    /// `name` (`Some(None)` when it has no such entry).
    fn synced_id(&mut self, registry: &'static str, name: &str) -> Option<Option<i32>> {
        (!self.builtin(registry))
            .then(|| registries::id_of(self.shared, self.new_side(), registry, name))
    }

    /// The prototype attribute modifiers of a 1.20.5 item, as values of the
    /// `attribute_modifiers` list.
    fn default_modifiers(&mut self, item: &str) -> Vec<Value> {
        let mut modifiers = Vec::new();
        for row in tables::default_modifiers(item) {
            let Some(
                [
                    serde_json::Value::String(attribute),
                    serde_json::Value::Array(uuid),
                    serde_json::Value::String(name),
                    amount,
                    operation,
                    serde_json::Value::String(slot),
                ],
            ) = row.as_array().map(Vec::as_slice)
            else {
                continue;
            };
            let ints: Vec<i32> = uuid
                .iter()
                .filter_map(|int| Some(int.as_i64()? as i32))
                .collect();
            let (Some(id), Some(slot), [_, _, _, _]) = (
                data::version(NEW)
                    .registry("attribute")
                    .and_then(|r| r.id(attribute)),
                tables::MODIFIER_SLOTS
                    .iter()
                    .chain(&["any"])
                    .find(|known| **known == slot.as_str())
                    .copied(),
                ints.as_slice(),
            ) else {
                continue;
            };
            modifiers.push(Value::Struct(vec![
                Value::Id(id),
                Value::Uuid(text::uuid_from_ints(&ints)),
                Value::String(name.clone()),
                Value::Double(amount.as_f64().unwrap_or(0.0)),
                Value::Int(operation.as_i64().unwrap_or(0) as i32),
                Value::Enum(slot),
            ]));
        }
        modifiers
    }

    /// A 1.20.4 JSON text component as 1.20.5 network NBT.
    fn json_to_text(&mut self, json: &str) -> io::Result<Vec<u8>> {
        let mut component = text::from_json_str(OLD, json)?;
        text::adapt(self.shared, self.upward, &mut component);
        Ok(text::to_nbt(NEW, &component).to_bytes(false))
    }

    /// A text component as 1.20.5 network NBT, from the 1.20.4 model.
    fn component_to_text(&mut self, mut component: text::Component) -> Vec<u8> {
        text::adapt(self.shared, self.upward, &mut component);
        text::to_nbt(NEW, &component).to_bytes(false)
    }

    /// The reverse of [`Self::json_to_text`].
    fn text_to_json(&mut self, value: &Value) -> io::Result<String> {
        Ok(text::to_json_string(OLD, &self.text_component(value)?))
    }

    /// A 1.20.5 network text component in the text model, converted for
    /// 1.20.4.
    fn text_component(&mut self, value: &Value) -> io::Result<text::Component> {
        let tag = value.as_nbt()?.ok_or_else(|| invalid("missing text"))?;
        let mut component = text::from_nbt(NEW, &tag)?;
        text::adapt(self.shared, self.downward(), &mut component);
        Ok(component)
    }

    /// A 1.20.4 stack as 1.20.5 has it.
    fn up(&mut self, item: ItemStack) -> io::Result<Stack> {
        if item.count <= 0 {
            return Ok(None);
        }
        let mut tag = match item.tag {
            None => Compound::new(),
            Some(Nbt::Compound(tag)) => tag,
            Some(_) => return Err(invalid("item tag is not a compound")),
        };
        let stash = match tag.remove(&stash_key(NEW)) {
            Some(Nbt::Compound(stash)) => Some(stash),
            Some(other) => {
                tag.insert(stash_key(NEW), other);
                None
            }
            None => None,
        };
        let restored = stash
            .as_ref()
            .and_then(|stash| stash.str("id"))
            .and_then(|name| item_id(NEW, data::strip(name)));
        let (id, stand_in_for) = match restored {
            Some(id) => (id, None),
            None => {
                let mapped = map_item(self.shared, self.upward, item.id)?;
                (mapped.id, mapped.stand_in_for)
            }
        };
        if restored.is_some() && stash.as_ref().and_then(|s| s.int("named")) == Some(1) {
            unname_stand_in(&mut tag);
        }
        let name = item_name(NEW, id).ok_or_else(|| invalid("unknown item"))?;
        let mut components = up::components(self, name, tag)?;
        if let Some(Nbt::List(entries)) = stash.as_ref().and_then(|stash| stash.get("components")) {
            restore(&mut components, entries)?;
        }
        if let Some(original) = stand_in_for {
            let mut stash = Compound::new();
            stash.insert("id", format!("minecraft:{original}"));
            let named = components.get("item_name").is_none();
            if named {
                let name = self.component_to_text(text::Component::text(readable(original)));
                components.insert("item_name", Value::Nbt(name));
            }
            if named {
                stash.insert("named", Nbt::Byte(1));
            }
            put_custom_data(&mut components, &stash_key(OLD), Nbt::Compound(stash))?;
        }
        Ok(Some(ItemStack {
            id,
            count: item.count,
            damage: 0,
            tag: None,
            components: Some(components),
        }))
    }

    /// A 1.20.5 stack as 1.20.4 has it.
    fn down(&mut self, item: ItemStack) -> io::Result<Stack> {
        if item.count <= 0 {
            return Ok(None);
        }
        let mut components = item.components.unwrap_or_default();
        let name = item_name(NEW, item.id).ok_or_else(|| invalid("unknown item"))?;
        let stash = take_custom_data(&mut components, &stash_key(OLD))?;
        let restored = stash
            .as_ref()
            .and_then(|stash| stash.str("id"))
            .and_then(|name| item_id(OLD, data::strip(name)));
        let (id, stand_in_for) = match restored {
            Some(id) => (id, None),
            None => {
                let mapped = map_item(self.shared, self.downward(), item.id)?;
                (mapped.id, mapped.stand_in_for)
            }
        };
        if restored.is_some() && stash.as_ref().and_then(|s| s.int("named")) == Some(1) {
            components.take("item_name");
        }
        let mut tag = down::tag(self, name, &components)?;
        let lost = self.lost(name, &components, &tag)?;
        if !lost.is_empty() || stand_in_for.is_some() {
            let mut stash = Compound::new();
            if let Some(original) = stand_in_for {
                if name_stand_in(&mut tag, OLD, original) {
                    stash.insert("named", Nbt::Byte(1));
                }
                stash.insert("id", format!("minecraft:{original}"));
            }
            if !lost.is_empty() {
                stash.insert("components", Nbt::List(lost));
            }
            tag.insert(stash_key(NEW), stash);
        }
        Ok(Some(ItemStack {
            id,
            count: item.count.min(i32::from(i8::MAX)),
            damage: 0,
            tag: (!tag.is_empty()).then_some(Nbt::Compound(tag)),
            components: None,
        }))
    }

    /// Stash entries for what converting `tag` back up would not restore
    /// of `components`: components it changes or loses, removals it drops,
    /// and components it adds.
    fn lost(
        &mut self,
        item: &str,
        components: &Components,
        tag: &Compound,
    ) -> io::Result<Vec<Nbt>> {
        if components.is_empty() {
            return Ok(Vec::new());
        }
        let back = up::components(self, item, tag.clone())?;
        let mut lost = Vec::new();
        for component in &components.added {
            if !back
                .get(component.kind)
                .is_some_and(|value| same(value, &component.value))
            {
                lost.push(stash::entry_set(NEW, component)?);
            }
        }
        for kind in &components.removed {
            if !back.removes(kind) {
                lost.push(stash::entry_marker(NEW, kind, "removed"));
            }
        }
        for kind in back
            .added
            .iter()
            .map(|component| component.kind)
            .chain(back.removed.iter().copied())
        {
            if components.get(kind).is_none() && !components.removes(kind) {
                lost.push(stash::entry_marker(NEW, kind, "absent"));
            }
        }
        Ok(lost)
    }

    /// A stack held in 1.20.4 NBT (`{id, Count, tag}`) as a 1.20.5 stack;
    /// `None` when it is empty or names no item 1.20.4 has.
    fn nbt_stack_up(&mut self, nbt: &Compound) -> io::Result<Option<ItemStack>> {
        let Some(id) = nbt
            .str("id")
            .and_then(|name| item_id(OLD, data::strip(name)))
        else {
            return Ok(None);
        };
        if item_name(OLD, id) == Some("air") {
            return Ok(None);
        }
        let item = ItemStack {
            id,
            count: nbt.int("Count").unwrap_or(1),
            damage: 0,
            tag: nbt.get("tag").filter(|tag| tag.kind() == 10).cloned(),
            components: None,
        };
        self.nested(|bridge| bridge.up(item))
    }

    /// The reverse of [`Self::nbt_stack_up`].
    fn nbt_stack_down(&mut self, stack: &ItemStack) -> io::Result<Option<Compound>> {
        let Some(item) = self.nested(|bridge| bridge.down(stack.clone()))? else {
            return Ok(None);
        };
        let mut nbt = Compound::new();
        let name = item_name(OLD, item.id).ok_or_else(|| invalid("unknown item"))?;
        nbt.insert("id", format!("minecraft:{name}"));
        nbt.insert("Count", item.count as i8);
        if let Some(tag) = item.tag {
            nbt.insert("tag", tag);
        }
        Ok(Some(nbt))
    }
}

/// Apply stash entries recorded for 1.20.5 to a patch.
fn restore(components: &mut Components, entries: &[Nbt]) -> io::Result<()> {
    let layout = super::components::layouts::Layout::of(NEW)?;
    for entry in entries {
        match stash::restore_entry(NEW, layout, entry)? {
            Some(Entry::Set(Component { kind, value })) => components.insert(kind, value),
            Some(Entry::Removed(kind)) => {
                components.take(kind);
                if !components.removes(kind) {
                    components.removed.push(kind);
                }
            }
            Some(Entry::Absent(kind)) => {
                components.take(kind);
                components.removed.retain(|known| *known != kind);
            }
            None => {}
        }
    }
    Ok(())
}

/// Remove `key` from the patch's `custom_data`, dropping the component
/// when nothing else is left in it.
pub(super) fn take_custom_data(
    components: &mut Components,
    key: &str,
) -> io::Result<Option<Compound>> {
    let Some(value) = components.get("custom_data") else {
        return Ok(None);
    };
    let Value::Nbt(bytes) = value else {
        return Err(invalid("custom_data is not NBT"));
    };
    if !bytes
        .windows(key.len())
        .any(|window| window == key.as_bytes())
    {
        return Ok(None);
    }
    let Some(Nbt::Compound(mut data)) = value.as_nbt()? else {
        return Err(invalid("custom_data is not a compound"));
    };
    let taken = match data.remove(key) {
        Some(Nbt::Compound(taken)) => taken,
        Some(other) => {
            data.insert(key, other);
            return Ok(None);
        }
        None => return Ok(None),
    };
    if data.is_empty() {
        components.take("custom_data");
    } else {
        components.insert("custom_data", Value::nbt(&Nbt::Compound(data)));
    }
    Ok(Some(taken))
}

pub(super) fn put_custom_data(
    components: &mut Components,
    key: &str,
    value: Nbt,
) -> io::Result<()> {
    let mut data = match components
        .get("custom_data")
        .map(Value::as_nbt)
        .transpose()?
    {
        Some(Some(Nbt::Compound(data))) => data,
        Some(_) => return Err(invalid("custom_data is not a compound")),
        None => Compound::new(),
    };
    data.insert(key, value);
    components.insert("custom_data", Value::nbt(&Nbt::Compound(data)));
    Ok(())
}

/// Whether two component values mean the same: NBT compares regardless of
/// compound key order.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Nbt(x), Value::Nbt(y)) => {
            x == y
                || matches!(
                    (Nbt::from_bytes(x, false), Nbt::from_bytes(y, false)),
                    (Ok(Some(x)), Ok(Some(y))) if same_nbt(&x, &y)
                )
        }
        (Value::Optional(x), Value::Optional(y)) => match (x, y) {
            (Some(x), Some(y)) => same(x, y),
            (None, None) => true,
            _ => false,
        },
        (Value::List(x), Value::List(y)) | (Value::Struct(x), Value::Struct(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Value::Direct(x), Value::Direct(y)) => same(x, y),
        (Value::Either(l, x), Value::Either(r, y)) => l == r && same(x, y),
        (Value::Variant(l, x), Value::Variant(r, y)) => l == r && same(x, y),
        (Value::Stack(Some(x)), Value::Stack(Some(y))) => {
            x.id == y.id
                && x.count == y.count
                && same_patch(
                    x.components.as_ref().unwrap_or(&Components::default()),
                    y.components.as_ref().unwrap_or(&Components::default()),
                )
        }
        _ => a == b,
    }
}

/// Whether two patches set and remove the same, in any order.
fn same_patch(a: &Components, b: &Components) -> bool {
    a.added.len() == b.added.len()
        && a.removed.len() == b.removed.len()
        && a.added.iter().all(|component| {
            b.get(component.kind)
                .is_some_and(|value| same(value, &component.value))
        })
        && a.removed.iter().all(|kind| b.removes(kind))
}

/// NBT equality regardless of compound key order.
pub(super) fn same_nbt(a: &Nbt, b: &Nbt) -> bool {
    match (a, b) {
        (Nbt::Compound(x), Nbt::Compound(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, value)| y.get(key).is_some_and(|other| same_nbt(value, other)))
        }
        (Nbt::List(x), Nbt::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_nbt(x, y))
        }
        _ => a == b,
    }
}

#[cfg(test)]
mod tests;
