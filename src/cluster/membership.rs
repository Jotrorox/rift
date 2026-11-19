//! `rift admin cluster ...`: membership changes and cluster operations.
//!
//! Membership is changed only by these commands, never by editing
//! `rift.lua`. Changes run on the leader (a follower forwards them), one at
//! a time, and the leader refuses a change that would leave fewer than three
//! voters (a witness counts as one) unless it is given `--force`:
//!
//! ```text
//! cluster                              status: members, roles, leader, term, indexes, lag
//! cluster add <name> <host:port> [--witness]   joins as a learner and catches up
//! cluster promote <name>               a caught-up learner becomes a voter
//! cluster demote <name> [--force]      a voter becomes a learner
//! cluster remove <name> [--force]      leaves the membership
//! cluster replace <name> [--force]     re-adds a peer that lost its Raft file under a new id
//! cluster leave <name>                 hands leadership away if the peer has it
//! cluster transfer-leader [<name>]     moves leadership to a voter
//! cluster backup <file>                a consistent copy of the state, after a linearizable read
//! ```
use super::{
    PeerId, PeerNode,
    network::{RaftRequest, RaftResponse, link_error},
    node::{Cluster, is_refusal},
    settings::valid_peer_name,
    state::{Entry, PeerEntry},
};
use openraft::ChangeMembers;
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, time::Duration};

pub const USAGE: &str = "usage: cluster [status] | cluster add <name> <host:port> [--witness] | cluster promote <name> | cluster demote <name> [--force] | cluster remove <name> [--force] | cluster replace <name> [--force] | cluster leave <name> | cluster transfer-leader [<name>] | cluster backup <file>";
/// How far behind the leader a learner may be when it is promoted.
const PROMOTE_LAG: u64 = 1000;
const ADMIN_TIMEOUT: Duration = Duration::from_secs(120);

impl Cluster {
    /// Run `rift admin cluster <args>` on this peer.
    pub async fn admin(&self, args: &[String]) -> Result<Value, String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            [] | ["status"] => Ok(self.status()),
            ["backup", file] => self.backup(Path::new(file)).await,
            [command, ..]
                if matches!(
                    *command,
                    "add"
                        | "promote"
                        | "demote"
                        | "remove"
                        | "replace"
                        | "leave"
                        | "transfer-leader"
                ) =>
            {
                self.on_leader(&args).await
            }
            _ => Err(USAGE.into()),
        }
    }

    /// Run a membership command on the leader, forwarding it if need be.
    async fn on_leader(&self, args: &[&str]) -> Result<Value, String> {
        let owned: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        if self.leadership().is_leader {
            return self.admin_here(&owned).await;
        }
        let leader = self
            .leadership()
            .leader_name
            .ok_or("there is no leader right now; retry once one is elected")?;
        match self
            .raft_request(&leader, &RaftRequest::Admin(owned), ADMIN_TIMEOUT)
            .await
        {
            Ok(RaftResponse::Admin(value)) => Ok(value),
            Ok(other) => Err(format!(
                "unexpected answer from the leader {leader}: {other:?}"
            )),
            Err(error) if is_refusal(&error) => Err(link_error(error)),
            Err(error) => Err(format!("cannot reach the leader {leader}: {error}")),
        }
    }

    /// A membership command on this peer, which must lead.
    pub(crate) async fn admin_here(&self, args: &[String]) -> Result<Value, String> {
        if !self.leadership().is_leader {
            return Err("this peer is no longer the leader; retry".into());
        }
        let _change = self
            .inner
            .membership
            .try_lock()
            .map_err(|_| "another membership change is running; retry when it is done")?;
        if self.membership_changing() {
            return Err(
                "the last membership change is still being committed; retry shortly".into(),
            );
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let force = args.contains(&"--force");
        let positional: Vec<&str> = args
            .iter()
            .copied()
            .filter(|arg| !arg.starts_with("--"))
            .collect();
        let flags_valid = args
            .iter()
            .filter(|arg| arg.starts_with("--"))
            .all(|flag| matches!(*flag, "--force" | "--witness"));
        if !flags_valid {
            return Err(USAGE.into());
        }
        match positional.as_slice() {
            ["add", name, address] if !force => {
                self.add(name, address, args.contains(&"--witness")).await
            }
            ["promote", name] if !force => self.promote(name).await,
            ["demote", name] => self.demote(name, force).await,
            ["remove", name] => self.remove(name, force).await,
            ["replace", name] => self.replace(name, force).await,
            ["leave", name] if !force => self.leave(name).await,
            ["transfer-leader"] if !force => self.transfer_to(None).await,
            ["transfer-leader", name] if !force => self.transfer_to(Some(*name)).await,
            _ => Err(USAGE.into()),
        }
    }

    async fn add(&self, name: &str, address: &str, witness: bool) -> Result<Value, String> {
        if !valid_peer_name(name) {
            return Err(format!(
                "{name:?}: peer names are 1..=64 lowercase letters, digits, '-' or '_', and not \"local\""
            ));
        }
        if !address.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|port| port > 0)
        }) {
            return Err(format!("{address:?}: expected host:port"));
        }
        if self.member_id(name).is_some() {
            return Err(format!("{name} is already a member"));
        }
        let rows = self.state().peers().map_err(|error| error.to_string())?;
        let raft_id = match rows.iter().find(|row| row.name == name) {
            // Added before, but the peer was not reached: reuse its id.
            Some(row) => row.raft_id,
            None => {
                let entry = Entry::Peer(PeerEntry::Added {
                    v: 1,
                    name: name.to_owned(),
                    address: address.to_owned(),
                    witness,
                });
                let value = self
                    .propose(entry)
                    .await
                    .map_err(|error| error.to_string())?
                    .into_result()?;
                value["raft_id"]
                    .as_u64()
                    .ok_or("the peer entry returned no raft id")?
            }
        };
        let cluster_id = self.cluster_id().ok_or("this peer has no cluster id")?;
        if let Err(error) = self.assign(name, address, &cluster_id, raft_id).await {
            return Err(format!(
                "{name} has raft id {raft_id} but was not reached ({error}); start it with its cluster.toml and run `rift admin cluster add {name} {address}` again"
            ));
        }
        let raft = self.raft().ok_or("not in a cluster")?;
        let node = PeerNode {
            name: name.to_owned(),
            address: address.to_owned(),
        };
        raft.add_learner(raft_id, node, false)
            .await
            .map_err(|error| error.to_string())?;
        log_info!("cluster: added {name} at {address} as learner (raft id {raft_id})");
        Ok(json!({"added": name, "raft_id": raft_id, "role": "learner", "witness": witness}))
    }

    /// Reach the new peer at `address` and hand it its Raft id.
    async fn assign(
        &self,
        name: &str,
        address: &str,
        cluster_id: &str,
        raft_id: PeerId,
    ) -> Result<(), String> {
        let presented = self
            .links()
            .connect_address(address, Duration::from_secs(10))
            .await
            .map_err(|error| error.to_string())?;
        if presented != name {
            return Err(format!("{address} is peer {presented}, not {name}"));
        }
        let request = RaftRequest::Assign {
            cluster_id: cluster_id.to_owned(),
            raft_id,
        };
        match self
            .raft_request(name, &request, Duration::from_secs(30))
            .await
        {
            Ok(RaftResponse::Done) => Ok(()),
            Ok(other) => Err(format!("unexpected answer: {other:?}")),
            Err(error) => Err(link_error(error)),
        }
    }

    async fn promote(&self, name: &str) -> Result<Value, String> {
        let id = self
            .member_id(name)
            .ok_or_else(|| format!("{name} is not a member"))?;
        let metrics = self.metrics().ok_or("not in a cluster")?;
        if metrics
            .membership_config
            .voter_ids()
            .any(|voter| voter == id)
        {
            return Err(format!("{name} already votes"));
        }
        let last = metrics.last_log_index.unwrap_or(0);
        let matched = metrics
            .replication
            .as_ref()
            .and_then(|replication| replication.get(&id).copied().flatten())
            .map_or(0, |log_id| log_id.index);
        if last.saturating_sub(matched) > PROMOTE_LAG {
            return Err(format!(
                "{name} is still catching up ({} entries behind); retry when it has",
                last - matched
            ));
        }
        let raft = self.raft().ok_or("not in a cluster")?;
        raft.change_membership(
            ChangeMembers::<PeerId, PeerNode>::AddVoterIds(BTreeSet::from([id])),
            true,
        )
        .await
        .map_err(|error| error.to_string())?;
        log_info!("cluster: promoted {name} to voter");
        Ok(json!({"promoted": name, "raft_id": id}))
    }

    async fn demote(&self, name: &str, force: bool) -> Result<Value, String> {
        let id = self
            .member_id(name)
            .ok_or_else(|| format!("{name} is not a member"))?;
        let voters = self.voters();
        if !voters.contains(&id) {
            return Err(format!("{name} does not vote"));
        }
        check_voters(voters.len() - 1, voters.len(), force)?;
        let raft = self.raft().ok_or("not in a cluster")?;
        raft.change_membership(
            ChangeMembers::<PeerId, PeerNode>::RemoveVoters(BTreeSet::from([id])),
            true,
        )
        .await
        .map_err(|error| error.to_string())?;
        log_info!("cluster: demoted {name} to learner");
        Ok(json!({"demoted": name, "raft_id": id}))
    }

    async fn remove(&self, name: &str, force: bool) -> Result<Value, String> {
        let raft = self.raft().ok_or("not in a cluster")?;
        let voters = self.voters();
        match self.member_id(name) {
            Some(id) if voters.contains(&id) => {
                check_voters(voters.len() - 1, voters.len(), force)?;
                raft.change_membership(
                    ChangeMembers::<PeerId, PeerNode>::RemoveVoters(BTreeSet::from([id])),
                    false,
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            Some(id) => {
                raft.change_membership(
                    ChangeMembers::<PeerId, PeerNode>::RemoveNodes(BTreeSet::from([id])),
                    false,
                )
                .await
                .map_err(|error| error.to_string())?;
            }
            None => {}
        }
        let removed = self
            .propose(Entry::Peer(PeerEntry::Removed {
                v: 1,
                name: name.to_owned(),
            }))
            .await
            .map_err(|error| error.to_string())?
            .into_result()?;
        log_info!("cluster: removed {name}");
        Ok(json!({"removed": name, "known": removed}))
    }

    /// Re-add a peer that lost its Raft file: remove its old id, then add it
    /// again under a new one. Its old id is never used again.
    async fn replace(&self, name: &str, force: bool) -> Result<Value, String> {
        let rows = self.state().peers().map_err(|error| error.to_string())?;
        let row = rows
            .into_iter()
            .find(|row| row.name == name)
            .ok_or_else(|| format!("{name} is not a known peer"))?;
        let removed = self.remove(name, force).await?;
        match self.add(name, &row.address, row.witness).await {
            Ok(added) => Ok(json!({"replaced": name, "removed": removed, "added": added})),
            Err(error) => Ok(json!({"replaced": name, "removed": removed, "pending": error})),
        }
    }

    /// Step 2 part of a drain: leadership moves away from the peer.
    async fn leave(&self, name: &str) -> Result<Value, String> {
        let id = self
            .member_id(name)
            .ok_or_else(|| format!("{name} is not a member"))?;
        if Some(id) == self.raft_id() {
            let target = self.transfer_to(None).await?;
            return Ok(json!({"leaving": name, "leadership": target}));
        }
        Ok(json!({"leaving": name, "leadership": "elsewhere already"}))
    }

    async fn transfer_to(&self, name: Option<&str>) -> Result<Value, String> {
        let target = match name {
            Some(name) => {
                let id = self
                    .member_id(name)
                    .ok_or_else(|| format!("{name} is not a member"))?;
                if !self.voters().contains(&id) {
                    return Err(format!("{name} does not vote and cannot lead"));
                }
                id
            }
            None => self
                .successor()
                .ok_or("no other voter is reachable and caught up")?,
        };
        if Some(target) == self.raft_id() {
            return Ok(json!({"leader": self.name()}));
        }
        self.transfer_leadership(target)
            .await
            .map_err(|error| error.to_string())?;
        let name = self.peer_name(target).unwrap_or_else(|| target.to_string());
        log_info!("cluster: handing leadership to {name}");
        Ok(json!({"transferring_to": name}))
    }

    async fn backup(&self, file: &Path) -> Result<Value, String> {
        if !file.is_absolute() {
            return Err("cluster backup: give an absolute file path".into());
        }
        let barrier = self
            .read_barrier()
            .await
            .map_err(|error| error.to_string())?;
        let index = self
            .state()
            .backup_to(file)
            .map_err(|error| error.to_string())?;
        Ok(json!({"file": file.display().to_string(), "applied_index": index.max(barrier)}))
    }

    /// The best voter to lead after this one: reachable, caught up, and not
    /// a witness if a player-serving voter qualifies.
    fn successor(&self) -> Option<PeerId> {
        let metrics = self.metrics()?;
        let rows = self.state().peers().ok()?;
        let last = metrics.last_log_index.unwrap_or(0);
        let mut candidates: Vec<(bool, u64, PeerId)> = metrics
            .membership_config
            .voter_ids()
            .filter(|id| *id != metrics.id)
            .filter(|id| {
                self.peer_name(*id)
                    .is_some_and(|name| self.links().is_up(&name))
            })
            .map(|id| {
                let witness = rows.iter().any(|row| row.raft_id == id && row.witness);
                let matched = metrics
                    .replication
                    .as_ref()
                    .and_then(|replication| replication.get(&id).copied().flatten())
                    .map_or(0, |log_id| log_id.index);
                (witness, last.saturating_sub(matched), id)
            })
            .collect();
        candidates.sort();
        candidates.first().map(|(_, _, id)| *id)
    }

    fn member_id(&self, name: &str) -> Option<PeerId> {
        let membership = self.membership()?;
        membership
            .nodes()
            .find(|(_, node)| node.name == name)
            .map(|(id, _)| *id)
    }

    fn voters(&self) -> BTreeSet<PeerId> {
        self.membership()
            .map(|membership| membership.voter_ids().collect())
            .unwrap_or_default()
    }
}

/// The voter rule: a change may not leave fewer than three voters unless
/// it is forced, but growing from fewer is always fine.
fn check_voters(remaining: usize, current: usize, force: bool) -> Result<(), String> {
    if remaining < current && remaining < 3 && !force {
        return Err(format!(
            "this leaves {remaining} voter(s); three (or two and a witness) are needed to survive a failure; add --force to do it anyway"
        ));
    }
    if remaining == 0 {
        return Err("the last voter cannot leave".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voter_rule_allows_growth_and_refuses_shrinking_below_three() {
        assert!(check_voters(2, 1, false).is_ok());
        assert!(check_voters(3, 4, false).is_ok());
        assert!(check_voters(2, 3, false).is_err());
        assert!(check_voters(2, 3, true).is_ok());
        assert!(check_voters(0, 1, true).is_err());
    }
}
