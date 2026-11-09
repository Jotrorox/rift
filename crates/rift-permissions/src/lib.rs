//! Rift's permission engine, shared by the proxy, Rust backend plugins and the
//! C ABI: the data model ([`node`], [`context`], [`subject`]), the two-stage
//! resolver ([`compiled`] compiles a user once on the proxy,
//! [`check`](mod@check) answers checks anywhere), the LuckPerms codec
//! ([`luckperms`]), the write model ([`change`]), the bus messages ([`wire`])
//! and a backend cache ([`replica`]).
//!
//! The crate has no I/O: callers pass `now` (Unix seconds) explicitly, which
//! keeps every result reproducible. `docs/permissions-protocol.md` documents
//! the JSON shapes; `tests/fixtures/permissions` holds the corpus that the Rust
//! and Java implementations both run.

pub mod change;
pub mod check;
pub mod compiled;
pub mod context;
mod error;
pub mod luckperms;
pub mod node;
pub mod replica;
pub mod subject;
pub mod wire;

pub use change::{Actor, Applied, Change, ChangeEvent, SubjectRef};
pub use check::{MetaView, Trace, Tristate, check};
pub use compiled::{CompiledSubject, compile};
pub use context::ContextSet;
pub use error::Error;
pub use node::{Node, NodeKind, Payload, Source};
pub use replica::Replica;
pub use subject::{DEFAULT_GROUP, Group, Limits, Track, User, Uuid};

/// The current Unix time in seconds, for the `now` arguments.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Whether an optional expiry has passed: a node expiring at `t` is absent
/// from `now >= t` on.
pub(crate) fn expired(expires_at: Option<u64>, now: u64) -> bool {
    expires_at.is_some_and(|at| at <= now)
}
