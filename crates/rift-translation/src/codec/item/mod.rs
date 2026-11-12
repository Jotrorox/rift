//! Item stacks before and after data components, and the packets that carry them.
//!
//! - [`read`], [`write`]: one stack in any protocol's layout (see `stack`).
//! - [`convert`]: a stack from the sender's terms into the receiver's, for
//!   the two adjacent protocols of a step: registry IDs, NBT tag contents
//!   (the 1.13 flattening, JSON lore, UUID arrays, potion effects...) and,
//!   from 1.20.5 on both sides, the data components. A receiver that lacks
//!   an item gets a stand-in named after the original, and whatever the
//!   receiver cannot hold is stashed in the tag under `rift:<protocol>` so
//!   the stack converts back when the client returns it.
//! - [`install`]: default handlers for every item-carrying packet, only for
//!   the pairs whose layouts or IDs differ.
//!
//! The packet models (`window`, `merchant`, `advancements`, `recipes`) read
//! and write each packet in every protocol's layout; handlers read in the
//! sender's layout, convert, and write in the receiver's.
pub mod advancements;
mod bridge;
pub mod components;
mod convert;
mod flatten;
mod handlers;
mod legacy;
mod legacy_text;
pub mod merchant;
pub mod recipes;
mod stack;
mod tags;
#[cfg(test)]
mod tests;
pub mod window;

use super::{HasShared, Shared};
use crate::{
    Direction,
    nbt::Nbt,
    step::RouteBuilder,
    version,
    wire::{Reader, Rewriter, Writer, invalid},
};
use components::Components;
use std::{collections::HashMap, io};

pub use handlers::{convert_offers, transcode_trade_list};

/// What this codec keeps for one step of one connection.
#[derive(Default)]
pub struct State {
    /// The latest state ID the backend sent (1.17.1 on), for clicks from
    /// clients that do not send one.
    state_id: i32,
    /// Action numbers for clicks towards backends before 1.17.
    action: i16,
    /// The cursor stack last sent to a 1.17 client, in the client's terms,
    /// for 1.17.1 container contents that carry it.
    cursor: Option<ItemStack>,
    /// Stacks the backend put in containers, in its terms, by container and
    /// slot: what a hashed click (1.21.5) from the client stands for.
    slots: HashMap<(i32, i16), ItemStack>,
    /// The selected hotbar slot, which 1.17.1 book edits name.
    held_slot: i32,
    /// Recipes of a backend before 1.21.2, for a client on 1.21.2 or newer.
    recipes: handlers::RecipeTable,
}

impl State {
    /// The recipe a 1.21.2 client names by display ID when its backend is
    /// older, for steps translating `place_recipe` and
    /// `recipe_book_seen_recipe`.
    pub fn recipe_name(&self, display: i32) -> Option<&str> {
        self.recipes.name(display)
    }
}

/// One item stack: `None` is the empty stack.
pub type Stack = Option<ItemStack>;

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
    stack::read(protocol, reader)
}

/// Write one item stack in `protocol`'s layout.
pub fn write(protocol: i32, stack: Option<&ItemStack>, writer: &mut Writer) -> io::Result<()> {
    stack::write(protocol, stack, writer)
}

/// Convert a stack read in the sender's protocol into the receiver's terms:
/// registry IDs, tag contents and, across 1.20.5, tag to components.
pub fn convert(
    shared: &mut Shared,
    direction: Direction,
    stack: &mut Option<ItemStack>,
) -> io::Result<()> {
    convert::stack(shared, direction, stack)
}

/// Convert the stack of a `show_item` hover event between 1.20.4 (its NBT
/// `tag`) and 1.20.5 (its data components in persistent form), for the text
/// codec. `Ok(false)` for any other pair, or when the sender's item is
/// unknown; the caller degrades the event then.
pub fn convert_hover_stack(
    shared: &mut Shared,
    direction: Direction,
    id: &mut String,
    count: &mut i32,
    tag: &mut Option<crate::nbt::Compound>,
    components: &mut Option<crate::nbt::Compound>,
) -> io::Result<bool> {
    let pair = (shared.source(direction), shared.target(direction));
    if pair != (765, 766) && pair != (766, 765) {
        return Ok(false);
    }
    bridge::hover(shared, direction, id, count, tag, components)
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

/// Read one stack template (26.1 on: never empty) in `protocol`'s layout,
/// for codecs whose structures hold one (item particles).
pub fn read_template(protocol: i32, reader: &mut Reader<'_>) -> io::Result<ItemStack> {
    stack::read_template(protocol, reader)
}

pub fn write_template(protocol: i32, stack: &ItemStack, writer: &mut Writer) -> io::Result<()> {
    stack::write_template(protocol, writer, stack)
}

/// Install this codec's default routes for the builder's pair.
pub fn install<S: HasShared + 'static>(builder: RouteBuilder<S>) -> RouteBuilder<S> {
    handlers::install(builder)
}

/// A text component as packets carry it: a JSON string before 1.20.3, an
/// NBT tag from then on.
#[derive(Clone, Debug, PartialEq)]
pub enum Text {
    Json(String),
    Nbt(Nbt),
}

/// Vanilla's limit for JSON text components on the wire.
const MAX_JSON_TEXT: usize = 262_144;

impl Text {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        if version::nbt_components(protocol) {
            Nbt::read(r, false)?
                .map(Self::Nbt)
                .ok_or_else(|| invalid("missing text component"))
        } else {
            Ok(Self::Json(r.string(MAX_JSON_TEXT)?.to_owned()))
        }
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        match (self, version::nbt_components(protocol)) {
            (Self::Nbt(tag), true) => tag.write(w, false),
            (Self::Json(json), false) => {
                w.string(json);
            }
            _ => return Err(invalid("text component does not fit the protocol")),
        }
        Ok(())
    }

    /// Convert through the text codec, which knows every protocol's format.
    pub fn convert(&mut self, shared: &mut Shared, direction: Direction) -> io::Result<()> {
        let mut source = Writer::new();
        self.write(shared.source(direction), &mut source)?;
        let mut rw = Rewriter::new(&source.buf);
        super::text::transcode(shared, direction, &mut rw)?;
        let target = rw.finish()?;
        let mut reader = Reader::new(&target);
        *self = Self::read(shared.target(direction), &mut reader)?;
        reader.finish()
    }
}
