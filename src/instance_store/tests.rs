use super::*;
use crate::{
    managed::{ManagedServers, Phase},
    players::PlayerRegistry,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Fixture {
    root: PathBuf,
    source: String,
}

impl Fixture {
    fn new(storage: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "rift-instance-db-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("assets")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        fs::write(root.join("assets/server.jar"), b"server seed").unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let source = format!(
            "return {{listeners={{public='127.0.0.1:0'}},backends={{}},routes={{public='games'}},templates={{seed={{server_jar='assets/server.jar'}}}},service_groups={{games={{template='seed',storage='{storage}',directory='instances/{{name}}',command={{'unused-test-command'}},port_range={{{port},{port}}},autostart=true,scaling={{capacity_per_instance=10}}}}}}}}"
        );
        Self { root, source }
    }
    fn config(&self) -> Config {
        Config::from_lua_at(&self.source, &self.root.join("rift.lua")).unwrap()
    }
    fn store(&self) -> Store {
        Store::open(&self.root.join("rift.sqlite3")).unwrap()
    }
    fn prepare(&self, store: &Store) -> (Config, String, Plan) {
        let mut config = self.config();
        let name = store.allocate_name(&config, "games").unwrap();
        let port = config.service_groups["games"].port_start;
        config.add_instance("games", &name, port).unwrap();
        let plan = provisioning::plan(&config, &name).unwrap();
        store.begin_creation(&config, &name, &plan).unwrap();
        (config, name, plan)
    }
    fn ready(&self, store: &Store) -> (Config, String, Plan) {
        let (config, name, plan) = self.prepare(store);
        provisioning::provision_planned(&config, &name, &plan).unwrap();
        store.commit_creation(&name).unwrap();
        (config, name, plan)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn reopen_preserves_registration_world_ownership_stop_and_sequence() {
    let fixture = Fixture::new("persistent");
    let store = fixture.store();
    let (original, name, plan) = fixture.ready(&store);
    fs::write(plan.directory.join("progress.dat"), b"player progress").unwrap();
    store.set_explicit_stop(&name, true).unwrap();
    drop(store);
    let store = fixture.store();
    // Existing instances keep their old template even if future instances use another.
    let changed = fixture
        .source
        .replace("template='seed',", "")
        .replace("templates={seed={server_jar='assets/server.jar'}},", "");
    let mut restored = Config::from_lua_at(&changed, &fixture.root.join("rift.lua")).unwrap();
    store.restore(&mut restored).unwrap();
    assert_eq!(restored.instances, original.instances);
    assert_eq!(restored.managed_servers, original.managed_servers);
    assert_eq!(
        store.stopped_names().unwrap(),
        BTreeSet::from([name.clone()])
    );
    assert_eq!(
        fs::read(plan.directory.join("progress.dat")).unwrap(),
        b"player progress"
    );
    store.reserve_restored_ports(&restored).unwrap();
    store.begin_removal(&name).unwrap();
    store.finish_removal(&name).unwrap();
    drop(store);
    let store = fixture.store();
    let mut restored = fixture.config();
    store.restore(&mut restored).unwrap();
    assert!(restored.instances.is_empty());
    assert!(plan.directory.exists());
    assert_eq!(store.allocate_name(&restored, "games").unwrap(), "games-2");
}

#[tokio::test]
async fn stop_intent_disables_autostart_and_manual_start_clears_it() {
    let fixture = Fixture::new("persistent");
    let store = Arc::new(fixture.store());
    let (config, name, _) = fixture.ready(&store);
    store.set_explicit_stop(&name, true).unwrap();
    let manager = ManagedServers::new(&config, Arc::new(PlayerRegistry::default()));
    manager.with_persistence(store.clone()).unwrap();
    manager.launch().unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let server = manager.snapshot().pop().unwrap();
    assert_eq!(server.state, Phase::Stopped);
    assert!(!server.automatic_enabled);
    assert!(manager.request_scale_start(&name).is_err());
    manager.request_start(&name).unwrap();
    assert!(store.stopped_names().unwrap().is_empty());
    manager.shutdown().await;
}

#[tokio::test]
async fn rejected_busy_stop_and_removal_do_not_change_durable_intent() {
    let fixture = Fixture::new("persistent");
    let store = Arc::new(fixture.store());
    let (mut config, name, _) = fixture.ready(&store);
    config.managed_servers.get_mut(&name).unwrap().autostart = false;
    let manager = ManagedServers::new(&config, Arc::new(PlayerRegistry::default()));
    manager.with_persistence(store.clone()).unwrap();
    manager.launch().unwrap();
    let lease = manager.reserve(&name).unwrap();
    assert!(manager.request_stop(&name).is_err());
    assert!(manager.remove(&name).await.is_err());
    assert!(store.stopped_names().unwrap().is_empty());
    assert_eq!(store.records().unwrap()[0].phase, "ready");
    drop(lease);
    manager.stop(&name).await.unwrap();
    assert!(store.stopped_names().unwrap().contains(&name));
    manager.shutdown().await;
}

#[test]
fn interrupted_creation_recovers_before_copy_during_copy_after_publish_and_during_rollback() {
    for boundary in 0..5 {
        let fixture = Fixture::new("disposable");
        let store = fixture.store();
        let (config, name, plan) = fixture.prepare(&store);
        match boundary {
            1 | 2 => {
                let stage = plan.stage.as_ref().unwrap();
                fs::create_dir_all(stage).unwrap();
                if boundary == 2 {
                    fs::write(
                        stage.join(".rift-instance.json"),
                        serde_json::to_vec(&plan.marker).unwrap(),
                    )
                    .unwrap();
                }
                fs::write(stage.join("partial-copy"), b"partial").unwrap();
            }
            3 | 4 => {
                provisioning::provision_planned(&config, &name, &plan).unwrap();
                if boundary == 4 {
                    let stage = plan.stage.as_ref().unwrap();
                    fs::rename(&plan.directory, stage).unwrap();
                    fs::remove_file(stage.join(".rift-instance.json")).unwrap();
                }
            }
            _ => {}
        }
        drop(store);
        let store = fixture.store();
        let mut restored = fixture.config();
        store.restore(&mut restored).unwrap();
        assert!(restored.instances.is_empty());
        assert!(store.records().unwrap().is_empty());
        assert!(!plan.directory.exists());
        assert!(!plan.stage.as_ref().unwrap().exists());
        assert_eq!(store.allocate_name(&restored, "games").unwrap(), "games-2");
        assert_eq!(
            fs::read(fixture.root.join("assets/server.jar")).unwrap(),
            b"server seed"
        );
    }
}

#[test]
fn aborted_creation_reusing_persistent_data_keeps_existing_world() {
    let fixture = Fixture::new("persistent");
    let store = fixture.store();
    let (config, name, plan) = fixture.ready(&store);
    fs::write(plan.directory.join("progress"), b"keep").unwrap();
    store.begin_removal(&name).unwrap();
    store.finish_removal(&name).unwrap();
    let reused = provisioning::plan(&config, &name).unwrap();
    assert!(!reused.created);
    store.begin_creation(&config, &name, &reused).unwrap();
    drop(store);
    let store = fixture.store();
    store.restore(&mut fixture.config()).unwrap();
    assert_eq!(fs::read(plan.directory.join("progress")).unwrap(), b"keep");
}

#[test]
fn interrupted_removal_finishes_after_intent_rename_partial_delete_and_full_delete() {
    for boundary in 0..4 {
        let fixture = Fixture::new("disposable");
        let store = fixture.store();
        let (_, name, plan) = fixture.ready(&store);
        store.begin_removal(&name).unwrap();
        if boundary >= 1 {
            provisioning::retire_storage(&plan).unwrap();
        }
        if boundary >= 2 {
            store.transition(&name, "removing", "deleting").unwrap();
            fs::remove_file(
                provisioning::tombstone(&plan)
                    .unwrap()
                    .join(".rift-instance.json"),
            )
            .unwrap();
        }
        if boundary == 3 {
            provisioning::delete_retired(&plan).unwrap();
        }
        drop(store);
        let store = fixture.store();
        let mut restored = fixture.config();
        store.restore(&mut restored).unwrap();
        assert!(restored.instances.is_empty());
        assert!(store.records().unwrap().is_empty());
        assert!(!plan.directory.exists());
        assert!(!provisioning::tombstone(&plan).unwrap().exists());
    }
}

#[test]
fn removal_recovery_refuses_to_delete_data_while_old_server_port_is_occupied() {
    let fixture = Fixture::new("disposable");
    let store = fixture.store();
    let (config, name, plan) = fixture.ready(&store);
    store.begin_removal(&name).unwrap();
    drop(store);
    let occupied = TcpListener::bind(("127.0.0.1", config.instances[&name].port)).unwrap();
    let store = fixture.store();
    let error = store
        .restore(&mut fixture.config())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&name) && error.contains("port") && error.contains("old server"),
        "{error}"
    );
    assert!(plan.directory.exists());
    drop(occupied);
    store.restore(&mut fixture.config()).unwrap();
    assert!(!plan.directory.exists());
}

#[test]
fn conflicts_are_named_and_never_drop_saved_registrations() {
    let fixture = Fixture::new("persistent");
    let store = fixture.store();
    let (config, name, plan) = fixture.ready(&store);
    let port = config.instances[&name].port;
    let occupied = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let mut restored = fixture.config();
    store.restore(&mut restored).unwrap();
    let error = store
        .reserve_restored_ports(&restored)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&name) && error.contains(&port.to_string()),
        "{error}"
    );
    drop(occupied);
    for (source, diagnostic) in [
        (
            fixture
                .source
                .replace("service_groups={games=", "service_groups={other="),
            "unknown service group",
        ),
        (
            fixture
                .source
                .replace("directory='instances/{name}'", "directory='changed/{name}'"),
            "directory changed",
        ),
        (
            fixture
                .source
                .replace("storage='persistent'", "storage='disposable'"),
            "storage changed",
        ),
        (
            fixture.source.replace(
                &format!("port_range={{{port},{port}}}"),
                "port_range={25000,25000}",
            ),
            "outside",
        ),
        (
            fixture.source.replace(
                "backends={}",
                &format!("backends={{['{name}']='127.0.0.1:24999'}}"),
            ),
            "name conflicts",
        ),
        (
            fixture.source.replace(
                "backends={}",
                &format!("backends={{static='localhost:{port}'}}"),
            ),
            "backends.static",
        ),
    ] {
        let mut candidate =
            Config::from_lua_at_unrestored(&source, &fixture.root.join("rift.lua")).unwrap();
        let error = store.restore(&mut candidate).unwrap_err().to_string();
        assert!(error.contains(diagnostic), "{diagnostic}: {error}");
        assert!(candidate.instances.is_empty());
        assert_eq!(store.records().unwrap().len(), 1);
        assert!(plan.directory.exists());
    }
}

#[test]
fn restore_populates_names_before_validating_routes_and_queues() {
    let fixture = Fixture::new("persistent");
    let store = fixture.store();
    let (_, name, _) = fixture.ready(&store);
    let source = fixture.source.replace(
        "routes={public='games'}",
        &format!("routes={{public='{name}'}}"),
    );
    let mut candidate =
        Config::from_lua_at_unrestored(&source, &fixture.root.join("rift.lua")).unwrap();
    store.restore(&mut candidate).unwrap();
    assert!(candidate.instances.contains_key(&name));
}

#[test]
fn changed_ownership_is_rejected_for_restoration_and_recovery() {
    for phase in ["creating", "ready", "removing"] {
        let fixture = Fixture::new("disposable");
        let store = fixture.store();
        let (config, name, plan) = fixture.prepare(&store);
        provisioning::provision_planned(&config, &name, &plan).unwrap();
        if phase != "creating" {
            store.commit_creation(&name).unwrap();
        }
        if phase == "removing" {
            store.begin_removal(&name).unwrap();
        }
        let mut marker = plan.marker.clone().unwrap();
        marker["generation"] = serde_json::json!("another-owner");
        fs::write(
            plan.directory.join(".rift-instance.json"),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        drop(store);
        let store = fixture.store();
        assert!(
            store
                .restore(&mut fixture.config())
                .unwrap_err()
                .to_string()
                .contains("ownership")
        );
        assert!(plan.directory.join("server.jar").exists());
        assert_eq!(store.records().unwrap().len(), 1);
    }
}

#[test]
fn exclusive_lock_corruption_and_newer_schema_have_clear_errors() {
    let fixture = Fixture::new("persistent");
    let store = fixture.store();
    assert!(
        Store::open(&store.path)
            .unwrap_err()
            .to_string()
            .contains("already in use")
    );
    let path = store.path.clone();
    store
        .connection
        .lock()
        .unwrap()
        .execute_batch("PRAGMA user_version=99")
        .unwrap();
    drop(store);
    assert!(
        Store::open(&path)
            .unwrap_err()
            .to_string()
            .contains("unsupported schema")
    );
    fs::write(&path, b"not a database").unwrap();
    assert!(
        Store::open(&path)
            .unwrap_err()
            .to_string()
            .contains("not a database")
    );
}

#[test]
fn recovery_never_deletes_a_stage_or_tombstone_reconfigured_as_a_static_server() {
    for phase in ["creating", "removing"] {
        let fixture = Fixture::new("disposable");
        let store = fixture.store();
        let (config, name, plan) = fixture.prepare(&store);
        let protected = if phase == "creating" {
            let stage = plan.stage.as_ref().unwrap();
            fs::create_dir_all(stage).unwrap();
            fs::write(stage.join("precious"), b"keep").unwrap();
            stage.clone()
        } else {
            provisioning::provision_planned(&config, &name, &plan).unwrap();
            store.commit_creation(&name).unwrap();
            store.begin_removal(&name).unwrap();
            provisioning::retire_storage(&plan).unwrap();
            provisioning::tombstone(&plan).unwrap()
        };
        let mut candidate = fixture.config();
        let mut definition = config.managed_servers[&name].clone();
        definition.directory = protected.clone();
        candidate
            .managed_servers
            .insert("static".into(), definition);
        candidate.backends.insert(
            "static".into(),
            crate::routing::Backend::parse("127.0.0.1:24999").unwrap(),
        );
        let error = store.restore(&mut candidate).unwrap_err().to_string();
        assert!(error.contains("managed_servers.static"), "{error}");
        assert!(protected.exists());
    }
}

#[test]
fn database_paths_resolve_beside_config_and_cannot_overlap_instance_storage() {
    let fixture = Fixture::new("persistent");
    let source = fixture.source.replacen(
        "return {",
        "return {instance_database='state/instances.db',",
        1,
    );
    let config = Config::from_lua_at(&source, &fixture.root.join("rift.lua")).unwrap();
    assert_eq!(
        config.instance_database,
        fixture.root.join("state/instances.db")
    );
    assert!(!config.instance_database.exists());
    for value in ["false", "''", "'   '", "'\\0'", "'.'", "'..'", "'/'"] {
        let source = fixture.source.replacen(
            "return {",
            &format!("return {{instance_database={value},"),
            1,
        );
        assert!(
            Config::from_lua_at(&source, &fixture.root.join("rift.lua"))
                .unwrap_err()
                .to_string()
                .contains("instance_database")
        );
    }
    let mut config = fixture.config();
    config.instance_database = fixture.root.join("instances/games-1/state.db");
    let port = config.service_groups["games"].port_start;
    let error = config
        .add_instance("games", "games-1", port)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("instance_database") && error.contains("overlaps"),
        "{error}"
    );
    assert!(config.instances.is_empty());
}
