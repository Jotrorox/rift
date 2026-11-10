//! Text components as JSON and NBT, and the packets that are mostly text.
//!
//! A component is a JSON string on the wire before 1.20.3 and an NBT tag
//! from then on (login disconnects and status stay JSON). Its format
//! changed many times besides; [`component`] models the union of all
//! versions, [`decode`] reads each protocol's format as leniently as its
//! client and [`encode`] writes each protocol's format exactly, degrading
//! what the receiver cannot express. [`snbt`] and [`legacy`] (section-sign
//! text) are public for the other codecs.
//!
//! Rift's implementation was written independently from Mojang's own
//! classes and minecraft-data's protocol descriptions.
pub mod component;
mod convert;
mod decode;
mod encode;
pub mod json;
pub mod legacy;
mod packets;
pub mod snbt;
#[cfg(test)]
mod tests;

pub use component::Component;
pub use convert::adapt;
pub use decode::json_to_nbt;
pub use encode::nbt_to_json;

use super::{HasShared, Shared};
use crate::{
    Direction,
    nbt::Nbt,
    step::RouteBuilder,
    wire::{Reader, Rewriter, Writer, invalid},
};
use decode::Node;
use json::Json;
use std::{collections::HashMap, io};

/// The protocols where the component format, or a structure inside it,
/// changes. Each is the first protocol with the new behaviour.
pub mod version {
    /// 1.13: flattened item IDs in `show_item`, JSON entity names in
    /// `show_entity`, SNBT printing.
    pub const FLATTENING: i32 = 393;
    /// 1.14: `nbt` components (block and entity sources); SNBT quoting.
    pub const NBT_CONTENT: i32 = 477;
    /// 1.15: the `storage` source and the `copy_to_clipboard` click action.
    pub const NBT_STORAGE: i32 = 573;
    pub const COPY_TO_CLIPBOARD: i32 = 573;
    /// 1.16: hex colours, fonts, structured hover `contents`; scores no
    /// longer carry a value.
    pub const HOVER_CONTENTS: i32 = 735;
    pub const HEX_COLORS: i32 = 735;
    pub const FONT: i32 = 735;
    /// 1.17: separators on selector and nbt components.
    pub const SEPARATOR: i32 = 755;
    /// 1.19.4: the translation `fallback`.
    pub const TRANSLATE_FALLBACK: i32 = 762;
    /// The last protocol whose play components are JSON (1.20.2).
    pub const LAST_JSON: i32 = 764;
    /// 1.20.3: NBT on the wire; the codec format (plain text collapses,
    /// primitive translation arguments, `type` fields).
    pub const NBT_ENCODING: i32 = 765;
    /// 1.20.5: `show_item` carries data components; item counts are always
    /// written.
    pub const ITEM_COMPONENTS: i32 = 766;
    pub const ITEM_COUNT_REQUIRED: i32 = 766;
    /// 1.21.4: `shadow_color`.
    pub const SHADOW_COLOR: i32 = 769;
    /// 1.21.5: `click_event`/`hover_event` with per-action fields.
    pub const SNAKE_CASE_EVENTS: i32 = 770;
    /// 1.21.6: `show_dialog` and `custom` click actions.
    pub const DIALOG_CLICK: i32 = 771;
    /// 1.21.9: `object` components (sprites and player heads).
    pub const OBJECT_CONTENT: i32 = 773;
    /// 1.21.11: item sprites moved to their own atlas.
    pub const ITEMS_ATLAS: i32 = 774;
    /// 26.1: `plain` on nbt components.
    pub const NBT_PLAIN: i32 = 775;

    const BOUNDARIES: &[i32] = &[
        FLATTENING,
        NBT_CONTENT,
        NBT_STORAGE,
        HOVER_CONTENTS,
        SEPARATOR,
        TRANSLATE_FALLBACK,
        NBT_ENCODING,
        ITEM_COMPONENTS,
        SHADOW_COLOR,
        SNAKE_CASE_EVENTS,
        DIALOG_CLICK,
        OBJECT_CONTENT,
        ITEMS_ATLAS,
        NBT_PLAIN,
    ];

    /// Whether components differ in format between two protocols.
    pub fn format_changes(a: i32, b: i32) -> bool {
        let (low, high) = (a.min(b), a.max(b));
        BOUNDARIES
            .iter()
            .any(|boundary| low < *boundary && *boundary <= high)
    }
}

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {
    /// Per direction: whether item or entity type names change (`None`
    /// until first asked).
    names_change: [Option<bool>; 2],
    pub(crate) packets: packets::State,
}

/// The maximum length of a JSON component on the wire before 1.20.3.
const MAX_JSON: usize = 262_144;

/// Read one component in `protocol`'s wire encoding.
pub fn read(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Component> {
    if protocol >= version::NBT_ENCODING {
        let tag = Nbt::read(reader, false)?.ok_or_else(|| invalid("missing text component"))?;
        decode::decode(protocol, Node::Nbt(&tag))
    } else {
        from_json_str(protocol, reader.string(MAX_JSON)?)
    }
}

/// Write one component in `protocol`'s wire encoding.
pub fn write(protocol: i32, component: &Component, writer: &mut Writer) {
    if protocol >= version::NBT_ENCODING {
        to_nbt(protocol, component).write(writer, false);
    } else {
        writer.string(&to_json_string(protocol, component));
    }
}

/// A JSON component string read as `protocol` reads one.
pub fn from_json_str(protocol: i32, text: &str) -> io::Result<Component> {
    let json = Json::parse(text)?;
    if json == Json::Null {
        return Ok(Component::text(""));
    }
    decode::decode(protocol, Node::Json(&json))
}

/// A component as `protocol` writes it in JSON (its wire format before
/// 1.20.3; the codec's JSON from then on).
pub fn to_json_string(protocol: i32, component: &Component) -> String {
    encode::encode(protocol, component).to_json().write()
}

/// A component read from NBT in `protocol`'s format (1.20.3 and newer).
pub fn from_nbt(protocol: i32, tag: &Nbt) -> io::Result<Component> {
    decode::decode(protocol, Node::Nbt(tag))
}

/// A component as NBT in `protocol`'s format (1.20.3 and newer).
pub fn to_nbt(protocol: i32, component: &Component) -> Nbt {
    encode::encode(protocol, component).to_nbt()
}

/// A style on its own, as 1.20.3+ number formats and chat decorations
/// carry it, read from NBT in `protocol`'s format.
pub fn style_from_nbt(protocol: i32, tag: &Nbt) -> io::Result<component::Style> {
    let mut with_content = match tag {
        Nbt::Compound(compound) => compound.clone(),
        _ => return Err(invalid("style is not a compound")),
    };
    with_content
        .0
        .retain(|(key, _)| key != "text" && key != "type");
    with_content.insert("text", "");
    Ok(from_nbt(protocol, &Nbt::Compound(with_content))?.style)
}

/// A style on its own as NBT in `protocol`'s format.
pub fn style_to_nbt(protocol: i32, style: &component::Style) -> Nbt {
    let component = Component {
        style: style.clone(),
        ..Component::text("")
    };
    match to_nbt(protocol, &component) {
        Nbt::Compound(mut compound) => {
            compound.remove("text");
            Nbt::Compound(compound)
        }
        _ => Nbt::Compound(Default::default()),
    }
}

/// Whether item or entity type names differ between the step's protocols
/// in `direction`, so hover events must be rewritten even where the format
/// stays the same.
pub fn names_change(shared: &mut Shared, direction: Direction) -> bool {
    if let Some(known) = shared.text.names_change[direction as usize] {
        return known;
    }
    let changes = registry_names_change(shared.source(direction), shared.target(direction));
    shared.text.names_change[direction as usize] = Some(changes);
    changes
}

/// Whether some item or entity type of `from` has no same-named entry in
/// `to`. Cached per pair: route building asks for every pair.
pub fn registry_names_change(from: i32, to: i32) -> bool {
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<(i32, i32), bool>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    *cache.entry((from, to)).or_insert_with(|| {
        let (source, target) = (crate::data::version(from), crate::data::version(to));
        ["item", "entity_type"].iter().any(|registry| {
            match (source.registry(registry), target.registry(registry)) {
                (Some(source), Some(target)) => {
                    source.names().any(|(_, name)| target.id(name).is_none())
                }
                _ => false,
            }
        })
    })
}

/// Whether a component whose encoded form is `raw` must be rewritten
/// between the step's protocols in `direction` rather than copied: the
/// format changes, or names change and the component refers to an item or
/// entity type (a cheap scan of the encoded bytes, which hold the action
/// names verbatim in JSON and NBT alike).
fn needs_rewrite(shared: &mut Shared, direction: Direction, raw: &[u8]) -> bool {
    if version::format_changes(shared.source(direction), shared.target(direction)) {
        return true;
    }
    let mentions = |needle: &[u8]| raw.windows(needle.len()).any(|window| window == needle);
    (mentions(b"show_item") || mentions(b"show_entity")) && names_change(shared, direction)
}

/// Copy one text component from `rw.r` to `rw.w`, converting between the two
/// protocols' encodings (a JSON string before 1.20.3, an NBT tag from then
/// on) and formats (hover and click events, item and entity references).
/// For other codecs whose packets contain components.
pub fn transcode(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let raw = if shared.source(direction) >= version::NBT_ENCODING {
        { rw.r }.nbt_bytes(false)?
    } else {
        { rw.r }.string(MAX_JSON)?.as_bytes()
    };
    if !needs_rewrite(shared, direction, raw) {
        // Same encoding and format on both sides: copy the bytes.
        if shared.source(direction) >= version::NBT_ENCODING {
            rw.nbt(false)?;
        } else {
            rw.string(MAX_JSON)?;
        }
        return Ok(());
    }
    transcode_full(shared, direction, rw)
}

/// [`transcode`] without the copy shortcut.
pub(crate) fn transcode_full(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let mut component = read(shared.source(direction), &mut rw.r)?;
    adapt(shared, direction, &mut component);
    write(shared.target(direction), &component, &mut rw.w);
    Ok(())
}

/// Convert a JSON text component held in a string, as item names, sign lines
/// and book pages are while both protocols keep them as JSON.
pub fn convert_json(shared: &mut Shared, direction: Direction, json: &str) -> io::Result<String> {
    if !needs_rewrite(shared, direction, json.as_bytes()) {
        return Ok(json.to_owned());
    }
    let mut component = from_json_str(shared.source(direction), json)?;
    adapt(shared, direction, &mut component);
    Ok(to_json_string(shared.target(direction), &component))
}

/// Convert a text component held as an NBT tag, for structures that hold
/// one in both protocols (1.20.3 and newer on both sides).
pub fn convert_nbt(shared: &mut Shared, direction: Direction, tag: &mut Nbt) -> io::Result<()> {
    if !needs_rewrite(shared, direction, &tag.to_bytes(false)) {
        return Ok(());
    }
    let mut component = from_nbt(shared.source(direction), tag)?;
    adapt(shared, direction, &mut component);
    *tag = to_nbt(shared.target(direction), &component);
    Ok(())
}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    packets::install(builder)
}

/// The order Java's `HashMap` iterates `keys` in, as indices into `keys`:
/// by bucket of the spread `String.hashCode`, insertion order within a
/// bucket, for a table grown from 16 entries at load factor 0.75. Vanilla
/// writes NBT compounds in this order.
pub fn java_order(keys: &[&str]) -> Vec<usize> {
    let mut capacity = 16usize;
    while keys.len() * 4 > capacity * 3 {
        capacity *= 2;
    }
    let bucket = |key: &str| {
        let hash = key.encode_utf16().fold(0i32, |hash, unit| {
            hash.wrapping_mul(31).wrapping_add(i32::from(unit))
        }) as u32;
        ((hash ^ (hash >> 16)) as usize) & (capacity - 1)
    };
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by_key(|index| bucket(keys[*index]));
    order
}

/// A UUID in its textual form, as `UUID.fromString` accepts it (hex groups
/// of any width up to their size).
pub fn parse_uuid(text: &str) -> Option<u128> {
    let parts: Vec<&str> = text.split('-').collect();
    if parts.len() != 5 || text.len() > 36 {
        return None;
    }
    let widths = [8, 4, 4, 4, 12];
    let mut value = 0u128;
    for (part, width) in parts.iter().zip(widths) {
        if part.is_empty() || part.len() > width {
            return None;
        }
        let number = u64::from_str_radix(part, 16).ok()?;
        value = (value << (width * 4)) | u128::from(number);
    }
    Some(value)
}

pub fn format_uuid(uuid: u128) -> String {
    let hex = format!("{uuid:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// A UUID as the four ints of its NBT form, most significant first.
pub fn uuid_ints(uuid: u128) -> [i32; 4] {
    [
        (uuid >> 96) as i32,
        (uuid >> 64) as i32,
        (uuid >> 32) as i32,
        uuid as i32,
    ]
}

pub fn uuid_from_ints(ints: &[i32]) -> u128 {
    ints.iter()
        .take(4)
        .fold(0u128, |value, int| (value << 32) | u128::from(*int as u32))
}
