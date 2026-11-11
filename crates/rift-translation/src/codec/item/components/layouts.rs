//! The wire layout of every data component type in every protocol from 766
//! (1.20.5) on, as rows of [`Node`] trees: a component's layout applies from
//! the protocol of its row until the next row. Which components a protocol
//! has, and their IDs, come from its `data_component_type` registry.
//!
//! Layouts were taken from the official servers' stream codecs: the
//! unobfuscated 26.x jars, and 1.20.6 to 1.21.11 remapped with Mojang's
//! published mappings. Components without a stream codec of their own go
//! over the network through their persistent codec as NBT (`Tag`). The
//! module's tests check the tables against recorded traffic and, where it
//! has a schema, minecraft-data.
use super::schema::Node::{self, *};
use crate::{data, wire::invalid};
use std::{collections::HashMap, io, sync::OnceLock};

/// `ByteBufCodecs.STRING_UTF8`.
pub(super) const STRING: Node = Str(32767);
pub(super) const ANY: usize = usize::MAX;

pub(super) const SOUND_EVENT: Node =
    Struct(&[("location", Identifier), ("fixed_range", Optional(&Float))]);
/// `SoundEvent.STREAM_CODEC`.
pub(super) const SOUND: Node = Holder("sound_event", &SOUND_EVENT);

pub(super) static EFFECT_DETAILS: Node = Struct(&[
    ("amplifier", VarInt),
    ("duration", VarInt),
    ("ambient", Bool),
    ("show_particles", Bool),
    ("show_icon", Bool),
    ("hidden_effect", Optional(&Ref(&EFFECT_DETAILS))),
]);
/// `MobEffectInstance.STREAM_CODEC`.
pub(super) const EFFECT: Node = Struct(&[
    ("effect", Id("mob_effect")),
    ("details", Ref(&EFFECT_DETAILS)),
]);

/// `Filterable.streamCodec`: the raw value and, optionally, its filtered form.
pub(super) const FILTERED_TITLE: Node =
    Struct(&[("raw", Str(32)), ("filtered", Optional(&Str(32)))]);
pub(super) const FILTERED_PAGE: Node =
    Struct(&[("raw", Str(1024)), ("filtered", Optional(&Str(1024)))]);
pub(super) const FILTERED_TEXT: Node = Struct(&[("raw", Text), ("filtered", Optional(&Text))]);

pub(super) const PROPERTY_MATCHER: Node = Struct(&[
    ("name", STRING),
    (
        "value",
        Either(
            &STRING,
            &Struct(&[("min", Optional(&STRING)), ("max", Optional(&STRING))]),
        ),
    ),
]);
pub(super) const BLOCKS: Node = Optional(&HolderSet("block"));
pub(super) const PROPERTIES: Node = Optional(&List(&PROPERTY_MATCHER, ANY));
pub(super) const NBT_PREDICATE: Node = Optional(&Compound);
/// `BlockPredicate.STREAM_CODEC` before 1.21.5.
pub(super) const BLOCK_PREDICATE_766: Node = Struct(&[
    ("blocks", BLOCKS),
    ("properties", PROPERTIES),
    ("nbt", NBT_PREDICATE),
]);
/// `DataComponentPredicate.Single`, 1.21.5 to 1.21.10: a predicate type and
/// its value in the predicate's persistent codec.
pub(super) const PARTIAL_770: Node = Struct(&[
    ("type", Id("data_component_predicate_type")),
    ("value", Tag),
]);
/// From 1.21.11 the type is either a predicate type or, for an "any value"
/// check, a component type (whose value is then an empty compound).
pub(super) const PARTIAL_774: Node = Struct(&[
    ("type", Either(&Id("data_component_predicate_type"), &Kind)),
    ("value", Tag),
]);
/// `DataComponentMatchers.STREAM_CODEC`: exact components and partial
/// predicates.
pub(super) const MATCHERS_770: Node = Struct(&[
    ("exact", List(&Typed, ANY)),
    ("partial", List(&PARTIAL_770, 64)),
]);
pub(super) const MATCHERS_774: Node = Struct(&[
    ("exact", List(&Typed, ANY)),
    ("partial", List(&PARTIAL_774, 64)),
]);
pub(super) const BLOCK_PREDICATE_770: Node = Struct(&[
    ("blocks", BLOCKS),
    ("properties", PROPERTIES),
    ("nbt", NBT_PREDICATE),
    ("components", MATCHERS_770),
]);
pub(super) const BLOCK_PREDICATE_774: Node = Struct(&[
    ("blocks", BLOCKS),
    ("properties", PROPERTIES),
    ("nbt", NBT_PREDICATE),
    ("components", MATCHERS_774),
]);
pub(super) const ADVENTURE_766: Node = Struct(&[
    ("predicates", List(&BLOCK_PREDICATE_766, ANY)),
    ("show_in_tooltip", Bool),
]);
pub(super) const ADVENTURE_770: Node = List(&BLOCK_PREDICATE_770, ANY);
pub(super) const ADVENTURE_774: Node = List(&BLOCK_PREDICATE_774, ANY);

pub(super) const ENCHANTMENT_LEVELS: Node = List(
    &Struct(&[("enchantment", Id("enchantment")), ("level", VarInt)]),
    ANY,
);
pub(super) const ENCHANTMENTS_766: Node =
    Struct(&[("levels", ENCHANTMENT_LEVELS), ("show_in_tooltip", Bool)]);

/// `EquipmentSlotGroup`.
pub(super) const SLOT_GROUPS_766: &[&str] = &[
    "any", "mainhand", "offhand", "hand", "feet", "legs", "chest", "head", "armor", "body",
];
pub(super) const SLOT_GROUPS_770: &[&str] = &[
    "any", "mainhand", "offhand", "hand", "feet", "legs", "chest", "head", "armor", "body",
    "saddle",
];
/// `EquipmentSlot`.
pub(super) const SLOTS_768: &[&str] = &[
    "mainhand", "offhand", "feet", "legs", "chest", "head", "body",
];
pub(super) const SLOTS_770: &[&str] = &[
    "mainhand", "offhand", "feet", "legs", "chest", "head", "body", "saddle",
];
/// `ItemUseAnimation`. ID 5 was called `spear` until 1.21.11 renamed it
/// `trident` and added a new `spear`; it is named by what it shows.
pub(super) const ANIMATIONS_768: &[&str] = &[
    "none",
    "eat",
    "drink",
    "block",
    "bow",
    "trident",
    "crossbow",
    "spyglass",
    "toot_horn",
    "brush",
];
pub(super) const ANIMATIONS_769: &[&str] = &[
    "none",
    "eat",
    "drink",
    "block",
    "bow",
    "trident",
    "crossbow",
    "spyglass",
    "toot_horn",
    "brush",
    "bundle",
];
pub(super) const ANIMATIONS_774: &[&str] = &[
    "none",
    "eat",
    "drink",
    "block",
    "bow",
    "trident",
    "crossbow",
    "spyglass",
    "toot_horn",
    "brush",
    "bundle",
    "spear",
];

pub(super) const ATTRIBUTE_MODIFIERS_766: Node = Struct(&[
    (
        "modifiers",
        List(
            &Struct(&[
                ("attribute", Id("attribute")),
                ("id", Uuid),
                ("name", STRING),
                ("amount", Double),
                ("operation", VarInt),
                ("slot", Enum(SLOT_GROUPS_766)),
            ]),
            ANY,
        ),
    ),
    ("show_in_tooltip", Bool),
]);
/// 1.21 replaced the modifier's UUID and name with an identifier.
pub(super) const ATTRIBUTE_MODIFIERS_767: Node = Struct(&[
    (
        "modifiers",
        List(
            &Struct(&[
                ("attribute", Id("attribute")),
                ("id", Identifier),
                ("amount", Double),
                ("operation", VarInt),
                ("slot", Enum(SLOT_GROUPS_766)),
            ]),
            ANY,
        ),
    ),
    ("show_in_tooltip", Bool),
]);
pub(super) const ATTRIBUTE_MODIFIERS_770: Node = List(
    &Struct(&[
        ("attribute", Id("attribute")),
        ("id", Identifier),
        ("amount", Double),
        ("operation", VarInt),
        ("slot", Enum(SLOT_GROUPS_770)),
    ]),
    ANY,
);
/// 1.21.6 added how the modifier shows in tooltips.
pub(super) const ATTRIBUTE_MODIFIERS_771: Node = List(
    &Struct(&[
        ("attribute", Id("attribute")),
        ("id", Identifier),
        ("amount", Double),
        ("operation", VarInt),
        ("slot", Enum(SLOT_GROUPS_770)),
        (
            "display",
            Union(&[("default", Unit), ("hidden", Unit), ("override", Text)]),
        ),
    ]),
    ANY,
);

pub(super) const CUSTOM_MODEL_DATA_769: Node = Struct(&[
    ("floats", List(&Float, ANY)),
    ("flags", List(&Bool, ANY)),
    ("strings", List(&STRING, ANY)),
    ("colors", List(&Int, ANY)),
]);

pub(super) const POSSIBLE_EFFECTS: Node =
    List(&Struct(&[("effect", EFFECT), ("probability", Float)]), ANY);
pub(super) const FOOD_766: Node = Struct(&[
    ("nutrition", VarInt),
    ("saturation", Float),
    ("can_always_eat", Bool),
    ("eat_seconds", Float),
    ("effects", POSSIBLE_EFFECTS),
]);
pub(super) const FOOD_767: Node = Struct(&[
    ("nutrition", VarInt),
    ("saturation", Float),
    ("can_always_eat", Bool),
    ("eat_seconds", Float),
    ("using_converts_to", Optional(&Stack)),
    ("effects", POSSIBLE_EFFECTS),
]);
pub(super) const FOOD_768: Node = Struct(&[
    ("nutrition", VarInt),
    ("saturation", Float),
    ("can_always_eat", Bool),
]);

pub(super) const TOOL_RULES: Node = List(
    &Struct(&[
        ("blocks", HolderSet("block")),
        ("speed", Optional(&Float)),
        ("correct_for_drops", Optional(&Bool)),
    ]),
    ANY,
);
pub(super) const TOOL_766: Node = Struct(&[
    ("rules", TOOL_RULES),
    ("default_mining_speed", Float),
    ("damage_per_block", VarInt),
]);
pub(super) const TOOL_770: Node = Struct(&[
    ("rules", TOOL_RULES),
    ("default_mining_speed", Float),
    ("damage_per_block", VarInt),
    ("can_destroy_blocks_in_creative", Bool),
]);

pub(super) const DYED_COLOR_766: Node = Struct(&[("rgb", Int), ("show_in_tooltip", Bool)]);

pub(super) const POTION_CONTENTS_766: Node = Struct(&[
    ("potion", Optional(&Id("potion"))),
    ("custom_color", Optional(&Int)),
    ("custom_effects", List(&EFFECT, ANY)),
]);
pub(super) const POTION_CONTENTS_768: Node = Struct(&[
    ("potion", Optional(&Id("potion"))),
    ("custom_color", Optional(&Int)),
    ("custom_effects", List(&EFFECT, ANY)),
    ("custom_name", Optional(&STRING)),
]);

pub(super) const SUSPICIOUS_STEW: Node = List(
    &Struct(&[("effect", Id("mob_effect")), ("duration", VarInt)]),
    ANY,
);
pub(super) const WRITABLE_BOOK: Node = List(&FILTERED_PAGE, 100);
pub(super) const WRITTEN_BOOK: Node = Struct(&[
    ("title", FILTERED_TITLE),
    ("author", STRING),
    ("generation", VarInt),
    ("pages", List(&FILTERED_TEXT, ANY)),
    ("resolved", Bool),
]);

/// `TrimMaterial.DIRECT_STREAM_CODEC`, which changed nearly every release.
pub(super) const TRIM_MATERIAL_766: Node = Struct(&[
    ("asset_name", STRING),
    ("ingredient", Id("item")),
    ("item_model_index", Float),
    (
        "override_armor_materials",
        List(
            &Struct(&[("material", Id("armor_material")), ("asset", STRING)]),
            ANY,
        ),
    ),
    ("description", Text),
]);
pub(super) const TRIM_MATERIAL_768: Node = Struct(&[
    ("asset_name", STRING),
    ("ingredient", Id("item")),
    ("item_model_index", Float),
    (
        "override_armor_materials",
        List(&Struct(&[("material", Identifier), ("asset", STRING)]), ANY),
    ),
    ("description", Text),
]);
pub(super) const TRIM_MATERIAL_769: Node = Struct(&[
    ("asset_name", STRING),
    ("ingredient", Id("item")),
    (
        "override_armor_assets",
        List(&Struct(&[("asset", Identifier), ("suffix", STRING)]), ANY),
    ),
    ("description", Text),
]);
pub(super) const TRIM_MATERIAL_770: Node = Struct(&[
    ("base", STRING),
    (
        "overrides",
        List(&Struct(&[("asset", Identifier), ("suffix", STRING)]), ANY),
    ),
    ("description", Text),
]);
pub(super) const TRIM_MATERIAL_777: Node =
    Struct(&[("palette", Identifier), ("description", Text)]);
pub(super) const TRIM_PATTERN_766: Node = Struct(&[
    ("asset_id", Identifier),
    ("template_item", Id("item")),
    ("description", Text),
    ("decal", Bool),
]);
pub(super) const TRIM_PATTERN_770: Node = Struct(&[
    ("asset_id", Identifier),
    ("description", Text),
    ("decal", Bool),
]);
pub(super) const TRIM_MATERIAL_HOLDER_770: Node = Holder("trim_material", &TRIM_MATERIAL_770);
pub(super) const TRIM_PATTERN_HOLDER_770: Node = Holder("trim_pattern", &TRIM_PATTERN_770);

pub(super) const TRIM_766: Node = Struct(&[
    ("material", Holder("trim_material", &TRIM_MATERIAL_766)),
    ("pattern", Holder("trim_pattern", &TRIM_PATTERN_766)),
    ("show_in_tooltip", Bool),
]);
pub(super) const TRIM_768: Node = Struct(&[
    ("material", Holder("trim_material", &TRIM_MATERIAL_768)),
    ("pattern", Holder("trim_pattern", &TRIM_PATTERN_766)),
    ("show_in_tooltip", Bool),
]);
pub(super) const TRIM_769: Node = Struct(&[
    ("material", Holder("trim_material", &TRIM_MATERIAL_769)),
    ("pattern", Holder("trim_pattern", &TRIM_PATTERN_766)),
    ("show_in_tooltip", Bool),
]);
pub(super) const TRIM_770: Node = Struct(&[
    ("material", TRIM_MATERIAL_HOLDER_770),
    ("pattern", TRIM_PATTERN_HOLDER_770),
]);
pub(super) const TRIM_777: Node = Struct(&[
    ("material", Holder("trim_material", &TRIM_MATERIAL_777)),
    ("pattern", TRIM_PATTERN_HOLDER_770),
]);

pub(super) const INSTRUMENT_766: Node = Holder(
    "instrument",
    &Struct(&[("sound", SOUND), ("use_duration", VarInt), ("range", Float)]),
);
pub(super) const INSTRUMENT_768: Node = Holder(
    "instrument",
    &Struct(&[
        ("sound", SOUND),
        ("use_duration", Float),
        ("range", Float),
        ("description", Text),
    ]),
);
pub(super) const INSTRUMENT_777: Node = Holder(
    "instrument",
    &Struct(&[
        ("sound", SOUND),
        ("use_duration", Float),
        ("range", Float),
        ("durability_damage", VarInt),
        ("description", Text),
    ]),
);

pub(super) const JUKEBOX_SONG: Node = Holder(
    "jukebox_song",
    &Struct(&[
        ("sound", SOUND),
        ("description", Text),
        ("length_in_seconds", Float),
        ("comparator_output", VarInt),
    ]),
);
/// `EitherHolder.streamCodec`: a holder, or the entry's key by name.
pub(super) const JUKEBOX_SONG_EITHER: Node = Either(&JUKEBOX_SONG, &Identifier);
pub(super) const JUKEBOX_PLAYABLE_767: Node =
    Struct(&[("song", JUKEBOX_SONG_EITHER), ("show_in_tooltip", Bool)]);

/// `ByteBufCodecs.GAME_PROFILE_PROPERTIES`.
pub(super) const PROFILE_PROPERTIES: Node = List(
    &Struct(&[
        ("name", Str(64)),
        ("value", STRING),
        ("signature", Optional(&Str(1024))),
    ]),
    16,
);
pub(super) const PROFILE_766: Node = Struct(&[
    ("name", Optional(&Str(16))),
    ("id", Optional(&Uuid)),
    ("properties", PROFILE_PROPERTIES),
]);
/// `ResolvableProfile.STREAM_CODEC` from 1.21.9: a complete game profile or
/// a partial one, and a skin override.
pub(super) const PROFILE_773: Node = Struct(&[
    (
        "profile",
        Either(
            &Struct(&[
                ("id", Uuid),
                ("name", Str(16)),
                ("properties", PROFILE_PROPERTIES),
            ]),
            &PROFILE_766,
        ),
    ),
    (
        "skin_patch",
        Struct(&[
            ("body", Optional(&Identifier)),
            ("cape", Optional(&Identifier)),
            ("elytra", Optional(&Identifier)),
            ("model", Optional(&Bool)),
        ]),
    ),
]);

pub(super) const BANNER_PATTERNS: Node = List(
    &Struct(&[
        (
            "pattern",
            Holder(
                "banner_pattern",
                &Struct(&[("asset_id", Identifier), ("translation_key", STRING)]),
            ),
        ),
        ("color", VarInt),
    ]),
    ANY,
);

pub(super) const POT_DECORATIONS_766: Node = List(&Id("item"), 4);
pub(super) const POT_DECORATIONS_777: Node = Struct(&[
    ("back", Optional(&Template)),
    ("left", Optional(&Template)),
    ("right", Optional(&Template)),
    ("front", Optional(&Template)),
]);

pub(super) const BLOCK_STATE: Node = List(&Struct(&[("name", STRING), ("value", STRING)]), ANY);

/// `TypedEntityData.streamCodec` (1.21.9+): the type, then its data.
pub(super) const ENTITY_DATA_773: Node = Struct(&[("type", Id("entity_type")), ("data", Compound)]);
pub(super) const BLOCK_ENTITY_DATA_773: Node =
    Struct(&[("type", Id("block_entity_type")), ("data", Compound)]);
pub(super) const BEES_766: Node = List(
    &Struct(&[
        ("entity_data", Compound),
        ("ticks_in_hive", VarInt),
        ("min_ticks_in_hive", VarInt),
    ]),
    ANY,
);
pub(super) const BEES_773: Node = List(
    &Struct(&[
        ("entity_data", ENTITY_DATA_773),
        ("ticks_in_hive", VarInt),
        ("min_ticks_in_hive", VarInt),
    ]),
    ANY,
);

pub(super) const LODESTONE_TRACKER: Node = Struct(&[
    (
        "target",
        Optional(&Struct(&[("dimension", Identifier), ("pos", Position)])),
    ),
    ("tracked", Bool),
]);
pub(super) const FIREWORK_EXPLOSION: Node = Struct(&[
    ("shape", VarInt),
    ("colors", List(&Int, ANY)),
    ("fade_colors", List(&Int, ANY)),
    ("has_trail", Bool),
    ("has_twinkle", Bool),
]);
pub(super) const FIREWORKS: Node = Struct(&[
    ("flight_duration", VarInt),
    ("explosions", List(&FIREWORK_EXPLOSION, 256)),
]);

pub(super) const APPLY_EFFECTS: Node =
    Struct(&[("effects", List(&EFFECT, ANY)), ("probability", Float)]);
/// `ConsumeEffect.STREAM_CODEC`.
pub(super) const CONSUME_EFFECT_768: Node = Dispatch(
    "consume_effect_type",
    &[
        ("apply_effects", APPLY_EFFECTS),
        ("remove_effects", HolderSet("mob_effect")),
        ("clear_all_effects", Unit),
        ("teleport_randomly", Float),
        ("play_sound", SOUND),
    ],
);
/// 26.3 can turn the particles of a random teleport towards its target.
pub(super) const CONSUME_EFFECT_777: Node = Dispatch(
    "consume_effect_type",
    &[
        ("apply_effects", APPLY_EFFECTS),
        ("remove_effects", HolderSet("mob_effect")),
        ("clear_all_effects", Unit),
        (
            "teleport_randomly",
            Struct(&[("diameter", Float), ("directional_particles", Bool)]),
        ),
        ("play_sound", SOUND),
    ],
);
pub(super) const CONSUMABLE_768: Node = Struct(&[
    ("consume_seconds", Float),
    ("animation", Enum(ANIMATIONS_768)),
    ("sound", SOUND),
    ("has_consume_particles", Bool),
    ("on_consume_effects", List(&CONSUME_EFFECT_768, ANY)),
]);
pub(super) const CONSUMABLE_769: Node = Struct(&[
    ("consume_seconds", Float),
    ("animation", Enum(ANIMATIONS_769)),
    ("sound", SOUND),
    ("has_consume_particles", Bool),
    ("on_consume_effects", List(&CONSUME_EFFECT_768, ANY)),
]);
pub(super) const CONSUMABLE_774: Node = Struct(&[
    ("consume_seconds", Float),
    ("animation", Enum(ANIMATIONS_774)),
    ("sound", SOUND),
    ("has_consume_particles", Bool),
    ("on_consume_effects", List(&CONSUME_EFFECT_768, ANY)),
]);
pub(super) const CONSUMABLE_777: Node = Struct(&[
    ("consume_seconds", Float),
    ("animation", Enum(ANIMATIONS_774)),
    ("sound", SOUND),
    ("has_consume_particles", Bool),
    ("on_consume_effects", List(&CONSUME_EFFECT_777, ANY)),
]);

pub(super) const USE_COOLDOWN: Node = Struct(&[
    ("seconds", Float),
    ("cooldown_group", Optional(&Identifier)),
]);

pub(super) const EQUIPPABLE_768: Node = Struct(&[
    ("slot", Enum(SLOTS_768)),
    ("equip_sound", SOUND),
    ("asset_id", Optional(&Identifier)),
    ("camera_overlay", Optional(&Identifier)),
    ("allowed_entities", Optional(&HolderSet("entity_type"))),
    ("dispensable", Bool),
    ("swappable", Bool),
    ("damage_on_hurt", Bool),
]);
pub(super) const EQUIPPABLE_770: Node = Struct(&[
    ("slot", Enum(SLOTS_770)),
    ("equip_sound", SOUND),
    ("asset_id", Optional(&Identifier)),
    ("camera_overlay", Optional(&Identifier)),
    ("allowed_entities", Optional(&HolderSet("entity_type"))),
    ("dispensable", Bool),
    ("swappable", Bool),
    ("damage_on_hurt", Bool),
    ("equip_on_interact", Bool),
]);
pub(super) const EQUIPPABLE_771: Node = Struct(&[
    ("slot", Enum(SLOTS_770)),
    ("equip_sound", SOUND),
    ("asset_id", Optional(&Identifier)),
    ("camera_overlay", Optional(&Identifier)),
    ("allowed_entities", Optional(&HolderSet("entity_type"))),
    ("dispensable", Bool),
    ("swappable", Bool),
    ("damage_on_hurt", Bool),
    ("equip_on_interact", Bool),
    ("can_be_sheared", Bool),
    ("shearing_sound", SOUND),
]);

pub(super) const DEATH_PROTECTION_768: Node = List(&CONSUME_EFFECT_768, ANY);
pub(super) const DEATH_PROTECTION_777: Node = List(&CONSUME_EFFECT_777, ANY);

pub(super) const WEAPON: Node = Struct(&[
    ("item_damage_per_attack", VarInt),
    ("disable_blocking_for_seconds", Float),
]);

pub(super) const DAMAGE_REDUCTIONS: Node = List(
    &Struct(&[
        ("horizontal_blocking_angle", Float),
        ("type", Optional(&HolderSet("damage_type"))),
        ("base", Float),
        ("factor", Float),
    ]),
    ANY,
);
pub(super) const ITEM_DAMAGE_FUNCTION: Node =
    Struct(&[("threshold", Float), ("base", Float), ("factor", Float)]);
pub(super) const BLOCKS_ATTACKS_770: Node = Struct(&[
    ("block_delay_seconds", Float),
    ("disable_cooldown_scale", Float),
    ("damage_reductions", DAMAGE_REDUCTIONS),
    ("item_damage", ITEM_DAMAGE_FUNCTION),
    ("bypassed_by", Optional(&Identifier)),
    ("block_sound", Optional(&SOUND)),
    ("disable_sound", Optional(&SOUND)),
]);
/// 26.1 turned the tag key of `bypassed_by` into a holder set.
pub(super) const BLOCKS_ATTACKS_775: Node = Struct(&[
    ("block_delay_seconds", Float),
    ("disable_cooldown_scale", Float),
    ("damage_reductions", DAMAGE_REDUCTIONS),
    ("item_damage", ITEM_DAMAGE_FUNCTION),
    ("bypassed_by", Optional(&HolderSet("damage_type"))),
    ("block_sound", Optional(&SOUND)),
    ("disable_sound", Optional(&SOUND)),
]);

pub(super) const TOOLTIP_DISPLAY: Node = Struct(&[
    ("hide_tooltip", Bool),
    ("hidden_components", List(&Kind, ANY)),
]);

pub(super) const PAINTING_VARIANT: Node = Holder(
    "painting_variant",
    &Struct(&[
        ("width", VarInt),
        ("height", VarInt),
        ("asset_id", Identifier),
        ("title", Optional(&Text)),
        ("author", Optional(&Text)),
    ]),
);

pub(super) const USE_EFFECTS: Node = Struct(&[
    ("can_sprint", Bool),
    ("interact_vibrations", Bool),
    ("speed_multiplier", Float),
]);
pub(super) const ATTACK_RANGE: Node = Struct(&[
    ("min_reach", Float),
    ("max_reach", Float),
    ("min_creative_reach", Float),
    ("max_creative_reach", Float),
    ("hitbox_margin", Float),
    ("mob_factor", Float),
]);
pub(super) const PIERCING_WEAPON: Node = Struct(&[
    ("deals_knockback", Bool),
    ("dismounts", Bool),
    ("sound", Optional(&SOUND)),
    ("hit_sound", Optional(&SOUND)),
]);
pub(super) const KINETIC_CONDITION: Node = Optional(&Struct(&[
    ("max_duration_ticks", VarInt),
    ("min_speed", Float),
    ("min_relative_speed", Float),
]));
pub(super) const KINETIC_WEAPON: Node = Struct(&[
    ("contact_cooldown_ticks", VarInt),
    ("delay_ticks", VarInt),
    ("dismount_conditions", KINETIC_CONDITION),
    ("knockback_conditions", KINETIC_CONDITION),
    ("damage_conditions", KINETIC_CONDITION),
    ("forward_movement", Float),
    ("damage_multiplier", Float),
    ("sound", Optional(&SOUND)),
    ("hit_sound", Optional(&SOUND)),
]);
pub(super) const SWING_ANIMATION: Node = Struct(&[("type", VarInt), ("duration", VarInt)]);

/// `ResolvableInt` and `ResolvableFloat` (26.3): a constant or a reference
/// to a context provider by name.
pub(super) const RESOLVABLE_INT: Node = Either(&Int, &Identifier);
pub(super) const RESOLVABLE_FLOAT: Node = Either(&Float, &Identifier);
pub(super) const SIGN_TEXT: Node = Struct(&[
    ("messages", Array(&Text, 4)),
    ("filtered_messages", Optional(&Array(&Text, 4))),
    ("color", VarInt),
    ("has_glowing_text", Bool),
]);

/// `SlotDisplay.STREAM_CODEC` of 1.21.2 to 1.21.4: the smithing trim
/// display's pattern was itself a display.
pub(super) static SLOT_DISPLAY_768: Node = Dispatch(
    "slot_display",
    &[
        ("empty", Unit),
        ("any_fuel", Unit),
        ("item", Id("item")),
        ("item_stack", Stack),
        ("tag", Identifier),
        (
            "smithing_trim",
            Struct(&[
                ("base", Ref(&SLOT_DISPLAY_768)),
                ("material", Ref(&SLOT_DISPLAY_768)),
                ("pattern", Ref(&SLOT_DISPLAY_768)),
            ]),
        ),
        (
            "with_remainder",
            Struct(&[
                ("input", Ref(&SLOT_DISPLAY_768)),
                ("remainder", Ref(&SLOT_DISPLAY_768)),
            ]),
        ),
        ("composite", List(&Ref(&SLOT_DISPLAY_768), ANY)),
    ],
);
/// 1.21.5 to 1.21.11: the pattern became a trim pattern holder.
pub(super) static SLOT_DISPLAY_770: Node = Dispatch(
    "slot_display",
    &[
        ("empty", Unit),
        ("any_fuel", Unit),
        ("item", Id("item")),
        ("item_stack", Stack),
        ("tag", Identifier),
        (
            "smithing_trim",
            Struct(&[
                ("base", Ref(&SLOT_DISPLAY_770)),
                ("material", Ref(&SLOT_DISPLAY_770)),
                ("pattern", TRIM_PATTERN_HOLDER_770),
            ]),
        ),
        (
            "with_remainder",
            Struct(&[
                ("input", Ref(&SLOT_DISPLAY_770)),
                ("remainder", Ref(&SLOT_DISPLAY_770)),
            ]),
        ),
        ("composite", List(&Ref(&SLOT_DISPLAY_770), ANY)),
    ],
);
/// 26.1: stacks became templates, and three display types were added.
pub(super) static SLOT_DISPLAY_775: Node = Dispatch(
    "slot_display",
    &[
        ("empty", Unit),
        ("any_fuel", Unit),
        ("with_any_potion", Ref(&SLOT_DISPLAY_775)),
        (
            "only_with_component",
            Struct(&[("source", Ref(&SLOT_DISPLAY_775)), ("component", Kind)]),
        ),
        ("item", Id("item")),
        ("item_stack", Template),
        ("tag", Identifier),
        (
            "dyed",
            Struct(&[
                ("dye", Ref(&SLOT_DISPLAY_775)),
                ("target", Ref(&SLOT_DISPLAY_775)),
            ]),
        ),
        (
            "smithing_trim",
            Struct(&[
                ("base", Ref(&SLOT_DISPLAY_775)),
                ("material", Ref(&SLOT_DISPLAY_775)),
                ("pattern", TRIM_PATTERN_HOLDER_770),
            ]),
        ),
        (
            "with_remainder",
            Struct(&[
                ("input", Ref(&SLOT_DISPLAY_775)),
                ("remainder", Ref(&SLOT_DISPLAY_775)),
            ]),
        ),
        ("composite", List(&Ref(&SLOT_DISPLAY_775), ANY)),
    ],
);
/// 26.3: a tag display holds a holder set.
pub(super) static SLOT_DISPLAY_777: Node = Dispatch(
    "slot_display",
    &[
        ("empty", Unit),
        ("any_fuel", Unit),
        ("with_any_potion", Ref(&SLOT_DISPLAY_777)),
        (
            "only_with_component",
            Struct(&[("source", Ref(&SLOT_DISPLAY_777)), ("component", Kind)]),
        ),
        ("item", Id("item")),
        ("item_stack", Template),
        ("tag", HolderSet("item")),
        (
            "dyed",
            Struct(&[
                ("dye", Ref(&SLOT_DISPLAY_777)),
                ("target", Ref(&SLOT_DISPLAY_777)),
            ]),
        ),
        (
            "smithing_trim",
            Struct(&[
                ("base", Ref(&SLOT_DISPLAY_777)),
                ("material", Ref(&SLOT_DISPLAY_777)),
                ("pattern", TRIM_PATTERN_HOLDER_770),
            ]),
        ),
        (
            "with_remainder",
            Struct(&[
                ("input", Ref(&SLOT_DISPLAY_777)),
                ("remainder", Ref(&SLOT_DISPLAY_777)),
            ]),
        ),
        ("composite", List(&Ref(&SLOT_DISPLAY_777), ANY)),
    ],
);

/// `RecipeDisplay.STREAM_CODEC` around one protocol's slot display.
macro_rules! recipe_display {
    ($name:ident, $slot:ident) => {
        pub(super) static $name: Node = Dispatch(
            "recipe_display",
            &[
                (
                    "crafting_shapeless",
                    Struct(&[
                        ("ingredients", List(&Ref(&$slot), ANY)),
                        ("result", Ref(&$slot)),
                        ("crafting_station", Ref(&$slot)),
                    ]),
                ),
                (
                    "crafting_shaped",
                    Struct(&[
                        ("width", VarInt),
                        ("height", VarInt),
                        ("ingredients", List(&Ref(&$slot), ANY)),
                        ("result", Ref(&$slot)),
                        ("crafting_station", Ref(&$slot)),
                    ]),
                ),
                (
                    "furnace",
                    Struct(&[
                        ("ingredient", Ref(&$slot)),
                        ("fuel", Ref(&$slot)),
                        ("result", Ref(&$slot)),
                        ("crafting_station", Ref(&$slot)),
                        ("duration", VarInt),
                        ("experience", Float),
                    ]),
                ),
                (
                    "stonecutter",
                    Struct(&[
                        ("input", Ref(&$slot)),
                        ("result", Ref(&$slot)),
                        ("crafting_station", Ref(&$slot)),
                    ]),
                ),
                (
                    "smithing",
                    Struct(&[
                        ("template", Ref(&$slot)),
                        ("base", Ref(&$slot)),
                        ("addition", Ref(&$slot)),
                        ("result", Ref(&$slot)),
                        ("crafting_station", Ref(&$slot)),
                    ]),
                ),
            ],
        );
    };
}
recipe_display!(RECIPE_DISPLAY_768, SLOT_DISPLAY_768);
recipe_display!(RECIPE_DISPLAY_770, SLOT_DISPLAY_770);
recipe_display!(RECIPE_DISPLAY_775, SLOT_DISPLAY_775);
recipe_display!(RECIPE_DISPLAY_777, SLOT_DISPLAY_777);

/// The layouts of one component type, each valid from a protocol on; `None`
/// where the component no longer exists is expressed by the registry, not
/// here.
macro_rules! def {
    ($name:literal: $($from:literal => $node:expr),+ $(,)?) => {
        Def {
            name: $name,
            layouts: &[$(($from, &$node)),+],
        }
    };
}

/// Every data component type of every protocol from 766 on.
pub(super) static DEFS: &[Def] = &[
    def!("custom_data": 766 => Compound),
    def!("max_stack_size": 766 => VarInt),
    def!("max_damage": 766 => VarInt),
    def!("damage": 766 => VarInt),
    def!("unbreakable": 766 => Bool, 770 => Unit),
    def!("use_effects": 774 => USE_EFFECTS),
    def!("custom_name": 766 => Text),
    def!("minimum_attack_charge": 774 => Float),
    def!("damage_type": 774 => Either(&Id("damage_type"), &Identifier), 775 => Id("damage_type")),
    def!("item_name": 766 => Text),
    def!("item_model": 768 => Identifier),
    def!("lore": 766 => List(&Text, 256)),
    def!("rarity": 766 => VarInt),
    def!("enchantments": 766 => ENCHANTMENTS_766, 770 => ENCHANTMENT_LEVELS),
    def!("can_place_on": 766 => ADVENTURE_766, 770 => ADVENTURE_770, 774 => ADVENTURE_774),
    def!("can_break": 766 => ADVENTURE_766, 770 => ADVENTURE_770, 774 => ADVENTURE_774),
    def!(
        "attribute_modifiers":
        766 => ATTRIBUTE_MODIFIERS_766,
        767 => ATTRIBUTE_MODIFIERS_767,
        770 => ATTRIBUTE_MODIFIERS_770,
        771 => ATTRIBUTE_MODIFIERS_771,
    ),
    def!("custom_model_data": 766 => VarInt, 769 => CUSTOM_MODEL_DATA_769),
    def!("hide_additional_tooltip": 766 => Unit),
    def!("hide_tooltip": 766 => Unit),
    def!("tooltip_display": 770 => TOOLTIP_DISPLAY),
    def!("repair_cost": 766 => VarInt),
    def!("creative_slot_lock": 766 => Unit),
    def!("enchantment_glint_override": 766 => Bool),
    def!("intangible_projectile": 766 => Tag),
    def!("food": 766 => FOOD_766, 767 => FOOD_767, 768 => FOOD_768),
    def!("fire_resistant": 766 => Unit),
    def!(
        "consumable":
        768 => CONSUMABLE_768,
        769 => CONSUMABLE_769,
        774 => CONSUMABLE_774,
        777 => CONSUMABLE_777,
    ),
    def!("use_remainder": 768 => Stack, 775 => Template),
    def!("use_cooldown": 768 => USE_COOLDOWN),
    def!("damage_resistant": 768 => Identifier, 775 => HolderSet("damage_type")),
    def!("tool": 766 => TOOL_766, 770 => TOOL_770),
    def!("weapon": 770 => WEAPON),
    def!("attack_range": 774 => ATTACK_RANGE),
    def!("enchantable": 768 => VarInt),
    def!("equippable": 768 => EQUIPPABLE_768, 770 => EQUIPPABLE_770, 771 => EQUIPPABLE_771),
    def!("repairable": 768 => HolderSet("item")),
    def!("glider": 768 => Unit),
    def!("tooltip_style": 768 => Identifier),
    def!("death_protection": 768 => DEATH_PROTECTION_768, 777 => DEATH_PROTECTION_777),
    def!("blocks_attacks": 770 => BLOCKS_ATTACKS_770, 775 => BLOCKS_ATTACKS_775),
    def!("piercing_weapon": 774 => PIERCING_WEAPON),
    def!("kinetic_weapon": 774 => KINETIC_WEAPON),
    def!("swing_animation": 774 => SWING_ANIMATION),
    def!("attack_animation": 777 => SWING_ANIMATION),
    def!("interact_animation": 777 => SWING_ANIMATION),
    def!("additional_trade_cost": 775 => VarInt),
    def!("block_transformer": 777 => Id("block_transformer")),
    def!("villager_food": 777 => Struct(&[("nutrition", VarInt)])),
    def!("stored_enchantments": 766 => ENCHANTMENTS_766, 770 => ENCHANTMENT_LEVELS),
    def!("dye": 775 => VarInt),
    def!("dyed_color": 766 => DYED_COLOR_766, 770 => Int),
    def!("map_color": 766 => Int),
    def!("map_id": 766 => VarInt),
    def!("map_decorations": 766 => Tag),
    def!("map_post_processing": 766 => VarInt),
    def!("charged_projectiles": 766 => List(&Stack, ANY), 775 => List(&Template, ANY), 776 => List(&Template, 1024)),
    def!("bundle_contents": 766 => List(&Stack, ANY), 775 => List(&Template, ANY)),
    def!("potion_contents": 766 => POTION_CONTENTS_766, 768 => POTION_CONTENTS_768),
    def!("potion_duration_scale": 770 => Float),
    def!("suspicious_stew_effects": 766 => SUSPICIOUS_STEW),
    def!("writable_book_content": 766 => WRITABLE_BOOK),
    def!("written_book_content": 766 => WRITTEN_BOOK),
    def!("trim": 766 => TRIM_766, 768 => TRIM_768, 769 => TRIM_769, 770 => TRIM_770, 777 => TRIM_777),
    def!("debug_stick_state": 766 => Tag),
    def!("entity_data": 766 => Compound, 773 => ENTITY_DATA_773),
    def!("bucket_entity_data": 766 => Compound),
    def!("block_entity_data": 766 => Compound, 773 => BLOCK_ENTITY_DATA_773),
    def!(
        "instrument":
        766 => INSTRUMENT_766,
        768 => INSTRUMENT_768,
        770 => Either(&INSTRUMENT_768, &Identifier),
        775 => INSTRUMENT_768,
        777 => INSTRUMENT_777,
    ),
    def!(
        "provides_trim_material":
        770 => Either(&TRIM_MATERIAL_HOLDER_770, &Identifier),
        775 => TRIM_MATERIAL_HOLDER_770,
        777 => Holder("trim_material", &TRIM_MATERIAL_777),
    ),
    def!("ominous_bottle_amplifier": 766 => VarInt),
    def!(
        "jukebox_playable":
        767 => JUKEBOX_PLAYABLE_767,
        770 => JUKEBOX_SONG_EITHER,
        775 => JUKEBOX_SONG,
    ),
    def!("provides_banner_patterns": 770 => Identifier, 775 => HolderSet("banner_pattern")),
    def!("recipes": 766 => Tag),
    def!("lodestone_tracker": 766 => LODESTONE_TRACKER),
    def!("firework_explosion": 766 => FIREWORK_EXPLOSION),
    def!("fireworks": 766 => FIREWORKS),
    def!("profile": 766 => PROFILE_766, 773 => PROFILE_773),
    def!("note_block_sound": 766 => Identifier),
    def!("banner_patterns": 766 => BANNER_PATTERNS),
    def!("base_color": 766 => VarInt),
    def!("pot_decorations": 766 => POT_DECORATIONS_766, 777 => POT_DECORATIONS_777),
    def!("container": 766 => List(&Stack, 256), 775 => List(&Optional(&Template), 256)),
    def!("block_state": 766 => BLOCK_STATE),
    def!("bees": 766 => BEES_766, 773 => BEES_773),
    def!("sulfur_cube_content": 776 => Template),
    def!("lock": 766 => Tag),
    def!("container_loot": 766 => Tag),
    def!("break_sound": 770 => SOUND),
    def!(
        "compostable":
        777 => Struct(&[("layers", RESOLVABLE_INT)]),
    ),
    def!(
        "cooking_fuel":
        777 => Struct(&[("burn_time", RESOLVABLE_INT), ("speed_multiplier", RESOLVABLE_FLOAT)]),
    ),
    def!(
        "brewing_fuel":
        777 => Struct(&[("uses", RESOLVABLE_INT), ("speed_multiplier", RESOLVABLE_FLOAT)]),
    ),
    def!(
        "mob_visibility":
        777 => Struct(&[("targeting_entity_types", HolderSet("entity_type")), ("visibility", Float)]),
    ),
    def!("villager/variant": 770 => Id("villager_type")),
    def!("wolf/variant": 770 => Id("wolf_variant")),
    def!("wolf/sound_variant": 770 => Id("wolf_sound_variant")),
    def!("wolf/collar": 770 => VarInt),
    def!("fox/variant": 770 => VarInt),
    def!("salmon/size": 770 => VarInt),
    def!("parrot/variant": 770 => VarInt),
    def!("tropical_fish/pattern": 770 => VarInt),
    def!("tropical_fish/base_color": 770 => VarInt),
    def!("tropical_fish/pattern_color": 770 => VarInt),
    def!("mooshroom/variant": 770 => VarInt),
    def!("rabbit/variant": 770 => VarInt),
    def!("pig/variant": 770 => Id("pig_variant")),
    def!("pig/sound_variant": 775 => Id("pig_sound_variant")),
    def!("cow/variant": 770 => Id("cow_variant")),
    def!("cow/sound_variant": 775 => Id("cow_sound_variant")),
    def!(
        "chicken/variant":
        770 => Either(&Id("chicken_variant"), &Identifier),
        775 => Id("chicken_variant"),
    ),
    def!("chicken/sound_variant": 775 => Id("chicken_sound_variant")),
    def!(
        "zombie_nautilus/variant":
        774 => Either(&Id("zombie_nautilus_variant"), &Identifier),
        775 => Id("zombie_nautilus_variant"),
    ),
    def!("frog/variant": 770 => Id("frog_variant")),
    def!("horse/variant": 770 => VarInt),
    def!("painting/variant": 770 => PAINTING_VARIANT),
    def!("llama/variant": 770 => VarInt),
    def!("axolotl/variant": 770 => VarInt),
    def!("cat/variant": 770 => Id("cat_variant")),
    def!("cat/sound_variant": 775 => Id("cat_sound_variant")),
    def!("cat/collar": 770 => VarInt),
    def!("sheep/color": 770 => VarInt),
    def!("shulker/color": 770 => VarInt),
    def!("provides_pottery_pattern": 777 => Id("decorated_pot_pattern")),
    def!("sign_text_front": 777 => SIGN_TEXT),
    def!("sign_text_back": 777 => SIGN_TEXT),
    def!("waxed": 777 => Unit),
    def!("cushion/color": 777 => VarInt),
];

/// One component type and its layouts, each valid from a protocol on.
pub struct Def {
    pub name: &'static str,
    pub layouts: &'static [(i32, &'static Node)],
}

impl Def {
    /// The layout in `protocol`, if the type has one there.
    pub fn layout(&self, protocol: i32) -> Option<&'static Node> {
        self.layouts
            .iter()
            .rev()
            .find(|(from, _)| *from <= protocol)
            .map(|(_, node)| *node)
    }
}

/// The component type `name`'s description.
pub fn def(name: &str) -> Option<&'static Def> {
    DEFS.iter().find(|def| def.name == name)
}

/// One protocol's component types by ID, with their layouts.
pub struct Layout {
    pub protocol: i32,
    by_id: Vec<Option<(&'static str, &'static Node)>>,
    by_name: HashMap<&'static str, i32>,
}

/// The first protocol with data components.
pub const FIRST: i32 = 766;

impl Layout {
    /// The layout of `protocol` (766 or newer), built on first use.
    pub fn of(protocol: i32) -> io::Result<&'static Layout> {
        static LAYOUTS: OnceLock<HashMap<i32, Layout>> = OnceLock::new();
        LAYOUTS
            .get_or_init(|| {
                crate::version::PROTOCOLS
                    .iter()
                    .filter(|(protocol, _)| *protocol >= FIRST)
                    .map(|(protocol, _)| (*protocol, Layout::build(*protocol)))
                    .collect()
            })
            .get(&protocol)
            .ok_or_else(|| invalid(&format!("protocol {protocol} has no data components")))
    }

    fn build(protocol: i32) -> Layout {
        let registry = data::version(protocol).expect_registry("data_component_type");
        let mut by_id = Vec::with_capacity(registry.len());
        let mut by_name = HashMap::new();
        for id in 0..registry.len() as i32 {
            let entry = registry
                .name(id)
                .and_then(def)
                .and_then(|def| Some((def.name, def.layout(protocol)?)));
            if let Some((name, _)) = entry {
                by_name.insert(name, id);
            }
            by_id.push(entry);
        }
        Layout {
            protocol,
            by_id,
            by_name,
        }
    }

    /// The type with `id`, and its layout.
    pub fn get(&self, id: i32) -> Option<(&'static str, &'static Node)> {
        *self.by_id.get(usize::try_from(id).ok()?)?
    }

    pub fn id(&self, name: &str) -> Option<i32> {
        self.by_name.get(name).copied()
    }

    /// The ID and layout of the type `name`.
    pub fn find(&self, name: &str) -> Option<(i32, &'static Node)> {
        let id = self.id(name)?;
        Some((id, self.get(id)?.1))
    }

    /// Registry entries without a layout; empty for a complete table.
    pub fn missing(&self) -> Vec<String> {
        let registry = data::version(self.protocol).expect_registry("data_component_type");
        (0..registry.len() as i32)
            .filter(|id| self.get(*id).is_none())
            .filter_map(|id| registry.name(id).map(str::to_owned))
            .collect()
    }
}
