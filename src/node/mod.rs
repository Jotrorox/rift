//! Game servers on more than one machine, managed from one proxy.
//!
//! The proxy stays the controller and decides what runs where. `rift node`
//! agents on other hosts only prepare instance directories and run processes,
//! talking to the controller over the authenticated QUIC message bus. Multiple
//! proxies or proxy failover are out of scope: the broker and the player
//! registry both live in the controller process.
pub mod agent;
pub mod protocol;
mod registry;

pub use registry::{DEFAULT_INSTANCE_MEMORY_MB, Nodes, Observation, Reachability, usage};
