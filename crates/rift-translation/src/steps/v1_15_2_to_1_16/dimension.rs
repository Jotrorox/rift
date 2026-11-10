//! Dimensions as 1.15 numbers them and as 1.16 names them.
//!
//! INTEGRATION: the registries codec is to provide the dimension parts of
//! `login` and `respawn`; until it does, this module converts them itself.
use crate::{
    nbt::Nbt,
    wire::{Reader, invalid},
};
use std::io;

/// The dimension codec a vanilla 1.16 server sends in `login`, as recorded
/// from the official server: overworld, overworld caves, the nether and the
/// end. A named root compound.
pub(in crate::steps) const VANILLA_CODEC: &[u8] = include_bytes!("dimension_codec.nbt");

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

/// The dimension type, and world, of a 1.15 dimension ID. 1.15 clients know
/// only the three vanilla dimensions; anything else is treated as the
/// overworld.
pub(super) fn identifier(legacy: i32) -> &'static str {
    match legacy {
        NETHER => VANILLA_WORLDS[1],
        END => VANILLA_WORLDS[2],
        _ => VANILLA_WORLDS[0],
    }
}

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

/// The dimension types a 1.16 backend declared in its codec, with the 1.15
/// dimension each one looks most like.
#[derive(Debug, Default)]
pub(super) struct Types {
    types: Vec<(String, i32)>,
}

impl Types {
    /// Read the codec (a named root compound) from `reader`.
    pub(super) fn read(reader: &mut Reader<'_>) -> io::Result<Self> {
        let codec = Nbt::read(reader, true)?.ok_or_else(|| invalid("missing dimension codec"))?;
        let list = codec
            .as_compound()
            .and_then(|codec| codec.list("dimension"))
            .ok_or_else(|| invalid("dimension codec without dimensions"))?;
        let mut types = Vec::with_capacity(list.len());
        for entry in list {
            let entry = entry
                .as_compound()
                .ok_or_else(|| invalid("dimension type is not a compound"))?;
            let name = entry
                .str("name")
                .ok_or_else(|| invalid("dimension type without a name"))?;
            let flag = |key: &str| entry.int(key).is_some_and(|value| value != 0);
            let legacy = match name {
                "minecraft:overworld" | "minecraft:overworld_caves" => OVERWORLD,
                "minecraft:the_nether" => NETHER,
                "minecraft:the_end" => END,
                // Custom types: hot or roofed without sky is the nether's
                // look, skyless without a roof the end's.
                _ if flag("ultrawarm") || (flag("has_ceiling") && !flag("has_skylight")) => NETHER,
                _ if !flag("has_skylight") => END,
                _ => OVERWORLD,
            };
            types.push((name.to_owned(), legacy));
        }
        Ok(Self { types })
    }

    /// The 1.15 dimension for the dimension type `name`.
    pub(super) fn legacy(&self, name: &str) -> i32 {
        let name = if name.contains(':') {
            name.to_owned()
        } else {
            format!("minecraft:{name}")
        };
        self.types
            .iter()
            .find(|(known, _)| *known == name)
            .map_or_else(
                || match name.as_str() {
                    "minecraft:the_nether" => NETHER,
                    "minecraft:the_end" => END,
                    _ => OVERWORLD,
                },
                |(_, legacy)| *legacy,
            )
    }
}
