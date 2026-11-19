//! Player actions for players on another peer of a cluster.
//!
//! Every [`super::Players`] method that acts on a player looks the player up
//! first: a player of this peer gets the action through its session's
//! channel, as on a single Rift; a player of another peer gets it as a
//! [`WireRequest`] that a [`PlayerRouter`] delivers to that peer, whose
//! [`super::Players::handle_remote`] runs it and answers with a
//! [`WireResult`]. Broadcasts go to every peer, and each delivers to its own
//! players.
//!
//! A single Rift routes nothing: [`LocalRouter`] has no peers.
use super::PlayerAction;
use crate::{dialog::Owner, extensions::Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeSet, fmt, future::Future, io, pin::Pin};

/// What a [`PlayerRouter`] or [`SessionHooks`] returns.
pub type RouteFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// A request for the peer that holds a player.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireRequest {
    /// The incarnation of the receiving peer the sender addressed. Another
    /// incarnation refuses the request: its connection ids name other
    /// sessions.
    pub incarnation: u64,
    pub body: WireBody,
}

/// What a [`WireRequest`] asks for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
// One short-lived value per routed request; boxing would only add an
// allocation to each.
#[allow(clippy::large_enum_variant)]
pub enum WireBody {
    /// One action for one session. The UUID guards against a connection id
    /// that a session of another player holds.
    Action {
        connection: u64,
        uuid: [u8; 16],
        action: PlayerAction,
    },
    /// An action for every player of the receiving peer, or those on one of
    /// `servers` (a server and the instances of a service group).
    Broadcast {
        servers: Option<BTreeSet<String>>,
        action: PlayerAction,
    },
    /// Move a session to a backend through its control queue, as
    /// `rift admin transfer` (`admin`) or BungeeCord `ConnectOther` do.
    Transfer {
        connection: u64,
        backend: String,
        admin: bool,
    },
    /// A click on a dialog the receiving peer showed on another peer's
    /// player. `context.dialog` holds the click; `plugin` is the owner, the
    /// configuration script for `None`.
    DialogClick {
        plugin: Option<String>,
        context: Context,
    },
}

/// The answer to a [`WireRequest`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireResult {
    /// Whether the session took the action: false when it left, cannot show
    /// it, or has a full queue.
    Sent(bool),
    /// How many players took a broadcast.
    Count(usize),
    /// A transfer's answer, as `rift admin transfer` reports it.
    Transfer(Result<Value, String>),
    /// A refusal with its reason, such as the boss bar limit.
    Error(String),
    /// Addressed to another incarnation, or to a connection id this peer did
    /// not mint.
    Stale,
}

/// Why a request did not reach its peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteError {
    /// No live link to the peer.
    Unreachable(u16),
    /// The link failed; the reason is for the log.
    Failed(String),
}

impl fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreachable(peer) => write!(
                formatter,
                "peer {} is unreachable",
                crate::players::identity::peer_label(*peer)
            ),
            Self::Failed(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for RouteError {}

/// Delivers requests to other peers. A cluster sends them on the `player`
/// stream of the peer link, and the receiving peer answers with
/// [`super::Players::handle_remote`].
pub trait PlayerRouter: Send + Sync + 'static {
    fn deliver(
        &self,
        peer: u16,
        request: WireRequest,
    ) -> RouteFuture<Result<WireResult, RouteError>>;

    /// The other live peers, for broadcasts.
    fn peers(&self) -> Vec<u16>;
}

/// A single Rift: no other peers.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalRouter;

impl PlayerRouter for LocalRouter {
    fn deliver(&self, peer: u16, _: WireRequest) -> RouteFuture<Result<WireResult, RouteError>> {
        Box::pin(std::future::ready(Err(RouteError::Unreachable(peer))))
    }

    fn peers(&self) -> Vec<u16> {
        Vec::new()
    }
}

/// What remote requests need from the running proxy beyond the player
/// directory: the session control queue that `rift admin transfer` uses, and
/// the extensions that receive dialog clicks. The runtime installs it with
/// [`super::Players::set_session_hooks`].
pub trait SessionHooks: Send + Sync + 'static {
    /// Move this peer's session `connection` to `backend`.
    fn transfer(
        &self,
        connection: u64,
        backend: String,
        admin: bool,
    ) -> RouteFuture<Result<Value, String>>;

    /// Deliver a click on a dialog this peer showed to `owner`.
    fn dialog(&self, context: Context, owner: Owner) -> RouteFuture<io::Result<()>>;
}
