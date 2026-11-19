//! openraft's RPCs over peer links.
//!
//! Every Raft message is one request on the `raft` stream kind, encoded as
//! a [`RaftRequest`]; snapshots go over the bulk connection in chunks (see
//! [`super::snapshot`]). The same stream also carries what a follower asks
//! of the leader (forwarded proposals, read barriers, admin commands) and
//! the leader's assignment of a Raft id to a peer being added.
use super::{
    PeerId, PeerNode, TypeConfig, Vote,
    link::{Links, StreamKind, remote_error},
    operations, snapshot,
    state::{Proposal, Reply, SnapshotFile},
};
use bytes::Bytes;
use openraft::{
    RaftNetworkFactory, RaftNetworkV2,
    errors::{NetworkError, RPCError, ReplicationClosed, StreamingError, Unreachable},
    network::RPCOption,
    raft::{
        AppendEntriesRequest, AppendEntriesResponse, AppendResult, SnapshotResponse,
        TransferLeaderRequest, TransferLeaderResponse, VoteRequest, VoteResponse,
    },
    type_config::alias::SnapshotOf,
};
use serde::{Deserialize, Serialize};
use std::{future::Future, io, time::Duration};

#[derive(Debug, Serialize, Deserialize)]
pub enum RaftRequest {
    Append(AppendEntriesRequest<TypeConfig>),
    Vote(VoteRequest<TypeConfig>),
    PreVote(VoteRequest<TypeConfig>),
    TransferLeader(TransferLeaderRequest<TypeConfig>),
    /// A follower's proposal for the leader to append.
    Propose(Proposal),
    /// The index a linearizable read must wait for, from the leader.
    ReadIndex,
    /// A member adds this (new) peer under `raft_id`.
    Assign {
        cluster_id: String,
        raft_id: PeerId,
    },
    /// `rift admin cluster ...` forwarded to the leader.
    Admin(Vec<String>),
    /// A diverged peer asks the leader for a copy at or after `min_index`.
    Repair {
        min_index: u64,
    },
    /// An operation on the instance database for the leader to execute.
    Operate(operations::Request),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum RaftResponse {
    Append(AppendEntriesResponse<TypeConfig>),
    Vote(VoteResponse<TypeConfig>),
    TransferLeader(TransferLeaderResponse<TypeConfig>),
    Proposed { index: u64, reply: Reply },
    NotLeader { leader: Option<PeerId> },
    ReadIndex(u64),
    Admin(serde_json::Value),
    Done,
    Operated(operations::Response),
}

/// Creates a [`PeerClient`] per replication target.
pub struct NetworkFactory {
    links: Links,
}

impl NetworkFactory {
    pub fn new(links: Links) -> Self {
        Self { links }
    }
}

impl RaftNetworkFactory<TypeConfig> for NetworkFactory {
    type Network = PeerClient;

    async fn new_client(&mut self, target: PeerId, node: &PeerNode) -> Self::Network {
        PeerClient {
            links: self.links.clone(),
            target,
            name: node.name.clone(),
        }
    }
}

/// Sends one peer the RPCs openraft makes. Connections are the links'
/// business: this never dials by itself.
pub struct PeerClient {
    links: Links,
    target: PeerId,
    name: String,
}

impl PeerClient {
    async fn call(
        &self,
        request: &RaftRequest,
        timeout: Duration,
    ) -> Result<RaftResponse, RPCError<TypeConfig>> {
        let body = serde_json::to_vec(request)
            .map_err(|error| RPCError::Network(NetworkError::new(&error)))?;
        match self
            .links
            .request(&self.name, StreamKind::Raft, Bytes::from(body), timeout)
            .await
        {
            Ok(reply) => serde_json::from_slice(&reply)
                .map_err(|error| RPCError::Network(NetworkError::new(&error))),
            // The peer answered: retry soon. Anything else means it was not
            // reached: back off.
            Err(error) if remote_error(&error).is_some() => {
                Err(RPCError::Network(NetworkError::new(&error)))
            }
            Err(error) => Err(RPCError::Unreachable(Unreachable::new(&error))),
        }
    }

    fn unexpected(&self, response: &RaftResponse) -> RPCError<TypeConfig> {
        RPCError::Network(NetworkError::from_string(format!(
            "peer {} (raft id {}) answered {response:?}",
            self.name, self.target
        )))
    }
}

impl RaftNetworkV2<TypeConfig> for PeerClient {
    type SnapshotData = SnapshotFile;

    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<AppendResult<TypeConfig>, RPCError<TypeConfig>> {
        let prev = rpc.prev_log_id;
        let last = rpc.last_log_id();
        match self
            .call(&RaftRequest::Append(rpc), option.soft_ttl())
            .await?
        {
            RaftResponse::Append(response) => Ok(response.into_append_result(prev, last)),
            other => Err(self.unexpected(&other)),
        }
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        match self
            .call(&RaftRequest::Vote(rpc), option.soft_ttl())
            .await?
        {
            RaftResponse::Vote(response) => Ok(response),
            other => Err(self.unexpected(&other)),
        }
    }

    async fn pre_vote(
        &mut self,
        rpc: VoteRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        match self
            .call(&RaftRequest::PreVote(rpc), option.soft_ttl())
            .await?
        {
            RaftResponse::Vote(response) => Ok(response),
            other => Err(self.unexpected(&other)),
        }
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote,
        snapshot: SnapshotOf<TypeConfig, SnapshotFile>,
        cancel: impl Future<Output = ReplicationClosed> + openraft::OptionalSend + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<TypeConfig>, StreamingError<TypeConfig>> {
        let per_chunk = option.soft_ttl().max(Duration::from_secs(10));
        snapshot::send(
            &self.links,
            &self.name,
            vote,
            snapshot,
            false,
            cancel,
            per_chunk,
        )
        .await
        .map(SnapshotResponse::new)
        .map_err(snapshot::SendError::streaming)
    }

    async fn transfer_leader(
        &mut self,
        rpc: TransferLeaderRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<TransferLeaderResponse<TypeConfig>, RPCError<TypeConfig>> {
        match self
            .call(&RaftRequest::TransferLeader(rpc), option.soft_ttl())
            .await?
        {
            RaftResponse::TransferLeader(response) => Ok(response),
            other => Err(self.unexpected(&other)),
        }
    }
}

/// An io error for a link failure, keeping the peer's own message.
pub(crate) fn link_error(error: io::Error) -> String {
    match remote_error(&error) {
        Some(remote) => remote.message.clone(),
        None => error.to_string(),
    }
}
