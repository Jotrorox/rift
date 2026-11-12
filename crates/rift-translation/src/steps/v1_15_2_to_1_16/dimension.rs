//! Dimensions as 1.15 numbers them and as 1.16 names them. The dimension
//! types a 1.16 backend declares are kept by the registries codec; this
//! module only chooses what a 1.15 client is shown.
use crate::{
    codec::{
        Shared,
        registries::{self, Side, login},
    },
    nbt::Compound,
};

/// The dimension codec a vanilla 1.16 server sends in `login`, as recorded
/// from the official server: overworld, overworld caves, the nether and the
/// end. A named root compound.
#[cfg(test)]
pub(in crate::steps) const RECORDED_CODEC: &[u8] = include_bytes!("dimension_codec.nbt");

/// The worlds of a vanilla server, which 1.16 lists in `login`.
pub(super) const VANILLA_WORLDS: [&str; 3] = [
    "minecraft:overworld",
    "minecraft:the_nether",
    "minecraft:the_end",
];

/// 1.15's dimension IDs.
pub(super) const NETHER: i32 = -1;
pub(super) const OVERWORLD: i32 = 0;
pub(super) const END: i32 = 1;

/// The level type a 1.15 client shows for a 1.16 world's flags.
pub(super) fn level_type(debug: bool, flat: bool) -> &'static str {
    if debug {
        "debug_all_block_states"
    } else if flat {
        "flat"
    } else {
        "default"
    }
}

/// A 1.15 level type as 1.16's debug and flat flags.
pub(super) fn flags(level_type: &str) -> (bool, bool) {
    let level_type = level_type.to_ascii_lowercase();
    (level_type == "debug_all_block_states", level_type == "flat")
}

/// The 1.15 dimension that the backend's dimension type `name` looks most
/// like. 1.15 clients know only the three vanilla dimensions; a custom type
/// that is hot or roofed without sky gets the nether's look, one without sky
/// or roof the end's.
pub(super) fn legacy(shared: &Shared, name: &str) -> i32 {
    match login::legacy_id(Some(name), None) {
        OVERWORLD => {}
        vanilla => return vanilla,
    }
    let element: Option<&Compound> =
        registries::element(shared, Side::Backend, "dimension_type", name);
    let Some(element) = element else {
        return OVERWORLD;
    };
    let flag = |key: &str| element.int(key).is_some_and(|value| value != 0);
    match crate::data::strip(name) {
        "overworld" | "overworld_caves" => OVERWORLD,
        _ if flag("ultrawarm") || (flag("has_ceiling") && !flag("has_skylight")) => NETHER,
        _ if !flag("has_skylight") => END,
        _ => OVERWORLD,
    }
}
