use super::*;
use bytes::Bytes;
use tokio::time::timeout;

fn source(extension: &str) -> String {
    format!(
        "return {{ listeners = {{ public = '127.0.0.1:0' }}, backends = {{ lobby = '127.0.0.1:25001', game = '127.0.0.1:25002' }}, routes = {{ public = 'lobby' }}, authentication = {{ online_mode = true }}, forwarding = {{ mode = 'velocity', secret_env = 'TEST_SECRET' }}, extensions = {{ api_version = 2, {extension} }} }}"
    )
}

fn setup(extension: &str) -> (Extensions, Broker) {
    let broker = Broker::new(Default::default()).unwrap();
    let config = Config::from_lua(&source(extension), "decisions.lua").unwrap();
    (
        Extensions::new(&config, broker.clone(), None).unwrap(),
        broker,
    )
}

fn session(runtime: &Extensions, id: u64) -> ExtensionSession {
    let mut context = Context::authenticated(
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

/// Answer every request on `subject` with `reply`, returning the requests seen.
fn responder(
    broker: &Broker,
    subject: &str,
    queue: Option<&str>,
    reply: &'static str,
) -> tokio::sync::mpsc::UnboundedReceiver<serde_json::Value> {
    let mut subscription = broker.subscribe(subject, queue).unwrap();
    let broker = broker.clone();
    let (seen, requests) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(message) = subscription.recv().await {
            let _ = seen.send(serde_json::from_slice(&message.payload).unwrap());
            broker
                .publish(
                    message.reply.as_deref().unwrap(),
                    Bytes::from_static(reply.as_bytes()),
                )
                .unwrap();
        }
    });
    requests
}

#[test]
fn decision_configuration_is_validated() {
    for invalid in [
        "decisions = 1",
        "decisions = { join = { subject = 'a' } }",
        "decisions = { login = 'a' }",
        "decisions = { login = {} }",
        "decisions = { login = { subject = 'a', typo = 1 } }",
        "decisions = { login = { subject = 'a.*' } }",
        "decisions = { login = { subject = 'rift.login' } }",
        "decisions = { login = { subject = '_INBOX.x' } }",
        "decisions = { login = { subject = 'a', timeout_ms = 0 } }",
        "decisions = { login = { subject = 'a', timeout_ms = 10001 } }",
        "decisions = { login = { subject = 'a', on_timeout = 'maybe' } }",
        "decisions = { login = { subject = 'a', on_no_responders = true } }",
        "decisions = { login = { subject = 'a', deny_message = '' } }",
        "decisions = { login = { subject = 'a', max_in_flight = 0 } }",
    ] {
        assert!(
            Config::from_lua(&source(invalid), "invalid.lua").is_err(),
            "{invalid}"
        );
    }
    let v1 = source("decisions = { login = { subject = 'a' } }")
        .replace("api_version = 2", "api_version = 1");
    assert!(
        Config::from_lua(&v1, "v1.lua")
            .unwrap_err()
            .to_string()
            .contains("requires api_version = 2")
    );
    setup(
        "decisions = { login = { subject = 'relaxoban.login', timeout_ms = 2000, on_timeout = 'deny', on_no_responders = 'deny', deny_message = 'retry', max_in_flight = 256 }, initial_server = { subject = 'a' }, before_transfer = { subject = 'network.transfer', timeout_ms = 300, on_timeout = 'allow' } }",
    );
}

#[tokio::test]
async fn lua_runs_first_and_any_deny_wins() {
    let (runtime, broker) = setup(
        "login = function(ctx) if ctx.name == 'Player1' then return { deny = 'lua says no' } end end, \
         decisions = { login = { subject = 'bans.login' } }",
    );
    let mut requests = responder(
        &broker,
        "bans.login",
        None,
        r#"{"action":"deny","reason":"You are banned"}"#,
    );
    assert_eq!(
        session(&runtime, 1).decision("login").await.unwrap(),
        Action::Deny("lua says no".into())
    );
    assert!(requests.try_recv().is_err(), "Lua deny skips the bus");
    assert_eq!(
        session(&runtime, 2).decision("login").await.unwrap(),
        Action::Deny("You are banned".into())
    );
    let request = requests.recv().await.unwrap();
    assert_eq!(request["hook"], "login");
    assert_eq!(request["uuid"], "02".repeat(16));
    assert_eq!(request["name"], "Player2");
    assert_eq!(request["connection_id"], "2");
    assert_eq!(request["default_server"], "lobby");
    // No decision for this hook: nothing is asked.
    assert_eq!(
        session(&runtime, 3)
            .decision("initial_server")
            .await
            .unwrap(),
        Action::Continue
    );
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn missing_and_slow_responders_apply_their_policies() {
    let (closed, _broker) = setup(
        "decisions = { login = { subject = 'bans.login', deny_message = 'Ban status unavailable' } }",
    );
    // Fail closed by default, without waiting for the timeout.
    let started = Instant::now();
    assert_eq!(
        session(&closed, 1).decision("login").await.unwrap(),
        Action::Deny("Ban status unavailable".into())
    );
    assert!(started.elapsed() < Duration::from_millis(500));

    let (open, broker) = setup(
        "decisions = { login = { subject = 'bans.login', timeout_ms = 50, on_timeout = 'allow', on_no_responders = 'deny', deny_message = 'nobody home' } }",
    );
    assert_eq!(
        session(&open, 1).decision("login").await.unwrap(),
        Action::Deny("nobody home".into())
    );
    let _silent = broker.subscribe("bans.login", None).unwrap();
    assert_eq!(
        session(&open, 2).decision("login").await.unwrap(),
        Action::Continue
    );
}

#[tokio::test]
async fn a_full_window_applies_the_timeout_policy_at_once() {
    let (runtime, broker) = setup(
        "decisions = { login = { subject = 'bans.login', timeout_ms = 10000, max_in_flight = 1, deny_message = 'busy' } }",
    );
    let mut silent = broker.subscribe("bans.login", None).unwrap();
    let first = session(&runtime, 1);
    let waiting = tokio::spawn(async move { first.decision("login").await });
    silent.recv().await.unwrap();
    let started = Instant::now();
    assert_eq!(
        session(&runtime, 2).decision("login").await.unwrap(),
        Action::Deny("busy".into())
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    // Cancelling the waiting decision frees its slot.
    waiting.abort();
    let _ = waiting.await;
    let _answer = responder(
        &broker,
        "bans.login",
        Some("deciders"),
        r#"{"action":"continue"}"#,
    );
    drop(silent);
    assert_eq!(
        session(&runtime, 3).decision("login").await.unwrap(),
        Action::Continue
    );
}

#[tokio::test]
async fn replies_are_validated_like_lua_results() {
    let (runtime, broker) = setup(
        "decisions = { login = { subject = 'login' }, initial_server = { subject = 'initial' } }",
    );
    let _initial = responder(
        &broker,
        "initial",
        None,
        r#"{"action":"server","server":"game"}"#,
    );
    assert_eq!(
        session(&runtime, 1)
            .decision("initial_server")
            .await
            .unwrap(),
        Action::Server("game".into())
    );
    let _login = responder(
        &broker,
        "login",
        None,
        r#"{"action":"server","server":"game"}"#,
    );
    assert!(session(&runtime, 2).decision("login").await.is_err());

    let (unknown, broker) = setup("decisions = { initial_server = { subject = 'initial' } }");
    let _initial = responder(
        &broker,
        "initial",
        None,
        r#"{"action":"server","server":"missing"}"#,
    );
    let error = session(&unknown, 1)
        .decision("initial_server")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown backend"));
}

#[tokio::test]
async fn queue_group_responders_share_the_load() {
    let (runtime, broker) = setup("decisions = { login = { subject = 'bans.login' } }");
    let mut a = responder(
        &broker,
        "bans.login",
        Some("deciders"),
        r#"{"action":"continue"}"#,
    );
    let mut b = responder(
        &broker,
        "bans.login",
        Some("deciders"),
        r#"{"action":"continue"}"#,
    );
    for id in 0..10 {
        assert_eq!(
            session(&runtime, id).decision("login").await.unwrap(),
            Action::Continue
        );
    }
    let (mut seen_a, mut seen_b) = (0, 0);
    while a.try_recv().is_ok() {
        seen_a += 1;
    }
    while b.try_recv().is_ok() {
        seen_b += 1;
    }
    assert_eq!((seen_a, seen_b), (5, 5));
}

#[tokio::test]
async fn transfer_denials_reserve_nothing() {
    let (runtime, broker) = setup(
        "queues = { game = 1 }, decisions = { before_transfer = { subject = 'network.transfer', timeout_ms = 300, on_timeout = 'allow' } }",
    );
    let mut requests = responder(
        &broker,
        "network.transfer",
        None,
        r#"{"action":"deny","reason":"Not during an event"}"#,
    );
    let mut player = session(&runtime, 1);
    player.ready("lobby").await;
    let error = player.before_transfer("game", "command").await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "Not during an event");
    let request = requests.recv().await.unwrap();
    assert_eq!(request["hook"], "before_transfer");
    assert_eq!(request["server"], "lobby");
    assert_eq!(request["target"], "game");
    assert_eq!(request["reason"], "command");
    // The denied transfer took no capacity.
    session(&runtime, 2).reserve("game").unwrap();
}

#[tokio::test]
async fn reload_keeps_open_decisions_counted() {
    let extension = "decisions = { login = { subject = 'bans.login', max_in_flight = 1, deny_message = 'busy' } }";
    let (old, broker) = setup(extension);
    let config = Config::from_lua(&source(extension), "decisions.lua").unwrap();
    let next = Extensions::new(&config, broker.clone(), Some(&old)).unwrap();
    assert!(Arc::ptr_eq(
        &old.decision_slots["login"],
        &next.decision_slots["login"]
    ));
    let changed = Config::from_lua(
        &source("decisions = { login = { subject = 'bans.login', max_in_flight = 2 } }"),
        "decisions.lua",
    )
    .unwrap();
    let resized = Extensions::new(&changed, broker, Some(&old)).unwrap();
    assert_eq!(resized.decision_slots["login"].available_permits(), 2);
}

#[tokio::test]
async fn player_directory_publishes_joins_switches_and_quits() {
    let (runtime, broker) = setup("");
    let mut events = broker.subscribe(players::EVENTS, None).unwrap();
    let (handle, _actions) = runtime
        .players()
        .register(9, &[0xab; 16], "Alice", 774, "lobby");
    handle.set_server("lobby");
    handle.set_server("game");
    drop(handle);
    let mut next = async || -> serde_json::Value {
        let message = timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        serde_json::from_slice(&message.payload).unwrap()
    };
    let uuid = "ab".repeat(16);
    assert_eq!(
        next().await,
        serde_json::json!({"event": "join", "connection_id": "9", "uuid": uuid, "name": "Alice", "server": "lobby"})
    );
    assert_eq!(
        next().await,
        serde_json::json!({"event": "switch", "connection_id": "9", "uuid": uuid, "name": "Alice", "server": "game", "previous": "lobby"})
    );
    assert_eq!(
        next().await,
        serde_json::json!({"event": "quit", "connection_id": "9", "uuid": uuid, "name": "Alice", "server": "game"})
    );
    assert!(events.try_recv().unwrap().is_none());
}
