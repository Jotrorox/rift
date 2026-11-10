//! Minecraft 26.2 (protocol 776) and 26.3 (protocol 777).
//!
//! What changed in the packets this step owns, from the official servers'
//! packet classes:
//!
//! - Join game and `respawn`: the game mode is a VarInt and the previous
//!   game mode an optional VarInt.
//! - Entity movement: position paths and stepped deltas (see `movement`).
//! - `animate` lost the arm swings, which became `swing_animation` (hand,
//!   animation type and duration); the remaining actions were renumbered.
//!   Serverbound `swing` became `punch`, sent only for an attack swing of
//!   the main hand.
//! - `player_action` gained `CHANGE_DESTROY_DIRECTION` at index 1, and the
//!   server now sends the block-cracking particles and sounds of the
//!   player's own mining as level events (2019/2020) every tick.
//! - `accept_teleportation` echoes the accepted position and rotation; the
//!   server applies them like a movement packet.
//! - `open_sign_editor` and `sign_update` name the sign side with a VarInt
//!   (back 0, front 1), and `sign_update` moved it after the lines.
//! - New level events: potion splash sounds (1053, 1054) played apart from
//!   their particles, particle-only block breaking (2014), teleport
//!   particles (2015 to 2018) and destroy progress (2019, 2020); wax-on
//!   (3003) no longer plays a sound.
//! - The brewing stand menu has four data slots (total brew time and total
//!   fuel joined brew time and fuel).
//! - New: `add_transient_block` (a block shown for a moment) and
//!   `post_effects` (shaders, in configuration and play).
//!
//! `BitSet` became a length-prefixed byte array (light masks, chat filter
//! masks), and command parsers, chunks, particles, item components and
//! advancements changed; those packets belong to the shared codecs.
mod down;
mod movement;
mod up;

#[cfg(test)]
mod tests;

use super::Make;
use crate::{
    Direction,
    codec::Shared,
    wire::{Reader, Writer, invalid},
};
use std::io;

pub const OLDER: i32 = 776;
pub const NEWER: i32 = 777;

/// A 26.3 client on a 26.2 backend.
pub const UP: Option<Make> = up::STEP;
/// A 26.2 client on a 26.3 backend.
pub const DOWN: Option<Make> = down::STEP;

/// 26.2 `animate` actions; 26.3 numbers what is left of them 0 to 2.
mod animate {
    pub const SWING_MAIN_HAND: u8 = 0;
    pub const WAKE_UP: u8 = 2;
    pub const SWING_OFF_HAND: u8 = 3;
    pub const CRITICAL_HIT: u8 = 4;
    pub const MAGIC_CRITICAL_HIT: u8 = 5;
    /// 26.3's numbers for wake up, critical and magic critical hit.
    pub const NEWER: [u8; 3] = [WAKE_UP, CRITICAL_HIT, MAGIC_CRITICAL_HIT];
}

/// `SwingAnimationType`: none, whack, stab.
const SWING_NONE: i32 = 0;
const SWING_WHACK: i32 = 1;
/// The default swing (`SwingAnimation.DEFAULT`), what a 26.2 client plays.
const SWING_DURATION: i32 = 6;

/// 26.3 `player_action`: the action 26.2 lacks, at index 1.
const CHANGE_DESTROY_DIRECTION: i32 = 1;
const START_DESTROY_BLOCK: i32 = 0;
const ABORT_DESTROY_BLOCK: i32 = 2;
const STOP_DESTROY_BLOCK: i32 = 3;

/// Level events whose data is a block state.
const BLOCK_STATE_EVENTS: [i32; 3] = [2001, 2014, 3008];
const DESTROY_BLOCK: i32 = 2001;
const DESTROY_BLOCK_PARTICLES: i32 = 2014;
const SPLASH_POTION: i32 = 2002;
const INSTANT_SPLASH_POTION: i32 = 2007;
const SPLASH_POTION_SOUND: i32 = 1053;
const INSTANT_SPLASH_POTION_SOUND: i32 = 1054;
const WAX_ON: i32 = 3003;
const DESTROY_PROGRESS: i32 = 2019;
const DESTROY_PROGRESS_AND_SOUND: i32 = 2020;
/// Level events 26.3 added that 26.2 clients do not know.
const NEWER_ONLY_EVENTS: [i32; 8] = [1053, 1054, 2015, 2016, 2017, 2018, 2019, 2020];

/// Brewing stand data slots 26.3 added: total brew time and total fuel,
/// with the values 26.2 hard-coded.
const BREWING_STAND_EXTRA_DATA: [(i16, i16); 2] = [(2, 400), (3, 20)];

/// A level event: ID, block position, data and the global flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LevelEvent {
    event: i32,
    position: i64,
    data: i32,
    global: bool,
}

impl LevelEvent {
    fn parse(data: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(data);
        let event = Self {
            event: r.i32()?,
            position: r.i64()?,
            data: r.i32()?,
            global: r.bool()?,
        };
        r.finish()?;
        Ok(event)
    }

    fn to_bytes(self) -> Vec<u8> {
        let mut w = Writer::with_capacity(17);
        w.i32(self.event)
            .i64(self.position)
            .i32(self.data)
            .bool(self.global);
        w.into_inner()
    }

    /// Map a block-state payload to the receiver; `None` when it has no
    /// such state.
    fn convert(mut self, shared: &mut Shared, direction: Direction) -> Option<Self> {
        if BLOCK_STATE_EVENTS.contains(&self.event) {
            self.data = shared.mapping(direction, "block_state").get(self.data)?;
        }
        Some(self)
    }
}

/// The sign side as 26.2's flag: true for the front.
fn sign_side_flag(slot: i32) -> bool {
    slot == 1
}

/// Copy the four lines of a sign (at most 384 characters each).
fn copy_sign_lines(r: &mut Reader<'_>, w: &mut Writer) -> io::Result<()> {
    for _ in 0..4 {
        w.string(r.string(384)?);
    }
    Ok(())
}

/// A brewing stand's `container_set_data` slot is the only menu data that
/// changed; anything else in a data packet passes as is.
fn container_data(data: &[u8]) -> io::Result<(i32, i16)> {
    let mut r = Reader::new(data);
    let container = r.varint()?;
    let slot = r.i16()?;
    r.i16()?;
    r.finish()?;
    Ok((container, slot))
}

/// The container ID of an `open_screen` that opens a brewing stand, in
/// `protocol`'s `menu` registry.
fn brewing_stand_screen(protocol: i32, data: &[u8]) -> io::Result<Option<i32>> {
    let mut r = Reader::new(data);
    let container = r.varint()?;
    let menu = r.varint()?;
    let brewing_stand = crate::data::version(protocol)
        .expect_registry("menu")
        .id("brewing_stand")
        .ok_or_else(|| invalid("no brewing stand menu"))?;
    Ok((menu == brewing_stand).then_some(container))
}
