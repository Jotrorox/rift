//! One bounded capacity decision per control-loop tick. Instance changes share
//! the configuration transaction path with administrator requests and reloads.
use std::{collections::BTreeMap, time::Instant};

use rift::{config::ServiceScaling, managed::ManagedServerSnapshot};

use crate::{control::Operation, runtime::Snapshot};

pub enum Change {
    Instance(Operation),
    Start(String),
}

#[derive(Default)]
pub struct Scaler {
    last_change: BTreeMap<String, Instant>,
    last_group: Option<String>,
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
        for (name, group) in groups {
            let Some(policy) = &group.scaling else {
                continue;
            };
            if self
                .last_change
                .get(name)
                .is_some_and(|last| now.duration_since(*last) < policy.cooldown)
            {
                continue;
            }
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
            let queued = queues.get(name).copied().unwrap_or(0)
                + members
                    .iter()
                    .map(|server| queues.get(&server.name).copied().unwrap_or(0))
                    .sum::<usize>();
            let change = decide(name, policy, &members, queued);
            if change.is_some() {
                // Failed provisioning and occupied removal races are also
                // throttled; they must not produce a hot retry loop.
                self.last_change.insert(name.clone(), now);
                self.last_group = Some(name.clone());
                return change;
            }
        }
        None
    }
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

fn decide(
    group: &str,
    policy: &ServiceScaling,
    members: &[&ManagedServerSnapshot],
    queued: usize,
) -> Option<Change> {
    // A session may be registered while its establishment reservation is still
    // held. Counting the greater value prevents double-counting that overlap.
    let load = members
        .iter()
        .map(|server| server.players.max(server.reservations))
        .sum();
    let count = desired(policy, load, queued);
    if members.len() < count {
        return Some(Change::Instance(Operation::CreateInstance {
            group: group.into(),
        }));
    }
    if members.len() > count {
        // Workers check occupancy again under their reservation lock before
        // retirement, closing the race against admission after this snapshot.
        if let Some(server) = members.iter().rev().find(|server| {
            server.players == 0
                && server.reservations == 0
                && !matches!(server.state, "starting" | "stopping")
        }) {
            return Some(Change::Instance(Operation::RemoveInstance {
                name: server.name.clone(),
            }));
        }
    }
    members
        .iter()
        .find(|server| {
            server.state == "stopped" && server.automatic_enabled && !server.restart_exhausted
        })
        .map(|server| Change::Start(server.name.clone()))
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
        state: &'static str,
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
        let busy = server("game-1", 1, 1, "running");
        let pending = server("game-2", 0, 1, "running");
        let starting = server("game-3", 0, 0, "starting");
        let empty = server("game-4", 0, 0, "running");
        let members = [&busy, &pending, &starting, &empty];
        let Some(Change::Instance(Operation::RemoveInstance { name })) =
            decide("game", &policy(), &members, 0)
        else {
            panic!("expected empty excess removal");
        };
        assert_eq!(name, "game-4");
        assert!(decide("game", &policy(), &members[..3], 0).is_none());
    }

    #[test]
    fn reservations_are_not_double_counted_and_failures_do_not_evade_retry_bounds() {
        let occupied = server("game-1", 8, 8, "running");
        let spare = server("game-2", 0, 0, "running");
        assert!(decide("game", &policy(), &[&occupied, &spare], 0).is_none());
        let mut failed = server("game-1", 0, 0, "failed");
        failed.restart_exhausted = true;
        assert!(decide("game", &policy(), &[&failed], 0).is_none());
        let stopped = server("game-1", 0, 0, "stopped");
        assert!(matches!(
            decide("game", &policy(), &[&stopped], 0),
            Some(Change::Start(_))
        ));
        let mut stopped_by_admin = stopped;
        stopped_by_admin.automatic_enabled = false;
        assert!(decide("game", &policy(), &[&stopped_by_admin], 0).is_none());
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
