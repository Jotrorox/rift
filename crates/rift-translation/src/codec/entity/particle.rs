//! Particle types and their options in every protocol.
use crate::{
    Direction,
    codec::Shared,
    wire::{Rewriter, invalid},
};
use std::io;

/// Copy one particle (its type ID and the options that follow) from `rw.r`
/// to `rw.w`, converting it between the two protocols of the step. `Ok(false)`
/// means the receiver has no such particle and no stand-in: nothing was
/// written and the caller drops what contains it. For 1.13 and newer, where a
/// particle is a type ID followed by its options; for other codecs whose
/// structures contain particles.
pub fn transcode(
    shared: &mut Shared,
    direction: Direction,
    rw: &mut Rewriter<'_>,
) -> io::Result<bool> {
    let _ = (shared, direction, rw);
    Err(invalid("particles are not implemented"))
}
