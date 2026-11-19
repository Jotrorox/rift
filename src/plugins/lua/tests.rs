//! The persistent Lua engine: the shared conformance scenarios, the example
//! plugin, and what only Lua has (environment, interrupts, late registration,
//! `rift.http`, script-mode parity).
use crate::{
    auth::AuthenticatedProfile,
    config::Config,
    extensions::{ExtensionSession, Extensions},
    messaging::Broker,
    plugins::{
        api::{Action, Context, Event, Reply},
        conformance::{self, Fixture, conformance_tests},
        worker::{Engines, Outcome, Status},
    },
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

/// The conformance guest as a Lua plugin folder.
struct LuaFixture;

impl Fixture for LuaFixture {
    fn engines(&self) -> Engines {
        Engines::builtin()
    }

    fn install(&self, folder: &Path, name: &str, extra: &str) {
        // The memory limit gives the guest's "fatal" behaviour its trap.
        std::fs::write(
            folder.join("plugin.toml"),
            format!(
                "name = \"{name}\"\nversion = \"1\"\napi = \"0.2\"\nengine = \"lua\"\n{extra}\n[lua]\nmemory_limit_mb = 64\n"
            ),
        )
        .unwrap();
        std::fs::write(folder.join("init.lua"), include_str!("conformance.lua")).unwrap();
    }
}

conformance_tests!(LuaFixture);

#[tokio::test]
async fn example_plugin_follows_the_shared_spec() {
    conformance::hello_example(
        Engines::builtin(),
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/hello-lua"),
    )
    .await;
}

const BASE: &str = "listeners = { public = '127.0.0.1:0' }, backends = { lobby = '127.0.0.1:25001' }, routes = { public = 'lobby' }, authentication = { online_mode = true }, forwarding = { mode = 'velocity', secret_env = 'TEST_SECRET' }";

fn directory() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "rift-lua-plugin-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// A configuration directory with `plugins/t` holding `files` and a Lua
/// manifest with `extra` lines.
fn folder(extra: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = directory();
    let plugin = root.join("plugins/t");
    std::fs::create_dir_all(&plugin).unwrap();
    for (path, source) in files {
        let path = plugin.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    std::fs::write(
        plugin.join("plugin.toml"),
        format!("name = \"t\"\nversion = \"1\"\napi = \"0.2\"\nengine = \"lua\"\n{extra}"),
    )
    .unwrap();
    root
}

fn runtime(root: &Path, script: &str, config_json: &str, broker: &Broker) -> Extensions {
    let mut config =
        Config::from_lua(&format!("return {{ {BASE}, {script} }}"), "lua.lua").unwrap();
    config.plugins = vec![conformance::installed(&root.join("plugins/t"), config_json)];
    Extensions::with_engines(&config, broker.clone(), None, Engines::builtin()).unwrap()
}

async fn started(root: &Path, config_json: &str, broker: &Broker) -> Extensions {
    let runtime = runtime(root, "", config_json, broker);
    runtime.plugins().start().await.unwrap();
    runtime
}

fn session(runtime: &Extensions, id: u64, name: &str) -> ExtensionSession {
    let mut context = crate::extensions::Context::authenticated(
        id,
        &AuthenticatedProfile {
            uuid: [id as u8; 16],
            name: name.into(),
            properties: vec![],
        },
    );
    context.default_server = Some("lobby".into());
    runtime.session(context).unwrap()
}

#[tokio::test]
async fn state_persists_and_the_environment_is_trusted() {
    let root = folder(
        "",
        &[
            (
                "init.lua",
                r#"
local name, config = ...
local helper = require("helper")
local nested = require("deep.module")
assert(name == "t" and config.greeting == "call" and nested == 42)
assert(rift.plugin_name == "t" and rift.meta.version == "1" and rift.meta.config_dir == nil)
assert(ffi == nil and package.loadlib == nil and package.cpath == "")
assert(not pcall(os.exit))
assert(("hello"):find("l") == 3 and coroutine and string.gsub)
assert(require("rift") == rift and require("rift.store") == rift.store)
local file = assert(io.open(rift.meta.data_dir .. "/note.txt", "w"))
file:write("written by init")
file:close()
local count = 0
rift.on("login", function(ctx)
    count = count + 1
    if count == 2 then
        return { deny = helper.text(config.greeting, count) }
    end
end)
"#,
            ),
            (
                "lua/helper.lua",
                "return { text = function(word, n) return word .. ' ' .. n end }",
            ),
            ("deep/module/init.lua", "return 42"),
        ],
    );
    let broker = Broker::default();
    let runtime = started(&root, r#"{"greeting":"call"}"#, &broker).await;
    assert_eq!(
        session(&runtime, 1, "A").decision("login").await.unwrap(),
        Action::Continue
    );
    assert_eq!(
        session(&runtime, 1, "A").decision("login").await.unwrap(),
        Action::Deny("call 2".into()),
        "globals and upvalues persist between calls"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("plugins/t/data/note.txt")).unwrap(),
        "written by init"
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn runaway_handlers_hit_their_deadline_and_the_state_survives() {
    let root = folder(
        "[deadlines]\nlogin = 50\nbefore_transfer = 50\n",
        &[(
            "init.lua",
            r#"
local calls = 0
rift.on("login", function()
    calls = calls + 1
    while true do rift.time() end
end)
rift.on("before_transfer", function()
    -- pcall and rift.try do not swallow an interrupt.
    pcall(function() while true do rift.time() end end)
    rift.try(function() while true do rift.time() end end)
    return { deny = "caught" }
end)
rift.on("initial_server", function()
    return { deny = "still alive after " .. calls }
end)
"#,
        )],
    );
    let broker = Broker::default();
    let runtime = started(&root, "{}", &broker).await;
    let instance = runtime.plugins().active()[0].clone();
    for event in [
        Event::Login(Context::default()),
        Event::BeforeTransfer(Context::default()),
    ] {
        let started = Instant::now();
        let outcome = instance.call(event, Duration::from_secs(5)).await;
        let Outcome::Failed(error) = outcome else {
            panic!("expected a handler error, got {outcome:?}");
        };
        assert!(error.contains("deadline exceeded"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    assert_eq!(instance.status(), Status::Running);
    assert_eq!(
        instance
            .call(
                Event::InitialServer(Context::default()),
                Duration::from_secs(5)
            )
            .await,
        Outcome::Reply(Reply::Action(Action::Deny("still alive after 1".into())))
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn without_the_jit_a_pure_lua_loop_is_interrupted() {
    let root = folder(
        "[deadlines]\nlogin = 50\n[lua]\njit = false\n",
        &[(
            "init.lua",
            r#"
assert(not jit.status(), "the compiler must be off")
assert(not pcall(jit.on), "jit.on must refuse")
rift.on("login", function()
    -- Calls no Rift function: only the instruction hook can stop it.
    while true do end
end)
rift.on("initial_server", function()
    local n = 0
    for i = 1, 100000 do n = n + i end
    return { deny = "alive " .. n .. " " .. tostring(jit.status()) }
end)
"#,
        )],
    );
    let broker = Broker::default();
    let runtime = started(&root, "{}", &broker).await;
    let instance = runtime.plugins().active()[0].clone();
    let started = Instant::now();
    let outcome = instance
        .call(Event::Login(Context::default()), Duration::from_secs(5))
        .await;
    let Outcome::Failed(error) = outcome else {
        panic!("expected a handler error, got {outcome:?}");
    };
    assert!(error.contains("deadline exceeded"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(instance.status(), Status::Running);
    assert_eq!(
        instance
            .call(
                Event::InitialServer(Context::default()),
                Duration::from_secs(5)
            )
            .await,
        Outcome::Reply(Reply::Action(Action::Deny("alive 5000050000 false".into())))
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn late_registration_is_refused_but_timers_work_with_their_hook() {
    let root = folder(
        "",
        &[(
            "init.lua",
            r#"
rift.every("heartbeat", 3600000, function() end)
rift.on("login", function()
    local _, on = pcall(rift.on, "join", function() end)
    local _, command = pcall(rift.command, "x", { permission = "x", run = function() end })
    local _, subscribe = pcall(rift.subscribe, "late.subject", function() end)
    rift.timers.after(10, function() rift.publish("late.fired", "fired") end)
    return { deny = on .. " | " .. command .. " | " .. subscribe }
end)
"#,
        )],
    );
    let broker = Broker::default();
    let mut fired = broker.subscribe("late.fired", None).unwrap();
    let runtime = started(&root, "{}", &broker).await;
    let Action::Deny(errors) = session(&runtime, 1, "A").decision("login").await.unwrap() else {
        panic!("login should deny with the errors");
    };
    let errors = errors.to_string();
    assert!(
        errors.contains("rift.on is only available while the plugin initializes"),
        "{errors}"
    );
    assert!(
        errors.contains("rift.command is only available"),
        "{errors}"
    );
    assert!(
        errors.contains("registered no message hook during initialization"),
        "{errors}"
    );
    let message = tokio::time::timeout(Duration::from_secs(5), fired.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.payload.as_ref(), b"fired");
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn init_errors_fail_startup_with_the_lua_message() {
    let broker = Broker::default();
    for (source, expected) in [
        ("rift.on('login', function() end", "init.lua"),
        ("error('no database')", "no database"),
        (
            "rift.on('nope', function() end)",
            "unknown Rift event: nope",
        ),
        (
            "rift.command('x', { permission = 'x', run = function() end, suggest = 3 })",
            "suggest must be",
        ),
    ] {
        let root = folder("[failure]\nrestart = \"never\"\n", &[("init.lua", source)]);
        let runtime = runtime(&root, "", "{}", &broker);
        let error = runtime.plugins().start().await.unwrap_err().to_string();
        assert!(error.contains(expected), "{source}: {error}");
    }
    let root = folder(
        "entry = \"missing.lua\"\n[failure]\nrestart = \"never\"\n",
        &[],
    );
    let error = runtime(&root, "", "{}", &broker)
        .plugins()
        .start()
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("missing.lua"), "{error}");
}

#[tokio::test]
async fn http_requests_need_the_network_capability() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let read = socket.read(&mut request).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nX-Test: a\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
        String::from_utf8_lossy(&request[..read]).into_owned()
    });
    let source = r#"
local _, config = ...
rift.on("login", function()
    local response = rift.http.request(config.url, {
        method = "POST", body = "ping", headers = { ["X-Plugin"] = "t" },
    })
    return { deny = response.status .. " " .. response.body .. " " .. response.headers["x-test"] }
end)
"#;
    let config_json = serde_json::json!({ "url": format!("http://{address}/hook") }).to_string();
    let broker = Broker::default();
    let root = folder("", &[("init.lua", source)]);
    let runtime = started(&root, &config_json, &broker).await;
    assert_eq!(
        session(&runtime, 1, "A").decision("login").await.unwrap(),
        Action::Deny("201 ok a".into())
    );
    let request = server.await.unwrap();
    assert!(request.starts_with("POST /hook HTTP/1.1"), "{request}");
    assert!(
        request.to_ascii_lowercase().contains("x-plugin: t"),
        "{request}"
    );
    runtime.plugins().shutdown().await;

    let root = folder("[capabilities]\nnetwork = false\n", &[("init.lua", source)]);
    let runtime = started(&root, &config_json, &broker).await;
    let error = session(&runtime, 1, "A")
        .decision("login")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("network access is not granted"), "{error}");
    runtime.plugins().shutdown().await;
}

/// A handler body written for `rift.lua` runs unchanged as a plugin.
const HANDLERS: &str = r#"
rift.on("login", function(ctx)
    if ctx.name == "Blocked" and not ctx.has_permission("vip.join") then
        return { deny = "Blocked (api " .. ctx.api_version .. ")" }
    end
end)
rift.on("initial_server", function(ctx)
    if ctx.has_permission("vip.join") then return { server = "lobby" } end
end)
rift.command("greet", {
    permission = "greet.use",
    suggest = function(ctx) return { "hello", { text = "hey", tooltip = ctx.word } } end,
    run = function(ctx)
        local visits = rift.store.increment("visits", ctx.uuid, 1)
        return { message = "Hi " .. ctx.name .. " " .. ctx.args .. " #" .. visits }
    end,
})
"#;

#[tokio::test]
async fn the_same_handler_body_runs_in_rift_lua_and_as_a_plugin() {
    let broker = Broker::default();
    let script = Extensions::new(
        &Config::from_lua(
            &format!(
                "rift.config.extensions = {{ api_version = 2 }}\n{HANDLERS}\nreturn {{ {BASE} }}"
            ),
            "rift.lua",
        )
        .unwrap(),
        broker.clone(),
        None,
    )
    .unwrap();
    let root = folder("", &[("init.lua", HANDLERS)]);
    let plugin = started(&root, "{}", &broker).await;
    for runtime in [&script, &plugin] {
        let permissions = &runtime.plugins().persistent.base.permissions;
        let vip = format!("{:02x}", 2u8).repeat(16);
        permissions.grant(&vip, "vip.join");
        permissions.grant(&vip, "greet.use");
        assert_eq!(
            session(runtime, 1, "Blocked")
                .decision("login")
                .await
                .unwrap(),
            Action::Deny("Blocked (api 3)".into())
        );
        let vip = session(runtime, 2, "Blocked");
        assert_eq!(vip.decision("login").await.unwrap(), Action::Continue);
        assert_eq!(
            vip.decision("initial_server").await.unwrap(),
            Action::Server("lobby".into())
        );
        assert_eq!(
            vip.command("greet there").await.unwrap(),
            Action::Message("Hi Blocked there #1".into())
        );
        let suggestions = vip.suggest("greet", "he", Vec::new).await.unwrap();
        assert_eq!(
            suggestions
                .iter()
                .map(|suggestion| (suggestion.text.as_str(), suggestion.tooltip.clone()))
                .collect::<Vec<_>>(),
            [("hello", None), ("hey", Some("he".into()))]
        );
        assert_eq!(
            session(runtime, 3, "Other").command("greet").await.unwrap(),
            Action::Message("You do not have permission to use this command.".into())
        );
    }
    plugin.plugins().shutdown().await;
}

#[tokio::test]
async fn plugins_show_dialogs_and_receive_their_clicks() {
    let root = folder(
        "",
        &[(
            "init.lua",
            r#"
rift.on("login", function(ctx)
    assert(rift.players.dialog(ctx.uuid, {
        type = "notice",
        title = "Welcome",
        inputs = { { type = "text", key = "nick", label = "Nickname" } },
        action = { label = "Save", action = { type = "dynamic/custom", id = "save", additions = { step = 2 } } },
    }))
    local ok, err = pcall(rift.players.dialog, ctx.uuid, { type = "notice" })
    assert(not ok and err:find("title is required"), err)
end)
rift.on("dialog", function(ctx)
    rift.players.message(ctx.uuid, ctx.action .. " " .. ctx.values.nick .. " " .. ctx.payload.step)
end)
"#,
        )],
    );
    let broker = Broker::default();
    let runtime = started(&root, "{}", &broker).await;
    let (_handle, mut actions) = runtime.players().register(1, &[1; 16], "A", 774, "lobby");
    let player = session(&runtime, 1, "A");
    assert_eq!(player.decision("login").await.unwrap(), Action::Continue);
    let Some(crate::extensions::PlayerAction::Dialog { dialog, owner }) = actions.try_recv().ok()
    else {
        panic!("expected a dialog");
    };
    assert_eq!(owner, crate::dialog::Owner::Plugin("t".into()));
    let id = dialog.wire()["action"]["action"]["id"].as_str().unwrap();
    let (response, closes) = dialog
        .resolve(id, Some(serde_json::json!({"nick": "Steve"})))
        .unwrap();
    assert!(closes);
    player.dialog(owner, response).await.unwrap();
    let message = tokio::time::timeout(Duration::from_secs(5), actions.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        message,
        crate::extensions::PlayerAction::Message("save Steve 2".into())
    );
    // A click for a plugin that is not configured is dropped.
    let (response, _) = dialog.resolve(id, None).unwrap();
    player
        .dialog(crate::dialog::Owner::Plugin("gone".into()), response)
        .await
        .unwrap();
    runtime.plugins().shutdown().await;
    assert!(actions.try_recv().is_err());
}

/// A player table carries the client's `locale` once the session published
/// it; before that it is nil.
#[tokio::test]
async fn player_tables_carry_the_client_locale_once_it_is_known() {
    let root = folder(
        "",
        &[(
            "init.lua",
            r#"
rift.on("login", function(ctx)
    local player = rift.players.get(ctx.uuid)
    assert(rift.players.list()[1].locale == player.locale)
    return { deny = "locale=" .. tostring(player.locale) }
end)
"#,
        )],
    );
    let broker = Broker::default();
    let runtime = started(&root, "{}", &broker).await;
    let (handle, _actions) = runtime
        .players()
        .register(7, &[7; 16], "Alex", 774, "lobby");
    let player = session(&runtime, 7, "Alex");
    assert_eq!(
        player.decision("login").await.unwrap(),
        Action::Deny("locale=nil".into())
    );
    for locale in ["de_de", "en_us"] {
        handle.set_locale(Some(locale));
        assert_eq!(
            player.decision("login").await.unwrap(),
            Action::Deny(format!("locale={locale}").as_str().into())
        );
    }
    assert_eq!(
        runtime.players().list(None).unwrap()[0].locale.as_deref(),
        Some("en_us")
    );
    runtime.plugins().shutdown().await;
}

/// `examples/plugins/vanish-lua`: `/vanish` sets the `rift.visible-to` meta
/// key; the directory's watcher resolves it onto both records, publishes a
/// `visibility` event, and every viewer without the node loses the player.
#[tokio::test]
async fn the_vanish_example_hides_a_player_from_viewers_without_the_node() {
    use crate::players::Viewer;
    let broker = Broker::default();
    let mut events = broker
        .subscribe(crate::extensions::players::EVENTS, None)
        .unwrap();
    let mut config = Config::from_lua(&format!("return {{ {BASE} }}"), "vanish.lua").unwrap();
    config.plugins = vec![conformance::installed(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/vanish-lua"),
        r#"{"see":"rift.vanish.see"}"#,
    )];
    let runtime =
        Extensions::with_engines(&config, broker.clone(), None, Engines::builtin()).unwrap();
    runtime.plugins().start().await.unwrap();
    let players = runtime.players().clone();
    let watcher = tokio::spawn(players.clone().watch());
    let staff = [2; 16];
    let permissions = &runtime.plugins().persistent.base.permissions;
    permissions.grant(&"02".repeat(16), "rift.vanish.use");
    permissions.grant(&"03".repeat(16), "rift.vanish.see");
    let _roster = players.roster().register(staff, "Staff", "lobby").unwrap();
    let (_staff, _actions) = players.register(2, &staff, "Staff", 774, "lobby");
    let mut session = session(&runtime, 2, "Staff");
    session.ready("lobby").await;
    let join: serde_json::Value =
        serde_json::from_slice(&events.recv().await.unwrap().payload).unwrap();
    assert_eq!(join["visible_to"], serde_json::Value::Null);

    assert_eq!(
        session.command("vanish").await.unwrap(),
        Action::Message("You are vanished.".into())
    );
    let event: serde_json::Value = serde_json::from_slice(
        &tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap()
            .payload,
    )
    .unwrap();
    assert_eq!(event["event"], "visibility");
    assert_eq!(event["visible_to"], "rift.vanish.see");
    assert_eq!(event["previous_visible_to"], serde_json::Value::Null);
    let roster = players.roster();
    assert_eq!(roster.count_for(Viewer::Public, None), 0);
    assert_eq!(roster.count_for(Viewer::System, None), 1);
    assert_eq!(
        players.list(None).unwrap()[0].visible_to.as_deref(),
        Some("rift.vanish.see")
    );
    let check = |uuid: [u8; 16]| {
        move |node: &str| {
            permissions.check_active(rift_permissions::Uuid::from_bytes(uuid), node)
                == rift_permissions::Tristate::True
        }
    };
    let (plain, moderator) = (check([4; 16]), check([3; 16]));
    assert!(
        players
            .names_for(Viewer::Player {
                uuid: [4; 16],
                holds: &plain
            })
            .is_empty()
    );
    assert_eq!(
        players.names_for(Viewer::Player {
            uuid: [3; 16],
            holds: &moderator
        }),
        ["Staff"]
    );

    // A timed vanish replaces the global node with one that expires.
    assert_eq!(
        session.command("vanish 2").await.unwrap(),
        Action::Message("You are vanished for 2 minutes.".into())
    );
    let user = permissions
        .user(rift_permissions::Uuid::from_bytes(staff))
        .unwrap();
    let node = user
        .nodes
        .iter()
        .find(|node| node.payload.key() == crate::players::VISIBLE_TO)
        .unwrap();
    assert!(node.expires_at.is_some() && node.contexts.is_empty());
    assert_eq!(
        node.source.to_string(),
        "plugin:vanish-lua",
        "the change log names the plugin"
    );
    assert_eq!(
        session.command("vanish").await.unwrap(),
        Action::Message("You are visible again.".into())
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while roster.count_for(Viewer::Public, None) == 0 {
        assert!(Instant::now() < deadline, "the player stayed hidden");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(players.list(None).unwrap()[0].visible_to, None);
    watcher.abort();
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn kick_handlers_register_the_hook_and_choose_where_the_player_goes() {
    let source = r#"
rift.on("kick", function(ctx)
    if ctx.reason == "Round over" then return { server = "lobby" } end
    if ctx.server == "arena" then return { deny = ctx.name .. " left " .. ctx.server } end
end)
"#;
    let root = folder("", &[("init.lua", source)]);
    let runtime = started(&root, "{}", &Broker::default()).await;
    assert!(
        runtime
            .plugins()
            .may_handle(crate::plugins::api::Hook::Kick)
    );
    let player = session(&runtime, 1, "Alex");
    assert_eq!(
        player.kick("game", "Round over").await.unwrap(),
        Action::Server("lobby".into())
    );
    assert_eq!(
        player.kick("arena", "Server closed").await.unwrap(),
        Action::Deny("Alex left arena".into())
    );
    assert_eq!(
        player.kick("game", "Server closed").await.unwrap(),
        Action::Continue
    );
    runtime.plugins().shutdown().await;
}

/// What 0.2 added, from Lua: command aliases and the console sender, the
/// player directory and languages, timed groups, and answers that wait for
/// a background worker.
#[tokio::test]
async fn aliases_the_console_the_directory_and_background_work_reach_lua() {
    let root = folder(
        "[background]\nworkers = 2\n[deadlines]\ncommand = 5000\nlogin = 5000\n",
        &[
            (
                "init.lua",
                r#"
assert(rift.meta.api == "0.2.1")
assert(not rift.background.is_worker())
assert(not pcall(rift.on, "work", function() end), "work belongs in the worker script")
rift.command("whois", {
    permission = "", aliases = { "who", "w" }, console = true,
    run = function(ctx)
        local found = rift.players.lookup(ctx.args)
        if not found then return { message = "unknown" } end
        return { message = table.concat({
            ctx.console and "console" or ctx.name, ctx.command, found.name,
            tostring(found.online), tostring(found.language), found.last_server,
            found.last_ip, #rift.players.search("al"),
        }, " ") }
    end,
})
rift.command("lang", { permission = "", run = function(ctx)
    local known = rift.players.set_language(ctx.uuid, ctx.args ~= "" and ctx.args or nil)
    return { message = tostring(known) .. " " .. tostring(ctx.language) }
end })
rift.command("temp", { permission = "", run = function(ctx)
    local added = rift.permissions.add_group(ctx.uuid, "vip", { expires_in = 3600 })
    assert(not pcall(rift.permissions.add_group, ctx.uuid, "vip", { expires_in = 1, expires_at = 2 }))
    return { message = tostring(added) .. " " .. table.concat(rift.permissions.groups(ctx.uuid), ",") }
end })
-- The callback form: the answer follows when the worker is done.
rift.command("slow", { permission = "", run = function(ctx)
    local ticket = rift.background.defer()
    assert(not pcall(rift.background.defer), "one answer per call")
    rift.background.run(ctx.args, function(result, err)
        assert(rift.background.resolve(ticket, { message = result or ("failed: " .. err) }))
        assert(not rift.background.resolve(ticket, nil), "answered already")
    end)
    return { message = "ignored" }
end })
-- The event form, for a lookup at login.
local waiting = {}
rift.on("login", function(ctx)
    local ticket = rift.background.defer()
    waiting[rift.background.run("login " .. ctx.name)] = ticket
end)
rift.on("job_done", function(handle, result, err)
    local ticket = waiting[handle]
    waiting[handle] = nil
    rift.background.resolve(ticket, result == "LOGIN BLOCKED" and { deny = "Banned" } or nil)
end)
"#,
            ),
            (
                "worker.lua",
                r#"
local name, config = ...
assert(name == "t" and config.greeting == "hi")
assert(rift.background.is_worker())
assert(not pcall(rift.on, "login", function() end), "a worker receives no events")
rift.on("work", function(payload)
    if payload == "fail" then error("worker says no") end
    return payload:upper()
end)
"#,
            ),
        ],
    );
    let broker = Broker::default();
    let runtime = started(&root, r#"{"greeting":"hi"}"#, &broker).await;
    let (_handle, _actions) = runtime.players().register_with(
        7,
        &[7; 16],
        "Alex",
        774,
        "lobby",
        crate::extensions::Profile {
            ip: "192.0.2.7".into(),
            properties: Vec::new(),
        },
    );
    // The join is written off the caller's task.
    for _ in 0..200 {
        if runtime.players().lookup("alex").unwrap().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let message = |text: &str| Action::Message(text.into());
    let alex = session(&runtime, 7, "Alex");
    assert_eq!(
        alex.commands().iter().filter(|name| *name == "w").count(),
        1
    );
    // An alias runs the command, which sees its own name.
    assert_eq!(
        alex.command("w alex").await.unwrap(),
        message("Alex whois Alex true nil lobby 192.0.2.7 1")
    );
    assert_eq!(
        alex.command("whois nobody").await.unwrap(),
        message("unknown")
    );
    // A language is remembered for the player and known to the next session.
    assert_eq!(
        alex.command("lang de-DE").await.unwrap(),
        message("true nil")
    );
    assert_eq!(
        runtime
            .players()
            .get("Alex")
            .unwrap()
            .unwrap()
            .language
            .as_deref(),
        Some("de_de")
    );
    let alex = session(&runtime, 7, "Alex");
    assert_eq!(alex.command("lang").await.unwrap(), message("true de_de"));
    assert_eq!(
        runtime.players().lookup("Alex").unwrap().unwrap().language,
        None
    );
    assert!(alex.command("lang x").await.is_err(), "not a language tag");
    // The console runs only commands that ask for it, as CONSOLE.
    let plugins = runtime.plugins();
    assert_eq!(
        plugins.console("/who Alex").await.unwrap().to_string(),
        "console whois Alex true nil lobby 192.0.2.7 1"
    );
    let error = plugins.console("lang de").await.unwrap_err();
    assert!(error.contains("players only"), "{error}");
    assert!(plugins.console("nope").await.is_err());
    // A timed membership.
    let groups = alex.command("temp").await.unwrap();
    let Action::Message(groups) = groups else {
        panic!("{groups:?}");
    };
    let groups = groups.to_string();
    assert!(groups.starts_with("true "), "{groups}");
    // Deferred answers: the hook returned long before the worker answered.
    assert_eq!(alex.command("slow hello").await.unwrap(), message("HELLO"));
    let failed = alex.command("slow fail").await.unwrap();
    let Action::Message(failed) = failed else {
        panic!("{failed:?}");
    };
    let failed = failed.to_string();
    assert!(
        failed.starts_with("failed: ") && failed.contains("worker says no"),
        "{failed}"
    );
    assert_eq!(
        session(&runtime, 8, "Blocked")
            .decision("login")
            .await
            .unwrap(),
        Action::Deny("Banned".into())
    );
    assert_eq!(alex.decision("login").await.unwrap(), Action::Continue);
    runtime.plugins().shutdown().await;
}

/// Without `[background]` there are no workers to give a job to.
#[tokio::test]
async fn background_jobs_need_workers() {
    let root = folder(
        "",
        &[(
            "init.lua",
            r#"
rift.command("slow", { permission = "", run = function(ctx)
    local ok, err = pcall(rift.background.run, "x")
    return { message = tostring(ok) .. " " .. err }
end })
"#,
        )],
    );
    let broker = Broker::default();
    let runtime = started(&root, "{}", &broker).await;
    let Action::Message(text) = session(&runtime, 1, "Alex").command("slow").await.unwrap() else {
        panic!("expected a message");
    };
    let text = text.to_string();
    assert!(
        text.starts_with("false ") && text.contains("[background] workers"),
        "{text}"
    );
    runtime.plugins().shutdown().await;
}

/// A plugin's own messages, by unqualified key, in the language of the
/// player who sees them: a message to one player, a broadcast to two, and a
/// decision.
#[tokio::test]
async fn messages_render_in_the_language_of_each_recipient() {
    crate::text::catalog::install_test_catalog();
    let root = folder(
        "[deadlines]\ncommand = 5000\nlogin = 5000\n",
        &[(
            "init.lua",
            r#"
rift.command("greet", { permission = "", run = function(ctx)
    local sent = rift.players.message(ctx.args, { message = "greet", args = { name = ctx.name } })
    return { message = tostring(sent) }
end })
rift.command("all", { permission = "", run = function(ctx)
    return { message = tostring(rift.players.broadcast({ message = "greet", args = { name = "all" } })) }
end })
rift.command("bad", { permission = "", run = function(ctx)
    local ok, err = pcall(rift.players.message, ctx.args, { message = "nope" })
    return { message = tostring(ok) .. " " .. tostring(err) }
end })
rift.on("login", function(ctx)
    return { deny = { message = "denied" } }
end)
"#,
        )],
    );
    let runtime = started(&root, "{}", &Broker::default()).await;
    let players = runtime.players();
    let (alex_handle, mut alex_queue) = players.register(7, &[7; 16], "Alex", 774, "lobby");
    alex_handle.set_locale(Some("de_DE"));
    let (_steve_handle, mut steve_queue) = players.register(8, &[8; 16], "Steve", 774, "lobby");
    let words = |queue: &mut tokio::sync::mpsc::Receiver<crate::extensions::PlayerAction>| {
        std::iter::from_fn(|| queue.try_recv().ok())
            .map(|action| match action {
                crate::extensions::PlayerAction::Message(text) => text.to_string(),
                other => panic!("{other:?}"),
            })
            .collect::<Vec<_>>()
    };
    let message = |text: &str| Action::Message(text.into());
    let alex = session(&runtime, 7, "Alex");
    assert_eq!(alex.command("greet Alex").await.unwrap(), message("true"));
    assert_eq!(alex.command("greet Steve").await.unwrap(), message("true"));
    assert_eq!(words(&mut alex_queue), ["Hallo Alex."]);
    assert_eq!(words(&mut steve_queue), ["Hello Alex."]);
    // One call, each player in their own language.
    assert_eq!(alex.command("all").await.unwrap(), message("2"));
    assert_eq!(words(&mut alex_queue), ["Hallo all."]);
    assert_eq!(words(&mut steve_queue), ["Hello all."]);
    // A bad call fails at the call.
    let Action::Message(failed) = alex.command("bad Steve").await.unwrap() else {
        panic!("expected a message");
    };
    let failed = failed.to_string();
    assert!(
        failed.starts_with("false ") && failed.contains("plugin t has no such message"),
        "{failed}"
    );
    assert!(words(&mut steve_queue).is_empty());
    // A decision is in the language of the player it is about.
    let denied = |language: Option<&str>| {
        let mut player = session(&runtime, 9, "Blocked");
        player.context.language = language.map(str::to_owned);
        async move {
            match player.decision("login").await.unwrap() {
                Action::Deny(text) => text.to_string(),
                other => panic!("{other:?}"),
            }
        }
    };
    assert_eq!(denied(Some("de_at")).await, "Kein Zutritt.");
    assert_eq!(denied(None).await, "No entry.");
    runtime.plugins().shutdown().await;
}

/// `rift.text` in a persistent plugin resolves the plugin's own messages by
/// unqualified key, in the language asked for, in `init`, in hooks and in
/// background workers alike.
#[tokio::test]
async fn rift_text_resolves_the_plugins_own_messages() {
    crate::text::catalog::install_test_catalog();
    let root = folder(
        "[background]\nworkers = 1\n[deadlines]\ncommand = 5000\n",
        &[
            (
                "init.lua",
                r#"
local greet = rift.text.message("greet", { name = "init" })
assert(greet.message == "greet" and greet.args.name == "init")
-- In init, outside any hook.
local at_init = rift.text.render(greet, "de")
assert(rift.text.plain(at_init) == "Hallo init.", rift.text.plain(at_init))
assert(rift.text.plain(greet) == "Hello init.")
assert(not pcall(rift.text.render, rift.text.message("nope")))
assert(not pcall(rift.text.plain, greet, "not a language"))
rift.command("words", { permission = "", run = function(ctx)
    local language = ctx.args ~= "" and ctx.args or nil
    return { message = rift.text.plain(rift.text.message("created"), language) .. " "
        .. rift.text.to_mini({ mini = "<red>x" }) }
end })
rift.command("slow", { permission = "", run = function(ctx)
    local ticket = rift.background.defer()
    rift.background.run(ctx.args, function(result, err)
        rift.background.resolve(ticket, { message = result or ("failed: " .. err) })
    end)
    return { message = "ignored" }
end })
"#,
            ),
            (
                "worker.lua",
                r#"
rift.on("work", function(language)
    return rift.text.plain({ message = "denied" }, language)
end)
"#,
            ),
        ],
    );
    let runtime = started(&root, "{}", &Broker::default()).await;
    let message = |text: &str| Action::Message(text.into());
    let alex = session(&runtime, 7, "Alex");
    assert_eq!(
        alex.command("words de").await.unwrap(),
        message("Erstellt. <red>x</red>")
    );
    // Without a language: the default, whatever the hook's player reads.
    let mut german = session(&runtime, 8, "Jonas");
    german.context.language = Some("de".into());
    assert_eq!(
        german.command("words").await.unwrap(),
        message("Created. <red>x</red>")
    );
    assert_eq!(
        alex.command("slow de").await.unwrap(),
        message("Kein Zutritt.")
    );
    runtime.plugins().shutdown().await;
}
