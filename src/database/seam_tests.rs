//! Tests of the write seam: typed operations, executors, apply hooks,
//! changesets, checksums and the replicated mode.
use super::{
    changeset::{self, Changeset, apply_changeset, capture, capture_on, checksum},
    operation::{self, MigrateOp, MigrateOutcome, execute_in_savepoint},
    *,
};
use crate::{
    config::{GroupMode, GroupState, NodeSchedule, PermissionsConfig},
    directory::{Directory, DirectoryOp},
    extensions::storage::{KvEntry, KvOp, KvOutcome, Quota, Quotas},
    instance_store::{InstanceOp, InstanceOutcome, Store},
    permissions::{
        Authority, Notification,
        ops::{PermBody, PermOp, PermOutcome, PermWrite},
    },
};
use rift_permissions::{Change as PermChange, Limits, Node, SubjectRef, Uuid};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};

/// The time every operation of these tests carries.
const NOW: u64 = 1_700_000_000;

fn alex() -> Uuid {
    Uuid::from_bytes([1; 16])
}

/// A memory database with every component's tables, as startup creates
/// them, and the authority that watches its permission tables.
fn prepared() -> (Arc<Database>, Authority) {
    prepared_on(Arc::new(Database::memory()))
}

/// [`prepared`] on `database`, which may live on a server.
fn prepared_on(database: Arc<Database>) -> (Arc<Database>, Authority) {
    let authority =
        Authority::open_at(database.clone(), &PermissionsConfig::default(), NOW).unwrap();
    Store::new(database.clone()).unwrap();
    Directory::open(database.clone()).unwrap();
    let migrate = Operation::Migrate(MigrateOp {
        component: "extension_store".into(),
        version: 1,
        now: NOW,
    });
    assert_eq!(
        database.run(migrate).unwrap(),
        Outcome::Migrate(MigrateOutcome::Done { changed: true })
    );
    (database, authority)
}

fn write(body: PermBody) -> Operation {
    Operation::Permissions(PermOp::Write(Box::new(PermWrite {
        actor: "cli".into(),
        reason: Some("test".into()),
        now: NOW,
        limits: Limits::default(),
        body,
    })))
}

/// One operation of every kind.
fn operations() -> Vec<Operation> {
    vec![
        Operation::Instances(InstanceOp::AllocateName {
            group: "games".into(),
            floor: 2,
            taken: vec!["games-3".into()],
            taken_nocase: vec!["GAMES-4".into()],
        }),
        Operation::Instances(InstanceOp::BeginCreation {
            name: "games-5".into(),
            node: "node1".into(),
            port: 30000,
            definition: "{}".into(),
        }),
        Operation::Instances(InstanceOp::Transition {
            name: "games-5".into(),
            from: "creating".into(),
            to: "ready".into(),
        }),
        Operation::Instances(InstanceOp::SetExplicitStop {
            name: "games-5".into(),
            stopped: true,
        }),
        Operation::Instances(InstanceOp::SetGroupState {
            group: "games".into(),
            suspended: true,
            mode: "dev".into(),
        }),
        Operation::Instances(InstanceOp::SetNodeSchedule {
            node: "node1".into(),
            schedule: "cordoned".into(),
        }),
        Operation::Instances(InstanceOp::SetMovedServer {
            name: "lobby".into(),
            configured_node: String::new(),
            configured_backend: "127.0.0.1:25566".into(),
            node: "node1".into(),
            port: 30001,
        }),
        Operation::Instances(InstanceOp::PruneMovedServers {
            keep: vec!["lobby".into()],
        }),
        Operation::Instances(InstanceOp::ForgetMovedServer {
            name: "lobby".into(),
        }),
        Operation::Instances(InstanceOp::SetServerState {
            name: "lobby".into(),
            state: crate::instance_store::ServerState {
                phase: "running".into(),
                last_error: Some("an earlier start failed".into()),
                automatic: true,
                restart_attempts: 2,
            },
        }),
        Operation::Instances(InstanceOp::SetServerState {
            name: "games-5".into(),
            state: crate::instance_store::ServerState {
                phase: "stopped".into(),
                last_error: None,
                automatic: false,
                restart_attempts: 0,
            },
        }),
        Operation::Instances(InstanceOp::PruneServerStates {
            keep: vec!["lobby".into()],
        }),
        write(PermBody::Change {
            change: PermChange::CreateGroup {
                name: "vip".into(),
                weight: 10,
                display_name: None,
            },
        }),
        write(PermBody::Change {
            change: PermChange::SetNode {
                subject: SubjectRef::User(alex()),
                node: Node::inherit("vip", true).unwrap(),
            },
        }),
        Operation::Permissions(PermOp::Join {
            uuid: alex(),
            name: "Alex".into(),
            now: NOW,
        }),
        Operation::Directory(DirectoryOp::Joined {
            uuid: alex().to_string(),
            name: "Alex".into(),
            ip: Some("203.0.113.7".into()),
            server: "lobby".into(),
            now: NOW,
        }),
        Operation::Kv(KvOp::Mutate {
            quotas: Quotas::default(),
            expect: vec![KvEntry {
                key: ("one".into(), "key".into()),
                value: None,
            }],
            changes: vec![KvEntry {
                key: ("one".into(), "key".into()),
                value: Some(b"hello\0\xff".to_vec()),
            }],
        }),
        Operation::Instances(InstanceOp::PruneGroupStates { keep: Vec::new() }),
    ]
}

#[test]
fn operations_and_outcomes_round_trip_through_serde() {
    let (database, _authority) = prepared();
    for operation in operations() {
        let json = serde_json::to_string(&operation).unwrap();
        let back: Operation = serde_json::from_str(&json).unwrap();
        assert_eq!(back, operation, "{json}");
        let outcome = database.run(operation).unwrap();
        assert!(!outcome.failed(), "{outcome:?}");
        let json = serde_json::to_string(&outcome).unwrap();
        let back: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back, outcome, "{json}");
    }
    // A failure is a normal outcome, and it serializes too.
    let failed = database
        .run(Operation::Instances(InstanceOp::Transition {
            name: "missing".into(),
            from: "ready".into(),
            to: "removing".into(),
        }))
        .unwrap();
    assert_eq!(
        failed,
        Outcome::Instances(InstanceOutcome::Failed(
            "instances.missing: expected phase ready".into()
        ))
    );
    let json = serde_json::to_string(&failed).unwrap();
    assert_eq!(serde_json::from_str::<Outcome>(&json).unwrap(), failed);
}

/// The default executor runs every operation on a server database too
/// (PostgreSQL or MariaDB), commits it and calls the hooks of its tables.
#[test]
fn operations_run_locally_on_a_server() {
    let Some(location) = test_server_or_skip("operations_run_locally_on_a_server") else {
        return;
    };
    let (database, _authority) = prepared_on(Arc::new(Database::connect(&location).unwrap()));
    let changes = Arc::new(AtomicUsize::new(0));
    let counted = changes.clone();
    let hook = database.on_change(&["group_states"], move |_, change| {
        assert!(!change.reload);
        counted.fetch_add(1, Ordering::SeqCst);
    });
    for operation in operations() {
        let outcome = database.run(operation).unwrap();
        assert!(!outcome.failed(), "{outcome:?}");
    }
    // SetGroupState and PruneGroupStates.
    assert_eq!(changes.load(Ordering::SeqCst), 2);
    // A failure rolls back and calls no hook.
    let failed = database
        .run(Operation::Instances(InstanceOp::SetGroupState {
            group: "games".into(),
            suspended: true,
            mode: "paused".into(),
        }))
        .unwrap();
    assert!(failed.failed());
    assert_eq!(changes.load(Ordering::SeqCst), 2);
    database.remove_hook(hook);
}

#[test]
fn the_same_operations_on_two_databases_give_the_same_state() {
    let (first, _first_authority) = prepared();
    let (second, _second_authority) = prepared();
    assert_eq!(
        checksum(&first.lock()).unwrap(),
        checksum(&second.lock()).unwrap()
    );
    for operation in operations() {
        let one = first.run(operation.clone()).unwrap();
        let two = second.run(operation).unwrap();
        assert_eq!(one, two);
        assert_eq!(
            checksum(&first.lock()).unwrap(),
            checksum(&second.lock()).unwrap()
        );
    }
    // The allocated name skips the taken ones, ASCII case ignored.
    let name = first
        .run(Operation::Instances(InstanceOp::AllocateName {
            group: "games".into(),
            floor: 0,
            taken: Vec::new(),
            taken_nocase: Vec::new(),
        }))
        .unwrap();
    assert_eq!(
        name,
        Outcome::Instances(InstanceOutcome::Name("games-6".into()))
    );
    assert_ne!(
        checksum(&first.lock()).unwrap(),
        checksum(&second.lock()).unwrap()
    );
}

#[test]
fn hooks_fire_once_per_change_and_on_reload() {
    let (database, _authority) = prepared();
    let store = Store::new(database.clone()).unwrap();
    let changes = Arc::new(AtomicUsize::new(0));
    let reloads = Arc::new(AtomicUsize::new(0));
    let (counted, reloaded) = (changes.clone(), reloads.clone());
    let hook = database.on_change(&["group_states"], move |_, change| {
        if change.reload {
            reloaded.fetch_add(1, Ordering::SeqCst);
        } else {
            assert!(change.tables.contains("group_states"));
            assert!(matches!(change.operation, Some(Operation::Instances(_))));
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    let dev = GroupState {
        suspended: false,
        mode: GroupMode::Dev,
    };
    store.set_group_state("games", dev).unwrap();
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    // Other tables do not call it.
    store
        .set_node_schedule("node1", NodeSchedule::Cordoned)
        .unwrap();
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    // A failed operation is rolled back and calls no hook.
    let failed = database
        .run(Operation::Instances(InstanceOp::SetGroupState {
            group: "games".into(),
            suspended: true,
            mode: "paused".into(),
        }))
        .unwrap();
    assert!(failed.failed());
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    assert_eq!(store.group_states().unwrap()["games"], dev);
    database.reload_all();
    assert_eq!(reloads.load(Ordering::SeqCst), 1);
    assert_eq!(changes.load(Ordering::SeqCst), 1);
    database.remove_hook(hook);
    store
        .set_group_state("games", GroupState::default())
        .unwrap();
    assert_eq!(changes.load(Ordering::SeqCst), 1);
}

#[test]
fn group_states_and_schedules_reach_memory_through_the_hook() {
    let (database, _authority) = prepared();
    let store = Store::new(database.clone()).unwrap();
    store
        .set_node_schedule("node1", NodeSchedule::Draining)
        .unwrap();
    let groups = Arc::new(Mutex::new(BTreeMap::new()));
    let nodes = Arc::new(Mutex::new(BTreeMap::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let (mirror, schedules, counted) = (groups.clone(), nodes.clone(), calls.clone());
    store
        .watch_states(
            move |states| {
                counted.fetch_add(1, Ordering::SeqCst);
                *mirror.lock().unwrap() = states;
            },
            move |states| *schedules.lock().unwrap() = states,
        )
        .unwrap();
    // The current tables at once.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(nodes.lock().unwrap()["node1"], NodeSchedule::Draining);
    let state = store
        .update_group_state("games", |state| state.suspended = true)
        .unwrap();
    assert!(state.suspended);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(groups.lock().unwrap()["games"], state);
    let state = store
        .update_group_state("games", |state| state.mode = GroupMode::Dev)
        .unwrap();
    assert_eq!(
        state,
        GroupState {
            suspended: true,
            mode: GroupMode::Dev
        },
        "both changes are kept"
    );
    assert_eq!(groups.lock().unwrap()["games"], state);
    // A prune is one change.
    let keep = vec!["lobby".to_owned()];
    assert_eq!(store.prune_group_states(&keep).unwrap(), ["games"]);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert!(groups.lock().unwrap().is_empty());
    assert_eq!(store.prune_node_schedules(&keep).unwrap(), ["node1"]);
    assert!(nodes.lock().unwrap().is_empty());
    // A write the hook did not see coming, such as an applied changeset, is
    // reported the same way.
    drop(store);
    database.reload_all();
    assert_eq!(calls.load(Ordering::SeqCst), 4, "hooks go with their store");
}

#[test]
fn every_operation_goes_through_the_executor() {
    struct Counting(AtomicUsize);
    impl Executor for Counting {
        fn run(
            &self,
            database: &Database,
            operation: Operation,
            deadline: Option<Instant>,
        ) -> io::Result<Outcome> {
            self.0.fetch_add(1, Ordering::SeqCst);
            database.execute_locally(operation, deadline)
        }
    }
    let (database, authority) = prepared();
    let store = Store::new(database.clone()).unwrap();
    let directory = Directory::open(database.clone()).unwrap();
    let counting = Arc::new(Counting(AtomicUsize::new(0)));
    database.set_executor(counting.clone());
    store
        .set_node_schedule("node1", NodeSchedule::Cordoned)
        .unwrap();
    authority
        .apply(
            rift_permissions::Actor::Cli,
            PermChange::CreateGroup {
                name: "staff".into(),
                weight: 0,
                display_name: None,
            },
        )
        .unwrap();
    directory.joined(&alex().to_string(), "Alex", None, "lobby");
    assert_eq!(counting.0.load(Ordering::SeqCst), 3);
    // Reads do not.
    store.node_schedules().unwrap();
    authority.group("staff").unwrap();
    assert_eq!(counting.0.load(Ordering::SeqCst), 3);
}

#[test]
fn kv_mutations_check_their_reads_and_quotas() {
    let (database, _authority) = prepared();
    let entry = |key: &str, value: Option<&[u8]>| KvEntry {
        key: ("one".into(), key.into()),
        value: value.map(<[u8]>::to_vec),
    };
    let quotas = Quotas {
        default: Quota {
            entries: 1,
            bytes: 1024,
        },
        plugins: BTreeMap::new(),
    };
    let mutate = |expect: Vec<KvEntry>, changes: Vec<KvEntry>| {
        let op = Operation::Kv(KvOp::Mutate {
            quotas: quotas.clone(),
            expect,
            changes,
        });
        match database.run(op).unwrap() {
            Outcome::Kv(outcome) => outcome,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        mutate(vec![entry("a", None)], vec![entry("a", Some(b"1"))]),
        KvOutcome::Applied {
            changes: vec![entry("a", Some(b"1"))]
        }
    );
    // A read that no longer holds is a conflict, and nothing is written.
    assert_eq!(
        mutate(vec![entry("a", None)], vec![entry("a", Some(b"2"))]),
        KvOutcome::Conflict {
            key: ("one".into(), "a".into())
        }
    );
    // The script may hold one entry.
    match mutate(Vec::new(), vec![entry("b", Some(b"1"))]) {
        KvOutcome::Rejected { message } => {
            assert!(message.contains("exceeds 1 entries"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    // Replacing and deleting stay within it; unchanged entries are not
    // written again.
    assert_eq!(
        mutate(Vec::new(), vec![entry("a", Some(b"1")), entry("b", None)]),
        KvOutcome::Applied {
            changes: Vec::new()
        }
    );
    assert_eq!(
        mutate(vec![entry("a", Some(b"1"))], vec![entry("a", None)]),
        KvOutcome::Applied {
            changes: vec![entry("a", None)]
        }
    );
    let rows: i64 = database
        .lock()
        .query_row("SELECT count(*) FROM extension_entries", params![], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0);
}

const PARENTS: &str = "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE child (
    id INTEGER PRIMARY KEY,
    parent INTEGER NOT NULL REFERENCES parent(id) ON DELETE CASCADE,
    value TEXT
);
INSERT INTO parent VALUES (1, 'one'), (2, 'two');
INSERT INTO child VALUES (10, 1, 'a'), (11, 1, 'b'), (12, 2, 'c');";

fn parents() -> Database {
    let database = Database::memory();
    database
        .migrate("changeset_test", 1, |_, _| PARENTS.into())
        .unwrap();
    database
}

/// A cascade delete, an insert, an update and a savepoint rolled back.
fn edit(connection: &Conn) -> i64 {
    connection
        .execute("DELETE FROM parent WHERE id = 1", params![])
        .unwrap();
    connection
        .execute("INSERT INTO parent VALUES (3, 'three')", params![])
        .unwrap();
    connection
        .execute("UPDATE child SET value = 'x' WHERE id = 12", params![])
        .unwrap();
    connection
        .execute_batch(
            "SAVEPOINT inner;
             INSERT INTO parent VALUES (4, 'four');
             UPDATE child SET value = 'y' WHERE id = 12;
             ROLLBACK TO inner;
             RELEASE inner;",
        )
        .unwrap();
    7
}

fn apply(database: &Database, changeset: &Changeset) -> Result<ChangedTables> {
    let mut connection = database.lock();
    let transaction = connection.transaction_immediate()?;
    let tables = apply_changeset(&transaction, changeset)?;
    transaction.commit()?;
    Ok(tables)
}

#[test]
fn a_captured_changeset_reproduces_the_transaction_elsewhere() {
    let leader = parents();
    let follower = parents();
    let before = checksum(&leader.lock()).unwrap();
    assert_eq!(before, checksum(&follower.lock()).unwrap());
    let (value, changeset) = capture(&mut leader.lock(), |transaction| edit(transaction)).unwrap();
    assert_eq!(value, 7);
    assert!(!changeset.is_empty());
    // Capturing rolls back.
    assert_eq!(checksum(&leader.lock()).unwrap(), before);
    let tables = apply(&follower, &changeset).unwrap();
    assert_eq!(
        tables,
        ["child", "parent"].into_iter().collect::<ChangedTables>()
    );
    // The same transaction committed on the leader gives the same state.
    {
        let mut connection = leader.lock();
        let transaction = connection.transaction_immediate().unwrap();
        edit(&transaction);
        transaction.commit().unwrap();
    }
    assert_eq!(
        checksum(&leader.lock()).unwrap(),
        checksum(&follower.lock()).unwrap()
    );
    let ids: Vec<(i64, Option<String>)> = follower
        .lock()
        .query_map("SELECT id, value FROM child ORDER BY id", params![], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(ids, [(12, Some("x".to_owned()))]);
    let parents: Vec<i64> = follower
        .lock()
        .query_map("SELECT id FROM parent ORDER BY id", params![], |r| r.get(0))
        .unwrap();
    assert_eq!(parents, [2, 3], "the rolled-back savepoint is left out");
    // Applying it again conflicts: the whole apply aborts.
    let applied = checksum(&follower.lock()).unwrap();
    assert!(apply(&follower, &changeset).is_err());
    assert_eq!(checksum(&follower.lock()).unwrap(), applied);
}

#[test]
fn a_capture_on_entries_in_flight_holds_only_its_own_changes() {
    let leader = parents();
    let follower = parents();
    let insert = |connection: &Conn| {
        connection
            .execute("INSERT INTO parent VALUES (3, 'three')", params![])
            .unwrap();
    };
    let rename = |connection: &Conn| {
        connection
            .execute("UPDATE parent SET name = 'drei' WHERE id = 3", params![])
            .unwrap();
    };
    let ((), first) = capture(&mut leader.lock(), |transaction| insert(transaction)).unwrap();
    // The first entry is still in flight: the second builds on it.
    let ((), second) = capture_on(
        &mut leader.lock(),
        std::slice::from_ref(&first),
        |transaction| rename(transaction),
    )
    .unwrap();
    assert_eq!(
        second.tables().unwrap(),
        ["parent"].into_iter().collect::<ChangedTables>()
    );
    for peer in [&leader, &follower] {
        apply(peer, &first).unwrap();
        apply(peer, &second).unwrap();
    }
    assert_eq!(
        checksum(&leader.lock()).unwrap(),
        checksum(&follower.lock()).unwrap()
    );
    let name: String = follower
        .lock()
        .query_row("SELECT name FROM parent WHERE id = 3", params![], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(name, "drei");
    // Alone, the second entry does not apply: its row does not exist.
    let fresh = parents();
    assert!(apply(&fresh, &second).is_err());
}

#[test]
fn a_leader_batch_replicates_and_fires_the_same_hooks_on_every_peer() {
    let (leader, leader_authority) = prepared();
    let (follower, follower_authority) = prepared();
    let mut events = follower_authority.subscribe();
    let batch = [
        write(PermBody::Change {
            change: PermChange::CreateGroup {
                name: "vip".into(),
                weight: 5,
                display_name: None,
            },
        }),
        // Fails (the group exists) and rolls back alone.
        write(PermBody::Change {
            change: PermChange::CreateGroup {
                name: "vip".into(),
                weight: 0,
                display_name: None,
            },
        }),
        Operation::Instances(InstanceOp::SetGroupState {
            group: "games".into(),
            suspended: true,
            mode: "active".into(),
        }),
    ];
    let (outcomes, changeset) = capture(&mut leader.lock(), |transaction| {
        batch
            .iter()
            .map(|operation| execute_in_savepoint(transaction, operation).unwrap())
            .collect::<Vec<_>>()
    })
    .unwrap();
    assert!(!outcomes[0].failed());
    assert!(outcomes[1].failed());
    assert!(!outcomes[2].failed());
    assert!(
        leader_authority.group("vip").is_none(),
        "captured, not applied"
    );
    for peer in [&leader, &follower] {
        let mut connection = peer.lock();
        let transaction = connection.transaction_immediate().unwrap();
        let tables = apply_changeset(&transaction, &changeset).unwrap();
        transaction.commit().unwrap();
        for (operation, outcome) in batch.iter().zip(&outcomes) {
            if !outcome.failed() {
                peer.fire(
                    &connection,
                    &Change {
                        tables: &tables,
                        operation: Some(operation),
                        outcome: Some(outcome),
                        reload: false,
                    },
                );
            }
        }
    }
    assert_eq!(
        checksum(&leader.lock()).unwrap(),
        checksum(&follower.lock()).unwrap()
    );
    for authority in [&leader_authority, &follower_authority] {
        assert_eq!(authority.revision(), 1);
        assert_eq!(authority.group("vip").unwrap().weight, 5);
    }
    // One notification for the one revision, although the hook ran twice.
    match events.try_recv().unwrap() {
        Notification::Committed { event, .. } => {
            assert_eq!(event.revision, 1);
            assert_eq!(event.groups, ["vip"]);
        }
        other => panic!("{other:?}"),
    }
    assert!(events.try_recv().is_err());
    assert!(matches!(
        outcomes[0],
        Outcome::Permissions(PermOutcome::Written { revision: 1, .. })
    ));
}

#[test]
fn every_table_needs_a_primary_key() {
    let database = Database::memory();
    changeset::assert_primary_keys(&database.lock()).unwrap();
    database
        .migrate("keys_test", 1, |_, _| {
            "CREATE TABLE keyed (id INTEGER PRIMARY KEY); CREATE TABLE loose (x TEXT);".into()
        })
        .unwrap();
    let error = changeset::assert_primary_keys(&database.lock()).unwrap_err();
    assert!(error.to_string().contains("loose"), "{error}");
    assert!(!error.to_string().contains("keyed"), "{error}");
}

#[test]
fn a_replicated_database_uses_wal_and_read_connections() {
    let directory = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("rift-database-replicated-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("rift.sqlite3");
    {
        let database = Arc::new(Database::open_with(&path, Mode::Replicated).unwrap());
        assert_eq!(database.mode(), Mode::Replicated);
        let journal: String = database
            .lock()
            .query_row("PRAGMA journal_mode", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(journal, "wal");
        let synchronous: i64 = database
            .lock()
            .query_row("PRAGMA synchronous", params![], |r| r.get(0))
            .unwrap();
        assert_eq!(synchronous, 1, "NORMAL");
        // A replica does not migrate when a component opens: it checks.
        assert!(Store::new(database.clone()).is_err());
        for (component, version) in operation::latest_versions() {
            let outcome = database
                .run(Operation::Migrate(MigrateOp {
                    component: component.into(),
                    version,
                    now: NOW,
                }))
                .unwrap();
            assert!(!outcome.failed(), "{component}: {outcome:?}");
        }
        let store = Store::new(database.clone()).unwrap();
        store
            .set_node_schedule("node1", NodeSchedule::Cordoned)
            .unwrap();
        // Reads use the read connections and see every committed write.
        assert_eq!(
            store.node_schedules().unwrap()["node1"],
            NodeSchedule::Cordoned
        );
        let readers: Vec<_> = (0..READERS).map(|_| database.read()).collect();
        assert!(
            readers[0]
                .execute("DELETE FROM node_schedules", params![])
                .is_err(),
            "read connections do not write"
        );
        drop(readers);
        // Every table has a primary key.
        changeset::assert_primary_keys(&database.lock()).unwrap();
    }
    // A file with a table without one is refused.
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE loose (x TEXT);")
            .unwrap();
    }
    let error = Database::open_with(&path, Mode::Replicated).unwrap_err();
    assert!(error.to_string().contains("loose"), "{error}");
    // A single Rift still opens it, in its own journal mode.
    let single = Database::open(&path).unwrap();
    let journal: String = single
        .lock()
        .query_row("PRAGMA journal_mode", params![], |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
    drop(single);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn migrations_run_inside_a_transaction_from_an_operation() {
    let database = Database::memory();
    let migrate = |component: &str, version: u32| {
        database
            .run(Operation::Migrate(MigrateOp {
                component: component.into(),
                version,
                now: NOW,
            }))
            .unwrap()
    };
    for (component, latest) in operation::latest_versions() {
        assert_eq!(
            migrate(component, latest),
            Outcome::Migrate(MigrateOutcome::Done { changed: true }),
            "{component}"
        );
        assert_eq!(
            migrate(component, latest),
            Outcome::Migrate(MigrateOutcome::Done { changed: false }),
            "{component}"
        );
        assert_eq!(version(&database.lock(), component).unwrap(), latest);
    }
    // The permission store's startup row carries the operation's time.
    let created: i64 = database
        .lock()
        .query_row(
            "SELECT created_at FROM perm_subject WHERE kind = 1 AND key = 'default'",
            params![],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(created, NOW as i64);
    assert!(migrate("permissions", 9).failed());
    assert!(migrate("nonexistent", 1).failed());
    // Startup through the components finds everything current.
    Store::new(Arc::new(database)).unwrap();
}
