//! Player actions over the `player` stream.
//!
//! [`LinkRouter`] sends a [`WireRequest`] to the peer that holds a player,
//! found by its Raft id in the membership or the peer table, and waits for
//! its [`WireResult`]. The receiving peer runs it with
//! [`Players::handle_remote`], which refuses a request addressed to another
//! incarnation of it (or to a connection id it did not mint) as stale.
//! Broadcasts go to every other peer that is up and not expired.
use crate::{
    cluster::{Cluster, StreamKind, link::remote_error, node},
    extensions::{
        Players,
        routing::{PlayerRouter, RouteError, RouteFuture, WireRequest, WireResult},
    },
    players::identity::MAX_PEER,
};
use bytes::Bytes;
use std::{
    io,
    sync::{Arc, Weak},
    time::Duration,
};

/// How long a routed request waits for the other peer's answer. A transfer
/// answers once the player switched or failed to, which can take a while.
pub const ROUTE_TIMEOUT: Duration = Duration::from_secs(30);

/// Delivers requests for other peers' players on the `player` stream.
///
/// It holds its cluster weakly: the [`Players`] it is installed in outlive
/// the cluster (and the `player` stream's handler holds those players), so
/// a strong handle would keep a stopped peer, its replica and its instance
/// database open. Once the cluster is gone every peer is unreachable.
pub struct LinkRouter {
    cluster: Weak<node::Inner>,
}

impl LinkRouter {
    pub fn new(cluster: Cluster) -> Self {
        Self {
            cluster: Arc::downgrade(&cluster.inner),
        }
    }

    fn cluster(&self) -> Option<Cluster> {
        self.cluster.upgrade().map(|inner| Cluster { inner })
    }
}

/// Whether `error` means the peer could not be reached at all, rather than
/// that it refused or failed the request.
fn unreachable(error: &io::Error) -> bool {
    remote_error(error).is_none()
        && matches!(
            error.kind(),
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::NotConnected
                | io::ErrorKind::TimedOut
                | io::ErrorKind::NotFound
                | io::ErrorKind::BrokenPipe
        )
}

impl PlayerRouter for LinkRouter {
    fn deliver(
        &self,
        peer: u16,
        request: WireRequest,
    ) -> RouteFuture<Result<WireResult, RouteError>> {
        let cluster = self.cluster();
        Box::pin(async move {
            let Some(cluster) = cluster else {
                return Err(RouteError::Unreachable(peer));
            };
            let Some(name) = cluster.peer_name(u64::from(peer)) else {
                return Err(RouteError::Unreachable(peer));
            };
            if name == cluster.name() {
                return Err(RouteError::Failed(format!(
                    "a request for peer {peer} reached that peer's own router"
                )));
            }
            let body = serde_json::to_vec(&request)
                .map_err(|error| RouteError::Failed(format!("player request: {error}")))?;
            match cluster
                .links()
                .request(&name, StreamKind::Player, Bytes::from(body), ROUTE_TIMEOUT)
                .await
            {
                Ok(reply) => serde_json::from_slice(&reply)
                    .map_err(|error| RouteError::Failed(format!("answer from {name}: {error}"))),
                Err(error) if unreachable(&error) => {
                    log_debug!("cluster: player request to {name}: {error}");
                    Err(RouteError::Unreachable(peer))
                }
                Err(error) => Err(RouteError::Failed(format!("{name}: {error}"))),
            }
        })
    }

    fn peers(&self) -> Vec<u16> {
        let Some(cluster) = self.cluster() else {
            return Vec::new();
        };
        let me = cluster.raft_id();
        cluster
            .peers()
            .unwrap_or_default()
            .into_iter()
            .filter(|peer| !peer.this_peer && peer.up && !peer.expired)
            .filter_map(|peer| peer.raft_id)
            .filter(|id| Some(*id) != me)
            .filter_map(|id| u16::try_from(id).ok())
            .filter(|id| (1..=MAX_PEER).contains(id))
            .collect()
    }
}

/// Answer requests other peers route to this peer's players.
pub fn serve(cluster: &Cluster, players: &Players) {
    let players = players.clone();
    cluster
        .links()
        .on_request(StreamKind::Player, move |_peer, body| {
            let players = players.clone();
            async move {
                let request: WireRequest = serde_json::from_slice(&body)
                    .map_err(|error| format!("player request: {error}"))?;
                let result = players.handle_remote(request).await;
                serde_json::to_vec(&result)
                    .map(Bytes::from)
                    .map_err(|error| format!("player answer: {error}"))
            }
        });
}
