//! `rift admin node plan`: suggest moves that even out memory across nodes.
//! It only suggests. Rift never moves a server on its own; each suggestion is
//! a `rift admin move` for an administrator to run, or not.
//!
//! A node's memory use is the larger of two shares, as far as each is known:
//! the memory allocated to its instances of its configured `memory_mb`, and
//! the memory in use of what its agent measures on the host
//! ([`rift::node::protocol::Resources`]). A node without either is left out,
//! as is a node that is not online.
//!
//! While the busiest and the least used node are further apart than the
//! allowed spread, the plan takes a server off the busiest node it can and
//! puts it on a less used one, choosing the move that leaves the busier of
//! the two nodes lowest. Only persistent instances and named servers with a
//! known size move: disposable instances are replaced anyway, and placement
//! puts new ones where memory is free. Every suggestion passes the checks of
//! a real move ([`crate::moving::check`]) and keeps room on its target after
//! the moves suggested before it, so the moves are meant to run in order.
use std::collections::{BTreeMap, BTreeSet};

use rift::{config::InstanceStorage, node::DEFAULT_INSTANCE_MEMORY_MB};
use serde_json::{Value, json};

use crate::runtime::Snapshot;

pub(crate) const USAGE: &str = "usage: node plan [--spread <percent>]";

/// How far apart, in percentage points of memory, the busiest and the least
/// used node may be before the plan suggests moves.
pub(crate) const DEFAULT_SPREAD: u8 = 20;

/// The most moves one plan suggests; run them and plan again for more.
const MAX_MOVES: usize = 20;

/// A move must lower the busier of its two nodes by at least this many
/// percentage points: moving a world costs a copy and a restart.
const MIN_GAIN: f64 = 1.0;

/// The allowed spread from `node plan [--spread <percent>]`.
pub(crate) fn parse(args: &[String]) -> Result<u8, String> {
    match args {
        [] => Ok(DEFAULT_SPREAD),
        [flag, value] if flag == "--spread" => parse_spread(value),
        _ => Err(USAGE.into()),
    }
}

/// A spread of 1 to 100 percentage points.
pub(crate) fn parse_spread(value: &str) -> Result<u8, String> {
    value
        .parse::<u8>()
        .ok()
        .filter(|spread| (1..=100).contains(spread))
        .ok_or_else(|| format!("the spread is a whole percentage from 1 to 100, not {value:?}"))
}

/// What the plan knows about a node's memory, updated as moves are planned.
#[derive(Debug, Clone)]
struct Load {
    memory_mb: Option<u64>,
    /// The `memory_mb` of its group instances ([`rift::node::usage`]).
    allocated: u64,
    total: Option<u64>,
    available: Option<u64>,
    instances: usize,
    max_instances: usize,
}

impl Load {
    /// The share of memory in use, in percent; `None` when nothing tells.
    fn percent(&self) -> Option<f64> {
        let share = |used: u64, capacity: u64| used as f64 * 100.0 / capacity as f64;
        let configured = self
            .memory_mb
            .filter(|&capacity| capacity > 0)
            .map(|capacity| share(self.allocated, capacity));
        let measured = match (self.total, self.available) {
            (Some(total), Some(available)) if total > 0 => {
                Some(share(total.saturating_sub(available), total))
            }
            // An agent that measures the total only (macOS), on a node
            // without `memory_mb`: judge the allocation against the host.
            (Some(total), None) if total > 0 && self.memory_mb.is_none() => {
                Some(share(self.allocated, total))
            }
            _ => None,
        };
        match (configured, measured) {
            (Some(configured), Some(measured)) => Some(configured.max(measured)),
            (one, other) => one.or(other),
        }
    }

    /// Whether `server` fits here on top of what is already planned.
    fn fits(&self, server: &Movable) -> bool {
        self.instances < self.max_instances
            && self
                .memory_mb
                .is_none_or(|capacity| capacity.saturating_sub(self.allocated) >= server.allocated)
            && self
                .available
                .is_none_or(|available| available >= server.memory)
    }

    fn take(&mut self, server: &Movable) {
        self.allocated += server.allocated;
        self.instances += 1;
        self.available = self
            .available
            .map(|available| available.saturating_sub(server.memory));
    }

    fn give(&mut self, server: &Movable) {
        self.allocated = self.allocated.saturating_sub(server.allocated);
        self.instances = self.instances.saturating_sub(1);
        self.available = self.available.map(|available| {
            let available = available + server.memory;
            self.total.map_or(available, |total| available.min(total))
        });
    }
}

/// A server the plan may move.
#[derive(Debug, Clone)]
struct Movable {
    name: String,
    group: Option<String>,
    /// What it adds to a node's allocated memory: its group's `memory_mb`,
    /// or nothing for a named server, as [`rift::node::usage`] counts.
    allocated: u64,
    /// The memory it is expected to use on the host.
    memory: u64,
    players: usize,
}

/// A planned move.
struct Planned {
    server: Movable,
    from: String,
    to: String,
}

/// The plan for the nodes of `snapshot`, allowing `max_spread` percentage
/// points between the busiest and the least used node.
pub(crate) fn plan(snapshot: &Snapshot, max_spread: u8) -> Value {
    let max_spread = f64::from(max_spread);
    let config = &snapshot.config;
    let usage = rift::node::usage(config);
    let mut notes = Vec::new();
    let mut loads = BTreeMap::new();
    for (name, node) in &config.nodes {
        let (instances, allocated) = usage.get(name).copied().unwrap_or_default();
        let host = snapshot.nodes.resources(name).unwrap_or_default();
        let load = Load {
            memory_mb: node.memory_mb.map(|memory| memory as u64),
            allocated: allocated as u64,
            total: host.memory_total_mb,
            available: host.memory_available_mb,
            instances,
            max_instances: node.max_instances,
        };
        if !snapshot.nodes.online(name) {
            notes.push(format!(
                "node {name} is {} and left out of the plan",
                snapshot.nodes.reachability(name).as_str()
            ));
        } else if load.percent().is_none() {
            notes.push(format!(
                "node {name} sets no memory_mb and its agent measures no memory, so it is left out of the plan"
            ));
        } else {
            loads.insert(name.clone(), load);
        }
    }
    let before = loads.clone();

    let servers = snapshot.managed.snapshot();
    let mut movable: BTreeMap<String, Vec<Movable>> = BTreeMap::new();
    let mut disposable = BTreeSet::new();
    for (name, server) in &config.managed_servers {
        let Some(node) = server
            .node
            .as_ref()
            .filter(|node| loads.contains_key(*node))
        else {
            continue;
        };
        let state = servers.iter().find(|state| state.name == *name);
        if state.is_some_and(|state| state.moving) || crate::moving::describe(name).is_some() {
            notes.push(format!("{name} is moving and left out of the plan"));
            continue;
        }
        let (group, allocated, memory) = match config.instances.get(name) {
            Some(instance) if instance.storage != InstanceStorage::Persistent => {
                disposable.insert(node.clone());
                continue;
            }
            Some(instance) => {
                let memory = config
                    .service_groups
                    .get(&instance.group)
                    .and_then(|group| group.memory_mb)
                    .unwrap_or(DEFAULT_INSTANCE_MEMORY_MB) as u64;
                (Some(instance.group.clone()), memory, memory)
            }
            None => match server.memory_limit {
                Some(limit) => (None, 0, limit.megabytes as u64),
                None => {
                    notes.push(format!(
                        "{name} on node {node} sets no memory_limit, so its size is unknown and the plan leaves it where it is"
                    ));
                    continue;
                }
            },
        };
        movable.entry(node.clone()).or_default().push(Movable {
            name: name.clone(),
            group,
            allocated,
            memory,
            players: state.map_or(0, |state| state.players.max(state.reservations)),
        });
    }

    // Whether a real move would be allowed now, by server and target.
    let mut checks: BTreeMap<(String, String), Result<(), String>> = BTreeMap::new();
    let mut check = |server: &str, to: &str| {
        checks
            .entry((server.to_owned(), to.to_owned()))
            .or_insert_with(|| crate::moving::check(snapshot, server, to, true).map(|_| ()))
            .clone()
    };
    let mut planned = Vec::new();
    let mut refusals = BTreeSet::new();
    while planned.len() < MAX_MOVES && spread(&loads) > max_spread {
        let Some(next) = next_move(&loads, &movable, &mut check, &mut refusals) else {
            break;
        };
        movable
            .get_mut(&next.from)
            .expect("the planned server's node")
            .retain(|server| server.name != next.server.name);
        loads
            .get_mut(&next.from)
            .expect("planned node")
            .give(&next.server);
        loads
            .get_mut(&next.to)
            .expect("planned node")
            .take(&next.server);
        planned.push(next);
    }

    let spread_before = spread(&before);
    let spread_after = spread(&loads);
    if spread_after > max_spread {
        let busiest = busiest(&loads);
        if planned.is_empty() {
            notes.push(match &busiest {
                Some(node) => {
                    format!("no server on node {node}, the busiest, can move to a less used node")
                }
                None => "no server can move to a less used node".into(),
            });
            notes.extend(refusals.into_iter().take(5));
        } else if planned.len() == MAX_MOVES {
            notes.push(format!(
                "the plan stops at {MAX_MOVES} moves; run them and plan again"
            ));
        } else {
            notes.push("the remaining spread cannot be narrowed by moving whole servers".into());
        }
        if let Some(node) = busiest.filter(|node| disposable.contains(node)) {
            notes.push(format!(
                "the disposable instances on node {node} are not moved: new ones go to the node with the most free memory"
            ));
        }
    }

    let nodes: Vec<_> = config
        .nodes
        .iter()
        .map(|(name, node)| {
            let host = snapshot.nodes.resources(name).unwrap_or_default();
            let mut warnings = Vec::new();
            if let Some(free) = host.disk_free_mb
                && free < node.min_free_disk_mb as u64
            {
                warnings.push(format!(
                    "{free} MiB of free disk, below min_free_disk_mb of {}: no server can move here",
                    node.min_free_disk_mb
                ));
            }
            let load_per_cpu = host
                .load
                .zip(host.cpus.filter(|&cpus| cpus > 0))
                .map(|(load, cpus)| load / f64::from(cpus));
            if let (Some(load), Some(cpus), Some(share)) = (host.load, host.cpus, load_per_cpu)
                && share > 1.0
            {
                warnings.push(format!(
                    "busy: a load of {load:.2} on {cpus} CPUs"
                ));
            }
            let (instances, _) = usage.get(name).copied().unwrap_or_default();
            json!({
                "name": name,
                "state": snapshot.nodes.reachability(name).as_str(),
                "schedule": snapshot.control.node(name).as_str(),
                "memory_used_percent": before.get(name).and_then(Load::percent).map(tenths),
                "planned_memory_used_percent": loads.get(name).and_then(Load::percent).map(tenths),
                "instances": instances,
                "planned_instances": loads.get(name).map_or(instances, |load| load.instances),
                "load_per_cpu": load_per_cpu.map(|share| (share * 100.0).round() / 100.0),
                "disk_free_mb": host.disk_free_mb,
                "warnings": warnings,
            })
        })
        .collect();
    let moves: Vec<_> = planned
        .iter()
        .map(|planned| {
            let server = &planned.server;
            json!({
                "server": server.name,
                "group": server.group,
                "from": planned.from,
                "to": planned.to,
                "memory_mb": server.memory,
                "players": server.players,
                "command": format!("rift admin move {} {}", server.name, planned.to),
            })
        })
        .collect();
    json!({
        "balanced": spread_before <= max_spread,
        "max_spread_percent": max_spread,
        "spread_percent": tenths(spread_before),
        "planned_spread_percent": tenths(spread_after),
        "nodes": nodes,
        "moves": moves,
        "notes": notes,
    })
}

/// The best move off the busiest node that has one, or `None`.
fn next_move(
    loads: &BTreeMap<String, Load>,
    movable: &BTreeMap<String, Vec<Movable>>,
    check: &mut impl FnMut(&str, &str) -> Result<(), String>,
    refusals: &mut BTreeSet<String>,
) -> Option<Planned> {
    let mut sources: Vec<_> = loads
        .iter()
        .filter_map(|(name, load)| Some((name, load.percent()?)))
        .collect();
    sources.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    for (from, level) in sources {
        let source = &loads[from];
        // (busier node afterwards, target afterwards, players, server, target)
        let mut best: Option<(f64, f64, usize, &Movable, &String)> = None;
        for server in movable.get(from).into_iter().flatten() {
            let mut source_after = source.clone();
            source_after.give(server);
            let Some(source_level) = source_after.percent() else {
                continue;
            };
            for (to, target) in loads {
                if to == from || target.percent().is_none_or(|target| target >= level) {
                    continue;
                }
                if !target.fits(server) {
                    continue;
                }
                if let Err(refusal) = check(&server.name, to) {
                    refusals.insert(refusal);
                    continue;
                }
                let mut target_after = target.clone();
                target_after.take(server);
                let Some(target_level) = target_after.percent() else {
                    continue;
                };
                let peak = source_level.max(target_level);
                if peak > level - MIN_GAIN {
                    continue;
                }
                let candidate = (peak, target_level, server.players, server, to);
                let better = best.as_ref().is_none_or(|best| {
                    candidate
                        .0
                        .total_cmp(&best.0)
                        .then(candidate.1.total_cmp(&best.1))
                        .then(candidate.2.cmp(&best.2))
                        .then(candidate.3.name.cmp(&best.3.name))
                        .then(candidate.4.cmp(best.4))
                        .is_lt()
                });
                if better {
                    best = Some(candidate);
                }
            }
        }
        if let Some((_, _, _, server, to)) = best {
            return Some(Planned {
                server: server.clone(),
                from: from.clone(),
                to: to.clone(),
            });
        }
    }
    None
}

/// Percentage points between the busiest and the least used node.
fn spread(loads: &BTreeMap<String, Load>) -> f64 {
    let levels = loads.values().filter_map(Load::percent);
    let (low, high) = levels.fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), level| {
        (low.min(level), high.max(level))
    });
    if high >= low { high - low } else { 0.0 }
}

fn busiest(loads: &BTreeMap<String, Load>) -> Option<String> {
    loads
        .iter()
        .filter_map(|(name, load)| Some((name, load.percent()?)))
        .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(name, _)| name.clone())
}

/// Rounded to a tenth, for display.
fn tenths(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift::node::protocol::{FEATURES, NodeState, Resources};

    const SOURCE: &str = "rift.config.listeners.public='127.0.0.1:0'
rift.config.routes.public='game'
rift.config.messaging={listen='127.0.0.1:0',certificate='c.pem',private_key='k.pem'}
rift.config.nodes.node1={address='10.0.0.1',memory_mb=8192}
rift.config.nodes.node2={address='10.0.0.2',memory_mb=8192}
rift.config.nodes.node3={address='10.0.0.3',memory_mb=8192}
rift.config.templates.t={server_jar='paper.jar'}
rift.config.service_groups.game={template='t',storage='disposable',port_range={30000,30099},memory_mb=1024,nodes={'node1','node2','node3'}}
rift.config.service_groups.world={template='t',storage='persistent',port_range={31000,31099},memory_mb=2048,nodes={'node1','node2','node3'}}";

    fn state(resources: Option<Resources>) -> Vec<u8> {
        serde_json::to_vec(&NodeState {
            agent: "agent".into(),
            version: "test".into(),
            features: FEATURES
                .iter()
                .map(|feature| (*feature).to_owned())
                .collect(),
            revision: 1,
            servers: Vec::new(),
            exits: Vec::new(),
            resources,
        })
        .unwrap()
    }

    /// Three worlds on node1, one on node2, none on node3; all online.
    fn snapshot(source: &str) -> Snapshot {
        let mut config = rift::config::Config::from_lua(source, "rebalance.lua").unwrap();
        for (name, port, node) in [
            ("world-1", 31000, "node1"),
            ("world-2", 31001, "node1"),
            ("world-3", 31002, "node1"),
            ("world-4", 31000, "node2"),
        ] {
            config
                .add_instance_on("world", name, port, Some(node))
                .unwrap();
        }
        let snapshot = Snapshot::new(config, None).unwrap();
        for node in ["node1", "node2", "node3"] {
            snapshot.nodes.record(node, &state(None)).unwrap();
        }
        snapshot
    }

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn the_spread_is_a_whole_percentage() {
        assert_eq!(parse(&[]).unwrap(), DEFAULT_SPREAD);
        assert_eq!(parse(&args(&["--spread", "35"])).unwrap(), 35);
        for bad in [
            &["--spread"][..],
            &["--spread", "0"],
            &["--spread", "101"],
            &["--spread", "12.5"],
            &["--spread", "10", "--spread", "20"],
            &["node1"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn worlds_move_from_the_busiest_node_to_the_emptiest() {
        let snapshot = snapshot(SOURCE);
        let plan = plan(&snapshot, DEFAULT_SPREAD);
        assert_eq!(plan["balanced"], false);
        assert_eq!(plan["spread_percent"], 75.0);
        // One world to the empty node3 leaves 50, 25 and 25 percent; a second
        // would only make node2 or node3 the busiest instead.
        assert_eq!(
            plan["moves"],
            json!([{
                "server": "world-1",
                "group": "world",
                "from": "node1",
                "to": "node3",
                "memory_mb": 2048,
                "players": 0,
                "command": "rift admin move world-1 node3",
            }])
        );
        assert_eq!(plan["planned_spread_percent"], 25.0);
        let node = |name: &str| {
            plan["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["name"] == name)
                .unwrap()
                .clone()
        };
        assert_eq!(node("node1")["memory_used_percent"], 75.0);
        assert_eq!(node("node1")["planned_memory_used_percent"], 50.0);
        assert_eq!(node("node3")["planned_instances"], 1);
        assert!(
            plan["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|note| note.as_str().unwrap().contains("whole servers")),
            "{plan}"
        );
        // Nothing moved for real.
        assert_eq!(
            snapshot.config.managed_servers["world-1"].node.as_deref(),
            Some("node1")
        );

        // A wider spread leaves the nodes as they are.
        let plan = super::plan(&snapshot, 80);
        assert_eq!(plan["balanced"], true);
        assert_eq!(plan["moves"], json!([]));
    }

    #[tokio::test]
    async fn moves_respect_group_nodes_and_cordons() {
        // node3 is not a node of the world group, so world-1 goes to node2.
        let source = SOURCE.replace(
            "memory_mb=2048,nodes={'node1','node2','node3'}",
            "memory_mb=2048,nodes={'node1','node2'}",
        );
        let snapshot = snapshot(&source);
        let plan = plan(&snapshot, DEFAULT_SPREAD);
        assert_eq!(plan["moves"][0]["to"], "node2", "{plan}");
        assert_eq!(plan["moves"].as_array().unwrap().len(), 1, "{plan}");

        // With node2 cordoned too, nothing can take a world.
        snapshot
            .update_node("node2", rift::config::NodeSchedule::Cordoned)
            .unwrap();
        let plan = super::plan(&snapshot, DEFAULT_SPREAD);
        assert_eq!(plan["moves"], json!([]));
        let notes: Vec<_> = plan["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note.as_str().unwrap().to_owned())
            .collect();
        assert!(notes[0].contains("node node1, the busiest"), "{notes:?}");
        assert!(
            notes.iter().any(|note| note.contains("cordoned")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|note| note.contains("does not match")),
            "{notes:?}"
        );
    }

    #[tokio::test]
    async fn measured_memory_counts_and_unknown_nodes_and_servers_stay_out() {
        let source = format!(
            "{SOURCE}
rift.config.nodes.node4={{address='10.0.0.4'}}
rift.config.backends.survival='10.0.0.2:25570'
rift.config.managed_servers.survival={{command={{'java','-jar','paper.jar'}},directory='survival',node='node2'}}"
        );
        let snapshot = snapshot(&source);
        // node2 is fuller than its allocation says: 6 of 8 GiB in use.
        snapshot
            .nodes
            .record(
                "node2",
                &state(Some(Resources {
                    memory_total_mb: Some(8192),
                    memory_available_mb: Some(2048),
                    ..Resources::default()
                })),
            )
            .unwrap();
        let plan = plan(&snapshot, DEFAULT_SPREAD);
        let node2 = &plan["nodes"].as_array().unwrap()[1];
        assert_eq!(node2["name"], "node2");
        assert_eq!(node2["memory_used_percent"], 75.0);
        let notes = plan["notes"].to_string();
        // node4 never connected; survival has no memory_limit.
        assert!(notes.contains("node node4 is never_connected"), "{notes}");
        assert!(
            notes.contains("survival on node node2 sets no memory_limit"),
            "{notes}"
        );
        assert_eq!(plan["nodes"][3]["memory_used_percent"], Value::Null);
        // Worlds still only go to node3, the one with room.
        for planned in plan["moves"].as_array().unwrap() {
            assert_eq!(planned["to"], "node3", "{plan}");
        }
    }
}
