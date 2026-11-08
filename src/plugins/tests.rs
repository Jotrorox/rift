//! The chain, deadlines, failures, restarts, the reload diff and command
//! ownership, exercised with an in-process engine instead of Lua or WASM.
use super::{
    api::{CommandSpec, Registration, SuggestKind},
    manifest::{EngineKind, Manifest},
    worker::{CallError, Engine, Engines, Status},
    *,
};
use crate::{
    auth::AuthenticatedProfile,
    config::Config,
    extensions::{ExtensionSession, Extensions},
    messaging::Broker,
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};
use tokio::time::timeout;

type Handler = Arc<dyn Fn(&Event, &services::Services) -> Result<Reply, CallError> + Send + Sync>;
type Init = Arc<dyn Fn(&str, &services::Services) -> Result<(), String> + Send + Sync>;

/// How one mock plugin behaves; looked up by plugin name.
#[derive(Clone)]
struct Plan {
    registration: Registration,
    init: Init,
    handler: Handler,
    /// How long `reload` and `shutdown` take.
    reload_delay: Duration,
    shutdown_delay: Duration,
}

impl Plan {
    fn new(
        handler: impl Fn(&Event, &services::Services) -> Result<Reply, CallError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            registration: Registration::default(),
            init: Arc::new(|_, _| Ok(())),
            handler: Arc::new(handler),
            reload_delay: Duration::ZERO,
            shutdown_delay: Duration::ZERO,
        }
    }

    fn hooks(mut self, hooks: &[Hook]) -> Self {
        self.registration.hooks = hooks.to_vec();
        self
    }

    /// `init` sleeps this long before registering.
    fn slow_init(mut self, delay: Duration) -> Self {
        self.init = Arc::new(move |_, _| {
            std::thread::sleep(delay);
            Ok(())
        });
        self
    }

    fn commands(mut self, names: &[&str]) -> Self {
        self.registration.commands = names
            .iter()
            .map(|name| CommandSpec {
                name: (*name).into(),
                permission: "test.command".into(),
                suggest: SuggestKind::Fixed(vec!["alpha".into(), "beta".into()]),
            })
            .collect();
        self
    }
}

#[derive(Clone, Default)]
struct Mocks {
    plans: Arc<Mutex<BTreeMap<String, Plan>>>,
    log: Arc<Mutex<Vec<String>>>,
    instances: Arc<AtomicUsize>,
}

impl Mocks {
    fn plan(&self, name: &str, plan: Plan) {
        self.plans.lock().unwrap().insert(name.into(), plan);
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// Entries ending in `:<suffix>`, in call order.
    fn calls(&self, suffix: &str) -> Vec<String> {
        self.log()
            .into_iter()
            .filter(|entry| entry.ends_with(&format!(":{suffix}")))
            .collect()
    }

    fn engines(&self) -> Engines {
        let mocks = self.clone();
        Engines::default().with(
            EngineKind::Lua,
            move |spec: &PluginSpec,
                  services: services::Services|
                  -> Result<Box<dyn Engine>, String> {
                let plan = mocks
                    .plans
                    .lock()
                    .unwrap()
                    .get(&spec.name)
                    .cloned()
                    .ok_or("no plan")?;
                mocks.instances.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Mock {
                    name: spec.name.clone(),
                    plan,
                    services,
                    log: mocks.log.clone(),
                }))
            },
        )
    }
}

struct Mock {
    name: String,
    plan: Plan,
    services: services::Services,
    log: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    fn record(&self, entry: String) {
        self.log
            .lock()
            .unwrap()
            .push(format!("{}:{entry}", self.name));
    }
}

impl Engine for Mock {
    fn init(&mut self, config_json: &str) -> Result<Registration, String> {
        self.record(format!("init {config_json}"));
        (self.plan.init)(config_json, &self.services)?;
        Ok(self.plan.registration.clone())
    }

    fn call(&mut self, event: Event) -> Result<Reply, CallError> {
        self.record(event.hook().name().into());
        (self.plan.handler)(&event, &self.services)
    }

    fn reload(&mut self, config_json: &str) -> Result<(), String> {
        std::thread::sleep(self.plan.reload_delay);
        self.record(format!("reload {config_json}"));
        Ok(())
    }

    fn shutdown(&mut self, reason: &str) {
        self.record(format!("shutdown {reason}"));
        if !self.plan.shutdown_delay.is_zero() {
            std::thread::sleep(self.plan.shutdown_delay);
            self.record("shutdown done".into());
        }
    }
}

fn directory() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "rift-plugin-tests-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn spec(root: &Path, name: &str, extra: &str, config_json: &str) -> PluginSpec {
    let text =
        format!("name = \"{name}\"\nversion = \"1\"\napi = \"0.1\"\nengine = \"lua\"\n{extra}");
    PluginSpec {
        name: name.into(),
        folder: root.join("plugins").join(name),
        config_root: root.to_owned(),
        manifest: Manifest::parse(&text, name).unwrap(),
        config_json: config_json.into(),
        fingerprint: "one".into(),
        online_mode: true,
    }
}

use std::path::Path;

const BASE: &str = "listeners = { public = '127.0.0.1:0' }, backends = { lobby = '127.0.0.1:25001', game = '127.0.0.1:25002' }, routes = { public = 'lobby' }, authentication = { online_mode = true }, forwarding = { mode = 'velocity', secret_env = 'TEST_SECRET' }";

fn config(extra: &str, plugins: Vec<PluginSpec>) -> Config {
    let mut config =
        Config::from_lua(&format!("return {{ {BASE}, {extra} }}"), "plugins.lua").unwrap();
    config.plugins = plugins;
    config
}

async fn runtime(config: &Config, mocks: &Mocks, broker: &Broker) -> Extensions {
    let runtime = Extensions::with_engines(config, broker.clone(), None, mocks.engines()).unwrap();
    runtime.plugins().start().await.unwrap();
    runtime
}

fn session(runtime: &Extensions, id: u64) -> ExtensionSession {
    let mut context = crate::extensions::Context::authenticated(
        id,
        &AuthenticatedProfile {
            uuid: [id as u8; 16],
            name: format!("Player{id}"),
            properties: vec![],
        },
    );
    context.default_server = Some("lobby".into());
    runtime.session(context).unwrap()
}

fn action(action: Action) -> impl Fn(&Event, &services::Services) -> Result<Reply, CallError> {
    move |event, _| match event {
        Event::Login(_) | Event::InitialServer(_) | Event::BeforeTransfer(_) => {
            Ok(Reply::Action(action.clone()))
        }
        event => Ok(Reply::default_for(event.hook())),
    }
}

#[tokio::test]
async fn decisions_run_script_then_plugins_in_order_then_remote() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan("first", Plan::new(action(Action::Continue)));
    mocks.plan(
        "second",
        Plan::new(action(Action::Deny("second says no".into()))),
    );
    mocks.plan(
        "third",
        Plan::new(action(Action::Deny("never asked".into()))),
    );
    let broker = Broker::default();
    let plugins = vec![
        spec(&root, "first", "", "{}"),
        spec(&root, "second", "", "{}"),
        spec(&root, "third", "", "{}"),
    ];
    // The script continues for Player1 and denies Player2 itself.
    let extension = "extensions = { api_version = 2, login = function(ctx) if ctx.name == 'Player2' then return { deny = 'script' } end end }";
    let runtime = runtime(&config(extension, plugins), &mocks, &broker).await;
    assert_eq!(
        session(&runtime, 1).decision("login").await.unwrap(),
        Action::Deny("second says no".into())
    );
    assert_eq!(mocks.calls("login"), ["first:login", "second:login"]);
    assert_eq!(
        session(&runtime, 2).decision("login").await.unwrap(),
        Action::Deny("script".into())
    );
    assert_eq!(
        mocks.calls("login").len(),
        2,
        "a script deny stops the chain"
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn remote_responders_answer_after_persistent_plugins_with_the_same_wire_format() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan("first", Plan::new(action(Action::Continue)));
    let broker = Broker::default();
    let mut requests = broker.subscribe("bans.login", None).unwrap();
    let responder = broker.clone();
    let seen = tokio::spawn(async move {
        let message = requests.recv().await.unwrap();
        responder
            .publish(
                message.reply.as_deref().unwrap(),
                bytes::Bytes::from_static(br#"{"action":"deny","reason":"banned"}"#),
            )
            .unwrap();
        message.payload
    });
    let extension =
        "extensions = { api_version = 2, decisions = { login = { subject = 'bans.login' } } }";
    let runtime = runtime(
        &config(extension, vec![spec(&root, "first", "", "{}")]),
        &mocks,
        &broker,
    )
    .await;
    assert_eq!(
        session(&runtime, 7).decision("login").await.unwrap(),
        Action::Deny("banned".into())
    );
    // Byte for byte what extensions.decisions has always sent.
    let expected = format!(
        r#"{{"connection_id":"7","default_server":"lobby","hook":"login","hostname":"","name":"Player7","peer_ip":"","protocol":0,"reason":null,"server":null,"target":null,"uuid":"{}"}}"#,
        "07".repeat(16)
    );
    assert_eq!(std::str::from_utf8(&seen.await.unwrap()).unwrap(), expected);
    assert_eq!(mocks.calls("login"), ["first:login"]);
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn first_answer_other_than_continue_wins() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan("first", Plan::new(action(Action::Server("game".into()))));
    mocks.plan("second", Plan::new(action(Action::Deny("late".into()))));
    let broker = Broker::default();
    let runtime = runtime(
        &config(
            "",
            vec![
                spec(&root, "first", "", "{}"),
                spec(&root, "second", "", "{}"),
            ],
        ),
        &mocks,
        &broker,
    )
    .await;
    assert_eq!(
        session(&runtime, 1)
            .decision("initial_server")
            .await
            .unwrap(),
        Action::Server("game".into())
    );
    assert!(!mocks.log().contains(&"second:initial_server".to_owned()));
    // A server answer is not valid for login: on_failure (deny) applies.
    assert!(session(&runtime, 2).decision("login").await.is_err());
    runtime.plugins().shutdown().await;
}

fn slow(event: &Event, _: &services::Services) -> Result<Reply, CallError> {
    if matches!(event, Event::Login(_)) {
        std::thread::sleep(Duration::from_millis(300));
    }
    Ok(Reply::default_for(event.hook()))
}

#[tokio::test]
async fn deadlines_apply_on_timeout_and_a_slow_plugin_stalls_only_itself() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan("slow", Plan::new(slow));
    mocks.plan("lenient", Plan::new(slow));
    mocks.plan("fast", Plan::new(action(Action::Deny("fast".into()))));
    let broker = Broker::default();
    let strict = runtime(
        &config(
            "",
            vec![spec(&root, "slow", "[deadlines]\nlogin = 20\n", "{}")],
        ),
        &mocks,
        &broker,
    )
    .await;
    let started = Instant::now();
    let error = session(&strict, 1).decision("login").await.unwrap_err();
    assert!(error.to_string().contains("exceeded 20 ms"), "{error}");
    assert!(started.elapsed() < Duration::from_millis(250));
    strict.plugins().shutdown().await;

    let skipping = runtime(
        &config(
            "",
            vec![
                spec(
                    &root,
                    "lenient",
                    "[deadlines]\nlogin = 20\n[failure]\non_timeout = \"continue\"\n",
                    "{}",
                ),
                spec(&root, "fast", "", "{}"),
            ],
        ),
        &mocks,
        &broker,
    )
    .await;
    assert_eq!(
        session(&skipping, 2).decision("login").await.unwrap(),
        Action::Deny("fast".into())
    );
    skipping.plugins().shutdown().await;
}

fn failing(event: &Event, _: &services::Services) -> Result<Reply, CallError> {
    match event {
        Event::Login(_) => Err(CallError::Handler("database down".into())),
        Event::BeforeTransfer(_) => Err(CallError::Fatal("trap".into())),
        event => Ok(Reply::default_for(event.hook())),
    }
}

#[tokio::test]
async fn failures_apply_on_failure_and_traps_restart_with_backoff() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan("flaky", Plan::new(failing));
    mocks.plan("tolerant", Plan::new(failing));
    mocks.plan("once", Plan::new(failing));
    let broker = Broker::default();
    let mut events = broker.subscribe(worker::EVENTS, None).unwrap();
    let runtime = runtime(
        &config(
            "",
            vec![
                spec(&root, "flaky", "", "{}"),
                spec(
                    &root,
                    "tolerant",
                    "[failure]\non_failure = \"continue\"\n",
                    "{}",
                ),
                spec(
                    &root,
                    "once",
                    "[failure]\nrestart = \"never\"\non_failure = \"continue\"\n",
                    "{}",
                ),
            ],
        ),
        &mocks,
        &broker,
    )
    .await;
    let error = session(&runtime, 1).decision("login").await.unwrap_err();
    assert!(error.to_string().contains("database down"), "{error}");
    let active = runtime.plugins().active();
    assert_eq!(
        active[0].status(),
        Status::Running,
        "a handler error is not fatal"
    );
    // A trap fails the plugin; while failed, its decisions deny.
    assert!(
        session(&runtime, 2)
            .before_transfer("game", "command")
            .await
            .is_err()
    );
    assert_eq!(active[0].status(), Status::Failed);
    assert!(session(&runtime, 3).decision("login").await.is_err());
    // on-failure restarts after the 1 s backoff; never stays failed.
    timeout(Duration::from_secs(5), async {
        while active[0].status() != Status::Running {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(active[0].report().restarts, 1);
    assert_eq!(
        mocks
            .log()
            .iter()
            .filter(|entry| *entry == "flaky:init {}")
            .count(),
        2
    );
    let mut seen = Vec::new();
    while let Ok(Some(message)) = events.try_recv() {
        let value: serde_json::Value = serde_json::from_slice(&message.payload).unwrap();
        seen.push(format!(
            "{}:{}",
            value["plugin"].as_str().unwrap(),
            value["event"].as_str().unwrap()
        ));
    }
    assert!(seen.contains(&"flaky:failed".to_owned()), "{seen:?}");
    assert!(seen.contains(&"flaky:restarting".to_owned()), "{seen:?}");
    // The first trap denied the transfer, so `once` was never asked; fail
    // it directly. With restart = "never" it stays failed.
    let failed = active[2]
        .call(
            Event::BeforeTransfer(Default::default()),
            Duration::from_secs(1),
        )
        .await;
    assert!(matches!(failed, Outcome::Failed(_)));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(active[2].status(), Status::Failed);
    runtime.plugins().shutdown().await;
    assert!(
        active
            .iter()
            .all(|instance| instance.status() == Status::Stopped)
    );
}

#[tokio::test]
async fn reload_diff_keeps_reloads_or_replaces_instances() {
    let root = directory();
    let mocks = Mocks::default();
    for name in ["same", "hot", "cold", "code", "gone"] {
        mocks.plan(name, Plan::new(action(Action::Continue)));
    }
    let broker = Broker::default();
    let first = config(
        "",
        vec![
            spec(&root, "same", "", "{}"),
            spec(&root, "hot", "hot_reload = true\n", "{\"a\":1}"),
            spec(&root, "cold", "", "{\"a\":1}"),
            spec(&root, "code", "", "{}"),
            spec(&root, "gone", "", "{}"),
        ],
    );
    let old = runtime(&first, &mocks, &broker).await;
    let before = old.plugins().active();
    let mut changed_code = spec(&root, "code", "", "{}");
    changed_code.fingerprint = "two".into();
    let second = config(
        "",
        vec![
            spec(&root, "new", "", "{}"),
            spec(&root, "same", "", "{}"),
            spec(&root, "hot", "hot_reload = true\n", "{\"a\":2}"),
            spec(&root, "cold", "", "{\"a\":2}"),
            changed_code,
        ],
    );
    mocks.plan("new", Plan::new(action(Action::Continue)));
    let next =
        Extensions::with_engines(&second, broker.clone(), Some(&old), Engines::default()).unwrap();
    next.plugins().activate().unwrap().await;
    let after = next.plugins().active();
    let names: Vec<_> = after
        .iter()
        .map(|instance| instance.name().to_owned())
        .collect();
    assert_eq!(names, ["new", "same", "hot", "cold", "code"]);
    assert!(Arc::ptr_eq(&after[1], &before[0]), "unchanged");
    assert!(Arc::ptr_eq(&after[2], &before[1]), "hot reloaded in place");
    assert!(!Arc::ptr_eq(&after[3], &before[2]), "replaced");
    assert!(!Arc::ptr_eq(&after[4], &before[3]), "replaced");
    for instance in after.iter() {
        instance.ready(Duration::from_secs(1)).await.unwrap();
    }
    assert_eq!(after[2].config_json(), "{\"a\":2}");
    let log = mocks.log();
    for expected in [
        "hot:reload {\"a\":2}",
        "cold:shutdown stop",
        "cold:init {\"a\":2}",
        "code:shutdown stop",
        "gone:shutdown stop",
        "new:init {}",
    ] {
        assert!(log.contains(&expected.to_owned()), "{expected} in {log:?}");
    }
    assert!(!log.contains(&"same:shutdown stop".to_owned()));
    // Old sessions see the new set: the active list is shared.
    assert_eq!(old.plugins().active().len(), 5);
    assert_eq!(before[4].status(), Status::Stopped);
    next.plugins().shutdown().await;
}

#[tokio::test]
async fn command_names_have_exactly_one_owner() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan("a", Plan::new(action(Action::Continue)).commands(&["ban"]));
    mocks.plan("b", Plan::new(action(Action::Continue)).commands(&["ban"]));
    mocks.plan(
        "c",
        Plan::new(action(Action::Continue)).commands(&["hello"]),
    );
    let broker = Broker::default();
    let colliding = config(
        "",
        vec![spec(&root, "a", "", "{}"), spec(&root, "b", "", "{}")],
    );
    let runtime =
        Extensions::with_engines(&colliding, broker.clone(), None, mocks.engines()).unwrap();
    let error = runtime.plugins().start().await.unwrap_err();
    assert!(
        error.to_string().contains("already registered by plugin a"),
        "{error}"
    );
    assert!(
        runtime.plugins().active().is_empty(),
        "a failed start stops everything"
    );

    let script = "extensions = { api_version = 2, commands = { hello = { permission = 'x', run = function() end } } }";
    let runtime = Extensions::with_engines(
        &config(script, vec![spec(&root, "c", "", "{}")]),
        broker,
        None,
        mocks.engines(),
    )
    .unwrap();
    let error = runtime.plugins().start().await.unwrap_err();
    assert!(
        error.to_string().contains("configuration script"),
        "{error}"
    );
}

#[tokio::test]
async fn plugin_commands_check_permissions_and_suggest() {
    let root = directory();
    let mocks = Mocks::default();
    let mut plan = Plan::new(|event, _| match event {
        Event::Command(ctx) => Ok(Reply::Action(Action::Message(format!("hi {}", ctx.args)))),
        event => Ok(Reply::default_for(event.hook())),
    })
    .commands(&["hello"]);
    let allowed = "01".repeat(16);
    plan.init = Arc::new(move |_, services| {
        services
            .permissions()
            .set(&allowed, "test.command", Some(true))
    });
    mocks.plan("greeter", plan);
    let broker = Broker::default();
    let runtime = runtime(
        &config("", vec![spec(&root, "greeter", "", "{}")]),
        &mocks,
        &broker,
    )
    .await;
    let player = session(&runtime, 1);
    assert_eq!(player.commands(), ["hello"]);
    assert_eq!(player.visible_commands(), [("hello".to_owned(), true)]);
    assert_eq!(
        player.command("hello  world").await.unwrap(),
        Action::Message("hi world".into())
    );
    let suggestions = player.suggest("hello", "al", Vec::new).await.unwrap();
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].text, "alpha");
    let stranger = session(&runtime, 2);
    assert!(stranger.visible_commands().is_empty());
    assert_eq!(
        stranger.command("hello").await.unwrap(),
        Action::Message("You do not have permission to use this command.".into())
    );
    assert_eq!(
        mocks
            .log()
            .iter()
            .filter(|entry| *entry == "greeter:command")
            .count(),
        1,
        "denied commands never reach the plugin"
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn observers_reach_every_plugin_and_sessions_exist_without_extensions() {
    let root = directory();
    let mocks = Mocks::default();
    let (seen, mut joins) = tokio::sync::mpsc::unbounded_channel();
    for name in ["a", "b"] {
        let seen = seen.clone();
        mocks.plan(
            name,
            Plan::new(move |event, services| {
                if matches!(event, Event::Join(_) | Event::Disconnect(_)) {
                    let _ = seen.send(format!(
                        "{}:{}",
                        services.meta().name(),
                        event.hook().name()
                    ));
                }
                Ok(Reply::default_for(event.hook()))
            }),
        );
    }
    let broker = Broker::default();
    let runtime = runtime(
        &config(
            "",
            vec![spec(&root, "a", "", "{}"), spec(&root, "b", "", "{}")],
        ),
        &mocks,
        &broker,
    )
    .await;
    let mut player = session(&runtime, 1);
    player.ready("lobby").await;
    drop(player);
    let mut received = Vec::new();
    for _ in 0..4 {
        received.push(
            timeout(Duration::from_secs(2), joins.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    received.sort();
    assert_eq!(
        received,
        ["a:disconnect", "a:join", "b:disconnect", "b:join"]
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn services_scope_the_store_and_deliver_timers_and_messages() {
    let root = directory();
    let mocks = Mocks::default();
    let (seen, mut events) = tokio::sync::mpsc::unbounded_channel();
    let mut plan = Plan::new(move |event, services| {
        match event {
            Event::Timer(handle) => {
                services.timers().cancel(*handle);
                let _ = seen.send(format!(
                    "timer {}",
                    services
                        .store()
                        .get("counters", "hits")
                        .unwrap()
                        .map(|v| String::from_utf8(v).unwrap())
                        .unwrap_or_default()
                ));
            }
            Event::Message(message) => {
                let _ = seen.send(format!(
                    "message {}",
                    String::from_utf8_lossy(&message.payload)
                ));
            }
            _ => {}
        }
        Ok(Reply::default_for(event.hook()))
    });
    plan.init = Arc::new(|_, services| {
        services.store().increment("counters", "hits", 2)?;
        assert_eq!(services.store().list("counters", "h")?.len(), 1);
        services.timers().every(10);
        services.bus().subscribe("plugin.inbox", None)?;
        assert!(services.store().get(&"x".repeat(128), "k").is_err());
        Ok(())
    });
    mocks.plan("svc", plan);
    let broker = Broker::default();
    let runtime = runtime(
        &config("", vec![spec(&root, "svc", "", "{}")]),
        &mocks,
        &broker,
    )
    .await;
    assert_eq!(
        timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap(),
        "timer 2"
    );
    broker
        .publish("plugin.inbox", bytes::Bytes::from_static(b"hello"))
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap(),
        "message hello"
    );
    // The store entry lives under the plugin's own namespace.
    let raw = runtime
        .plugins()
        .persistent
        .base
        .store
        .namespace("plugin.svc.counters");
    assert_eq!(raw, [("hits".to_owned(), b"2".to_vec())]);
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn route_status_and_http_follow_the_script() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan(
        "edge",
        Plan::new(|event, _| match event {
            Event::Route(connection) if connection.peer_ip == "203.0.113.9" => {
                Ok(Reply::Route(api::RouteDecision::Reject {
                    reason: Some("blocked".into()),
                }))
            }
            Event::Status(_) => Ok(Reply::Status(Some(api::StatusOverride {
                motd_json: Some("\"Plugin MOTD\"".into()),
                ..Default::default()
            }))),
            Event::Http(request) if request.path == "/ext/hello" => {
                Ok(Reply::Http(Some(api::HttpResponse {
                    status: 200,
                    content_type: "text/plain".into(),
                    body: "hi".into(),
                })))
            }
            event => Ok(Reply::default_for(event.hook())),
        }),
    );
    let broker = Broker::default();
    let config = config("", vec![spec(&root, "edge", "", "{}")]);
    let runtime = runtime(&config, &mocks, &broker).await;
    let router = crate::hooks::Router::new(&config).with_plugins(runtime.plugins().clone());
    let connection = |ip: &str| crate::hooks::ConnectionInfo {
        listener: "public".into(),
        peer_addr: format!("{ip}:5000").parse().unwrap(),
        local_addr: "127.0.0.1:25565".parse().unwrap(),
        default_backend: Some("lobby".into()),
    };
    assert_eq!(
        router.route(connection("203.0.113.9")).await.unwrap(),
        api::RouteDecision::Reject {
            reason: Some("blocked".into())
        }
    );
    assert_eq!(
        router.route(connection("198.51.100.1")).await.unwrap(),
        api::RouteDecision::Default
    );
    assert!(router.has_status_hook());
    let ping = crate::server_list::Ping {
        listener: "public".into(),
        hostname: "play.example".into(),
        protocol: 774,
        peer_addr: "198.51.100.1:5000".parse().unwrap(),
        backend: Some("lobby".into()),
        maintenance: false,
        online: 0,
        max_players: 20,
    };
    let changes = router.status(ping).await.unwrap().unwrap();
    assert_eq!(changes.motd, Some(serde_json::json!("Plugin MOTD")));
    let request = |path: &str| crate::http_script::HttpRequest {
        method: "GET".into(),
        path: path.into(),
        query: String::new(),
        body: String::new(),
        headers: BTreeMap::new(),
        context: serde_json::json!({}),
    };
    assert_eq!(
        runtime
            .plugins()
            .http(&request("/ext/hello"))
            .await
            .unwrap()
            .unwrap()
            .body,
        "hi"
    );
    assert_eq!(
        runtime
            .plugins()
            .http(&request("/ext/other"))
            .await
            .unwrap(),
        None
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn unavailable_engines_are_refused_before_anything_starts() {
    let root = directory();
    let error = Extensions::with_engines(
        &config("", vec![spec(&root, "a", "", "{}")]),
        Broker::default(),
        None,
        Engines::default(),
    )
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("engine \"lua\" is not available"),
        "{error}"
    );
}

#[tokio::test]
async fn operator_restarts_replace_the_instance_and_are_counted_logged_and_announced() {
    use monitor::Outcome as Counted;
    let root = directory();
    let mocks = Mocks::default();
    let mut first = Plan::new(action(Action::Continue)).commands(&["hello"]);
    first.init = Arc::new(|_, services: &services::Services| {
        services.log(api::Level::Info, "init says hi\nforged");
        Ok(())
    });
    mocks.plan("first", first);
    mocks.plan("second", Plan::new(action(Action::Server("game".into()))));
    let broker = Broker::default();
    let mut events = broker.subscribe(worker::EVENTS, None).unwrap();
    let runtime = runtime(
        &config(
            "extensions = { api_version = 2, join = function() end }",
            vec![
                spec(&root, "first", "", "{\"a\":1}"),
                spec(&root, "second", "", "{}"),
            ],
        ),
        &mocks,
        &broker,
    )
    .await;
    let host = runtime.plugins().clone();
    let mut lifecycle = host.monitor().subscribe();
    // first continues; second answers login with an invalid server reply.
    assert!(session(&runtime, 1).decision("login").await.is_err());
    let statuses = host.statuses();
    let names: Vec<_> = statuses.iter().map(|status| status.name.as_str()).collect();
    assert_eq!(names, [SCRIPT, "first", "second"]);
    assert_eq!(statuses[0].mode, "script");
    assert!(!statuses[0].restartable);
    assert_eq!(statuses[1].mode, "persistent");
    assert_eq!(statuses[1].status, "running");
    assert_eq!(statuses[1].commands, ["hello"]);
    assert_eq!(statuses[1].restarts, 0);
    assert!(statuses[1].hooks.contains(&"login"));
    let first = host.monitor().counters("first");
    assert_eq!(first.calls[&(Hook::Login, Counted::Ok)], 1);
    assert_eq!(first.latency[&Hook::Login].count, 1);
    let second = host.monitor().counters("second");
    assert_eq!(second.calls[&(Hook::Login, Counted::Error)], 1);
    let logs = host.monitor().logs("second").unwrap();
    assert!(
        logs.iter()
            .any(|line| line.message.starts_with("login: invalid reply")
                && line.message.ends_with("; denied")),
        "{logs:?}"
    );
    let logs = host.monitor().logs("first").unwrap();
    assert!(
        logs.iter()
            .any(|line| line.message == "init says hi forged")
    );
    assert!(logs.iter().any(|line| line.message == "lifecycle: started"));

    let old = host.active()[0].clone();
    host.restart("first", "alice").unwrap().await;
    let active = host.active();
    assert!(!Arc::ptr_eq(&old, &active[0]));
    assert_eq!(active[0].name(), "first");
    assert_eq!(active[1].name(), "second");
    active[0].ready(Duration::from_secs(1)).await.unwrap();
    assert_eq!(old.status(), Status::Stopped);
    assert_eq!(active[0].config_json(), "{\"a\":1}");
    // The fresh instance initialized with the same configuration.
    assert_eq!(mocks.calls("init {\"a\":1}").len(), 2);
    assert!(matches!(
        host.command_owner("hello"),
        Some(CommandOwner::Plugin(instance, _)) if instance.name() == "first"
    ));
    assert_eq!(host.statuses()[1].restarts, 1);
    let mut seen = Vec::new();
    while seen.last().map(String::as_str) != Some("started") {
        let change = timeout(Duration::from_secs(1), lifecycle.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(change.plugin, "first");
        if change.event == "restart_requested" {
            assert_eq!(change.actor.as_deref(), Some("alice"));
        }
        seen.push(change.event);
    }
    assert_eq!(seen, ["restart_requested", "stopped", "started"]);
    let mut announced = false;
    while let Ok(Ok(message)) = timeout(Duration::from_millis(100), events.recv()).await {
        let body: serde_json::Value = serde_json::from_slice(&message.payload).unwrap();
        if body["event"] == "restart_requested" {
            assert_eq!(body["plugin"], "first");
            assert_eq!(body["actor"], "alice");
            announced = true;
        }
    }
    assert!(announced);
    let error = host.restart("ghost", "alice").err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let error = host.restart(SCRIPT, "alice").err().unwrap();
    assert!(
        error.to_string().contains("reload the configuration"),
        "{error}"
    );
    runtime.plugins().shutdown().await;
    assert!(host.monitor().logs("first").is_some());
}

/// Poll `done` every 10 ms for at most `limit`.
async fn eventually(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn http(event: &Event, _: &services::Services) -> Result<Reply, CallError> {
    match event {
        Event::Http(_) => Err(CallError::Fatal("trap".into())),
        event => Ok(Reply::default_for(event.hook())),
    }
}

#[tokio::test]
async fn a_failed_plugin_fails_only_the_hooks_it_registered() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan("web", Plan::new(http).hooks(&[Hook::Http, Hook::Timer]));
    let broker = Broker::default();
    let config = config(
        "",
        vec![spec(&root, "web", "[failure]\nrestart = \"never\"\n", "{}")],
    );
    let runtime = runtime(&config, &mocks, &broker).await;
    let request = crate::http_script::HttpRequest {
        method: "GET".into(),
        path: "/ext/web".into(),
        query: String::new(),
        body: String::new(),
        headers: BTreeMap::new(),
        context: serde_json::json!({}),
    };
    // The trap fails the plugin; on_failure (deny) applies to http.
    assert!(runtime.plugins().http(&request).await.is_err());
    let plugin = runtime.plugins().active()[0].clone();
    assert_eq!(plugin.status(), Status::Failed);
    assert!(runtime.plugins().http(&request).await.is_err());
    // It never registered login, routes or pings: those continue.
    assert_eq!(
        session(&runtime, 1).decision("login").await.unwrap(),
        Action::Continue
    );
    assert_eq!(
        session(&runtime, 2)
            .decision("initial_server")
            .await
            .unwrap(),
        Action::Continue
    );
    assert!(!runtime.plugins().may_handle(Hook::Route));
    let router = crate::hooks::Router::new(&config).with_plugins(runtime.plugins().clone());
    assert_eq!(
        router
            .route(crate::hooks::ConnectionInfo {
                listener: "public".into(),
                peer_addr: "198.51.100.1:5000".parse().unwrap(),
                local_addr: "127.0.0.1:25565".parse().unwrap(),
                default_backend: Some("lobby".into()),
            })
            .await
            .unwrap(),
        api::RouteDecision::Default
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn starting_plugins_are_asked_only_for_what_they_claimed() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan(
        "web",
        Plan::new(action(Action::Continue))
            .hooks(&[Hook::Http])
            .slow_init(Duration::from_millis(400)),
    );
    mocks.plan(
        "ban",
        Plan::new(action(Action::Deny("banned".into())))
            .hooks(&[Hook::Login])
            .slow_init(Duration::from_millis(300)),
    );
    let broker = Broker::default();
    let first = config("", vec![spec(&root, "web", "", "{}")]);
    let runtime = runtime(&first, &mocks, &broker).await;
    let host = runtime.plugins().clone();
    let router = crate::hooks::Router::new(&first).with_plugins(host.clone());
    let connection = || crate::hooks::ConnectionInfo {
        listener: "public".into(),
        peer_addr: "198.51.100.1:5000".parse().unwrap(),
        local_addr: "127.0.0.1:25565".parse().unwrap(),
        default_backend: Some("lobby".into()),
    };
    // Restarting an http-only plugin keeps its claims: routes and logins
    // are not held up behind its init.
    host.restart("web", "alice").unwrap().await;
    assert_eq!(host.active()[0].status(), Status::Starting);
    assert!(!host.may_handle(Hook::Route));
    assert!(host.may_handle(Hook::Http));
    let started = Instant::now();
    assert_eq!(
        router.route(connection()).await.unwrap(),
        api::RouteDecision::Default
    );
    assert_eq!(
        session(&runtime, 1).decision("login").await.unwrap(),
        Action::Continue
    );
    assert!(started.elapsed() < Duration::from_millis(200));
    host.active()[0]
        .ready(Duration::from_secs(1))
        .await
        .unwrap();

    // A plugin added on reload has claimed nothing until its init returns.
    let second = config(
        "",
        vec![spec(&root, "web", "", "{}"), spec(&root, "ban", "", "{}")],
    );
    let next =
        Extensions::with_engines(&second, broker.clone(), Some(&runtime), Engines::default())
            .unwrap();
    next.plugins().activate().unwrap().await;
    assert_eq!(next.plugins().active()[1].status(), Status::Starting);
    assert_eq!(
        session(&next, 2).decision("login").await.unwrap(),
        Action::Continue
    );
    next.plugins().active()[1]
        .ready(Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        session(&next, 3).decision("login").await.unwrap(),
        Action::Deny("banned".into())
    );
    // A replaced plugin keeps its claims: logins queue behind the new init.
    let mut changed = spec(&root, "ban", "", "{}");
    changed.fingerprint = "two".into();
    let third = config("", vec![spec(&root, "web", "", "{}"), changed]);
    let last =
        Extensions::with_engines(&third, broker.clone(), Some(&next), Engines::default()).unwrap();
    last.plugins().activate().unwrap().await;
    assert_eq!(last.plugins().active()[1].status(), Status::Starting);
    assert_eq!(
        session(&last, 4).decision("login").await.unwrap(),
        Action::Deny("banned".into())
    );
    last.plugins().shutdown().await;
}

#[tokio::test]
async fn a_stuck_predecessor_is_abandoned_and_later_plugins_start() {
    let root = directory();
    let mocks = Mocks::default();
    let mut stuck = Plan::new(action(Action::Continue)).commands(&["stuck"]);
    stuck.shutdown_delay = Duration::from_secs(3);
    mocks.plan("stuck", stuck);
    mocks.plan("other", Plan::new(action(Action::Continue)));
    let broker = Broker::default();
    let grace = "[deadlines]\nshutdown = 100\n";
    let first = config("", vec![spec(&root, "stuck", grace, "{}")]);
    let runtime = runtime(&first, &mocks, &broker).await;
    let old = runtime.plugins().active()[0].clone();
    let mut changed = spec(&root, "stuck", grace, "{}");
    changed.fingerprint = "two".into();
    let second = config("", vec![changed, spec(&root, "other", "", "{}")]);
    let next =
        Extensions::with_engines(&second, broker.clone(), Some(&runtime), Engines::default())
            .unwrap();
    let started = Instant::now();
    tokio::spawn(next.plugins().activate().unwrap());
    let active = next.plugins().active();
    // `other` initializes without waiting for `stuck`'s old thread...
    eventually("other's init", Duration::from_millis(600), || {
        mocks.log().contains(&"other:init {}".to_owned())
    })
    .await;
    // ...and both run once the old thread's grace and the margin passed,
    // although that thread is still busy. `other` claims its commands
    // after `stuck` did.
    active[0].ready(Duration::from_secs(3)).await.unwrap();
    active[1].ready(Duration::from_secs(1)).await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(2500));
    assert!(!old.has_exited());
    assert!(matches!(
        next.plugins().command_owner("stuck"),
        Some(CommandOwner::Plugin(instance, _)) if Arc::ptr_eq(&instance, &active[0])
    ));
    next.plugins().shutdown().await;
}

#[tokio::test]
async fn a_plugin_added_again_waits_for_its_stopping_generation() {
    let root = directory();
    let mocks = Mocks::default();
    let mut slow = Plan::new(action(Action::Continue)).commands(&["slow"]);
    slow.shutdown_delay = Duration::from_millis(500);
    mocks.plan("slow", slow);
    let broker = Broker::default();
    let with = config("", vec![spec(&root, "slow", "", "{}")]);
    let runtime = runtime(&with, &mocks, &broker).await;
    let old = runtime.plugins().active()[0].clone();
    // Removed: its shutdown takes a while.
    let without = Extensions::with_engines(
        &config("", vec![]),
        broker.clone(),
        Some(&runtime),
        Engines::default(),
    )
    .unwrap();
    tokio::spawn(without.plugins().activate().unwrap());
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Added again before the old generation exited.
    let again = Extensions::with_engines(&with, broker.clone(), Some(&without), Engines::default())
        .unwrap();
    again.plugins().activate().unwrap().await;
    let new = again.plugins().active()[0].clone();
    new.ready(Duration::from_secs(2)).await.unwrap();
    assert!(old.has_exited());
    let log = mocks.log();
    let done = log.iter().position(|entry| entry == "slow:shutdown done");
    let init = log.iter().rposition(|entry| entry == "slow:init {}");
    assert!(done.is_some() && done < init, "{log:?}");
    // The old generation's exit did not release the new one's command.
    let owner = again
        .plugins()
        .persistent
        .commands
        .0
        .lock()
        .unwrap()
        .plugins
        .get("slow")
        .map(|(_, name)| name.clone());
    assert_eq!(owner.as_deref(), Some("slow"));
    again.plugins().shutdown().await;
}

#[tokio::test]
async fn a_failed_activation_starts_nothing_and_keeps_the_active_set() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan("kept", Plan::new(action(Action::Continue)));
    mocks.plan("fresh", Plan::new(action(Action::Continue)));
    let broker = Broker::default();
    let runtime = runtime(
        &config(
            "extensions = { api_version = 2 }",
            vec![spec(&root, "kept", "", "{}")],
        ),
        &mocks,
        &broker,
    )
    .await;
    let before = runtime.plugins().active();
    let script = "extensions = { api_version = 2, commands = { hello = { permission = 'x', run = function() end } } }";
    // A spec without an engine cannot start; nothing of this activation may.
    let mut broken = spec(&root, "broken", "", "{}");
    broken.manifest.engine = None;
    let next = Extensions::with_engines(
        &config(script, vec![spec(&root, "fresh", "", "{}"), broken]),
        broker.clone(),
        Some(&runtime),
        Engines::default(),
    )
    .unwrap();
    assert!(next.plugins().activate().is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!mocks.log().iter().any(|entry| entry.starts_with("fresh:")));
    let after = runtime.plugins().active();
    assert_eq!(after.len(), 1);
    assert!(Arc::ptr_eq(&after[0], &before[0]));
    assert!(
        runtime
            .plugins()
            .persistent
            .commands
            .0
            .lock()
            .unwrap()
            .script
            .is_empty()
    );
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn an_operator_restart_uses_the_configured_table_during_a_hot_reload() {
    let root = directory();
    let mocks = Mocks::default();
    let mut plan = Plan::new(action(Action::Continue));
    plan.reload_delay = Duration::from_millis(300);
    mocks.plan("hot", plan);
    let broker = Broker::default();
    let manifest = "hot_reload = true\n";
    let runtime = runtime(
        &config("", vec![spec(&root, "hot", manifest, "{\"a\":1}")]),
        &mocks,
        &broker,
    )
    .await;
    let next = Extensions::with_engines(
        &config("", vec![spec(&root, "hot", manifest, "{\"a\":2}")]),
        broker.clone(),
        Some(&runtime),
        Engines::default(),
    )
    .unwrap();
    tokio::spawn(next.plugins().activate().unwrap());
    tokio::time::sleep(Duration::from_millis(50)).await;
    next.plugins().restart("hot", "alice").unwrap().await;
    next.plugins().active()[0]
        .ready(Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(mocks.calls("init {\"a\":2}"), ["hot:init {\"a\":2}"]);
    next.plugins().shutdown().await;
}

#[tokio::test]
async fn script_commands_may_not_take_a_replaced_plugins_command() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan(
        "greeter",
        Plan::new(action(Action::Continue)).commands(&["hello"]),
    );
    let broker = Broker::default();
    let runtime = runtime(
        &config(
            "extensions = { api_version = 2 }",
            vec![spec(&root, "greeter", "", "{}")],
        ),
        &mocks,
        &broker,
    )
    .await;
    let mut changed = spec(&root, "greeter", "", "{}");
    changed.fingerprint = "two".into();
    let script = "extensions = { api_version = 2, commands = { hello = { permission = 'x', run = function() end } } }";
    let error = Extensions::with_engines(
        &config(script, vec![changed]),
        broker.clone(),
        Some(&runtime),
        Engines::default(),
    )
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("registered by plugin greeter and by the configuration script"),
        "{error}"
    );
    assert_eq!(runtime.plugins().active()[0].status(), Status::Running);
    runtime.plugins().shutdown().await;
}

#[tokio::test]
async fn without_online_mode_plugins_that_register_player_hooks_fail_to_start() {
    let root = directory();
    let mocks = Mocks::default();
    mocks.plan(
        "ban",
        Plan::new(action(Action::Deny("banned".into()))).hooks(&[Hook::Login]),
    );
    mocks.plan(
        "greeter",
        Plan::new(action(Action::Continue))
            .hooks(&[Hook::Timer])
            .commands(&["hello"]),
    );
    mocks.plan("everything", Plan::new(action(Action::Continue)));
    mocks.plan(
        "web",
        Plan::new(action(Action::Continue)).hooks(&[Hook::Http, Hook::Timer]),
    );
    let offline = |plugins: Vec<PluginSpec>| {
        let mut config = Config::from_lua(
            "return { listeners = { public = '127.0.0.1:0' }, backends = { lobby = '127.0.0.1:25001' }, routes = { public = 'lobby' } }",
            "offline.lua",
        )
        .unwrap();
        config.plugins = plugins
            .into_iter()
            .map(|mut spec| {
                spec.online_mode = false;
                spec
            })
            .collect();
        config
    };
    let broker = Broker::default();
    for (name, expected) in [
        ("ban", "hook login needs authentication.online_mode"),
        ("greeter", "commands need"),
        ("everything", "an empty hook list"),
    ] {
        let config = offline(vec![spec(&root, name, "", "{}")]);
        let runtime =
            Extensions::with_engines(&config, broker.clone(), None, mocks.engines()).unwrap();
        let error = runtime.plugins().start().await.unwrap_err().to_string();
        assert!(error.contains(expected), "{name}: {error}");
    }
    // Plugins without player hooks run offline.
    let config = offline(vec![spec(&root, "web", "", "{}")]);
    let runtime = runtime(&config, &mocks, &broker).await;
    assert_eq!(runtime.plugins().active()[0].status(), Status::Running);
    runtime.plugins().shutdown().await;
}
