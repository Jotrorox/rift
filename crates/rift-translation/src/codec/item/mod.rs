//! Item stacks before and after data components, and the packets that carry them.
pub mod components;

use super::{HasShared, Shared};
use crate::{
    Direction,
    nbt::Nbt,
    step::RouteBuilder,
    wire::{Reader, Rewriter, Writer, invalid},
};
use components::Components;
use std::io;

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {}

/// One non-empty item stack in any protocol's terms. Which fields carry
/// meaning depends on the protocol the stack was read in:
///
/// - before 1.13, `id` is the numeric item ID and `damage` its data value;
/// - from 1.13, `id` is the flattened item's registry ID and `damage` is 0;
/// - before 1.20.5, `tag` is the stack's NBT tag, if any;
/// - from 1.20.5, `components` holds the stack's data component patch.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemStack {
    pub id: i32,
    pub count: i32,
    pub damage: i16,
    pub tag: Option<Nbt>,
    pub components: Option<Components>,
}

/// Read one item stack in `protocol`'s layout; `None` is the empty stack.
pub fn read(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Option<ItemStack>> {
    let _ = (protocol, reader);
    Err(invalid("item stacks are not implemented"))
}

/// Write one item stack in `protocol`'s layout.
pub fn write(protocol: i32, stack: Option<&ItemStack>, writer: &mut Writer) -> io::Result<()> {
    let _ = (protocol, stack, writer);
    Err(invalid("item stacks are not implemented"))
}

/// Convert a stack read in the sender's protocol into the receiver's terms:
/// registry IDs, tag contents and, across 1.20.5, tag to components.
pub fn convert(
    shared: &mut Shared,
    direction: Direction,
    stack: &mut Option<ItemStack>,
) -> io::Result<()> {
    let _ = (shared, direction, stack);
    Err(invalid("item stacks are not implemented"))
}

/// Copy one item stack from `rw.r` to `rw.w`, converting it between the two
/// protocols of the step. For other codecs whose structures contain stacks.
pub fn transcode(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let mut stack = read(shared.source(direction), &mut rw.r)?;
    convert(shared, direction, &mut stack)?;
    write(shared.target(direction), stack.as_ref(), &mut rw.w)
}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    builder
}
