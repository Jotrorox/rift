//! Minecraft 1.17 (protocol 755) and 1.17.1 (protocol 756).
//!
//! 1.17.1 numbers the states of each container menu: slot packets carry the
//! state they leave the menu in and clicks the state they were made in, so
//! the server can resend everything after a click on an outdated view. The
//! whole contents also carry the cursor stack. Book edits send their pages
//! as text instead of a whole book item, and entity removal is a list again
//! after 1.17's one-entity packet.
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;

pub const OLDER: i32 = 755;
pub const NEWER: i32 = 756;

/// A 1.17.1 client on a 1.17 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.17 client on a 1.17.1 backend.
pub const DOWN: Option<Make> = down::STEP;
