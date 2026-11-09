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
                "name = \"{name}\"\nversion = \"1\"\napi = \"0.1\"\nengine = \"lua\"\n{extra}\n[lua]\nmemory_limit_mb = 64\n"
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
        format!("name = \"t\"\nversion = \"1\"\napi = \"0.1\"\nengine = \"lua\"\n{extra}"),
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
            Action::Deny("Blocked (api 2)".into())
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
                .map(|suggestion| (suggestion.text.as_str(), suggestion.tooltip.as_deref()))
                .collect::<Vec<_>>(),
            [("hello", None), ("hey", Some("he"))]
        );
        assert_eq!(
            session(runtime, 3, "Other").command("greet").await.unwrap(),
            Action::Message("You do not have permission to use this command.".into())
        );
    }
    plugin.plugins().shutdown().await;
}
