//! Applying a [`Change`] to the in-memory store, with the proxy's semantics:
//! one revision per change that does something, before/after ops, and the
//! players whose compiled subjects changed.

use std::collections::{BTreeSet, HashSet};

use rift_permissions::{
    Actor, Applied, Change, ChangeEvent, ContextSet, Group, Node, NodeKind, Payload, SubjectRef,
    Track, User, Uuid,
    change::{GroupAttrs, Op},
    node::NodeKey,
    unix_now,
    wire::{ErrorCode, ErrorReply},
};
use rift_permissions_client::Error;

use crate::Store;

fn reject(code: ErrorCode, error: impl ToString) -> Error {
    Error::Rejected(ErrorReply {
        code,
        error: error.to_string(),
    })
}

fn invalid(error: rift_permissions::Error) -> Error {
    let code = match error {
        rift_permissions::Error::Track { .. } => ErrorCode::Conflict,
        rift_permissions::Error::TooManyEntries { .. } => ErrorCode::Limit,
        _ => ErrorCode::InvalidRequest,
    };
    reject(code, error)
}

fn missing(what: &str, name: &str) -> Error {
    reject(ErrorCode::NotFound, format!("unknown {what} {name:?}"))
}

fn attrs(group: &Group) -> GroupAttrs {
    GroupAttrs {
        weight: group.weight,
        display_name: group.display_name.clone(),
    }
}

impl Store {
    /// Apply `change` atomically. Returns the reply and, when something
    /// changed, the event to publish.
    pub(crate) fn commit(
        &mut self,
        actor: Actor,
        change: Change,
    ) -> Result<(Applied, Option<ChangeEvent>), Error> {
        let change = change.normalized().map_err(invalid)?;
        let mut next = self.clone();
        let mut ops = Vec::new();
        next.apply(change, &mut ops)?;
        if ops.is_empty() {
            let applied = Applied {
                revision: self.revision,
                ..Applied::default()
            };
            return Ok((applied, None));
        }
        next.revision += 1;
        let changed: Vec<Uuid> = self
            .users
            .keys()
            .chain(next.users.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|uuid| {
                let before = self.compile(**uuid).ok();
                let after = next.compile(**uuid).ok().map(|mut subject| {
                    subject.revision = self.revision;
                    subject
                });
                before != after
            })
            .copied()
            .collect();
        let groups: Vec<String> = ops
            .iter()
            .filter_map(|op| match op {
                Op::Node {
                    subject: SubjectRef::Group(name),
                    ..
                }
                | Op::Group { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        *self = next;
        let applied = Applied {
            revision: self.revision,
            ops: ops.len(),
            changed_users: changed.clone(),
            changed_groups: groups.clone(),
        };
        let event = ChangeEvent {
            revision: self.revision,
            at: unix_now(),
            actor,
            reason: None,
            ops,
            ops_truncated: false,
            changed,
            groups,
        };
        Ok((applied, Some(event)))
    }

    fn apply(&mut self, change: Change, ops: &mut Vec<Op>) -> Result<(), Error> {
        match change {
            Change::SetNode { subject, node } => self.set_node(&subject, node, ops)?,
            Change::UnsetNode { subject, key } => {
                let nodes = self.nodes(&subject)?;
                if let Some(at) = nodes.iter().position(|node| node.key() == key) {
                    let before = nodes.remove(at);
                    ops.push(Op::Node {
                        subject: subject.clone(),
                        before: Some(before),
                        after: None,
                    });
                    self.sync_columns(&subject);
                }
            }
            Change::ClearSubject { subject, contexts } => {
                let nodes = self.nodes(&subject)?;
                let (removed, kept) = std::mem::take(nodes)
                    .into_iter()
                    .partition(|node| contexts.as_ref().is_none_or(|c| *c == node.contexts));
                *nodes = kept;
                ops.extend(removed.into_iter().map(|node| Op::Node {
                    subject: subject.clone(),
                    before: Some(node),
                    after: None,
                }));
                self.sync_columns(&subject);
            }
            Change::CreateGroup {
                name,
                weight,
                display_name,
            } => {
                if self.groups.contains_key(&name) {
                    return Err(reject(
                        ErrorCode::Conflict,
                        format!("group {name:?} exists"),
                    ));
                }
                let mut group = Group::new(&name).map_err(invalid)?;
                group.weight = weight;
                group.display_name = display_name.clone();
                if weight != 0 {
                    group.nodes.push(Node::weight(weight));
                }
                if let Some(display_name) = &display_name {
                    group
                        .nodes
                        .push(Node::display_name(display_name).map_err(invalid)?);
                }
                ops.push(Op::Group {
                    name: name.clone(),
                    before: None,
                    after: Some(attrs(&group)),
                });
                self.groups.insert(name, group);
            }
            Change::DeleteGroup { name } => {
                let group = self
                    .groups
                    .remove(&name)
                    .ok_or_else(|| missing("group", &name))?;
                ops.push(Op::Group {
                    name,
                    before: Some(attrs(&group)),
                    after: None,
                });
            }
            Change::SetWeight { group, weight } => {
                self.group(&group)?;
                let subject = SubjectRef::Group(group);
                self.set_node(&subject, Node::weight(weight), ops)?;
            }
            Change::SetDisplayName {
                group,
                display_name,
            } => {
                self.group(&group)?;
                let subject = SubjectRef::Group(group);
                match display_name {
                    Some(name) => {
                        let node = Node::display_name(&name).map_err(invalid)?;
                        self.set_node(&subject, node, ops)?;
                    }
                    None => {
                        let key =
                            NodeKey::new(NodeKind::DisplayName, "display_name", ContextSet::new())
                                .map_err(invalid)?;
                        self.apply(Change::UnsetNode { subject, key }, ops)?;
                    }
                }
            }
            Change::SetPrimaryGroup { user, group } => {
                if let Some(group) = &group {
                    self.group(group)?;
                }
                let entry = self.users.entry(user).or_insert_with(|| User::new(user));
                if entry.primary_group != group {
                    ops.push(Op::PrimaryGroup {
                        user,
                        before: entry.primary_group.clone(),
                        after: group.clone(),
                    });
                    entry.primary_group = group;
                }
            }
            Change::SetTrack { name, groups } => {
                for group in &groups {
                    self.group(group)?;
                }
                let before = self.tracks.get(&name).map(|track| track.groups.clone());
                if before.as_ref() != Some(&groups) {
                    ops.push(Op::Track {
                        name: name.clone(),
                        before,
                        after: Some(groups.clone()),
                    });
                    self.tracks.insert(name.clone(), Track { name, groups });
                }
            }
            Change::DeleteTrack { name } => {
                let track = self
                    .tracks
                    .remove(&name)
                    .ok_or_else(|| missing("track", &name))?;
                ops.push(Op::Track {
                    name,
                    before: Some(track.groups),
                    after: None,
                });
            }
            Change::Promote {
                user,
                track,
                contexts,
            } => self.move_on_track(user, &track, &contexts, true, ops)?,
            Change::Demote {
                user,
                track,
                contexts,
            } => self.move_on_track(user, &track, &contexts, false, ops)?,
            Change::Bulk { filter, patch } => {
                let subjects: Vec<SubjectRef> = self
                    .users
                    .keys()
                    .map(|uuid| SubjectRef::User(*uuid))
                    .chain(
                        self.groups
                            .keys()
                            .map(|name| SubjectRef::Group(name.clone())),
                    )
                    .collect();
                for subject in subjects {
                    let nodes = self.nodes(&subject)?;
                    let mut patched = Vec::new();
                    let mut moved: HashSet<NodeKey> = HashSet::new();
                    for node in std::mem::take(nodes) {
                        if !filter.matches(&subject, &node) {
                            patched.push((node, false));
                            continue;
                        }
                        let after = patch.apply(&node).map_err(invalid)?;
                        if after.as_ref() != Some(&node) {
                            ops.push(Op::Node {
                                subject: subject.clone(),
                                before: Some(node),
                                after: after.clone(),
                            });
                        }
                        if let Some(after) = after {
                            moved.insert(after.key());
                            patched.push((after, true));
                        }
                    }
                    // A node moved into an occupied slot replaces its occupant.
                    for (node, was_patched) in patched {
                        if !was_patched && moved.contains(&node.key()) {
                            ops.push(Op::Node {
                                subject: subject.clone(),
                                before: Some(node),
                                after: None,
                            });
                        } else {
                            nodes.push(node);
                        }
                    }
                    self.sync_columns(&subject);
                }
            }
            Change::Batch { changes } => {
                for change in changes {
                    self.apply(change, ops)?;
                }
            }
        }
        Ok(())
    }

    /// Promote (`up`) or demote a user on a track, in exactly `contexts`.
    fn move_on_track(
        &mut self,
        user: Uuid,
        track: &str,
        contexts: &ContextSet,
        up: bool,
        ops: &mut Vec<Op>,
    ) -> Result<(), Error> {
        let track = self
            .tracks
            .get(track)
            .ok_or_else(|| missing("track", track))?;
        let subject = SubjectRef::User(user);
        let nodes = self.users.get(&user).map_or(&[][..], |u| &u.nodes);
        let step = if up {
            track.promote(nodes, contexts, unix_now())
        } else {
            track.demote(nodes, contexts, unix_now())
        }
        .map_err(invalid)?;
        let parent = |group: &str| {
            Node::inherit(group, true)
                .and_then(|node| node.with_contexts(contexts.clone()))
                .map_err(invalid)
        };
        if let Some(group) = step.remove {
            let key = parent(&group)?.key();
            self.apply(
                Change::UnsetNode {
                    subject: subject.clone(),
                    key,
                },
                ops,
            )?;
        }
        if let Some(group) = step.add {
            self.set_node(&subject, parent(&group)?, ops)?;
        }
        Ok(())
    }

    fn group(&mut self, name: &str) -> Result<&mut Group, Error> {
        self.groups
            .get_mut(name)
            .ok_or_else(|| missing("group", name))
    }

    fn nodes(&mut self, subject: &SubjectRef) -> Result<&mut Vec<Node>, Error> {
        Ok(match subject {
            SubjectRef::User(uuid) => {
                &mut self
                    .users
                    .entry(*uuid)
                    .or_insert_with(|| User::new(*uuid))
                    .nodes
            }
            SubjectRef::Group(name) => &mut self.group(name)?.nodes,
        })
    }

    /// Insert `node`, replacing the one in its slot.
    fn set_node(
        &mut self,
        subject: &SubjectRef,
        node: Node,
        ops: &mut Vec<Op>,
    ) -> Result<(), Error> {
        let nodes = self.nodes(subject)?;
        let key = node.key();
        let before = match nodes.iter().position(|other| other.key() == key) {
            Some(_) if nodes.contains(&node) => return Ok(()),
            Some(at) => Some(std::mem::replace(&mut nodes[at], node.clone())),
            None => {
                nodes.push(node.clone());
                None
            }
        };
        ops.push(Op::Node {
            subject: subject.clone(),
            before,
            after: Some(node),
        });
        self.sync_columns(subject);
        Ok(())
    }

    /// Keep a group's weight and display name columns equal to its global
    /// `weight` and `display_name` nodes.
    fn sync_columns(&mut self, subject: &SubjectRef) {
        let SubjectRef::Group(name) = subject else {
            return;
        };
        let Some(group) = self.groups.get_mut(name) else {
            return;
        };
        let global = group.nodes.iter().filter(|node| node.contexts.is_empty());
        let (mut weight, mut display_name) = (0, None);
        for node in global {
            match &node.payload {
                Payload::Weight { weight: w } => weight = *w,
                Payload::DisplayName { name } => display_name = Some(name.clone()),
                _ => {}
            }
        }
        group.weight = weight;
        group.display_name = display_name;
    }
}
