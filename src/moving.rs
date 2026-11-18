//! `rift admin move <server> <node>`: move a persistent instance, or a named
//! server of `managed_servers`, to another node on purpose. Rift never does
//! this on its own; such a server otherwise stays on its node.
//!
//! 1. Check the move against the current configuration and nodes.
//! 2. Park the instance: refuse every start and stop its process
//!    ([`rift::managed::ManagedServers::park`]). Players must have left.
//! 3. Copy its folder from the old node's agent to a staging folder on the new
//!    one, through the controller ([`rift::node::transfer`]). `--no-copy`
//!    skips this when the folder is already on the new node, for example
//!    restored from a backup of a node that died.
//! 4. Commit in the instance transaction queue (`runtime::move_instance`):
//!    put the folder in place, refresh its port, re-register the instance in
//!    the instance database and give it a new worker on the new node. A
//!    named server keeps its port, and the instance database remembers its
//!    new node over the Lua configuration until that configuration changes
//!    the server's node or backend ([`rift::config::MovedServer`]).
//! 5. Delete the old folder unless `--keep-source`, and start the server
//!    again if it was running.
//!
//! The copy runs outside the transaction queue, so reloads, scaling and other
//! instance changes carry on meanwhile; only the moving instance's own
//! service group holds still. Any failure before the commit leaves the
//! instance on its old node with its files untouched.
use std::{path::PathBuf, sync::Arc, time::Duration};

use rift::{
    config::{InstanceStorage, LOCAL_NODE},
    node::{DEFAULT_INSTANCE_MEMORY_MB, transfer},
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::{admin, runtime::Snapshot};

/// How long the administration service waits for a move, copy included.
pub(crate) const DEADLINE: Duration = Duration::from_secs(6 * 60 * 60);

const USAGE: &str = "usage: move <server> <node> [--no-copy] [--keep-source]";

/// A parsed `move` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub instance: String,
    pub node: String,
    /// Copy the folder from the old node; `--no-copy` uses one already there.
    pub copy: bool,
    /// Keep the old folder after a copy.
    pub keep_source: bool,
}

impl Request {
    pub(crate) fn parse(args: &[String]) -> Result<Self, String> {
        let mut positional = Vec::new();
        let (mut copy, mut keep_source) = (true, false);
        for arg in args {
            match arg.as_str() {
                "--no-copy" if copy => copy = false,
                "--keep-source" if !keep_source => keep_source = true,
                flag if flag.starts_with("--") => {
                    return Err(format!("unknown or repeated option {flag:?}; {USAGE}"));
                }
                _ => positional.push(arg.clone()),
            }
        }
        let [instance, node] = <[String; 2]>::try_from(positional).map_err(|_| USAGE.to_owned())?;
        if !copy && keep_source {
            return Err(
                "--keep-source applies to copies; --no-copy never deletes the old folder".into(),
            );
        }
        Ok(Self {
            instance,
            node,
            copy,
            keep_source,
        })
    }
}

/// What the instance transaction needs to commit a prepared move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Commit {
    pub instance: String,
    pub from: String,
    pub to: String,
    pub copy: bool,
    pub keep_source: bool,
}

/// A move that [`check`] allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Checked {
    pub from: String,
    /// The server's folder, relative to each agent's root; the same on
    /// every node.
    pub directory: PathBuf,
    /// A named server of `managed_servers` rather than a service instance.
    pub named: bool,
}

/// Whether `instance`, a service instance or a named server of
/// `managed_servers`, may move to node `to` now. `source_online` also
/// requires the old node to be reachable, which a copy needs.
pub(crate) fn check(
    snapshot: &Snapshot,
    instance: &str,
    to: &str,
    source_online: bool,
) -> Result<Checked, String> {
    let config = &snapshot.config;
    let names = || {
        config
            .nodes
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let Some(server) = config.managed_servers.get(instance) else {
        return Err(format!(
            "unknown server {instance:?}; use servers to list servers"
        ));
    };
    let registration = config.instances.get(instance);
    let Some(from) = server.node.clone() else {
        return Err(format!(
            "{instance} runs on this host; only servers on nodes can move"
        ));
    };
    if to == LOCAL_NODE {
        return Err(format!(
            "{instance} runs on nodes, not on this host; choose one of: {}",
            names()
        ));
    }
    let Some(node) = config.nodes.get(to) else {
        return Err(format!(
            "unknown node {to:?}; configured nodes: {}",
            names()
        ));
    };
    if to == from {
        return Err(format!("{instance} already runs on node {to}"));
    }
    // What a service instance needs on top: a persistent folder, a node its
    // group may use and room in that node's memory.
    let mut memory = None;
    if let Some(registration) = registration {
        let group_name = &registration.group;
        if registration.storage != InstanceStorage::Persistent {
            return Err(format!(
                "{instance} is {}: its folder is rebuilt from the template anyway, so remove it and let service group {group_name} create a new instance instead",
                registration.storage.as_str()
            ));
        }
        let group = config
            .service_groups
            .get(group_name)
            .ok_or_else(|| format!("unknown service group {group_name:?}"))?;
        if !group
            .nodes
            .as_ref()
            .is_some_and(|selector| selector.matches(to, node))
        {
            return Err(format!(
                "node {to} does not match service_groups.{group_name}.nodes; add the node or its labels there first"
            ));
        }
        memory = Some(group.memory_mb.unwrap_or(DEFAULT_INSTANCE_MEMORY_MB));
    } else {
        check_named_port(snapshot, instance, to)?;
    }
    let (instances, allocated) = rift::node::usage(config)
        .get(to)
        .copied()
        .unwrap_or_default();
    if instances >= node.max_instances {
        return Err(format!(
            "node {to} already runs its max_instances ({}) servers",
            node.max_instances
        ));
    }
    if let (Some(total), Some(memory)) = (node.memory_mb, memory)
        && total.saturating_sub(allocated) < memory
    {
        return Err(format!(
            "node {to} has {} MiB of unallocated memory, but {instance} needs {memory} MiB",
            total.saturating_sub(allocated)
        ));
    }
    // What the agent measures, as placement judges new instances.
    let host = snapshot.nodes.resources(to).unwrap_or_default();
    if let Some(disk) = host.disk_free_mb
        && disk < node.min_free_disk_mb as u64
    {
        return Err(format!(
            "node {to} has {disk} MiB of free disk, below its min_free_disk_mb of {}",
            node.min_free_disk_mb
        ));
    }
    if let (Some(available), Some(memory)) = (host.memory_available_mb, memory)
        && available < memory as u64
    {
        return Err(format!(
            "node {to} has {available} MiB of available memory, but {instance} needs {memory} MiB"
        ));
    }
    let directory = server.directory.clone();
    let schedule = snapshot.control.node(to);
    if !schedule.accepts_instances() {
        return Err(format!(
            "node {to} is {} and takes no new instances; uncordon it first",
            schedule.as_str()
        ));
    }
    let lacking = snapshot
        .nodes
        .lacking(to, &rift::node::server_requirements(server));
    if !lacking.is_empty() {
        let definition = match registration {
            Some(registration) => format!("service_groups.{}", registration.group),
            None => format!("managed_servers.{instance}"),
        };
        return Err(format!(
            "node {to} runs an agent that cannot apply {} for {definition}; upgrade rift on that node to {}",
            lacking.join(", "),
            env!("CARGO_PKG_VERSION")
        ));
    }
    if !snapshot.nodes.online(to) {
        return Err(format!(
            "node {to} is {}; it must be online to receive {instance}",
            snapshot.nodes.reachability(to).as_str()
        ));
    }
    // An agent from before moves would refuse the commands as invalid, after
    // the instance was already stopped; say so before touching anything.
    let move_feature = [rift::node::protocol::Requirement {
        feature: rift::node::protocol::feature::MOVE,
        mandatory: true,
    }];
    for node in std::iter::once(to).chain(source_online.then_some(from.as_str())) {
        if !snapshot.nodes.lacking(node, &move_feature).is_empty() {
            return Err(format!(
                "node {node} runs an agent that cannot take part in moves; upgrade rift on that node to {}",
                env!("CARGO_PKG_VERSION")
            ));
        }
    }
    if source_online && !snapshot.nodes.online(&from) {
        return Err(format!(
            "node {from} is {}, so the folder of {instance} cannot be copied from it. If the node is gone for good, restore {} on node {to} from a backup and run: rift admin move {instance} {to} --no-copy",
            snapshot.nodes.reachability(&from).as_str(),
            directory.display()
        ));
    }
    Ok(Checked {
        from,
        directory,
        named: registration.is_none(),
    })
}

/// A named server keeps its backend port on the new node, since its own
/// files may name it; that port must be free there. Its move is remembered
/// in the instance database, so it also needs one.
fn check_named_port(snapshot: &Snapshot, name: &str, to: &str) -> Result<(), String> {
    let config = &snapshot.config;
    let Some(store) = &snapshot.instance_store else {
        return Err(format!(
            "managed_servers.{name}: moving a named server needs the instance database, which Rift opens when it runs from a configuration file"
        ));
    };
    let port_of = |name: &str| {
        config
            .backends
            .get(name)?
            .address()
            .rsplit_once(':')?
            .1
            .parse::<u16>()
            .ok()
    };
    let port =
        port_of(name).ok_or_else(|| format!("managed_servers.{name}: backend has no port"))?;
    if let Some((other, _)) = config.managed_servers.iter().find(|(other, server)| {
        other.as_str() != name
            && server.node.as_deref() == Some(to)
            && port_of(other.as_str()) == Some(port)
    }) {
        return Err(format!(
            "port {port} of {name} is already used on node {to} by {other}; a named server keeps its port when it moves"
        ));
    }
    let reserved = store
        .reserved_ports_on(Some(to))
        .map_err(|error| error.to_string())?;
    if reserved.contains(&port) {
        return Err(format!(
            "port {port} of {name} is reserved on node {to} by an instance being created or removed; try again later"
        ));
    }
    Ok(())
}

/// Run a whole move; see the module documentation.
pub(crate) async fn run(
    snapshot: Arc<Snapshot>,
    commands: &mpsc::Sender<admin::Command>,
    request: Request,
) -> Result<Value, String> {
    let Request {
        instance,
        node: to,
        copy,
        keep_source,
    } = request;
    let checked = check(&snapshot, &instance, &to, copy)?;
    let from = checked.from.clone();
    let parked = snapshot
        .managed
        .park(&instance)
        .await
        .map_err(|error| format!("cannot stop {instance} for the move: {error}"))?;
    log_info!("moving {instance} from node {from} to node {to}");
    let copied = if copy {
        match transfer::copy(&snapshot.nodes, &instance, &from, &to, &checked.directory).await {
            Ok(copied) => Some(copied),
            Err(error) => {
                parked.resume();
                return Err(format!(
                    "copying {instance} from node {from} to node {to} failed: {error}; {instance} stays on node {from}"
                ));
            }
        }
    } else {
        None
    };
    let (reply, receiver) = oneshot::channel();
    let commit = Commit {
        instance: instance.clone(),
        from: from.clone(),
        to: to.clone(),
        copy,
        keep_source,
    };
    // Wait for room in the queue: after a long copy, a busy queue is no
    // reason to throw the copy away.
    let result = match commands
        .send(admin::Command::MoveInstance { commit, reply })
        .await
    {
        Ok(()) => receiver
            .await
            .unwrap_or_else(|_| Err("proxy is shutting down".into())),
        Err(_) => Err("proxy is shutting down".into()),
    };
    match result {
        Ok(mut response) => {
            if let Some(copied) = copied {
                response["copied"] = json!({
                    "files": copied.files,
                    "folders": copied.folders,
                    "bytes": copied.bytes,
                });
            }
            log_info!("moved {instance} from node {from} to node {to}");
            parked.resume();
            Ok(response)
        }
        Err(error) => {
            if copy {
                transfer::abort(&snapshot.nodes, &instance, &to, &checked.directory).await;
            }
            parked.resume();
            Err(format!("{error}; {instance} stays on node {from}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Request, String> {
        Request::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn move_requests_take_an_instance_a_node_and_two_options() {
        assert_eq!(
            parse(&["lobby-1", "node2"]).unwrap(),
            Request {
                instance: "lobby-1".into(),
                node: "node2".into(),
                copy: true,
                keep_source: false,
            }
        );
        let request = parse(&["--keep-source", "lobby-1", "node2"]).unwrap();
        assert!(request.copy && request.keep_source);
        assert!(!parse(&["lobby-1", "node2", "--no-copy"]).unwrap().copy);
        for bad in [
            &["lobby-1"][..],
            &["lobby-1", "node2", "node3"],
            &["lobby-1", "node2", "--force"],
            &["lobby-1", "node2", "--no-copy", "--no-copy"],
            &["lobby-1", "node2", "--no-copy", "--keep-source"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
