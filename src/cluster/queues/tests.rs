//! Network-wide queues, plugin leases and job scopes on simulated peers, on
//! the paused clock. Each peer has its own extension host with `queues =
//! { game = N }`; players are extension sessions of the peer they joined.
use super::*;
use crate::{
    auth::AuthenticatedProfile,
    cluster::{
        sim::{Sim, until},
        state::EntryKind,
    },
    config::Config,
    extensions::{Context, ExtensionSession, Extensions},
    messaging::Broker,
};
use rift_permissions::{Actor, Change, Node, SubjectRef, Uuid};
use std::sync::atomic::{AtomicU64, Ordering};

const WAIT: Duration = Duration::from_secs(30);

fn runtime(capacity: usize) -> Extensions {
    let source = format!(
        "return {{ listeners = {{ public = '127.0.0.1:0' }}, backends = {{ lobby = '127.0.0.1:25001', game = '127.0.0.1:25002' }}, routes = {{ public = 'lobby' }}, authentication = {{ online_mode = true }}, forwarding = {{ mode = 'velocity', secret_env = 'TEST_SECRET' }}, extensions = {{ api_version = 1, queues = {{ game = {capacity} }} }} }}"
    );
    let config = Config::from_lua(&source, "queue-cluster.lua").unwrap();
    Extensions::new(&config, Broker::new(Default::default()).unwrap(), None).unwrap()
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

fn player(id: u64) -> SubjectRef {
    SubjectRef::User(Uuid::from_bytes([id as u8; 16]))
}

struct Net {
    sim: Sim,
    runtimes: Vec<Extensions>,
    attached: Vec<Option<Attached>>,
}

impl Net {
    /// `count` peers (`voters` of them vote) with the lease component, each
    /// with an extension host whose `game` queue has `capacity` slots,
    /// attached to its peer.
    async fn form(count: usize, voters: usize, capacity: usize) -> Self {
        let mut sim = Sim::new(count);
        sim.components = Some(Arc::new(|_| components()));
        sim.form(voters).await;
        for index in 0..count {
            let peer = sim.peer(index).clone();
            until(WAIT, move || {
                peer.is_enabled(EntryKind::Lease).unwrap_or(false)
            })
            .await;
        }
        let mut net = Self {
            sim,
            runtimes: (0..count).map(|_| runtime(capacity)).collect(),
            attached: (0..count).map(|_| None).collect(),
        };
        for index in 0..count {
            net.attach(index);
        }
        net
    }

    fn attach(&mut self, index: usize) {
        self.attached[index] = Some(attach(
            self.sim.peer(index),
            self.runtimes[index].queue_host(),
        ));
    }

    /// Stop peer `index` without a goodbye, its queue link first.
    async fn crash(&mut self, index: usize) {
        drop(self.attached[index].take());
        self.sim.crash(index).await;
    }

    fn session(&self, index: usize, id: u64) -> ExtensionSession {
        self.runtimes[index].session(context(id)).unwrap()
    }

    /// The `game` queue as the leader last told peer `index`.
    fn game(&self, index: usize) -> Option<DestinationView> {
        self.runtimes[index]
            .queue_host()
            .view()?
            .destinations
            .get("game")
            .cloned()
    }

    /// Wait until peer `index`'s view of `game` satisfies `condition`.
    async fn sees(&self, index: usize, condition: impl Fn(&DestinationView) -> bool) {
        until(WAIT, || {
            self.game(index).is_some_and(|view| condition(&view))
        })
        .await;
    }

    fn set_meta(&self, index: usize, id: u64, key: &str, value: &str) {
        let change = Change::SetNode {
            subject: player(id),
            node: Node::meta(key, value).unwrap(),
        };
        self.runtimes[index]
            .permissions()
            .apply(Actor::Cli, change)
            .unwrap();
    }

    fn grant(&self, index: usize, id: u64, node: &str) {
        let change = Change::SetNode {
            subject: player(id),
            node: Node::permission(node, true).unwrap(),
        };
        self.runtimes[index]
            .permissions()
            .apply(Actor::Cli, change)
            .unwrap();
    }

    async fn shutdown(mut self) {
        for attached in &mut self.attached {
            drop(attached.take());
        }
        self.sim.shutdown().await;
    }
}

fn head(peer: &str, id: u64) -> Option<TicketRef> {
    Some(TicketRef {
        peer: peer.to_owned(),
        id,
    })
}

#[tokio::test(start_paused = true)]
async fn players_on_three_peers_queue_and_enter_in_network_order() {
    let mut net = Net::form(3, 3, 2).await;
    let leader = net.sim.leader().await;
    let (a, b) = ((leader + 1) % 3, (leader + 2) % 3);
    let (name_a, name_b) = (net.sim.name(a), net.sim.name(b));

    // Two players on two peers take both slots of game.
    let mut occupant_a = net.session(a, 1);
    occupant_a.reserve("game").unwrap();
    occupant_a.ready("game").await;
    let mut occupant_b = net.session(b, 2);
    occupant_b.reserve("game").unwrap();
    occupant_b.ready("game").await;
    // Capacity counts network-wide: the third peer has no player there,
    // yet it is full.
    net.sees(leader, |view| view.others_used == 2).await;
    let direct = net.session(leader, 3);
    assert!(direct.reserve("game").is_err());

    // Tickets arrive on a, b and the leader, in that order; positions
    // count every peer's tickets.
    let mut first = net.session(a, 10);
    assert_eq!(first.enqueue("game").unwrap(), 1);
    net.sees(a, |view| view.mine.contains_key(&10)).await;
    net.sees(b, |view| view.priorities.len() == 1).await;
    let mut second = net.session(b, 11);
    assert_eq!(second.enqueue("game").unwrap(), 2);
    net.sees(b, |view| view.mine.contains_key(&11)).await;
    net.sees(leader, |view| view.priorities.len() == 2).await;
    let third = net.session(leader, 12);
    assert_eq!(third.enqueue("game").unwrap(), 3);
    net.sees(leader, |view| view.mine.contains_key(&12)).await;
    // A higher priority goes in front, wherever it joined.
    net.set_meta(b, 13, "queue_priority", "5");
    let mut vip = net.session(b, 13);
    assert_eq!(vip.enqueue("game").unwrap(), 1);
    net.sees(a, |view| view.mine.get(&10) == Some(&2)).await;
    net.sees(b, |view| {
        view.mine.get(&13) == Some(&1) && view.mine.get(&11) == Some(&3)
    })
    .await;
    net.sees(leader, |view| view.mine.get(&12) == Some(&4))
        .await;
    assert_eq!(first.enqueue("game").unwrap(), 2);
    assert_eq!(second.enqueue("game").unwrap(), 3);
    assert_eq!(third.enqueue("game").unwrap(), 4);
    assert_eq!(net.runtimes[leader].queue_lengths()["game"], 4);

    // A slot frees on a: the head, on b, may enter; nobody else.
    drop(occupant_a);
    until(WAIT, || {
        vip.queued_target().unwrap().as_deref() == Some("game")
    })
    .await;
    assert_eq!(first.queued_target().unwrap(), None);
    assert!(first.reserve("game").is_err());
    vip.reserve("game").unwrap();
    vip.ready("game").await;
    net.sees(a, |view| {
        view.head == head(&name_a, 10) && view.others_used == 2
    })
    .await;
    assert_eq!(first.queued_target().unwrap(), None);
    assert!(first.reserve("game").is_err());

    // Bypass enters although the destination is full and others wait.
    net.grant(leader, 14, "rift.queue.bypass");
    let bypasser = net.session(leader, 14);
    bypasser.reserve("game").unwrap();
    bypasser.release("game");

    // The next slot goes to the earliest arrival, on a, then on b.
    drop(occupant_b);
    until(WAIT, || {
        first.queued_target().unwrap().as_deref() == Some("game")
    })
    .await;
    first.reserve("game").unwrap();
    first.ready("game").await;
    drop(vip);
    net.sees(b, |view| view.head == head(&name_b, 11)).await;
    until(WAIT, || {
        second.queued_target().unwrap().as_deref() == Some("game")
    })
    .await;
    assert_eq!(third.queued_target().unwrap(), None);
    second.reserve("game").unwrap();
    second.ready("game").await;
    net.sees(leader, |view| view.priorities.len() == 1).await;
    drop((first, second, third, direct, bypasser));
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_new_leader_rebuilds_the_order_from_resubmitted_tickets() {
    let mut net = Net::form(3, 3, 1).await;
    let leader = net.sim.leader().await;
    let (a, b) = ((leader + 1) % 3, (leader + 2) % 3);
    let (name_a, name_b) = (net.sim.name(a), net.sim.name(b));
    let mut occupant = net.session(a, 1);
    occupant.reserve("game").unwrap();
    occupant.ready("game").await;
    // Arrivals alternate between the peers: a, b, a, then the leader.
    let x = net.session(a, 20);
    x.enqueue("game").unwrap();
    net.sees(a, |view| view.mine.contains_key(&20)).await;
    let y = net.session(b, 21);
    y.enqueue("game").unwrap();
    net.sees(b, |view| view.mine.contains_key(&21)).await;
    let z = net.session(a, 22);
    z.enqueue("game").unwrap();
    net.sees(a, |view| view.mine.get(&22) == Some(&3)).await;
    let w = net.session(leader, 23);
    w.enqueue("game").unwrap();
    net.sees(leader, |view| view.mine.get(&23) == Some(&4))
        .await;

    net.crash(leader).await;
    drop(w);
    let new = net.sim.leader().await;
    assert_ne!(new, leader);
    // The old leader's ticket is gone with it; the others keep their order
    // although a submits both of its tickets at once.
    net.sees(a, |view| {
        view.priorities.len() == 3
            && view.mine.get(&20) == Some(&1)
            && view.mine.get(&22) == Some(&3)
    })
    .await;
    net.sees(b, |view| {
        view.mine.get(&21) == Some(&2) && view.head == head(&name_a, 20)
    })
    .await;
    assert_eq!(net.runtimes[new].queue_lengths()["game"], 3);
    // Admission goes on under the new leader, in the same order.
    drop(occupant);
    until(WAIT, || {
        x.queued_target().unwrap().as_deref() == Some("game")
    })
    .await;
    assert_eq!(y.queued_target().unwrap(), None);
    drop(x);
    net.sees(b, |view| view.head == head(&name_b, 21)).await;
    until(WAIT, || {
        y.queued_target().unwrap().as_deref() == Some("game")
    })
    .await;
    drop((y, z));
    net.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_lost_peer_takes_its_tickets_and_slots_along() {
    // Three voters and a learner, which can go without costing the quorum.
    let mut net = Net::form(4, 3, 1).await;
    let leader = net.sim.leader().await;
    let learner = 3;
    let name = net.sim.name(leader);
    let mut occupant = net.session(learner, 1);
    occupant.reserve("game").unwrap();
    occupant.ready("game").await;
    let lost = net.session(learner, 40);
    lost.enqueue("game").unwrap();
    net.sees(learner, |view| view.mine.contains_key(&40)).await;
    let waiting = net.session(leader, 41);
    waiting.enqueue("game").unwrap();
    net.sees(leader, |view| {
        view.mine.get(&41) == Some(&2) && view.others_used == 1
    })
    .await;
    assert_eq!(waiting.queued_target().unwrap(), None);

    net.crash(learner).await;
    drop((occupant, lost));
    // After the peer timeout the leader drops the learner's ticket and its
    // slot: the leader's player is first and the slot is free.
    net.sees(leader, |view| {
        view.head == head(&name, 41) && view.others_used == 0 && view.priorities.len() == 1
    })
    .await;
    until(WAIT, || {
        waiting.queued_target().unwrap().as_deref() == Some("game")
    })
    .await;
    drop(waiting);
    net.shutdown().await;
}

/// Wait until every running peer of `sim` joined with its incarnation.
async fn joined(sim: &Sim) {
    for (_, peer) in sim.running() {
        let peer = peer.clone();
        until(WAIT, move || {
            peer.state().peers().unwrap_or_default().iter().any(|row| {
                row.name == peer.name() && row.incarnation == peer.incarnation() && !row.expired
            })
        })
        .await;
    }
}

/// Ask until `cluster` gets the lease (another peer may still hold it).
async fn acquire_eventually(cluster: &Cluster, owner: &str, name: &str) -> u64 {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(Some(token)) = leases::acquire(cluster, owner, name, 60_000).await {
            return token;
        }
        assert!(
            Instant::now() < deadline,
            "{} never got lease {name}",
            cluster.name()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn one_peer_holds_a_lease_and_its_token_only_grows() {
    let mut sim = Sim::new(4);
    sim.components = Some(Arc::new(|_| components()));
    sim.form(3).await;
    for (_, peer) in sim.running() {
        let peer = peer.clone();
        until(WAIT, move || {
            peer.is_enabled(EntryKind::Lease).unwrap_or(false)
        })
        .await;
    }
    joined(&sim).await;
    let owner = leases::plugin_owner("sweeper");
    let leader = sim.leader().await;
    let follower = (0..3).find(|index| *index != leader).unwrap();
    let learner = 3;

    // Two peers race; one wins.
    let (one, two) = tokio::join!(
        leases::acquire(sim.peer(follower), &owner, "sweep", 60_000),
        leases::acquire(sim.peer(learner), &owner, "sweep", 60_000),
    );
    let (one, two) = (one.unwrap(), two.unwrap());
    assert!(one.is_some() != two.is_some(), "{one:?} {two:?}");
    // Make the learner the holder.
    let mut token = match (one, two) {
        (Some(token), None) => {
            assert!(
                !leases::renew(sim.peer(learner), &owner, "sweep", token, 60_000)
                    .await
                    .unwrap()
            );
            assert!(
                leases::release(sim.peer(follower), &owner, "sweep", token)
                    .await
                    .unwrap()
            );
            let next = acquire_eventually(sim.peer(learner), &owner, "sweep").await;
            assert!(next > token);
            next
        }
        (None, Some(token)) => token,
        _ => unreachable!(),
    };
    // Asking again keeps the token; another plugin's lease of that name is
    // another lease.
    assert_eq!(
        leases::acquire(sim.peer(learner), &owner, "sweep", 60_000)
            .await
            .unwrap(),
        Some(token)
    );
    assert!(
        leases::acquire(
            sim.peer(follower),
            &leases::plugin_owner("other"),
            "sweep",
            60_000
        )
        .await
        .unwrap()
        .is_some()
    );

    // A leader change keeps the holder and its token.
    sim.crash(leader).await;
    let new = sim.leader().await;
    assert_ne!(new, leader);
    assert_eq!(
        leases::acquire(sim.peer(learner), &owner, "sweep", 60_000)
            .await
            .unwrap(),
        Some(token)
    );
    assert_eq!(
        leases::acquire(sim.peer(follower), &owner, "sweep", 60_000)
            .await
            .unwrap(),
        None
    );
    assert!(
        leases::renew(sim.peer(learner), &owner, "sweep", token, 60_000)
            .await
            .unwrap()
    );

    // The holder's peer goes away: once the leader expired it, the lease is
    // free and the next grant has a higher token.
    sim.crash(learner).await;
    let taken = acquire_eventually(sim.peer(follower), &owner, "sweep").await;
    assert!(taken > token);
    let old = token;
    token = taken;
    // The deposed holder comes back: its renew fails and it cannot take
    // the lease while the follower holds it.
    sim.start(learner).await.unwrap();
    joined(&sim).await;
    assert!(
        !leases::renew(sim.peer(learner), &owner, "sweep", old, 60_000)
            .await
            .unwrap()
    );
    assert_eq!(
        leases::acquire(sim.peer(learner), &owner, "sweep", 60_000)
            .await
            .unwrap(),
        None
    );
    assert!(
        leases::renew(sim.peer(follower), &owner, "sweep", token, 60_000)
            .await
            .unwrap()
    );
    sim.shutdown().await;
}

/// Count the ticks of a job with `scope` on each running peer of `sim`.
fn tick_counters(sim: &Sim, scope: jobs::Scope) -> (Vec<Arc<AtomicU64>>, Vec<JoinHandle<()>>) {
    let mut counters = Vec::new();
    let mut tasks = Vec::new();
    for index in 0..3 {
        let counter = Arc::new(AtomicU64::new(0));
        counters.push(counter.clone());
        let cluster = sim.peer(index).clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if jobs::due_on(Some(&cluster), scope) {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    (counters, tasks)
}

fn counts(counters: &[Arc<AtomicU64>]) -> Vec<u64> {
    counters
        .iter()
        .map(|counter| counter.swap(0, Ordering::Relaxed))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_cluster_job_runs_on_the_leader_only_and_a_peer_job_everywhere() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;

    let (everywhere, peer_tasks) = tick_counters(&sim, jobs::Scope::Peer);
    let (once, cluster_tasks) = tick_counters(&sim, jobs::Scope::Cluster);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(counts(&everywhere).iter().all(|ticks| *ticks > 0));
    for (index, ticks) in counts(&once).into_iter().enumerate() {
        assert_eq!(ticks > 0, index == leader, "peer {index}: {ticks}");
    }
    for task in peer_tasks.into_iter().chain(cluster_tasks) {
        task.abort();
    }

    // Leadership moves: so does the job.
    sim.crash(leader).await;
    let new = sim.leader().await;
    assert_ne!(new, leader);
    let mut counters = Vec::new();
    let mut tasks = Vec::new();
    for index in (0..3).filter(|index| *index != leader) {
        let counter = Arc::new(AtomicU64::new(0));
        counters.push((index, counter.clone()));
        let cluster = sim.peer(index).clone();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if jobs::due_on(Some(&cluster), jobs::Scope::Cluster) {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    for (index, counter) in &counters {
        let ticks = counter.load(Ordering::Relaxed);
        assert_eq!(ticks > 0, *index == new, "peer {index}: {ticks}");
    }
    for task in tasks {
        task.abort();
    }
    sim.shutdown().await;
}
