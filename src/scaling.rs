//! One bounded capacity decision per control-loop tick. Instance changes share
//! the configuration transaction path with administrator requests and reloads.
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use rift::{
    config::{Config, GroupMode, Placement, ServiceScaling},
    managed::{ManagedServerSnapshot, Phase},
    node::{DEFAULT_INSTANCE_MEMORY_MB, Nodes, protocol::Resources},
};

use crate::{admin::Control, control::Operation, runtime::Snapshot};

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

/// How long a rejected retirement of an instance on a draining node waits
/// before retrying, for example after a player joined it meanwhile.
const RETIRE_RETRY: Duration = Duration::from_secs(5);

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
    /// When each instance on a draining node was last asked to retire.
    retirements: BTreeMap<String, Instant>,
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
    /// [`Scaler::next_with`] with no reports from other peers.
    #[cfg(test)]
    pub fn next(&mut self, snapshot: &Snapshot, now: Instant) -> Option<Change> {
        self.next_with(snapshot, &BTreeMap::new(), now)
    }

    /// [`Scaler::next`] for a cluster leader: `remote` adds the reservations
    /// that the other peers reported for each server (docs/clustering.md,
    /// "Inputs from other peers"), so a server players are on their way to
    /// through another peer is neither counted as idle nor retired.
    pub fn next_with(
        &mut self,
        snapshot: &Snapshot,
        remote: &BTreeMap<String, usize>,
        now: Instant,
    ) -> Option<Change> {
        let mut servers = snapshot.managed.snapshot();
        for server in &mut servers {
            if let Some(reserved) = remote.get(&server.name) {
                server.reservations = server.reservations.saturating_add(*reserved);
            }
        }
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
        self.retirements
            .retain(|_, at| now.saturating_duration_since(*at) < RETIRE_RETRY);
        let evacuating = |server: &ManagedServerSnapshot| {
            snapshot.control.evacuating(&snapshot.config, &server.name)
        };
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
            // A group holds still while an administrator moves one of its
            // instances: the member is stopped on purpose, so it must not be
            // replaced, started, stopped or retired underneath the move.
            if members.iter().any(|server| server.moving) {
                continue;
            }
            // Operator state outranks the scaling policy: a stopped group
            // only winds down, and a group in dev mode is left as it is.
            let state = snapshot.control.group(name);
            // A draining node sheds its disposable instances one by one as
            // they empty, whatever the group's scaling policy; they take no
            // new players, so they are not capacity worth keeping.
            if state.mode != GroupMode::Dev
                && let Some(server) = retire_candidate(&members, evacuating, &self.retirements)
            {
                self.retirements.insert(server.clone(), now);
                self.last_group = Some(name.clone());
                return Some(Change::Instance(Operation::RemoveInstance { name: server }));
            }
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
            let disposable = group.storage.discards_data();
            let change = decide(name, policy, &members, queued, disposable, &evacuating);
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

/// The next empty instance on a draining node to retire, unless it was
/// just asked. Occupied ones keep running until their players leave.
fn retire_candidate(
    members: &[&ManagedServerSnapshot],
    evacuating: impl Fn(&ManagedServerSnapshot) -> bool,
    recent: &BTreeMap<String, Instant>,
) -> Option<String> {
    members
        .iter()
        .find(|server| {
            evacuating(server)
                && server.players == 0
                && server.reservations == 0
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
///
/// Instances on a draining node (`evacuating`) take no new players: they are
/// not capacity, so replacements are created elsewhere, and the caller
/// retires them once empty. Their players still count as load.
fn decide(
    group: &str,
    policy: &ServiceScaling,
    members: &[&ManagedServerSnapshot],
    queued: usize,
    disposable: bool,
    evacuating: &dyn Fn(&ManagedServerSnapshot) -> bool,
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
        .filter(|server| !failed(server) && !(disposable && unknown(server)) && !evacuating(server))
        .count();
    let full = members.len() >= policy.max_instances;
    if healthy < count && !full {
        return Some(Change::Instance(Operation::CreateInstance {
            group: group.into(),
        }));
    }
    // From here on, only instances that stay on their node.
    let members: Vec<_> = members
        .iter()
        .copied()
        .filter(|server| !evacuating(server))
        .collect();
    let idle = |server: &&&ManagedServerSnapshot| server.players == 0 && server.reservations == 0;
    let replaced = healthy >= count;
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
/// group's selector with a free instance slot, enough memory and enough free
/// disk, chosen by the group's placement policy. Ties go to the first name.
/// Nodes whose agent cannot apply a setting the group requires are skipped,
/// and cordoned and draining nodes take no new instances.
///
/// Memory is the smaller of the node's unallocated `memory_mb` and the
/// available memory its agent reports, so a host whose servers use more than
/// they were allocated, or whose operator set no limit, is judged by what is
/// really left. Agents that report nothing are judged by the configuration.
pub fn place(
    config: &Config,
    group: &str,
    nodes: &Nodes,
    control: &Control,
) -> Result<String, String> {
    let requirements = config
        .service_groups
        .get(group)
        .map(|definition| rift::node::server_requirements(&definition.server))
        .unwrap_or_default();
    choose(
        config,
        group,
        |node| nodes.online(node) && control.node(node).accepts_instances(),
        |node| nodes.lacking(node, &requirements),
        |node| nodes.resources(node),
    )
}

fn choose(
    config: &Config,
    group: &str,
    available: impl Fn(&str) -> bool,
    lacking: impl Fn(&str) -> Vec<&'static str>,
    resources: impl Fn(&str) -> Option<Resources>,
) -> Result<String, String> {
    let definition = config
        .service_groups
        .get(group)
        .ok_or_else(|| format!("unknown service group {group:?}"))?;
    let mut outdated = Vec::new();
    let selector = definition
        .nodes
        .as_ref()
        .ok_or_else(|| format!("service group {group} runs on this host"))?;
    let memory = definition.memory_mb.unwrap_or(DEFAULT_INSTANCE_MEMORY_MB);
    let usage = rift::node::usage(config);
    let mut matching = 0;
    let mut skipped = Vec::new();
    let candidates: Vec<_> = config
        .nodes
        .iter()
        .filter(|(name, node)| selector.matches(name, node))
        .inspect(|_| matching += 1)
        .filter_map(|(name, node)| {
            if !available(name) {
                skipped.push(format!("{name} is offline, cordoned or draining"));
                return None;
            }
            let lacking = lacking(name);
            if !lacking.is_empty() {
                outdated.push(format!("{name} (lacks {})", lacking.join(", ")));
                return None;
            }
            let (instances, allocated) = usage.get(name.as_str()).copied().unwrap_or_default();
            if instances >= node.max_instances {
                skipped.push(format!("{name} has no free instance slot"));
                return None;
            }
            let host = resources(name).unwrap_or_default();
            if let Some(disk) = host.disk_free_mb
                && disk < node.min_free_disk_mb as u64
            {
                skipped.push(format!(
                    "{name} has {disk} MiB of free disk, below its min_free_disk_mb of {}",
                    node.min_free_disk_mb
                ));
                return None;
            }
            let unallocated = node.memory_mb.map(|total| total.saturating_sub(allocated));
            if let Some(unallocated) = unallocated.filter(|&free| free < memory) {
                skipped.push(format!(
                    "{name} has {unallocated} MiB of unallocated memory"
                ));
                return None;
            }
            let available = host
                .memory_available_mb
                .map(|available| usize::try_from(available).unwrap_or(usize::MAX));
            if let Some(available) = available.filter(|&free| free < memory) {
                skipped.push(format!("{name} has {available} MiB of available memory"));
                return None;
            }
            let free = match (unallocated, available) {
                (Some(unallocated), Some(available)) => unallocated.min(available),
                (unallocated, available) => unallocated.or(available).unwrap_or(usize::MAX),
            };
            Some((name, instances, free))
        })
        .collect();
    let chosen = match definition.placement {
        Placement::LeastMemory => candidates
            .into_iter()
            .min_by_key(|(name, _, free)| (std::cmp::Reverse(*free), (*name).clone())),
        Placement::FewestInstances => candidates
            .into_iter()
            .min_by_key(|(name, instances, _)| (*instances, (*name).clone())),
    };
    chosen.map(|(name, _, _)| name.clone()).ok_or_else(|| {
        if matching == 0 {
            format!("no configured node matches service_groups.{group}.nodes")
        } else if !outdated.is_empty() {
            let others = if skipped.is_empty() {
                String::new()
            } else {
                format!("; {}", skipped.join("; "))
            };
            format!(
                "no node for {group} is online with a free instance slot, {memory} MiB of memory, enough free disk and an agent that applies its settings; upgrade rift on {}{others}",
                outdated.join(", ")
            )
        } else {
            format!(
                "no node for {group} is online, not cordoned or draining, and has a free instance slot, {memory} MiB of memory and enough free disk: {}",
                skipped.join("; ")
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn stays(_: &ManagedServerSnapshot) -> bool {
        false
    }

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
            moving: false,
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
            decide("game", &policy(), &members, 0, false, &stays)
        else {
            panic!("expected empty excess removal");
        };
        assert_eq!(name, "game-4");
        assert!(decide("game", &policy(), &members[..3], 0, false, &stays).is_none());
    }

    #[test]
    fn reservations_are_not_double_counted_and_failures_do_not_evade_retry_bounds() {
        let occupied = server("game-1", 8, 8, Phase::Running);
        let spare = server("game-2", 0, 0, Phase::Running);
        assert!(decide("game", &policy(), &[&occupied, &spare], 0, false, &stays).is_none());
        // An instance that gave up restarting is never restarted by the
        // scaler; it is replaced instead (see the crash-loop test).
        let mut failed = server("game-1", 0, 0, Phase::Failed);
        failed.restart_exhausted = true;
        assert!(!matches!(
            decide("game", &policy(), &[&failed], 0, false, &stays),
            Some(Change::Start(_))
        ));
        let stopped = server("game-1", 0, 0, Phase::Stopped);
        assert!(matches!(
            decide("game", &policy(), &[&stopped], 0, false, &stays),
            Some(Change::Start(_))
        ));
        let mut stopped_by_admin = stopped;
        stopped_by_admin.automatic_enabled = false;
        assert!(decide("game", &policy(), &[&stopped_by_admin], 0, false, &stays).is_none());
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
                decide("game", &policy, &[&recovering], 0, disposable, &stays),
                Some(Change::Instance(Operation::CreateInstance { .. }))
            ));
            let replacement = server("game-2", 0, 0, Phase::Running);
            let Some(Change::Instance(Operation::RemoveInstance { name })) = decide(
                "game",
                &policy,
                &[&recovering, &replacement],
                0,
                disposable,
                &stays,
            ) else {
                panic!("expected retirement of the surplus failed instance");
            };
            assert_eq!(name, "game-1");
            let replacement = server("game-2", 0, 0, Phase::Starting);
            // Out of retries and replaced: retired.
            let mut exhausted = recovering.clone();
            exhausted.restart_exhausted = true;
            let Some(Change::Instance(Operation::RemoveInstance { name })) = decide(
                "game",
                &policy,
                &[&exhausted, &replacement],
                0,
                disposable,
                &stays,
            ) else {
                panic!("expected retirement of the exhausted instance");
            };
            assert_eq!(name, "game-1");
            // A failed instance with players left on it is never retired.
            let mut occupied = exhausted.clone();
            occupied.reservations = 1;
            assert!(!matches!(
                decide(
                    "game",
                    &policy,
                    &[&occupied, &replacement],
                    0,
                    disposable,
                    &stays
                ),
                Some(Change::Instance(Operation::RemoveInstance { .. }))
            ));
        }
        // With every slot taken by exhausted instances, a disposable group
        // frees one for a replacement; a persistent world waits for an operator.
        policy.max_instances = 1;
        let mut exhausted = server("game-1", 0, 0, Phase::Failed);
        exhausted.restart_exhausted = true;
        assert!(matches!(
            decide("game", &policy, &[&exhausted], 0, true, &stays),
            Some(Change::Instance(Operation::RemoveInstance { .. }))
        ));
        assert!(decide("game", &policy, &[&exhausted], 0, false, &stays).is_none());
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
            decide("game", &policy, &[&lost, &healthy], 9, true, &stays),
            Some(Change::Instance(Operation::CreateInstance { .. }))
        ));
        // Once replaced, the empty unreachable instance is retired.
        let Some(Change::Instance(Operation::RemoveInstance { name })) =
            decide("game", &policy, &[&lost, &healthy], 0, true, &stays)
        else {
            panic!("expected retirement of unreachable capacity");
        };
        assert_eq!(name, "game-1");
        // Occupied unreachable instances are never retired automatically.
        lost.players = 3;
        assert!(!matches!(
            decide("game", &policy, &[&lost, &healthy], 0, true, &stays),
            Some(Change::Instance(Operation::RemoveInstance { name })) if name == "game-1"
        ));
        // Persistent: unreachable worlds keep counting and are never removed.
        lost.players = 0;
        assert!(decide("game", &policy, &[&lost, &healthy], 9, false, &stays).is_none());
        assert!(decide("game", &policy, &[&healthy, &lost], 0, false, &stays).is_none());
        // At max_instances, an empty unreachable slot is freed for its replacement.
        policy.max_instances = 2;
        assert!(matches!(
            decide("game", &policy, &[&lost, &healthy], 9, true, &stays),
            Some(Change::Instance(Operation::RemoveInstance { .. }))
        ));
    }

    #[test]
    fn instances_on_a_draining_node_are_replaced_not_counted_or_restarted() {
        let mut policy = policy();
        policy.spare_instances = 0;
        let mut leaving = server("game-1", 3, 0, Phase::Running);
        let staying = server("game-2", 0, 0, Phase::Running);
        let evacuating = |server: &ManagedServerSnapshot| server.name == "game-1";
        // Its players still count as load, but it is not capacity.
        assert!(matches!(
            decide("game", &policy, &[&leaving], 0, true, &evacuating),
            Some(Change::Instance(Operation::CreateInstance { .. }))
        ));
        // Once replaced, the staying instance is not retired as surplus
        // while the leaving one still holds players.
        assert!(decide("game", &policy, &[&leaving, &staying], 0, true, &evacuating).is_none());
        // A stopped instance on a draining node is never started again.
        leaving.players = 0;
        leaving.state = Phase::Stopped;
        assert!(
            decide("game", &policy, &[&leaving], 0, true, &evacuating)
                .is_none_or(|change| !matches!(change, Change::Start(name) if name == "game-1"))
        );
    }

    fn drain_snapshot() -> Snapshot {
        let source = "rift.config.listeners.public='127.0.0.1:0'
rift.config.routes.public='game'
rift.config.messaging={listen='127.0.0.1:0',certificate='c.pem',private_key='k.pem'}
rift.config.nodes.a={address='10.0.0.1'}
rift.config.nodes.b={address='10.0.0.2'}
rift.config.templates.t={server_jar='paper.jar'}
rift.config.service_groups.game={template='t',storage='disposable',port_range={30000,30099},nodes={'a','b'}}
rift.config.service_groups.world={template='t',storage='persistent',port_range={31000,31099},nodes={'a','b'}}";
        let mut config = rift::config::Config::from_lua(source, "drain.lua").unwrap();
        for (group, name, port, node) in [
            ("game", "game-1", 30000, "a"),
            ("world", "world-1", 31000, "a"),
            ("game", "game-2", 30000, "b"),
        ] {
            config
                .add_instance_on(group, name, port, Some(node))
                .unwrap();
        }
        Snapshot::new(config, None).unwrap()
    }

    #[tokio::test]
    async fn draining_a_node_retires_its_empty_disposable_instances() {
        // The group has no scaling policy: draining is an operator request,
        // not a scaling decision.
        let snapshot = drain_snapshot();
        let mut scaler = Scaler::default();
        let now = Instant::now();
        assert!(scaler.next(&snapshot, now).is_none());
        snapshot
            .update_node("a", rift::config::NodeSchedule::Cordoned)
            .unwrap();
        assert!(
            scaler.next(&snapshot, now).is_none(),
            "cordon keeps instances"
        );
        snapshot
            .update_node("a", rift::config::NodeSchedule::Draining)
            .unwrap();
        let Some(Change::Instance(Operation::RemoveInstance { name })) =
            scaler.next(&snapshot, now)
        else {
            panic!("expected retirement of game-1");
        };
        // The persistent world and the instance on b stay.
        assert_eq!(name, "game-1");
        // A retirement that did not happen is retried, but not at once.
        assert!(!matches!(
            scaler.next(&snapshot, now + Duration::from_millis(100)),
            Some(Change::Instance(Operation::RemoveInstance { .. }))
        ));
        assert!(matches!(
            scaler.next(&snapshot, now + RETIRE_RETRY),
            Some(Change::Instance(Operation::RemoveInstance { name })) if name == "game-1"
        ));
        // A group in dev mode is left as it is.
        snapshot
            .update_group("game", |state| state.mode = GroupMode::Dev)
            .unwrap();
        assert!(scaler.next(&snapshot, now + RETIRE_RETRY * 2).is_none());
    }

    #[tokio::test]
    async fn placement_skips_cordoned_and_draining_nodes() {
        let snapshot = drain_snapshot();
        let state = |agent: &str| {
            serde_json::to_vec(&rift::node::protocol::NodeState {
                agent: agent.into(),
                version: "test".into(),
                features: Vec::new(),
                revision: 1,
                servers: Vec::new(),
                exits: Vec::new(),
                resources: None,
            })
            .unwrap()
        };
        snapshot.nodes.record("a", &state("a")).unwrap();
        snapshot.nodes.record("b", &state("b")).unwrap();
        let chosen = || place(&snapshot.config, "game", &snapshot.nodes, &snapshot.control);
        // Both have one instance and no memory limit; ties go to the first name.
        assert_eq!(chosen().unwrap(), "a");
        snapshot
            .update_node("a", rift::config::NodeSchedule::Cordoned)
            .unwrap();
        assert_eq!(chosen().unwrap(), "b");
        snapshot
            .update_node("b", rift::config::NodeSchedule::Draining)
            .unwrap();
        let error = chosen().unwrap_err();
        assert!(error.contains("cordoned"), "{error}");
        snapshot
            .update_node("a", rift::config::NodeSchedule::Active)
            .unwrap();
        assert_eq!(chosen().unwrap(), "a");
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
        let none = |_: &str| Vec::new();
        let unmeasured = |_: &str| None;
        // d has no memory limit, so it has the most unallocated memory.
        assert_eq!(choose(&config, "game", all, none, unmeasured).unwrap(), "d");
        assert_eq!(
            choose(&config, "game", |n| n != "d", none, unmeasured).unwrap(),
            "b"
        );
        config
            .add_instance_on("game", "game-1", 30000, Some("d"))
            .unwrap();
        // d is full (max_instances = 1); c lacks the label.
        assert_eq!(choose(&config, "game", all, none, unmeasured).unwrap(), "b");
        for (name, port) in [("game-2", 30001), ("game-3", 30002), ("game-4", 30003)] {
            config
                .add_instance_on("game", name, port, Some("b"))
                .unwrap();
        }
        // b has 2048 MiB left, a has 4096 MiB.
        assert_eq!(choose(&config, "game", all, none, unmeasured).unwrap(), "a");
        // Fewest instances, restricted to the named nodes.
        assert_eq!(
            choose(&config, "lobby", all, none, unmeasured).unwrap(),
            "a"
        );
        assert_eq!(
            choose(&config, "lobby", |n| n != "a", none, unmeasured).unwrap(),
            "b"
        );
        assert!(choose(&config, "lobby", |_| false, none, unmeasured).is_err());
        // An agent that cannot apply the group's settings gets no instance.
        let outdated = |n: &str| {
            if n == "a" {
                vec!["memory_limit"]
            } else {
                vec![]
            }
        };
        assert_eq!(
            choose(&config, "lobby", all, outdated, unmeasured).unwrap(),
            "b"
        );
        let error = choose(&config, "lobby", |n| n == "a", outdated, unmeasured).unwrap_err();
        assert!(
            error.contains("upgrade rift on a (lacks memory_limit)"),
            "{error}"
        );
        config
            .add_instance_on("game", "game-5", 30000, Some("a"))
            .unwrap();
        config
            .add_instance_on("game", "game-6", 30001, Some("a"))
            .unwrap();
        assert_eq!(choose(&config, "game", all, none, unmeasured).unwrap(), "b");
        config
            .add_instance_on("game", "game-7", 30004, Some("b"))
            .unwrap();
        let error = choose(&config, "game", all, none, unmeasured).unwrap_err();
        assert!(error.contains("2048 MiB"), "{error}");
    }

    #[test]
    fn placement_respects_reported_memory_and_disk() {
        let source = "rift.config.listeners.public='127.0.0.1:0'
rift.config.routes.public='game'
rift.config.messaging={listen='127.0.0.1:0',certificate='c.pem',private_key='k.pem'}
rift.config.nodes.a={address='10.0.0.1',memory_mb=16384}
rift.config.nodes.b={address='10.0.0.2'}
rift.config.nodes.c={address='10.0.0.3',min_free_disk_mb=0}
rift.config.templates.t={server_jar='paper.jar'}
rift.config.service_groups.game={template='t',storage='disposable',memory_mb=2048,port_range={30000,30099},nodes={'a','b','c'}}";
        let config = rift::config::Config::from_lua(source, "placement.lua").unwrap();
        let all = |_: &str| true;
        let host = |memory: u64, disk: u64| Resources {
            memory_available_mb: Some(memory),
            disk_free_mb: Some(disk),
            ..Resources::default()
        };
        // Without reports, the configured limit decides: b and c have none.
        assert_eq!(
            choose(&config, "game", all, |_| Vec::new(), |_| None).unwrap(),
            "b"
        );
        // Reports rank the unlimited nodes by what their hosts have left, and
        // a's real memory caps its configured 16 GiB.
        let reports = |node: &str| match node {
            "a" => Some(host(12000, 50_000)),
            "b" => Some(host(6000, 50_000)),
            _ => Some(host(9000, 50_000)),
        };
        assert_eq!(
            choose(&config, "game", all, |_| Vec::new(), reports).unwrap(),
            "a"
        );
        let reports = |node: &str| match node {
            "a" => Some(host(4000, 50_000)),
            "b" => Some(host(6000, 50_000)),
            _ => Some(host(5000, 50_000)),
        };
        assert_eq!(
            choose(&config, "game", all, |_| Vec::new(), reports).unwrap(),
            "b"
        );
        // A nearly full disk takes b out; c disabled the check.
        let reports = |node: &str| match node {
            "a" => Some(host(1000, 50_000)),
            "b" => Some(host(6000, 900)),
            _ => Some(host(5000, 10)),
        };
        assert_eq!(
            choose(&config, "game", all, |_| Vec::new(), reports).unwrap(),
            "c"
        );
        // Too little real memory everywhere: the error names every reason.
        let reports = |node: &str| match node {
            "a" => Some(host(1000, 50_000)),
            "b" => Some(host(6000, 900)),
            _ => Some(host(2047, 50_000)),
        };
        let error = choose(&config, "game", all, |_| Vec::new(), reports).unwrap_err();
        for reason in [
            "a has 1000 MiB of available memory",
            "b has 900 MiB of free disk, below its min_free_disk_mb of 1024",
            "c has 2047 MiB of available memory",
        ] {
            assert!(error.contains(reason), "{error}");
        }
        let chosen = choose(
            &config,
            "game",
            |node| node != "a",
            |_| Vec::new(),
            |_| None,
        );
        assert_eq!(chosen.unwrap(), "b");
        let error = choose(&config, "game", |_| false, |_| Vec::new(), |_| None).unwrap_err();
        assert!(
            error.contains("a is offline, cordoned or draining"),
            "{error}"
        );
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
