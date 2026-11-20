//! Buttons as item stacks. A button becomes one stack of the newest
//! protocol (26.3, data components), and the translation crate converts it
//! down to each client's protocol by the same steps a backend's stacks take
//! ([`rift_translation::codec::item::newest`]), with stand-ins for items an
//! old client lacks.
//!
//! What a button promises that the conversion alone does not keep is fixed
//! here, per client: names and lore are upright on clients that keep them
//! as `§` strings, a glint and hidden details survive down to 1.12, and
//! `tooltip = false` leaves an empty name where the tooltip cannot be
//! hidden.
//!
//! A converted stack depends only on the newest-protocol stack and the
//! client's protocol, so [`StackCache`] keeps the bytes by exactly that.
//! Texts are resolved before the stack is built, so one player's language
//! or placeholders give a different stack and never another player's
//! cached bytes.
use super::{Head, Look, Model};
use crate::text::{Profile, Text};
use rift_translation::{
    codec::{
        item::{
            self, ItemStack,
            components::{Components, Value, layouts::Layout},
            newest,
        },
        text as wire,
    },
    nbt::{Compound, Nbt},
    version::{self, NEWEST},
    wire::{Writer, invalid},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, OnceLock},
};

#[cfg(test)]
mod tests;

/// Converted stacks the shared cache keeps before it starts over.
const CACHE_ENTRIES: usize = 4096;
/// Stack counts before 1.20.5 are shown up to this.
const MAX_LEGACY_COUNT: i32 = 64;
/// Item names became JSON text in 1.13.
const JSON_NAMES: i32 = 393;
/// Lore lines became JSON text in 1.14.
const JSON_LORE: i32 = 477;
/// All `HideFlags` bits: enchantments, modifiers, unbreakable, can
/// destroy, can place on, other details, dye and armour trims. Clients
/// ignore the bits they do not know.
const HIDE_ALL: i32 = 0xff;
const HIDE_ENCHANTMENTS: i32 = 1;
/// Items that keep their colour in `potion_contents`, not `dyed_color`.
const POTIONS: &[&str] = &[
    "potion",
    "splash_potion",
    "lingering_potion",
    "tipped_arrow",
];
/// Components whose tooltip lines `details = false` hides: what 1.21.5's
/// data fixer turned `HideFlags` and `hide_additional_tooltip` into, and
/// the newer components that add lines of their own.
const DETAILS: &[&str] = &[
    "attribute_modifiers",
    "enchantments",
    "stored_enchantments",
    "unbreakable",
    "dyed_color",
    "trim",
    "can_place_on",
    "can_break",
    "jukebox_playable",
    "banner_patterns",
    "bees",
    "block_entity_data",
    "block_state",
    "bundle_contents",
    "charged_projectiles",
    "container",
    "container_loot",
    "firework_explosion",
    "fireworks",
    "instrument",
    "map_id",
    "painting/variant",
    "pot_decorations",
    "potion_contents",
    "tropical_fish/pattern",
    "written_book_content",
    "ominous_bottle_amplifier",
    "suspicious_stew_effects",
    "potion_duration_scale",
    "provides_trim_material",
    "provides_banner_patterns",
];

/// The signed `textures` property of a player profile, as the session
/// server returns it and head databases publish it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Textures {
    /// Base64 JSON naming the skin.
    pub value: String,
    pub signature: Option<String>,
}

impl Look {
    /// This look as a stack of the newest protocol. `heads` returns the
    /// `textures` property of an online player, by the name or UUID a
    /// `head = { player = ... }` gives; without one the head shows the
    /// default skin.
    pub fn stack(&self, heads: &dyn Fn(&str) -> Option<Textures>) -> io::Result<ItemStack> {
        let id = newest::item_id(self.item)
            .ok_or_else(|| invalid(&format!("unknown item {}", self.item)))?;
        let mut patch = Components::default();
        if let Some(name) = &self.name {
            patch.insert("custom_name", upright(name)?);
        }
        if !self.lore.is_empty() {
            let lines = self.lore.iter().map(upright).collect::<io::Result<_>>()?;
            patch.insert("lore", Value::List(lines));
        }
        if let Some(glint) = self.glint {
            patch.insert("enchantment_glint_override", Value::Bool(glint));
        }
        if let Some(model) = &self.model {
            patch.insert("custom_model_data", model_data(model));
        }
        if let Some(model) = &self.item_model {
            patch.insert("item_model", Value::String(model.clone()));
        }
        let textures = match &self.head {
            Some(Head::Texture(textures)) => Some(textures.clone()),
            Some(Head::Player(player)) => heads(player),
            None => None,
        };
        if let Some(textures) = textures {
            patch.insert("profile", profile(&textures));
        }
        if let Some(color) = self.color {
            let rgb = Value::Int(color as i32);
            if POTIONS.contains(&self.item) {
                patch.insert(
                    "potion_contents",
                    Value::Struct(vec![
                        Value::none(),
                        Value::some(rgb),
                        Value::List(Vec::new()),
                        Value::none(),
                    ]),
                );
            } else {
                patch.insert("dyed_color", rgb);
            }
        }
        if !self.details {
            // No modifiers at all, so no client shows its attack damage.
            patch.insert("attribute_modifiers", Value::List(Vec::new()));
        }
        if !self.details || !self.tooltip {
            let layout = Layout::of(NEWEST)?;
            let hidden = if self.details {
                Vec::new()
            } else {
                DETAILS
                    .iter()
                    .filter(|kind| layout.id(kind).is_some())
                    .map(|kind| Value::Kind(kind))
                    .collect()
            };
            patch.insert(
                "tooltip_display",
                Value::Struct(vec![Value::Bool(!self.tooltip), Value::List(hidden)]),
            );
        }
        Ok(ItemStack {
            id,
            count: i32::from(self.count),
            damage: 0,
            tag: None,
            components: Some(patch),
        })
    }
}

/// A name or lore line as an NBT text of the newest protocol, upright
/// unless the text itself says italic (the game italicizes both).
fn upright(text: &Text) -> io::Result<Value> {
    let json = text.for_client(Profile::UNKNOWN);
    let mut component = wire::from_json_str(NEWEST, &json.to_string())?;
    component.style.italic.get_or_insert(false);
    Ok(Value::Nbt(wire::to_nbt(NEWEST, &component).to_bytes(false)))
}

fn model_data(model: &Model) -> Value {
    Value::Struct(vec![
        Value::List(model.floats.iter().copied().map(Value::Float).collect()),
        Value::List(model.flags.iter().copied().map(Value::Bool).collect()),
        Value::List(model.strings.iter().cloned().map(Value::String).collect()),
        Value::List(model.colors.iter().copied().map(Value::Int).collect()),
    ])
}

/// A partial profile with the texture and an ID made from it: clients
/// before 1.20.2 drop a profile without a name or ID.
fn profile(textures: &Textures) -> Value {
    let digest = Sha256::digest(textures.value.as_bytes());
    let mut id = [0; 16];
    id.copy_from_slice(&digest[..16]);
    let property = Value::Struct(vec![
        Value::String("textures".into()),
        Value::String(textures.value.clone()),
        match &textures.signature {
            Some(signature) => Value::some(Value::String(signature.clone())),
            None => Value::none(),
        },
    ]);
    let partial = Value::Struct(vec![
        Value::none(),
        Value::some(Value::Uuid(u128::from_be_bytes(id))),
        Value::List(vec![property]),
    ]);
    let no_skin_patch = Value::Struct(vec![Value::none(); 4]);
    Value::Struct(vec![Value::Either(false, Box::new(partial)), no_skin_patch])
}

/// `stack`, a stack of the newest protocol, as a client on `protocol`
/// reads it (`None` is the empty slot). Uncached: see [`StackCache`].
pub fn encode(protocol: i32, stack: Option<&ItemStack>) -> io::Result<Vec<u8>> {
    let converted = match stack {
        Some(stack) => convert(protocol, stack)?,
        None => None,
    };
    let mut writer = Writer::new();
    item::write(protocol, converted.as_ref(), &mut writer)?;
    Ok(writer.into_inner())
}

/// What the newest stack asks for beyond what the conversion keeps.
struct Wishes {
    glint: bool,
    hide_details: bool,
    hide_tooltip: bool,
}

impl Wishes {
    fn of(stack: &ItemStack) -> Self {
        let patch = stack.components.as_ref();
        let get = |kind| patch.and_then(|patch| patch.get(kind));
        let (hide_tooltip, hide_details) = match get("tooltip_display") {
            Some(Value::Struct(fields)) => (
                fields.first() == Some(&Value::Bool(true)),
                matches!(fields.get(1), Some(Value::List(hidden)) if !hidden.is_empty()),
            ),
            _ => (false, false),
        };
        Self {
            glint: get("enchantment_glint_override") == Some(&Value::Bool(true)),
            hide_details,
            hide_tooltip,
        }
    }
}

fn convert(protocol: i32, stack: &ItemStack) -> io::Result<Option<ItemStack>> {
    let wishes = Wishes::of(stack);
    let mut stack = stack.clone();
    let legacy = !version::item_components(protocol);
    if legacy {
        stack.count = stack.count.min(MAX_LEGACY_COUNT);
        if wishes.hide_tooltip {
            // The tooltip cannot be hidden: the closest is an empty name.
            if let Some(patch) = &mut stack.components {
                let empty = wire::to_nbt(NEWEST, &wire::Component::text(""));
                patch.insert("custom_name", Value::Nbt(empty.to_bytes(false)));
                patch.take("lore");
            }
        }
    }
    let mut converted = newest::from_newest(protocol, Some(stack))?;
    if legacy && let Some(converted) = &mut converted {
        fix_tag(protocol, &wishes, converted);
    }
    Ok(converted)
}

/// Give a stack with an NBT tag what the conversion could not carry.
fn fix_tag(protocol: i32, wishes: &Wishes, stack: &mut ItemStack) {
    let tag = stack
        .tag
        .get_or_insert_with(|| Nbt::Compound(Compound::new()));
    let Nbt::Compound(tag) = tag else {
        return;
    };
    let mut hide = tag.int("HideFlags").unwrap_or(0);
    if wishes.hide_details || wishes.hide_tooltip {
        hide |= HIDE_ALL;
    }
    if wishes.glint && !version::flattened(protocol) {
        // 1.12 shows the glint only for a list with an enchantment in it;
        // its line is hidden.
        let mut enchantment = Compound::new();
        enchantment.insert("id", Nbt::Short(0));
        enchantment.insert("lvl", Nbt::Short(1));
        tag.insert("ench", Nbt::List(vec![Nbt::Compound(enchantment)]));
        hide |= HIDE_ENCHANTMENTS;
    }
    if hide != 0 {
        tag.insert("HideFlags", Nbt::Int(hide));
    }
    if let Some(display) = tag.compound_mut("display") {
        if protocol < JSON_NAMES
            && let Some(Nbt::String(name)) = display.get_mut("Name")
        {
            *name = legacy_upright(name, 'r');
        }
        if protocol < JSON_LORE
            && let Some(Nbt::List(lines)) = display.get_mut("Lore")
        {
            for line in lines {
                if let Nbt::String(line) = line {
                    *line = legacy_upright(line, '5');
                }
            }
        }
    }
    if tag.is_empty() {
        stack.tag = None;
    }
}

/// A `§` string the game shows italic, upright: a colour code (or a reset)
/// in front ends the italics the game puts before it. `color` is the one
/// the game would show: `r` for names, `5` (dark purple) for lore.
fn legacy_upright(text: &str, color: char) -> String {
    let mut chars = text.chars();
    let starts_with_color = chars.next() == Some('§')
        && chars
            .next()
            .is_some_and(|code| code.is_ascii_hexdigit() || code.eq_ignore_ascii_case(&'r'));
    if starts_with_color {
        text.to_owned()
    } else {
        format!("§{color}{text}")
    }
}

/// Converted stacks by client protocol, then by the newest-protocol stack's
/// bytes.
type Entries = HashMap<i32, HashMap<Vec<u8>, Arc<[u8]>>>;

/// Converted stacks by client protocol and newest-protocol stack, so a
/// button shown to many players on one version is converted once.
pub struct StackCache {
    entries: Mutex<Entries>,
    capacity: usize,
}

impl StackCache {
    /// A cache that starts over after `capacity` stacks.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// The cache every session shares.
    pub fn shared() -> &'static Self {
        static SHARED: OnceLock<StackCache> = OnceLock::new();
        SHARED.get_or_init(|| Self::new(CACHE_ENTRIES))
    }

    /// [`encode`], from the cache when this stack was converted for
    /// `protocol` before.
    pub fn encode(&self, protocol: i32, stack: Option<&ItemStack>) -> io::Result<Arc<[u8]>> {
        let Some(stack) = stack else {
            return Ok(encode(protocol, None)?.into());
        };
        let mut key = Writer::new();
        item::write(NEWEST, Some(stack), &mut key)?;
        let key = key.into_inner();
        if let Some(bytes) = self
            .lock()
            .get(&protocol)
            .and_then(|stacks| stacks.get(key.as_slice()))
        {
            return Ok(bytes.clone());
        }
        let bytes: Arc<[u8]> = encode(protocol, Some(stack))?.into();
        let mut entries = self.lock();
        if entries.values().map(HashMap::len).sum::<usize>() >= self.capacity {
            entries.clear();
        }
        entries
            .entry(protocol)
            .or_default()
            .insert(key, bytes.clone());
        Ok(bytes)
    }

    /// Stacks the cache holds.
    pub fn len(&self) -> usize {
        self.lock().values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Entries> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl super::Menu {
    /// Slot `slot` of this menu as a client on `protocol` reads it: the
    /// button's stack, or the empty stack.
    pub fn slot_stack(
        &self,
        slot: usize,
        protocol: i32,
        heads: &dyn Fn(&str) -> Option<Textures>,
        cache: &StackCache,
    ) -> io::Result<Arc<[u8]>> {
        let stack = match self.slots().get(slot) {
            Some(Some(button)) => Some(button.look.stack(heads)?),
            Some(None) => None,
            None => return Err(invalid(&format!("slot {slot} is outside the menu"))),
        };
        cache.encode(protocol, stack.as_ref())
    }
}
