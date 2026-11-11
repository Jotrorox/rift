//! Fixed names on both sides of the bridge: values 1.20.4 tags hold as
//! numbers or short codes and 1.20.5 by name or enum index.
use crate::data;

/// `HideFlags` bits.
pub const HIDE_ENCHANTMENTS: i32 = 1;
pub const HIDE_MODIFIERS: i32 = 2;
pub const HIDE_UNBREAKABLE: i32 = 4;
pub const HIDE_CAN_DESTROY: i32 = 8;
pub const HIDE_CAN_PLACE: i32 = 16;
pub const HIDE_ADDITIONAL: i32 = 32;
pub const HIDE_DYE: i32 = 64;
pub const HIDE_UPGRADES: i32 = 128;

/// Banner pattern short codes of 1.20.4 tags and their 1.20.5 names.
pub const BANNER_PATTERNS: &[(&str, &str)] = &[
    ("b", "base"),
    ("bl", "square_bottom_left"),
    ("br", "square_bottom_right"),
    ("tl", "square_top_left"),
    ("tr", "square_top_right"),
    ("bs", "stripe_bottom"),
    ("ts", "stripe_top"),
    ("ls", "stripe_left"),
    ("rs", "stripe_right"),
    ("cs", "stripe_center"),
    ("ms", "stripe_middle"),
    ("drs", "stripe_downright"),
    ("dls", "stripe_downleft"),
    ("ss", "small_stripes"),
    ("cr", "cross"),
    ("sc", "straight_cross"),
    ("bt", "triangle_bottom"),
    ("tt", "triangle_top"),
    ("bts", "triangles_bottom"),
    ("tts", "triangles_top"),
    ("ld", "diagonal_left"),
    ("rd", "diagonal_up_right"),
    ("lud", "diagonal_up_left"),
    ("rud", "diagonal_right"),
    ("mc", "circle"),
    ("mr", "rhombus"),
    ("vh", "half_vertical"),
    ("hh", "half_horizontal"),
    ("vhr", "half_vertical_right"),
    ("hhb", "half_horizontal_bottom"),
    ("bo", "border"),
    ("cbo", "curly_border"),
    ("gra", "gradient"),
    ("gru", "gradient_up"),
    ("bri", "bricks"),
    ("glb", "globe"),
    ("cre", "creeper"),
    ("sku", "skull"),
    ("flo", "flower"),
    ("moj", "mojang"),
    ("pig", "piglin"),
];

pub fn pattern_name(code: &str) -> Option<&'static str> {
    BANNER_PATTERNS
        .iter()
        .find(|(known, _)| *known == code)
        .map(|(_, name)| *name)
}

pub fn pattern_code(name: &str) -> Option<&'static str> {
    let name = data::strip(name);
    BANNER_PATTERNS
        .iter()
        .find(|(_, known)| *known == name)
        .map(|(code, _)| *code)
}

/// Map decoration types: their 1.20.4 numbers are positions here.
pub const MAP_DECORATIONS: &[&str] = &[
    "player",
    "frame",
    "red_marker",
    "blue_marker",
    "target_x",
    "target_point",
    "player_off_map",
    "player_off_limits",
    "mansion",
    "monument",
    "banner_white",
    "banner_orange",
    "banner_magenta",
    "banner_light_blue",
    "banner_yellow",
    "banner_lime",
    "banner_pink",
    "banner_gray",
    "banner_light_gray",
    "banner_cyan",
    "banner_purple",
    "banner_blue",
    "banner_brown",
    "banner_green",
    "banner_red",
    "banner_black",
    "red_x",
    "village_desert",
    "village_plains",
    "village_savanna",
    "village_snowy",
    "village_taiga",
    "jungle_temple",
    "swamp_hut",
];

/// 1.20.5's `EquipmentSlotGroup` names that a 1.20.4 `Slot` can name.
pub const MODIFIER_SLOTS: &[&str] = &["mainhand", "offhand", "feet", "legs", "chest", "head"];

/// Tags of bucketed mobs that 1.20.5 keeps in `bucket_entity_data`.
pub const BUCKETED_MOB_TAGS: &[&str] = &[
    "NoAI",
    "Silent",
    "NoGravity",
    "Glowing",
    "Invulnerable",
    "Health",
    "Age",
    "Variant",
    "HuntingCooldown",
    "BucketVariantTag",
];

pub const BUCKETS: &[&str] = &[
    "pufferfish_bucket",
    "salmon_bucket",
    "cod_bucket",
    "tropical_fish_bucket",
    "axolotl_bucket",
    "tadpole_bucket",
];

pub const POTION_HOLDERS: &[&str] = &[
    "potion",
    "splash_potion",
    "lingering_potion",
    "tipped_arrow",
];

/// Block state properties whose 1.20.4 `BlockStateTag` values may be bytes
/// standing for booleans.
pub const BOOLEAN_PROPERTIES: &[&str] = &[
    "attached",
    "bottom",
    "conditional",
    "disarmed",
    "drag",
    "enabled",
    "extended",
    "eye",
    "falling",
    "hanging",
    "has_bottle_0",
    "has_bottle_1",
    "has_bottle_2",
    "has_record",
    "has_book",
    "inverted",
    "in_wall",
    "lit",
    "locked",
    "occupied",
    "open",
    "persistent",
    "powered",
    "short",
    "signal_fire",
    "snowy",
    "triggered",
    "unstable",
    "waterlogged",
    "berries",
    "bloom",
    "shrieking",
    "can_summon",
    "up",
    "down",
    "north",
    "east",
    "south",
    "west",
    "slot_0_occupied",
    "slot_1_occupied",
    "slot_2_occupied",
    "slot_3_occupied",
    "slot_4_occupied",
    "slot_5_occupied",
    "cracked",
    "crafting",
];

/// What a block entity item's `BlockEntityTag` holds beyond the generic
/// keys, by the block entity its item places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockEntity {
    Skull,
    DecoratedPot,
    Banner,
    Container,
    Beehive,
    Other,
}

/// Block entities whose `Items` 1.20.5 keeps in `container`.
const CONTAINERS: &[&str] = &[
    "shulker_box",
    "chest",
    "trapped_chest",
    "furnace",
    "ender_chest",
    "dispenser",
    "dropper",
    "brewing_stand",
    "hopper",
    "barrel",
    "smoker",
    "blast_furnace",
    "campfire",
    "chiseled_bookshelf",
    "crafter",
];

pub fn block_entity_kind(block_entity: &str) -> BlockEntity {
    match data::strip(block_entity) {
        "skull" => BlockEntity::Skull,
        "decorated_pot" => BlockEntity::DecoratedPot,
        "banner" => BlockEntity::Banner,
        "beehive" => BlockEntity::Beehive,
        other if CONTAINERS.contains(&other) => BlockEntity::Container,
        _ => BlockEntity::Other,
    }
}

/// The block entity type an item's block has (vanilla's
/// `ITEM_TO_BLOCKENTITY` for the stacks it converts), by item name;
/// `None` for items whose blocks have none.
pub fn block_entity_of(item: &str) -> Option<&'static str> {
    let item = data::strip(item);
    let registry = data::version(super::NEW).registry("block_entity_type")?;
    let by_suffix = [
        ("_shulker_box", "shulker_box"),
        ("_hanging_sign", "hanging_sign"),
        ("_sign", "sign"),
        ("_banner", "banner"),
        ("_bed", "bed"),
        ("_skull", "skull"),
        ("_head", "skull"),
        ("command_block", "command_block"),
        ("campfire", "campfire"),
    ];
    let name = match item {
        "shield" => "banner",
        "bee_nest" => "beehive",
        "spawner" => "mob_spawner",
        "daylight_detector" => "daylight_detector",
        _ => by_suffix
            .iter()
            .find(|(suffix, _)| item.ends_with(suffix))
            .map_or(item, |(_, name)| *name),
    };
    registry.id(name).and_then(|id| registry.name(id))
}

/// The block position packing of 1.14 and newer.
pub fn pack_position(x: i32, y: i32, z: i32) -> i64 {
    ((i64::from(x) & 0x3ff_ffff) << 38)
        | ((i64::from(z) & 0x3ff_ffff) << 12)
        | (i64::from(y) & 0xfff)
}

pub fn unpack_position(packed: i64) -> (i32, i32, i32) {
    let x = (packed >> 38) as i32;
    let y = (packed << 52 >> 52) as i32;
    let z = (packed << 26 >> 38) as i32;
    (x, y, z)
}

/// The maximum damage of a 1.20.5 item by default; `None` for items that
/// do not wear out.
pub fn max_damage(item: &str) -> Option<i32> {
    data::file("items/766")?
        .get("max_damage")?
        .get(data::strip(item))?
        .as_i64()
        .map(|value| value as i32)
}

/// The prototype attribute modifiers of a 1.20.5 item, as data rows
/// `[attribute, uuid, name, amount, operation, slot]`; empty for items whose
/// prototype sets none.
pub fn default_modifiers(item: &str) -> &'static [serde_json::Value] {
    data::file("items/766")
        .and_then(|file| {
            file.get("attribute_modifiers")?
                .get(data::strip(item))?
                .as_array()
        })
        .map_or(&[], Vec::as_slice)
}

/// The entity an item places when its `EntityTag` names none.
pub fn entity_of(item: &str) -> Option<&'static str> {
    let item = data::strip(item);
    let name = match item {
        "armor_stand" | "item_frame" | "glow_item_frame" | "painting" => item,
        _ => item.strip_suffix("_spawn_egg")?,
    };
    let registry = data::version(super::NEW).registry("entity_type")?;
    registry.id(name).and_then(|id| registry.name(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_pack_like_vanilla() {
        for (x, y, z) in [(1, 2, 3), (-30_000_000, -64, 29_999_999), (0, 319, -1)] {
            assert_eq!(unpack_position(pack_position(x, y, z)), (x, y, z));
        }
        assert_eq!(pack_position(1, 2, 3), 274_877_919_234);
    }

    #[test]
    fn block_entities_follow_their_items() {
        assert_eq!(block_entity_of("purple_shulker_box"), Some("shulker_box"));
        assert_eq!(block_entity_of("shield"), Some("banner"));
        assert_eq!(block_entity_of("player_head"), Some("skull"));
        assert_eq!(block_entity_of("oak_hanging_sign"), Some("hanging_sign"));
        assert_eq!(block_entity_of("chest"), Some("chest"));
        assert_eq!(block_entity_of("stone"), None);
        assert_eq!(max_damage("diamond_sword"), Some(1561));
        assert_eq!(max_damage("stone"), None);
        assert_eq!(default_modifiers("diamond_sword").len(), 2);
        assert!(default_modifiers("diamond_chestplate").is_empty());
        assert_eq!(entity_of("mooshroom_spawn_egg"), Some("mooshroom"));
        assert_eq!(entity_of("armor_stand"), Some("armor_stand"));
        assert_eq!(entity_of("stick"), None);
    }
}
