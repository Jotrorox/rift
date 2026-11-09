//! One bounded capacity decision per control-loop tick. Instance changes share
//! the configuration transaction path with administrator requests and reloads.
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use rift::{
    config::{Config, GroupMode, InstanceStorage, Placement, ServiceScaling},
    managed::{ManagedServerSnapshot, Phase},
    node::{DEFAULT_INSTANCE_MEMORY_MB, Nodes},
};

use crate::{control::Operation, runtime::Snapshot};

pub enum Change {
    Instance(Operation),
    Start(String),
    /// Stop a member of a stopped group that has become empty.
    Stop(String),
}

/// The longest a group with failed instances waits between changes.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// How long a rejected stop of a stopped group's member waits before retrying.
const STOP_RETRY: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Scaler {
    last_change: BTreeMap<String, Instant>,
    last_group: Option<String>,
    /// Changes made in a row while a group had failed instances. Each one
    /// doubles the group's cooldown, so a template that crashes on start is
    /// replaced ever more slowly instead of in a tight create/remove loop.
    failures: BTreeMap<String, u32>,
    /// When each member of a stopped group was last asked to stop.
    stops: BTreeMap<String, Instant>,
}

/// The cooldown after `failures` consecutive changes made around failed
/// instances: doubled each time, capped at [`MAX_BACKOFF`] or the configured
/// cooldown, whichever is longer.
fn backoff(cooldown: Duration, failures: u32) -> Duration {
    let cap = MAX_BACKOFF.max(cooldown);
    cooldown
        .checked_mul(1 << failures.min(16))
        .map_or(cap, |delay| delay.min(cap))
}

impl Scaler {
    pub fn next(&mut self, snapshot: &Snapshot, now: Instant) -> Option<Change> {
        let servers = snapshot.managed.snapshot();
        let queues = snapshot.extensions.queue_lengths();
        let mut groups: Vec<_> = snapshot.config.service_groups.iter().collect();
        let start = self
            .last_group
            .as_ref()
            .and_then(|last| groups.iter().position(|(name, _)| *name > last))
            .unwrap_or(0);
        groups.rotate_left(start);
        self.stops
            .retain(|_, at| now.saturating_duration_since(*at) < STOP_RETRY);
        for (name, group) in groups {
            let members: Vec<_> = servers
                .iter()
                .filter(|server| {
                    snapshot
                        .config
                        .instances
                        .get(&server.name)
                        .is_some_and(|instance| instance.group == *name)
                })
                .collect();
            // Operator state outranks the scaling policy: a stopped group
            // only winds down, and a group in dev mode is left as it is.
            let state = snapshot.control.group(name);
            if state.suspended {
                if let Some(server) = stop_candidate(&members, &self.stops) {
                    self.stops.insert(server.clone(), now);
                    self.last_group = Some(name.clone());
                    return Some(Change::Stop(server));
                }
                continue;
            }
            if state.mode == GroupMode::Dev {
                continue;
            }
            let Some(policy) = &group.scaling else {
                continue;
            };
            let failing = members.iter().any(|server| server.state == Phase::Failed);
            if !failing {
                self.failures.remove(name);
            }
            let failures = self.failures.get(name).copied().unwrap_or(0);
            if self
                .last_change
                .get(name)
                .is_some_and(|last| now.duration_since(*last) < backoff(policy.cooldown, failures))
            {
                continue;
            }
            let queued = queues.get(name).copied().unwrap_or(0)
                + members
                    .iter()
                    .map(|server| queues.get(&server.name).copied().unwrap_or(0))
                    .sum::<usize>();
            let disposable = group.storage == InstanceStorage::Disposable;
            let change = decide(name, policy, &members, queued, disposable);
            if change.is_some() {
                // Failed provisioning and occupied removal races are also
                // throttled; they must not produce a hot retry loop.
                self.last_change.insert(name.clone(), now);
                self.last_group = Some(name.clone());
                if failing {
                    self.failures
                        .insert(name.clone(), failures.saturating_add(1));
                }
                return change;
            }
        }
        None
    }
}

/// The next member of a stopped group to stop: one that is empty, would
/// still wake on its own and was not just asked. Occupied members keep
/// running, draining, until their players leave.
fn stop_candidate(
    members: &[&ManagedServerSnapshot],
    recent: &BTreeMap<String, Instant>,
) -> Option<String> {
    members
        .iter()
        .find(|server| {
            server.automatic_enabled
                && server.players == 0
                && server.reservations == 0
                && !matches!(server.state, Phase::Stopping | Phase::Unknown)
                && !recent.contains_key(&server.name)
        })
        .map(|server| server.name.clone())
}

fn desired(policy: &ServiceScaling, load: usize, queued: usize) -> usize {
    let target = (policy.capacity_per_instance * policy.target_occupancy_percent).div_ceil(100);
    let queue_demand = if queued >= policy.queue_threshold {
        queued
    } else {
        0
    };
    // Include queue pressure as future occupancy. Keeping the same queue must
    // yield the same desired count, rather than adding an instance every tick.
    (load + queue_demand)
        .div_ceil(target)
        .saturating_add(policy.spare_instances)
        .max(policy.min_instances)
        .min(policy.max_instances)
}

/// Instances on an unreachable node are `unknown`. Disposable groups replace
/// that capacity elsewhere and retire the unreachable instances once they are
/// empty; persistent instances stay tied to their node and keep counting.
///
/// `failed` instances never count as capacity: one that is still within its
/// restart retries may recover, but the group does not wait on it. Failed
/// instances are retired first once a replacement exists; one whose retries
/// are exhausted is also retired to free the slot a disposable group's
/// replacement needs. The caller throttles these changes with an exponential
/// backoff.
fn decide(
    group: &str,
    policy: &ServiceScaling,
    members: &[&ManagedServerSnapshot],
    queued: usize,
    disposable: bool,
) -> Option<Change> {
    // A session may be registered while its establishment reservation is still
    // held. Counting the greater value prevents double-counting that overlap.
    let load = members
        .iter()
        .map(|server| server.players.max(server.reservations))
        .sum();
    let count = desired(policy, load, queued);
    let unknown = |server: &&&ManagedServerSnapshot| server.state == Phase::Unknown;
    let failed = |server: &&&ManagedServerSnapshot| server.state == Phase::Failed;
    let healthy = members
        .iter()
        .filter(|server| !failed(server) && !(disposable && unknown(server)))
        .count();
    if healthy < count && members.len() < policy.max_instances {
        return Some(Change::Instance(Operation::CreateInstance {
            group: group.into(),
        }));
    }
    let idle = |server: &&&ManagedServerSnapshot| server.players == 0 && server.reservations == 0;
    let replaced = healthy >= count;
    let full = members.len() >= policy.max_instances;
    // Retire unreachable capacity once it is replaced, or when it occupies the
    // slots that its replacement needs.
    if disposable
        && (replaced || full)
        && let Some(server) = members
            .iter()
            .find(|server| unknown(server) && idle(server))
    {
        return Some(Change::Instance(Operation::RemoveInstance {
            name: server.name.clone(),
        }));
    }
    // Retire instances that gave up restarting. A persistent world is only
    // retired once replaced, never just to make room; its files stay on disk.
    if (replaced || (disposable && full))
        && let Some(server) = members
            .iter()
            .find(|server| failed(server) && server.restart_exhausted && idle(server))
    {
        return Some(Change::Instance(Operation::RemoveInstance {
            name: server.name.clone(),
        }));
    }
    // Persistent groups do not shrink while one of their nodes is unreachable.
    if members.len() > count && (disposable || !members.iter().any(|server| unknown(&server))) {
        // Workers check occupancy again under their reservation lock before
        // retirement, closing the race against admission after this snapshot.
        // Failed instances go first; they are not serving anyone.
        let retirable = |server: &&&ManagedServerSnapshot| {
            idle(server)
                && !matches!(
                    server.state,
                    Phase::Starting | Phase::Stopping | Phase::Unknown
                )
        };
        if let Some(server) = members
            .iter()
            .rev()
            .find(|server| failed(server) && retirable(server))
            .or_else(|| members.iter().rev().find(|server| retirable(server)))
        {
            return Some(Change::Instance(Operation::RemoveInstance {
                name: server.name.clone(),
            }));
        }
    }
    members
        .iter()
        .find(|server| {
            server.state == Phase::Stopped && server.automatic_enabled && !server.restart_exhausted
        })
        .map(|server| Change::Start(server.name.clone()))
}

/// Pick the node for a new instance of `group`: an online node matching the
/// group's selector with a free instance slot and enough unallocated memory,
/// chosen by the group's placement policy. Ties go to the first name.
pub fn place(config: &Config, group: &str, nodes: &Nodes) -> Result<String, String> {
    choose(config, group, |node| nodes.online(node))
}

fn choose(config: &Config, group: &str, online: impl Fn(&str) -> bool) -> Result<String, String> {
    let definition = config
        .service_groups
        .get(group)
        .ok_or_else(|| format!("unknown service group {group:?}"))?;
    let selector = definition
        .nodes
        .as_ref()
        .ok_or_else(|| format!("service group {group} runs on this host"))?;
    let memory = definition.memory_mb.unwrap_or(DEFAULT_INSTANCE_MEMORY_MB);
    let usage = rift::node::usage(config);
    let mut matching = 0;
    let candidates =
        config
            .nodes
            .iter()
            .filter(|(name, node)| selector.matches(name, node))
            .inspect(|_| matching += 1)
            .filter(|(name, _)| online(name))
            .filter_map(|(name, node)| {
                let (instances, allocated) = usage.get(name.as_str()).copied().unwrap_or_default();
                let free = node.memory_mb.map(|total| total.saturating_sub(allocated));
                (instances < node.max_instances && free.is_none_or(|free| free >= memory))
                    .then_some((name, instances, free.unwrap_or(usize::MAX)))
            });
    let chosen = match definition.placement {
        Placement::LeastMemory => {
            candidates.min_by_key(|(name, _, free)| (std::cmp::Reverse(*free), (*name).clone()))
        }
        Placement::FewestInstances => {
            candidates.min_by_key(|(name, instances, _)| (*instances, (*name).clone()))
        }
    };
    chosen.map(|(name, _, _)| name.clone()).ok_or_else(|| {
        if matching == 0 {
            format!("no configured node matches service_groups.{group}.nodes")
        } else {
            format!(
                "no node for {group} is online with a free instance slot and {memory} MiB of unallocated memory"
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn policy() -> ServiceScaling {
        ServiceScaling {
            min_instances: 1,
            max_instances: 4,
            spare_instances: 1,
            capacity_per_instance: 10,
            target_occupancy_percent: 80,
            queue_threshold: 3,
            cooldown: Duration::from_secs(5),
        }
    }

    fn server(
        name: &str,
        players: usize,
        reservations: usize,
        state: Phase,
    ) -> ManagedServerSnapshot {
        ManagedServerSnapshot {
            name: name.into(),
            state,
            pid: None,
            players,
            reservations,
            automatic_start: true,
            automatic_enabled: true,
            last_error: None,
            restart_attempts: 0,
            restart_exhausted: false,
            node: None,
        }
    }

    #[test]
    fn occupancy_spares_and_queue_pressure_have_stable_bounded_targets() {
        let mut policy = policy();
        assert_eq!(desired(&policy, 0, 0), 1);
        assert_eq!(desired(&policy, 1, 0), 2);
        assert_eq!(desired(&policy, 8, 0), 2);
        assert_eq!(desired(&policy, 9, 0), 3);
        assert_eq!(desired(&policy, 8, 2), 2); // below queue trigger
        assert_eq!(desired(&policy, 8, 3), 3);
        assert_eq!(desired(&policy, 100_000, 1024), 4);
        policy.min_instances = 3;
        assert_eq!(desired(&policy, 0, 0), 3);
        policy.min_instances = 0;
        policy.spare_instances = 0;
        assert_eq!(desired(&policy, 0, 0), 0);
        policy.capacity_per_instance = 1;
        assert_eq!(desired(&policy, 1, 0), 1); // rounded target never zero
    }

    #[test]
    fn shrinking_never_retires_players_pending_connections_or_starting_workers() {
        let busy = server("game-1", 1, 1, Phase::Running);
        let pending = server("game-2", 0, 1, Phase::Running);
        let starting = server("game-3", 0, 0, Phase::Starting);
        let empty = server("game-4", 0, 0, Phase::Running);
        let members = [&busy, &pending, &starting, &empty];
        let Some(Change::Instance(Operation::RemoveInstance { name })) =
            decide("game", &policy(), &members, 0, false)
        else {
            panic!("expected empty excess removal");
        };
        assert_eq!(name, "game-4");
        assert!(decide("game", &policy(), &members[..3], 0, false).is_none());
    }

    #[test]
    fn reservations_are_not_double_counted_and_failures_do_not_evade_retry_bounds() {
        let occupied = server("game-1", 8, 8, Phase::Running);
        let spare = server("game-2", 0, 0, Phase::Running);
        assert!(decide("game", &policy(), &[&occupied, &spare], 0, false).is_none());
        // An instance that gave up restarting is never restarted by the
        // scaler; it is replaced instead (see the crash-loop test).
        let mut failed = server("game-1", 0, 0, Phase::Failed);
        failed.restart_exhausted = true;
        assert!(!matches!(
            decide("game", &policy(), &[&failed], 0, false),
            Some(Change::Start(_))
        ));
        let stopped = server("game-1", 0, 0, Phase::Stopped);
        assert!(matches!(
            decide("game", &policy(), &[&stopped], 0, false),
            Some(Change::Start(_))
        ));
        let mut stopped_by_admin = stopped;
        stopped_by_admin.automatic_enabled = false;
        assert!(decide("game", &policy(), &[&stopped_by_admin], 0, false).is_none());
    }

    #[test]
    fn failed_instances_are_not_capacity_and_are_retired_once_exhausted() {
        let mut policy = policy();
        policy.spare_instances = 0;
        policy.max_instances = 2;
        for disposable in [true, false] {
            // Still within its restart retries: replaced, and then retired
            // ahead of healthy instances as the surplus.
            let recovering = server("game-1", 0, 0, Phase::Failed);
            assert!(matches!(
                decide("game", &policy, &[&recovering], 0, disposable),
                Some(Change::Instance(Operation::CreateInstance { .. }))
            ));
            let replacement = server("game-2", 0, 0, Phase::Running);
            let Some(Change::Instance(Operation::RemoveInstance { name })) =
                decide("game", &policy, &[&recovering, &replacement], 0, disposable)
            else {
                panic!("expected retirement of the surplus failed instance");
            };
            assert_eq!(name, "game-1");
            let replacement = server("game-2", 0, 0, Phase::Starting);
            // Out of retries and replaced: retired.
            let mut exhausted = recovering.clone();
            exhausted.restart_exhausted = true;
            let Some(Change::Instance(Operation::RemoveInstance { name })) =
                decide("game", &policy, &[&exhausted, &replacement], 0, disposable)
            else {
                panic!("expected retirement of the exhausted instance");
            };
            assert_eq!(name, "game-1");
            // A failed instance with players left on it is never retired.
            let mut occupied = exhausted.clone();
            occupied.reservations = 1;
            assert!(!matches!(
                decide("game", &policy, &[&occupied, &replacement], 0, disposable),
                Some(Change::Instance(Operation::RemoveInstance { .. }))
            ));
        }
        // With every slot taken by exhausted instances, a disposable group
        // frees one for a replacement; a persistent world waits for an operator.
        policy.max_instances = 1;
        let mut exhausted = server("game-1", 0, 0, Phase::Failed);
        exhausted.restart_exhausted = true;
        assert!(matches!(
            decide("game", &policy, &[&exhausted], 0, true),
            Some(Change::Instance(Operation::RemoveInstance { .. }))
        ));
        assert!(decide("game", &policy, &[&exhausted], 0, false).is_none());
    }

    #[test]
    fn crash_looping_groups_back_off_exponentially() {
        assert_eq!(backoff(Duration::from_secs(5), 0), Duration::from_secs(5));
        assert_eq!(backoff(Duration::from_secs(5), 3), Duration::from_secs(40));
        assert_eq!(backoff(Duration::from_secs(5), 99), MAX_BACKOFF);
        assert_eq!(
            backoff(Duration::from_secs(600), 4),
            Duration::from_secs(600)
        );
        let source = "return {listeners={public='127.0.0.1:0'},backends={},routes={public='game'},service_groups={game={command={'unused'},directory='instances/{name}',port_range={26000,26003},scaling={capacity_per_instance=10,cooldown_ms=1000}}}}";
        let config = rift::config::Config::from_lua(source, "backoff.lua").unwrap();
        let snapshot = Snapshot::new(config, None).unwrap();
        let mut scaler = Scaler::default();
        let now = Instant::now();
        // No failed members: plain cooldown.
        assert!(scaler.next(&snapshot, now).is_some());
        assert!(scaler.failures.is_empty());
        // A backoff left from earlier failures (4 s here) ends as soon as the
        // group has no failed members, back to the 1 s cooldown.
        scaler.failures.insert("game".into(), 2);
        scaler.last_change.insert("game".into(), now);
        assert!(
            scaler
                .next(&snapshot, now + Duration::from_secs(1))
                .is_some()
        );
        assert!(scaler.failures.is_empty());
    }

    #[test]
    fn unreachable_disposable_capacity_is_replaced_and_persistent_worlds_stay() {
        let mut lost = server("game-1", 0, 0, Phase::Unknown);
        let healthy = server("game-2", 0, 0, Phase::Running);
        let mut policy = policy();
        policy.spare_instances = 0;
        // Disposable: the unreachable instance no longer counts as capacity.
        assert!(matches!(
            decide("game", &policy, &[&lost, &healthy], 9, true),
            Some(Change::Instance(Operation::CreateInstance { .. }))
        ));
        // Once replaced, the empty unreachable instance is retired.
        let Some(Change::Instance(Operation::RemoveInstance { name })) =
            decide("game", &policy, &[&lost, &healthy], 0, true)
        else {
            panic!("expected retirement of unreachable capacity");
        };
        assert_eq!(name, "game-1");
        // Occupied unreachable instances are never retired automatically.
        lost.players = 3;
        assert!(!matches!(
            decide("game", &policy, &[&lost, &healthy], 0, true),
            Some(Change::Instance(Operation::RemoveInstance { name })) if name == "game-1"
        ));
        // Persistent: unreachable worlds keep counting and are never removed.
        lost.players = 0;
        assert!(decide("game", &policy, &[&lost, &healthy], 9, false).is_none());
        assert!(decide("game", &policy, &[&healthy, &lost], 0, false).is_none());
        // At max_instances, an empty unreachable slot is freed for its replacement.
        policy.max_instances = 2;
        assert!(matches!(
            decide("game", &policy, &[&lost, &healthy], 9, true),
            Some(Change::Instance(Operation::RemoveInstance { .. }))
        ));
    }

    #[test]
    fn placement_filters_by_selector_capacity_and_reachability() {
        let source = "rift.config.listeners.public='127.0.0.1:0'
rift.config.routes.public='game'
rift.config.messaging={listen='127.0.0.1:0',certificate='c.pem',private_key='k.pem'}
rift.config.nodes.a={address='10.0.0.1',memory_mb=4096,labels={'games'}}
rift.config.nodes.b={address='10.0.0.2',memory_mb=8192,labels={'games'}}
rift.config.nodes.c={address='10.0.0.3',memory_mb=65536}
rift.config.nodes.d={address='10.0.0.4',max_instances=1,labels={'games'}}
rift.config.templates.t={server_jar='paper.jar'}
rift.config.service_groups.game={template='t',storage='disposable',memory_mb=2048,port_range={30000,30099},nodes={labels={'games'}}}
rift.config.service_groups.lobby={template='t',port_range={31000,31099},nodes={'a','b'},placement='fewest_instances'}";
        let mut config = rift::config::Config::from_lua(source, "placement.lua").unwrap();
        let all = |_: &str| true;
        // d has no memory limit, so it has the most unallocated memory.
        assert_eq!(choose(&config, "game", all).unwrap(), "d");
        assert_eq!(choose(&config, "game", |n| n != "d").unwrap(), "b");
        config
            .add_instance_on("game", "game-1", 30000, Some("d"))
            .unwrap();
        // d is full (max_instances = 1); c lacks the label.
        assert_eq!(choose(&config, "game", all).unwrap(), "b");
        for (name, port) in [("game-2", 30001), ("game-3", 30002), ("game-4", 30003)] {
            config
                .add_instance_on("game", name, port, Some("b"))
                .unwrap();
        }
        // b has 2048 MiB left, a has 4096 MiB.
        assert_eq!(choose(&config, "game", all).unwrap(), "a");
        // Fewest instances, restricted to the named nodes.
        assert_eq!(choose(&config, "lobby", all).unwrap(), "a");
        assert_eq!(choose(&config, "lobby", |n| n != "a").unwrap(), "b");
        assert!(choose(&config, "lobby", |_| false).is_err());
        config
            .add_instance_on("game", "game-5", 30000, Some("a"))
            .unwrap();
        config
            .add_instance_on("game", "game-6", 30001, Some("a"))
            .unwrap();
        assert_eq!(choose(&config, "game", all).unwrap(), "b");
        config
            .add_instance_on("game", "game-7", 30004, Some("b"))
            .unwrap();
        let error = choose(&config, "game", all).unwrap_err();
        assert!(error.contains("2048 MiB"), "{error}");
    }

    #[test]
    fn cooldown_survives_snapshot_replacement() {
        let source = "return {listeners={public='127.0.0.1:0'},backends={},routes={public='game'},service_groups={game={command={'unused'},directory='instances/{name}',port_range={26000,26003},scaling={capacity_per_instance=10}}}}";
        let config = rift::config::Config::from_lua(source, "scaling.lua").unwrap();
        let snapshot = Snapshot::new(config.clone(), None).unwrap();
        let now = Instant::now();
        let mut scaler = Scaler::default();
        assert!(matches!(
            scaler.next(&snapshot, now),
            Some(Change::Instance(Operation::CreateInstance { .. }))
        ));
        let next = Snapshot::new(config, Some(&snapshot)).unwrap();
        assert!(scaler.next(&next, now + Duration::from_secs(4)).is_none());
        assert!(scaler.next(&next, now + Duration::from_secs(5)).is_some());
    }

    #[test]
    fn stopped_groups_only_stop_empty_members_and_dev_groups_are_frozen() {
        let mut paused = server("game-4", 0, 0, Phase::Stopped);
        paused.automatic_enabled = false;
        let members = [
            server("game-1", 3, 0, Phase::Running),
            server("game-2", 0, 1, Phase::Running),
            server("game-3", 0, 0, Phase::Stopping),
            paused,
            server("game-5", 0, 0, Phase::Unknown),
            server("game-6", 0, 0, Phase::Running),
            server("game-7", 0, 0, Phase::Stopped),
        ];
        let members: Vec<_> = members.iter().collect();
        let mut recent = BTreeMap::new();
        // Occupied, reserved, stopping, already paused and unreachable
        // members are left alone; idle-stopped ones are paused too.
        assert_eq!(stop_candidate(&members, &recent).as_deref(), Some("game-6"));
        recent.insert("game-6".to_owned(), Instant::now());
        assert_eq!(stop_candidate(&members, &recent).as_deref(), Some("game-7"));
        recent.insert("game-7".to_owned(), Instant::now());
        assert_eq!(stop_candidate(&members, &recent), None);

        // A group below its minimum creates nothing while stopped or in dev
        // mode, and scales again once active.
        let source = "return {listeners={public='127.0.0.1:0'},backends={},routes={public='game'},service_groups={game={command={'unused'},directory='instances/{name}',port_range={26000,26003},scaling={capacity_per_instance=10}}}}";
        let config = rift::config::Config::from_lua(source, "stopped-scaling.lua").unwrap();
        let snapshot = Snapshot::new(config, None).unwrap();
        let mut scaler = Scaler::default();
        let now = Instant::now();
        snapshot
            .update_group("game", |state| state.suspended = true)
            .unwrap();
        assert!(scaler.next(&snapshot, now).is_none());
        snapshot
            .update_group("game", |state| {
                state.suspended = false;
                state.mode = GroupMode::Dev;
            })
            .unwrap();
        assert!(scaler.next(&snapshot, now).is_none());
        snapshot
            .update_group("game", |state| state.mode = GroupMode::Active)
            .unwrap();
        assert!(matches!(
            scaler.next(&snapshot, now),
            Some(Change::Instance(Operation::CreateInstance { .. }))
        ));
    }

    #[test]
    fn short_cooldowns_and_failed_operations_cannot_starve_later_groups() {
        let source = "return {listeners={public='127.0.0.1:0'},backends={},routes={public='game'},service_groups={game={command={'unused'},directory='instances/{name}',port_range={26000,26003},scaling={capacity_per_instance=10,cooldown_ms=1}}}}";
        let mut config = rift::config::Config::from_lua(source, "fair-scaling.lua").unwrap();
        config
            .service_groups
            .insert("zgame".into(), config.service_groups["game"].clone());
        let snapshot = Snapshot::new(config, None).unwrap();
        let now = Instant::now();
        let mut scaler = Scaler::default();
        // No mutation follows either decision, modelling failed provisioning.
        for (index, expected) in ["game", "zgame", "game", "zgame"].into_iter().enumerate() {
            let Some(Change::Instance(Operation::CreateInstance { group })) =
                scaler.next(&snapshot, now + Duration::from_millis(index as u64 * 100))
            else {
                panic!("expected capacity decision");
            };
            assert_eq!(group, expected);
        }
    }
}
