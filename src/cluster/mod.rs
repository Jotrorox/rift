//! Several active proxies as one Raft cluster (docs/clustering.md).
//!
//! Every Rift process with a `cluster.toml` beside its configuration is a
//! **peer**. Peers replicate a log with [openraft] and apply it to a SQLite
//! state machine; the log, the vote and this peer's identity live in
//! `rift.raft.sqlite3`. Nothing here runs unless a cluster is configured: a
//! single Rift never opens these files or binds the peer port.
//!
//! The pieces, bottom up:
//!
//! - [`transport`]: one message per stream over some connection. QUIC in
//!   production ([`quic`]), an in-memory network with faults in tests.
//! - [`link`]: authenticated peer links on top of a transport. Two
//!   connections per pair (control and bulk), typed stream kinds, handlers,
//!   liveness and reconnects. Other modules talk to peers only through it.
//! - [`log_store`]: the Raft log and vote in `rift.raft.sqlite3`.
//! - [`state`]: the replicated state machine, its entry kinds and the
//!   [`state::Component`] registry later steps plug into.
//! - [`network`]: openraft's RPCs over the `raft`, `snapshot` stream kinds.
//! - [`replicated`]: the instance database as the state machine: the
//!   `operation`, `kv`, `directory` and `migrate` entry kinds.
//! - [`operations`]: writes to the instance database: the cluster's
//!   executor and the leader's group-committed execution.
//! - [`attach`]: wiring the instance database into a starting peer.
//! - [`node`]: the [`Cluster`] handle: start, propose, reads, leadership and
//!   its lease, leader tasks and the peer table.
//! - [`membership`]: `rift admin cluster ...`.
//! - [`cli`]: `rift cluster init|recover` and `rift witness`.
//!
//! # Encoding
//!
//! Log entries, Raft messages and handshakes are JSON (serde_json). JSON is
//! self-describing, so a peer of a newer version reads the entries of an
//! older one, and an unknown entry kind fails loudly instead of being
//! misread; every entry kind also carries its own version number `v`.
//! Snapshot and blob contents travel as raw bytes behind a small JSON header.
pub mod attach;
pub mod bus;
pub mod cli;
pub mod link;
pub mod log_store;
pub mod membership;
pub mod network;
pub mod node;
pub mod operations;
pub mod players;
pub mod quic;
pub mod replicated;
pub mod settings;
pub mod snapshot;
pub mod state;
pub mod transport;

#[cfg(test)]
mod sim;

pub use link::{Class, LinkEvent, Links, StreamKind};
pub use node::{Cluster, Leadership, Options, PeerStatus, Role};
pub use operations::ClusterExecutor;
pub use settings::Settings;
pub use state::{ApplyContext, Changes, Component, Components, Entry, EntryKind, Reply};

use serde::{Deserialize, Serialize};
use std::{fmt, sync::OnceLock};

/// A peer's Raft id: assigned once when the peer is added, never reused.
pub type PeerId = u64;

/// What the membership stores about a peer: enough to reach it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerNode {
    pub name: String,
    /// `host:port` other peers dial (`advertise` in its `cluster.toml`).
    pub address: String,
}

impl fmt::Display for PeerNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.address)
    }
}

/// The leader id of a term: `(term, node)`, several leaders per term allowed
/// (openraft's default "advanced" leader id).
pub type LeaderId = openraft::impls::leader_id_adv::LeaderId<u64, PeerId>;
/// `(term, leader, index)` of a log entry.
pub type LogId = openraft::LogId<LeaderId>;
pub type Vote = openraft::impls::Vote<LeaderId>;
pub type Membership = openraft::impls::Membership<PeerId, PeerNode>;
pub type StoredMembership = openraft::StoredMembership<LeaderId, Membership>;
pub type SnapshotMeta = openraft::SnapshotMeta<LeaderId, Membership>;
/// A log entry as stored and replicated.
pub type LogEntry =
    openraft::impls::Entry<LeaderId, openraft::impls::EntryPayload<state::Proposal, Membership>>;

/// openraft's type parameters for Rift. Spelled out instead of
/// `declare_raft_types!`, whose defaults name a deprecated runtime path.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct TypeConfig {}

impl openraft::RaftTypeConfig for TypeConfig {
    type D = state::Proposal;
    type R = Reply;
    type NodeId = PeerId;
    type Node = PeerNode;
    type Term = u64;
    type LeaderId = LeaderId;
    type Vote = Vote;
    type Membership = Membership;
    type MembershipEdit = openraft::impls::ChangeMembers<PeerId, PeerNode>;
    type Payload = openraft::impls::EntryPayload<state::Proposal, Membership>;
    type Entry = LogEntry;
    type Responder<T>
        = openraft::impls::ProgressResponder<Self, T>
    where
        T: openraft::OptionalSend + 'static;
    type Batch<T>
        = openraft::impls::InlineBatch<T>
    where
        T: openraft::OptionalSend + 'static;
    type AsyncRuntime = openraft_rt_tokio::TokioRuntime;
    type ErrorSource = openraft::impls::BoxedErrorSource;
}

/// The Raft node of this process.
pub type Raft = openraft::Raft<TypeConfig, state::StateMachine>;

/// The version a peer reports in its handshake and `peer joined` entries.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

static GLOBAL: OnceLock<Cluster> = OnceLock::new();

/// The cluster this process runs, once `main` started it. `None` on a single
/// Rift, which is every process without a `cluster.toml`.
pub fn global() -> Option<&'static Cluster> {
    GLOBAL.get()
}

/// Make `cluster` the process-wide handle that the admin API, the web API and
/// metrics read. Only the first call takes effect; returns whether it did.
pub fn install_global(cluster: Cluster) -> bool {
    GLOBAL.set(cluster).is_ok()
}

/// Milliseconds since the Unix epoch: the leader's stamp on entries.
pub(crate) fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Bytes of a lowercase or uppercase hex string; `None` if it is not one.
pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

pub(crate) fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

pub(crate) fn sql_error(error: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(format!("cluster storage: {error}"))
}

pub(crate) fn json_error(error: serde_json::Error) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("cluster encoding: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 0xab, 0xff];
        assert_eq!(hex(&bytes), "0001abff");
        assert_eq!(unhex("0001ABff").unwrap(), bytes);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }
}
