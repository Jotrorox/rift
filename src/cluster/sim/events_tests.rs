//! Applied events on three simulated peers (docs/clustering.md, "Subject
//! scopes"): a change of the instance registry, made through one peer's
//! replicated store, is announced on `rift.events.instances` exactly once
//! on every peer's own broker, the peer that made it included.
//!
//! The stores' synchronous APIs block their thread until the write is
//! applied, so the tests call them on a thread of their own
//! ([`off_thread`]) while the paused runtime keeps the cluster going.
use super::Sim;
use crate::{
    config::{InstanceStorage, MovedServer, ServiceInstance},
    database::Operation,
    instance_store::{
        InstanceOp, Store,
        events::{self, InstanceEvents},
    },
    messaging::{Broker, Subscription},
    provisioning::Plan,
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::time::Instant;

/// Run `work` on a thread outside the runtime and wait for it.
async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver.await.expect("the thread finished")
}

/// Every event each subscriber has received so far.
fn drain(subscriptions: &mut [Subscription], received: &mut [Vec<Value>]) {
    for (subscription, events) in subscriptions.iter_mut().zip(received.iter_mut()) {
        while let Some(message) = subscription.try_recv().unwrap() {
            events.push(serde_json::from_slice(&message.payload).unwrap());
        }
    }
}

/// Wait until every peer announced something, then a while longer, and
/// return each peer's one announcement.
async fn one_each(subscriptions: &mut [Subscription]) -> Vec<Value> {
    let mut received: Vec<Vec<Value>> = subscriptions.iter().map(|_| Vec::new()).collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        drain(subscriptions, &mut received);
        if received.iter().all(|events| !events.is_empty()) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "not every peer announced the change: {received:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    drain(subscriptions, &mut received);
    received
        .into_iter()
        .enumerate()
        .map(|(index, mut events)| {
            assert_eq!(events.len(), 1, "p{index}: {events:?}");
            events.pop().unwrap()
        })
        .collect()
}

fn assert_every(events: &[Value], expected: &Value) {
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event, expected, "p{index}");
    }
}

#[tokio::test(start_paused = true)]
async fn instance_changes_are_announced_once_on_every_peer() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    let follower = (0..3).find(|index| *index != leader).unwrap();
    let brokers: Vec<Broker> = (0..3).map(|_| Broker::default()).collect();
    let mut subscriptions: Vec<Subscription> = brokers
        .iter()
        .map(|broker| broker.subscribe(events::SUBJECT, None).unwrap())
        .collect();
    let stores: Vec<Arc<Store>> = brokers
        .iter()
        .enumerate()
        .map(|(index, broker)| {
            let store = Arc::new(Store::new(sim.database(index)).unwrap());
            let events = InstanceEvents::new(broker.clone(), |node: &str, port: u16| {
                Some(format!("{node}.example:{port}"))
            });
            store.watch_events(Arc::new(events)).unwrap();
            store
        })
        .collect();

    // An unfinished creation is no instance yet: nothing is announced.
    let instance = ServiceInstance {
        group: "games".into(),
        port: 30001,
        template: Some("lobby".into()),
        storage: InstanceStorage::Disposable,
        node: Some("n1".into()),
    };
    let plan = Plan {
        directory: PathBuf::from("games-1"),
        marker: None,
        stage: None,
        created: false,
    };
    let outcome = sim
        .peer(follower)
        .run_operation(Operation::Instances(InstanceOp::BeginCreation {
            name: "games-1".into(),
            node: "n1".into(),
            port: 30001,
            definition: json!({ "instance": instance, "plan": plan }).to_string(),
        }))
        .await
        .unwrap();
    assert!(!outcome.failed(), "{outcome:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut received: Vec<Vec<Value>> = (0..3).map(|_| Vec::new()).collect();
    drain(&mut subscriptions, &mut received);
    assert!(
        received.iter().all(Vec::is_empty),
        "announced before it was ready: {received:?}"
    );

    // The registration, committed through a follower's store.
    let store = stores[follower].clone();
    off_thread(move || store.commit_creation("games-1"))
        .await
        .unwrap();
    assert_every(
        &one_each(&mut subscriptions).await,
        &json!({
            "name": "games-1",
            "group": "games",
            "port": 30001,
            "address": "n1.example:30001",
            "node": "n1",
            "template": "lobby",
            "storage": "disposable",
            "created": true,
            "event": "created",
        }),
    );

    // Its removal, through the leader's store.
    let store = stores[leader].clone();
    off_thread(move || store.begin_removal("games-1"))
        .await
        .unwrap();
    assert_every(
        &one_each(&mut subscriptions).await,
        &json!({
            "name": "games-1",
            "node": "n1",
            "removed": true,
            "storage": "disposable",
            "event": "removed",
        }),
    );

    // A named server moved, then moved back where the configuration has it.
    let moved = MovedServer {
        configured_node: "n1".into(),
        configured_backend: "10.0.0.1:25565".into(),
        node: "n2".into(),
        port: 25565,
    };
    let store = stores[follower].clone();
    off_thread(move || store.set_moved_server("lobby", Some(&moved)))
        .await
        .unwrap();
    assert_every(
        &one_each(&mut subscriptions).await,
        &json!({
            "name": "lobby",
            "moved": true,
            "group": null,
            "from": "n1",
            "node": "n2",
            "port": 25565,
            "address": "n2.example:25565",
            "storage": null,
            "configured_node": "n1",
            "event": "moved",
        }),
    );
    let store = stores[leader].clone();
    off_thread(move || store.set_moved_server("lobby", None))
        .await
        .unwrap();
    assert_every(
        &one_each(&mut subscriptions).await,
        &json!({
            "name": "lobby",
            "moved": true,
            "group": null,
            "from": "n2",
            "node": "n1",
            "port": 25565,
            "address": "10.0.0.1:25565",
            "storage": null,
            "configured_node": null,
            "event": "moved",
        }),
    );

    drop(stores);
    sim.shutdown().await;
}
