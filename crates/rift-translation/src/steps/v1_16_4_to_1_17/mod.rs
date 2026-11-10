//! Minecraft 1.16.4/1.16.5 (protocol 754) and 1.17 (protocol 755).
//!
//! 1.17 split the multi-purpose title, world border and combat packets into
//! one packet per action, replaced container transaction confirmations with
//! a generic ping, and let dimensions choose their vertical extent. Container
//! clicks now carry the client's view of the slots they changed instead of
//! the stack the client picked up.
pub(in crate::steps) mod common;
mod down;
mod menus;
#[cfg(test)]
mod tests;
mod up;

use super::Make;

pub const OLDER: i32 = 754;
pub const NEWER: i32 = 755;

/// A 1.17 client on a 1.16.4 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.16.4 client on a 1.17 backend.
pub const DOWN: Option<Make> = down::STEP;

/// 1.16 world border actions, in their numbering, and the 1.17 packet each
/// became. The bodies after the action are identical.
const BORDER: [&str; 6] = [
    "set_border_size",
    "set_border_lerp_size",
    "set_border_center",
    "initialize_border",
    "set_border_warning_delay",
    "set_border_warning_distance",
];

/// 1.16 combat actions and their 1.17 packets.
const COMBAT: [&str; 3] = [
    "player_combat_enter",
    "player_combat_end",
    "player_combat_kill",
];

/// 1.16 title actions 0 to 3 and their 1.17 packets; 4 (hide) and 5 (reset)
/// are both `clear_titles`, told apart by its reset flag.
const TITLES: [&str; 4] = [
    "set_title_text",
    "set_subtitle_text",
    "set_action_bar_text",
    "set_titles_animation",
];
const HIDE_TITLES: i32 = 4;
const RESET_TITLES: i32 = 5;

/// Check a world border body of the 1.16 `action` (or the matching 1.17
/// packet) field by field: the steps move it between packets unchanged.
fn check_border(action: usize, data: &[u8]) -> std::io::Result<()> {
    let mut r = crate::wire::Reader::new(data);
    match action {
        0 => drop(r.f64()?),
        1 => {
            r.f64()?;
            r.f64()?;
            r.varlong()?;
        }
        2 => {
            r.f64()?;
            r.f64()?;
        }
        3 => {
            for _ in 0..4 {
                r.f64()?;
            }
            r.varlong()?;
            for _ in 0..3 {
                r.varint()?;
            }
        }
        4 | 5 => drop(r.varint()?),
        _ => return Err(crate::wire::invalid("unknown world border action")),
    }
    r.finish()
}
