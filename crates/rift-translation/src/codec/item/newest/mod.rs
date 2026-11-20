//! Stacks Rift makes itself, outside a connection: written once in the
//! newest protocol's terms and converted down to any client's, one step at
//! a time, by the same conversions backend stacks take.
use super::{ItemStack, Stack, components::Components, convert, write};
use crate::{
    Direction,
    codec::Shared,
    nbt::{Compound, Nbt},
    version::{self, NEWEST, PROTOCOLS},
    wire::{Writer, invalid},
};
use std::io;

/// The prefix of every key a conversion stashes in a tag or in
/// `custom_data` so that the stack can convert back.
const STASH_PREFIX: &str = "rift:";

/// The item ID of `name` in the newest protocol, with or without the
/// `minecraft:` namespace.
pub fn item_id(name: &str) -> Option<i32> {
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    if name.is_empty() || name.contains(':') {
        return None;
    }
    convert::item_id(NEWEST, name)
}

/// The name of item `id` of the newest protocol, without namespace.
pub fn item_name(id: i32) -> Option<&'static str> {
    convert::item_name(NEWEST, id)
}

/// Convert `stack`, in the newest protocol's terms, into `protocol`'s: the
/// stack a client on `protocol` would get from a backend on the newest
/// protocol, stand-ins for items it lacks included. What the conversions
/// stash for the way back is dropped, because such a stack never goes
/// back: the result is what the client shows, nothing more.
pub fn from_newest(protocol: i32, mut stack: Stack) -> io::Result<Stack> {
    let start = version::index(protocol)
        .ok_or_else(|| invalid(&format!("unsupported protocol {protocol}")))?;
    for pair in PROTOCOLS[start..].windows(2).rev() {
        let (client, backend) = (pair[0].0, pair[1].0);
        let mut shared = Shared::new(client, backend);
        convert(&mut shared, Direction::Clientbound, &mut stack)?;
        if let Some(item) = &mut stack {
            drop_stashes(item)?;
        }
    }
    Ok(stack)
}

/// [`from_newest`], written in `protocol`'s layout of one stack.
pub fn encode_from_newest(protocol: i32, stack: Stack) -> io::Result<Vec<u8>> {
    let converted = from_newest(protocol, stack)?;
    let mut writer = Writer::new();
    write(protocol, converted.as_ref(), &mut writer)?;
    Ok(writer.into_inner())
}

/// Remove the `rift:` entries from a stack's tag or `custom_data`.
fn drop_stashes(item: &mut ItemStack) -> io::Result<()> {
    if let Some(Nbt::Compound(tag)) = &mut item.tag {
        unstash(tag);
        if tag.is_empty() {
            item.tag = None;
        }
    }
    if let Some(components) = &mut item.components {
        drop_custom_stashes(components)?;
    }
    Ok(())
}

fn unstash(tag: &mut Compound) {
    tag.0.retain(|(key, _)| !key.starts_with(STASH_PREFIX));
}

fn drop_custom_stashes(components: &mut Components) -> io::Result<()> {
    use super::components::Value;
    let Some(value) = components.get("custom_data") else {
        return Ok(());
    };
    let Value::Nbt(bytes) = value else {
        return Err(invalid("custom_data is not NBT"));
    };
    if !bytes
        .windows(STASH_PREFIX.len())
        .any(|window| window == STASH_PREFIX.as_bytes())
    {
        return Ok(());
    }
    let Some(Nbt::Compound(mut data)) = value.as_nbt()? else {
        return Err(invalid("custom_data is not a compound"));
    };
    unstash(&mut data);
    if data.is_empty() {
        components.take("custom_data");
    } else {
        components.insert("custom_data", Value::nbt(&Nbt::Compound(data)));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
