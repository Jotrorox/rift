//! The replication contract for a future follower proxy. Nothing here runs
//! yet; the leader already writes everything a follower needs.
//!
//! # Source
//!
//! The leader appends one `perm_change` row per committed write and
//! publishes the same row, as a [`rift_permissions::ChangeEvent`], to the
//! file-backed retained stream `rift.perms.changes`. Revisions are assigned
//! only by the leader, start at 1 and increase by exactly 1, so a gap in what
//! a follower received is always detectable.
//!
//! # Replay
//!
//! A follower keeps its own SQLite file with the same `perm_*` tables and
//! applies each event whose revision is exactly one more than its
//! `perm_meta.revision`, in one transaction that also inserts the
//! `perm_change` row and advances `perm_meta.revision`:
//!
//! - `Op::Node { subject, before, after }`: delete the row in `before`'s slot
//!   `(subject, kind, key, contexts)` when `before` is set, then insert or
//!   replace `after` when it is set. A user subject gets its `perm_subject`
//!   row on first use.
//! - `Op::Group { name, before: None, after }` creates the group row before
//!   any node op names it; `after: None` deletes the row (its nodes were
//!   removed by earlier ops of the same change). Otherwise the weight and
//!   display name columns take `after`; they always equal the group's global
//!   `weight` and `display_name` nodes.
//! - `Op::Track` upserts or deletes the `perm_track` row.
//! - `Op::PrimaryGroup` sets `perm_subject.primary_group`.
//!
//! Ops are recorded in the order the leader executed them, so applying them
//! in list order reproduces the leader's state exactly. Events published on
//! the live subjects may carry `ops_truncated`; a follower must then read the
//! row from the stream or the leader's log instead.
//!
//! # Mode
//!
//! In follower mode the authority rejects [`super::Authority::apply`] and
//! every other write with `ErrorCode::Unavailable`, serves reads and checks
//! from its replica, and recompiles and notifies exactly as the leader does
//! after each replayed revision. Last-seen names and the registered
//! permission catalogue are not policy and are not replicated.
