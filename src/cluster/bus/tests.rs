//! The bus of three simulated peers, each with its own broker, on the
//! paused clock. Bus clients are in-process subscribers of each peer's
//! broker (a QUIC client is one such subscriber with `client` set); the
//! QUIC listener's part of node sessions is tested in
//! `crate::messaging::quic`.
use super::*;
use crate::cluster::sim::{Sim, until};
use crate::messaging::{
    ConsumerConfig, StreamConfig, SubscribeOptions,
    quic::{DynamicIdentities, NodeSessionObserver},
};
use std::collections::HashMap as Map;

const WAIT: Duration = Duration::from_secs(20);

struct Net {
    sim: Sim,
    states: Arc<Mutex<Map<usize, Arc<BusState>>>>,
    brokers: Vec<Broker>,
    attached: Vec<Option<Attached>>,
}

impl Net {
    /// `count` voters with the bus components, formed but not attached.
    async fn form(count: usize) -> Self {
        let states: Arc<Mutex<Map<usize, Arc<BusState>>>> = Arc::default();
        let mut sim = Sim::new(count);
        let created = states.clone();
        sim.components = Some(Arc::new(move |index| {
            let state = BusState::new();
            created.lock().unwrap().insert(index, state.clone());
            components(&state)
        }));
        sim.form(count).await;
        for index in 0..count {
            let peer = sim.peer(index).clone();
            until(WAIT, move || {
                peer.is_enabled(EntryKind::Bus).unwrap_or(false)
            })
            .await;
        }
        Self {
            sim,
            states,
            brokers: (0..count).map(|_| Broker::default()).collect(),
            attached: (0..count).map(|_| None).collect(),
        }
    }

    fn state(&self, index: usize) -> Arc<BusState> {
        self.states.lock().unwrap()[&index].clone()
    }

    async fn attach(&mut self, index: usize, options: AttachOptions) {
        let attached = attach(
            self.sim.peer(index),
            &self.brokers[index],
            &self.state(index),
            options,
        )
        .await
        .unwrap();
        self.attached[index] = Some(attached);
    }

    async fn attach_all(&mut self) {
        for index in 0..self.brokers.len() {
            self.attach(index, AttachOptions::default()).await;
        }
    }

    async fn shutdown(mut self) {
        for attached in &mut self.attached {
            drop(attached.take());
        }
        self.sim.shutdown().await;
    }
}

/// Wait until a request from `from` to `subject` finds a responder, which
/// shows that `from` knows the responder's interest.
async fn reachable(broker: &Broker, subject: &str) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if broker
            .request(subject, Bytes::new(), Duration::from_millis(500))
            .await
            .is_ok()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{subject} never became reachable"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Answer every request on `subject` at `broker` with `answer`.
fn respond(broker: &Broker, subject: &str, answer: &'static str) -> JoinHandle<()> {
    let mut subscription = broker.subscribe(subject, None).unwrap();
    let broker = broker.clone();
    tokio::spawn(async move {
        while let Ok(request) = subscription.recv().await {
            if let Some(reply) = request.reply.as_deref() {
                let _ = broker.publish(reply, Bytes::from_static(answer.as_bytes()));
            }
        }
    })
}

async fn drain(subscription: &mut crate::messaging::Subscription) -> Vec<Bytes> {
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut messages = Vec::new();
    while let Some(message) = subscription.try_recv().unwrap() {
        messages.push(message.payload);
    }
    messages
}

#[tokio::test(start_paused = true)]
async fn messages_requests_and_queue_groups_cross_peers() {
    let mut net = Net::form(3).await;
    net.attach_all().await;
    let [a, b, c] = [0, 1, 2].map(|index| net.brokers[index].clone());
    // Subscribed before the readiness probes, so their interest is known
    // once a probe is answered: frames to a peer arrive in order.
    let mut on_b = b.subscribe("game.score", None).unwrap();
    let mut on_c = c.subscribe("game.>", None).unwrap();
    let mut workers = [
        b.subscribe("jobs.run", Some("workers")).unwrap(),
        c.subscribe("jobs.run", Some("workers")).unwrap(),
    ];
    let _probe_b = respond(&b, "probe.b", "b");
    let _probe_c = respond(&c, "probe.c", "c");
    reachable(&a, "probe.b").await;
    reachable(&a, "probe.c").await;

    let report = a.publish("game.score", Bytes::from_static(b"7")).unwrap();
    assert_eq!(report.forwarded, 2);
    assert_eq!(drain(&mut on_b).await, [Bytes::from_static(b"7")]);
    assert_eq!(drain(&mut on_c).await, [Bytes::from_static(b"7")]);

    // Request and reply between peers, through `_INBOX.<peer>.` inboxes.
    let reply = a
        .request("probe.c", Bytes::new(), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(reply.payload, "c");

    // One member of the queue group across the cluster gets each message.
    for _ in 0..20 {
        a.publish("jobs.run", Bytes::new()).unwrap();
    }
    let mut total = 0;
    for worker in &mut workers {
        total += drain(worker).await.len();
    }
    assert_eq!(total, 20);

    // A built-in request subject is answered by the peer it reaches only.
    let _control = [
        respond(&a, "rift.control.status", "a"),
        respond(&b, "rift.control.status", "b"),
        respond(&c, "rift.control.status", "c"),
    ];
    for (broker, name) in [(&a, "a"), (&b, "b"), (&c, "c")] {
        let reply = broker
            .request("rift.control.status", Bytes::new(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(reply.payload, name);
    }
    let forwarded = net.attached[0].as_ref().unwrap().counters.forwarded("p1");
    assert!(forwarded >= 1, "{forwarded}");
    assert!(
        net.attached[0]
            .as_ref()
            .unwrap()
            .render_metrics()
            .contains("rift_cluster_bus_forwarded_total{peer=\"p1\"}")
    );
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn node_commands_follow_the_session_and_states_reach_the_leader() {
    let mut net = Net::form(3).await;
    net.attach_all().await;
    let leader = net.sim.leader().await;
    let agent_peer = (leader + 1) % 3;
    let other = (leader + 2) % 3;
    // The agent of node n1 connects to `agent_peer`.
    let agent_state = net.state(agent_peer);
    sessions::Observer {
        state: agent_state.clone(),
    }
    .opened("node.n1", 7);
    let agent_name = net.sim.name(agent_peer);
    for index in 0..3 {
        let state = net.state(index);
        let expected = agent_name.clone();
        until(WAIT, move || {
            state.session_holder("n1").as_deref() == Some(expected.as_str())
        })
        .await;
    }
    let mut commands = net.brokers[agent_peer]
        .subscribe_with(
            "rift.node.n1.cmd",
            None,
            SubscribeOptions {
                client: true,
                ..SubscribeOptions::default()
            },
        )
        .unwrap();
    let agent_broker = net.brokers[agent_peer].clone();
    let agent = tokio::spawn(async move {
        let command = commands.recv().await.unwrap();
        agent_broker
            .publish(
                command.reply.as_deref().unwrap(),
                Bytes::from_static(b"done"),
            )
            .unwrap();
    });
    // The leader's registry asks the agent, wherever it is connected.
    let deadline = tokio::time::Instant::now() + WAIT;
    let reply = loop {
        match net.brokers[leader]
            .request("rift.node.n1.cmd", Bytes::new(), Duration::from_secs(1))
            .await
        {
            Ok(reply) => break reply,
            Err(error) => {
                assert!(tokio::time::Instant::now() < deadline, "{error}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    assert_eq!(reply.payload, "done");
    agent.await.unwrap();

    // States go to the leader.
    let mut states = net.brokers[leader]
        .subscribe("rift.node.*.state", None)
        .unwrap();
    let report = net.brokers[agent_peer]
        .publish("rift.node.n1.state", Bytes::from_static(b"{}"))
        .unwrap();
    assert_eq!(report.forwarded, 1);
    let state = tokio::time::timeout(WAIT, states.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&*state.subject, "rift.node.n1.state");

    // The agent reconnects elsewhere: its older session closes.
    until(WAIT, || {
        agent_state
            .sessions
            .lock()
            .unwrap()
            .get("n1")
            .is_some_and(|session| session.claimed)
    })
    .await;
    sessions::Observer {
        state: net.state(other),
    }
    .opened("node.n1", 3);
    until(WAIT, || {
        !agent_state.sessions.lock().unwrap().contains_key("n1")
    })
    .await;
    let other_name = net.sim.name(other);
    let leader_state = net.state(leader);
    until(WAIT, move || {
        leader_state.session_holder("n1").as_deref() == Some(other_name.as_str())
    })
    .await;
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_token_minted_on_one_peer_authenticates_on_another() {
    let mut net = Net::form(3).await;
    let logins: Vec<Arc<Logins>> = (0..3u8)
        .map(|index| Arc::new(Logins::with_key([index + 1; instance_login::KEY_BYTES])))
        .collect();
    for (index, logins) in logins.iter().enumerate() {
        net.attach(
            index,
            AttachOptions {
                logins: Some(logins.clone()),
                ..AttachOptions::default()
            },
        )
        .await;
    }
    let all = logins.clone();
    until(WAIT, move || {
        all.iter().all(|logins| logins.key() == all[0].key())
    })
    .await;
    let grants = crate::config::InstanceMessaging {
        publish: vec!["rift.perms.hello".into()],
        subscribe: vec![],
    };
    for logins in &logins {
        logins.register("lobby-1", 3, Some("lobby"), grants.clone());
    }
    let token = logins[0].token("lobby-1", 3);
    let identity = logins[2].authenticate(&token).expect("accepted elsewhere");
    assert_eq!(identity.name, "server.lobby-1");
    net.shutdown().await;
}

async fn streams_of(net: &Net, index: usize) -> Map<String, Stream> {
    let durable = Stream::open(
        net.sim.directory(index).join("jobs-stream"),
        StreamConfig {
            subjects: vec!["jobs.>".into()],
            sync_on_write: false,
            ..StreamConfig::default()
        },
    )
    .await
    .unwrap();
    let memory = Stream::memory(StreamConfig {
        subjects: vec!["volatile.>".into()],
        ..StreamConfig::default()
    })
    .unwrap();
    Map::from([
        ("jobs".to_owned(), durable),
        ("volatile".to_owned(), memory),
    ])
}

#[tokio::test(start_paused = true)]
async fn streams_are_served_by_the_leader_and_survive_a_leader_change() {
    let mut net = Net::form(3).await;
    let mut streams = Vec::new();
    for index in 0..3 {
        let handles = streams_of(&net, index).await;
        streams.push(handles.clone());
        net.attach(
            index,
            AttachOptions {
                streams: handles,
                ..AttachOptions::default()
            },
        )
        .await;
    }
    let leader = net.sim.leader().await;
    let publisher = (leader + 1) % 3;
    let reader = (leader + 2) % 3;
    for n in 1..=3u8 {
        let record = streams[publisher]["jobs"]
            .publish("jobs.run", None, Bytes::from(vec![n]))
            .await
            .unwrap();
        assert_eq!(record.sequence, u64::from(n));
    }
    streams[publisher]["volatile"]
        .publish("volatile.x", None, Bytes::new())
        .await
        .unwrap();
    // Too large for any stream entry.
    assert!(matches!(
        streams[publisher]["jobs"]
            .publish("jobs.big", None, Bytes::from(vec![0; 2 * 1024 * 1024]))
            .await,
        Err(crate::messaging::Error::PayloadTooLarge { .. })
    ));
    let config = ConsumerConfig {
        ack_wait: Duration::from_secs(60),
        ..ConsumerConfig::default()
    };
    streams[leader]["jobs"]
        .consumer("worker", config)
        .await
        .unwrap();
    let consumer = streams[reader]["jobs"]
        .get_consumer("worker")
        .await
        .unwrap();
    let delivered: Vec<u64> = consumer
        .fetch(10)
        .await
        .unwrap()
        .iter()
        .map(|delivery| delivery.record.sequence)
        .collect();
    assert_eq!(delivered, [1, 2, 3]);
    consumer.ack(1).await.unwrap();
    assert!(matches!(
        consumer.ack(9).await,
        Err(crate::messaging::Error::InvalidAck)
    ));
    // Nothing is due again before the ack wait.
    assert!(consumer.fetch(10).await.unwrap().is_empty());
    assert_eq!(
        streams[reader]["volatile"]
            .replay(1, 10)
            .await
            .unwrap()
            .len(),
        1
    );

    drop(net.attached[leader].take());
    net.sim.crash(leader).await;
    let new_leader = net.sim.leader().await;
    assert_ne!(new_leader, leader);
    // The acknowledged record stays acknowledged; the pending ones come again.
    let deadline = tokio::time::Instant::now() + WAIT;
    let again = loop {
        match consumer.fetch(10).await {
            Ok(deliveries) => break deliveries,
            Err(error) => {
                assert!(tokio::time::Instant::now() < deadline, "{error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    let sequences: Vec<u64> = again
        .iter()
        .map(|delivery| delivery.record.sequence)
        .collect();
    assert_eq!(sequences, [2, 3]);
    assert_eq!(again[0].record.message.payload, Bytes::from(vec![2u8]));
    let records = streams[reader]["jobs"].replay(1, 10).await.unwrap();
    assert_eq!(records.len(), 3);
    // The memory stream started empty on the new leader.
    let survivor = if publisher == new_leader {
        reader
    } else {
        publisher
    };
    assert!(
        streams[survivor]["volatile"]
            .replay(1, 10)
            .await
            .unwrap()
            .is_empty()
    );
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn federation_recovers_after_a_partition_heals() {
    let mut net = Net::form(3).await;
    net.attach_all().await;
    let [a, c] = [0, 2].map(|index| net.brokers[index].clone());
    let _probe = respond(&c, "probe.before", "c");
    reachable(&a, "probe.before").await;
    net.sim.isolate(2);
    let peers = net.sim.peer(0).clone();
    until(WAIT, move || !peers.links().is_up("p2")).await;
    // Interest that changes while the link is down is lost on the way.
    let mut late = c.subscribe("game.late", None).unwrap();
    let _late_probe = respond(&c, "probe.after", "c");
    net.sim.heal();
    reachable(&a, "probe.after").await;
    assert_eq!(
        a.publish("game.late", Bytes::from_static(b"1"))
            .unwrap()
            .forwarded,
        1
    );
    assert_eq!(drain(&mut late).await, [Bytes::from_static(b"1")]);
    net.shutdown().await;
}

#[test]
fn peer_events_describe_the_table_changes() {
    let before = BTreeMap::from([
        ("p0".to_owned(), (1, false)),
        ("p1".to_owned(), (1, false)),
        ("p2".to_owned(), (2, false)),
    ]);
    let after = BTreeMap::from([
        ("p0".to_owned(), (2, false)),
        ("p1".to_owned(), (1, true)),
        ("p3".to_owned(), (0, false)),
    ]);
    let events: Vec<String> = peer_events(&before, &after)
        .into_iter()
        .map(|event| {
            format!(
                "{} {}",
                event["event"].as_str().unwrap(),
                event["name"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        events,
        [
            "peer_joined p0",
            "peer_expired p1",
            "peer_added p3",
            "peer_removed p2"
        ]
    );
}
