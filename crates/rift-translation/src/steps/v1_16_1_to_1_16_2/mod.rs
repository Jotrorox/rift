//! Minecraft 1.16.1 (protocol 736) and 1.16.2 (protocol 751).
//!
//! What changes for this step's own packets:
//!
//! - `login` and `respawn` describe the dimension type by its full NBT
//!   element instead of its name; `login`'s codec becomes a set of
//!   registries (dimension types and biomes), the hardcore flag leaves the
//!   game mode and max players becomes a VarInt.
//! - `chunk_blocks_update` (a chunk column, absolute heights) becomes
//!   `section_blocks_update` (one section, packed VarLong records).
//! - Serverbound `recipe_book_update` splits into
//!   `recipe_book_change_settings` (one book at a time) and
//!   `recipe_book_seen_recipe`.
//!
//! Chunk biomes, the recipe book's new flags in `recipe`, piglin entity data
//! and the rest are the shared codecs' part.
mod dimension;
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;

pub const OLDER: i32 = 736;
pub const NEWER: i32 = 751;

/// A 1.16.2 client on a 1.16.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.16.1 client on a 1.16.2 backend.
pub const DOWN: Option<Make> = down::STEP;

/// The game mode byte's hardcore bit before 1.16.2.
const HARDCORE: u8 = 0x08;

/// `recipe_book_update` types.
const SHOWN: i32 = 0;
const SETTINGS: i32 = 1;
/// Crafting, furnace, blast furnace, smoker.
const BOOKS: i32 = 4;

/// One block change: section-relative position and block state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Change {
    x: u8,
    y: u8,
    z: u8,
    state: i32,
}

/// A section position packed as 1.16.2 does: x and z 22 bits, y 20 bits.
fn pack_section(x: i32, y: i32, z: i32) -> i64 {
    (i64::from(x) & 0x3f_ffff) << 42 | (i64::from(z) & 0x3f_ffff) << 20 | (i64::from(y) & 0xf_ffff)
}

fn unpack_section(packed: i64) -> (i32, i32, i32) {
    (
        (packed >> 42) as i32,
        (packed << 44 >> 44) as i32,
        (packed << 22 >> 42) as i32,
    )
}
