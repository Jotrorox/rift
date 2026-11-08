//! Installed plugins in the configuration: `rift.plugin` with `plugin.toml`,
//! the `rift.config.plugins` alias, `rift.every` and `rift.subscribe`.
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use rift::{config::Config, message_script::MessageHandler, messaging::Broker};

const BASE: &str = r#"
    local config = require('rift.config')
    config.listeners.public = '127.0.0.1:0'
    config.backends.lobby = '127.0.0.1:25566'
    config.routes.public = 'lobby'
"#;

const MANIFEST: &str = "name = \"relaxoban\"\nversion = \"0.3.1\"\napi = \"0.1\"\nengine = \"wasm\"\nentry = \"relaxoban.wasm\"\n";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rift-plugins-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    fn load(&self, script: &str) -> std::io::Result<Config> {
        self.write("rift.lua", &format!("{BASE}\n{script}"));
        Config::load(&self.0.join("rift.lua"))
    }

    fn installed(&self) {
        self.write("plugins/relaxoban/plugin.toml", MANIFEST);
        self.write("plugins/relaxoban/relaxoban.wasm", "\0asm");
        // Trusted code is never evaluated in the configuration sandbox.
        self.write(
            "plugins/relaxoban/init.lua",
            "error('persistent plugin code must not run')",
        );
        self.write("plugins/relaxoban/data/state.lua", "error('data')");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn persistent_plugins_are_recorded_with_their_configuration_and_never_evaluated() {
    let fixture = Fixture::new();
    fixture.installed();
    let config = fixture
        .load(
            r#"
        assert(rift.plugin('relaxoban', { database_env = 'RELAXOBAN_DSN', limits = { 1, 2 }, nested = {} }) == true)
    "#,
        )
        .unwrap();
    assert_eq!(config.plugins.len(), 1);
    let spec = &config.plugins[0];
    assert_eq!(spec.name, "relaxoban");
    assert_eq!(spec.manifest.version, "0.3.1");
    assert!(spec.folder.ends_with("plugins/relaxoban"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&spec.config_json).unwrap(),
        serde_json::json!({"database_env": "RELAXOBAN_DSN", "limits": [1, 2], "nested": {}})
    );
    assert_eq!(spec.fingerprint.len(), 64);
    // Without a table the plugin gets `{}`; the data directory is not part of
    // the fingerprint, the artifact is.
    fixture.write("plugins/relaxoban/data/state.lua", "changed");
    let same = fixture.load("rift.plugin('relaxoban')").unwrap();
    assert_eq!(same.plugins[0].config_json, "{}");
    assert_eq!(same.plugins[0].fingerprint, spec.fingerprint);
    fixture.write("plugins/relaxoban/relaxoban.wasm", "\0asm\x01");
    let changed = fixture.load("rift.plugin('relaxoban')").unwrap();
    assert_ne!(changed.plugins[0].fingerprint, spec.fingerprint);
}

#[test]
fn hidden_paths_neither_hold_the_entry_nor_hide_symbolic_links() {
    let fixture = Fixture::new();
    fixture.installed();
    fixture.write(
        "plugins/relaxoban/plugin.toml",
        &MANIFEST.replace("relaxoban.wasm", ".build/relaxoban.wasm"),
    );
    fixture.write("plugins/relaxoban/.build/relaxoban.wasm", "\0asm");
    let error = fixture
        .load("rift.plugin('relaxoban')")
        .unwrap_err()
        .to_string();
    assert!(error.contains("hidden"), "{error}");
    fixture.write("plugins/relaxoban/plugin.toml", MANIFEST);
    // Hidden files are still left out of the fingerprint...
    let before = fixture.load("rift.plugin('relaxoban')").unwrap().plugins[0]
        .fingerprint
        .clone();
    fixture.write("plugins/relaxoban/.notes", "changed");
    let after = fixture.load("rift.plugin('relaxoban')").unwrap().plugins[0]
        .fingerprint
        .clone();
    assert_eq!(before, after);
    // ...but a hidden symbolic link is refused like any other.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&fixture.0, fixture.0.join("plugins/relaxoban/.escape"))
            .unwrap();
        let error = fixture
            .load("rift.plugin('relaxoban')")
            .unwrap_err()
            .to_string();
        assert!(error.contains("symbolic links"), "{error}");
        fs::remove_file(fixture.0.join("plugins/relaxoban/.escape")).unwrap();
    }
}

#[test]
fn webassembly_grants_covering_the_plugin_cache_are_refused() {
    let fixture = Fixture::new();
    fixture.installed();
    // The filesystem root: `/`, or the drive on Windows (a TOML literal
    // string, so its backslash is not an escape).
    let root = fixture.0.ancestors().last().unwrap().display().to_string();
    fixture.write(
        "plugins/relaxoban/plugin.toml",
        &format!("{MANIFEST}[capabilities]\nfilesystem = ['{root}']\n"),
    );
    let error = fixture
        .load("rift.plugin('relaxoban')")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&format!(
            "plugin relaxoban: capabilities.filesystem: {root} overlaps"
        )),
        "{error}"
    );
}

#[test]
fn script_mode_manifests_load_in_the_sandbox_as_before() {
    let fixture = Fixture::new();
    fixture.write(
        "plugins/greeter/plugin.toml",
        "name = \"greeter\"\nversion = \"1\"\napi = \"0.1\"\nmode = \"script\"\n",
    );
    fixture.write(
        "plugins/greeter/init.lua",
        "local name, options = ...\nassert(name == 'greeter' and options.backend == 'lobby')\nrift.on('route', function() return { backend = options.backend } end)",
    );
    let config = fixture
        .load("rift.plugin('greeter', { backend = 'lobby' })")
        .unwrap();
    assert!(config.plugins.is_empty());
    assert!(config.on_route.is_some());
}

#[test]
fn invalid_installed_plugins_and_registrations_are_rejected() {
    let fixture = Fixture::new();
    fixture.installed();
    for (script, expected) in [
        (
            "rift.plugin('relaxoban'); rift.plugin('relaxoban')",
            "already registered",
        ),
        (
            "rift.plugin('relaxoban', 'x')",
            "optional configuration table",
        ),
        (
            "rift.plugin('relaxoban', { f = function() end })",
            "cannot contain a function",
        ),
        (
            "rift.plugin('relaxoban', { 1, x = 2 })",
            "dense list or have only string keys",
        ),
        ("rift.plugin('missing')", "plugins/missing/plugin.toml"),
    ] {
        let error = fixture.load(script).unwrap_err().to_string();
        assert!(error.contains(expected), "{script}: {error}");
    }
    fixture.write(
        "plugins/relaxoban/plugin.toml",
        &format!("{MANIFEST}typo = true\n"),
    );
    let error = fixture
        .load("rift.plugin('relaxoban')")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("plugin.toml") && error.contains("typo"),
        "{error}"
    );
    // An unreferenced broken manifest is not an error.
    fixture.load("").unwrap();
    fixture.write(
        "plugins/relaxoban/plugin.toml",
        &MANIFEST.replace("relaxoban.wasm", "missing.wasm"),
    );
    let error = fixture
        .load("rift.plugin('relaxoban')")
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing.wasm"), "{error}");
}

#[test]
fn plugins_is_an_alias_for_extensions() {
    let fixture = Fixture::new();
    let auth = "config.authentication = { online_mode = true }\nconfig.forwarding = { mode = 'velocity', secret_env = 'TEST_SECRET' }\n";
    let config = fixture
        .load(&format!(
            "{auth}config.plugins = {{ api_version = 2, login = function() end }}"
        ))
        .unwrap();
    assert!(config.extensions.is_some());
    let error = fixture
        .load(&format!(
            "{auth}config.plugins = {{ api_version = 2 }}\nconfig.extensions = {{ api_version = 2 }}"
        ))
        .unwrap_err()
        .to_string();
    assert!(error.contains("not both"), "{error}");
}

#[test]
fn every_registers_an_api_v2_job() {
    let fixture = Fixture::new();
    let auth = "config.authentication = { online_mode = true }\nconfig.forwarding = { mode = 'velocity', secret_env = 'TEST_SECRET' }\n";
    let config = fixture
        .load(&format!(
            "{auth}rift.every('tick', 1000, function() end)\nrift.on('login', function() end)"
        ))
        .unwrap();
    let extensions = config.extensions.unwrap();
    assert!(extensions.has_jobs());
    assert!(extensions.permission_groups().is_some(), "api_version = 2");
    for (script, expected) in [
        (
            "rift.every('tick', 1000, function() end) rift.every('tick', 1000, function() end)",
            "duplicate rift.every job",
        ),
        ("rift.every('tick', 5, function() end)", "every_ms"),
    ] {
        let error = fixture
            .load(&format!("{auth}{script}"))
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[tokio::test]
async fn subscribe_routes_messages_by_subscription() {
    let fixture = Fixture::new();
    let config = fixture
        .load(
            r#"
        rift.subscribe('plugin.a', function(message) rift.publish('reply', 'a:' .. message.payload) end)
        rift.subscribe('plugin.b', 'workers', function(message) rift.publish('reply', 'b:' .. message.payload) end)
        config.messaging = { subscriptions = { { subject = 'plugin.c' } } }
        rift.on('message', function(message) rift.publish('reply', 'other:' .. message.subject) end)
    "#,
        )
        .unwrap();
    let subscriptions = &config.messaging.as_ref().unwrap().subscriptions;
    assert_eq!(subscriptions.len(), 3);
    assert_eq!(subscriptions[0].subject, "plugin.c");
    assert_eq!(subscriptions[2].queue.as_deref(), Some("workers"));
    let broker = Broker::default();
    let mut replies = broker.subscribe("reply", None).unwrap();
    let handler = MessageHandler::new(&config, broker.clone())
        .unwrap()
        .unwrap();
    let worker = tokio::spawn(handler.run());
    let mut received = Vec::new();
    for (subject, payload) in [("plugin.a", "1"), ("plugin.b", "2"), ("plugin.c", "3")] {
        broker
            .publish(subject, bytes::Bytes::copy_from_slice(payload.as_bytes()))
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(2), replies.recv())
            .await
            .unwrap()
            .unwrap();
        received.push(String::from_utf8(reply.payload.to_vec()).unwrap());
    }
    assert_eq!(received, ["a:1", "b:2", "other:plugin.c"]);
    worker.abort();
    let _ = worker.await;
}
