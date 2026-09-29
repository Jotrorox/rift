//! Script-style configuration, local modules and folder plugins through public APIs.
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use rift::{
    auth::AuthenticatedProfile,
    config::Config,
    extensions::{Action, Context, Extensions},
    hooks::{ConnectionInfo, RouteDecision, Router},
    http_script::HttpRequest,
    message_script::MessageHandler,
    messaging::Broker,
};

const BASE: &str = r#"
    local config = require('rift.config')
    config.listeners.public = '127.0.0.1:0'
    config.backends.lobby = '127.0.0.1:25566'
    config.backends.game = '127.0.0.1:25567'
    config.routes.public = 'lobby'
"#;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rift-lua-api-{}-{}",
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
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn connection() -> ConnectionInfo {
    ConnectionInfo {
        listener: "public".into(),
        peer_addr: "127.0.0.1:12345".parse().unwrap(),
        local_addr: "127.0.0.1:25565".parse().unwrap(),
        default_backend: Some("lobby".into()),
    }
}

#[test]
fn scripts_setup_and_legacy_tables_have_the_same_validation() {
    let config = Config::from_lua(BASE, "script.lua").unwrap();
    for source in [
        "rift.setup { listeners = { public = '127.0.0.1:0' }, backends = { lobby = '127.0.0.1:25566', game = '127.0.0.1:25567' }, routes = { public = 'lobby' } }",
        "return { listeners = { public = '127.0.0.1:0' }, backends = { lobby = '127.0.0.1:25566', game = '127.0.0.1:25567' }, routes = { public = 'lobby' } }",
    ] {
        assert_eq!(Config::from_lua(source, "script.lua").unwrap(), config);
    }
    for (script, expected) in [
        ("rift.config.listners = {}", "unknown field"),
        ("rift.config.limits.max_connection = 5", "unknown field"),
        ("rift.on('typo', function() end)", "unknown Rift event"),
        ("rift.on('route', true)", "expects a function"),
        ("rift.config.on_http = true", "expected a function"),
        ("rift.config = {}", "instead of replacing"),
        (
            "rift.on('login', function() end)",
            "requires authentication",
        ),
        (
            "rift.publish('test', 'no')",
            "only available inside runtime callbacks",
        ),
    ] {
        let error = Config::from_lua(&format!("{BASE}\n{script}"), "bad.lua").unwrap_err();
        assert!(error.to_string().contains(expected), "{script}: {error}");
    }
}

#[test]
fn modules_support_nested_names_init_files_exports_and_side_effects() {
    let fixture = Fixture::new();
    fixture.write(
        "lua/options.lua",
        "local c = require('rift.config'); c.limits.max_connections = 27; loads = (loads or 0) + 1",
    );
    fixture.write(
        "lua/network/routes/init.lua",
        "assert(... == 'network.routes'); return { public = 'game' }",
    );
    fixture.write("lua/preferred.lua", "return 'file'");
    fixture.write("lua/preferred/init.lua", "return 'directory'");
    let config = fixture
        .load(
            r#"
        assert(require('rift') == rift)
        assert(require('rift.config') == rift.config)
        assert(require('options') == true and require('options') == true and loads == 1)
        rift.config.routes = require('network.routes')
        assert(require('preferred') == 'file')
    "#,
        )
        .unwrap();
    assert_eq!(config.limits.max_connections, 27);
    assert_eq!(
        config.routes["public"],
        rift::config::Route::Direct("game".into())
    );
    // The library's source-only API must not guess a filesystem base from a label.
    assert!(
        Config::from_lua(
            "require('options')",
            &fixture.0.join("rift.lua").display().to_string()
        )
        .is_err()
    );
    // Edited, not-yet-saved entry files can still resolve neighboring modules.
    Config::from_lua_at(
        &format!("{BASE}; require('options')"),
        &fixture.0.join("new.lua"),
    )
    .unwrap();
}

#[tokio::test]
async fn plugins_are_explicit_cached_and_compose_hooks_in_import_order() {
    let fixture = Fixture::new();
    fixture.write(
        "plugins/unused/init.lua",
        "error('must not run automatically')",
    );
    fixture.write(
        "plugins/a/init.lua",
        r#"
        assert(... == 'a')
        local api = require('rift')
        local choice = require('a.choice')
        local calls = 0
        api.on('route', function(ctx)
            calls = calls + 1; assert(calls == 1)
            assert(ctx.seen == 'direct'); ctx.seen = 'a'
        end)
        return { choice = choice }
    "#,
    );
    fixture.write("plugins/a/lua/a/choice.lua", "return 'game'");
    fixture.write(
        "plugins/b/init.lua",
        r#"
        rift.on('route', function(ctx)
            assert(ctx.seen == 'a')
            return { backend = require('a.choice') }
        end)
    "#,
    );
    let config = fixture
        .load(
            r#"
        local a = rift.plugin('a')
        assert(a == rift.plugin('a') and a.choice == 'game')
        assert(rift.plugin('b') == true and rift.plugin('b') == true)
        rift.on('route', function() error('first decision must stop the chain') end)
        -- Legacy/direct callbacks run before registered handlers.
        return { on_route = function(ctx) ctx.seen = 'direct' end }
    "#,
        )
        .unwrap();
    let router = Router::new(&config);
    for _ in 0..2 {
        assert_eq!(
            router.route(connection()).await.unwrap(),
            RouteDecision::Backend("game".into())
        );
    }
}

#[tokio::test]
async fn callbacks_pin_modules_and_plugins_until_reload_including_lazy_requires() {
    let fixture = Fixture::new();
    fixture.write("lua/choice.lua", "return 'game'");
    fixture.write(
        "plugins/route/init.lua",
        "rift.on('route', function() return { backend = require('choice') } end)",
    );
    let config = fixture.load("rift.plugin('route')").unwrap();
    let old = Router::new(&config);
    fixture.write("lua/choice.lua", "return 'lobby'");
    let new = Router::new(&Config::load(&fixture.0.join("rift.lua")).unwrap());
    assert_eq!(
        old.route(connection()).await.unwrap(),
        RouteDecision::Backend("game".into())
    );
    assert_eq!(
        new.route(connection()).await.unwrap(),
        RouteDecision::Backend("lobby".into())
    );
    fixture.write("plugins/route/init.lua", "error('broken update')");
    assert!(Config::load(&fixture.0.join("rift.lua")).is_err());
    fs::remove_dir_all(fixture.0.join("lua")).unwrap();
    fs::remove_dir_all(fixture.0.join("plugins")).unwrap();
    assert_eq!(
        old.route(connection()).await.unwrap(),
        RouteDecision::Backend("game".into())
    );
}

#[tokio::test]
async fn imported_api_and_publish_references_work_in_http_message_and_authenticated_hooks() {
    let fixture = Fixture::new();
    fixture.write(
        "plugins/features/lua/features/text.lua",
        "return 'from plugin'",
    );
    fixture.write("plugins/features/init.lua", r#"
        local api = require('rift')
        local publish = api.publish
        api.on('http', function(req)
            assert(api.enabled)
            publish('observations', 'http')
            return { body = require('features.text') }
        end)
        api.on('message', function(msg) publish(msg.reply, msg.payload); return true end)
        api.on('message', function(msg) publish(msg.reply, require('features.text')) end)
        api.on('login', function(ctx) assert(ctx.authenticated); publish('observations', 'login') end)
        api.on('initial_server', function() return { server = 'game' } end)
        api.command('hello', {
            permission = 'plugin.hello',
            run = function(ctx) return { message = require('features.text') .. ' ' .. ctx.args } end,
        })
    "#);
    let config = fixture
        .load(
            r#"
        rift.config.authentication = { online_mode = true }
        rift.config.forwarding = { mode = 'velocity' }
        rift.config.extensions = { permissions = { ['*'] = { ['plugin.hello'] = true } } }
        rift.config.messaging = { subscriptions = {{ subject = 'echo' }} }
        rift.plugin('features')
    "#,
        )
        .unwrap();
    fs::remove_dir_all(fixture.0.join("plugins")).unwrap();
    let broker = Broker::default();
    let mut observations = broker.subscribe("observations", None).unwrap();
    let response = config
        .on_http
        .as_ref()
        .unwrap()
        .execute_with_messaging(
            HttpRequest {
                method: "GET".into(),
                path: "/ext/test".into(),
                query: "".into(),
                body: "".into(),
                headers: BTreeMap::new(),
                context: serde_json::json!({}),
            },
            broker.clone(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.body, "from plugin");
    let mut replies = broker.subscribe("reply", None).unwrap();
    let handler = MessageHandler::new(&config, broker.clone())
        .unwrap()
        .unwrap();
    broker
        .publish_with_reply("echo", Some("reply"), bytes::Bytes::from_static(b"hello"))
        .unwrap();
    let worker = tokio::spawn(handler.run());
    for expected in ["hello", "from plugin"] {
        let reply = tokio::time::timeout(Duration::from_secs(2), replies.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&reply.payload[..], expected.as_bytes());
    }
    worker.abort();
    let _ = worker.await;
    let extensions = Extensions::new(&config, broker, None);
    let session = extensions
        .session(Context::authenticated(
            1,
            &AuthenticatedProfile {
                uuid: [1; 16],
                name: "Player".into(),
                properties: vec![],
            },
        ))
        .unwrap();
    assert_eq!(session.decision("login").await.unwrap(), Action::Continue);
    assert_eq!(
        session.decision("initial_server").await.unwrap(),
        Action::Server("game".into())
    );
    assert_eq!(
        session.command("hello friend").await.unwrap(),
        Action::Message("from plugin friend".into())
    );
    for expected in ["http", "login"] {
        let message = tokio::time::timeout(Duration::from_secs(2), observations.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&message.payload[..], expected.as_bytes());
    }
}

#[test]
fn import_errors_report_names_and_do_not_open_arbitrary_paths() {
    let fixture = Fixture::new();
    fixture.write("lua/a.lua", "return require('b')");
    fixture.write("lua/b.lua", "return require('a')");
    fixture.write("lua/broken.lua", "local nope = ");
    fixture.write("plugins/cycle/init.lua", "rift.plugin('cycle')");
    for (source, expected) in [
        ("require('missing')", "module 'missing' not found"),
        ("require('../secret')", "dotted module name"),
        ("require('/tmp/secret')", "dotted module name"),
        ("require('a..b')", "dotted module name"),
        ("require('ffi')", "module 'ffi' not found"),
        ("require('a')", "circular require"),
        ("require('broken')", "broken.lua"),
        ("rift.plugin('../secret')", "folder name"),
        ("rift.plugin('missing')", "plugins/missing/init.lua"),
        ("rift.plugin('cycle')", "circular load"),
    ] {
        let error = fixture.load(source).unwrap_err().to_string();
        assert!(error.contains(expected), "{source}: {error}");
    }
}

#[test]
fn imported_scripts_share_execution_and_source_budgets() {
    let fixture = Fixture::new();
    fixture.write("lua/runaway.lua", "while true do end");
    let error = fixture.load("require('runaway')").unwrap_err().to_string();
    assert!(
        error.contains("instruction limit") || error.contains("deadline"),
        "{error}"
    );
    fixture.write("lua/runaway.lua", "return string.rep('x', 9 * 1024 * 1024)");
    assert!(
        fixture
            .load("require('runaway')")
            .unwrap_err()
            .to_string()
            .contains("memory")
    );
    fixture.write("lua/one.lua", &" ".repeat(130 * 1024));
    fixture.write("lua/two.lua", &" ".repeat(130 * 1024));
    assert!(
        fixture
            .load("")
            .unwrap_err()
            .to_string()
            .contains("combined Lua source")
    );
}

#[test]
fn bytecode_cannot_be_loaded_as_a_module() {
    let fixture = Fixture::new();
    let lua = mlua::Lua::new();
    let bytecode = lua.load("return 42").into_function().unwrap().dump(false);
    fixture.write("lua/bytecode.lua", "");
    fs::write(fixture.0.join("lua/bytecode.lua"), bytecode).unwrap();
    assert!(fixture.load("require('bytecode')").is_err());
}

#[cfg(unix)]
#[test]
fn symbolic_links_cannot_escape_the_module_snapshot() {
    let fixture = Fixture::new();
    let outside = Fixture::new();
    outside.write("secret.lua", "return 'private'");
    fs::create_dir(fixture.0.join("lua")).unwrap();
    std::os::unix::fs::symlink(
        outside.0.join("secret.lua"),
        fixture.0.join("lua/secret.lua"),
    )
    .unwrap();
    assert!(
        fixture
            .load("require('secret')")
            .unwrap_err()
            .to_string()
            .contains("symbolic links")
    );
}

#[cfg(unix)]
#[test]
fn entry_symlinks_resolve_modules_beside_the_target_for_checks_and_reload() {
    let fixture = Fixture::new();
    let target = Fixture::new();
    target.write("rift.lua", &format!("{BASE}; require('settings')"));
    target.write(
        "lua/settings.lua",
        "rift.config.limits.max_connections = 17",
    );
    std::os::unix::fs::symlink(target.0.join("rift.lua"), fixture.0.join("rift.lua")).unwrap();
    let checked = Config::load(&fixture.0.join("rift.lua")).unwrap();
    let reloaded = Config::load(&target.0.join("rift.lua")).unwrap();
    assert_eq!(checked, reloaded);
    assert_eq!(checked.limits.max_connections, 17);
}

#[test]
fn snapshot_walks_are_bounded_and_skip_hidden_plugin_metadata() {
    let fixture = Fixture::new();
    fixture.write("plugins/a/.git/ignored.lua", &" ".repeat(300 * 1024));
    fixture.write("plugins/a/init.lua", "");
    fixture.load("rift.plugin('a')").unwrap();
    for i in 0..256 {
        fixture.write(&format!("lua/file{i}.lua"), "");
    }
    assert!(
        fixture
            .load("")
            .unwrap_err()
            .to_string()
            .contains("256 module/plugin files")
    );
}

#[test]
fn duplicate_commands_and_runtime_registrations_are_rejected() {
    for script in [
        "rift.command('hello', { permission = 'hello', run = function() end }); rift.command('hello', {})",
        "rift.config.extensions = { commands = { hello = {} } }; rift.command('hello', {})",
    ] {
        assert!(
            Config::from_lua(&format!("{BASE}; {script}"), "bad.lua")
                .unwrap_err()
                .to_string()
                .contains("duplicate Rift command")
        );
    }
    let config = Config::from_lua(
        &format!("{BASE}; rift.on('http', function() rift.on('http', function() end) end)"),
        "runtime.lua",
    )
    .unwrap();
    let error = config
        .on_http
        .unwrap()
        .evaluate(
            &HttpRequest {
                method: "GET".into(),
                path: "/ext/test".into(),
                query: "".into(),
                body: "".into(),
                headers: BTreeMap::new(),
                context: serde_json::json!({}),
            },
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("only available during initialization"),
        "{error}"
    );
}
