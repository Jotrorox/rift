//! Bus client for Rift permissions: the `hello` handshake, snapshot requests,
//! update subscriptions, the offline policy and `/perms` forwarding, feeding a
//! [`rift_permissions::Replica`]. See `docs/permissions-protocol.md` for the
//! protocol and `docs/permissions-clients.md` for a guide.
//!
//! ```rust,no_run
//! use rift_permissions::{ContextSet, Uuid};
//! use rift_permissions_client::{Client, Config, RiftPermissions};
//!
//! # async fn example() -> Result<(), rift_permissions_client::Error> {
//! let config = Config::new("proxy.internal:7443", "proxy.internal", "certs/ca.pem",
//!                          "RIFT_SURVIVAL_1_TOKEN", "survival-1");
//! let perms = Client::start(config)?;
//! let player = Uuid::parse("0123456789abcdef0123456789abcdef").unwrap();
//! perms.join(player).await; // at login: waits up to 2 s for the snapshot
//! let fly = perms.check(player, "essentials.fly", &ContextSet::new()).as_bool(false);
//! perms.leave(player); // at logout
//! perms.shutdown();
//! # Ok(()) }
//! ```

mod client;
mod config;
mod listeners;

use std::{fmt, future::Future, io, pin::Pin, sync::Arc};

use rift_permissions::{
    Applied, Change, ChangeEvent, ContextSet, MetaView, Tristate, Uuid,
    wire::{ErrorReply, GroupInfo},
};

pub use client::Client;
pub use config::{Config, OfflinePolicy};
pub use listeners::{Listener, Listeners};

/// A boxed, sendable future, so [`RiftPermissions`] works as a trait object.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The permission API a backend plugin programs against: [`Client`] on a
/// server, `rift_permissions_testing::TestPermissions` in unit tests.
///
/// Reads answer from memory and never wait on the network. `contexts` is the
/// player's active context set (`world`, `gamemode`, custom keys); [`Client`]
/// adds this backend's `server` when the set does not name one.
pub trait RiftPermissions: Send + Sync {
    /// The answer for `node`; `Undefined` when nothing matches or there is
    /// no data for the player, so the caller's own default applies.
    fn check(&self, uuid: Uuid, node: &str, contexts: &ContextSet) -> Tristate;

    /// Prefix, suffix, meta values and primary group; `None` without data.
    fn meta(&self, uuid: Uuid, contexts: &ContextSet) -> Option<MetaView>;

    fn primary_group(&self, uuid: Uuid) -> Option<String>;

    /// The groups the player is in for `contexts`, in rank order.
    fn groups(&self, uuid: Uuid, contexts: &ContextSet) -> Vec<String>;

    /// Every group with weight, display name and decorations.
    fn group_list(&self) -> Arc<Vec<GroupInfo>>;

    /// Call `listener` for every committed change, until the returned
    /// [`Listener`] is dropped. Listeners run on the client's task: keep them
    /// short and hand slow work to your own task.
    fn on_change(&self, listener: Box<dyn Fn(&ChangeEvent) + Send + Sync>) -> Listener;

    /// Apply a change on the proxy, on behalf of the player `actor` (who must
    /// hold the matching `rift.perms.*` node) or of this backend when `None`.
    /// Resolves once the proxy has committed it.
    fn apply(&self, actor: Option<Uuid>, change: Change) -> BoxFuture<'_, Result<Applied, Error>>;

    /// The newest store revision this replica has seen.
    fn revision(&self) -> u64;
}

/// Why a client operation failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The configuration, token variable, certificate or an offline-policy
    /// file is unusable.
    Config(String),
    /// Not connected to the proxy, or nothing on the bus answers permission
    /// requests.
    Unavailable,
    /// No reply in time. A timed-out write may still have been applied:
    /// compare revisions before retrying.
    Timeout,
    /// The proxy refused the request.
    Rejected(ErrorReply),
    /// The connection failed or a reply was malformed.
    Transport(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "invalid permissions configuration: {message}"),
            Self::Unavailable => f.write_str("permissions unavailable: the proxy is not reachable"),
            Self::Timeout => f.write_str("the proxy did not answer in time"),
            Self::Rejected(reply) => write!(f, "the proxy refused: {reply}"),
            Self::Transport(error) => write!(f, "permissions transport: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(reply) => Some(reply),
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}
