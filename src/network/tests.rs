use super::*;
use rift::config::Config;
use std::sync::Arc;

fn config() -> Config {
    Config::from_lua(
        "return {
            listeners = { public = '127.0.0.1:0' },
            backends = {
                lobby = '127.0.0.1:25001',
                backup = '127.0.0.1:25002',
                survival = '127.0.0.1:25003',
                restricted = '127.0.0.1:25004',
            },
            routes = { public = 'survival' },
            fallbacks = {
                survival = { 'restricted', 'backup', 'lobby' },
                restricted = { 'lobby' },
                backup = { 'survival' },
            },
            network = {
                initial = { 'restricted', 'lobby', 'backup' },
                hubs = { 'lobby', 'backup' },
                access = {
                    restricted = { allow = { 'Alice' }, deny = { 'Bob' } },
                },
            },
        }",
        "network-policy.lua",
    )
    .unwrap()
}

#[test]
fn explicit_initial_order_filters_access_without_using_backend_fallbacks() {
    let snapshot = Snapshot::new(config(), None).unwrap();
    let metrics = Metrics::default();
    let network = Network {
        snapshot: &snapshot,
        addresses: &[],
        metrics: &metrics,
    };
    assert_eq!(
        network
            .initial_candidates("survival", true, "ALICE")
            .unwrap(),
        ["restricted", "lobby", "backup"]
    );
    assert_eq!(
        network.initial_candidates("survival", true, "Bob").unwrap(),
        ["lobby", "backup"]
    );
    // An explicit initial list replaces the direct primary, including its ACL.
    assert_eq!(
        network
            .initial_candidates("restricted", true, "Eve")
            .unwrap(),
        ["lobby", "backup"]
    );
}

#[test]
fn selected_route_permissions_are_terminal_and_cannot_be_bypassed_by_fallbacks() {
    let snapshot = Snapshot::new(config(), None).unwrap();
    let metrics = Metrics::default();
    let network = Network {
        snapshot: &snapshot,
        addresses: &[],
        metrics: &metrics,
    };
    let error = network
        .initial_candidates("restricted", false, "Bob")
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        network
            .initial_candidates("restricted", false, "Alice")
            .unwrap(),
        ["restricted", "lobby"]
    );
    assert_eq!(
        network
            .initial_candidates("missing", false, "Alice")
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn ordinary_routes_keep_their_order_and_filter_restricted_fallbacks() {
    let snapshot = Snapshot::new(config(), None).unwrap();
    let metrics = Metrics::default();
    let network = Network {
        snapshot: &snapshot,
        addresses: &[],
        metrics: &metrics,
    };
    assert_eq!(
        network
            .initial_candidates("survival", false, "Eve")
            .unwrap(),
        ["survival", "backup", "lobby"]
    );
    assert_eq!(
        network
            .initial_candidates("survival", false, "Alice")
            .unwrap(),
        ["survival", "restricted", "backup", "lobby"]
    );
}

#[test]
fn recovery_prefers_hubs_then_flat_fallbacks_excluding_the_current_server() {
    let mut config = config();
    config.network.hubs = vec!["lobby".into(), "survival".into(), "backup".into()];
    let snapshot = Snapshot::new(config, None).unwrap();
    let metrics = Metrics::default();
    let network = Network {
        snapshot: &snapshot,
        addresses: &[],
        metrics: &metrics,
    };
    assert_eq!(
        network.recovery_candidates("survival"),
        ["lobby", "backup", "restricted"]
    );
    assert_eq!(network.recovery_candidates("lobby"), ["survival", "backup"]);
    assert_eq!(network.recovery_candidates("backup"), ["lobby", "survival"]);
}

#[test]
fn no_configured_recovery_destinations_means_no_implicit_cross_server_access() {
    let mut config = config();
    config.network.hubs.clear();
    config.fallbacks.clear();
    let snapshot = Snapshot::new(config, None).unwrap();
    let metrics = Metrics::default();
    let network = Network {
        snapshot: &snapshot,
        addresses: &[],
        metrics: &metrics,
    };
    assert!(network.recovery_candidates("survival").is_empty());
}

#[test]
fn reloads_share_registry_and_old_session_cleanup_releases_new_snapshot_reservations() {
    let original = Snapshot::new(config(), None).unwrap();
    let guard = original
        .players
        .register([1; 16], "Alice", "lobby")
        .unwrap();
    let mut changed = config();
    changed.network.access.get_mut("restricted").unwrap().allow = Some(Vec::new());
    let updated = Snapshot::new(changed, Some(&original)).unwrap();
    assert!(Arc::ptr_eq(&original.players, &updated.players));
    assert_eq!(updated.players.get(&[1; 16]).unwrap().server, "lobby");
    assert_eq!(
        updated
            .players
            .register([2; 16], "aLiCe", "backup")
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    guard.set_server("survival");
    assert_eq!(updated.players.get(&[1; 16]).unwrap().server, "survival");
    drop(original);
    drop(guard);
    assert!(updated.players.is_empty());
    let _new_guard = updated
        .players
        .register([2; 16], "Alice", "backup")
        .unwrap();
    assert_eq!(updated.players.len(), 1);
}
