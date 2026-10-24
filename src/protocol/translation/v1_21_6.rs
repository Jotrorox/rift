//! Wire layouts shared by Minecraft 1.21.6 (protocol 771) and 1.21.7/1.21.8
//! (protocol 772), sufficient to find and rewrite every built-in registry ID
//! that the two versions number differently.
//!
//! Layouts were derived from the official 1.21.6 server's stream codecs
//! (`DataComponents`, `ItemStack`, `HashedStack`, `ParticleTypes`,
//! `EntityDataSerializers`, `SlotDisplay`, `RecipeDisplay`, `ConsumeEffect` and
//! the packet classes), read with Mojang's published mappings, and checked
//! against real server traffic by tests/translation_minecraft.py. Registry
//! names come from the server's data reports (see data/).
//!
//! Only packets that can carry an item or sound event ID are parsed. They are
//! parsed completely: a value this layout does not understand is an error,
//! never a guess, and trailing bytes after the last field fail translation.
use super::data::Version;
use super::wire::{Rename, Rewriter};
use crate::protocol::{Direction, Packet, State, invalid};
use std::io;

/// Lists are bounded by the bytes that remain; every element is non-empty.
const ANY: usize = usize::MAX;

/// Built-in registry IDs rewritten while copying: source ID to target ID.
/// `None` leaves a registry's IDs unchanged.
pub(super) struct Ids<'a> {
    pub item: Option<&'a [i32]>,
    pub sound: Option<&'a [i32]>,
    /// Item names the receiver lacks, with stand-ins (for registry NBT).
    pub missing_items: &'a [(String, String)],
    /// Sound event names the receiver lacks (for registry NBT).
    pub missing_sounds: &'a [String],
}

impl Ids<'_> {
    fn map(table: Option<&[i32]>, id: i32, what: &str) -> io::Result<i32> {
        let Some(table) = table else {
            return if id >= 0 {
                Ok(id)
            } else {
                Err(invalid(&format!("invalid {what} ID")))
            };
        };
        usize::try_from(id)
            .ok()
            .and_then(|id| table.get(id).copied())
            .ok_or_else(|| invalid(&format!("unknown {what} ID {id}")))
    }

    /// `Item.STREAM_CODEC` and `ByteBufCodecs.registry(ITEM)`: a plain ID.
    fn item(&self, r: &mut Rewriter) -> io::Result<()> {
        let id = r.read_varint()?;
        r.write_varint(Self::map(self.item, id, "item")?);
        Ok(())
    }

    /// `SoundEvent.STREAM_CODEC`: ID + 1, or 0 and an inline sound event.
    fn sound(&self, r: &mut Rewriter) -> io::Result<()> {
        let id = r.read_varint()?;
        if id == 0 {
            r.write_varint(0);
            r.identifier()?;
            return r.optional(|r| r.copy(4)); // Fixed range.
        }
        let id = Self::map(self.sound, id - 1, "sound event")?;
        r.write_varint(id + 1);
        Ok(())
    }

    /// The table for a registry named in a tags packet, if it is rewritten.
    fn registry(&self, name: &str) -> Option<&[i32]> {
        match name {
            "minecraft:item" => self.item,
            "minecraft:sound_event" => self.sound,
            _ => None,
        }
    }
}

/// Data component value layouts, grouped by wire shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Component {
    Unit,
    Bool,
    VarInt,
    Int,
    Float,
    Identifier,
    Nbt,
    Lore,
    Enchantments,
    AdventurePredicate,
    AttributeModifiers,
    CustomModelData,
    TooltipDisplay,
    Food,
    Consumable,
    ItemStack,
    UseCooldown,
    Tool,
    Weapon,
    Equippable,
    Repairable,
    DeathProtection,
    BlocksAttacks,
    ItemStacks,
    PotionContents,
    SuspiciousStewEffects,
    WritableBook,
    WrittenBook,
    Trim,
    Instrument,
    ProvidesTrimMaterial,
    JukeboxPlayable,
    LodestoneTracker,
    FireworkExplosion,
    Fireworks,
    Profile,
    BannerPatterns,
    PotDecorations,
    BlockState,
    Bees,
    Sound,
    ChickenVariant,
    PaintingVariant,
}

/// Every 1.21.6 data component type. Components without a dedicated network
/// codec fall back to NBT (`DataComponentType.Builder.build`).
const COMPONENTS: &[(&str, Component)] = {
    use Component::*;
    &[
        ("custom_data", Nbt),
        ("max_stack_size", VarInt),
        ("max_damage", VarInt),
        ("damage", VarInt),
        ("unbreakable", Unit),
        ("custom_name", Nbt),
        ("item_name", Nbt),
        ("item_model", Identifier),
        ("lore", Lore),
        ("rarity", VarInt),
        ("enchantments", Enchantments),
        ("can_place_on", AdventurePredicate),
        ("can_break", AdventurePredicate),
        ("attribute_modifiers", AttributeModifiers),
        ("custom_model_data", CustomModelData),
        ("tooltip_display", TooltipDisplay),
        ("repair_cost", VarInt),
        ("creative_slot_lock", Unit),
        ("enchantment_glint_override", Bool),
        ("intangible_projectile", Nbt),
        ("food", Food),
        ("consumable", Consumable),
        ("use_remainder", ItemStack),
        ("use_cooldown", UseCooldown),
        ("damage_resistant", Identifier),
        ("tool", Tool),
        ("weapon", Weapon),
        ("enchantable", VarInt),
        ("equippable", Equippable),
        ("repairable", Repairable),
        ("glider", Unit),
        ("tooltip_style", Identifier),
        ("death_protection", DeathProtection),
        ("blocks_attacks", BlocksAttacks),
        ("stored_enchantments", Enchantments),
        ("dyed_color", Int),
        ("map_color", Int),
        ("map_id", VarInt),
        ("map_decorations", Nbt),
        ("map_post_processing", VarInt),
        ("charged_projectiles", ItemStacks),
        ("bundle_contents", ItemStacks),
        ("potion_contents", PotionContents),
        ("potion_duration_scale", Float),
        ("suspicious_stew_effects", SuspiciousStewEffects),
        ("writable_book_content", WritableBook),
        ("written_book_content", WrittenBook),
        ("trim", Trim),
        ("debug_stick_state", Nbt),
        ("entity_data", Nbt),
        ("bucket_entity_data", Nbt),
        ("block_entity_data", Nbt),
        ("instrument", Instrument),
        ("provides_trim_material", ProvidesTrimMaterial),
        ("ominous_bottle_amplifier", VarInt),
        ("jukebox_playable", JukeboxPlayable),
        ("provides_banner_patterns", Identifier),
        ("recipes", Nbt),
        ("lodestone_tracker", LodestoneTracker),
        ("firework_explosion", FireworkExplosion),
        ("fireworks", Fireworks),
        ("profile", Profile),
        ("note_block_sound", Identifier),
        ("banner_patterns", BannerPatterns),
        ("base_color", VarInt),
        ("pot_decorations", PotDecorations),
        ("container", ItemStacks),
        ("block_state", BlockState),
        ("bees", Bees),
        ("lock", Nbt),
        ("container_loot", Nbt),
        ("break_sound", Sound),
        ("villager/variant", VarInt),
        ("wolf/variant", VarInt),
        ("wolf/sound_variant", VarInt),
        ("wolf/collar", VarInt),
        ("fox/variant", VarInt),
        ("salmon/size", VarInt),
        ("parrot/variant", VarInt),
        ("tropical_fish/pattern", VarInt),
        ("tropical_fish/base_color", VarInt),
        ("tropical_fish/pattern_color", VarInt),
        ("mooshroom/variant", VarInt),
        ("rabbit/variant", VarInt),
        ("pig/variant", VarInt),
        ("cow/variant", VarInt),
        ("chicken/variant", ChickenVariant),
        ("frog/variant", VarInt),
        ("horse/variant", VarInt),
        ("painting/variant", PaintingVariant),
        ("llama/variant", VarInt),
        ("axolotl/variant", VarInt),
        ("cat/variant", VarInt),
        ("cat/collar", VarInt),
        ("sheep/color", VarInt),
        ("shulker/color", VarInt),
    ]
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Particle {
    Simple,
    BlockState,
    Dust,
    DustColorTransition,
    Color,
    Item,
    SculkCharge,
    Shriek,
    Vibration,
    Trail,
}

/// 1.21.6 particle types that carry options; all others are simple.
const PARTICLES: &[(&str, Particle)] = {
    use Particle::*;
    &[
        ("block", BlockState),
        ("block_marker", BlockState),
        ("falling_dust", BlockState),
        ("dust_pillar", BlockState),
        ("block_crumble", BlockState),
        ("dust", Dust),
        ("dust_color_transition", DustColorTransition),
        ("entity_effect", Color),
        ("tinted_leaves", Color),
        ("item", Item),
        ("sculk_charge", SculkCharge),
        ("shriek", Shriek),
        ("vibration", Vibration),
        ("trail", Trail),
    ]
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConsumeEffect {
    ApplyEffects,
    RemoveEffects,
    ClearAllEffects,
    TeleportRandomly,
    PlaySound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotDisplay {
    Empty,
    AnyFuel,
    Item,
    ItemStack,
    Tag,
    SmithingTrim,
    WithRemainder,
    Composite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecipeDisplay {
    CraftingShapeless,
    CraftingShaped,
    Furnace,
    Stonecutter,
    Smithing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PositionSource {
    Block,
    Entity,
}

/// Packets that can carry an item or sound event ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    RegistryData,
    UpdateTags,
    AwardStats,
    ContainerSetContent,
    ContainerSetSlot,
    Explode,
    LevelParticles,
    MerchantOffers,
    PlaceGhostRecipe,
    RecipeBookAdd,
    SetCursorItem,
    SetEntityData,
    SetEquipment,
    SetPlayerInventory,
    SoundEntity,
    Sound,
    UpdateAdvancements,
    UpdateRecipes,
    ContainerClick,
    SetCreativeModeSlot,
}

const CONFIGURATION_CLIENTBOUND: &[(&str, Kind)] = &[
    ("registry_data", Kind::RegistryData),
    ("update_tags", Kind::UpdateTags),
];
const PLAY_CLIENTBOUND: &[(&str, Kind)] = {
    use Kind::*;
    &[
        ("award_stats", AwardStats),
        ("container_set_content", ContainerSetContent),
        ("container_set_slot", ContainerSetSlot),
        ("explode", Explode),
        ("level_particles", LevelParticles),
        ("merchant_offers", MerchantOffers),
        ("place_ghost_recipe", PlaceGhostRecipe),
        ("recipe_book_add", RecipeBookAdd),
        ("set_cursor_item", SetCursorItem),
        ("set_entity_data", SetEntityData),
        ("set_equipment", SetEquipment),
        ("set_player_inventory", SetPlayerInventory),
        ("sound_entity", SoundEntity),
        ("sound", Sound),
        ("update_advancements", UpdateAdvancements),
        ("update_recipes", UpdateRecipes),
        ("update_tags", UpdateTags),
    ]
};
const PLAY_SERVERBOUND: &[(&str, Kind)] = &[
    ("container_click", Kind::ContainerClick),
    ("set_creative_mode_slot", Kind::SetCreativeModeSlot),
];

/// Stat types whose value is an item rather than a block, entity or custom stat.
const ITEM_STATS: &[&str] = &["crafted", "used", "broken", "picked_up", "dropped"];

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Layout {
    components: Vec<Component>,
    particles: Vec<Particle>,
    consume_effects: Vec<ConsumeEffect>,
    slot_displays: Vec<SlotDisplay>,
    recipe_displays: Vec<RecipeDisplay>,
    item_stats: Vec<bool>,
    position_sources: Vec<PositionSource>,
    component_predicates: usize,
    configuration_clientbound: Vec<Option<Kind>>,
    play_clientbound: Vec<Option<Kind>>,
    play_serverbound: Vec<Option<Kind>>,
}

fn strip(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

/// Resolve every registry entry through `known`; unknown names are an error.
fn resolve<T: Copy>(
    names: &[String],
    known: &[(&str, T)],
    default: Option<T>,
) -> Result<Vec<T>, String> {
    names
        .iter()
        .map(|name| {
            known
                .iter()
                .find(|(known, _)| *known == strip(name))
                .map(|(_, value)| *value)
                .or(default)
                .ok_or_else(|| format!("no 1.21.6 layout for {name}"))
        })
        .collect()
}

fn packets(names: &[String], wanted: &[(&str, Kind)]) -> Result<Vec<Option<Kind>>, String> {
    for (name, _) in wanted {
        if !names.iter().any(|known| strip(known) == *name) {
            return Err(format!("missing packet {name}"));
        }
    }
    Ok(names
        .iter()
        .map(|name| {
            wanted
                .iter()
                .find(|(known, _)| *known == strip(name))
                .map(|(_, kind)| *kind)
        })
        .collect())
}

impl Layout {
    pub(super) fn new(version: &Version) -> Result<Self, String> {
        use ConsumeEffect as C;
        use RecipeDisplay as R;
        use SlotDisplay as S;
        Ok(Self {
            components: resolve(
                version.registry("minecraft:data_component_type"),
                COMPONENTS,
                None,
            )?,
            particles: resolve(
                version.registry("minecraft:particle_type"),
                PARTICLES,
                Some(Particle::Simple),
            )?,
            consume_effects: resolve(
                version.registry("minecraft:consume_effect_type"),
                &[
                    ("apply_effects", C::ApplyEffects),
                    ("remove_effects", C::RemoveEffects),
                    ("clear_all_effects", C::ClearAllEffects),
                    ("teleport_randomly", C::TeleportRandomly),
                    ("play_sound", C::PlaySound),
                ],
                None,
            )?,
            slot_displays: resolve(
                version.registry("minecraft:slot_display"),
                &[
                    ("empty", S::Empty),
                    ("any_fuel", S::AnyFuel),
                    ("item", S::Item),
                    ("item_stack", S::ItemStack),
                    ("tag", S::Tag),
                    ("smithing_trim", S::SmithingTrim),
                    ("with_remainder", S::WithRemainder),
                    ("composite", S::Composite),
                ],
                None,
            )?,
            recipe_displays: resolve(
                version.registry("minecraft:recipe_display"),
                &[
                    ("crafting_shapeless", R::CraftingShapeless),
                    ("crafting_shaped", R::CraftingShaped),
                    ("furnace", R::Furnace),
                    ("stonecutter", R::Stonecutter),
                    ("smithing", R::Smithing),
                ],
                None,
            )?,
            item_stats: version
                .registry("minecraft:stat_type")
                .iter()
                .map(|name| ITEM_STATS.contains(&strip(name)))
                .collect(),
            position_sources: resolve(
                version.registry("minecraft:position_source_type"),
                &[
                    ("block", PositionSource::Block),
                    ("entity", PositionSource::Entity),
                ],
                None,
            )?,
            component_predicates: version
                .registry("minecraft:data_component_predicate_type")
                .len(),
            configuration_clientbound: packets(
                version.packets("configuration/clientbound"),
                CONFIGURATION_CLIENTBOUND,
            )?,
            play_clientbound: packets(version.packets("play/clientbound"), PLAY_CLIENTBOUND)?,
            play_serverbound: packets(version.packets("play/serverbound"), PLAY_SERVERBOUND)?,
        })
    }

    /// Rewrite `packet`'s registry IDs, or return it unchanged when its kind
    /// cannot carry any.
    pub(super) fn rewrite(
        &self,
        state: State,
        direction: Direction,
        packet: Packet,
        ids: &Ids,
    ) -> io::Result<Packet> {
        let table = match (state, direction) {
            (State::Configuration, Direction::Clientbound) => &self.configuration_clientbound,
            (State::Play, Direction::Clientbound) => &self.play_clientbound,
            (State::Play, Direction::Serverbound) => &self.play_serverbound,
            _ => return Ok(packet),
        };
        let Some(kind) = usize::try_from(packet.id)
            .ok()
            .and_then(|id| table.get(id).copied().flatten())
        else {
            return Ok(packet);
        };
        if kind == Kind::RegistryData
            && ids.missing_items.is_empty()
            && ids.missing_sounds.is_empty()
        {
            return Ok(packet); // Data-driven entries name built-ins the receiver has.
        }
        let mut r = Rewriter::new(&packet.data);
        self.packet(&mut r, ids, kind)?;
        Ok(Packet::new(packet.id, r.finish()?))
    }

    fn packet<'i>(&self, r: &mut Rewriter, ids: &Ids<'i>, kind: Kind) -> io::Result<()> {
        match kind {
            // Data-driven registry entries refer to built-in entries by name.
            // A name the receiver lacks would fail its whole registry load, so
            // items become their stand-ins and sound events are sent inline.
            Kind::RegistryData => {
                r.identifier()?; // Registry.
                let rename = |value: &[u8]| -> Option<Rename<'i>> {
                    if let Some((_, stand_in)) = ids
                        .missing_items
                        .iter()
                        .find(|(name, _)| name.as_bytes() == value)
                    {
                        return Some(Rename::String(stand_in));
                    }
                    ids.missing_sounds
                        .iter()
                        .any(|name| name.as_bytes() == value)
                        .then_some(Rename::SoundEvent)
                };
                r.list(ANY, |r| {
                    r.identifier()?; // Entry.
                    r.optional(|r| r.nbt_renaming(&rename))
                })
            }
            Kind::UpdateTags => r.list(ANY, |r| {
                let table = ids.registry(r.identifier()?);
                r.list(ANY, |r| {
                    r.identifier()?;
                    r.list(ANY, |r| match table {
                        Some(_) => {
                            let id = r.read_varint()?;
                            r.write_varint(Ids::map(table, id, "tagged")?);
                            Ok(())
                        }
                        None => r.varint().map(drop),
                    })
                })
            }),
            Kind::AwardStats => r.list(ANY, |r| {
                let stat = r.varint()?;
                let item = usize::try_from(stat)
                    .ok()
                    .and_then(|stat| self.item_stats.get(stat).copied())
                    .ok_or_else(|| invalid("unknown stat type"))?;
                if item {
                    ids.item(r)?;
                } else {
                    r.varint()?;
                }
                r.varint().map(drop)
            }),
            Kind::ContainerSetContent => {
                r.varint()?; // Container ID.
                r.varint()?; // State ID.
                r.list(ANY, |r| self.item_stack(r, ids))?;
                self.item_stack(r, ids)
            }
            Kind::ContainerSetSlot => {
                r.varint()?;
                r.varint()?;
                r.copy(2)?; // Slot.
                self.item_stack(r, ids)
            }
            Kind::Explode => {
                r.copy(24)?; // Center.
                r.optional(|r| r.copy(24))?; // Player knockback.
                self.particle(r, ids)?;
                ids.sound(r)
            }
            Kind::LevelParticles => {
                r.bool()?; // Override limiter.
                r.bool()?; // Always show.
                r.copy(24 + 12 + 4 + 4)?; // Position, distance, speed, count.
                self.particle(r, ids)
            }
            Kind::MerchantOffers => {
                r.varint()?;
                r.list(ANY, |r| {
                    self.item_cost(r, ids)?;
                    self.item_stack(r, ids)?;
                    r.optional(|r| self.item_cost(r, ids))?;
                    r.bool()?; // Out of stock.
                    r.copy(24) // Uses, max uses, XP, special price, multiplier, demand.
                })?;
                r.varint()?; // Villager level.
                r.varint()?; // Villager XP.
                r.bool()?;
                r.bool().map(drop)
            }
            Kind::PlaceGhostRecipe => {
                r.varint()?;
                self.recipe_display(r, ids)
            }
            Kind::RecipeBookAdd => {
                r.list(ANY, |r| {
                    r.varint()?; // Display ID.
                    self.recipe_display(r, ids)?;
                    r.varint()?; // Optional group, zero when absent.
                    r.varint()?; // Recipe book category.
                    r.optional(|r| r.list(ANY, |r| self.holder_set(r, Some(ids))))?;
                    r.byte().map(drop) // Flags.
                })?;
                r.bool().map(drop) // Replace.
            }
            Kind::SetCursorItem => self.item_stack(r, ids),
            Kind::SetEntityData => {
                r.varint()?;
                self.entity_data(r, ids)
            }
            Kind::SetEquipment => {
                r.varint()?;
                loop {
                    let slot = r.byte()?;
                    self.item_stack(r, ids)?;
                    if slot & 0x80 == 0 {
                        return Ok(());
                    }
                }
            }
            Kind::SetPlayerInventory => {
                r.varint()?;
                self.item_stack(r, ids)
            }
            Kind::SoundEntity => {
                ids.sound(r)?;
                r.varint()?; // Source.
                r.varint()?; // Entity.
                r.copy(4 + 4 + 8) // Volume, pitch and seed.
            }
            Kind::Sound => {
                ids.sound(r)?;
                r.varint()?;
                r.copy(12 + 4 + 4 + 8) // Position, volume, pitch and seed.
            }
            Kind::UpdateAdvancements => {
                r.bool()?; // Reset.
                r.list(ANY, |r| {
                    r.identifier()?;
                    r.optional(|r| r.identifier().map(drop))?; // Parent.
                    r.optional(|r| {
                        r.nbt()?; // Title.
                        r.nbt()?; // Description.
                        self.item_stack(r, ids)?; // Icon.
                        r.varint()?; // Frame type.
                        if r.int()? & 1 != 0 {
                            r.identifier()?; // Background.
                        }
                        r.copy(8) // Position.
                    })?;
                    r.list(ANY, |r| r.list(ANY, |r| r.string(32767).map(drop)))?;
                    r.bool().map(drop) // Sends telemetry.
                })?;
                // Removed advancements and progress cannot contain items.
                r.copy_rest();
                Ok(())
            }
            Kind::UpdateRecipes => {
                r.list(ANY, |r| {
                    r.identifier()?; // Property set.
                    r.list(ANY, |r| ids.item(r))
                })?;
                r.list(ANY, |r| {
                    self.holder_set(r, Some(ids))?;
                    self.slot_display(r, ids)
                })
            }
            Kind::ContainerClick => {
                r.varint()?; // Container ID.
                r.varint()?; // State ID.
                r.copy(2)?; // Slot.
                r.byte()?; // Button.
                r.varint()?; // Click type.
                r.list(128, |r| {
                    r.copy(2)?;
                    self.hashed_stack(r, ids)
                })?;
                self.hashed_stack(r, ids)
            }
            Kind::SetCreativeModeSlot => {
                r.copy(2)?;
                self.item_stack_with(r, ids, true)
            }
        }
    }

    /// `ItemStack.OPTIONAL_STREAM_CODEC` (and the non-empty `STREAM_CODEC`,
    /// which has the same encoding).
    pub(super) fn item_stack(&self, r: &mut Rewriter, ids: &Ids) -> io::Result<()> {
        self.item_stack_with(r, ids, false)
    }

    /// Untrusted stacks, sent by creative clients, length-prefix each component.
    fn item_stack_with(&self, r: &mut Rewriter, ids: &Ids, untrusted: bool) -> io::Result<()> {
        r.nested(|r| {
            if r.varint()? <= 0 {
                return Ok(()); // Empty stack.
            }
            ids.item(r)?;
            let added = r.count(ANY)?;
            let removed = r.count(ANY)?;
            for _ in 0..added {
                let kind = self.component_type(r)?;
                if untrusted {
                    r.length_prefixed(|r| self.component(r, ids, kind))?;
                } else {
                    self.component(r, ids, kind)?;
                }
            }
            for _ in 0..removed {
                self.component_type(r)?;
            }
            Ok(())
        })
    }

    /// `HashedStack.STREAM_CODEC`: hashes are computed over registry names,
    /// so only the item ID changes.
    fn hashed_stack(&self, r: &mut Rewriter, ids: &Ids) -> io::Result<()> {
        r.optional(|r| {
            ids.item(r)?;
            r.varint()?; // Count.
            r.list(256, |r| {
                self.component_type(r)?;
                r.copy(4) // Hash.
            })?;
            r.list(256, |r| self.component_type(r).map(drop))
        })
    }

    fn component_type(&self, r: &mut Rewriter) -> io::Result<Component> {
        let id = r.varint()?;
        usize::try_from(id)
            .ok()
            .and_then(|id| self.components.get(id).copied())
            .ok_or_else(|| invalid("unknown data component type"))
    }

    /// `TypedDataComponent.STREAM_CODEC`.
    fn typed_component(&self, r: &mut Rewriter, ids: &Ids) -> io::Result<()> {
        let kind = self.component_type(r)?;
        r.nested(|r| self.component(r, ids, kind))
    }

    fn component(&self, r: &mut Rewriter, ids: &Ids, kind: Component) -> io::Result<()> {
        use Component::*;
        match kind {
            Unit => Ok(()),
            Bool => r.bool().map(drop),
            VarInt => r.varint().map(drop),
            Int | Float => r.copy(4),
            Identifier => r.identifier().map(drop),
            Nbt => r.nbt(),
            Lore => r.list(256, |r| r.nbt()),
            Enchantments => r.list(ANY, |r| {
                r.varint()?; // Enchantment.
                r.varint().map(drop) // Level.
            }),
            AdventurePredicate => r.list(ANY, |r| self.block_predicate(r, ids)),
            AttributeModifiers => r.list(ANY, |r| {
                r.varint()?; // Attribute.
                r.identifier()?; // Modifier ID.
                r.copy(8)?; // Amount.
                r.varint()?; // Operation.
                r.varint()?; // Slot group.
                match r.varint()? {
                    0 | 1 => Ok(()), // Default or hidden display.
                    2 => r.nbt(),    // Override text.
                    _ => Err(invalid("unknown attribute modifier display")),
                }
            }),
            CustomModelData => {
                r.list(ANY, |r| r.copy(4))?;
                r.list(ANY, |r| r.bool().map(drop))?;
                r.list(ANY, |r| r.string(32767).map(drop))?;
                r.list(ANY, |r| r.copy(4))
            }
            TooltipDisplay => {
                r.bool()?;
                r.list(ANY, |r| self.component_type(r).map(drop))
            }
            Food => {
                r.varint()?;
                r.copy(4)?;
                r.bool().map(drop)
            }
            Consumable => {
                r.copy(4)?; // Seconds.
                r.varint()?; // Animation.
                ids.sound(r)?;
                r.bool()?; // Particles.
                r.list(ANY, |r| self.consume_effect(r, ids))
            }
            ItemStack => self.item_stack(r, ids),
            UseCooldown => {
                r.copy(4)?;
                r.optional(|r| r.identifier().map(drop))
            }
            Tool => {
                r.list(ANY, |r| {
                    self.holder_set(r, None)?; // Blocks.
                    r.optional(|r| r.copy(4))?;
                    r.optional(|r| r.bool().map(drop))
                })?;
                r.copy(4)?;
                r.varint()?;
                r.bool().map(drop)
            }
            Weapon => {
                r.varint()?;
                r.copy(4)
            }
            Equippable => {
                r.varint()?; // Slot.
                ids.sound(r)?; // Equip sound.
                r.optional(|r| r.identifier().map(drop))?; // Asset.
                r.optional(|r| r.identifier().map(drop))?; // Camera overlay.
                r.optional(|r| self.holder_set(r, None))?; // Entity types.
                for _ in 0..5 {
                    r.bool()?;
                }
                ids.sound(r) // Shearing sound.
            }
            Repairable => self.holder_set(r, Some(ids)),
            DeathProtection => r.list(ANY, |r| self.consume_effect(r, ids)),
            BlocksAttacks => {
                r.copy(8)?; // Block delay and disable cooldown scale.
                r.list(ANY, |r| {
                    r.copy(4)?;
                    r.optional(|r| self.holder_set(r, None))?; // Damage types.
                    r.copy(8)
                })?;
                r.copy(12)?; // Item damage function.
                r.optional(|r| r.identifier().map(drop))?; // Bypassed by.
                r.optional(|r| ids.sound(r))?;
                r.optional(|r| ids.sound(r))
            }
            ItemStacks => r.list(ANY, |r| self.item_stack(r, ids)),
            PotionContents => {
                r.optional(|r| r.varint().map(drop))?; // Potion.
                r.optional(|r| r.copy(4))?; // Custom color.
                r.list(ANY, |r| self.mob_effect(r))?;
                r.optional(|r| r.string(32767).map(drop))
            }
            SuspiciousStewEffects => r.list(ANY, |r| {
                r.varint()?;
                r.varint().map(drop)
            }),
            WritableBook => r.list(100, |r| {
                r.string(1024)?;
                r.optional(|r| r.string(1024).map(drop))
            }),
            WrittenBook => {
                r.string(32)?;
                r.optional(|r| r.string(32).map(drop))?;
                r.string(32767)?; // Author.
                r.varint()?; // Generation.
                r.list(ANY, |r| {
                    r.nbt()?;
                    r.optional(|r| r.nbt())
                })?;
                r.bool().map(drop) // Resolved.
            }
            Trim => {
                self.trim_material(r)?;
                self.trim_pattern(r)
            }
            Instrument => self.either_holder(r, |r| {
                self.holder(r, |r| {
                    ids.sound(r)?;
                    r.copy(8)?; // Use duration and range.
                    r.nbt() // Description.
                })
            }),
            ProvidesTrimMaterial => self.either_holder(r, |r| self.trim_material(r)),
            JukeboxPlayable => self.either_holder(r, |r| {
                self.holder(r, |r| {
                    ids.sound(r)?;
                    r.nbt()?; // Description.
                    r.copy(4)?; // Length.
                    r.varint().map(drop) // Comparator output.
                })
            }),
            LodestoneTracker => {
                r.optional(|r| {
                    r.identifier()?; // Dimension.
                    r.copy(8) // Position.
                })?;
                r.bool().map(drop)
            }
            FireworkExplosion => self.firework_explosion(r),
            Fireworks => {
                r.varint()?;
                r.list(256, |r| self.firework_explosion(r))
            }
            Profile => {
                r.optional(|r| r.string(16).map(drop))?;
                r.optional(|r| r.copy(16))?;
                r.list(16, |r| {
                    r.string(64)?;
                    r.string(32767)?;
                    r.optional(|r| r.string(1024).map(drop))
                })
            }
            BannerPatterns => r.list(ANY, |r| {
                self.holder(r, |r| {
                    r.identifier()?; // Asset.
                    r.string(32767).map(drop) // Translation key.
                })?;
                r.varint().map(drop) // Color.
            }),
            PotDecorations => r.list(4, |r| ids.item(r)),
            BlockState => r.list(ANY, |r| {
                r.string(32767)?;
                r.string(32767).map(drop)
            }),
            Bees => r.list(ANY, |r| {
                r.nbt()?;
                r.varint()?;
                r.varint().map(drop)
            }),
            Sound => ids.sound(r),
            ChickenVariant => {
                if r.bool()? {
                    r.varint().map(drop)
                } else {
                    r.identifier().map(drop)
                }
            }
            PaintingVariant => self.holder(r, |r| {
                r.varint()?; // Width.
                r.varint()?; // Height.
                r.identifier()?; // Asset.
                r.optional(|r| r.nbt())?; // Title.
                r.optional(|r| r.nbt()) // Author.
            }),
        }
    }

    /// `ByteBufCodecs.holder`: ID + 1, or 0 and an inline value.
    fn holder(
        &self,
        r: &mut Rewriter,
        inline: impl FnOnce(&mut Rewriter) -> io::Result<()>,
    ) -> io::Result<()> {
        if r.varint()? == 0 {
            r.nested(inline)?;
        }
        Ok(())
    }

    /// `EitherHolder.streamCodec`: a holder, or a registry key by name.
    fn either_holder(
        &self,
        r: &mut Rewriter,
        holder: impl FnOnce(&mut Rewriter) -> io::Result<()>,
    ) -> io::Result<()> {
        if r.bool()? {
            holder(r)
        } else {
            r.identifier().map(drop)
        }
    }

    /// `ByteBufCodecs.holderSet`: 0 and a tag name, or count + 1 plain IDs.
    /// `items` is set for item holder sets, whose IDs are rewritten.
    fn holder_set(&self, r: &mut Rewriter, items: Option<&Ids>) -> io::Result<()> {
        let size = r.varint()?;
        if size == 0 {
            return r.identifier().map(drop);
        }
        if size < 0 || size as usize - 1 > r.remaining() {
            return Err(invalid("invalid holder set size"));
        }
        for _ in 1..size {
            match items {
                Some(ids) => ids.item(r)?,
                None => drop(r.varint()?),
            }
        }
        Ok(())
    }

    fn trim_material(&self, r: &mut Rewriter) -> io::Result<()> {
        self.holder(r, |r| {
            r.string(32767)?; // Base asset suffix.
            r.list(ANY, |r| {
                r.identifier()?; // Equipment asset.
                r.string(32767).map(drop)
            })?;
            r.nbt() // Description.
        })
    }

    fn trim_pattern(&self, r: &mut Rewriter) -> io::Result<()> {
        self.holder(r, |r| {
            r.identifier()?;
            r.nbt()?;
            r.bool().map(drop) // Decal.
        })
    }

    fn firework_explosion(&self, r: &mut Rewriter) -> io::Result<()> {
        r.varint()?; // Shape.
        r.list(ANY, |r| r.copy(4))?;
        r.list(ANY, |r| r.copy(4))?;
        r.bool()?;
        r.bool().map(drop)
    }

    /// `MobEffectInstance.STREAM_CODEC`.
    fn mob_effect(&self, r: &mut Rewriter) -> io::Result<()> {
        r.varint()?; // Effect.
        self.mob_effect_details(r)
    }

    fn mob_effect_details(&self, r: &mut Rewriter) -> io::Result<()> {
        r.nested(|r| {
            r.varint()?; // Amplifier.
            r.varint()?; // Duration.
            for _ in 0..3 {
                r.bool()?; // Ambient, particles and icon.
            }
            r.optional(|r| self.mob_effect_details(r)) // Hidden effect.
        })
    }

    fn consume_effect(&self, r: &mut Rewriter, ids: &Ids) -> io::Result<()> {
        let id = r.varint()?;
        let kind = usize::try_from(id)
            .ok()
            .and_then(|id| self.consume_effects.get(id).copied())
            .ok_or_else(|| invalid("unknown consume effect type"))?;
        match kind {
            ConsumeEffect::ApplyEffects => {
                r.list(ANY, |r| self.mob_effect(r))?;
                r.copy(4) // Probability.
            }
            ConsumeEffect::RemoveEffects => self.holder_set(r, None),
            ConsumeEffect::ClearAllEffects => Ok(()),
            ConsumeEffect::TeleportRandomly => r.copy(4),
            ConsumeEffect::PlaySound => ids.sound(r),
        }
    }

    /// `BlockPredicate.STREAM_CODEC`, used by adventure-mode predicates.
    fn block_predicate(&self, r: &mut Rewriter, ids: &Ids) -> io::Result<()> {
        r.optional(|r| self.holder_set(r, None))?; // Blocks.
        r.optional(|r| {
            r.list(ANY, |r| {
                r.string(32767)?; // Property.
                if r.bool()? {
                    r.string(32767).map(drop) // Exact value.
                } else {
                    r.optional(|r| r.string(32767).map(drop))?; // Minimum.
                    r.optional(|r| r.string(32767).map(drop)) // Maximum.
                }
            })
        })?;
        r.optional(|r| r.nbt())?;
        // Exact component matches carry full component values, which may
        // themselves contain items; partial predicates are NBT.
        r.list(ANY, |r| self.typed_component(r, ids))?;
        r.list(64, |r| {
            let id = r.varint()?;
            if !usize::try_from(id).is_ok_and(|id| id < self.component_predicates) {
                return Err(invalid("unknown data component predicate type"));
            }
            r.nbt()
        })
    }

    /// `ItemCost.STREAM_CODEC`: item, count and exact component matches.
    fn item_cost(&self, r: &mut Rewriter, ids: &Ids) -> io::Result<()> {
        ids.item(r)?;
        r.varint()?;
        r.list(ANY, |r| self.typed_component(r, ids))
    }

    fn particle(&self, r: &mut Rewriter, ids: &Ids) -> io::Result<()> {
        let id = r.varint()?;
        let kind = usize::try_from(id)
            .ok()
            .and_then(|id| self.particles.get(id).copied())
            .ok_or_else(|| invalid("unknown particle type"))?;
        match kind {
            Particle::Simple => Ok(()),
            Particle::BlockState | Particle::Shriek => r.varint().map(drop),
            Particle::Dust => r.copy(8),
            Particle::DustColorTransition => r.copy(12),
            Particle::Color | Particle::SculkCharge => r.copy(4),
            Particle::Item => self.item_stack(r, ids),
            Particle::Vibration => {
                let source = r.varint()?;
                match usize::try_from(source)
                    .ok()
                    .and_then(|source| self.position_sources.get(source).copied())
                {
                    Some(PositionSource::Block) => r.copy(8)?,
                    Some(PositionSource::Entity) => {
                        r.varint()?;
                        r.copy(4)?; // Y offset.
                    }
                    None => return Err(invalid("unknown position source type")),
                }
                r.varint().map(drop) // Arrival ticks.
            }
            Particle::Trail => {
                r.copy(24)?; // Target.
                r.copy(4)?; // Color.
                r.varint().map(drop) // Duration.
            }
        }
    }

    /// Entity metadata, by `EntityDataSerializers` registration order.
    fn entity_data(&self, r: &mut Rewriter, ids: &Ids) -> io::Result<()> {
        loop {
            if r.byte()? == 0xff {
                return Ok(());
            }
            match r.varint()? {
                0 => r.copy(1)?, // Byte.
                1 | 12 | 14 | 15 | 20 | 21 => {
                    // VarInt-valued.
                    r.varint()?;
                }
                2 => r.varlong()?,             // Long.
                3 => r.copy(4)?,               // Float.
                4 => drop(r.string(32767)?),   // String.
                5 | 16 => r.nbt()?,            // Component, compound tag.
                6 => r.optional(|r| r.nbt())?, // Optional component.
                7 => self.item_stack(r, ids)?,
                8 => drop(r.bool()?),
                9 => r.copy(12)?,                  // Rotations.
                10 => r.copy(8)?,                  // Block position.
                11 => r.optional(|r| r.copy(8))?,  // Optional block position.
                13 => r.optional(|r| r.copy(16))?, // Optional entity reference.
                17 => self.particle(r, ids)?,
                18 => r.list(ANY, |r| self.particle(r, ids))?,
                19 => {
                    // Villager data.
                    for _ in 0..3 {
                        r.varint()?;
                    }
                }
                22..=28 | 31 | 32 => {
                    // Variants and states.
                    r.varint()?;
                }
                29 => r.optional(|r| {
                    // Optional global position.
                    r.identifier()?;
                    r.copy(8)
                })?,
                30 => self.component(r, ids, Component::PaintingVariant)?,
                33 => r.copy(12)?, // Vector3f.
                34 => r.copy(16)?, // Quaternionf.
                _ => return Err(invalid("unknown entity data serializer")),
            }
        }
    }

    fn slot_display(&self, r: &mut Rewriter, ids: &Ids) -> io::Result<()> {
        r.nested(|r| {
            let id = r.varint()?;
            let kind = usize::try_from(id)
                .ok()
                .and_then(|id| self.slot_displays.get(id).copied())
                .ok_or_else(|| invalid("unknown slot display type"))?;
            match kind {
                SlotDisplay::Empty | SlotDisplay::AnyFuel => Ok(()),
                SlotDisplay::Item => ids.item(r),
                SlotDisplay::ItemStack => self.item_stack(r, ids),
                SlotDisplay::Tag => r.identifier().map(drop),
                SlotDisplay::SmithingTrim => {
                    self.slot_display(r, ids)?; // Base.
                    self.slot_display(r, ids)?; // Material.
                    self.trim_pattern(r)
                }
                SlotDisplay::WithRemainder => {
                    self.slot_display(r, ids)?;
                    self.slot_display(r, ids)
                }
                SlotDisplay::Composite => r.list(ANY, |r| self.slot_display(r, ids)),
            }
        })
    }

    fn recipe_display(&self, r: &mut Rewriter, ids: &Ids) -> io::Result<()> {
        let id = r.varint()?;
        let kind = usize::try_from(id)
            .ok()
            .and_then(|id| self.recipe_displays.get(id).copied())
            .ok_or_else(|| invalid("unknown recipe display type"))?;
        let displays =
            |r: &mut Rewriter, count| (0..count).try_for_each(|_| self.slot_display(r, ids));
        match kind {
            RecipeDisplay::CraftingShapeless => {
                r.list(ANY, |r| self.slot_display(r, ids))?;
                displays(r, 2) // Result and station.
            }
            RecipeDisplay::CraftingShaped => {
                r.varint()?; // Width.
                r.varint()?; // Height.
                r.list(ANY, |r| self.slot_display(r, ids))?;
                displays(r, 2)
            }
            RecipeDisplay::Furnace => {
                displays(r, 4)?; // Ingredient, fuel, result and station.
                r.varint()?; // Duration.
                r.copy(4) // Experience.
            }
            RecipeDisplay::Stonecutter => displays(r, 3),
            RecipeDisplay::Smithing => displays(r, 5),
        }
    }
}
