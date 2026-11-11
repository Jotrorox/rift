//! Minecraft 1.14.3 (protocol 490) and 1.14.4 (protocol 498).
//!
//! The two protocols share packet IDs except for one new packet:
//! `block_break_ack`, with which a 1.14.4 server confirms or corrects each
//! digging action. A 1.14.4 client does without it (it forgets unconfirmed
//! actions after a while), so going up nothing replaces it; going down a
//! correction becomes the block update a 1.14.3 server would have sent.
//! `merchant_offers` gained each trade's demand, which is the item codec's.
mod down;
mod up;

use super::Make;

pub const OLDER: i32 = 490;
pub const NEWER: i32 = 498;

/// A 1.14.4 client on a 1.14.3 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.14.3 client on a 1.14.4 backend.
pub const DOWN: Option<Make> = down::STEP;
