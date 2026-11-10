//! Handlers both orientations of the step share. Each reads its packet in
//! the sender's protocol and writes it in the receiver's; which way it
//! converts follows from the step's backend and client protocols.
use super::NEWER;
use crate::{
    Direction, Packet, State,
    codec::{HasShared, item},
    data,
    step::Cx,
    wire::{Position, Reader, Rewriter, Writer, invalid},
};
use std::io;

/// The longest JSON text component 1.13 reads.
pub(super) const TEXT: usize = 262_144;

/// Boss bar flags: 0x02 plays the boss music and, before 1.13.1, also
/// creates fog; from 1.13.1 fog has 0x04.
const MUSIC: u8 = 0x02;
const FOG: u8 = 0x04;

pub(super) fn boss_event<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let to_newer = step.shared().backend < NEWER;
    let mut rw = Rewriter::new(&packet.data);
    rw.uuid()?;
    let has_flags = match rw.varint()? {
        // Add: title, progress, color, overlay, flags.
        0 => {
            rw.string(TEXT)?;
            rw.f32()?;
            rw.varint()?;
            rw.varint()?;
            true
        }
        1 => false,
        2 => {
            rw.f32()?;
            false
        }
        3 => {
            rw.string(TEXT)?;
            false
        }
        4 => {
            rw.varint()?;
            rw.varint()?;
            false
        }
        5 => true,
        _ => return Err(invalid("unknown boss bar operation")),
    };
    if has_flags {
        let flags = rw.r.u8()?;
        rw.w.u8(if to_newer {
            boss_flags_up(flags)
        } else {
            boss_flags_down(flags)
        });
    }
    cx.to_client(state, Packet::new(packet.id, rw.finish()?));
    Ok(())
}

/// A 1.13 bar with 0x02 meant music and fog: the dragon fight has both.
fn boss_flags_up(flags: u8) -> u8 {
    if flags & MUSIC != 0 {
        flags | FOG
    } else {
        flags
    }
}

/// A 1.13 client cannot show fog without music. Fog wins: a bar with fog
/// alone (none of vanilla's) also plays the music.
fn boss_flags_down(flags: u8) -> u8 {
    if flags & FOG != 0 {
        flags & !FOG | MUSIC
    } else {
        flags
    }
}

/// Level events whose data the two versions encode differently.
const RECORD: i32 = 1010;
const SMOKE: i32 = 2000;
const BLOCK_BREAK: i32 = 2001;

/// 1.13.1 facings by 3D data value.
const DOWN: i32 = 0;
const UP: i32 = 1;
const NORTH: i32 = 2;
const SOUTH: i32 = 3;
const WEST: i32 = 4;
const EAST: i32 = 5;

pub(super) fn level_event<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let to_newer = shared.backend < NEWER;
    let mut reader = Reader::new(&packet.data);
    let event = reader.i32()?;
    let mut position = reader.position_legacy()?;
    let mut data = reader.i32()?;
    let global = reader.bool()?;
    reader.finish()?;
    match event {
        // The record's item ID; 0 (air) stops the music.
        RECORD => {
            data = shared
                .mapping(Direction::Clientbound, "item")
                .get_or(data, 0)
        }
        BLOCK_BREAK => {
            data = shared
                .mapping(Direction::Clientbound, "block_state")
                .get_or(data, 0)
        }
        SMOKE if to_newer => data = smoke_facing(data),
        SMOKE => (position, data) = smoke_cell(position, data),
        _ => {
            cx.to_client(state, packet);
            return Ok(());
        }
    }
    let mut body = Writer::with_capacity(packet.data.len());
    body.i32(event)
        .position_legacy(position)
        .i32(data)
        .bool(global);
    cx.to_client(state, Packet::new(packet.id, body.into_inner()));
    Ok(())
}

/// The facing of a dispenser whose smoke 1.13 puts in grid `cell`. Vanilla
/// only uses the four sides and the centre, which stands for both up and
/// down: up, where the smoke of a 1.13 client rises out of the block too.
fn smoke_facing(cell: i32) -> i32 {
    match cell {
        1 => NORTH,
        7 => SOUTH,
        0 | 3 | 6 => WEST,
        2 | 5 | 8 => EAST,
        _ => UP,
    }
}

/// The grid cell for a 1.13 client of smoke from a dispenser at `position`
/// facing `facing`. The grid has no up or down: smoke from the centre cell
/// of the block above or below comes closest.
fn smoke_cell(mut position: Position, facing: i32) -> (Position, i32) {
    let cell = match facing {
        NORTH => 1,
        SOUTH => 7,
        WEST => 3,
        EAST => 5,
        DOWN | UP => {
            let y = position.y + if facing == UP { 1 } else { -1 };
            // Within the 12 bits a 1.13 position holds.
            if (-2048..2048).contains(&y) {
                position.y = y;
            }
            4
        }
        _ => 4,
    };
    (position, cell)
}

/// Which hand a 1.13.1 client edits a book in.
const MAIN_HAND: i32 = 0;
const OFF_HAND: i32 = 1;
/// The `player_action` that swaps the items in the two hands.
const SWAP_HANDS: i32 = 6;

pub(super) fn edit_book<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    let shared = step.shared();
    let from_newer = shared.client >= NEWER;
    let mut rw = Rewriter::new(&packet.data);
    // INTEGRATION: the item codec renumbers the book's item ID (1.13.1
    // inserted five items before it) and converts its tag.
    item::transcode(shared, Direction::Serverbound, &mut rw)?;
    rw.bool()?; // Signing.
    if !from_newer {
        rw.w.varint(MAIN_HAND);
        cx.to_backend(state, Packet::new(packet.id, rw.finish()?));
        return Ok(());
    }
    let hand = rw.r.varint()?;
    let edit = Packet::new(packet.id, rw.finish()?);
    match hand {
        MAIN_HAND => cx.to_backend(state, edit),
        // A 1.13 server writes into the main hand's book, or the main hand's
        // other book. Swap the hands around the edit, as the player could.
        OFF_HAND => {
            let swap = Packet::new(
                shared.target_data(Direction::Serverbound).expect_packet(
                    State::Play,
                    Direction::Serverbound,
                    "player_action",
                ),
                swap_hands(),
            );
            cx.to_backend(state, swap.clone());
            cx.to_backend(state, edit);
            cx.to_backend(state, swap);
        }
        _ => return Err(invalid("unknown hand")),
    }
    Ok(())
}

/// A 1.13 `player_action` swapping the hands, as a 1.13 client sends it:
/// no position, facing down.
pub(super) fn swap_hands() -> Vec<u8> {
    let mut body = Writer::new();
    body.varint(SWAP_HANDS)
        .position_legacy(Position { x: 0, y: 0, z: 0 })
        .i8(0);
    body.into_inner()
}

/// Plugin messages from the backend. Of vanilla's 1.13 channels only the
/// villager trade list carries something that changes between 1.13, 1.13.1
/// and 1.13.2: item stacks. Everything else passes untouched.
pub(in crate::steps) fn custom_payload<S: HasShared>(
    step: &mut S,
    cx: &mut Cx<'_>,
    state: State,
    packet: Packet,
) -> io::Result<()> {
    if data::strip(Reader::new(&packet.data).str()?) != "trader_list" {
        cx.to_client(state, packet);
        return Ok(());
    }
    let shared = step.shared();
    let mut rw = Rewriter::new(&packet.data);
    rw.str()?; // Channel.
    rw.i32()?; // Window.
    for _ in 0..rw.u8()? {
        // INTEGRATION: the item codec converts each stack (IDs from 1.13.1,
        // the stack layout from 1.13.2).
        item::transcode(shared, Direction::Clientbound, &mut rw)?; // First cost.
        item::transcode(shared, Direction::Clientbound, &mut rw)?; // Result.
        if rw.bool()? {
            item::transcode(shared, Direction::Clientbound, &mut rw)?; // Second cost.
        }
        rw.bool()?; // Out of stock.
        rw.i32()?; // Uses.
        rw.i32()?; // Maximum uses.
    }
    cx.to_client(state, Packet::new(packet.id, rw.finish()?));
    Ok(())
}
