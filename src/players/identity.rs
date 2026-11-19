//! Which peer of a cluster this process is, and the layout of connection ids.
//!
//! A connection id names one connection anywhere in the cluster. Its 53 bits
//! are, from the most significant:
//!
//! | Bits | Field | Range |
//! | --- | --- | --- |
//! | 8 | the peer's numeric id | 1..=255 in a cluster, 0 without one |
//! | 8 | the low 8 bits of the peer's incarnation | 0..=255 |
//! | 37 | the process-local counter from `src/events.rs` | 1..=2^37-1, then wraps to 1 |
//!
//! The layout stays below 2^53 on purpose: connection ids cross JSON (the
//! HTTP API, bus payloads, plugin hosts) as numbers, and JavaScript and
//! LuaJIT hold numbers as doubles, which are exact only up to 2^53. Without a
//! cluster the peer and incarnation are 0, so a single Rift keeps the ids it
//! always had: the counter itself (until 137 billion connections).
//!
//! The owning peer of any connection id can be read from the id
//! ([`owner_of`]), and an id minted by an older incarnation of a peer does not
//! match its current one ([`Identity::owns`]), so an action addressed to it is
//! refused instead of reaching another player's session.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

/// Bits of the peer id in a connection id.
pub const PEER_BITS: u32 = 8;
/// Bits of the incarnation (its low bits) in a connection id.
pub const INCARNATION_BITS: u32 = 8;
/// Bits of the process-local counter in a connection id.
pub const COUNTER_BITS: u32 = 37;
/// The largest peer id a cluster may assign.
pub const MAX_PEER: u16 = (1 << PEER_BITS) - 1;
/// The largest connection id: 2^53 - 1, the largest integer a double holds
/// exactly.
pub const MAX_CONNECTION_ID: u64 = (1 << (PEER_BITS + INCARNATION_BITS + COUNTER_BITS)) - 1;

const COUNTER_MASK: u64 = (1 << COUNTER_BITS) - 1;
const TAG_MASK: u64 = (1 << INCARNATION_BITS) - 1;

/// A peer and the incarnation it runs as. Peer 0 is a single Rift without a
/// cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Identity {
    pub peer: u16,
    pub incarnation: u64,
}

impl Identity {
    /// A single Rift: no cluster.
    pub const LOCAL: Self = Self {
        peer: 0,
        incarnation: 0,
    };

    /// Whether this identity belongs to a cluster.
    pub fn is_clustered(self) -> bool {
        self.peer != 0
    }

    /// The incarnation as connection ids carry it.
    pub fn tag(self) -> u8 {
        (self.incarnation & TAG_MASK) as u8
    }

    /// The connection id for the `counter`-th connection of this identity.
    /// Counter values above 2^37 - 1 wrap to 1, so the id never becomes 0.
    pub fn connection_id(self, counter: u64) -> u64 {
        let counter = (counter.max(1) - 1) % COUNTER_MASK + 1;
        (u64::from(self.peer.min(MAX_PEER)) << (INCARNATION_BITS + COUNTER_BITS))
            | (u64::from(self.tag()) << COUNTER_BITS)
            | counter
    }

    /// Whether `id` was minted by this peer in this incarnation.
    pub fn owns(self, id: u64) -> bool {
        owner_of(id) == (self.peer, self.tag())
    }
}

/// The peer and incarnation tag (the low 8 bits of the incarnation) a
/// connection id was minted by.
pub fn owner_of(id: u64) -> (u16, u8) {
    (
        ((id >> (INCARNATION_BITS + COUNTER_BITS)) & u64::from(MAX_PEER)) as u16,
        ((id >> COUNTER_BITS) & TAG_MASK) as u8,
    )
}

static PEER: AtomicU32 = AtomicU32::new(0);
static INCARNATION: AtomicU64 = AtomicU64::new(0);
static NAMES: Mutex<BTreeMap<u16, String>> = Mutex::new(BTreeMap::new());

/// Set this process's cluster identity. Call it once, before the first
/// connection is accepted; ids minted before keep the old identity. Without
/// a call the identity is [`Identity::LOCAL`].
pub fn set_identity(peer: u16, incarnation: u64) -> Result<(), String> {
    if peer > MAX_PEER {
        return Err(format!("peer ids must be at most {MAX_PEER}, not {peer}"));
    }
    INCARNATION.store(incarnation, Ordering::Release);
    PEER.store(u32::from(peer), Ordering::Release);
    Ok(())
}

/// This process's cluster identity.
pub fn identity() -> Identity {
    Identity {
        peer: PEER.load(Ordering::Acquire) as u16,
        incarnation: INCARNATION.load(Ordering::Acquire),
    }
}

/// The connection id for the process-local `counter`.
pub fn connection_id(counter: u64) -> u64 {
    identity().connection_id(counter)
}

/// Name a peer id for listings (`eu-1`); peers without a name are listed by
/// their number.
pub fn set_peer_name(peer: u16, name: impl Into<String>) {
    NAMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(peer, name.into());
}

/// The name listings give `peer`: its name, else its number.
pub fn peer_label(peer: u16) -> String {
    NAMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&peer)
        .cloned()
        .unwrap_or_else(|| peer.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_process_ids_are_the_counter() {
        for counter in [1, 2, 1000, COUNTER_MASK] {
            assert_eq!(Identity::LOCAL.connection_id(counter), counter);
            assert!(Identity::LOCAL.owns(counter));
        }
        // Wraps to 1, never 0, and never into the peer bits.
        assert_eq!(Identity::LOCAL.connection_id(COUNTER_MASK + 1), 1);
        assert_eq!(Identity::LOCAL.connection_id(0), 1);
    }

    #[test]
    fn cluster_ids_name_their_owner_and_stay_exact_in_doubles() {
        let peer = Identity {
            peer: MAX_PEER,
            incarnation: 0x1_02ff,
        };
        let id = peer.connection_id(u64::MAX);
        assert!(id <= MAX_CONNECTION_ID);
        assert_eq!(MAX_CONNECTION_ID, (1u64 << 53) - 1);
        assert_eq!(id as f64 as u64, id);
        assert_eq!(owner_of(id), (MAX_PEER, 0xff));
        assert!(peer.owns(id));
        let restarted = Identity {
            incarnation: peer.incarnation + 1,
            ..peer
        };
        assert!(!restarted.owns(id));
        let other = Identity {
            peer: 3,
            incarnation: peer.incarnation,
        };
        assert!(!other.owns(id));
        assert_eq!(owner_of(other.connection_id(42)), (3, 0xff));
        assert!(!Identity::LOCAL.owns(id));
    }
}
