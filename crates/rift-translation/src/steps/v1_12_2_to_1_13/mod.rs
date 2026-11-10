//! Minecraft 1.12.2 (protocol 340) and 1.13 (protocol 393).
//!
//! Translation keys 1.13 renamed are converted by the text codec with
//! [`upgrade_translation_key`] and [`legacy_translation`].
mod down;
mod up;

use super::Make;

pub(crate) use down::translations::{Legacy, legacy_translation};
pub(crate) use up::translations::upgrade_translation_key;

pub const OLDER: i32 = 340;
pub const NEWER: i32 = 393;

/// A 1.13 client on a 1.12.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.12.2 client on a 1.13 backend.
pub const DOWN: Option<Make> = down::STEP;
