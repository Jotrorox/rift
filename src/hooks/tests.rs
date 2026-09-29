use super::*;
use std::{sync::mpsc, time::Duration};

fn config(body: &str) -> Config {
    Config::from_lua(
        &format!(
            "local calls = 0
        return {{
            listeners = {{ public = '127.0.0.1:0' }},
            backends = {{ lobby = '127.0.0.1:25566', creative = '[::1]:25567' }},
            routes = {{ public = 'lobby' }},
            on_route = function(connection) {body} end,
        }}"
        ),
        "routing.lua",
    )
    .unwrap()
}

fn connection() -> ConnectionInfo {
    ConnectionInfo {
        listener: "public".into(),
        peer_addr: "[::1]:12345".parse().unwrap(),
        local_addr: "[::1]:25565".parse().unwrap(),
        default_backend: Some("lobby".into()),
    }
}

#[tokio::test]
async fn hook_receives_metadata_and_can_select_reject_or_use_the_default() {
    let metadata = "
        assert(connection.listener == 'public')
        assert(connection.peer_addr == '[::1]:12345')
        assert(connection.peer_ip == '::1' and connection.peer_port == 12345)
        assert(connection.local_addr == '[::1]:25565')
        assert(connection.local_ip == '::1' and connection.local_port == 25565)
        assert(connection.default_backend == 'lobby')
    ";
    for (body, expected) in [
        (
            "return { backend = 'creative' }",
            RouteDecision::Backend("creative".into()),
        ),
        (
            "return { reject = true, reason = 'maintenance' }",
            RouteDecision::Reject {
                reason: Some("maintenance".into()),
            },
        ),
        (
            "return { reject = true }",
            RouteDecision::Reject { reason: None },
        ),
        ("return nil", RouteDecision::Default),
    ] {
        let router = Router::new(&config(&format!("{metadata}\n{body}")));
        assert_eq!(router.route(connection()).await.unwrap(), expected);
    }
    let mut no_hook = config("error('must not run')");
    no_hook.on_route = None;
    let router = Router::new(&no_hook);
    // Static routes do not consume script capacity.
    let _slots = router
        .slots
        .clone()
        .acquire_many_owned(crate::script::MAX_CONCURRENT as u32)
        .await
        .unwrap();
    assert_eq!(
        router.route(connection()).await.unwrap(),
        RouteDecision::Default
    );
}

#[tokio::test]
async fn malformed_results_and_script_errors_fail_closed() {
    for expression in [
        "42",
        "true",
        "'creative'",
        "{}",
        "{ backend = 42 }",
        "{ backend = '' }",
        "{ backend = 'creative', reject = true }",
        "{ backend = 'creative', reason = 'wrong' }",
        "{ reject = false }",
        "{ reject = 'no' }",
        "{ reject = true, reason = 42 }",
        "{ reject = true, reason = string.rep('a', 1025) }",
        "{ reject = true, reason = string.char(255) }",
        "{ backend = 'creative', typo = true }",
        "{ [1] = 'creative' }",
    ] {
        let error = Router::new(&config(&format!("return {expression}")))
            .route(connection())
            .await
            .unwrap_err();
        assert!(
            matches!(error, RouteError::Script(_)),
            "{expression}: {error}"
        );
        assert!(
            error.to_string().contains("routing.lua: on_route:"),
            "{error}"
        );
    }
    let error = Router::new(&config("error('broken hook')"))
        .route(connection())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("broken hook"), "{error}");
    let error = Router::new(&config("return { backend = 'missing' }"))
        .route(connection())
        .await
        .unwrap_err();
    assert_eq!(error, RouteError::UnknownBackend("missing".into()));
    let error = Router::new(&config("error(string.rep('x', 4096))"))
        .route(connection())
        .await
        .unwrap_err();
    assert_eq!(error.to_string().chars().count(), 2048);
    let error = Router::new(&config("return { backend = string.rep('x', 4096) }"))
        .route(connection())
        .await
        .unwrap_err();
    assert!(error.to_string().len() < 300);
}

#[tokio::test]
async fn globals_and_upvalues_are_isolated_between_calls() {
    let router = Router::new(&config(
        "
        assert(calls == 0 and seen == nil)
        calls = calls + 1
        seen = connection.peer_addr
        connection.default_backend = 'mutated'
        return nil
    ",
    ));
    for _ in 0..8 {
        assert_eq!(
            router.route(connection()).await.unwrap(),
            RouteDecision::Default
        );
    }
}

#[tokio::test]
async fn runaway_scripts_and_allocations_stop_and_release_capacity() {
    for (body, expected) in [
        ("while true do end", "instruction limit"),
        (
            "return { reject = true, reason = string.rep('a', 16 * 1024 * 1024) }",
            "memory",
        ),
        (
            "table.insert({}, -2147483648, 'x')",
            "position out of bounds",
        ),
        // Insertion shifts must consume the remaining instruction budget.
        (
            "local t = {}; for i = 1, 20000 do t[i] = true end; table.insert(t, 1, 'x')",
            "instruction limit",
        ),
    ] {
        let router = Router::new(&config(body));
        for _ in 0..8 {
            let error = tokio::time::timeout(Duration::from_secs(2), router.route(connection()))
                .await
                .unwrap()
                .unwrap_err();
            assert!(
                error.to_string().contains(expected) || error.to_string().contains("deadline"),
                "{error}"
            );
            tokio::time::timeout(Duration::from_secs(2), async {
                while router.slots.available_permits() != crate::script::MAX_CONCURRENT {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
    }
}

#[tokio::test]
async fn scripts_cannot_bypass_budgets_or_access_external_resources() {
    let router = Router::new(&config(
        "
        assert(io == nil and os == nil and package == nil and type(require) == 'function')
        assert(jit == nil and debug == nil and ffi == nil)
        assert(pcall == nil and xpcall == nil and coroutine == nil)
        assert(load == nil and loadstring == nil and loadfile == nil and dofile == nil)
        assert(collectgarbage == nil and newproxy == nil and setmetatable == nil)
        assert(getfenv == nil and setfenv == nil and print == nil)
        assert(string.find == nil and string.match == nil and string.gsub == nil)
        assert(string.gmatch == nil and table.sort == nil)
        assert(('::1'):sub(1, 2) == '::')
        local values = {'b'}
        table.insert(values, 1, 'a')
        table.insert(values, 'c')
        assert(table.concat(values) == 'abc')
        return nil
    ",
    ));
    router.route(connection()).await.unwrap();
}

#[tokio::test]
async fn overload_is_immediate_and_capacity_is_shared_by_clones() {
    let router = Router::new(&config("return nil"));
    let slots = router
        .slots
        .clone()
        .acquire_many_owned(crate::script::MAX_CONCURRENT as u32)
        .await
        .unwrap();
    assert_eq!(
        router.clone().route(connection()).await.unwrap_err(),
        RouteError::Busy
    );
    drop(slots);
    assert!(router.route(connection()).await.is_ok());
}

#[test]
fn timeout_and_cancellation_keep_the_permit_until_the_worker_finishes() {
    // Hold the sole blocking thread so that the script is queued deterministically.
    // A timed-out/cancelled caller must not admit unlimited replacement work.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        for cancel in [false, true] {
            let router = Router::new(&config("return nil"));
            let (release, held) = mpsc::channel();
            let blocker =
                spawn_blocking(move || held.recv_timeout(Duration::from_secs(5)).unwrap());
            let task_router = router.clone();
            let task = tokio::spawn(async move { task_router.route(connection()).await });
            tokio::time::timeout(Duration::from_secs(2), async {
                while router.slots.available_permits() == crate::script::MAX_CONCURRENT {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if cancel {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                assert_eq!(task.await.unwrap().unwrap_err(), RouteError::TimedOut);
            }
            assert_eq!(
                router.slots.available_permits(),
                crate::script::MAX_CONCURRENT - 1
            );
            release.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while router.slots.available_permits() != crate::script::MAX_CONCURRENT {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(router.route(connection()).await.is_ok());
        }
    });
}

#[tokio::test]
async fn reloads_share_script_capacity_with_old_generations() {
    let old = Router::new(&config("return nil"));
    let permit = old
        .slots
        .clone()
        .acquire_many_owned(crate::script::MAX_CONCURRENT as u32)
        .await
        .unwrap();
    let new = old.reconfigured(&config("return { backend = 'creative' }"));
    assert_eq!(new.route(connection()).await.unwrap_err(), RouteError::Busy);
    drop(permit);
    assert_eq!(
        new.route(connection()).await.unwrap(),
        RouteDecision::Backend("creative".into())
    );
    assert_eq!(
        old.route(connection()).await.unwrap(),
        RouteDecision::Default
    );
}

#[tokio::test]
async fn routing_hooks_publish_to_the_shared_broker_across_reloads() {
    let broker = crate::messaging::Broker::default();
    let mut subscription = broker.subscribe("connections.*", None).unwrap();
    let config = config(
        "assert(rift.enabled); local report = rift.publish('connections.accepted', string.char(0, 255) .. connection.peer_ip); assert(report.delivered == 1); return nil",
    );
    let router = Router::new(&config).with_messaging(broker);
    for router in [router.clone(), router.reconfigured(&config)] {
        assert_eq!(
            router.route(connection()).await.unwrap(),
            RouteDecision::Default
        );
        let message = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&message.payload[..], b"\0\xff::1");
    }
}

#[tokio::test]
async fn hook_publications_are_bounded_and_disabled_without_context() {
    let disabled = Router::new(&config(
        "assert(not rift.enabled); rift.publish('events.x', 'x')",
    ));
    assert!(
        disabled
            .route(connection())
            .await
            .unwrap_err()
            .to_string()
            .contains("broker unavailable")
    );
    let broker = crate::messaging::Broker::default();
    let router = Router::new(&config(
        "for i = 1, 257 do rift.publish('events.x', 'x') end",
    ))
    .with_messaging(broker);
    assert!(
        router
            .route(connection())
            .await
            .unwrap_err()
            .to_string()
            .contains("messaging budget")
    );
}
