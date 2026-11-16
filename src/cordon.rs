//! Taking a node out of service, shared by the HTTP API and the admin socket.
//!
//! `cordon` stops new placement on a node. `drain` also keeps new players off
//! its disposable instances and lets the scaler retire each one once it is
//! empty; persistent instances and named servers stay where they are.
//! `uncordon` returns the node to service. The schedule is recorded first,
//! then reported with what happens to each instance on the node.
use crate::{control::Error, runtime::Snapshot};
use rift::config::NodeSchedule;
use serde_json::{Value, json};

/// Apply `action` (`cordon`, `drain` or `uncordon`) to `node`.
pub fn apply(snapshot: &Snapshot, node: &str, action: &str) -> Result<Value, Error> {
    let schedule = match action {
        "cordon" => NodeSchedule::Cordoned,
        "drain" => NodeSchedule::Draining,
        "uncordon" => NodeSchedule::Active,
        _ => {
            return Err(Error::invalid(
                "node action must be cordon, drain or uncordon",
            ));
        }
    };
    if !snapshot.config.nodes.contains_key(node) {
        return Err(Error {
            status: 404,
            message: format!("unknown node {node:?}"),
        });
    }
    let schedule = snapshot
        .update_node(node, schedule)
        .map_err(|error| Error {
            status: 500,
            message: format!("node schedule could not be recorded: {error}"),
        })?;
    Ok(json!({
        "node": node,
        "schedule": schedule.as_str(),
        "drained": drained(snapshot, node),
        "instances": instances(snapshot, node, schedule),
    }))
}

/// Whether `node` is draining and no disposable instance is left on it.
pub fn drained(snapshot: &Snapshot, node: &str) -> bool {
    snapshot.control.node(node) == NodeSchedule::Draining
        && !snapshot.config.instances.values().any(|instance| {
            instance.node.as_deref() == Some(node) && instance.storage.discards_data()
        })
}

/// What happens to each server on `node` under `schedule`.
fn instances(snapshot: &Snapshot, node: &str, schedule: NodeSchedule) -> Vec<Value> {
    let servers = snapshot.managed.snapshot();
    snapshot
        .config
        .managed_servers
        .iter()
        .filter(|(_, server)| server.node.as_deref() == Some(node))
        .map(|(name, _)| {
            let instance = snapshot.config.instances.get(name);
            let disposable = instance.is_some_and(|instance| instance.storage.discards_data());
            let players = servers
                .iter()
                .find(|server| server.name == *name)
                .map_or(0, |server| server.players.max(server.reservations));
            let (result, message) = match (schedule, instance) {
                (NodeSchedule::Draining, Some(_)) if disposable && players > 0 => (
                    "draining",
                    Some(format!("{players} players; retired once they leave")),
                ),
                (NodeSchedule::Draining, Some(_)) if disposable => ("retiring", None),
                (NodeSchedule::Active, _) => ("running", None),
                // Persistent instances and named servers stay on their node.
                _ => ("kept", None),
            };
            match message {
                Some(message) => json!({"name":name,"result":result,"message":message}),
                None => json!({"name":name,"result":result}),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "rift.config.listeners.public='127.0.0.1:0'
rift.config.routes.public='game'
rift.config.messaging={listen='127.0.0.1:0',certificate='c.pem',private_key='k.pem'}
rift.config.nodes.node1={address='10.0.0.1'}
rift.config.nodes.node2={address='10.0.0.2'}
rift.config.templates.t={server_jar='paper.jar'}
rift.config.service_groups.game={template='t',storage='disposable',port_range={30000,30099},nodes={'node1','node2'}}
rift.config.service_groups.world={template='t',storage='persistent',port_range={31000,31099},nodes={'node1','node2'}}";

    fn snapshot() -> Snapshot {
        let mut config = rift::config::Config::from_lua(SOURCE, "cordon.lua").unwrap();
        config
            .add_instance_on("game", "game-1", 30000, Some("node1"))
            .unwrap();
        config
            .add_instance_on("world", "world-1", 31000, Some("node1"))
            .unwrap();
        config
            .add_instance_on("game", "game-2", 30000, Some("node2"))
            .unwrap();
        Snapshot::new(config, None).unwrap()
    }

    #[tokio::test]
    async fn drain_marks_only_disposable_instances_on_the_node() {
        let snapshot = snapshot();
        let config = &snapshot.config;
        let cordoned = apply(&snapshot, "node1", "cordon").unwrap();
        assert_eq!(cordoned["schedule"], "cordoned");
        assert_eq!(cordoned["drained"], false);
        assert!(!snapshot.control.draining(config, "game-1"));

        let draining = apply(&snapshot, "node1", "drain").unwrap();
        assert_eq!(draining["schedule"], "draining");
        assert_eq!(
            draining["instances"],
            json!([
                {"name":"game-1","result":"retiring"},
                {"name":"world-1","result":"kept"},
            ])
        );
        assert_eq!(snapshot.control.node("node1"), NodeSchedule::Draining);
        assert!(snapshot.control.draining(config, "game-1"));
        assert!(snapshot.control.evacuating(config, "game-1"));
        // Persistent worlds and other nodes keep taking players.
        assert!(!snapshot.control.draining(config, "world-1"));
        assert!(!snapshot.control.draining(config, "game-2"));
        assert!(!drained(&snapshot, "node1"));

        let active = apply(&snapshot, "node1", "uncordon").unwrap();
        assert_eq!(active["schedule"], "active");
        assert!(!snapshot.control.draining(config, "game-1"));
        assert_eq!(snapshot.control.node("node1"), NodeSchedule::Active);
    }

    #[tokio::test]
    async fn a_draining_node_without_disposable_instances_is_drained() {
        let snapshot = snapshot();
        apply(&snapshot, "node2", "drain").unwrap();
        assert!(!drained(&snapshot, "node2"));
        let mut config = snapshot.config.clone();
        config.instances.remove("game-2");
        config.backends.remove("game-2");
        config.managed_servers.remove("game-2");
        let next = Snapshot::new(config, Some(&snapshot)).unwrap();
        assert!(drained(&next, "node2"), "schedule survives a new snapshot");
        // A cordoned node is never reported as drained.
        apply(&next, "node2", "cordon").unwrap();
        assert!(!drained(&next, "node2"));
    }

    #[tokio::test]
    async fn unknown_nodes_and_actions_are_rejected() {
        let snapshot = snapshot();
        assert_eq!(apply(&snapshot, "node9", "drain").unwrap_err().status, 404);
        assert_eq!(apply(&snapshot, "node1", "evict").unwrap_err().status, 400);
        assert_eq!(snapshot.control.node("node9"), NodeSchedule::Active);
    }
}
