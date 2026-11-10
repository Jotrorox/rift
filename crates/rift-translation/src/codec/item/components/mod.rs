//! Data components of item stacks (1.20.5 and newer): every component type's
//! layout in every protocol from 766 on, and their conversion between
//! adjacent protocols.
use crate::{
    Direction,
    codec::Shared,
    wire::{Reader, Writer, invalid},
};
use std::io;

/// The component patch of one item stack, as read in one protocol.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Components {}

/// Read the component patch that follows a stack's count and item ID (the
/// added and removed counts, then the components) in `protocol`'s layout.
pub fn read(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Components> {
    let _ = (protocol, reader);
    Err(invalid("data components are not implemented"))
}

/// Write a component patch in `protocol`'s layout.
pub fn write(protocol: i32, components: &Components, writer: &mut Writer) -> io::Result<()> {
    let _ = (protocol, components, writer);
    Err(invalid("data components are not implemented"))
}

/// Convert a patch read in the sender's protocol (1.20.5 or newer on both
/// sides) into the receiver's: component types, layouts and every registry
/// ID inside. `item` is the stack's item ID in the receiver's terms.
pub fn convert(
    shared: &mut Shared,
    direction: Direction,
    item: i32,
    components: &mut Components,
) -> io::Result<()> {
    let _ = (shared, direction, item, components);
    Err(invalid("data components are not implemented"))
}
