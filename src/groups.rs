//! Whole-group operations shared by the HTTP API and the admin socket:
//! start or stop every instance of a service group, and switch its mode.
//! Each one records the group's state first, then acts on its members and
//! reports what happened to each.
use crate::{control::Error, runtime::Snapshot};
use rift::{
    config::{GroupMode, GroupState},
    managed::Phase,
};
use serde_json::{Value, json};
use std::io;

fn unknown(group: &str) -> Error {
    Error {
        status: 404,
        message: format!("unknown service group {group:?}"),
    }
}

fn record(
    snapshot: &Snapshot,
    group: &str,
    change: impl FnOnce(&mut GroupState),
) -> Result<GroupState, Error> {
    if !snapshot.config.service_groups.contains_key(group) {
        return Err(unknown(group));
    }
    snapshot.update_group(group, change).map_err(|error| Error {
        status: 500,
        message: format!("group state could not be recorded: {error}"),
    })
}

fn outcome(name: &str, result: &str, message: Option<String>) -> Value {
    match message {
        Some(message) => json!({"name":name,"result":result,"message":message}),
        None => json!({"name":name,"result":result}),
    }
}

fn response(group: &str, state: GroupState, instances: Vec<Value>) -> Value {
    json!({
        "group":group,
        "suspended":state.suspended,
        "mode":state.mode.as_str(),
        "instances":instances,
    })
}

/// Resume the group and start every member that is not already up. Manual
/// starts also clear an earlier explicit stop and exhausted restart retries.
pub fn start(snapshot: &Snapshot, group: &str) -> Result<Value, Error> {
    let state = record(snapshot, group, |state| state.suspended = false)?;
    let servers = snapshot.managed.snapshot();
    let instances = snapshot
        .config
        .instance_names(group)
        .into_iter()
        .map(|name| {
            let phase = servers
                .iter()
                .find(|server| server.name == name)
                .map(|server| server.state);
            if matches!(phase, Some(Phase::Running | Phase::Starting)) {
                return outcome(&name, "running", None);
            }
            match snapshot.managed.request_start(&name) {
                Ok(()) => outcome(&name, "starting", None),
                Err(error) => outcome(&name, "failed", Some(error.to_string())),
            }
        })
        .collect();
    Ok(response(group, state, instances))
}

/// Stop the group: no scaling, no new players, and every empty member stops
/// now. Members with players drain and the scaler stops each one once its
/// last player leaves; nobody is disconnected.
pub fn stop(snapshot: &Snapshot, group: &str) -> Result<Value, Error> {
    let state = record(snapshot, group, |state| state.suspended = true)?;
    let servers = snapshot.managed.snapshot();
    let instances = snapshot
        .config
        .instance_names(group)
        .into_iter()
        .map(|name| {
            let Some(server) = servers.iter().find(|server| server.name == name) else {
                return outcome(&name, "failed", Some("instance is being removed".into()));
            };
            if server.players > 0 || server.reservations > 0 {
                return outcome(
                    &name,
                    "draining",
                    Some(format!(
                        "{} players; stops once they leave",
                        server.players.max(server.reservations)
                    )),
                );
            }
            if !server.automatic_enabled && matches!(server.state, Phase::Stopped | Phase::Stopping)
            {
                return outcome(&name, "stopped", None);
            }
            match snapshot.managed.request_stop(&name) {
                Ok(()) => outcome(&name, "stopping", None),
                // A player arrived between the snapshot and the request;
                // the scaler stops it once it is empty again.
                Err(error) if error.kind() == io::ErrorKind::ResourceBusy => {
                    outcome(&name, "draining", Some(error.to_string()))
                }
                Err(error) => outcome(&name, "failed", Some(error.to_string())),
            }
        })
        .collect();
    Ok(response(group, state, instances))
}

/// Switch who may join. Players already on the group's servers stay.
pub fn set_mode(snapshot: &Snapshot, group: &str, mode: &str) -> Result<Value, Error> {
    let mode = GroupMode::parse(mode)
        .ok_or_else(|| Error::invalid("mode must be \"active\" or \"dev\""))?;
    let state = record(snapshot, group, |state| state.mode = mode)?;
    let mut value = response(group, state, Vec::new());
    value.as_object_mut().unwrap().remove("instances");
    value["dev_permission"] = json!(GroupMode::dev_permission(group));
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        let source = "return {listeners={public='127.0.0.1:0'},backends={},routes={public='game'},service_groups={game={command={'unused'},directory='instances/{name}',port_range={26100,26103}}}}";
        let config = rift::config::Config::from_lua(source, "groups.lua").unwrap();
        Snapshot::new(config, None).unwrap()
    }

    #[test]
    fn operations_record_state_and_reject_unknown_groups_and_modes() {
        let snapshot = snapshot();
        let stopped = stop(&snapshot, "game").unwrap();
        assert_eq!(stopped["suspended"], true);
        assert_eq!(stopped["instances"], json!([]));
        assert!(snapshot.control.group("game").suspended);
        let dev = set_mode(&snapshot, "game", "dev").unwrap();
        assert_eq!(dev["mode"], "dev");
        assert_eq!(dev["suspended"], true);
        assert_eq!(dev["dev_permission"], "rift.group.game.dev");
        let started = start(&snapshot, "game").unwrap();
        assert_eq!(started["suspended"], false);
        assert_eq!(started["mode"], "dev");
        set_mode(&snapshot, "game", "active").unwrap();
        assert!(snapshot.control.group("game").is_default());
        assert_eq!(stop(&snapshot, "missing").unwrap_err().status, 404);
        assert_eq!(start(&snapshot, "missing").unwrap_err().status, 404);
        assert_eq!(
            set_mode(&snapshot, "game", "paused").unwrap_err().status,
            400
        );
        assert!(snapshot.control.group("missing").is_default());
    }

    #[test]
    fn removing_a_group_definition_forgets_its_state() {
        let snapshot = snapshot();
        stop(&snapshot, "game").unwrap();
        let source = "return {listeners={public='127.0.0.1:0'},backends={},routes={public='other'},service_groups={other={command={'unused'},directory='other/{name}',port_range={26104,26105}}}}";
        let config = rift::config::Config::from_lua(source, "groups.lua").unwrap();
        let next = Snapshot::new(config, Some(&snapshot)).unwrap();
        assert!(next.control.group("game").suspended, "kept until pruned");
        next.prune_groups();
        assert!(next.control.group("game").is_default());
    }

    #[test]
    fn dev_mode_admits_only_players_with_the_group_permission() {
        let snapshot = snapshot();
        let player = rift_permissions::Uuid::from_bytes([7; 16]);
        assert!(snapshot.admits("game", "Alice", None));
        set_mode(&snapshot, "game", "dev").unwrap();
        assert!(!snapshot.admits("game", "Alice", None));
        assert!(!snapshot.admits("game", "Alice", Some(player)));
        // Backends outside the group are unaffected.
        assert!(snapshot.dev_admits("primary", None));
        let authority = snapshot.extensions.permissions();
        authority
            .apply(
                rift_permissions::Actor::Cli,
                rift_permissions::Change::SetNode {
                    subject: rift_permissions::SubjectRef::User(player),
                    node: rift_permissions::Node::permission("rift.group.game.dev", true).unwrap(),
                },
            )
            .unwrap();
        assert!(snapshot.admits("game", "Alice", Some(player)));
    }
}
