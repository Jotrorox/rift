use super::*;
use tokio::time::timeout;

fn source(extension: &str) -> String {
    format!(
        "return {{ listeners = {{ public = '127.0.0.1:0' }}, backends = {{ lobby = '127.0.0.1:25001', game = '127.0.0.1:25002' }}, routes = {{ public = 'lobby' }}, authentication = {{ online_mode = true }}, forwarding = {{ mode = 'velocity', secret_env = 'TEST_SECRET' }}, extensions = {{ {extension} }} }}"
    )
}
fn config(extension: &str) -> Config {
    Config::from_lua(&source(extension), "extension-test.lua").unwrap()
}
fn runtime(extension: &str) -> Extensions {
    Extensions::new(
        &config(extension),
        Broker::new(Default::default()).unwrap(),
        None,
    )
    .unwrap()
}
fn context(id: u64) -> Context {
    Context::authenticated(
        id,
        &AuthenticatedProfile {
            uuid: [id as u8; 16],
            name: format!("Player{id}"),
            properties: vec![],
        },
    )
}

#[test]
fn version_schema_and_authenticated_mode_are_required() {
    for invalid in [
        "",
        "api_version = 3",
        "api_version = 1, typo = true",
        "api_version = 1, login = true",
        "api_version = 1, commands = { hub = { permission = 'use', run = function() end } }",
        "api_version = 1, commands = { game = { run = function() end } }",
        "api_version = 1, permissions = { Player = { use = true } }",
        "api_version = 1, queues = { missing = 1 }",
        "api_version = 1, queues = { game = 0 }",
    ] {
        assert!(
            Config::from_lua(&source(invalid), "invalid.lua").is_err(),
            "{invalid}"
        );
    }
    let offline = source("api_version = 1").replace("online_mode = true", "online_mode = false");
    assert!(
        Config::from_lua(&offline, "offline.lua")
            .unwrap_err()
            .to_string()
            .contains("requires authentication")
    );
}

#[tokio::test]
async fn verified_identity_permissions_and_command_arguments_reach_lua() {
    let runtime = runtime(
        r#"api_version = 1,
        permissions = { ['01010101010101010101010101010101'] = { ['route.game'] = true } },
        login = function(ctx)
            assert(ctx.api_version == 1 and ctx.authenticated)
            assert(ctx.uuid == '01010101010101010101010101010101' and ctx.name == 'Player1')
            assert(ctx.server == nil)
        end,
        initial_server = function(ctx) if ctx.has_permission('route.game') then return {server='game'} end end,
        commands = { game = { permission='route.game', run=function(ctx)
            assert(ctx.command == 'game' and ctx.args == 'one two')
            return {server='game'}
        end } }
    "#,
    );
    let allowed = runtime.session(context(1)).unwrap();
    assert_eq!(allowed.decision("login").await.unwrap(), Action::Continue);
    assert_eq!(
        allowed.decision("initial_server").await.unwrap(),
        Action::Server("game".into())
    );
    assert_eq!(
        allowed.command("game one two").await.unwrap(),
        Action::Server("game".into())
    );
    let denied = runtime.session(context(2)).unwrap();
    // The callback would error on these arguments if permission checking ran later.
    assert!(matches!(
        denied.command("game wrong").await.unwrap(),
        Action::Message(_)
    ));
}

#[tokio::test]
async fn malformed_decisions_unknown_servers_and_runaway_callbacks_fail_closed() {
    for result in [
        "true",
        "{}",
        "{server='missing'}",
        "{server='game', deny='no'}",
        "{server=string.rep('a', 1025)}",
        "{queue='game'}",
    ] {
        let runtime = runtime(&format!(
            "api_version=1, initial_server=function() return {result} end"
        ));
        assert!(
            runtime
                .session(context(1))
                .unwrap()
                .decision("initial_server")
                .await
                .is_err(),
            "{result}"
        );
    }
    for body in [
        "while true do end",
        "return {server='game'}",
        "error('closed')",
    ] {
        let runtime = runtime(&format!("api_version=1, login=function() {body} end"));
        assert!(
            timeout(
                Duration::from_secs(1),
                runtime.session(context(1)).unwrap().decision("login")
            )
            .await
            .unwrap()
            .is_err()
        );
    }
}

#[tokio::test]
async fn fifo_capacity_cannot_be_bypassed_and_drop_releases_tickets_and_leases() {
    let runtime = runtime("api_version=1, queues={game=1}");
    let mut occupant = runtime.session(context(1)).unwrap();
    occupant.reserve("game").unwrap();
    occupant.ready("game").await;
    let mut first = runtime.session(context(2)).unwrap();
    let second = runtime.session(context(3)).unwrap();
    assert_eq!(first.enqueue("game").unwrap(), 1);
    assert_eq!(first.enqueue("game").unwrap(), 1);
    assert_eq!(second.enqueue("game").unwrap(), 2);
    assert!(first.reserve("game").is_err());
    drop(occupant);
    assert!(second.reserve("game").is_err());
    first.before_transfer("game", "queue").await.unwrap();
    assert!(second.reserve("game").is_err());
    first.ready("game").await;
    assert!(second.reserve("game").is_err());
    drop(first);
    second.reserve("game").unwrap();
    drop(second);
    let host = runtime.host.lock().unwrap();
    assert!(host.leases.is_empty());
    assert!(host.queues.values().all(VecDeque::is_empty));
}

#[tokio::test]
async fn aborted_transfer_releases_only_target_and_notifications_are_ordered() {
    let runtime = runtime(
        r#"api_version=1, queues={game=1},
        join=function(ctx) rift.publish('events', 'join:' .. ctx.server) end,
        before_transfer=function(ctx)
            if ctx.target == 'blocked' then return {deny='No access'} end
            rift.publish('events', 'before:' .. ctx.server .. ':' .. ctx.reason)
        end,
        after_transfer=function(ctx) rift.publish('events', 'after:' .. tostring(ctx.success)) end,
        disconnect=function(ctx) rift.publish('events', 'disconnect:' .. ctx.server) end
    "#,
    );
    let mut events = runtime.broker.subscribe("events", None).unwrap();
    let mut player = runtime.session(context(1)).unwrap();
    player.reserve("lobby").unwrap();
    player.ready("lobby").await;
    assert!(player.before_transfer("blocked", "command").await.is_err());
    player.before_transfer("game", "admin").await.unwrap();
    player.transfer_failed().await;
    assert_eq!(
        runtime.host.lock().unwrap().leases[&1],
        BTreeSet::from(["lobby".into()])
    );
    player.before_transfer("game", "queue").await.unwrap();
    player.ready("game").await;
    // Cancellation in the middle of another transfer still emits its failed
    // completion before disconnect, and releases leases synchronously.
    player.before_transfer("lobby", "recovery").await.unwrap();
    drop(player);
    assert!(runtime.host.lock().unwrap().leases.is_empty());
    for expected in [
        "join:lobby",
        "before:lobby:admin",
        "after:false",
        "before:lobby:queue",
        "after:true",
        "before:game:recovery",
        "after:false",
        "disconnect:game",
    ] {
        let message = timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.payload.as_ref(), expected.as_bytes());
    }
}

#[tokio::test]
async fn reload_pins_callbacks_and_permissions_but_shares_capacity_and_fifo() {
    let old = runtime(
        "api_version=1, queues={game=1}, initial_server=function() return {server='lobby'} end",
    );
    let next = Extensions::new(
        &config(
            "api_version=1, queues={game=1}, initial_server=function() return {server='game'} end",
        ),
        old.broker.clone(),
        Some(&old),
    )
    .unwrap();
    let existing = old.session(context(1)).unwrap();
    let new = next.session(context(2)).unwrap();
    existing.enqueue("game").unwrap();
    assert_eq!(new.enqueue("game").unwrap(), 2);
    assert_eq!(
        existing.decision("initial_server").await.unwrap(),
        Action::Server("lobby".into())
    );
    assert_eq!(
        new.decision("initial_server").await.unwrap(),
        Action::Server("game".into())
    );
    let permits = old
        .slots
        .clone()
        .acquire_many_owned(script::MAX_CONCURRENT as u32)
        .await
        .unwrap();
    assert!(new.decision("initial_server").await.is_err());
    drop(existing); // host cleanup does not need Lua worker capacity
    assert!(new.reserve("game").is_ok());
    drop(permits);
}

#[tokio::test]
async fn local_lua_state_does_not_survive_invocations_and_expired_queue_cleans_up() {
    let cfg = Config::from_lua(&format!("local calls=0; {}", source("api_version=1, queues={game=1}, login=function() calls=calls+1; assert(calls==1) end")), "state.lua").unwrap();
    let runtime = Extensions::new(&cfg, Broker::new(Default::default()).unwrap(), None).unwrap();
    let player = runtime.session(context(1)).unwrap();
    for _ in 0..2 {
        player.decision("login").await.unwrap();
    }
    player.enqueue("game").unwrap();
    runtime.host.lock().unwrap().queues.get_mut("game").unwrap()[0].1 =
        Instant::now() - Duration::from_secs(301);
    assert!(player.queued_target().is_err());
    assert!(player.queued_target().unwrap().is_none());
}

#[test]
fn timeout_and_cancellation_retain_worker_limits_and_cannot_reorder_teardown() {
    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    tokio_runtime.block_on(async {
        for cancel in [false, true] {
            let runtime = runtime("api_version=1, queues={game=1}, login=function() rift.publish('event','login') end, disconnect=function() rift.publish('event','disconnect') end");
            let mut events = runtime.broker.subscribe("event", None).unwrap();
            let (release, held) = std::sync::mpsc::channel();
            let (ready, started) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || { ready.send(()).unwrap(); held.recv_timeout(Duration::from_secs(5)).unwrap(); });
            started.recv_timeout(Duration::from_secs(1)).unwrap();
            let session = runtime.session(context(1)).unwrap();
            session.reserve("game").unwrap();
            let task = tokio::spawn(async move { session.decision("login").await });
            timeout(Duration::from_secs(1), async {
                while runtime.slots.available_permits() == script::MAX_CONCURRENT { tokio::task::yield_now().await; }
            }).await.unwrap();
            if cancel { task.abort(); assert!(task.await.unwrap_err().is_cancelled()); }
            else { assert!(task.await.unwrap().is_err()); }
            assert!(runtime.host.lock().unwrap().leases.is_empty());
            assert_eq!(runtime.slots.available_permits(), script::MAX_CONCURRENT - 1);
            // Let the deadline expire while the actual worker is still queued.
            tokio::time::sleep(script::EXECUTION_TIMEOUT).await;
            release.send(()).unwrap(); blocker.await.unwrap();
            timeout(Duration::from_secs(1), async {
                while runtime.slots.available_permits() != script::MAX_CONCURRENT { tokio::task::yield_now().await; }
            }).await.unwrap();
            // The expired login cannot publish, and teardown did not overtake it.
            assert!(timeout(Duration::from_millis(10), events.recv()).await.is_err());
        }
    });
}

#[test]
fn concurrent_admissions_never_overbook_the_last_slot() {
    let runtime = runtime("api_version=1, queues={game=1}");
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let attempts: Vec<_> = (1..=2)
        .map(|id| {
            let session = runtime.session(context(id)).unwrap();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let result = session.reserve("game");
                (session, result)
            })
        })
        .collect();
    let results: Vec<_> = attempts.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    drop(results);
    assert!(runtime.host.lock().unwrap().leases.is_empty());
}
