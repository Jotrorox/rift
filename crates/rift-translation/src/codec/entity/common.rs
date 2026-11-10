//! What the entity codec's converters share: registry and block state
//! mappings that tolerate registries a protocol lacks, which pairs change
//! the structures other codecs own, and small wire conversions.
use crate::{
    Direction,
    codec::{
        Shared,
        registries::{self, Side},
    },
    data,
    mapping::Mapping,
    version,
    wire::{Position, Reader, Writer, invalid},
};
use std::io;

/// The mapping of a built-in registry for packets travelling in
/// `direction`, or `None` when either protocol lacks the registry (it is
/// data-driven there, or does not exist yet).
pub fn registry(
    shared: &mut Shared,
    direction: Direction,
    name: &'static str,
) -> Option<&'static Mapping> {
    let (from, to) = (shared.source(direction), shared.target(direction));
    if data::version(from).registry(name).is_none() || data::version(to).registry(name).is_none() {
        return None;
    }
    Some(shared.mapping(direction, name))
}

/// The receiver's ID for the sender's `id` in `registry`: through the
/// built-in mapping when both protocols build the registry in, else by
/// entry name through what the registries codec saw on each side (one or
/// both synchronise it). `None` when the receiver has no such entry; IDs of
/// registries neither knows pass unchanged.
pub fn registry_id(
    shared: &mut Shared,
    direction: Direction,
    name: &'static str,
    id: i32,
) -> Option<i32> {
    if let Some(mapping) = registry(shared, direction, name) {
        return mapping.get(id);
    }
    if registries::size(shared, Side::sender(direction), name) == 0 {
        return Some(id);
    }
    registries::map(shared, direction, name, id)
}

/// Whether the two protocols agree on whether `name` is built in, so that
/// an unchanged built-in registry (or one synchronised on both sides, where
/// the client side keeps the backend's numbering) keeps every ID.
pub fn registry_unchanged(from: i32, to: i32, name: &str) -> bool {
    data::version(from).registry(name).is_some() == data::version(to).registry(name).is_some()
        && registry_identity(from, to, name)
}

/// Whether a built-in registry keeps every ID between two protocols (true
/// when either lacks it: then there is nothing to map).
pub fn registry_identity(from: i32, to: i32, name: &str) -> bool {
    if from == to
        || data::version(from).registry(name).is_none()
        || data::version(to).registry(name).is_none()
    {
        return true;
    }
    // Sparse pre-1.13 registries map their gaps to -1, which is no
    // identity mapping although no ID changes.
    let (a, b) = (
        data::version(from).expect_registry(name),
        data::version(to).expect_registry(name),
    );
    (a.len() == b.len() && a.names().eq(b.names()))
        || crate::mapping::mapping(from, to, name).is_identity()
}

/// Block state IDs between two flattened protocols, by block name and
/// property values with the stand-in rules of the mapping engine. `None`
/// when either protocol is older than 1.13.
// INTEGRATION: map pre-1.13 numeric block IDs through the data engineer's
// `legacy` module.
pub fn block_states(from: i32, to: i32) -> Option<&'static Mapping> {
    if !version::flattened(from) || !version::flattened(to) {
        return None;
    }
    crate::mapping::try_mapping(from, to, "block_state")
}

/// Convert a block state ID for packets travelling in `direction`. Unknown
/// states become air; pre-1.13 IDs pass through untouched (see
/// [`block_states`]). Across the flattening, the 1.12 side is the
/// `id | metadata << 12` form entity data, spawn data and particles use
/// (chunks and block updates send `id << 4 | metadata`).
pub fn block_state(shared: &Shared, direction: Direction, id: i32) -> i32 {
    let (from, to) = (shared.source(direction), shared.target(direction));
    if from == to {
        return id;
    }
    match block_states(from, to) {
        Some(mapping) => mapping.get_or(id, 0),
        None if version::flattened(from) != version::flattened(to) => {
            let Some(mapping) = crate::mapping::try_mapping(from, to, "block_state") else {
                return 0;
            };
            if version::flattened(to) {
                mapping.get_or((id & 0xfff) << 4 | (id >> 12 & 0xf), 0)
            } else {
                let state = mapping.get_or(id, 0);
                state >> 4 | (state & 0xf) << 12
            }
        }
        None => id,
    }
}

pub fn block_states_identity(from: i32, to: i32) -> bool {
    from == to
        || block_states(from, to).is_some_and(Mapping::is_identity)
        || (!version::flattened(from) && !version::flattened(to))
}

/// Protocols where text components changed shape, so a component crossing
/// one of them needs the text codec: 1.13 (translation keys), 1.16 (hover
/// contents, hex colours), 1.20.3 (NBT), 1.20.5 (item components in hover
/// events), 1.21.5 (snake_case events).
// INTEGRATION: ask the text codec once it can say which pairs change text.
const TEXT_CHANGES: &[i32] = &[393, 735, 765, 766, 770];

/// Whether text components need converting between two protocols.
pub fn text_changes(from: i32, to: i32) -> bool {
    let (low, high) = (from.min(to), from.max(to));
    TEXT_CHANGES
        .iter()
        .any(|boundary| low < *boundary && *boundary <= high)
}

/// Whether item stacks may need converting between two protocols: their
/// layout changed (1.13, 1.13.2, 1.20.2, 1.20.5), or the item registry did. Component patches (1.20.5 on)
/// change nearly every version, so any pair there converts.
// INTEGRATION: ask the item codec once it can say which pairs change stacks.
pub fn items_change(from: i32, to: i32) -> bool {
    if from == to {
        return false;
    }
    let family = |protocol: i32| match protocol {
        ..393 => 0,
        393..404 => 1,
        404..764 => 2,
        764..766 => 3,
        _ => 4,
    };
    family(from) != family(to) || family(from) == 4 || !registry_identity(from, to, "item")
}

/// Copy one item stack in `protocol`'s layout unchanged. Layouts before
/// 1.20.5 are read here; component patches go through the item codec.
pub fn copy_item(protocol: i32, reader: &mut Reader<'_>, writer: &mut Writer) -> io::Result<()> {
    let start = *reader;
    skip_item(protocol, reader)?;
    writer.bytes(&start.peek()[..start.remaining() - reader.remaining()]);
    Ok(())
}

fn skip_item(protocol: i32, reader: &mut Reader<'_>) -> io::Result<()> {
    if protocol < 393 {
        if reader.i16()? >= 0 {
            reader.u8()?;
            reader.i16()?;
            reader.nbt_bytes(true)?;
        }
    } else if protocol < 404 {
        // 1.13 and 1.13.1: a short ID, -1 for none; no damage any more.
        if reader.i16()? >= 0 {
            reader.u8()?;
            reader.nbt_bytes(true)?;
        }
    } else if protocol < 766 {
        if reader.bool()? {
            reader.varint()?;
            reader.u8()?;
            reader.nbt_bytes(!version::nameless_nbt(protocol))?;
        }
    } else {
        crate::codec::item::read(protocol, reader)?;
    }
    Ok(())
}

/// Read a block position in `protocol`'s packing.
pub fn read_position(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Position> {
    if version::modern_positions(protocol) {
        reader.position()
    } else {
        reader.position_legacy()
    }
}

pub fn write_position(protocol: i32, position: Position, writer: &mut Writer) {
    if version::modern_positions(protocol) {
        writer.position(position);
    } else {
        writer.position_legacy(position);
    }
}

/// Copy one NBT tag, changing its root between the named (before 1.20.2)
/// and nameless forms as the two protocols need.
pub fn convert_nbt(
    from: i32,
    to: i32,
    reader: &mut Reader<'_>,
    writer: &mut Writer,
) -> io::Result<()> {
    let (named_from, named_to) = (!version::nameless_nbt(from), !version::nameless_nbt(to));
    let tag = reader.nbt_bytes(named_from)?;
    if named_from == named_to || tag == [0] {
        writer.bytes(tag);
    } else if named_from {
        let name = usize::from(u16::from_be_bytes([tag[1], tag[2]]));
        writer.u8(tag[0]).bytes(&tag[3 + name..]);
    } else {
        writer.u8(tag[0]).u16(0).bytes(&tag[1..]);
    }
    Ok(())
}

/// A JSON text component holding `text` literally, for strings that become
/// components (pre-1.13 custom names).
pub fn literal_json(text: &str) -> String {
    serde_json::json!({ "text": text }).to_string()
}

/// The plain text of a JSON text component, for components that become
/// strings: the `text` of it and its `extra` children, or a translation key
/// when that is all there is.
pub fn flatten_json(json: &str) -> String {
    fn walk(value: &serde_json::Value, out: &mut String, depth: usize) {
        if depth > 32 {
            return;
        }
        match value {
            serde_json::Value::String(text) => out.push_str(text),
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, out, depth + 1);
                }
            }
            serde_json::Value::Object(object) => {
                if let Some(text) = object.get("text").and_then(|text| text.as_str()) {
                    out.push_str(text);
                } else if let Some(key) = object.get("translate").and_then(|key| key.as_str()) {
                    out.push_str(key);
                }
                if let Some(extra) = object.get("extra") {
                    walk(extra, out, depth + 1);
                }
            }
            other => out.push_str(&other.to_string()),
        }
    }
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(value) => {
            let mut out = String::new();
            walk(&value, &mut out, 0);
            out
        }
        Err(_) => json.to_owned(),
    }
}

/// Fail unless `value` fits the receiver's registry or enum of `size`.
pub fn in_range(value: i32, size: usize, what: &str) -> io::Result<i32> {
    if value >= 0 && (value as usize) < size {
        Ok(value)
    } else {
        Err(invalid(&format!("{what} {value} is out of range")))
    }
}
