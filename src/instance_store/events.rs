//! `rift.events.instances` on the peers of a cluster (docs/clustering.md,
//! "Subject scopes"): an `applied` event, which every peer publishes to its
//! own subscribers when a change of the registry applies on it.
//!
//! A single Rift announces each creation, removal and move from the code
//! that made it, with that code's answer. In a cluster the leader makes the
//! change and every peer applies it, so each peer derives the event from
//! what changed in its replica instead: [`InstanceEvents`] remembers the
//! ready registrations and the moves of named servers, and announces every
//! difference after each write to them, whichever peer made it. Every peer,
//! the one that made the change included, publishes it exactly once; the
//! proxy does not announce a change itself in a cluster.
//!
//! The payloads are those of a single Rift, less what only the code that
//! made the change knows: `files_removed` and `cleanup_error` of a removal,
//! and `files_copied`, `source_removed` and `cleanup_error` of a move.
//! A named server handed back to where the configuration has it (moved
//! back, or its node or backend changed in the configuration) is announced
//! as a move to its configured node.
use crate::{
    config::{MovedServer, ServiceInstance},
    messaging::Broker,
};
use bytes::Bytes;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Mutex, PoisonError},
};

/// Where creations, removals and moves of instances are announced.
pub const SUBJECT: &str = "rift.events.instances";

/// The backend address of `port` on a node, `None` for an unknown node.
type Addresses = Box<dyn Fn(&str, u16) -> Option<String> + Send + Sync>;

/// Announces the changes of one peer's registry on its broker; installed
/// with [`super::Store::watch_events`].
pub struct InstanceEvents {
    broker: Broker,
    addresses: Addresses,
    seen: Mutex<Seen>,
}

/// What the last announcement left the registry as.
#[derive(Default)]
struct Seen {
    instances: BTreeMap<String, ServiceInstance>,
    moved: BTreeMap<String, MovedServer>,
}

impl InstanceEvents {
    /// Publish on `broker`; `addresses` gives a node's backend address for
    /// a port, as the configuration this peer runs has the node. It is
    /// called while the database is held and must not use it.
    pub fn new(
        broker: Broker,
        addresses: impl Fn(&str, u16) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            broker,
            addresses: Box::new(addresses),
            seen: Mutex::default(),
        }
    }

    /// The registry as it is before the first announcement.
    pub(super) fn seed(
        &self,
        instances: BTreeMap<String, ServiceInstance>,
        moved: BTreeMap<String, MovedServer>,
    ) {
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Seen { instances, moved };
    }

    /// The ready registrations after a write: announce what was created,
    /// removed or moved since the last one.
    pub(super) fn instances(&self, ready: BTreeMap<String, ServiceInstance>) {
        let events = {
            let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            let mut events = Vec::new();
            for (name, before) in &seen.instances {
                match ready.get(name) {
                    None => events.push(self.removed(name, before)),
                    Some(after) if after.node != before.node || after.port != before.port => {
                        events.push(self.moved_instance(name, before, after));
                    }
                    Some(_) => {}
                }
            }
            for (name, after) in &ready {
                if !seen.instances.contains_key(name) {
                    events.push(self.created(name, after));
                }
            }
            seen.instances = ready;
            events
        };
        self.publish(events);
    }

    /// The moves of named servers after a write: announce every server that
    /// runs elsewhere since the last one.
    pub(super) fn moved(&self, moved: BTreeMap<String, MovedServer>) {
        let events = {
            let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            let mut events = Vec::new();
            for (name, before) in &seen.moved {
                if !moved.contains_key(name) {
                    events.push(handed_back(name, before));
                }
            }
            for (name, after) in &moved {
                let from = match seen.moved.get(name) {
                    Some(before) if before.node == after.node && before.port == after.port => {
                        continue;
                    }
                    Some(before) => &before.node,
                    None => &after.configured_node,
                };
                events.push(json!({
                    "name": name,
                    "moved": true,
                    "group": null,
                    "from": from,
                    "node": after.node,
                    "port": after.port,
                    "address": (self.addresses)(&after.node, after.port),
                    "storage": null,
                    "configured_node": after.configured_node,
                    "event": "moved",
                }));
            }
            seen.moved = moved;
            events
        };
        self.publish(events);
    }

    fn publish(&self, events: Vec<Value>) {
        for event in events {
            let payload = Bytes::from(event.to_string());
            let _ = self.broker.publish(SUBJECT, payload);
        }
    }

    /// Where an instance listens: loopback here, or its node's address.
    fn address(&self, instance: &ServiceInstance) -> Option<String> {
        match &instance.node {
            None => Some(format!("127.0.0.1:{}", instance.port)),
            Some(node) => (self.addresses)(node, instance.port),
        }
    }

    fn created(&self, name: &str, instance: &ServiceInstance) -> Value {
        let mut event = json!({
            "name": name,
            "group": instance.group,
            "port": instance.port,
            "address": self.address(instance),
            "template": instance.template,
            "storage": instance.storage.as_str(),
            "created": true,
            "event": "created",
        });
        if let Some(node) = &instance.node {
            event["node"] = json!(node);
        }
        event
    }

    fn removed(&self, name: &str, instance: &ServiceInstance) -> Value {
        let mut event = json!({
            "name": name,
            "removed": true,
            "storage": instance.storage.as_str(),
            "event": "removed",
        });
        if let Some(node) = &instance.node {
            event["node"] = json!(node);
        }
        event
    }

    fn moved_instance(
        &self,
        name: &str,
        before: &ServiceInstance,
        after: &ServiceInstance,
    ) -> Value {
        json!({
            "name": name,
            "moved": true,
            "group": after.group,
            "from": before.node,
            "node": after.node,
            "port": after.port,
            "address": self.address(after),
            "storage": after.storage.as_str(),
            "configured_node": null,
            "event": "moved",
        })
    }
}

/// A named server whose move was forgotten: it runs where the
/// configuration had it before its first move.
fn handed_back(name: &str, before: &MovedServer) -> Value {
    let port = before
        .configured_backend
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok());
    json!({
        "name": name,
        "moved": true,
        "group": null,
        "from": before.node,
        "node": before.configured_node,
        "port": port,
        "address": before.configured_backend,
        "storage": null,
        "configured_node": null,
        "event": "moved",
    })
}
