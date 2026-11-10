//! Hashed stacks (1.21.5+): what a client sends in `container_click` for the
//! slots it predicts changed, instead of whole stacks. Each component is a
//! CRC32C hash of its persistent codec's encoding (`HashOps`), which names
//! registry entries rather than numbering them, so remapping IDs never
//! changes a hash.
//!
//! Converting a hashed stack maps the item and component type IDs and keeps
//! every hash. Where a component's persistent encoding differs between the
//! two protocols, the kept hash no longer matches what the receiving server
//! computes for its own stack; the server then treats the slot as out of
//! sync and sends it again in full, which is safe (vanilla does the same
//! for any misprediction). Recomputing such a hash would need the value,
//! which a hashed stack does not carry. The one exact recomputation is a
//! pure rename: 26.3 split `swing_animation` into `attack_animation` and
//! `interact_animation` with the same encoding, so both get its hash and
//! going back keeps the attack one. Types the receiver lacks are dropped
//! (again a resync at worst), and a stack whose item the receiver lacks
//! becomes empty.
//!
//! [`from_stack`] hashes a whole stack for a client that sends whole stacks
//! to a server that expects hashes (1.21.4 and older on 1.21.5 and newer),
//! exactly for components whose persistent encoding is a plain value
//! (numbers, flags, units, identifiers, `custom_data`), checked against the
//! official 26.3 server's `HashOps`; other components get a placeholder and
//! cost a resend of the slot.
use super::{Component, Value, convert::Conv, layouts::Layout};
use crate::{
    Direction,
    codec::{Shared, item::ItemStack},
    nbt::Nbt,
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

/// Vanilla's limit on hashed components and removed types per stack.
const LIMIT: usize = 256;

/// One non-empty hashed stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashedStack {
    pub item: i32,
    pub count: i32,
    /// Component types set, with their hashes.
    pub added: Vec<(&'static str, i32)>,
    /// Component types removed.
    pub removed: Vec<&'static str>,
}

fn kind(layout: &Layout, r: &mut Reader<'_>) -> io::Result<&'static str> {
    let id = r.varint()?;
    layout
        .get(id)
        .map(|(kind, _)| kind)
        .ok_or_else(|| invalid(&format!("unknown data component type {id}")))
}

/// `HashedStack.STREAM_CODEC`; `None` is the empty stack.
pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Option<HashedStack>> {
    if !r.bool()? {
        return Ok(None);
    }
    let layout = Layout::of(protocol)?;
    let item = r.varint()?;
    let count = r.varint()?;
    let added_count = r.count()?;
    if added_count > LIMIT {
        return Err(invalid("too many hashed components"));
    }
    let mut added = Vec::with_capacity(added_count);
    for _ in 0..added_count {
        added.push((kind(layout, r)?, r.i32()?));
    }
    let removed_count = r.count()?;
    if removed_count > LIMIT {
        return Err(invalid("too many removed components"));
    }
    let mut removed = Vec::with_capacity(removed_count);
    for _ in 0..removed_count {
        removed.push(kind(layout, r)?);
    }
    Ok(Some(HashedStack {
        item,
        count,
        added,
        removed,
    }))
}

pub fn write(protocol: i32, stack: Option<&HashedStack>, w: &mut Writer) -> io::Result<()> {
    let Some(stack) = stack else {
        w.bool(false);
        return Ok(());
    };
    let layout = Layout::of(protocol)?;
    let id = |kind: &str| {
        layout
            .id(kind)
            .ok_or_else(|| invalid(&format!("protocol {protocol} has no {kind} component")))
    };
    w.bool(true).varint(stack.item).varint(stack.count);
    w.varint(stack.added.len() as i32);
    for (kind, hash) in &stack.added {
        w.varint(id(kind)?).i32(*hash);
    }
    w.varint(stack.removed.len() as i32);
    for kind in &stack.removed {
        w.varint(id(kind)?);
    }
    Ok(())
}

/// The receiver's types for the sender's type `kind`: usually one, none
/// when it lacks the type, two for a split.
fn kinds(cx: &mut Conv<'_>, kind: &'static str) -> Vec<&'static str> {
    let renamed: &[&str] = match (cx.older(), cx.up(), kind) {
        (776, true, "swing_animation") => &["attack_animation", "interact_animation"],
        (776, false, "attack_animation") => &["swing_animation"],
        (776, false, "interact_animation") => &[],
        _ => return cx.target_kind(kind).into_iter().collect(),
    };
    let layout = Layout::of(cx.target).ok();
    renamed
        .iter()
        .filter_map(|name| {
            let layout = layout?;
            Some(layout.get(layout.id(name)?)?.0)
        })
        .collect()
}

/// Convert a hashed stack between the two protocols of the step.
pub fn convert(
    shared: &mut Shared,
    direction: Direction,
    stack: &mut Option<HashedStack>,
) -> io::Result<()> {
    let Some(hashed) = stack.take() else {
        return Ok(());
    };
    *stack = super::convert::with(shared, direction, |cx| {
        let Some(item) = cx.map_id("item", hashed.item) else {
            return Ok(None);
        };
        let mut added = Vec::with_capacity(hashed.added.len());
        for (kind, hash) in &hashed.added {
            for kind in kinds(cx, kind) {
                if !added.iter().any(|(known, _)| *known == kind) {
                    added.push((kind, *hash));
                }
            }
        }
        let mut removed = Vec::with_capacity(hashed.removed.len());
        for kind in &hashed.removed {
            for kind in kinds(cx, kind) {
                if !removed.contains(&kind) {
                    removed.push(kind);
                }
            }
        }
        Ok(Some(HashedStack {
            item,
            count: hashed.count,
            added,
            removed,
        }))
    })?
    .flatten();
    Ok(())
}

/// Copy one hashed stack from `rw.r` to `rw.w`, converting it.
pub fn transcode(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let mut stack = read(shared.source(direction), &mut rw.r)?;
    convert(shared, direction, &mut stack)?;
    write(shared.target(direction), stack.as_ref(), &mut rw.w)
}

/// CRC32C (Castagnoli), as Guava's `Hashing.crc32c()`.
fn crc32c(bytes: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            let mut crc = index as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0x82f6_3b78
                } else {
                    crc >> 1
                };
            }
            *entry = crc;
        }
        table
    });
    !bytes.iter().fold(!0u32, |crc, byte| {
        table[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8)
    })
}

/// `HashOps.CRC32C_INSTANCE` (1.21.5+): a value's hash is the CRC32C of a
/// type tag and its little-endian payload; maps hash their entries' hashes
/// sorted by key hash, then value hash.
mod ops {
    use super::crc32c;

    fn tagged(tag: u8, payload: &[u8]) -> u32 {
        let mut bytes = Vec::with_capacity(payload.len() + 1);
        bytes.push(tag);
        bytes.extend_from_slice(payload);
        crc32c(&bytes)
    }

    pub fn byte(value: i8) -> u32 {
        tagged(6, &value.to_le_bytes())
    }

    pub fn short(value: i16) -> u32 {
        tagged(7, &value.to_le_bytes())
    }

    pub fn int(value: i32) -> u32 {
        tagged(8, &value.to_le_bytes())
    }

    pub fn long(value: i64) -> u32 {
        tagged(9, &value.to_le_bytes())
    }

    pub fn float(value: f32) -> u32 {
        tagged(10, &value.to_bits().to_le_bytes())
    }

    pub fn double(value: f64) -> u32 {
        tagged(11, &value.to_bits().to_le_bytes())
    }

    /// Java strings: the UTF-16 length, then each unit little-endian.
    pub fn string(value: &str) -> u32 {
        let units: Vec<u16> = value.encode_utf16().collect();
        let mut payload = Vec::with_capacity(4 + units.len() * 2);
        payload.extend_from_slice(&(units.len() as i32).to_le_bytes());
        for unit in units {
            payload.extend_from_slice(&unit.to_le_bytes());
        }
        tagged(12, &payload)
    }

    pub fn boolean(value: bool) -> u32 {
        tagged(13, &[u8::from(value)])
    }

    pub fn map(mut entries: Vec<(u32, u32)>) -> u32 {
        entries.sort_unstable();
        let mut bytes = Vec::with_capacity(entries.len() * 8 + 2);
        bytes.push(2);
        for (key, value) in entries {
            bytes.extend_from_slice(&key.to_le_bytes());
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.push(3);
        crc32c(&bytes)
    }

    pub fn list(elements: impl IntoIterator<Item = u32>) -> u32 {
        let mut bytes = vec![4];
        for element in elements {
            bytes.extend_from_slice(&element.to_le_bytes());
        }
        bytes.push(5);
        crc32c(&bytes)
    }

    /// A typed array: start tag, the elements little-endian, end tag.
    pub fn array(start: u8, payload: &[u8]) -> u32 {
        let mut bytes = Vec::with_capacity(payload.len() + 2);
        bytes.push(start);
        bytes.extend_from_slice(payload);
        bytes.push(start + 1);
        crc32c(&bytes)
    }
}

/// The hash of an NBT tag through `NbtOps` into `HashOps`.
fn hash_nbt(tag: &Nbt) -> u32 {
    match tag {
        Nbt::Byte(value) => ops::byte(*value),
        Nbt::Short(value) => ops::short(*value),
        Nbt::Int(value) => ops::int(*value),
        Nbt::Long(value) => ops::long(*value),
        Nbt::Float(value) => ops::float(*value),
        Nbt::Double(value) => ops::double(*value),
        Nbt::String(value) => ops::string(value),
        Nbt::ByteArray(values) => ops::array(14, values),
        Nbt::IntArray(values) => ops::array(
            16,
            &values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<_>>(),
        ),
        Nbt::LongArray(values) => ops::array(
            18,
            &values
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<_>>(),
        ),
        Nbt::List(values) => ops::list(values.iter().map(hash_nbt)),
        Nbt::Compound(compound) => ops::map(
            compound
                .iter()
                .map(|(key, value)| (ops::string(key), hash_nbt(value)))
                .collect(),
        ),
    }
}

/// `Rarity`, by ID.
const RARITIES: &[&str] = &["common", "uncommon", "rare", "epic"];

/// The hash a 1.21.5+ client computes for `component`, where its persistent
/// encoding is a plain value this module can reproduce exactly; `None` for
/// the others (text, registry entries by name, records).
pub fn hash_component(component: &Component) -> Option<i32> {
    let value = &component.value;
    let hash = match component.kind {
        "custom_data" => hash_nbt(&value.as_nbt().ok()??),
        "max_stack_size"
        | "max_damage"
        | "damage"
        | "repair_cost"
        | "map_id"
        | "map_color"
        | "ominous_bottle_amplifier" => ops::int(value.as_int()?),
        "dyed_color" => match value {
            Value::Int(rgb) => ops::int(*rgb),
            _ => return None,
        },
        "enchantment_glint_override" => ops::boolean(value.as_bool()?),
        "potion_duration_scale" | "minimum_attack_charge" => match value {
            Value::Float(scale) => ops::float(*scale),
            _ => return None,
        },
        "item_model" | "tooltip_style" | "note_block_sound" => ops::string(value.as_str()?),
        "rarity" => ops::string(RARITIES.get(usize::try_from(value.as_int()?).ok()?)?),
        "unbreakable" | "glider" | "waxed" if *value == Value::Unit => ops::map(Vec::new()),
        // A unit sent through its persistent codec as an empty compound.
        "intangible_projectile" => ops::map(Vec::new()),
        "enchantable" => ops::map(vec![(
            ops::string("value"),
            ops::int(value.fields().ok()?[0].as_int()?),
        )]),
        _ => return None,
    };
    Some(hash as i32)
}

/// The hashed form of a whole stack in `protocol` (1.21.5+), for a client
/// that sends whole stacks when the server expects hashes. Components
/// [`hash_component`] cannot hash get 0: the server then sees a mismatch
/// and resends the slot, which is safe.
pub fn from_stack(protocol: i32, stack: Option<&ItemStack>) -> io::Result<Option<HashedStack>> {
    let layout = Layout::of(protocol)?;
    let Some(stack) = stack.filter(|stack| stack.count > 0) else {
        return Ok(None);
    };
    let components = stack.components.clone().unwrap_or_default();
    let mut added = Vec::with_capacity(components.added.len());
    for component in &components.added {
        if layout.id(component.kind).is_none() {
            return Err(invalid(&format!(
                "protocol {protocol} has no {} component",
                component.kind
            )));
        }
        added.push((component.kind, hash_component(component).unwrap_or(0)));
    }
    Ok(Some(HashedStack {
        item: stack.id,
        count: stack.count,
        added,
        removed: components.removed,
    }))
}
