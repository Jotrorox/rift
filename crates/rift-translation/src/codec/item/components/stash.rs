//! Components a receiver cannot express travel in the stack's `custom_data`
//! and come back when the stack is converted to their protocol again.
//!
//! A creative client sends whole stacks back to the server, and so does a
//! server echoing what a client put somewhere; without the stash, a
//! component dropped on the way to an older peer would be gone for good.
//! Each entry keeps the component exactly as the sender encoded it: the
//! protocol, the type name and the value bytes in that protocol's layout,
//! or a marker that the patch removed the type, or that it did not set a
//! type a conversion back would add. Restoring decodes the value
//! with that protocol's layout, so a stack that goes N to N-1 and back to N
//! comes back unchanged. Entries for other protocols stay, so the stash also
//! works across several steps.
use super::{
    Component, Components,
    convert::{Conv, Out},
    layouts::Layout,
    schema::{self, Value},
};
use crate::{
    nbt::{Compound, Nbt},
    wire::{Reader, Writer, invalid},
};
use std::io;

/// The `custom_data` key that holds the stash: a list of compounds.
pub const KEY: &str = "rift:lost_components";

/// A patch with the stash entries for the receiver taken out.
pub struct Restored {
    /// The patch without them.
    pub rest: Components,
    /// Components to set, already in the receiver's layout.
    pub components: Vec<Component>,
    /// Types the receiver's patch removes.
    pub removed: Vec<&'static str>,
    /// Types the receiver's patch neither sets nor removes, although
    /// converting it back did.
    pub absent: Vec<&'static str>,
}

/// Take the entries recorded for the receiver's protocol out of the
/// patch's `custom_data`.
pub fn take(cx: &mut Conv<'_>, mut input: Components) -> io::Result<Restored> {
    let mut restored = Restored {
        rest: Components::default(),
        components: Vec::new(),
        removed: Vec::new(),
        absent: Vec::new(),
    };
    let index = input
        .added
        .iter()
        .position(|component| component.kind == "custom_data");
    let Some(index) = index else {
        restored.rest = input;
        return Ok(restored);
    };
    let Value::Nbt(bytes) = &input.added[index].value else {
        return Err(invalid("custom_data is not NBT"));
    };
    // Most stacks carry no stash; skip parsing their custom data.
    if !contains(bytes, KEY.as_bytes()) {
        restored.rest = input;
        return Ok(restored);
    }
    let Some(Nbt::Compound(mut data)) = Nbt::from_bytes(bytes, false)? else {
        return Err(invalid("custom_data is not a compound"));
    };
    let layout = Layout::of(cx.target)?;
    if let Some(Nbt::List(entries)) = data.get_mut(KEY) {
        let mut kept = Vec::with_capacity(entries.len());
        for entry in std::mem::take(entries) {
            match restore(cx.target, layout, &entry)? {
                Some(Entry::Set(component)) => restored.components.push(component),
                Some(Entry::Removed(kind)) => restored.removed.push(kind),
                Some(Entry::Absent(kind)) => restored.absent.push(kind),
                None => kept.push(entry),
            }
        }
        *entries = kept;
        if entries.is_empty() {
            data.remove(KEY);
        }
    }
    if data.is_empty() {
        input.added.remove(index);
    } else {
        input.added[index].value = Value::nbt(&Nbt::Compound(data));
    }
    restored.rest = input;
    Ok(restored)
}

/// What one stash entry records.
pub enum Entry {
    /// The patch set this component.
    Set(Component),
    /// The patch removed this type.
    Removed(&'static str),
    /// The patch neither set nor removed this type.
    Absent(&'static str),
}

/// What an entry stashes for `protocol`, or `None` when the entry is for
/// another protocol.
fn restore(protocol: i32, layout: &Layout, entry: &Nbt) -> io::Result<Option<Entry>> {
    let entry = entry
        .as_compound()
        .ok_or_else(|| invalid("invalid stashed component"))?;
    if entry.int("protocol") != Some(protocol) {
        return Ok(None);
    }
    let kind = entry
        .str("type")
        .ok_or_else(|| invalid("invalid stashed component"))?;
    let Some((id, node)) = layout.find(kind) else {
        return Ok(None);
    };
    let (kind, _) = layout.get(id).expect("found by name");
    if entry.contains("removed") {
        return Ok(Some(Entry::Removed(kind)));
    }
    if entry.contains("absent") {
        return Ok(Some(Entry::Absent(kind)));
    }
    let Some(Nbt::ByteArray(bytes)) = entry.get("value") else {
        return Err(invalid("invalid stashed component"));
    };
    let mut reader = Reader::new(bytes);
    let value = schema::read(protocol, node, &mut reader, 1)?;
    reader.finish()?;
    Ok(Some(Entry::Set(Component { kind, value })))
}

/// Record what the conversion lost in the converted patch's `custom_data`.
pub fn put(cx: &mut Conv<'_>, out: &mut Out) -> io::Result<()> {
    if out.lost.is_empty() && out.lost_removed.is_empty() && out.absent.is_empty() {
        return Ok(());
    }
    let layout = Layout::of(cx.source)?;
    let mut entries = Vec::with_capacity(out.lost.len() + out.lost_removed.len());
    for component in std::mem::take(&mut out.lost) {
        let (_, node) = layout
            .find(component.kind)
            .ok_or_else(|| invalid("lost component has no layout"))?;
        let mut writer = Writer::new();
        schema::write(cx.source, node, &component.value, &mut writer)?;
        entries.push(entry(cx.source, component.kind, |entry| {
            entry.insert("value", Nbt::ByteArray(writer.buf));
        }));
    }
    for kind in std::mem::take(&mut out.lost_removed) {
        entries.push(entry(cx.source, kind, |entry| {
            entry.insert("removed", Nbt::Byte(1));
        }));
    }
    for kind in std::mem::take(&mut out.absent) {
        entries.push(entry(cx.source, kind, |entry| {
            entry.insert("absent", Nbt::Byte(1));
        }));
    }
    let mut data = match out.components.get("custom_data") {
        Some(value) => match value.as_nbt()? {
            Some(Nbt::Compound(data)) => data,
            _ => return Err(invalid("custom_data is not a compound")),
        },
        None => Compound::new(),
    };
    match data.get_mut(KEY) {
        Some(Nbt::List(list)) if list.iter().all(|entry| entry.kind() == 10) => {
            list.extend(entries)
        }
        _ => {
            data.insert(KEY, Nbt::List(entries));
        }
    }
    out.components
        .insert("custom_data", Value::nbt(&Nbt::Compound(data)));
    Ok(())
}

/// An entry recording that a patch of `protocol` set `component`, for the
/// item bridge, which stashes components in 1.20.4 tags.
pub fn entry_set(protocol: i32, component: &Component) -> io::Result<Nbt> {
    let (_, node) = Layout::of(protocol)?
        .find(component.kind)
        .ok_or_else(|| invalid("stashed component has no layout"))?;
    let mut writer = Writer::new();
    schema::write(protocol, node, &component.value, &mut writer)?;
    Ok(entry(protocol, component.kind, |entry| {
        entry.insert("value", Nbt::ByteArray(writer.buf));
    }))
}

/// An entry recording that a patch of `protocol` removed (`"removed"`) or
/// did not mention (`"absent"`) the type `kind`.
pub fn entry_marker(protocol: i32, kind: &str, marker: &str) -> Nbt {
    entry(protocol, kind, |entry| {
        entry.insert(marker, Nbt::Byte(1));
    })
}

/// What an entry stashes for `protocol`; `None` for another protocol's.
pub fn restore_entry(protocol: i32, layout: &Layout, entry: &Nbt) -> io::Result<Option<Entry>> {
    restore(protocol, layout, entry)
}

fn entry(protocol: i32, kind: &str, fill: impl FnOnce(&mut Compound)) -> Nbt {
    let mut entry = Compound::new();
    entry.insert("protocol", Nbt::Int(protocol));
    entry.insert("type", Nbt::String(kind.to_owned()));
    fill(&mut entry);
    Nbt::Compound(entry)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
