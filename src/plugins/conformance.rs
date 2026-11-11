//! Engine conformance: the chain scenarios of `tests.rs` run against real
//! engines. Each scenario is an async function over a [`Fixture`], so every
//! engine runs the same scenarios against its own build of one guest, the
//! *conformance guest*, whose behaviour its configuration selects.
//! [`conformance_tests!`] expands to one `#[tokio::test]` per scenario.
//!
//! # The conformance guest
//!
//! The guest reports what it does by publishing plain-text payloads on the
//! bus subject `conformance.<plugin name>`, so scenarios observe calls,
//! lifecycle and host-API effects the same way for every engine. Its
//! configuration (the `rift.plugin` table as JSON) holds these optional keys:
//!
//! | Key | Behaviour |
//! | --- | --- |
//! | `tag` (string) | `init` reports `init <tag>`; a hot reload adopts the new configuration and reports `reload <tag>` (`-` without a tag). `shutdown(reason)` reports `shutdown <reason>`. |
//! | `decide` | Object from `login`, `initial_server`, `before_transfer` (and `command`, which then answers this instead of `hi <args>`) to the action that hook returns, in the serde form of [`Action`]: `"continue"`, `{"deny": "..."}`, `{"server": "..."}`. A `deny` or `message` holding an object or a list is a JSON text component, returned through the `-json` form. Missing hooks continue. |
//! | `slow` | Hooks that busy-wait 300 ms (calling the host clock) before answering. |
//! | `fail` | Object from hook name to `"handler"` (the handler fails with an error containing `database down`) or `"fatal"` (a trap: the instance fails; the Lua guest exhausts its memory limit, the WebAssembly guest panics). WebAssembly hooks have no error result, so its "handler" failure answers a decision the hook rejects, carrying `database down`. |
//! | `commands` | List of `{name, permission, suggest}`; `suggest` is `"none"`, `"players"`, `"servers"`, `"dynamic"` or a list of strings. Every command answers `{"message": "hi <args>"}`; a dynamic `suggest` returns `alpha`, `beta`, `gamma`. |
//! | `grant` | List of `[uuid, node]` pairs granted with `permissions.set(uuid, node, true)` during init. |
//! | `increment` | Integer added to store entry `counters`/`hits` during init. |
//! | `timer_ms` | Starts a repeating timer during init. On its first tick the guest cancels it and reports `timer <counters/hits value>`. |
//! | `subscribe` | A subject subscribed during init; each message reports `message <payload>`. |
//! | `route_reject` | `{ip, reason}`: `route` rejects connections from `ip` with `reason`. |
//! | `status_motd` | `status` replaces the MOTD with this text. |
//! | `http` | `{path, body}`: `http` answers that path with status 200, `text/plain` and `body`. |
//! | `rich` | A JSON text component. `join` sends it to the joining player with `players.message-json`, `action-bar-json`, `broadcast-json` and `kick-json`, in that order, and reports `rich <message> <action bar> <broadcast> <kick>` (each result, or `error(<message>)`). |
//!
//! Every hook the guest handles reports its name (`login`, `command`, `join`,
//! ...) before acting. The guest registers route, status, http, login,
//! initial_server, before_transfer, join, after_transfer and disconnect,
//! plus command (and suggest for dynamic suggestions), timer and message
//! when configured; every other hook keeps the contract's default.
//!
//! The shared example plugins (`examples/plugins/hello-*`) run through
//! [`hello_example`] the same way.
use super::{
    PluginSpec,
    api::{Action, CommandSpec, Event, Hook, SuggestKind},
    manifest::Manifest,
    worker::{self, Engines, Outcome, Status},
};
use crate::{
    auth::AuthenticatedProfile,
    config::Config,
    extensions::{ExtensionSession, Extensions, PlayerAction},
    messaging::{Broker, Subscription},
    text::Text,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::time::timeout;

/// One engine's build of the conformance guest.
pub(crate) trait Fixture: Sync {
    /// The engine set the scenarios start plugins with.
    fn engines(&self) -> Engines;

    /// Write the conformance guest into `folder` (`<root>/plugins/<name>`,
    /// already created): a `plugin.toml` with `name`, `version = "1"`,
    /// `api = "0.1"`, this engine and its entry, then `extra` verbatim (TOML
    /// top-level keys such as `hot_reload` and `[deadlines]`/`[failure]`
    /// tables), then any tables of the engine's own, and the guest itself.
    fn install(&self, folder: &Path, name: &str, extra: &str);
}

fn directory() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "rift-conformance-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

const BASE: &str = "listeners = { public = '127.0.0.1:0' }, backends = { lobby = '127.0.0.1:25001', game = '127.0.0.1:25002' }, routes = { public = 'lobby' }, authentication = { online_mode = true }, forwarding = { mode = 'velocity', secret_env = 'TEST_SECRET' }";

fn config(extra: &str, plugins: Vec<PluginSpec>) -> Config {
    let mut config =
        Config::from_lua(&format!("return {{ {BASE}, {extra} }}"), "conformance.lua").unwrap();
    config.plugins = plugins;
    config
}

/// A spec for a plugin folder that already holds its `plugin.toml`.
pub(crate) fn installed(folder: &Path, config_json: &str) -> PluginSpec {
    let name = folder.file_name().unwrap().to_str().unwrap().to_owned();
    let text = std::fs::read_to_string(folder.join(super::manifest::FILE)).unwrap();
    PluginSpec {
        manifest: Manifest::parse(&text, &name).unwrap(),
        name,
        folder: folder.to_owned(),
        config_root: folder.parent().unwrap().parent().unwrap().to_owned(),
        config_json: config_json.into(),
        fingerprint: "one".into(),
        online_mode: true,
        signed_chat: true,
    }
}

fn uuid(id: u64) -> String {
    format!("{:02x}", id as u8).repeat(16)
}

fn session_named(runtime: &Extensions, id: u64, name: &str) -> ExtensionSession {
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

fn session(runtime: &Extensions, id: u64) -> ExtensionSession {
    session_named(runtime, id, &format!("Player{id}"))
}

/// A scenario's plugins folder, bus and the guests' reports.
struct Lab<'a> {
    fixture: &'a dyn Fixture,
    root: PathBuf,
    broker: Broker,
    reports: Subscription,
    seen: Vec<String>,
}

impl<'a> Lab<'a> {
    fn new(fixture: &'a dyn Fixture) -> Self {
        let broker = Broker::default();
        let reports = broker.subscribe("conformance.>", None).unwrap();
        Self {
            fixture,
            root: directory(),
            broker,
            reports,
            seen: Vec::new(),
        }
    }

    fn plugin(&self, name: &str, extra: &str, config: serde_json::Value) -> PluginSpec {
        let folder = self.root.join("plugins").join(name);
        std::fs::create_dir_all(&folder).unwrap();
        self.fixture.install(&folder, name, extra);
        installed(&folder, &config.to_string())
    }

    fn extensions(&self, config: &Config) -> std::io::Result<Extensions> {
        Extensions::with_engines(config, self.broker.clone(), None, self.fixture.engines())
    }

    async fn runtime(&self, config: &Config) -> Extensions {
        let runtime = self.extensions(config).unwrap();
        runtime.plugins().start().await.unwrap();
        runtime
    }

    fn record(&mut self, message: crate::messaging::Message) {
        let plugin = message.subject.strip_prefix("conformance.").unwrap_or("?");
        self.seen.push(format!(
            "{plugin}:{}",
            String::from_utf8_lossy(&message.payload)
        ));
    }

    /// Every report so far, in order.
    fn reports(&mut self) -> Vec<String> {
        while let Ok(Some(message)) = self.reports.try_recv() {
            self.record(message);
        }
        self.seen.clone()
    }

    /// Reports ending in `:<suffix>`.
    fn calls(&mut self, suffix: &str) -> Vec<String> {
        self.reports()
            .into_iter()
            .filter(|entry| entry.ends_with(&format!(":{suffix}")))
            .collect()
    }

    /// Wait until a report satisfies `found`, at most five seconds.
    async fn wait(&mut self, found: impl Fn(&str) -> bool) -> String {
        if let Some(entry) = self.reports().into_iter().find(|entry| found(entry)) {
            return entry;
        }
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let message = timeout(left, self.reports.recv())
                .await
                .unwrap_or_else(|_| panic!("no matching report in {:?}", self.seen))
                .unwrap();
            self.record(message);
            let last = self.seen.last().unwrap().clone();
            if found(&last) {
                return last;
            }
        }
    }
}

pub(crate) async fn decisions_follow_the_chain(fixture: &dyn Fixture) {
    let mut lab = Lab::new(fixture);
    let plugins = vec![
        lab.plugin("first", "", json!({"decide": {"login": "continue"}})),
        lab.plugin(
            "second",
            "",
            json!({"decide": {"login": {"deny": "second says no"}}}),
        ),
        lab.plugin(
            "third",
            "",
            json!({"decide": {"login": {"deny": "never asked"}}}),
        ),
    ];
    // The script continues for Player1 and denies Player2 itself.
    let extension = "extensions = { api_version = 2, login = function(ctx) if ctx.name == 'Player2' then return { deny = 'script' } end end }";
    let runtime = lab.runtime(&config(extension, plugins)).await;
    assert_eq!(
        session(&runtime, 1).decision("login").await.unwrap(),
        Action::Deny("second says no".into())
    );
    assert_eq!(lab.calls("login"), ["first:login", "second:login"]);
    assert_eq!(
        session(&runtime, 2).decision("login").await.unwrap(),
        Action::Deny("script".into())
    );
    assert_eq!(lab.calls("login").len(), 2, "a script deny stops the chain");
    runtime.plugins().shutdown().await;
}

pub(crate) async fn remote_responders_follow_plugins(fixture: &dyn Fixture) {
    let mut lab = Lab::new(fixture);
    let plugins = vec![lab.plugin("first", "", json!({}))];
    let mut requests = lab.broker.subscribe("bans.login", None).unwrap();
    let responder = lab.broker.clone();
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
    let runtime = lab.runtime(&config(extension, plugins)).await;
    assert_eq!(
        session(&runtime, 7).decision("login").await.unwrap(),
        Action::Deny("banned".into())
    );
    let expected = format!(
        r#"{{"connection_id":"7","default_server":"lobby","hook":"login","hostname":"","name":"Player7","peer_ip":"","protocol":0,"reason":null,"server":null,"target":null,"uuid":"{}"}}"#,
        uuid(7)
    );
    assert_eq!(std::str::from_utf8(&seen.await.unwrap()).unwrap(), expected);
    assert_eq!(lab.calls("login"), ["first:login"]);
    runtime.plugins().shutdown().await;
}

pub(crate) async fn first_answer_other_than_continue_wins(fixture: &dyn Fixture) {
    let mut lab = Lab::new(fixture);
    let plugins = vec![
        lab.plugin(
            "first",
            "",
            json!({"decide": {"initial_server": {"server": "game"}, "login": {"server": "game"}}}),
        ),
        lab.plugin(
            "second",
            "",
            json!({"decide": {"initial_server": {"deny": "late"}}}),
        ),
    ];
    let runtime = lab.runtime(&config("", plugins)).await;
    assert_eq!(
        session(&runtime, 1)
            .decision("initial_server")
            .await
            .unwrap(),
        Action::Server("game".into())
    );
    assert!(lab.calls("initial_server") == ["first:initial_server"]);
    // A server answer is not valid for login: on_failure (deny) applies.
    assert!(session(&runtime, 2).decision("login").await.is_err());
    runtime.plugins().shutdown().await;
}

pub(crate) async fn deadlines_apply_on_timeout(fixture: &dyn Fixture) {
    let lab = Lab::new(fixture);
    let strict = lab
        .runtime(&config(
            "",
            vec![lab.plugin(
                "slow",
                "[deadlines]\nlogin = 20\n",
                json!({"slow": ["login"]}),
            )],
        ))
        .await;
    let started = Instant::now();
    let error = session(&strict, 1).decision("login").await.unwrap_err();
    assert!(error.to_string().contains("exceeded 20 ms"), "{error}");
    assert!(started.elapsed() < Duration::from_millis(250));
    strict.plugins().shutdown().await;

    let skipping = lab
        .runtime(&config(
            "",
            vec![
                lab.plugin(
                    "lenient",
                    "[deadlines]\nlogin = 20\n[failure]\non_timeout = \"continue\"\n",
                    json!({"slow": ["login"]}),
                ),
                lab.plugin("fast", "", json!({"decide": {"login": {"deny": "fast"}}})),
            ],
        ))
        .await;
    assert_eq!(
        session(&skipping, 2).decision("login").await.unwrap(),
        Action::Deny("fast".into())
    );
    skipping.plugins().shutdown().await;
}

pub(crate) async fn failures_apply_on_failure_and_restart(fixture: &dyn Fixture) {
    let mut lab = Lab::new(fixture);
    let failing = json!({"tag": "x", "fail": {"login": "handler", "before_transfer": "fatal"}});
    let plugins = vec![
        lab.plugin("flaky", "", failing.clone()),
        lab.plugin(
            "tolerant",
            "[failure]\non_failure = \"continue\"\n",
            failing.clone(),
        ),
        lab.plugin(
            "once",
            "[failure]\nrestart = \"never\"\non_failure = \"continue\"\n",
            failing,
        ),
    ];
    let mut events = lab.broker.subscribe(worker::EVENTS, None).unwrap();
    let runtime = lab.runtime(&config("", plugins)).await;
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
    assert_eq!(lab.calls("init x").len(), 4, "three starts and one restart");
    assert_eq!(
        lab.reports()
            .iter()
            .filter(|entry| *entry == "flaky:init x")
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
            Duration::from_secs(5),
        )
        .await;
    assert!(matches!(failed, Outcome::Failed(_)), "{failed:?}");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(active[2].status(), Status::Failed);
    runtime.plugins().shutdown().await;
    assert!(
        active
            .iter()
            .all(|instance| instance.status() == Status::Stopped)
    );
}

pub(crate) async fn reload_diff_keeps_reloads_or_replaces(fixture: &dyn Fixture) {
    let mut lab = Lab::new(fixture);
    let a = json!({"tag": "a"});
    let b = json!({"tag": "b"});
    let first = config(
        "",
        vec![
            lab.plugin("same", "", a.clone()),
            lab.plugin("hot", "hot_reload = true\n", a.clone()),
            lab.plugin("cold", "", a.clone()),
            lab.plugin("code", "", a.clone()),
            lab.plugin("gone", "", a.clone()),
        ],
    );
    let old = lab.runtime(&first).await;
    let before = old.plugins().active();
    let mut changed_code = lab.plugin("code", "", a.clone());
    changed_code.fingerprint = "two".into();
    let second = config(
        "",
        vec![
            lab.plugin("new", "", a.clone()),
            lab.plugin("same", "", a.clone()),
            lab.plugin("hot", "hot_reload = true\n", b.clone()),
            lab.plugin("cold", "", b.clone()),
            changed_code,
        ],
    );
    let next =
        Extensions::with_engines(&second, lab.broker.clone(), Some(&old), Engines::default())
            .unwrap();
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
    assert_eq!(after[2].config_json(), b.to_string());
    let log = lab.reports();
    for expected in [
        "hot:reload b",
        "cold:shutdown stop",
        "cold:init b",
        "code:shutdown stop",
        "code:init a",
        "gone:shutdown stop",
        "new:init a",
    ] {
        assert!(log.contains(&expected.to_owned()), "{expected} in {log:?}");
    }
    assert!(!log.contains(&"same:shutdown stop".to_owned()));
    assert!(!log.contains(&"hot:shutdown stop".to_owned()));
    assert_eq!(old.plugins().active().len(), 5);
    assert_eq!(before[4].status(), Status::Stopped);
    next.plugins().shutdown().await;
}

fn command(name: &str, suggest: serde_json::Value) -> serde_json::Value {
    json!({"name": name, "permission": "test.command", "suggest": suggest})
}

pub(crate) async fn command_names_have_one_owner(fixture: &dyn Fixture) {
    let lab = Lab::new(fixture);
    let ban = json!({"commands": [command("ban", json!("none"))]});
    let colliding = config(
        "",
        vec![lab.plugin("a", "", ban.clone()), lab.plugin("b", "", ban)],
    );
    let runtime = lab.extensions(&colliding).unwrap();
    let error = runtime.plugins().start().await.unwrap_err();
    assert!(
        error.to_string().contains("already registered by plugin a"),
        "{error}"
    );
    assert!(runtime.plugins().active().is_empty());

    let script = "extensions = { api_version = 2, commands = { hello = { permission = 'x', run = function() end } } }";
    let runtime = lab
        .extensions(&config(
            script,
            vec![lab.plugin(
                "c",
                "",
                json!({"commands": [command("hello", json!("none"))]}),
            )],
        ))
        .unwrap();
    let error = runtime.plugins().start().await.unwrap_err();
    assert!(
        error.to_string().contains("configuration script"),
        "{error}"
    );
}

pub(crate) async fn rich_text_reaches_players(fixture: &dyn Fixture) {
    let mut lab = Lab::new(fixture);
    let banned = json!({
        "text": "Banned",
        "color": "red",
        "extra": [{"text": "\nReason: spam", "color": "gray", "italic": true}],
    });
    let invite = json!([
        {"text": "Alex invited you. "},
        {
            "text": "[Accept]",
            "color": "green",
            "click_event": {"action": "run_command", "command": "/party accept Alex"},
            "hover_event": {"action": "show_text", "value": {"text": "Join", "bold": true}},
        },
    ]);
    let plugins = vec![
        lab.plugin(
            "rich",
            "",
            json!({
                "decide": {"login": {"deny": banned}, "command": {"message": invite}},
                "commands": [command("party", json!("none"))],
                "grant": [[uuid(1), "test.command"]],
                "rich": invite,
            }),
        ),
        // An empty component is not a valid decision: on_failure applies.
        lab.plugin(
            "empty",
            "",
            json!({"decide": {"initial_server": {"deny": {}}}}),
        ),
    ];
    let runtime = lab.runtime(&config("", plugins)).await;
    assert_eq!(
        session(&runtime, 1).decision("login").await.unwrap(),
        Action::Deny(Text::Component(banned))
    );
    let error = session(&runtime, 1)
        .decision("initial_server")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("must not be empty"), "{error}");
    assert_eq!(
        session(&runtime, 1).command("party").await.unwrap(),
        Action::Message(Text::Component(invite.clone()))
    );

    let (_presence, mut actions) = runtime
        .players()
        .register(1, &[1; 16], "Player1", 774, "lobby");
    session(&runtime, 1).ready("lobby").await;
    lab.wait(|entry| entry == "rich:rich true true 1 true")
        .await;
    let mut received = Vec::new();
    for _ in 0..4 {
        let action = timeout(Duration::from_secs(5), actions.recv());
        received.push(action.await.unwrap().unwrap());
    }
    let text = Text::Component(invite);
    assert_eq!(
        received,
        [
            PlayerAction::Message(text.clone()),
            PlayerAction::ActionBar(text.clone()),
            PlayerAction::Message(text.clone()),
            PlayerAction::Kick(text),
        ]
    );
    runtime.plugins().shutdown().await;
}

pub(crate) async fn commands_check_permissions(fixture: &dyn Fixture) {
    let mut lab = Lab::new(fixture);
    let plugins = vec![lab.plugin(
        "greeter",
        "",
        json!({
            "commands": [command("hello", json!(["alpha", "beta"]))],
            "grant": [[uuid(1), "test.command"]],
        }),
    )];
    let runtime = lab.runtime(&config("", plugins)).await;
    let registration = runtime.plugins().active()[0].registration().unwrap();
    assert_eq!(
        registration.commands,
        [CommandSpec {
            name: "hello".into(),
            permission: "test.command".into(),
            suggest: SuggestKind::Fixed(vec!["alpha".into(), "beta".into()]),
        }]
    );
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
        lab.calls("command"),
        ["greeter:command"],
        "denied commands never reach the plugin"
    );
    runtime.plugins().shutdown().await;
}

pub(crate) async fn suggest_kinds_complete(fixture: &dyn Fixture) {
    let lab = Lab::new(fixture);
    let plugins = vec![lab.plugin(
        "completer",
        "",
        json!({
            "commands": [
                command("fixed", json!(["alpha", "beta"])),
                command("who", json!("players")),
                command("where", json!("servers")),
                command("dyn", json!("dynamic")),
                command("plain", json!("none")),
            ],
            "grant": [[uuid(1), "test.command"]],
        }),
    )];
    let runtime = lab.runtime(&config("", plugins)).await;
    let registration = runtime.plugins().active()[0].registration().unwrap();
    assert!(registration.wants(Hook::Suggest), "dynamic suggestions");
    let (_alice, _actions) = runtime
        .players()
        .register(9, &[9; 16], "Alice", 774, "lobby");
    let player = session(&runtime, 1);
    assert_eq!(
        player.visible_commands(),
        [
            ("fixed".to_owned(), true),
            ("who".to_owned(), true),
            ("where".to_owned(), true),
            ("dyn".to_owned(), true),
            ("plain".to_owned(), false),
        ]
    );
    let texts = |suggestions: Vec<crate::protocol::Suggestion>| -> Vec<String> {
        suggestions
            .into_iter()
            .map(|suggestion| suggestion.text)
            .collect()
    };
    assert_eq!(
        texts(player.suggest("fixed", "b", Vec::new).await.unwrap()),
        ["beta"]
    );
    assert_eq!(
        texts(player.suggest("who", "al", Vec::new).await.unwrap()),
        ["Alice"]
    );
    assert_eq!(
        texts(
            player
                .suggest("where", "lo", || vec!["lobby".into(), "game".into()])
                .await
                .unwrap()
        ),
        ["lobby"]
    );
    assert_eq!(
        texts(player.suggest("dyn", "x ga", Vec::new).await.unwrap()),
        ["gamma"]
    );
    assert!(
        player
            .suggest("plain", "", Vec::new)
            .await
            .unwrap()
            .is_empty()
    );
    // Without the permission there are no suggestions at all.
    assert!(
        session(&runtime, 2)
            .suggest("fixed", "", Vec::new)
            .await
            .unwrap()
            .is_empty()
    );
    runtime.plugins().shutdown().await;
}

pub(crate) async fn observers_reach_every_plugin(fixture: &dyn Fixture) {
    let mut lab = Lab::new(fixture);
    let plugins = vec![
        lab.plugin("a", "", json!({})),
        lab.plugin("b", "", json!({})),
    ];
    let runtime = lab.runtime(&config("", plugins)).await;
    let mut player = session(&runtime, 1);
    player.ready("lobby").await;
    drop(player);
    for expected in ["a:join", "b:join", "a:disconnect", "b:disconnect"] {
        lab.wait(|entry| entry == expected).await;
    }
    runtime.plugins().shutdown().await;
}

pub(crate) async fn services_scope_the_store_and_deliver_timers_and_messages(
    fixture: &dyn Fixture,
) {
    let mut lab = Lab::new(fixture);
    let plugins = vec![lab.plugin(
        "svc",
        "",
        json!({"increment": 2, "timer_ms": 10, "subscribe": "plugin.inbox"}),
    )];
    let runtime = lab.runtime(&config("", plugins)).await;
    lab.wait(|entry| entry == "svc:timer 2").await;
    lab.broker
        .publish("plugin.inbox", bytes::Bytes::from_static(b"hello"))
        .unwrap();
    lab.wait(|entry| entry == "svc:message hello").await;
    // The store entry lives under the plugin's own namespace.
    let store = &runtime.plugins().persistent.base.store;
    assert_eq!(
        store.namespace("plugin.svc.counters"),
        [("hits".to_owned(), b"2".to_vec())]
    );
    assert!(store.namespace("counters").is_empty());
    runtime.plugins().shutdown().await;
}

pub(crate) async fn route_status_and_http_follow_the_script(fixture: &dyn Fixture) {
    let lab = Lab::new(fixture);
    let config = config(
        "",
        vec![lab.plugin(
            "edge",
            "",
            json!({
                "route_reject": {"ip": "203.0.113.9", "reason": "blocked"},
                "status_motd": "Plugin MOTD",
                "http": {"path": "/ext/hello", "body": "hi"},
            }),
        )],
    );
    let runtime = lab.runtime(&config).await;
    let router = crate::hooks::Router::new(&config).with_plugins(runtime.plugins().clone());
    let connection = |ip: &str| crate::hooks::ConnectionInfo {
        listener: "public".into(),
        peer_addr: format!("{ip}:5000").parse().unwrap(),
        local_addr: "127.0.0.1:25565".parse().unwrap(),
        default_backend: Some("lobby".into()),
    };
    assert_eq!(
        router.route(connection("203.0.113.9")).await.unwrap(),
        super::api::RouteDecision::Reject {
            reason: Some("blocked".into())
        }
    );
    assert_eq!(
        router.route(connection("198.51.100.1")).await.unwrap(),
        super::api::RouteDecision::Default
    );
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
    assert_eq!(changes.motd, Some(json!("Plugin MOTD")));
    let request = |path: &str| crate::http_script::HttpRequest {
        method: "GET".into(),
        path: path.into(),
        query: String::new(),
        body: String::new(),
        headers: BTreeMap::new(),
        context: json!({}),
    };
    let response = runtime
        .plugins()
        .http(&request("/ext/hello"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (response.status, response.body.as_str()),
        (200, "hi"),
        "{response:?}"
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

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            if entry.file_name() != "target" && entry.file_name() != "data" {
                copy(&entry.path(), &target);
            }
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// The shared example spec (`examples/plugins/README.md`): `example` is a
/// ready plugin folder, copied into a fresh configuration directory.
pub(crate) async fn hello_example(engines: Engines, example: &Path) {
    let root = directory();
    let folder = root
        .join("plugins")
        .join(example.file_name().expect("an example folder"));
    copy(example, &folder);
    let spec = installed(&folder, r#"{"deny":"Mallory"}"#);
    let namespace = format!("plugin.{}.hello", spec.name);
    let broker = Broker::default();
    let runtime =
        Extensions::with_engines(&config("", vec![spec]), broker.clone(), None, engines).unwrap();
    runtime.plugins().start().await.unwrap();
    let registration = runtime.plugins().active()[0].registration().unwrap();
    assert_eq!(
        registration.commands,
        [CommandSpec {
            name: "hello".into(),
            permission: "hello.use".into(),
            suggest: SuggestKind::Players,
        }]
    );
    assert_eq!(
        registration.hooks,
        [
            Hook::Login,
            Hook::Command,
            Hook::Join,
            Hook::Message,
            Hook::Timer
        ]
    );

    assert_eq!(
        session_named(&runtime, 2, "Mallory")
            .decision("login")
            .await
            .unwrap(),
        Action::Deny("You are not welcome here.".into())
    );
    assert_eq!(
        session(&runtime, 1).decision("login").await.unwrap(),
        Action::Continue
    );

    let base = &runtime.plugins().persistent.base;
    base.permissions.grant(&uuid(1), "hello.use");
    let (_presence, mut actions) = runtime
        .players()
        .register(1, &[1; 16], "Player1", 774, "lobby");
    let mut player = session(&runtime, 1);
    assert_eq!(player.visible_commands(), [("hello".to_owned(), true)]);
    assert_eq!(
        player.command("hello").await.unwrap(),
        Action::Message("Hello, Player1!".into())
    );
    let suggestions = player.suggest("hello", "pl", Vec::new).await.unwrap();
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].text, "Player1");
    assert_eq!(
        session(&runtime, 3).command("hello").await.unwrap(),
        Action::Message("You do not have permission to use this command.".into())
    );

    player.ready("lobby").await;
    let welcome = timeout(Duration::from_secs(5), actions.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(welcome, PlayerAction::Message("Welcome, Player1!".into()));
    timeout(Duration::from_secs(5), async {
        while base.store.namespace(&namespace) != [("joins".to_owned(), b"1".to_vec())] {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("join increments hello/joins");

    let reply = broker
        .request(
            "hello.ping",
            bytes::Bytes::from_static(b"ping \x00 payload"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(reply.payload.as_ref(), b"ping \x00 payload");
    runtime.plugins().shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}

/// One `#[tokio::test]` per scenario for `$fixture`, a `Fixture` value.
/// With `if $ready`, a scenario returns at once while `$ready` is false (an
/// engine whose guest cannot be built on this machine).
macro_rules! conformance_tests {
    ($fixture:expr, if $ready:expr) => {
        $crate::plugins::conformance::conformance_tests!(@scenarios $fixture, $ready);
    };
    ($fixture:expr) => {
        $crate::plugins::conformance::conformance_tests!(@scenarios $fixture, true);
    };
    (@scenarios $fixture:expr, $ready:expr) => {
        #[tokio::test]
        async fn decisions_follow_the_chain() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::decisions_follow_the_chain(&$fixture).await;
        }

        #[tokio::test]
        async fn remote_responders_follow_plugins() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::remote_responders_follow_plugins(&$fixture).await;
        }

        #[tokio::test]
        async fn first_answer_other_than_continue_wins() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::first_answer_other_than_continue_wins(&$fixture).await;
        }

        #[tokio::test]
        async fn deadlines_apply_on_timeout() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::deadlines_apply_on_timeout(&$fixture).await;
        }

        #[tokio::test]
        async fn failures_apply_on_failure_and_restart() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::failures_apply_on_failure_and_restart(&$fixture).await;
        }

        #[tokio::test]
        async fn reload_diff_keeps_reloads_or_replaces() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::reload_diff_keeps_reloads_or_replaces(&$fixture).await;
        }

        #[tokio::test]
        async fn command_names_have_one_owner() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::command_names_have_one_owner(&$fixture).await;
        }

        #[tokio::test]
        async fn rich_text_reaches_players() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::rich_text_reaches_players(&$fixture).await;
        }

        #[tokio::test]
        async fn commands_check_permissions() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::commands_check_permissions(&$fixture).await;
        }

        #[tokio::test]
        async fn suggest_kinds_complete() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::suggest_kinds_complete(&$fixture).await;
        }

        #[tokio::test]
        async fn observers_reach_every_plugin() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::observers_reach_every_plugin(&$fixture).await;
        }

        #[tokio::test]
        async fn services_scope_the_store_and_deliver_timers_and_messages() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::services_scope_the_store_and_deliver_timers_and_messages(
                &$fixture,
            )
            .await;
        }

        #[tokio::test]
        async fn route_status_and_http_follow_the_script() {
            if !$ready {
                return;
            }
            $crate::plugins::conformance::route_status_and_http_follow_the_script(&$fixture).await;
        }
    };
}
pub(crate) use conformance_tests;
