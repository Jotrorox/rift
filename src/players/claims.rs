//! The part of presence that needs consensus: who holds which player name
//! and UUID.
//!
//! A login claims its lowercase name before it contacts a backend, then its
//! UUID and name together once the identity is confirmed. Every claim is a
//! [`ClaimOp`] applied to a [`ClaimTable`]. Applying is deterministic: it
//! reads neither the clock nor anything outside the table and the operation,
//! so peers that apply the same operations in the same order agree on every
//! claim, and of two simultaneous logins of one player exactly one wins.
//!
//! [`Claims`] is the seam between the registry and whoever orders the
//! operations. [`LocalClaims`] applies them at once under a mutex, which is
//! what a single Rift has always done. A cluster implements [`Claims`] by
//! proposing each operation to its Raft log and applying the same
//! [`ClaimTable`] when the entry commits; `src/players/memory.rs` simulates
//! that for tests.
use super::identity::Identity;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll, Waker},
};
use tokio::sync::broadcast;

/// What [`Claims`] returns: ready at once on a single Rift, ready once the
/// entry has committed and been applied in a cluster.
pub type ClaimFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Who holds a claim: one login on one peer. `token` is unique within the
/// peer's incarnation, so two logins never share a holder even when they
/// carry no connection id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Holder {
    pub peer: u16,
    pub incarnation: u64,
    /// The session's connection id; 0 when the registration has none.
    pub connection: u64,
    pub token: u64,
}

impl Holder {
    /// A new holder for a login of `identity` on `connection`.
    pub fn new(identity: Identity, connection: u64) -> Self {
        static TOKENS: AtomicU64 = AtomicU64::new(1);
        Self {
            peer: identity.peer,
            incarnation: identity.incarnation,
            connection,
            token: TOKENS.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// One live claim: a pending login's name (`uuid` is `None`), or a player's
/// name and UUID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRecord {
    /// The lowercase name.
    pub key: String,
    /// The name as the player spells it.
    pub name: String,
    pub uuid: Option<[u8; 16]>,
    pub holder: Holder,
}

/// Why a claim was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// Another live claim holds the UUID.
    AlreadyConnected,
    /// Another live claim holds the name.
    DuplicateName,
    /// The network holds `cluster.max_players` players and the claim carries
    /// no bypass.
    NetworkFull,
    /// This peer admits nobody: it is cut off from the majority.
    Fenced,
    /// The claim could not be decided, for example without a quorum. The
    /// reason is for the log.
    Unavailable(String),
}

impl fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyConnected => formatter.write_str("the player is already connected"),
            Self::DuplicateName => formatter.write_str("the name is already in use"),
            Self::NetworkFull => formatter.write_str("the network is full"),
            Self::Fenced => formatter.write_str("this peer is fenced"),
            Self::Unavailable(reason) => write!(formatter, "the claim is undecided: {reason}"),
        }
    }
}

/// One change to the claims, deterministic given the table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimOp {
    /// Claim a pending login's lowercase name.
    Name {
        key: String,
        name: String,
        holder: Holder,
    },
    /// Claim a player's UUID and name. With `upgrade`, a pending name claim
    /// of the same holder becomes this claim without ever being free.
    Player {
        key: String,
        name: String,
        uuid: [u8; 16],
        holder: Holder,
        upgrade: bool,
        /// The login hook let the player past `cap`.
        bypass_cap: bool,
        /// `cluster.max_players` when the claim was made; `None` is no cap.
        cap: Option<usize>,
    },
    /// Claim again what a player held before this peer lost contact: no-op
    /// while the claim is still held by `holder`, a fresh claim once it was
    /// released, and refused when somebody else holds the name or UUID now.
    Reclaim {
        key: String,
        name: String,
        uuid: [u8; 16],
        holder: Holder,
    },
    /// Release the claim of `key` if `holder` still holds it.
    Release { key: String, holder: Holder },
    /// Release every claim of `peer`, or only those of incarnations before
    /// `before`: a peer that expired, or one that started again.
    ExpirePeer { peer: u16, before: Option<u64> },
}

/// A change readers of the claims learn about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimChange {
    Claimed(ClaimRecord),
    Released(ClaimRecord),
}

/// Every live claim. Ordered maps, so that iteration (and a checksum over
/// it) is the same on every peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimTable {
    by_key: BTreeMap<String, ClaimRecord>,
    by_uuid: BTreeMap<[u8; 16], String>,
    players: usize,
}

impl ClaimTable {
    /// Apply `op`. Changes are pushed to `changes` when it is given; a
    /// refused operation changes nothing.
    pub fn apply(
        &mut self,
        op: &ClaimOp,
        mut changes: Option<&mut Vec<ClaimChange>>,
    ) -> Result<(), Refusal> {
        let mut notify = |change: &dyn Fn() -> ClaimChange| {
            if let Some(changes) = changes.as_deref_mut() {
                changes.push(change());
            }
        };
        match op {
            ClaimOp::Name { key, name, holder } => {
                if self.by_key.contains_key(key) {
                    return Err(Refusal::DuplicateName);
                }
                let record = ClaimRecord {
                    key: key.clone(),
                    name: name.clone(),
                    uuid: None,
                    holder: *holder,
                };
                notify(&|| ClaimChange::Claimed(record.clone()));
                self.by_key.insert(key.clone(), record);
                Ok(())
            }
            ClaimOp::Player {
                key,
                name,
                uuid,
                holder,
                upgrade,
                bypass_cap,
                cap,
            } => {
                if self.by_uuid.contains_key(uuid) {
                    return Err(Refusal::AlreadyConnected);
                }
                match self.by_key.get(key) {
                    None => {}
                    Some(record)
                        if *upgrade && record.uuid.is_none() && record.holder == *holder => {}
                    Some(_) => return Err(Refusal::DuplicateName),
                }
                if !bypass_cap && cap.is_some_and(|cap| self.players >= cap) {
                    return Err(Refusal::NetworkFull);
                }
                self.insert_player(key, name, *uuid, *holder, &mut notify);
                Ok(())
            }
            ClaimOp::Reclaim {
                key,
                name,
                uuid,
                holder,
            } => {
                if let Some(record) = self.by_key.get(key)
                    && record.holder == *holder
                    && record.uuid == Some(*uuid)
                {
                    return Ok(());
                }
                if self.by_uuid.contains_key(uuid) {
                    return Err(Refusal::AlreadyConnected);
                }
                if self.by_key.contains_key(key) {
                    return Err(Refusal::DuplicateName);
                }
                // The player is connected already, so no cap applies.
                self.insert_player(key, name, *uuid, *holder, &mut notify);
                Ok(())
            }
            ClaimOp::Release { key, holder } => {
                if self
                    .by_key
                    .get(key)
                    .is_some_and(|record| record.holder == *holder)
                {
                    let record = self.remove(key);
                    notify(&|| ClaimChange::Released(record.clone()));
                }
                Ok(())
            }
            ClaimOp::ExpirePeer { peer, before } => {
                let expired: Vec<String> = self
                    .by_key
                    .values()
                    .filter(|record| {
                        record.holder.peer == *peer
                            && before.is_none_or(|before| record.holder.incarnation < before)
                    })
                    .map(|record| record.key.clone())
                    .collect();
                for key in expired {
                    let record = self.remove(&key);
                    notify(&|| ClaimChange::Released(record.clone()));
                }
                Ok(())
            }
        }
    }

    fn insert_player(
        &mut self,
        key: &str,
        name: &str,
        uuid: [u8; 16],
        holder: Holder,
        notify: &mut impl FnMut(&dyn Fn() -> ClaimChange),
    ) {
        let record = ClaimRecord {
            key: key.to_owned(),
            name: name.to_owned(),
            uuid: Some(uuid),
            holder,
        };
        notify(&|| ClaimChange::Claimed(record.clone()));
        // An upgraded name claim is replaced in place.
        self.by_key.insert(key.to_owned(), record);
        self.by_uuid.insert(uuid, key.to_owned());
        self.players += 1;
    }

    fn remove(&mut self, key: &str) -> ClaimRecord {
        let record = self.by_key.remove(key).expect("claim present");
        if let Some(uuid) = record.uuid {
            self.by_uuid.remove(&uuid);
            self.players -= 1;
        }
        record
    }

    /// The table holding exactly `records`, as a replica stored them: keys
    /// are unique, and so are the UUIDs of claims that the table granted.
    pub fn from_records(records: impl IntoIterator<Item = ClaimRecord>) -> Self {
        let mut table = Self::default();
        for record in records {
            if let Some(uuid) = record.uuid {
                table.by_uuid.insert(uuid, record.key.clone());
                table.players += 1;
            }
            table.by_key.insert(record.key.clone(), record);
        }
        table
    }

    /// Every live claim, by lowercase name.
    pub fn live(&self) -> impl Iterator<Item = &ClaimRecord> {
        self.by_key.values()
    }

    /// The claim holding `uuid`.
    pub fn by_uuid(&self, uuid: &[u8; 16]) -> Option<&ClaimRecord> {
        self.by_key.get(self.by_uuid.get(uuid)?)
    }

    /// Players (not pending names) holding claims.
    pub fn players(&self) -> usize {
        self.players
    }
}

/// Orders claim operations and applies them.
pub trait Claims: Send + Sync + 'static {
    /// Apply `op` once it is decided: at once on a single Rift, once
    /// committed and applied in a cluster. A future dropped before it is
    /// ready must not leave a claim behind: an implementation that commits it
    /// anyway releases it.
    fn propose(&self, op: ClaimOp) -> ClaimFuture<Result<(), Refusal>>;

    /// Release the claim of `key` if `holder` still holds it. Called when a
    /// session's guard drops, so it must not block; it must still apply
    /// eventually.
    fn release(&self, key: String, holder: Holder);

    /// Every live claim.
    fn live(&self) -> Vec<ClaimRecord>;

    /// Claim changes from now on. A receiver that lags reconciles with
    /// [`Claims::live`].
    fn subscribe(&self) -> broadcast::Receiver<ClaimChange>;

    /// Whether other processes claim names in the same table (a cluster):
    /// then every login must claim its name before a backend sees it, even
    /// one that a single Rift passes through without reading its name.
    fn networked(&self) -> bool {
        false
    }

    /// Claim a pending login's lowercase name.
    fn claim_name(
        &self,
        key: String,
        name: String,
        holder: Holder,
    ) -> ClaimFuture<Result<(), Refusal>> {
        self.propose(ClaimOp::Name { key, name, holder })
    }

    /// Claim a player's UUID and name; see [`ClaimOp::Player`].
    #[allow(clippy::too_many_arguments)]
    fn claim_player(
        &self,
        key: String,
        name: String,
        uuid: [u8; 16],
        holder: Holder,
        upgrade: bool,
        bypass_cap: bool,
        cap: Option<usize>,
    ) -> ClaimFuture<Result<(), Refusal>> {
        self.propose(ClaimOp::Player {
            key,
            name,
            uuid,
            holder,
            upgrade,
            bypass_cap,
            cap,
        })
    }

    /// Release the claims of `peer`, or of its incarnations before `before`.
    fn expire_peer(&self, peer: u16, before: Option<u64>) -> ClaimFuture<Result<(), Refusal>> {
        self.propose(ClaimOp::ExpirePeer { peer, before })
    }
}

/// The claims of a single Rift: applied at once, under one lock.
#[derive(Debug)]
pub struct LocalClaims {
    table: Mutex<ClaimTable>,
    changes: broadcast::Sender<ClaimChange>,
}

impl Default for LocalClaims {
    fn default() -> Self {
        Self {
            table: Mutex::default(),
            changes: broadcast::channel(1024).0,
        }
    }
}

impl LocalClaims {
    fn apply(&self, op: &ClaimOp) -> Result<(), Refusal> {
        // Nobody listens on a single Rift unless asked to; then the changes
        // are collected, and not cloned otherwise.
        let mut changes = Vec::new();
        let listening = self.changes.receiver_count() > 0;
        let result = self
            .table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .apply(op, listening.then_some(&mut changes));
        for change in changes {
            let _ = self.changes.send(change);
        }
        result
    }
}

impl Claims for LocalClaims {
    fn propose(&self, op: ClaimOp) -> ClaimFuture<Result<(), Refusal>> {
        Box::pin(std::future::ready(self.apply(&op)))
    }

    fn release(&self, key: String, holder: Holder) {
        let _ = self.apply(&ClaimOp::Release { key, holder });
    }

    fn live(&self) -> Vec<ClaimRecord> {
        self.table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .live()
            .cloned()
            .collect()
    }

    fn subscribe(&self) -> broadcast::Receiver<ClaimChange> {
        self.changes.subscribe()
    }
}

/// The result of `future` if it is ready without waiting, for the
/// synchronous registry methods: always with [`LocalClaims`], never with
/// claims that wait for a commit.
pub fn ready_now<T>(mut future: ClaimFuture<T>) -> Option<T> {
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(peer: u16, incarnation: u64) -> Holder {
        Holder::new(Identity { peer, incarnation }, 0)
    }

    fn player(key: &str, uuid: u8, holder: Holder, cap: Option<usize>, bypass: bool) -> ClaimOp {
        ClaimOp::Player {
            key: key.into(),
            name: key.into(),
            uuid: [uuid; 16],
            holder,
            upgrade: false,
            bypass_cap: bypass,
            cap,
        }
    }

    #[test]
    fn names_and_uuids_are_unique_and_pending_names_upgrade() {
        let mut table = ClaimTable::default();
        let alice = holder(1, 1);
        let name = ClaimOp::Name {
            key: "alice".into(),
            name: "Alice".into(),
            holder: alice,
        };
        table.apply(&name, None).unwrap();
        assert_eq!(table.apply(&name, None), Err(Refusal::DuplicateName));
        // Another login may not take over the pending name, even upgrading.
        let mut other = player("alice", 2, holder(2, 1), None, false);
        assert_eq!(table.apply(&other, None), Err(Refusal::DuplicateName));
        if let ClaimOp::Player { upgrade, .. } = &mut other {
            *upgrade = true;
        }
        assert_eq!(table.apply(&other, None), Err(Refusal::DuplicateName));
        let mut changes = Vec::new();
        table
            .apply(
                &ClaimOp::Player {
                    key: "alice".into(),
                    name: "Alice".into(),
                    uuid: [1; 16],
                    holder: alice,
                    upgrade: true,
                    bypass_cap: false,
                    cap: None,
                },
                Some(&mut changes),
            )
            .unwrap();
        assert!(
            matches!(&changes[..], [ClaimChange::Claimed(record)] if record.uuid == Some([1; 16]))
        );
        assert_eq!(table.players(), 1);
        assert_eq!(
            table.apply(&player("bob", 1, holder(2, 1), None, false), None),
            Err(Refusal::AlreadyConnected)
        );
        // Only the holder releases.
        table
            .apply(
                &ClaimOp::Release {
                    key: "alice".into(),
                    holder: holder(1, 1),
                },
                None,
            )
            .unwrap();
        assert_eq!(table.players(), 1);
        table
            .apply(
                &ClaimOp::Release {
                    key: "alice".into(),
                    holder: alice,
                },
                None,
            )
            .unwrap();
        assert_eq!(table.players(), 0);
        assert!(table.live().next().is_none());
    }

    #[test]
    fn the_cap_counts_players_and_honours_a_bypass() {
        let mut table = ClaimTable::default();
        table
            .apply(&player("a", 1, holder(1, 1), Some(1), false), None)
            .unwrap();
        // Pending names do not count.
        table
            .apply(
                &ClaimOp::Name {
                    key: "p".into(),
                    name: "p".into(),
                    holder: holder(1, 1),
                },
                None,
            )
            .unwrap();
        assert_eq!(
            table.apply(&player("b", 2, holder(1, 1), Some(1), false), None),
            Err(Refusal::NetworkFull)
        );
        table
            .apply(&player("b", 2, holder(2, 1), Some(1), true), None)
            .unwrap();
        assert_eq!(table.players(), 2);
    }

    #[test]
    fn expiry_releases_one_peer_or_its_older_incarnations() {
        let mut table = ClaimTable::default();
        table
            .apply(&player("old", 1, holder(1, 1), None, false), None)
            .unwrap();
        table
            .apply(&player("new", 2, holder(1, 2), None, false), None)
            .unwrap();
        table
            .apply(&player("other", 3, holder(2, 1), None, false), None)
            .unwrap();
        let mut changes = Vec::new();
        table
            .apply(
                &ClaimOp::ExpirePeer {
                    peer: 1,
                    before: Some(2),
                },
                Some(&mut changes),
            )
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert!(table.by_uuid(&[1; 16]).is_none());
        assert!(table.by_uuid(&[2; 16]).is_some());
        table
            .apply(
                &ClaimOp::ExpirePeer {
                    peer: 1,
                    before: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(table.players(), 1);
        assert_eq!(table.by_uuid(&[3; 16]).unwrap().key, "other");
    }

    #[test]
    fn reclaims_keep_held_claims_and_lose_taken_ones() {
        let mut table = ClaimTable::default();
        let alice = holder(1, 1);
        let reclaim = ClaimOp::Reclaim {
            key: "alice".into(),
            name: "Alice".into(),
            uuid: [1; 16],
            holder: alice,
        };
        table.apply(&reclaim, None).unwrap();
        table.apply(&reclaim, None).unwrap();
        assert_eq!(table.players(), 1);
        table
            .apply(
                &ClaimOp::ExpirePeer {
                    peer: 1,
                    before: None,
                },
                None,
            )
            .unwrap();
        table
            .apply(&player("alice", 1, holder(2, 1), None, false), None)
            .unwrap();
        assert_eq!(table.apply(&reclaim, None), Err(Refusal::AlreadyConnected));
    }

    #[test]
    fn local_claims_are_ready_at_once_and_announce_changes() {
        let claims = LocalClaims::default();
        let alice = holder(0, 0);
        assert_eq!(
            ready_now(claims.claim_name("alice".into(), "Alice".into(), alice)),
            Some(Ok(()))
        );
        let mut changes = claims.subscribe();
        claims.release("alice".into(), alice);
        assert!(matches!(changes.try_recv(), Ok(ClaimChange::Released(_))));
        assert!(claims.live().is_empty());
    }
}
