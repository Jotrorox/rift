//! Text components as JSON and NBT, and the packets that are mostly text.
use super::{HasShared, Shared};
use crate::{
    Direction,
    nbt::Nbt,
    step::RouteBuilder,
    wire::{Rewriter, invalid},
};
use std::io;

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {}

/// Copy one text component from `rw.r` to `rw.w`, converting between the two
/// protocols' encodings (a JSON string before 1.20.3, an NBT tag from then
/// on) and formats (hover and click events, item and entity references).
/// For other codecs whose packets contain components.
pub fn transcode(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let _ = (shared, direction, rw);
    Err(invalid("text components are not implemented"))
}

/// Convert a JSON text component held in a string, as item names, sign lines
/// and book pages are while both protocols keep them as JSON.
pub fn convert_json(shared: &mut Shared, direction: Direction, json: &str) -> io::Result<String> {
    let _ = (shared, direction, json);
    Err(invalid("text components are not implemented"))
}

/// Convert a text component held as an NBT tag, for structures that hold
/// one in both protocols (1.20.3 and newer on both sides).
pub fn convert_nbt(shared: &mut Shared, direction: Direction, tag: &mut Nbt) -> io::Result<()> {
    let _ = (shared, direction, tag);
    Err(invalid("text components are not implemented"))
}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    builder
}
