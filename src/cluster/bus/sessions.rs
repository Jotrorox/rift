//! Node sessions and owner routing (docs/clustering.md, "Node sessions").
//!
//! A node agent has one session at a time. On a single Rift the QUIC
//! listener closes the older of two sessions itself; in a cluster the agent
//! may connect to any peer, so each session is claimed with a `bus` entry.
//! The claim that applies last holds the node: commands go to its peer, and
//! the peer whose claimed session it replaced closes that session (see
//! `tables::replace_sessions`).
use super::{BusState, LocalSession, tables::BusEntry};
use crate::cluster::Reply;
use crate::messaging::{OwnerRouter, PeerId, quic::NodeSessionObserver};
use std::{sync::Arc, time::Duration};

/// Reports this peer's node sessions to the cluster.
pub(super) struct Observer {
    pub(super) state: Arc<BusState>,
}

impl NodeSessionObserver for Observer {
    fn opened(&self, name: &str, session: u64) {
        let Some(node) = name.strip_prefix("node.") else {
            return;
        };
        self.state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                node.to_owned(),
                LocalSession {
                    id: session,
                    claimed: false,
                },
            );
        spawn_claim(&self.state, node.to_owned(), session);
    }

    fn closed(&self, name: &str, session: u64) {
        let Some(node) = name.strip_prefix("node.") else {
            return;
        };
        let current = {
            let mut sessions = self
                .state
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match sessions.get(node) {
                Some(local) if local.id == session => sessions.remove(node).is_some(),
                _ => false,
            }
        };
        // A session closed because another peer's replaced it was already
        // forgotten: its claim is gone, nothing to release.
        if !current {
            return;
        }
        let Some(cluster) = self.state.cluster() else {
            return;
        };
        let entry = BusEntry::NodeRelease {
            node: node.to_owned(),
            peer: cluster.name().to_owned(),
            incarnation: cluster.incarnation(),
            session,
        };
        tokio::spawn(async move {
            if let Err(error) = cluster.propose(entry.entry()).await {
                log_debug!("cluster bus: releasing a node session: {error}");
            }
        });
    }
}

/// Whether `session` is still this peer's session of `node`.
fn is_current(state: &BusState, node: &str, session: u64) -> bool {
    state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(node)
        .is_some_and(|local| local.id == session)
}

/// Claim `node` for `session` until the claim applies or the session ends.
pub(super) fn spawn_claim(state: &Arc<BusState>, node: String, session: u64) {
    let state = state.clone();
    tokio::spawn(async move {
        let mut delay = Duration::from_millis(100);
        loop {
            if !is_current(&state, &node, session) {
                return;
            }
            let Some(cluster) = state.cluster() else {
                return;
            };
            let entry = BusEntry::NodeClaim {
                node: node.clone(),
                peer: cluster.name().to_owned(),
                incarnation: cluster.incarnation(),
                session,
            };
            match cluster.propose(entry.entry()).await {
                Ok(Reply::Ok(_)) => return,
                Ok(Reply::Err(reason)) => {
                    log_warn!(
                        "cluster bus: the session claim of node {node} was refused: {reason}"
                    );
                    return;
                }
                Err(error) => log_debug!("cluster bus: claiming node {node}: {error}; retrying"),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    });
}

/// Routes `rift.node.<n>.cmd` to the peer holding node `n`'s session, or,
/// when no `rift node` agent holds it, to peer `n` itself if it is a peer
/// that serves players (its embedded agent, docs/clustering.md "Servers on
/// the peers' own hosts"); `rift.node.<n>.state` goes to the leader. A node
/// nobody holds, or no leader, keeps the message on this peer, where it
/// finds no subscriber.
pub(super) struct Router {
    pub(super) state: Arc<BusState>,
}

impl Router {
    /// `node` if it names a peer that runs an embedded agent.
    fn peer_node(&self, node: &str) -> Option<String> {
        let cluster = self.state.cluster()?;
        let rows = cluster.state().peers().ok()?;
        rows.iter()
            .any(|row| row.name == node && !row.witness)
            .then(|| node.to_owned())
    }
}

impl OwnerRouter for Router {
    fn route(&self, subject: &str) -> Option<PeerId> {
        let (node, kind) = subject.strip_prefix("rift.node.")?.rsplit_once('.')?;
        match kind {
            "cmd" => self
                .state
                .session_holder(node)
                .or_else(|| self.peer_node(node))
                .map(PeerId::from),
            "state" => self
                .state
                .cluster()?
                .leadership()
                .leader_name
                .map(PeerId::from),
            _ => None,
        }
    }
}
