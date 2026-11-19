//! Game servers on more than one machine, managed from one proxy.
//!
//! The proxy stays the controller and decides what runs where. `rift node`
//! agents on other hosts only prepare instance directories and run processes,
//! talking to the controller over the authenticated QUIC message bus. In a
//! cluster of several proxies (docs/clustering.md) the Raft leader is the
//! controller: agents connect to any peer, the leader fences them with its
//! term before it sends control commands, and every peer that serves players
//! is also a node named after itself, with its agent inside the peer.
pub mod agent;
pub mod backup;
pub mod fence;
mod files;
mod host;
pub mod protocol;
mod registry;
pub mod seal;
pub mod transfer;

pub use files::Remote;
pub use registry::{
    ActingTerm, DEFAULT_INSTANCE_MEMORY_MB, Nodes, Observation, Reachability, identities,
    server_requirements, token, usage,
};
