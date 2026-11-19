//! Three brokers federated through in-memory links.

use super::*;
use std::collections::HashSet;
use std::sync::atomic::AtomicUsize;

/// One peer's links: an ordered channel to each other peer, which a task
/// drains into that peer's broker.
struct Link {
    local: PeerId,
    up: Mutex<HashSet<PeerId>>,
    outbox: HashMap<PeerId, mpsc::UnboundedSender<FederationFrame>>,
    sent: Mutex<Vec<(PeerId, FederationFrame)>>,
    in_flight: Arc<AtomicUsize>,
}

impl Federation for Link {
    fn local_peer(&self) -> PeerId {
        self.local.clone()
    }

    fn peers(&self) -> Vec<PeerId> {
        let mut peers: Vec<PeerId> = self.up.lock().unwrap().iter().cloned().collect();
        peers.sort();
        peers
    }

    fn send(&self, peer: &PeerId, frame: FederationFrame) {
        if !self.up.lock().unwrap().contains(peer) {
            return;
        }
        // The binary form must carry everything the broker relies on.
        let frame = FederationFrame::decode(frame.encode().unwrap()).unwrap();
        self.sent
            .lock()
            .unwrap()
            .push((peer.clone(), frame.clone()));
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.outbox[peer].send(frame).unwrap();
    }
}

impl Link {
    fn set_up(&self, peer: &PeerId, up: bool) {
        let mut peers = self.up.lock().unwrap();
        if up {
            peers.insert(peer.clone());
        } else {
            peers.remove(peer);
        }
    }

    /// Messages this peer forwarded, by target.
    fn forwarded(&self) -> Vec<(PeerId, ForwardedMessage)> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(peer, frame)| match frame {
                FederationFrame::Message(message) => Some((peer.clone(), message.clone())),
                _ => None,
            })
            .collect()
    }

    /// Every pattern this peer announced.
    fn announced(&self) -> Vec<String> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, frame)| match frame {
                FederationFrame::Interest(update) => Some(update.entries.clone()),
                _ => None,
            })
            .flatten()
            .map(|entry| entry.key.pattern)
            .collect()
    }

    fn clear(&self) {
        self.sent.lock().unwrap().clear();
    }
}

struct Mesh {
    names: Vec<PeerId>,
    brokers: Vec<Broker>,
    links: Vec<Arc<Link>>,
    in_flight: Arc<AtomicUsize>,
}

impl Mesh {
    fn new(names: &[&str]) -> Self {
        let names: Vec<PeerId> = names.iter().map(|name| Arc::from(*name)).collect();
        let brokers: Vec<Broker> = names.iter().map(|_| Broker::default()).collect();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let mut links = Vec::new();
        for (i, local) in names.iter().enumerate() {
            let mut outbox = HashMap::new();
            for (j, peer) in names.iter().enumerate() {
                if i == j {
                    continue;
                }
                let (sender, mut receiver) = mpsc::unbounded_channel::<FederationFrame>();
                let (broker, from, in_flight) =
                    (brokers[j].clone(), local.clone(), in_flight.clone());
                tokio::spawn(async move {
                    while let Some(frame) = receiver.recv().await {
                        broker.handle_federation_frame(&from, frame);
                        in_flight.fetch_sub(1, Ordering::SeqCst);
                    }
                });
                outbox.insert(peer.clone(), sender);
            }
            links.push(Arc::new(Link {
                local: local.clone(),
                up: Mutex::new(
                    names
                        .iter()
                        .filter(|peer| *peer != local)
                        .cloned()
                        .collect(),
                ),
                outbox,
                sent: Mutex::new(Vec::new()),
                in_flight: in_flight.clone(),
            }));
        }
        for (broker, link) in brokers.iter().zip(&links) {
            broker.set_federation(link.clone());
        }
        Self {
            names,
            brokers,
            links,
            in_flight,
        }
    }

    /// Wait until every frame sent so far, and every frame those caused,
    /// has been handled.
    async fn settle(&self) {
        for _ in 0..2000 {
            tokio::task::yield_now().await;
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("federation frames did not settle");
    }

    fn clear(&self) {
        for link in &self.links {
            link.clear();
        }
    }
}

fn client() -> SubscribeOptions {
    SubscribeOptions {
        client: true,
        ..SubscribeOptions::default()
    }
}

fn local_scope() -> SubscribeOptions {
    SubscribeOptions {
        scope: SubscriptionScope::Local,
        ..SubscribeOptions::default()
    }
}

#[tokio::test]
async fn publications_reach_subscribers_on_other_peers_once() {
    let mesh = Mesh::new(&["a", "b", "c"]);
    let mut on_a = mesh.brokers[0].subscribe("game.>", None).unwrap();
    let mut on_b = mesh.brokers[1].subscribe("game.score", None).unwrap();
    let mut also_on_b = mesh.brokers[1].subscribe("game.*", None).unwrap();
    let mut on_c = mesh.brokers[2].subscribe("game.score", None).unwrap();
    mesh.settle().await;
    mesh.clear();
    let report = mesh.brokers[0]
        .publish("game.score", Bytes::from_static(b"7"))
        .unwrap();
    assert_eq!(report.delivered, 1);
    assert_eq!(report.forwarded, 2);
    mesh.settle().await;
    for subscription in [&mut on_a, &mut on_b, &mut also_on_b, &mut on_c] {
        assert_eq!(subscription.try_recv().unwrap().unwrap().payload, "7");
        assert!(subscription.try_recv().unwrap().is_none());
    }
    // One frame per peer with interest, and nothing travels further.
    let mut targets: Vec<PeerId> = mesh.links[0]
        .forwarded()
        .into_iter()
        .map(|(peer, message)| {
            assert_eq!(message.origin, "a");
            peer
        })
        .collect();
    targets.sort();
    assert_eq!(targets, vec![mesh.names[1].clone(), mesh.names[2].clone()]);
    assert!(mesh.links[1].forwarded().is_empty());
    assert!(mesh.links[2].forwarded().is_empty());
    // No interest, no frame.
    mesh.clear();
    let report = mesh.brokers[1]
        .publish("other.subject", Bytes::new())
        .unwrap();
    assert_eq!(report, PublishReport::default());
    mesh.settle().await;
    assert!(mesh.links[1].sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn withdrawn_interest_stops_forwarding() {
    let mesh = Mesh::new(&["a", "b"]);
    let subscription = mesh.brokers[1].subscribe("game.score", None).unwrap();
    mesh.settle().await;
    assert_eq!(
        mesh.brokers[0]
            .publish("game.score", Bytes::new())
            .unwrap()
            .forwarded,
        1
    );
    drop(subscription);
    mesh.settle().await;
    assert_eq!(
        mesh.brokers[0]
            .publish("game.score", Bytes::new())
            .unwrap()
            .forwarded,
        0
    );
}

#[tokio::test]
async fn queue_groups_deliver_once_and_prefer_local_members() {
    let mesh = Mesh::new(&["a", "b", "c"]);
    let mut on_a = mesh.brokers[0]
        .subscribe("jobs.run", Some("workers"))
        .unwrap();
    let mut on_b = [
        mesh.brokers[1]
            .subscribe("jobs.*", Some("workers"))
            .unwrap(),
        mesh.brokers[1]
            .subscribe("jobs.run", Some("workers"))
            .unwrap(),
    ];
    let mut fanout = mesh.brokers[1].subscribe("jobs.run", None).unwrap();
    mesh.settle().await;
    // A local member wins; only B's plain subscriber needs a copy.
    for _ in 0..10 {
        let report = mesh.brokers[0].publish("jobs.run", Bytes::new()).unwrap();
        assert_eq!(report.delivered, 1);
        assert_eq!(report.forwarded, 1);
    }
    mesh.settle().await;
    for message in mesh.links[0].forwarded() {
        assert!(message.1.queues.is_empty());
    }
    for _ in 0..10 {
        on_a.try_recv().unwrap().unwrap();
        fanout.try_recv().unwrap().unwrap();
    }
    assert!(on_a.try_recv().unwrap().is_none());
    for member in &mut on_b {
        assert!(member.try_recv().unwrap().is_none());
    }
    // From a peer without members, by member count: B has two, A one.
    for _ in 0..30 {
        mesh.brokers[2].publish("jobs.run", Bytes::new()).unwrap();
    }
    mesh.settle().await;
    let mut from_a = 0;
    while on_a.try_recv().unwrap().is_some() {
        from_a += 1;
    }
    let mut from_b = 0;
    for member in &mut on_b {
        let mut count = 0;
        while member.try_recv().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, 10);
        from_b += count;
    }
    assert_eq!((from_a, from_b), (10, 20));
    for _ in 0..30 {
        fanout.try_recv().unwrap().unwrap();
    }
    assert!(fanout.try_recv().unwrap().is_none());
}

#[tokio::test]
async fn requests_are_answered_across_peers_through_prefixed_inboxes() {
    let mesh = Mesh::new(&["a", "b", "c"]);
    let mut service = mesh.brokers[1].subscribe("svc.echo", None).unwrap();
    mesh.settle().await;
    let responder = mesh.brokers[1].clone();
    let task = tokio::spawn(async move {
        let request = service.recv().await.unwrap();
        let reply = request.reply.as_deref().unwrap().to_owned();
        assert!(reply.starts_with("_INBOX.a."), "{reply}");
        responder.publish(&reply, request.payload).unwrap();
    });
    let reply = mesh.brokers[0]
        .request(
            "svc.echo",
            Bytes::from_static(b"hi"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert_eq!(reply.payload, "hi");
    task.await.unwrap();
    assert_eq!(mesh.brokers[0].inbox_prefix(), "_INBOX.a.");
    // A reply goes to the peer its inbox names, without any interest.
    mesh.settle().await;
    mesh.clear();
    let report = mesh.brokers[1]
        .publish("_INBOX.c.nobody-listens", Bytes::new())
        .unwrap();
    assert_eq!(report.forwarded, 1);
    assert_eq!(mesh.links[1].forwarded()[0].0, mesh.names[2]);
    // Inboxes without a peer use interest as before.
    assert_eq!(
        mesh.brokers[1]
            .publish("_INBOX.0123", Bytes::new())
            .unwrap()
            .forwarded,
        0
    );
    // A broker's own prefix never leaves it.
    assert_eq!(
        mesh.brokers[1]
            .publish("_INBOX.b.x", Bytes::new())
            .unwrap()
            .forwarded,
        0
    );
}

#[tokio::test]
async fn no_responders_fails_at_once_and_from_a_peer_whose_subscriber_left() {
    let mesh = Mesh::new(&["a", "b", "c"]);
    let started = tokio::time::Instant::now();
    assert!(matches!(
        mesh.brokers[0]
            .request("svc.none", Bytes::new(), Duration::from_secs(30))
            .await,
        Err(Error::NoResponders)
    ));
    let subscription = mesh.brokers[1].subscribe("svc.gone", None).unwrap();
    mesh.settle().await;
    // The withdrawal is still queued (current-thread runtime) when A
    // forwards the request, so B must answer that nobody listens.
    drop(subscription);
    assert!(matches!(
        mesh.brokers[0]
            .request("svc.gone", Bytes::new(), Duration::from_secs(30))
            .await,
        Err(Error::NoResponders)
    ));
    assert!(started.elapsed() < Duration::from_secs(10));
    mesh.settle().await;
    let refusals = mesh.links[1]
        .sent
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, frame)| matches!(frame, FederationFrame::NoResponders { .. }))
        .count();
    assert_eq!(refusals, 1);
    // A lost link counts as a refusal too.
    let _idle = mesh.brokers[2].subscribe("svc.idle", None).unwrap();
    mesh.settle().await;
    mesh.links[0].set_up(&mesh.names[2], false);
    let request = {
        let broker = mesh.brokers[0].clone();
        tokio::spawn(async move {
            broker
                .request("svc.idle", Bytes::new(), Duration::from_secs(30))
                .await
        })
    };
    // Wait until the request went to C and waits for it.
    for _ in 0..1000 {
        tokio::task::yield_now().await;
        let waiting = mesh.brokers[0]
            .inner
            .pending
            .lock()
            .unwrap()
            .values()
            .any(|request| !request.targets.is_empty());
        if waiting {
            break;
        }
    }
    mesh.brokers[0].peer_down(&mesh.names[2]);
    assert!(matches!(request.await.unwrap(), Err(Error::NoResponders)));
}

#[tokio::test]
async fn scopes_decide_what_crosses_peers() {
    let mesh = Mesh::new(&["a", "b"]);
    // Local subscriptions are never announced and never hear other peers.
    let mut local = mesh.brokers[1]
        .subscribe_with("game.mixed", None, local_scope())
        .unwrap();
    let mut only_local = mesh.brokers[1]
        .subscribe_with("game.local", None, local_scope())
        .unwrap();
    let mut cluster = mesh.brokers[1].subscribe("game.mixed", None).unwrap();
    // In-process and client subscribers to every kind of event.
    let mut plugin_events = mesh.brokers[1].subscribe("rift.events.>", None).unwrap();
    let mut client_players = mesh.brokers[1]
        .subscribe_with("rift.events.players", None, client())
        .unwrap();
    let mut client_instances = mesh.brokers[1]
        .subscribe_with("rift.events.instances", None, client())
        .unwrap();
    let mut everything = mesh.brokers[1].subscribe(">", None).unwrap();
    let mut client_everything = mesh.brokers[1].subscribe_with(">", None, client()).unwrap();
    let mut control = mesh.brokers[1].subscribe("rift.control.*", None).unwrap();
    mesh.settle().await;
    let announced = mesh.links[1].announced();
    for pattern in [
        "game.local",
        "rift.events.>",
        "rift.events.instances",
        "rift.control.*",
    ] {
        assert!(!announced.iter().any(|p| p == pattern), "{pattern}");
    }
    for pattern in ["game.mixed", "rift.events.players", ">"] {
        assert!(announced.iter().any(|p| p == pattern), "{pattern}");
    }
    let a = &mesh.brokers[0];
    assert_eq!(a.publish("game.mixed", Bytes::new()).unwrap().forwarded, 1);
    assert_eq!(a.publish("game.local", Bytes::new()).unwrap().forwarded, 1);
    // Applied and local subjects never travel.
    assert_eq!(
        a.publish("rift.events.instances", Bytes::new())
            .unwrap()
            .forwarded,
        0
    );
    assert_eq!(
        a.publish("rift.control.status", Bytes::new())
            .unwrap()
            .forwarded,
        0
    );
    // Origin events travel for B's clients only.
    let mut own_events = a.subscribe("rift.events.players", None).unwrap();
    assert_eq!(
        a.publish("rift.events.players", Bytes::from_static(b"join"))
            .unwrap()
            .forwarded,
        1
    );
    mesh.settle().await;
    assert!(cluster.try_recv().unwrap().is_some());
    assert!(local.try_recv().unwrap().is_none());
    assert!(only_local.try_recv().unwrap().is_none());
    assert!(plugin_events.try_recv().unwrap().is_none());
    assert!(client_instances.try_recv().unwrap().is_none());
    assert!(control.try_recv().unwrap().is_none());
    assert_eq!(client_players.try_recv().unwrap().unwrap().payload, "join");
    assert!(client_players.try_recv().unwrap().is_none());
    assert_eq!(own_events.try_recv().unwrap().unwrap().payload, "join");
    // The in-process wildcard heard the user subjects; the client wildcard
    // heard those and the origin event.
    let subjects = |subscription: &mut Subscription| {
        let mut subjects = Vec::new();
        while let Some(message) = subscription.try_recv().unwrap() {
            subjects.push(message.subject.to_string());
        }
        subjects
    };
    assert_eq!(subjects(&mut everything), ["game.mixed", "game.local"]);
    assert_eq!(
        subjects(&mut client_everything),
        ["game.mixed", "game.local", "rift.events.players"]
    );
    // B's own events reach all of B's subscribers, as on a single Rift.
    mesh.brokers[1]
        .publish("rift.events.instances", Bytes::new())
        .unwrap();
    assert!(plugin_events.try_recv().unwrap().is_some());
    assert!(client_instances.try_recv().unwrap().is_some());
}

#[tokio::test]
async fn interest_is_resent_after_a_reconnect_and_after_a_gap() {
    let mesh = Mesh::new(&["a", "b"]);
    let (a, b) = (mesh.names[0].clone(), mesh.names[1].clone());
    mesh.settle().await;
    // The link goes down; B's new interest is lost on the way.
    mesh.links[0].set_up(&b, false);
    mesh.links[1].set_up(&a, false);
    mesh.brokers[0].peer_down(&b);
    mesh.brokers[1].peer_down(&a);
    let mut late = mesh.brokers[1].subscribe("game.late", None).unwrap();
    mesh.settle().await;
    assert_eq!(
        mesh.brokers[0]
            .publish("game.late", Bytes::new())
            .unwrap()
            .forwarded,
        0
    );
    mesh.links[0].set_up(&b, true);
    mesh.links[1].set_up(&a, true);
    mesh.brokers[0].peer_up(&b);
    mesh.brokers[1].peer_up(&a);
    mesh.settle().await;
    mesh.brokers[0]
        .publish("game.late", Bytes::from_static(b"1"))
        .unwrap();
    mesh.settle().await;
    assert_eq!(late.try_recv().unwrap().unwrap().payload, "1");
    // One update lost without the link noticing: the next one shows a gap,
    // and A asks for everything again.
    mesh.links[1].set_up(&a, false);
    let mut lost = mesh.brokers[1].subscribe("game.lost", None).unwrap();
    mesh.links[1].set_up(&a, true);
    let mut next = mesh.brokers[1].subscribe("game.next", None).unwrap();
    mesh.settle().await;
    assert!(
        mesh.links[0]
            .sent
            .lock()
            .unwrap()
            .iter()
            .any(|(peer, frame)| { *peer == b && matches!(frame, FederationFrame::Resync) })
    );
    for subject in ["game.lost", "game.next"] {
        assert_eq!(
            mesh.brokers[0]
                .publish(subject, Bytes::new())
                .unwrap()
                .forwarded,
            1,
            "{subject}"
        );
    }
    mesh.settle().await;
    assert!(lost.try_recv().unwrap().is_some());
    assert!(next.try_recv().unwrap().is_some());
}

#[tokio::test]
async fn forwarded_messages_keep_their_publisher() {
    let mesh = Mesh::new(&["a", "b"]);
    let mut subscription = mesh.brokers[1].subscribe("game.sender", None).unwrap();
    mesh.settle().await;
    mesh.brokers[0]
        .publish_as("game.sender", None, Bytes::new(), Some("server.lobby-1"))
        .unwrap();
    mesh.brokers[0]
        .publish("game.sender", Bytes::new())
        .unwrap();
    mesh.settle().await;
    let message = subscription.try_recv().unwrap().unwrap();
    assert_eq!(message.sender.as_deref(), Some("server.lobby-1"));
    // Rift's own publishers stay Rift's own on every peer.
    assert_eq!(subscription.try_recv().unwrap().unwrap().sender, None);
}

#[tokio::test]
async fn owner_subjects_follow_the_router() {
    struct ToPeer(PeerId);
    impl OwnerRouter for ToPeer {
        fn route(&self, _subject: &str) -> Option<PeerId> {
            Some(self.0.clone())
        }
    }
    let mesh = Mesh::new(&["a", "b", "c"]);
    let mut agent = mesh.brokers[1]
        .subscribe_with("rift.node.n1.cmd", None, client())
        .unwrap();
    let mut elsewhere = mesh.brokers[2].subscribe("rift.node.*.cmd", None).unwrap();
    mesh.settle().await;
    // Without a router, owner subjects stay where they are published.
    assert_eq!(
        mesh.brokers[0]
            .publish("rift.node.n1.cmd", Bytes::new())
            .unwrap()
            .forwarded,
        0
    );
    mesh.brokers[0].set_owner_router(Arc::new(ToPeer(mesh.names[1].clone())));
    let responder = mesh.brokers[1].clone();
    let task = tokio::spawn(async move {
        let command = agent.recv().await.unwrap();
        responder
            .publish(command.reply.as_deref().unwrap(), Bytes::from_static(b"ok"))
            .unwrap();
    });
    let reply = mesh.brokers[0]
        .request("rift.node.n1.cmd", Bytes::new(), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(reply.payload, "ok");
    task.await.unwrap();
    mesh.settle().await;
    assert!(elsewhere.try_recv().unwrap().is_none());
}

#[tokio::test]
async fn without_a_federation_nothing_changes() {
    let broker = Broker::default();
    assert_eq!(broker.inbox_prefix(), "_INBOX.");
    let mut subscription = broker.subscribe("game.>", None).unwrap();
    let report = broker.publish("game.x", Bytes::new()).unwrap();
    assert_eq!(
        report,
        PublishReport {
            delivered: 1,
            slow_consumers: 0,
            forwarded: 0,
        }
    );
    subscription.try_recv().unwrap().unwrap();
    // A frame arriving without a federation is ignored.
    broker.handle_federation_frame(
        &Arc::from("ghost"),
        FederationFrame::Message(ForwardedMessage {
            subject: "game.x".into(),
            reply: None,
            payload: Bytes::new(),
            sender: None,
            origin: "ghost".into(),
            queues: Vec::new(),
        }),
    );
    assert!(subscription.try_recv().unwrap().is_none());
}
