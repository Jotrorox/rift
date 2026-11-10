//! Between 1.20.4 (NBT tags) and 1.20.5 (data components).
//!
//! INTEGRATION: converting a tag into a component patch and back is a
//! follow-up once both the tag rules here and the component codec exist; it
//! slots in as [`convert`]. Until then the pair fails translation of any
//! non-empty stack rather than passing on one the receiver cannot read.
use super::Stack;
use crate::{Direction, codec::Shared, wire::invalid};
use std::io;

pub(super) fn convert(
    shared: &mut Shared,
    direction: Direction,
    stack: &mut Stack,
) -> io::Result<()> {
    let _ = (shared, direction);
    if stack.is_none() {
        return Ok(());
    }
    Err(invalid(
        "converting item stacks between NBT tags (1.20.4) and data components (1.20.5) is not implemented",
    ))
}
