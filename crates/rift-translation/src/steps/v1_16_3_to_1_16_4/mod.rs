//! Minecraft 1.16.3 (protocol 753) and 1.16.4/1.16.5 (protocol 754).
//!
//! The protocols differ in one packet: serverbound `edit_book` names the
//! book's inventory slot (hotbar 0 to 8, or 40 for the off hand) where 1.16.3
//! named the hand holding it. Registries are the same.
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;
use crate::wire::{Rewriter, invalid};
use std::io;

pub const OLDER: i32 = 753;
pub const NEWER: i32 = 754;

/// A 1.16.4 client on a 1.16.3 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.16.3 client on a 1.16.4 backend.
pub const DOWN: Option<Make> = down::STEP;

/// 1.16.4's slot number for the off hand.
const OFF_HAND_SLOT: i32 = 40;
/// 1.16.3's hands.
const MAIN_HAND: i32 = 0;
const OFF_HAND: i32 = 1;

/// Copy one item stack as 1.13.2 to 1.20.4 write it: present, then ID,
/// count and NBT tag. Both protocols number items identically (checked by
/// the tests), so the stack needs no conversion.
fn copy_stack(rw: &mut Rewriter<'_>) -> io::Result<()> {
    if rw.bool()? {
        rw.varint()?; // Item.
        rw.i8()?; // Count.
        rw.nbt(true)?;
    }
    Ok(())
}

/// Copy the book and the signing flag of an `edit_book`, then hand back the
/// trailing hand or slot for the caller to rewrite.
fn edit_book_prefix(rw: &mut Rewriter<'_>) -> io::Result<i32> {
    copy_stack(rw)?;
    rw.bool()?; // Signing.
    rw.r.varint()
}

fn check_hotbar(slot: i32) -> io::Result<i32> {
    if (0..9).contains(&slot) {
        Ok(slot)
    } else {
        Err(invalid("invalid hotbar slot"))
    }
}
