//! Minecraft 1.12.1 (protocol 338) and 1.12.2 (protocol 340).
//!
//! The only difference is the keep-alive ID: a VarInt up to 1.12.1, a long
//! from 1.12.2, in both directions. Every other packet keeps its ID and
//! layout.
//!
//! A server checks that the client echoes the ID it sent, and a 1.12.2
//! server disconnects a player who answers with any other one, so an ID
//! that does not survive the trip unchanged must never reach it. Widening
//! is lossless; narrowing a long the client cannot hold uses stand-in IDs
//! (see `down`).
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;

pub const OLDER: i32 = 338;
pub const NEWER: i32 = 340;

/// A 1.12.2 client on a 1.12.1 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.12.1 client on a 1.12.2 backend.
pub const DOWN: Option<Make> = down::STEP;
