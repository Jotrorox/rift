//! Minecraft 1.18/1.18.1 (protocol 757) and 1.18.2 (protocol 758).
//!
//! 1.18.2 names the blocks that burn forever in a dimension by a tag
//! reference, and sends mob effect IDs as VarInts (the entity codec's).
//! Everything else is unchanged.
mod down;
#[cfg(test)]
mod tests;
mod up;

use super::Make;
use crate::{
    Packet, State,
    step::Cx,
    steps::v1_16_4_to_1_17::common::{self, Login, Respawn},
};
use std::io;

pub const OLDER: i32 = 757;
pub const NEWER: i32 = 758;

/// A 1.18.2 client on a 1.18 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.18 client on a 1.18.2 backend.
pub const DOWN: Option<Make> = down::STEP;

/// Give every dimension type of a join game packet the receiver's form of
/// `infiniburn`: hashed for 1.18.2.
// INTEGRATION: the registries codec's conversion of the dimension codec.
fn login(cx: &mut Cx<'_>, state: State, packet: Packet, hashed: bool) -> io::Result<()> {
    let protocol = if hashed { OLDER } else { NEWER };
    let mut login = Login::read(protocol, &packet.data)?;
    login.edit_dimension_types(|dimension| common::tag_infiniburn(dimension, hashed))?;
    cx.to_client(state, Packet::new(packet.id, login.write(protocol)));
    Ok(())
}

// INTEGRATION: the registries codec's conversion of the dimension type.
fn respawn(cx: &mut Cx<'_>, state: State, packet: Packet, hashed: bool) -> io::Result<()> {
    let mut respawn = Respawn::read(&packet.data)?;
    common::tag_infiniburn(respawn.dimension_type()?, hashed)?;
    cx.to_client(state, Packet::new(packet.id, respawn.write()));
    Ok(())
}
