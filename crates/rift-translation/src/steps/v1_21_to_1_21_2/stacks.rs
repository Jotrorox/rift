//! Item stacks inside packets this pair owns. Both protocols write a stack as
//! a count (0 for none), the item's registry ID and a data component patch
//! (counts of added and removed components, then the components). A stack
//! with an empty patch only needs its ID mapped; one with components is the
//! item codec's to convert.
use crate::{
    Direction,
    codec::{self, Shared},
    wire::{Reader, Rewriter, Writer, invalid},
};
use std::io;

/// One stack read with an empty component patch, or `Complex` when its
/// patch has entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Peeked {
    Empty,
    Plain { count: i32, item: i32 },
    Complex,
}

/// Look at the stack at the start of `reader` without consuming it.
pub(super) fn peek<'a>(reader: &Reader<'a>) -> io::Result<(Peeked, Reader<'a>)> {
    let mut after = *reader;
    let count = after.varint()?;
    if count <= 0 {
        return Ok((Peeked::Empty, after));
    }
    let item = after.varint()?;
    let added = after.varint()?;
    let removed = after.varint()?;
    if added == 0 && removed == 0 {
        Ok((Peeked::Plain { count, item }, after))
    } else {
        Ok((Peeked::Complex, *reader))
    }
}

/// The receiver's ID of item `id`, or `None` when it has no counterpart.
pub(super) fn map_item(shared: &mut Shared, direction: Direction, id: i32) -> Option<i32> {
    shared.mapping(direction, "item").get(id)
}

pub(super) fn write_plain(writer: &mut Writer, count: i32, item: i32) {
    writer.varint(count).varint(item).varint(0).varint(0);
}

/// Copy one stack that may be empty. An item the receiver has no
/// counterpart for becomes the empty stack.
pub(super) fn transcode_optional(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<()> {
    let (peeked, after) = peek(&rw.r)?;
    match peeked {
        Peeked::Empty => {
            rw.r = after;
            rw.w.varint(0);
            Ok(())
        }
        Peeked::Plain { count, item } => {
            rw.r = after;
            match map_item(shared, direction, item) {
                Some(item) => write_plain(&mut rw.w, count, item),
                None => drop(rw.w.varint(0)),
            }
            Ok(())
        }
        // INTEGRATION: the item codec converts the components (stubbed until
        // it lands, so such stacks fail the packet for now).
        Peeked::Complex => codec::item::transcode(shared, direction, rw),
    }
}

/// Copy one stack that may not be empty (recipe results and displays).
/// `Ok(false)` means the receiver has no such item: nothing was written.
pub(super) fn transcode_required(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<bool> {
    let (peeked, after) = peek(&rw.r)?;
    match peeked {
        Peeked::Empty => Err(invalid("empty item stack where one is required")),
        Peeked::Plain { count, item } => {
            rw.r = after;
            match map_item(shared, direction, item) {
                Some(item) => {
                    write_plain(&mut rw.w, count, item);
                    Ok(true)
                }
                None => Ok(false),
            }
        }
        // INTEGRATION: see `transcode_optional`.
        Peeked::Complex => codec::item::transcode(shared, direction, rw).map(|()| true),
    }
}

/// The item of a required stack, consuming it. Ingredients and item sets
/// only need the item.
pub(super) fn read_item(
    shared: &mut Shared,
    direction: Direction,
    reader: &mut Reader<'_>,
) -> io::Result<i32> {
    let (peeked, after) = peek(reader)?;
    match peeked {
        Peeked::Empty => Err(invalid("empty item stack where one is required")),
        Peeked::Plain { item, .. } => {
            *reader = after;
            Ok(item)
        }
        Peeked::Complex => {
            // INTEGRATION: the item codec reads the components.
            let stack = codec::item::read(shared.source(direction), reader)?
                .ok_or_else(|| invalid("empty item stack where one is required"))?;
            Ok(stack.id)
        }
    }
}
