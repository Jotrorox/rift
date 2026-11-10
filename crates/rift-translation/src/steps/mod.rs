//! One module per pair of adjacent protocols. Each has an `up` step (a newer
//! client on an older backend) and a `down` step (an older client on a newer
//! backend); a pair without one cannot be crossed in that direction yet.
//!
//! # Writing a step
//!
//! A step is a type with per-connection state that implements
//! [`Step`](crate::step::Step), usually through a routing table:
//!
//! ```ignore
//! use crate::{
//!     Packet, State, routed_step,
//!     codec::{HasShared, Shared},
//!     step::{Cx, RouteBuilder, Routes},
//!     wire::Rewriter,
//! };
//! use std::{io, sync::OnceLock};
//!
//! pub struct Up {
//!     shared: Shared,
//!     // ... whatever else the step tracks per connection ...
//! }
//!
//! impl HasShared for Up {
//!     fn shared(&mut self) -> &mut Shared {
//!         &mut self.shared
//!     }
//! }
//!
//! impl Up {
//!     fn new() -> Self {
//!         // Client side first: for `up` that is the newer protocol.
//!         Self { shared: Shared::new(super::NEWER, super::OLDER) }
//!     }
//!
//!     pub fn routes() -> &'static Routes<Self> {
//!         static ROUTES: OnceLock<Routes<Up>> = OnceLock::new();
//!         ROUTES.get_or_init(|| {
//!             RouteBuilder::new(super::NEWER, super::OLDER)
//!                 // Items, entities, chunks, text, tags... of this pair.
//!                 .defaults()
//!                 // What is specific to the pair. Names are the sender's.
//!                 .clientbound(State::Play, "set_health", Self::set_health)
//!                 .drop_serverbound(State::Play, "only_the_newer_client_sends_this")
//!                 .build()
//!         })
//!     }
//!
//!     fn set_health(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
//!         let mut rw = Rewriter::new(&packet.data);
//!         rw.f32()?;
//!         // ... change what differs ...
//!         rw.copy_rest();
//!         cx.to_client(state, Packet::new(packet.id, rw.finish()?));
//!         Ok(())
//!     }
//! }
//! routed_step!(Up);
//!
//! pub const STEP: Option<Make> = Some(|| Box::new(Up::new()));
//!
//! #[cfg(test)]
//! mod tests {
//!     #[test]
//!     fn every_packet_has_a_route() {
//!         assert_eq!(super::Up::routes().problems(), &[] as &[String]);
//!     }
//! }
//! ```
//!
//! Packets named the same on both sides pass through with only their ID
//! changed, unless a shared codec (see [`crate::codec`]) or the step handles
//! them. A packet the receiving side lacks needs a rule;
//! `Routes::problems()` lists what is missing and each step's tests assert
//! that it is empty. A handler gets the packet with its ID already replaced
//! by the same-named packet's ID on the receiving side.
use crate::step::Step;

mod v1_12_1_to_1_12_2;
mod v1_12_2_to_1_13;
mod v1_12_to_1_12_1;
mod v1_13_1_to_1_13_2;
mod v1_13_2_to_1_14;
mod v1_13_to_1_13_1;
mod v1_14_1_to_1_14_2;
mod v1_14_2_to_1_14_3;
mod v1_14_3_to_1_14_4;
mod v1_14_4_to_1_15;
mod v1_14_to_1_14_1;
mod v1_15_1_to_1_15_2;
mod v1_15_2_to_1_16;
mod v1_15_to_1_15_1;
mod v1_16_1_to_1_16_2;
mod v1_16_2_to_1_16_3;
mod v1_16_3_to_1_16_4;
mod v1_16_4_to_1_17;
mod v1_16_to_1_16_1;
mod v1_17_1_to_1_18;
mod v1_17_to_1_17_1;
mod v1_18_2_to_1_19;
mod v1_18_to_1_18_2;
mod v1_19_1_to_1_19_3;
mod v1_19_3_to_1_19_4;
mod v1_19_4_to_1_20;
mod v1_19_to_1_19_1;
mod v1_20_2_to_1_20_3;
mod v1_20_3_to_1_20_5;
mod v1_20_5_to_1_21;
mod v1_20_to_1_20_2;
mod v1_21_11_to_26_1;
mod v1_21_2_to_1_21_4;
mod v1_21_4_to_1_21_5;
mod v1_21_5_to_1_21_6;
mod v1_21_6_to_1_21_7;
mod v1_21_7_to_1_21_9;
mod v1_21_9_to_1_21_11;
mod v1_21_to_1_21_2;
mod v26_1_to_26_2;
mod v26_2_to_26_3;

/// Creates a step's per-connection instance.
pub type Make = fn() -> Box<dyn Step>;

pub struct Pair {
    pub older: i32,
    pub newer: i32,
    /// A newer client on an older backend.
    pub up: Option<Make>,
    /// An older client on a newer backend.
    pub down: Option<Make>,
}

macro_rules! pair {
    ($module:ident) => {
        Pair {
            older: $module::OLDER,
            newer: $module::NEWER,
            up: $module::UP,
            down: $module::DOWN,
        }
    };
}

/// Every adjacent pair of [`crate::version::PROTOCOLS`], oldest first.
pub const PAIRS: &[Pair] = &[
    pair!(v1_12_to_1_12_1),
    pair!(v1_12_1_to_1_12_2),
    pair!(v1_12_2_to_1_13),
    pair!(v1_13_to_1_13_1),
    pair!(v1_13_1_to_1_13_2),
    pair!(v1_13_2_to_1_14),
    pair!(v1_14_to_1_14_1),
    pair!(v1_14_1_to_1_14_2),
    pair!(v1_14_2_to_1_14_3),
    pair!(v1_14_3_to_1_14_4),
    pair!(v1_14_4_to_1_15),
    pair!(v1_15_to_1_15_1),
    pair!(v1_15_1_to_1_15_2),
    pair!(v1_15_2_to_1_16),
    pair!(v1_16_to_1_16_1),
    pair!(v1_16_1_to_1_16_2),
    pair!(v1_16_2_to_1_16_3),
    pair!(v1_16_3_to_1_16_4),
    pair!(v1_16_4_to_1_17),
    pair!(v1_17_to_1_17_1),
    pair!(v1_17_1_to_1_18),
    pair!(v1_18_to_1_18_2),
    pair!(v1_18_2_to_1_19),
    pair!(v1_19_to_1_19_1),
    pair!(v1_19_1_to_1_19_3),
    pair!(v1_19_3_to_1_19_4),
    pair!(v1_19_4_to_1_20),
    pair!(v1_20_to_1_20_2),
    pair!(v1_20_2_to_1_20_3),
    pair!(v1_20_3_to_1_20_5),
    pair!(v1_20_5_to_1_21),
    pair!(v1_21_to_1_21_2),
    pair!(v1_21_2_to_1_21_4),
    pair!(v1_21_4_to_1_21_5),
    pair!(v1_21_5_to_1_21_6),
    pair!(v1_21_6_to_1_21_7),
    pair!(v1_21_7_to_1_21_9),
    pair!(v1_21_9_to_1_21_11),
    pair!(v1_21_11_to_26_1),
    pair!(v26_1_to_26_2),
    pair!(v26_2_to_26_3),
];

/// The steps between `client` and `backend`, nearest the client first, or
/// `None` when a pair on the way has no step for that direction.
pub fn chain(client: i32, backend: i32) -> Option<Vec<Box<dyn Step>>> {
    let (client_index, backend_index) = (
        crate::version::index(client)?,
        crate::version::index(backend)?,
    );
    if client_index > backend_index {
        // Upgrading: the pair below the client's protocol is nearest to it.
        PAIRS[backend_index..client_index]
            .iter()
            .rev()
            .map(|pair| pair.up.map(|make| make()))
            .collect()
    } else {
        PAIRS[client_index..backend_index]
            .iter()
            .map(|pair| pair.down.map(|make| make()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::PROTOCOLS;

    #[test]
    fn pairs_cover_every_adjacent_protocol() {
        assert_eq!(PAIRS.len(), PROTOCOLS.len() - 1);
        for (pair, protocols) in PAIRS.iter().zip(PROTOCOLS.windows(2)) {
            assert_eq!((pair.older, pair.newer), (protocols[0].0, protocols[1].0));
        }
    }
}
