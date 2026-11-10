//! Minecraft 1.16.1 (protocol 736) and 1.16.2 (protocol 751).
//!
//! What changes for this step's own packets:
//!
//! - `login` and `respawn` describe the dimension type by its full NBT
//!   element instead of its name; `login`'s codec becomes a set of
//!   registries (dimension types and biomes), the hardcore flag leaves the
//!   game mode and max players becomes a VarInt. The registry parts are
//!   converted with [`crate::codec::registries::login`], which keeps the
//!   registries both sides were sent (biome IDs in chunks follow them).
//! - Serverbound `recipe_book_update` splits into
//!   `recipe_book_change_settings` (one book at a time) and
//!   `recipe_book_seen_recipe`.
//!
//! Chunk biomes, `chunk_blocks_update` becoming `section_blocks_update`,
//! the recipe book's new flags in `recipe`, piglin entity data and the rest
//! are the shared codecs' part.
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
