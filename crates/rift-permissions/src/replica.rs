//! A backend's in-memory copy of compiled subjects and the group table, kept
//! current from `rift.perms.*` messages. Reads take a shared lock and never
//! wait on the network.
//!
//! Ordering rules (see `docs/permissions-protocol.md`):
//!
//! - A snapshot replaces the stored one only when its revision is newer, so
//!   duplicated or reordered pushes are harmless.
//! - The replica's revision follows `hello` and [`ChangeEvent`]s. An event at
//!   exactly the next revision lists the stored subjects to refetch; an older
//!   one is ignored; a newer one reveals a lost message and asks for a resync.

use crate::{
    change::ChangeEvent,
    check::{MetaView, Tristate},
    compiled::CompiledSubject,
    context::ContextSet,
    subject::Uuid,
    wire::GroupInfo,
};
use std::{
    collections::HashMap,
    sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

/// How long a replica keeps a player's snapshot after they leave, in seconds.
pub const EVICT_AFTER: u64 = 600;

/// What an applied [`ChangeEvent`] asks the caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Not newer than the replica's revision: a duplicate or a reordered
    /// event. Nothing to do.
    Stale,
    /// The next revision. Refetch these stored subjects unless their pushed
    /// snapshot arrives first, and the group table when `groups` is set.
    Applied { refetch: Vec<Uuid>, groups: bool },
    /// A revision was skipped, so an event was lost: refetch every stored
    /// subject (`subject.batch`) and the group table.
    Gap { refetch: Vec<Uuid> },
}

struct Slot {
    subject: Arc<CompiledSubject>,
    /// When the player left this backend, for eviction.
    left_at: Option<u64>,
}

#[derive(Default)]
struct Inner {
    revision: u64,
    subjects: HashMap<Uuid, Slot>,
    groups: Arc<Vec<GroupInfo>>,
    groups_revision: u64,
}

/// Thread-safe cache of compiled subjects keyed by UUID.
#[derive(Default)]
pub struct Replica {
    inner: RwLock<Inner>,
}

impl Replica {
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The last revision seen from `hello` or a [`ChangeEvent`].
    pub fn revision(&self) -> u64 {
        self.read().revision
    }

    /// Adopt the proxy's revision from a `hello` reply. Returns true when it
    /// differs from a non-empty replica's, which means every stored subject
    /// must be refetched.
    pub fn sync_revision(&self, proxy: u64) -> bool {
        let mut inner = self.write();
        let resync = inner.revision != proxy && !inner.subjects.is_empty();
        inner.revision = proxy;
        resync
    }

    /// Store a snapshot if it is newer than the stored one for that UUID.
    /// Returns whether it was stored. Keeps the player's eviction state.
    pub fn insert(&self, subject: impl Into<Arc<CompiledSubject>>) -> bool {
        let subject = subject.into();
        let mut inner = self.write();
        match inner.subjects.get_mut(&subject.uuid) {
            Some(slot) if slot.subject.revision >= subject.revision => false,
            Some(slot) => {
                slot.subject = subject;
                true
            }
            None => {
                let slot = Slot {
                    subject: subject.clone(),
                    left_at: None,
                };
                inner.subjects.insert(subject.uuid, slot);
                true
            }
        }
    }

    pub fn get(&self, uuid: Uuid) -> Option<Arc<CompiledSubject>> {
        self.read()
            .subjects
            .get(&uuid)
            .map(|slot| slot.subject.clone())
    }

    pub fn contains(&self, uuid: Uuid) -> bool {
        self.read().subjects.contains_key(&uuid)
    }

    pub fn remove(&self, uuid: Uuid) -> Option<Arc<CompiledSubject>> {
        self.write().subjects.remove(&uuid).map(|slot| slot.subject)
    }

    pub fn len(&self) -> usize {
        self.read().subjects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.read().subjects.is_empty()
    }

    /// Every stored UUID, for a resync.
    pub fn uuids(&self) -> Vec<Uuid> {
        self.read().subjects.keys().copied().collect()
    }

    /// Every stored snapshot, for persisting the replica.
    pub fn subjects(&self) -> Vec<Arc<CompiledSubject>> {
        self.read()
            .subjects
            .values()
            .map(|slot| slot.subject.clone())
            .collect()
    }

    /// Check from the stored snapshot; `Undefined` when there is none.
    pub fn check(&self, uuid: Uuid, node: &str, active: &ContextSet, now: u64) -> Tristate {
        self.read()
            .subjects
            .get(&uuid)
            .map_or(Tristate::Undefined, |slot| {
                slot.subject.check(node, active, now)
            })
    }

    pub fn meta(&self, uuid: Uuid, active: &ContextSet, now: u64) -> Option<MetaView> {
        self.read()
            .subjects
            .get(&uuid)
            .map(|slot| slot.subject.meta(active, now))
    }

    /// Apply a [`ChangeEvent`] from `rift.perms.update`.
    pub fn apply_update(&self, event: &ChangeEvent) -> UpdateOutcome {
        let mut inner = self.write();
        if event.revision <= inner.revision {
            return UpdateOutcome::Stale;
        }
        let gap = event.revision != inner.revision + 1;
        inner.revision = event.revision;
        if gap {
            return UpdateOutcome::Gap {
                refetch: inner.subjects.keys().copied().collect(),
            };
        }
        let refetch = event
            .changed
            .iter()
            .filter(|uuid| {
                inner
                    .subjects
                    .get(uuid)
                    .is_some_and(|slot| slot.subject.revision < event.revision)
            })
            .copied()
            .collect();
        UpdateOutcome::Applied {
            refetch,
            groups: !event.groups.is_empty(),
        }
    }

    /// Mark a player as gone from this backend at `now`; the snapshot stays
    /// until [`evict`](Self::evict) removes it.
    pub fn mark_left(&self, uuid: Uuid, now: u64) {
        if let Some(slot) = self.write().subjects.get_mut(&uuid) {
            slot.left_at = Some(now);
        }
    }

    /// Cancel a pending eviction when the player is back.
    pub fn mark_joined(&self, uuid: Uuid) {
        if let Some(slot) = self.write().subjects.get_mut(&uuid) {
            slot.left_at = None;
        }
    }

    /// Remove players who left at least `after` seconds before `now`
    /// (normally [`EVICT_AFTER`]); returns their UUIDs.
    pub fn evict(&self, now: u64, after: u64) -> Vec<Uuid> {
        let mut inner = self.write();
        let gone: Vec<Uuid> = inner
            .subjects
            .iter()
            .filter(|(_, slot)| {
                slot.left_at
                    .is_some_and(|left| left.saturating_add(after) <= now)
            })
            .map(|(uuid, _)| *uuid)
            .collect();
        for uuid in &gone {
            inner.subjects.remove(uuid);
        }
        gone
    }

    /// Replace the group table when `revision` is not older than the stored
    /// one. Returns whether it was stored.
    pub fn set_groups(&self, revision: u64, groups: Vec<GroupInfo>) -> bool {
        let mut inner = self.write();
        if revision < inner.groups_revision {
            return false;
        }
        inner.groups_revision = revision;
        inner.groups = Arc::new(groups);
        true
    }

    /// The group table and the revision it was fetched at.
    pub fn groups(&self) -> (u64, Arc<Vec<GroupInfo>>) {
        let inner = self.read();
        (inner.groups_revision, inner.groups.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        change::Actor,
        compiled::{Entry, EntryKind},
    };

    fn subject(uuid: u128, revision: u64, value: bool) -> CompiledSubject {
        CompiledSubject {
            revision,
            uuid: Uuid::from_u128(uuid),
            primary_group: "default".into(),
            entries: vec![Entry {
                kind: EntryKind::Permission,
                key: "fly".into(),
                value,
                contexts: ContextSet::new(),
                rank: 0,
                expires_at: None,
            }],
            prefix: vec![],
            suffix: vec![],
            meta: vec![],
        }
    }

    fn event(revision: u64, changed: &[u128]) -> ChangeEvent {
        ChangeEvent {
            revision,
            at: 0,
            actor: Actor::System,
            reason: None,
            ops: vec![],
            ops_truncated: false,
            changed: changed.iter().map(|&u| Uuid::from_u128(u)).collect(),
            groups: vec![],
        }
    }

    #[test]
    fn newer_snapshots_win() {
        let replica = Replica::new();
        let uuid = Uuid::from_u128(1);
        let none = ContextSet::new();
        assert_eq!(replica.check(uuid, "fly", &none, 0), Tristate::Undefined);
        assert!(replica.insert(subject(1, 5, true)));
        assert!(!replica.insert(subject(1, 4, false)));
        assert!(!replica.insert(subject(1, 5, false)));
        assert_eq!(replica.check(uuid, "fly", &none, 0), Tristate::True);
        assert!(replica.insert(subject(1, 6, false)));
        assert_eq!(replica.check(uuid, "fly", &none, 0), Tristate::False);
        assert_eq!(
            replica.meta(uuid, &none, 0).unwrap().primary_group,
            "default"
        );
        assert_eq!(replica.len(), 1);
        assert!(replica.remove(uuid).is_some());
        assert!(replica.is_empty());
    }

    #[test]
    fn updates_track_revisions() {
        let replica = Replica::new();
        assert!(!replica.sync_revision(10));
        replica.insert(subject(1, 8, true));
        replica.insert(subject(2, 11, true));
        assert_eq!(
            replica.apply_update(&event(11, &[1, 2, 3])),
            UpdateOutcome::Applied {
                refetch: vec![Uuid::from_u128(1)],
                groups: false
            }
        );
        assert_eq!(replica.revision(), 11);
        assert_eq!(replica.apply_update(&event(11, &[1])), UpdateOutcome::Stale);
        assert_eq!(replica.apply_update(&event(9, &[1])), UpdateOutcome::Stale);
        let UpdateOutcome::Gap { mut refetch } = replica.apply_update(&event(13, &[])) else {
            panic!("expected a gap");
        };
        refetch.sort();
        assert_eq!(refetch, [Uuid::from_u128(1), Uuid::from_u128(2)]);
        assert_eq!(replica.revision(), 13);
        assert!(replica.sync_revision(20));
        assert!(!replica.sync_revision(20));
    }

    #[test]
    fn eviction_after_leaving() {
        let replica = Replica::new();
        replica.insert(subject(1, 1, true));
        replica.insert(subject(2, 1, true));
        replica.mark_left(Uuid::from_u128(1), 100);
        replica.mark_left(Uuid::from_u128(2), 100);
        replica.mark_joined(Uuid::from_u128(2));
        assert!(replica.evict(699, EVICT_AFTER).is_empty());
        assert_eq!(replica.evict(700, EVICT_AFTER), [Uuid::from_u128(1)]);
        assert_eq!(replica.uuids(), [Uuid::from_u128(2)]);
    }

    #[test]
    fn group_table() {
        let replica = Replica::new();
        let vip = GroupInfo {
            name: "vip".into(),
            weight: 1,
            display_name: None,
            nodes: vec![],
        };
        assert!(replica.set_groups(5, vec![vip.clone()]));
        assert!(!replica.set_groups(4, vec![]));
        let (revision, groups) = replica.groups();
        assert_eq!((revision, groups.as_slice()), (5, [vip].as_slice()));
    }
}
