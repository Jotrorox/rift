//! Cross-check of the layout tables against PrismarineJS minecraft-data's
//! `SlotComponent` schema (MIT), an independent description of the same
//! wire format for every version it covers. Both are reduced to a wire
//! signature (what bytes follow in which order, ignoring names and enum
//! values); every difference must be one of [`KNOWN`], each of which was
//! settled against the official server's stream codec.
//!
//! Skips when minecraft-data is not on this machine
//! (`$RIFT_MINECRAFT_DATA` or `~/.cache/rift-via/ref/minecraft-data`).
use super::{layouts::Layout, schema::Node};
use crate::data;
use serde_json::Value as Json;
use std::path::PathBuf;

/// minecraft-data's folder for each protocol it describes.
const VERSIONS: &[(i32, &str)] = &[
    (766, "1.20.5"),
    (767, "1.21.1"),
    (768, "1.21.3"),
    (769, "1.21.4"),
    (770, "1.21.5"),
    (771, "1.21.6"),
    (772, "1.21.8"),
    (773, "1.21.9"),
    (774, "1.21.11"),
    (775, "26.1"),
];

/// Differences between minecraft-data and these tables, by protocol range
/// and component, with what the official server's code shows. In every
/// case the table follows Mojang's stream codec (and, where the corpus has
/// the component, recorded traffic).
pub const KNOWN: &[(i32, i32, &str, &str)] = &[
    (
        766,
        769,
        "lore",
        "minecraft-data reads each line as an optional tag; ItemLore.STREAM_CODEC is \
         ComponentSerialization.STREAM_CODEC, a present tag",
    ),
    (
        766,
        777,
        "can_place_on",
        "minecraft-data has plain strings for ranged property bounds, an NBT predicate \
         without its presence flag and, from 1.21.5, a bare type ID per partial predicate; \
         Mojang: StatePropertiesPredicate.RangedMatcher holds two optional strings, \
         BlockPredicate an optional(NbtPredicate), DataComponentPredicate.Single the type \
         and then the predicate as NBT, the type being either(predicate type, component \
         type) from 1.21.11",
    ),
    (766, 777, "can_break", "as can_place_on"),
    (
        766,
        766,
        "food",
        "minecraft-data has 1.21's using_converts_to and a bare effect ID; 1.20.6 \
         FoodProperties has no using_converts_to and PossibleEffect is a whole \
         MobEffectInstance and a probability",
    ),
    (
        767,
        767,
        "food",
        "minecraft-data reads using_converts_to as a stack without presence flag; \
         1.21.1: ItemStack.STREAM_CODEC.apply(ByteBufCodecs::optional)",
    ),
    (
        766,
        767,
        "potion_contents",
        "minecraft-data has custom_name, which 1.21.2 added",
    ),
    (
        766,
        777,
        "written_book_content",
        "minecraft-data reads a filtered page as an optional tag; Filterable.streamCodec \
         writes optional(...) with a presence flag",
    ),
    (
        766,
        768,
        "trim",
        "minecraft-data's trim material lacks item_model_index (a float, until 1.21.4) and \
         keys overrides by name where 1.20.6/1.21.1 key them by armor material ID",
    ),
    (
        766,
        767,
        "instrument",
        "minecraft-data has 1.21.2's layout; 1.20.6/1.21.1 Instrument: use duration as a \
         VarInt of ticks, range, no description",
    ),
    (
        768,
        768,
        "custom_model_data",
        "minecraft-data has 1.21.4's layout; 1.21.3 CustomModelData is a VarInt",
    ),
    (
        768,
        768,
        "banner_patterns",
        "minecraft-data reads the pattern as a sound event holder; BannerPattern \
         .DIRECT_STREAM_CODEC is an asset ID and a translation key",
    ),
    (
        770,
        777,
        "painting/variant",
        "minecraft-data reads width and height as i32; PaintingVariant.DIRECT_STREAM_CODEC \
         has VAR_INT (also seen in recorded traffic by the 1.21.6/1.21.7 step's tests)",
    ),
    (
        773,
        777,
        "profile",
        "minecraft-data reads the skin model as a VarInt enum; PlayerModelType.STREAM_CODEC \
         is a boolean (the same bytes for valid values)",
    ),
    (
        773,
        777,
        "bees",
        "minecraft-data reads occupant data as plain NBT; from 1.21.9 Occupant holds \
         TypedEntityData, an entity type ID and then the compound",
    ),
    (
        775,
        775,
        "damage_type",
        "minecraft-data keeps an either-holder with an inline damage type; 26.1 \
         DamageType.STREAM_CODEC is holderRegistry, a plain ID",
    ),
    (
        775,
        775,
        "damage_resistant",
        "minecraft-data keeps 1.21.11's tag key; 26.1 DamageResistant is a holder set",
    ),
    (
        775,
        775,
        "blocks_attacks",
        "minecraft-data keeps bypassed_by as a tag key; 26.1 has an optional holder set",
    ),
    (
        775,
        775,
        "instrument",
        "minecraft-data keeps 1.21.11's either-holder; 26.1 InstrumentComponent is the \
         holder alone",
    ),
    (775, 775, "provides_trim_material", "as instrument"),
    (775, 775, "jukebox_playable", "as instrument"),
    (
        775,
        775,
        "provides_banner_patterns",
        "minecraft-data keeps 1.21.11's tag key; 26.1 has a holder set",
    ),
    (
        775,
        775,
        "chicken/variant",
        "minecraft-data reads a holder with an inline name; 26.1 ChickenVariant.STREAM_CODEC \
         is holderRegistry, a plain ID",
    ),
    (775, 775, "chicken/sound_variant", "as chicken/variant"),
    (775, 775, "zombie_nautilus/variant", "as chicken/variant"),
];

/// The wire signature of one of our nodes; a node met again inside itself
/// is `R`.
fn ours(node: &Node) -> String {
    ours_in(node, &mut Vec::new())
}

fn ours_in(node: &Node, stack: &mut Vec<usize>) -> String {
    use Node::*;
    let ours = |node: &Node, stack: &mut Vec<usize>| ours_in(node, stack);
    match node {
        Unit => String::new(),
        Bool => "b".into(),
        Byte => "B".into(),
        VarInt | Id(_) | Enum(_) | Kind => "v".into(),
        VarLong => "V".into(),
        Int => "i".into(),
        Long | Position => "l".into(),
        Float => "f".into(),
        Double => "d".into(),
        Str(_) | Identifier => "s".into(),
        Uuid => "u".into(),
        Tag | Compound | Text => "n".into(),
        Holder(_, inline) => format!("H({})", ours(inline, stack)),
        HolderSet(_) => "S".into(),
        Optional(inner) => format!("?({})", ours(inner, stack)),
        List(element, _) => format!("[{}]", ours(element, stack)),
        Array(element, count) => ours(element, stack).repeat(*count),
        Struct(fields) => fields.iter().map(|(_, field)| ours(field, stack)).collect(),
        Either(left, right) => format!("E({}|{})", ours(left, stack), ours(right, stack)),
        Dispatch(..) | Union(_) => "D".into(),
        Stack => "STACK".into(),
        Template => "TEMPLATE".into(),
        Typed => "TYPED".into(),
        Ref(inner) => {
            let address = *inner as *const Node as usize;
            if stack.contains(&address) {
                return "R".into();
            }
            stack.push(address);
            let signature = ours(inner, stack);
            stack.pop();
            signature
        }
    }
}

thread_local! {
    /// minecraft-data type names being expanded, to end recursive types.
    static RECURSING: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The wire signature of a minecraft-data type.
fn theirs(types: &serde_json::Map<String, Json>, json: &Json, depth: usize) -> String {
    if depth > 40 {
        return "…".into();
    }
    match json {
        Json::String(name) => match name.as_str() {
            "void" => String::new(),
            "bool" => "b".into(),
            "i8" | "u8" => "B".into(),
            "varint" | "SlotComponentType" | "ContainerID" => "v".into(),
            "varlong" => "V".into(),
            "i32" | "u32" => "i".into(),
            "i64" | "position" => "l".into(),
            "f32" => "f".into(),
            "f64" => "d".into(),
            "string" | "pstring" => "s".into(),
            "UUID" => "u".into(),
            "anonymousNbt" | "nbt" => "n".into(),
            "anonOptionalNbt" | "optionalNbt" => "n?".into(),
            "Slot" => "STACK".into(),
            "ItemStackTemplate" | "SlotTemplate" => "TEMPLATE".into(),
            "SlotComponent" => "TYPED".into(),
            "IDSet" => "S".into(),
            other => match types.get(other) {
                Some(json) => {
                    if RECURSING.with(|stack| stack.borrow().iter().any(|name| name == other)) {
                        return "R".into();
                    }
                    RECURSING.with(|stack| stack.borrow_mut().push(other.to_owned()));
                    let signature = theirs(types, json, depth + 1);
                    RECURSING.with(|stack| stack.borrow_mut().pop());
                    signature
                }
                None => format!("<{other}>"),
            },
        },
        Json::Array(pair) if pair.len() == 2 => {
            let (kind, args) = (pair[0].as_str().unwrap_or(""), &pair[1]);
            match kind {
                "container" => container(types, args.as_array().unwrap(), depth),
                "option" => format!("?({})", theirs(types, args, depth + 1)),
                "array" => {
                    let element = theirs(types, &args["type"], depth + 1);
                    match &args["count"] {
                        Json::Number(count) => element.repeat(count.as_u64().unwrap() as usize),
                        _ => format!("[{element}]"),
                    }
                }
                "mapper" => theirs(types, &args["type"], depth + 1),
                "bitfield" => {
                    let bits: u64 = args
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|field| field["size"].as_u64().unwrap())
                        .sum();
                    if bits == 64 {
                        "l".into()
                    } else {
                        format!("<bits{bits}>")
                    }
                }
                "registryEntryHolder" => format!(
                    "H({})",
                    theirs(types, &args["otherwise"]["type"], depth + 1)
                ),
                "registryEntryHolderSet" => "S".into(),
                "switch" => "D".into(),
                other => format!("<{other}>"),
            }
        }
        other => format!("<{other}>"),
    }
}

/// A container's fields; a boolean or two-valued enum that a later switch
/// reads becomes an either, any other switch a dispatch.
fn container(types: &serde_json::Map<String, Json>, fields: &[Json], depth: usize) -> String {
    let mut out = String::new();
    for (index, field) in fields.iter().enumerate() {
        let kind = &field["type"];
        if let Some("switch") = kind.get(0).and_then(Json::as_str) {
            let args = &kind[1];
            let compare = args["compareTo"].as_str().unwrap_or("");
            let cases = args["fields"].as_object().cloned().unwrap_or_default();
            let sibling = fields
                .iter()
                .position(|other| other["name"].as_str() == Some(compare));
            let two = |a: &str, b: &str| -> Option<String> {
                Some(format!(
                    "E({}|{})",
                    theirs(types, cases.get(a)?, depth + 1),
                    theirs(types, cases.get(b)?, depth + 1)
                ))
            };
            let either = two("true", "false").or_else(|| two("complete", "partial"));
            // Drop the selector already written: an either or a dispatch
            // includes it.
            if let Some(sibling) = sibling.filter(|sibling| *sibling < index) {
                let selector = theirs(types, &fields[sibling]["type"], depth + 1);
                if out.ends_with(&selector) {
                    out.truncate(out.len() - selector.len());
                }
            }
            out.push_str(&either.unwrap_or_else(|| "D".into()));
            continue;
        }
        out.push_str(&theirs(types, kind, depth + 1));
    }
    out
}

fn minecraft_data() -> Option<PathBuf> {
    let root = std::env::var_os("RIFT_MINECRAFT_DATA")
        .map(PathBuf::from)
        .or_else(|| {
            Some(
                PathBuf::from(std::env::var_os("HOME")?).join(".cache/rift-via/ref/minecraft-data"),
            )
        })?;
    root.join("data/pc")
        .is_dir()
        .then_some(root.join("data/pc"))
}

/// Every difference between the two descriptions: (protocol, component,
/// ours, theirs).
pub fn differences() -> Option<Vec<(i32, String, String, String)>> {
    let root = minecraft_data()?;
    let mut found = Vec::new();
    for (protocol, folder) in VERSIONS {
        let text = std::fs::read_to_string(root.join(folder).join("protocol.json")).ok()?;
        let json: Json = serde_json::from_str(&text).unwrap();
        let types = json["types"].as_object().unwrap();
        let slot = types["SlotComponent"][1].as_array().unwrap();
        let data = slot.iter().find(|field| field["name"] == "data").unwrap();
        let cases = data["type"][1]["fields"].as_object().unwrap();
        let layout = Layout::of(*protocol).unwrap();
        let registry = data::version(*protocol).expect_registry("data_component_type");
        for id in 0..registry.len() as i32 {
            let (kind, node) = layout.get(id).unwrap();
            let ours = ours(node);
            let theirs = match cases.get(kind) {
                Some(json) => theirs(types, json, 0),
                None => "<missing>".into(),
            };
            if ours != theirs {
                found.push((*protocol, kind.to_owned(), ours, theirs));
            }
        }
    }
    Some(found)
}

#[test]
fn layouts_agree_with_minecraft_data_except_known_differences() {
    let Some(found) = differences() else {
        eprintln!("minecraft-data not found; skipping");
        return;
    };
    let unexplained: Vec<_> = found
        .iter()
        .filter(|(protocol, kind, _, _)| {
            !KNOWN
                .iter()
                .any(|(from, to, known, _)| known == kind && (from..=to).contains(&protocol))
        })
        .collect();
    for (protocol, kind, ours, theirs) in &unexplained {
        eprintln!("{protocol} {kind}\n  ours:   {ours}\n  theirs: {theirs}");
    }
    assert!(
        unexplained.is_empty(),
        "{} unexplained differences",
        unexplained.len()
    );
}
