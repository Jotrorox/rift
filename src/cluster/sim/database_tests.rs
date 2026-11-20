//! The replicated instance database on three simulated peers, with the real
//! stores: writes from any peer, hooks and events on every peer, group
//! commit and rollback, idempotent retries, the hot-path entry kinds,
//! snapshot installs and checksums.
//!
//! The stores' synchronous APIs block their thread until the write is
//! applied, so the tests call them on a thread of their own
//! ([`off_thread`]) while the paused runtime keeps the cluster going.
use super::{Sim, until};
use crate::{
    cluster::{operations::Request, state::Origin},
    config::{GroupMode, GroupState, PermissionsConfig},
    database::{Database, Operation, Outcome, params},
    directory::{Directory, DirectoryOp},
    extensions::storage::{self, KvEntry, KvOp, KvOutcome, Quota, Quotas, StorageConfig, Target},
    instance_store::{InstanceOp, InstanceOutcome, Store},
    permissions::{
        Authority, Notification,
        ops::{PermBody, PermOp, PermWrite},
    },
};
use rift_permissions::{Actor, Change as PermChange, Limits, Node, SubjectRef};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

/// The time the operations of these tests carry.
const NOW: u64 = 1_700_000_000;

/// Run `work` on a thread outside the runtime and wait for it.
async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver.await.expect("the thread finished")
}

fn authority(sim: &Sim, index: usize) -> Arc<Authority> {
    Arc::new(Authority::open_at(sim.database(index), &PermissionsConfig::default(), NOW).unwrap())
}

fn write(change: PermChange) -> Operation {
    Operation::Permissions(PermOp::Write(Box::new(PermWrite {
        actor: "cli".into(),
        reason: None,
        now: NOW,
        limits: Limits::default(),
        body: PermBody::Change { change },
    })))
}

fn create_group(name: &str, weight: i32) -> Operation {
    write(PermChange::CreateGroup {
        name: name.into(),
        weight,
        display_name: None,
    })
}

fn allocate(group: &str) -> Operation {
    Operation::Instances(InstanceOp::AllocateName {
        group: group.into(),
        floor: 0,
        taken: Vec::new(),
        taken_nocase: Vec::new(),
    })
}

fn begin(name: &str, port: u16) -> Operation {
    Operation::Instances(InstanceOp::BeginCreation {
        name: name.into(),
        node: "n1".into(),
        port,
        definition: "{}".into(),
    })
}

/// One integer a query returns on `database`'s replica.
fn number(database: &Database, sql: &str) -> i64 {
    database
        .read()
        .query_row(sql, params![], |row| row.get(0))
        .unwrap()
}

fn groups_named(database: &Database, name: &str) -> i64 {
    database
        .read()
        .query_row(
            "SELECT count(*) FROM perm_subject WHERE kind = 1 AND key = ?1",
            params![name],
            |row| row.get(0),
        )
        .unwrap()
}

/// Every running peer has applied everything committed so far.
async fn settle(sim: &Sim) {
    for (_, peer) in sim.running() {
        peer.read_barrier().await.unwrap();
    }
}

/// Every running peer's checksum, which must agree, and no peer diverged.
fn assert_identical(sim: &Sim) {
    let sums: BTreeSet<String> = sim
        .running()
        .map(|(index, peer)| {
            assert!(!peer.state().is_diverged(), "p{index} diverged");
            peer.state().checksum_now().unwrap()
        })
        .collect();
    assert_eq!(sums.len(), 1, "checksums differ: {sums:?}");
}

#[tokio::test(start_paused = true)]
async fn a_follower_write_reaches_every_peer_through_its_hooks() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    let follower = (0..3).find(|index| *index != leader).unwrap();
    let authorities: Vec<Arc<Authority>> = (0..3).map(|index| authority(&sim, index)).collect();
    let mut events: Vec<_> = authorities.iter().map(|a| a.subscribe()).collect();
    let stores: Vec<Arc<Store>> = (0..3)
        .map(|index| Arc::new(Store::new(sim.database(index)).unwrap()))
        .collect();
    let mirrors: Vec<Arc<Mutex<BTreeMap<String, GroupState>>>> = (0..3)
        .map(|_| Arc::new(Mutex::new(BTreeMap::new())))
        .collect();
    for (store, mirror) in stores.iter().zip(&mirrors) {
        let mirror = mirror.clone();
        store
            .watch_states(move |states| *mirror.lock().unwrap() = states, |_| {})
            .unwrap();
    }
    let created: Vec<Arc<AtomicUsize>> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    for (index, counter) in created.iter().enumerate() {
        let counter = counter.clone();
        sim.database(index)
            .on_change(&["instances"], move |_, change| {
                if !change.reload {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            });
    }

    // A permission change on a follower.
    let writer = authorities[follower].clone();
    off_thread(move || {
        writer
            .apply(
                Actor::Cli,
                PermChange::CreateGroup {
                    name: "vip".into(),
                    weight: 5,
                    display_name: None,
                },
            )
            .unwrap()
    })
    .await;
    assert_eq!(
        authorities[follower].group("vip").unwrap().weight,
        5,
        "the writer reads its own write"
    );
    for authority in &authorities {
        let authority = authority.clone();
        until(Duration::from_secs(5), move || {
            authority.group("vip").is_some()
        })
        .await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    for receiver in &mut events {
        match receiver.try_recv().unwrap() {
            Notification::Committed { event, .. } => assert_eq!(event.revision, 1),
            other => panic!("{other:?}"),
        }
        assert!(receiver.try_recv().is_err(), "one event per peer");
    }

    // A group state, mirrored on every peer.
    let state = GroupState {
        suspended: true,
        mode: GroupMode::Dev,
    };
    let store = stores[follower].clone();
    off_thread(move || store.set_group_state("games", state))
        .await
        .unwrap();
    for mirror in &mirrors {
        let mirror = mirror.clone();
        until(Duration::from_secs(5), move || {
            mirror.lock().unwrap().get("games") == Some(&state)
        })
        .await;
    }

    // An instance creation.
    let outcome = sim
        .peer(follower)
        .run_operation(begin("games-1", 30001))
        .await
        .unwrap();
    assert!(!outcome.failed(), "{outcome:?}");
    for counter in &created {
        let counter = counter.clone();
        until(Duration::from_secs(5), move || {
            counter.load(Ordering::SeqCst) == 1
        })
        .await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    for (index, counter) in created.iter().enumerate() {
        assert_eq!(counter.load(Ordering::SeqCst), 1, "p{index}: hook ran once");
        assert_eq!(
            number(&sim.database(index), "SELECT count(*) FROM instances"),
            1
        );
    }
    assert_identical(&sim);
    drop((stores, authorities));
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn concurrent_writers_on_every_peer_are_serialized() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let mut tasks = Vec::new();
    for index in 0..3 {
        for _ in 0..8 {
            let peer = sim.peer(index).clone();
            tasks.push(tokio::spawn(async move {
                peer.run_operation(allocate("games")).await
            }));
        }
    }
    let mut names = BTreeSet::new();
    for task in tasks {
        match task.await.unwrap().unwrap() {
            Outcome::Instances(InstanceOutcome::Name(name)) => {
                assert!(names.insert(name.clone()), "{name} allocated twice")
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(names.len(), 24);
    // Every peer tries every port; UNIQUE(node, port) lets one win each.
    let mut tasks = Vec::new();
    for index in 0..3 {
        for port in 0..4u16 {
            let peer = sim.peer(index).clone();
            let name = format!("p{index}-s{port}");
            tasks.push(tokio::spawn(async move {
                peer.run_operation(begin(&name, 30000 + port)).await
            }));
        }
    }
    let mut created = 0;
    for task in tasks {
        if !task.await.unwrap().unwrap().failed() {
            created += 1;
        }
    }
    assert_eq!(created, 4);
    settle(&sim).await;
    for index in 0..3 {
        let database = sim.database(index);
        assert_eq!(number(&database, "SELECT count(*) FROM instances"), 4);
        assert_eq!(
            number(
                &database,
                "SELECT count(DISTINCT port) FROM instances WHERE node = 'n1'"
            ),
            4
        );
    }
    assert_identical(&sim);
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_failing_operation_rolls_back_alone_inside_a_batch() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    let cluster = sim.peer(leader).clone();
    // Queued together, so the leader takes them into one entry.
    let (first, second, third) = tokio::join!(
        cluster.run_operation(create_group("vip", 5)),
        cluster.run_operation(create_group("vip", 0)),
        cluster.run_operation(Operation::Instances(InstanceOp::SetGroupState {
            group: "games".into(),
            suspended: true,
            mode: "active".into(),
        })),
    );
    assert!(!first.unwrap().failed());
    assert!(second.unwrap().failed(), "the group exists already");
    assert!(!third.unwrap().failed());
    settle(&sim).await;
    for index in 0..3 {
        let database = sim.database(index);
        assert_eq!(groups_named(&database, "vip"), 1);
        assert_eq!(
            number(
                &database,
                "SELECT weight FROM perm_subject WHERE kind = 1 AND key = 'vip'"
            ),
            5
        );
        assert_eq!(number(&database, "SELECT count(*) FROM group_states"), 1);
        assert_eq!(number(&database, "SELECT count(*) FROM perm_change"), 1);
    }
    assert_identical(&sim);
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_retry_after_a_leader_change_returns_the_stored_outcome() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let old = sim.leader().await;
    let follower = (0..3).find(|index| *index != old).unwrap();
    let origin: Origin = sim.peer(follower).next_origin();
    let request = Request {
        origin,
        term: None,
        operation: allocate("games"),
    };
    let first = sim
        .peer(follower)
        .submit_operation(request.clone(), Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(
        first,
        Outcome::Instances(InstanceOutcome::Name("games-1".into()))
    );
    sim.crash(old).await;
    let new = sim.leader().await;
    assert_ne!(new, old);
    let again = sim
        .peer(follower)
        .submit_operation(request, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(again, first, "the stored outcome, not a second allocation");
    let next = sim
        .peer(follower)
        .run_operation(allocate("games"))
        .await
        .unwrap();
    assert_eq!(
        next,
        Outcome::Instances(InstanceOutcome::Name("games-2".into()))
    );
    // An operation decided in an old term is refused.
    let refused = sim
        .peer(new)
        .run_in_term(allocate("games"), 1)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("term"), "{refused}");
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn kv_mutations_and_directory_upserts_replicate() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    let follower = (0..3).find(|index| *index != leader).unwrap();
    let quotas = Quotas {
        default: Quota {
            entries: 2,
            bytes: 4096,
        },
        plugins: BTreeMap::new(),
    };
    let config = StorageConfig {
        target: Target::Database,
        quotas: quotas.clone(),
    };
    let stores: Vec<storage::Store> = (0..3)
        .map(|index| storage::Store::open(Some(&config), Some(&sim.database(index))).unwrap())
        .collect();
    let deadline = || std::time::Instant::now() + Duration::from_secs(30);
    let key = |key: &str| ("game".to_owned(), key.to_owned());
    let writer = stores[follower].clone();
    off_thread(move || {
        writer.mutate(deadline(), |entries| {
            entries.insert(key("a"), b"1".to_vec());
            entries.insert(key("b"), b"2".to_vec());
            Ok(())
        })
    })
    .await
    .unwrap();
    for store in &stores {
        let store = store.clone();
        until(Duration::from_secs(5), move || {
            store.get("game".into(), "b".into(), deadline()).unwrap() == Some(b"2".to_vec())
        })
        .await;
    }
    // The quota is checked when the entry applies, on every peer alike.
    let rejected = sim
        .peer(follower)
        .run_operation(Operation::Kv(KvOp::Mutate {
            quotas,
            expect: Vec::new(),
            changes: vec![KvEntry {
                key: key("c"),
                value: Some(b"3".to_vec()),
            }],
        }))
        .await
        .unwrap();
    assert!(
        matches!(rejected, Outcome::Kv(KvOutcome::Rejected { .. })),
        "{rejected:?}"
    );
    // Player directory upserts.
    let directories: Vec<Directory> = (0..3)
        .map(|index| Directory::open(sim.database(index)).unwrap())
        .collect();
    let uuid = "0123456789abcdef0123456789abcdef";
    let writer = directories[follower].clone();
    off_thread(move || writer.joined(uuid, "Alex", Some("203.0.113.7"), "lobby")).await;
    for directory in &directories {
        let directory = directory.clone();
        until(Duration::from_secs(5), move || {
            directory
                .lookup(uuid)
                .unwrap()
                .is_some_and(|record| record.last_server == "lobby")
        })
        .await;
    }
    let outcome = sim
        .peer(leader)
        .run_operation(Operation::Directory(DirectoryOp::Left {
            uuid: uuid.into(),
            server: "games-1".into(),
            now: NOW,
        }))
        .await
        .unwrap();
    assert!(!outcome.failed(), "{outcome:?}");
    settle(&sim).await;
    for directory in &directories {
        assert_eq!(
            directory.lookup("alex").unwrap().unwrap().last_server,
            "games-1"
        );
    }
    assert_identical(&sim);
    drop((stores, directories));
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_new_peer_installs_a_snapshot_and_its_mirrors_reload() {
    let mut sim = Sim::new(4);
    sim.init(0).await;
    for index in 1..3 {
        sim.start(index).await.unwrap();
        sim.admin(0, &["add", &sim.name(index), &sim.address(index)])
            .await
            .unwrap();
        sim.retry_admin(0, &["promote", &sim.name(index)]).await;
    }
    let leader = sim.leader().await;
    sim.peer(leader)
        .run_operation(create_group("vip", 5))
        .await
        .unwrap();
    sim.start(3).await.unwrap();
    let reloads = Arc::new(AtomicUsize::new(0));
    let counted = reloads.clone();
    sim.database(3)
        .on_change(&["perm_subject"], move |_, change| {
            if change.reload {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        });
    sim.retry_admin(leader, &["add", "p3", "p3:7444"]).await;
    let late = sim.database(3);
    until(Duration::from_secs(20), || {
        reloads.load(Ordering::SeqCst) >= 1
    })
    .await;
    assert_eq!(groups_named(&late, "vip"), 1);
    assert!(sim.peer(3).metrics().unwrap().snapshot.is_some());
    let mirror = authority(&sim, 3);
    assert_eq!(mirror.group("vip").unwrap().weight, 5);
    // Later entries reach it through its hook.
    sim.peer(leader)
        .run_operation(create_group("staff", 1))
        .await
        .unwrap();
    let watched = mirror.clone();
    until(Duration::from_secs(5), move || {
        watched.group("staff").is_some()
    })
    .await;
    settle(&sim).await;
    assert_identical(&sim);
    drop(mirror);
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn replicas_have_identical_checksums_after_a_random_workload() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let mut seed: u64 = 0x5eed;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        seed >> 33
    };
    let mut tasks = Vec::new();
    for step in 0..60u64 {
        let peer = sim.peer((next() % 3) as usize).clone();
        let operation = match next() % 5 {
            0 => allocate(&format!("g{}", next() % 3)),
            1 => create_group(&format!("group{}", next() % 6), (next() % 10) as i32),
            2 => Operation::Kv(KvOp::Mutate {
                quotas: Quotas::default(),
                expect: Vec::new(),
                changes: vec![KvEntry {
                    key: ("game".into(), format!("k{}", next() % 8)),
                    value: Some(step.to_string().into_bytes()),
                }],
            }),
            3 => Operation::Directory(DirectoryOp::Joined {
                uuid: format!("{:032x}", next() % 5),
                name: format!("player{step}"),
                ip: None,
                server: "lobby".into(),
                now: NOW + step,
            }),
            _ => begin(&format!("s{step}"), 31000 + (next() % 10) as u16),
        };
        tasks.push(tokio::spawn(
            async move { peer.run_operation(operation).await },
        ));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    settle(&sim).await;
    assert_identical(&sim);
    sim.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_cascade_delete_replicates_without_divergence() {
    let mut sim = Sim::new(3);
    sim.form(3).await;
    let leader = sim.leader().await;
    let follower = (0..3).find(|index| *index != leader).unwrap();
    let cluster = sim.peer(leader).clone();
    assert!(
        !cluster
            .run_operation(create_group("staff", 3))
            .await
            .unwrap()
            .failed()
    );
    for permission in ["rift.kick", "rift.ban"] {
        let outcome = cluster
            .run_operation(write(PermChange::SetNode {
                subject: SubjectRef::Group("staff".into()),
                node: Node::permission(permission, true).unwrap(),
            }))
            .await
            .unwrap();
        assert!(!outcome.failed(), "{outcome:?}");
    }
    settle(&sim).await;
    let nodes = "SELECT count(*) FROM perm_node WHERE subject_id IN
                 (SELECT id FROM perm_subject WHERE kind = 1 AND key = 'staff')";
    for index in 0..3 {
        assert!(number(&sim.database(index), nodes) >= 2);
    }
    // Deleting the group cascades to its nodes; the changeset carries them.
    let outcome = sim
        .peer(follower)
        .run_operation(write(PermChange::DeleteGroup {
            name: "staff".into(),
        }))
        .await
        .unwrap();
    assert!(!outcome.failed(), "{outcome:?}");
    settle(&sim).await;
    for index in 0..3 {
        let database = sim.database(index);
        assert_eq!(groups_named(&database, "staff"), 0);
        assert_eq!(
            number(
                &database,
                "SELECT count(*) FROM perm_node WHERE subject_id NOT IN (SELECT id FROM perm_subject)"
            ),
            0
        );
    }
    assert_identical(&sim);
    sim.shutdown().await;
}
