//! Data components of item stacks (1.20.5 and newer): every component type's
//! layout in every protocol from 766 on, and their conversion between
//! adjacent protocols.
//!
//! Layouts are declarative ([`layouts`], built from [`schema::Node`]s), so
//! reading, writing and registry ID remapping all follow one description;
//! a new protocol adds table rows. [`convert()`] carries a patch from the
//! sender's protocol to the receiver's: component types by name, registry
//! IDs inside values, coercions between equivalent encodings, and explicit
//! rules where a component was split, merged, renamed or restructured. What
//! the receiver cannot express is dropped, and the original is stashed in
//! the stack's `custom_data` and restored when the stack comes back (see
//! `stash`), so creative clients and container clicks lose nothing.
//!
//! # API for packet code
//!
//! - [`read`], [`write()`], [`convert()`], [`transcode`]: the patch after a
//!   stack's count and item ID; [`read_delimited`] and friends for the
//!   length-prefixed patches of `set_creative_mode_slot`.
//! - [`schema::read_stack`], [`schema::write_stack`] and the template
//!   variants: whole 1.20.5+ stacks.
//! - [`hashed`]: hashed stacks of 1.21.5+ `container_click`, and
//!   [`hashed::from_stack`] for older clients on newer servers.
//! - [`display`]: slot and recipe displays, trade costs, component matchers
//!   and consume effects, each as a `transcode_*` function.
//! - [`identical`]: whether a pair needs no component conversion at all.
//!
//! A step that must treat a component differently converts the patch with
//! [`convert()`] and then edits the resulting [`Components`]; for values
//! outside a patch, [`convert::with`] gives a conversion context for
//! [`convert_value`].
pub mod convert;
#[cfg(test)]
mod crosscheck;
pub mod display;
pub mod hashed;
pub mod layouts;
mod rules;
pub mod schema;
mod stash;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod vectors;

pub use convert::{compatible, convert_value, identical};
pub use schema::{Node, Value};

use crate::{
    Direction,
    codec::Shared,
    wire::{Reader, Rewriter, Writer},
};
use std::io;

/// One component of a patch: its type by name and its value.
#[derive(Clone, Debug, PartialEq)]
pub struct Component {
    pub kind: &'static str,
    pub value: Value,
}

/// The component patch of one item stack, as read in one protocol: the
/// components it sets, in wire order, and the types it removes from the
/// item's defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Components {
    pub added: Vec<Component>,
    pub removed: Vec<&'static str>,
}

impl Components {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }

    /// The value the patch sets for `kind`.
    pub fn get(&self, kind: &str) -> Option<&Value> {
        self.added
            .iter()
            .find(|component| component.kind == kind)
            .map(|component| &component.value)
    }

    pub fn get_mut(&mut self, kind: &str) -> Option<&mut Value> {
        self.added
            .iter_mut()
            .find(|component| component.kind == kind)
            .map(|component| &mut component.value)
    }

    /// Set `kind` to `value`, in place if it is set already, and stop
    /// removing it.
    pub fn insert(&mut self, kind: &'static str, value: Value) {
        self.removed.retain(|removed| *removed != kind);
        match self.get_mut(kind) {
            Some(slot) => *slot = value,
            None => self.added.push(Component { kind, value }),
        }
    }

    /// Stop setting `kind`, returning its value.
    pub fn take(&mut self, kind: &str) -> Option<Value> {
        let index = self
            .added
            .iter()
            .position(|component| component.kind == kind)?;
        Some(self.added.remove(index).value)
    }

    /// Whether the patch removes `kind` from the item's defaults.
    pub fn removes(&self, kind: &str) -> bool {
        self.removed.contains(&kind)
    }
}

/// Read the component patch that follows a stack's count and item ID (the
/// added and removed counts, then the components) in `protocol`'s layout.
pub fn read(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Components> {
    schema::read_patch(protocol, reader, 0, false)
}

/// Write a component patch in `protocol`'s layout.
pub fn write(protocol: i32, components: &Components, writer: &mut Writer) -> io::Result<()> {
    schema::write_patch(protocol, components, writer, false)
}

/// Read a patch whose component values are each prefixed with their length
/// (`DataComponentPatch.DELIMITED_STREAM_CODEC`), as creative clients send
/// stacks in `set_creative_mode_slot`.
pub fn read_delimited(protocol: i32, reader: &mut Reader<'_>) -> io::Result<Components> {
    schema::read_patch(protocol, reader, 0, true)
}

/// Write a patch in the length-prefixed form; see [`read_delimited`].
pub fn write_delimited(
    protocol: i32,
    components: &Components,
    writer: &mut Writer,
) -> io::Result<()> {
    schema::write_patch(protocol, components, writer, true)
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
    convert::patch(shared, direction, item, components, 0)
}

/// Copy one patch from `rw.r` to `rw.w`, converting it between the two
/// protocols of the step. `item` is the stack's item ID in the receiver's
/// terms.
pub fn transcode(
    shared: &mut Shared,
    direction: Direction,
    item: i32,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let mut components = read(shared.source(direction), &mut rw.r)?;
    convert(shared, direction, item, &mut components)?;
    write(shared.target(direction), &components, &mut rw.w)
}

/// The length-prefixed form of [`transcode`], for creative stacks.
pub fn transcode_delimited(
    shared: &mut Shared,
    direction: Direction,
    item: i32,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let mut components = read_delimited(shared.source(direction), &mut rw.r)?;
    convert(shared, direction, item, &mut components)?;
    write_delimited(shared.target(direction), &components, &mut rw.w)
}

/// Whether a protocol has data components (1.20.5 and newer).
pub fn supported(protocol: i32) -> bool {
    layouts::Layout::of(protocol).is_ok()
}
