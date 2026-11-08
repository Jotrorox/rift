//! Reads of the append-only `perm_change` log: the audit view and, later,
//! the replay source of a follower proxy.
use super::PermError;
use rift_permissions::{Actor, ChangeEvent, SubjectRef, change::Op};
use rusqlite::{Connection, params};
use std::collections::BTreeSet;

/// Rows one query returns by default and at most.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;

/// Which changes [`crate::permissions::Authority::log`] returns. Rows come in
/// revision order. With `since`, the first `limit` revisions after it;
/// without, the latest `limit`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogQuery {
    /// Only revisions greater than this.
    pub since: Option<u64>,
    /// Only changes by this actor.
    pub actor: Option<Actor>,
    /// Only changes touching this user or group.
    pub subject: Option<SubjectRef>,
    /// 0 means 100; at most 1000.
    pub limit: usize,
}

/// Whether `op` touches `subject`.
fn touches(op: &Op, subject: &SubjectRef) -> bool {
    match (op, subject) {
        (Op::Node { subject: s, .. }, _) => s == subject,
        (Op::Group { name, .. }, SubjectRef::Group(group)) => name == group,
        (Op::PrimaryGroup { user, .. }, SubjectRef::User(uuid)) => user == uuid,
        _ => false,
    }
}

/// A change row as an event, with the users and groups its ops name.
fn event(
    revision: u64,
    at: u64,
    actor: &str,
    ops: &str,
    reason: Option<String>,
) -> Result<ChangeEvent, PermError> {
    let corrupt = |e: &dyn std::fmt::Display| {
        PermError::Storage(format!("invalid perm_change row {revision}: {e}"))
    };
    let ops: Vec<Op> = serde_json::from_str(ops).map_err(|e| corrupt(&e))?;
    let mut users = BTreeSet::new();
    let mut groups = BTreeSet::new();
    for op in &ops {
        match op {
            Op::Node {
                subject: SubjectRef::User(uuid),
                ..
            }
            | Op::PrimaryGroup { user: uuid, .. } => {
                users.insert(*uuid);
            }
            Op::Node {
                subject: SubjectRef::Group(name),
                ..
            }
            | Op::Group { name, .. } => {
                groups.insert(name.clone());
            }
            Op::Track { .. } => {}
        }
    }
    Ok(ChangeEvent {
        revision,
        at,
        actor: actor.parse().map_err(|e| corrupt(&e))?,
        reason,
        ops,
        ops_truncated: false,
        changed: users.into_iter().collect(),
        groups: groups.into_iter().collect(),
    })
}

pub(super) fn read(
    connection: &Connection,
    query: &LogQuery,
) -> Result<Vec<ChangeEvent>, PermError> {
    let limit = match query.limit {
        0 => DEFAULT_LIMIT,
        limit => limit.min(MAX_LIMIT),
    };
    // A cheap textual filter first; `touches` decides.
    let needle = query.subject.as_ref().map(|subject| match subject {
        SubjectRef::User(uuid) => uuid.to_string(),
        SubjectRef::Group(name) => format!("\"{name}\""),
    });
    let order = if query.since.is_some() { "ASC" } else { "DESC" };
    let mut statement = connection.prepare(&format!(
        "SELECT revision, at, actor, ops, reason FROM perm_change
         WHERE revision > ?1 AND (?2 IS NULL OR actor = ?2) AND (?3 IS NULL OR instr(ops, ?3) > 0)
         ORDER BY revision {order}"
    ))?;
    let mut rows = statement.query(params![
        super::store::sql_time(query.since.unwrap_or(0)),
        query.actor.as_ref().map(ToString::to_string),
        needle,
    ])?;
    let mut events = Vec::new();
    while let Some(row) = rows.next()?
        && events.len() < limit
    {
        let event = event(
            row.get::<_, i64>(0)? as u64,
            row.get::<_, i64>(1)?.max(0) as u64,
            &row.get::<_, String>(2)?,
            &row.get::<_, String>(3)?,
            row.get(4)?,
        )?;
        if query
            .subject
            .as_ref()
            .is_none_or(|subject| event.ops.iter().any(|op| touches(op, subject)))
        {
            events.push(event);
        }
    }
    if query.since.is_none() {
        events.reverse();
    }
    Ok(events)
}
