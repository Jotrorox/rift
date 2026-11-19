//! Where scheduled jobs run in a cluster (docs/clustering.md, decision 8):
//! `rift.every` jobs of the configuration script and of Lua plugins, and
//! `extensions.jobs`, take a `scope`.
//!
//! - `"cluster"`, the default: the job runs on one peer, the leader. Every
//!   peer keeps its timer, and before each tick it checks that it leads and
//!   holds the leader lease ([`Cluster::is_leader_with_lease`]);
//!   only that peer runs the tick. When leadership moves, the new leader's
//!   next tick runs and the old leader's do not.
//! - `"peer"`: every peer runs the job, as each process did before clusters.
//!
//! Without a cluster every job runs, whatever its scope.
use crate::cluster::Cluster;

/// Where a scheduled job runs in a cluster.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Scope {
    /// On the leader only.
    #[default]
    Cluster,
    /// On every peer.
    Peer,
}

impl Scope {
    /// `"cluster"` or `"peer"`; `None` (the field left out) is the default.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("cluster") => Ok(Self::Cluster),
            Some("peer") => Ok(Self::Peer),
            Some(other) => Err(format!(
                "job scope must be \"cluster\" (run on the leader only) or \"peer\" (run on every peer), not {other:?}"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Cluster => "cluster",
            Self::Peer => "peer",
        }
    }
}

/// Whether a tick of a job with `scope` runs on this process now.
pub fn due(scope: Scope) -> bool {
    due_on(crate::cluster::global(), scope)
}

/// [`due`] on `cluster` (`None`: no cluster, every tick runs).
pub fn due_on(cluster: Option<&Cluster>, scope: Scope) -> bool {
    match (scope, cluster) {
        (Scope::Peer, _) | (Scope::Cluster, None) => true,
        (Scope::Cluster, Some(cluster)) => cluster.is_leader_with_lease(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_parse_and_run_everywhere_without_a_cluster() {
        assert_eq!(Scope::parse(None).unwrap(), Scope::Cluster);
        assert_eq!(Scope::parse(Some("cluster")).unwrap(), Scope::Cluster);
        assert_eq!(Scope::parse(Some("peer")).unwrap(), Scope::Peer);
        assert!(Scope::parse(Some("leader")).is_err());
        assert!(due_on(None, Scope::Cluster));
        assert!(due_on(None, Scope::Peer));
    }
}
