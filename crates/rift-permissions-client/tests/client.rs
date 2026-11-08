//! The client against a real Rift QUIC broker, with the test playing the
//! proxy's `rift.perms.*` handlers.

use std::{
    collections::{HashMap, HashSet},
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use rift::messaging::{
    Broker,
    quic::{AuthConfig, Identity, Server, server_config_from_der},
};
use rift_permissions::{
    Actor, Change, ChangeEvent, CompiledSubject, ContextSet, SubjectRef, Tristate, Uuid,
    compiled::{Entry, EntryKind},
    unix_now,
    wire::{
        self, Applied, ApplyRequest, CommandReply, CommandRequest, ContextReport, ErrorCode,
        GroupsReply, HelloReply, HelloRequest, RegisterRequest, SubjectBatchReply,
        SubjectBatchRequest, SubjectGetRequest,
    },
};
use rift_permissions_client::{Client, Config, Error, OfflinePolicy, RiftPermissions};
use rustls::pki_types::PrivateKeyDer;
use serde::Serialize;
use serde_json::json;

const TOKEN: &str = "rift-permissions-client-test-token";
const ALICE: Uuid = Uuid::from_u128(0xa11ce);
const BOB: Uuid = Uuid::from_u128(0xb0b);

/// The backend principal exactly as `docs/permissions-protocol.md` lists it.
fn principal() -> Identity {
    let strings = |list: &[&str]| list.iter().map(|s| s.to_string()).collect();
    Identity {
        name: "server.survival-1".into(),
        token: TOKEN.into(),
        publish: strings(&[
            "rift.perms.hello",
            "rift.perms.subject.*",
            "rift.perms.groups.*",
            "rift.perms.apply",
            "rift.perms.register",
            "rift.perms.command",
            "rift.perms.context",
        ]),
        subscribe: strings(&[
            "rift.perms.update",
            "rift.perms.update.>",
            "rift.events.permissions",
        ]),
        control: false,
    }
}

fn subject(uuid: Uuid, revision: u64, node: &str, value: bool) -> CompiledSubject {
    CompiledSubject {
        revision,
        uuid,
        primary_group: "default".into(),
        entries: vec![Entry {
            kind: EntryKind::Permission,
            key: node.into(),
            value,
            contexts: ContextSet::new(),
            rank: 0,
            expires_at: None,
        }],
        prefix: vec![],
        suffix: vec![],
        meta: vec![],
    }
}

fn event(revision: u64, changed: &[Uuid]) -> ChangeEvent {
    ChangeEvent {
        revision,
        at: unix_now(),
        actor: Actor::System,
        reason: None,
        ops: vec![],
        ops_truncated: false,
        changed: changed.to_vec(),
        groups: vec![],
    }
}

/// What the fake proxy holds and what it saw.
#[derive(Default)]
struct State {
    revision: u64,
    subjects: HashMap<Uuid, CompiledSubject>,
    /// Request subjects left unanswered, so the client times out.
    silent: HashSet<&'static str>,
    hellos: Vec<HelloRequest>,
    gets: usize,
    batches: Vec<Vec<Uuid>>,
    applied: Vec<ApplyRequest>,
    commands: Vec<CommandRequest>,
    registered: Vec<RegisterRequest>,
    contexts: Vec<ContextReport>,
}

struct Proxy {
    broker: Broker,
    server: Option<Server>,
    address: SocketAddr,
    tls: quinn::ServerConfig,
    state: Arc<Mutex<State>>,
    dir: PathBuf,
}

impl Proxy {
    async fn start(revision: u64) -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rift-permissions-client-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        fs::write(dir.join("ca.pem"), cert.cert.pem()).unwrap();
        let tls = server_config_from_der(
            vec![cert.cert.der().clone()],
            PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
        )
        .unwrap();
        let broker = Broker::default();
        let state = Arc::new(Mutex::new(State {
            revision,
            ..State::default()
        }));
        tokio::spawn(respond(broker.clone(), state.clone()));
        let server = bind(&broker, "127.0.0.1:0".parse().unwrap(), tls.clone());
        let address = server.local_addr().unwrap();
        Self {
            broker,
            server: Some(server),
            address,
            tls,
            state,
            dir,
        }
    }

    fn config(&self) -> Config {
        let mut config = Config::new(
            self.address.to_string(),
            "localhost",
            self.dir.join("ca.pem"),
            "UNUSED",
            "survival-1",
        );
        config.join_timeout = Duration::from_millis(500);
        config.request_timeout = Duration::from_millis(500);
        config
    }

    async fn client(&self, config: Config) -> Client {
        let client = Client::start_with_token(config, TOKEN).unwrap();
        assert!(client.wait_online(Duration::from_secs(5)).await);
        client
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Change a player's data at a new revision without telling anyone.
    fn set(&self, revision: u64, snapshot: CompiledSubject) {
        let mut state = self.state();
        state.revision = revision;
        state.subjects.insert(snapshot.uuid, snapshot);
    }

    fn publish(&self, subject: &str, body: &impl Serialize) {
        let payload = Bytes::from(serde_json::to_vec(body).unwrap());
        self.broker.publish(subject, payload).unwrap();
    }

    fn push(&self, snapshot: &CompiledSubject) {
        self.publish(&wire::update_subject(snapshot.uuid), snapshot);
    }

    async fn restart(&mut self) {
        let server = self.server.take().unwrap();
        server.shutdown().await;
        drop(server);
        let deadline = Instant::now() + Duration::from_secs(5);
        let server = loop {
            match Server::bind(self.address, self.tls.clone(), self.broker.clone(), auth()) {
                Ok(server) => break server,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("rebinding {}: {error}", self.address),
            }
        };
        self.server = Some(server);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn auth() -> AuthConfig {
    AuthConfig {
        identities: vec![principal()],
        ..AuthConfig::default()
    }
}

fn bind(broker: &Broker, address: SocketAddr, tls: quinn::ServerConfig) -> Server {
    Server::bind(address, tls, broker.clone(), auth()).unwrap()
}

/// The proxy's request handlers, answering from `state`.
async fn respond(broker: Broker, state: Arc<Mutex<State>>) {
    let mut requests = broker.subscribe("rift.perms.>", None).unwrap();
    while let Ok(message) = requests.recv().await {
        let payload = &message.payload;
        let reply = {
            let mut state = state.lock().unwrap();
            let subject: &str = &message.subject;
            if state.silent.contains(subject) {
                continue;
            }
            let ok = |data: serde_json::Value| Some(json!({"ok": true, "data": data}));
            match subject {
                wire::HELLO => {
                    state.hellos.push(serde_json::from_slice(payload).unwrap());
                    let reply = HelloReply {
                        revision: state.revision,
                        protocol: wire::PROTOCOL_VERSION,
                        now: unix_now(),
                        limits: Default::default(),
                        external_contexts: vec!["region".into()],
                    };
                    ok(serde_json::to_value(reply).unwrap())
                }
                wire::SUBJECT_GET => {
                    state.gets += 1;
                    let request: SubjectGetRequest = serde_json::from_slice(payload).unwrap();
                    assert_eq!(request.server.as_deref(), Some("survival-1"));
                    let snapshot = state.subjects.get(&request.uuid).cloned();
                    let snapshot = snapshot.unwrap_or_else(|| subject_of(request.uuid));
                    ok(serde_json::to_value(snapshot).unwrap())
                }
                wire::SUBJECT_BATCH => {
                    let request: SubjectBatchRequest = serde_json::from_slice(payload).unwrap();
                    let subjects = request
                        .uuids
                        .iter()
                        .map(|uuid| {
                            state
                                .subjects
                                .get(uuid)
                                .cloned()
                                .unwrap_or_else(|| subject_of(*uuid))
                        })
                        .collect();
                    state.batches.push(request.uuids);
                    let reply = SubjectBatchReply {
                        revision: state.revision,
                        subjects,
                    };
                    ok(serde_json::to_value(reply).unwrap())
                }
                wire::GROUPS_GET => {
                    let reply = GroupsReply {
                        revision: state.revision,
                        groups: vec![],
                    };
                    ok(serde_json::to_value(reply).unwrap())
                }
                wire::APPLY => {
                    let request: ApplyRequest = serde_json::from_slice(payload).unwrap();
                    state.applied.push(request);
                    state.revision += 1;
                    let applied = Applied {
                        revision: state.revision,
                        ops: 1,
                        changed_users: vec![ALICE],
                        changed_groups: vec![],
                    };
                    ok(serde_json::to_value(applied).unwrap())
                }
                wire::COMMAND => {
                    let request: CommandRequest = serde_json::from_slice(payload).unwrap();
                    state.commands.push(request);
                    let reply = CommandReply {
                        revision: state.revision,
                        component: json!({"text": "done"}),
                    };
                    ok(serde_json::to_value(reply).unwrap())
                }
                wire::REGISTER => {
                    state
                        .registered
                        .push(serde_json::from_slice(payload).unwrap());
                    None
                }
                wire::CONTEXT => {
                    state
                        .contexts
                        .push(serde_json::from_slice(payload).unwrap());
                    None
                }
                _ => None,
            }
        };
        if let (Some(reply), Some(inbox)) = (reply, message.reply) {
            let _ = broker.publish(&inbox, Bytes::from(serde_json::to_vec(&reply).unwrap()));
        }
    }

    /// A player the proxy has no data for compiles to an empty subject.
    fn subject_of(uuid: Uuid) -> CompiledSubject {
        CompiledSubject {
            entries: vec![],
            ..subject(uuid, 1, "unused", false)
        }
    }
}

async fn eventually(what: &str, wait: Duration, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + wait;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn check(client: &Client, uuid: Uuid, node: &str) -> Tristate {
    client.check(uuid, node, &ContextSet::new())
}

#[tokio::test]
async fn join_fetches_the_snapshot_and_newer_pushes_replace_it() {
    let proxy = Proxy::start(5).await;
    proxy.set(5, subject(ALICE, 5, "fly", true));
    let client = proxy.client(proxy.config()).await;
    let snapshots = Arc::new(AtomicUsize::new(0));
    let counter = snapshots.clone();
    let _listener = client.on_snapshot(move |_| {
        counter.fetch_add(1, Ordering::Relaxed);
    });
    {
        let state = proxy.state();
        assert_eq!(state.hellos.len(), 1);
        assert_eq!(state.hellos[0].server, "survival-1");
        assert_eq!(state.hellos[0].revision, 0);
    }
    assert_eq!(client.revision(), 5);
    assert_eq!(check(&client, ALICE, "fly"), Tristate::Undefined);
    assert!(client.join(ALICE).await);
    assert_eq!(check(&client, ALICE, "fly"), Tristate::True);
    assert_eq!(client.primary_group(ALICE).as_deref(), Some("default"));
    // A joined player with a snapshot needs no second request.
    assert!(client.join(ALICE).await);
    assert_eq!(proxy.state().gets, 1);

    proxy.push(&subject(ALICE, 7, "fly", false));
    eventually("the newer push", Duration::from_secs(5), || {
        check(&client, ALICE, "fly") == Tristate::False
    })
    .await;
    // An older push arriving late changes nothing.
    proxy.push(&subject(ALICE, 6, "fly", true));
    // A push ahead of a join is stored, so that join needs no request.
    proxy.push(&subject(BOB, 7, "build", true));
    eventually("bob's push", Duration::from_secs(5), || {
        client.replica().contains(BOB)
    })
    .await;
    assert_eq!(check(&client, ALICE, "fly"), Tristate::False);
    assert!(client.join(BOB).await);
    assert_eq!(proxy.state().gets, 1);
    assert_eq!(snapshots.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn change_events_refetch_in_order_and_resync_after_a_gap() {
    let proxy = Proxy::start(5).await;
    proxy.set(5, subject(ALICE, 5, "fly", true));
    let client = proxy.client(proxy.config()).await;
    assert!(client.join(ALICE).await);

    // The next revision names alice and no push follows: refetch her.
    proxy.set(6, subject(ALICE, 6, "fly", false));
    proxy.publish(wire::UPDATE, &event(6, &[ALICE]));
    eventually("the refetch", Duration::from_secs(5), || {
        check(&client, ALICE, "fly") == Tristate::False
    })
    .await;
    assert_eq!(client.revision(), 6);
    assert_eq!(proxy.state().batches, [vec![ALICE]]);

    // A duplicate is ignored.
    proxy.publish(wire::UPDATE, &event(6, &[ALICE]));
    // The next revision with its push in time needs no request.
    proxy.publish(wire::UPDATE, &event(7, &[ALICE]));
    proxy.push(&subject(ALICE, 7, "fly", true));
    eventually("the push", Duration::from_secs(5), || {
        check(&client, ALICE, "fly") == Tristate::True
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(proxy.state().batches.len(), 1);

    // Revision 8 was lost: everything is refetched.
    proxy.set(9, subject(ALICE, 9, "fly", false));
    proxy.publish(wire::UPDATE, &event(9, &[]));
    eventually("the resync", Duration::from_secs(5), || {
        check(&client, ALICE, "fly") == Tristate::False
    })
    .await;
    assert_eq!(client.revision(), 9);
    assert_eq!(proxy.state().batches.len(), 2);
}

#[tokio::test]
async fn reconnecting_compares_revisions_and_resyncs() {
    let mut proxy = Proxy::start(5).await;
    proxy.set(5, subject(ALICE, 5, "fly", true));
    let client = proxy.client(proxy.config()).await;
    assert!(client.join(ALICE).await);

    // The proxy moves on unannounced, then the bus drops.
    proxy.set(8, subject(ALICE, 8, "fly", false));
    proxy.restart().await;
    eventually(
        "the resync after reconnecting",
        Duration::from_secs(10),
        || check(&client, ALICE, "fly") == Tristate::False,
    )
    .await;
    assert!(client.is_online());
    assert_eq!(client.revision(), 8);
    let state = proxy.state();
    assert_eq!(state.hellos.len(), 2);
    assert_eq!(state.hellos[1].revision, 5);
    assert_eq!(state.batches, [vec![ALICE]]);
}

#[tokio::test]
async fn a_snapshot_timeout_applies_the_fallback_file_and_retries() {
    let proxy = Proxy::start(5).await;
    proxy.set(5, subject(ALICE, 5, "fly", true));
    let fallback = proxy.dir.join("fallback.json");
    fs::write(
        &fallback,
        r#"{"groups": [{"name": "default", "nodes": [{"kind": "permission", "key": "spawn", "value": true}]}]}"#,
    )
    .unwrap();
    let mut config = proxy.config();
    config.offline_policy = OfflinePolicy::FallbackFile(fallback);
    let client = proxy.client(config).await;
    proxy
        .state()
        .silent
        .extend([wire::SUBJECT_GET, wire::SUBJECT_BATCH]);

    let started = Instant::now();
    assert!(!client.join(ALICE).await);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(check(&client, ALICE, "spawn"), Tristate::True);
    assert_eq!(check(&client, ALICE, "fly"), Tristate::Undefined);

    proxy.state().silent.clear();
    eventually("the retry", Duration::from_secs(8), || {
        check(&client, ALICE, "fly") == Tristate::True
    })
    .await;
    assert_eq!(check(&client, ALICE, "spawn"), Tristate::Undefined);
}

#[tokio::test]
async fn an_unreachable_proxy_denies_and_reports_unavailable() {
    let proxy = Proxy::start(1).await;
    let mut config = proxy.config();
    config.address = "127.0.0.1:9".into();
    let client = Client::start_with_token(config, TOKEN).unwrap();
    assert!(!client.wait_online(Duration::from_millis(200)).await);
    assert!(!client.join(ALICE).await);
    assert_eq!(check(&client, ALICE, "fly"), Tristate::Undefined);
    assert!(client.meta(ALICE, &ContextSet::new()).is_none());
    let change = Change::SetPrimaryGroup {
        user: ALICE,
        group: None,
    };
    assert!(matches!(
        client.apply(None, change).await,
        Err(Error::Unavailable)
    ));
    let mut missing = proxy.config();
    missing.token_env = "RIFT_PERMISSIONS_CLIENT_TEST_UNSET".into();
    assert!(matches!(Client::start(missing), Err(Error::Config(_))));
}

#[tokio::test]
async fn last_snapshot_survives_a_restart_without_the_proxy() {
    let proxy = Proxy::start(5).await;
    proxy.set(5, subject(ALICE, 5, "fly", true));
    let path = proxy.dir.join("replica.json");
    let mut config = proxy.config();
    config.offline_policy = OfflinePolicy::LastSnapshot(path.clone());
    let client = proxy.client(config.clone()).await;
    assert!(client.join(ALICE).await);
    client.shutdown().unwrap();
    assert!(!client.is_online());
    drop(client);

    config.address = "127.0.0.1:9".into();
    let restarted = Client::start_with_token(config, TOKEN).unwrap();
    assert_eq!(restarted.revision(), 5);
    assert_eq!(check(&restarted, ALICE, "fly"), Tristate::True);
    assert!(restarted.join(ALICE).await);
}

#[tokio::test]
async fn writes_time_out_and_round_trip() {
    let proxy = Proxy::start(5).await;
    let client = proxy.client(proxy.config()).await;
    let change = Change::SetNode {
        subject: SubjectRef::User(ALICE),
        node: rift_permissions::Node::permission("Essentials.Fly", true).unwrap(),
    };

    proxy.state().silent.insert(wire::APPLY);
    let started = Instant::now();
    assert!(matches!(
        client.apply(Some(BOB), change.clone()).await,
        Err(Error::Timeout)
    ));
    assert!(started.elapsed() < Duration::from_secs(2));

    proxy.state().silent.clear();
    let applied = client.apply(Some(BOB), change).await.unwrap();
    assert_eq!(applied.revision, 6);
    {
        let state = proxy.state();
        assert_eq!(state.applied.len(), 1);
        assert_eq!(state.applied[0].actor_uuid, Some(BOB));
    }
    let invalid = Change::DeleteGroup {
        name: "default".into(),
    };
    let Err(Error::Rejected(reply)) = client.apply(None, invalid).await else {
        panic!("deleting default must be refused locally");
    };
    assert_eq!(reply.code, ErrorCode::InvalidRequest);

    let reply = client
        .command(
            Some(BOB),
            vec!["user".into(), "alice".into(), "info".into()],
        )
        .await
        .unwrap();
    assert_eq!(reply.component, json!({"text": "done"}));
    let registered = vec![wire::RegisteredPermission {
        node: "essentials.fly".into(),
        description: Some("Fly".into()),
        default: "op".parse().unwrap(),
        plugin: Some("Essentials".into()),
    }];
    client.register(&registered).await.unwrap();
    let region = ContextSet::of("region", "spawn").unwrap();
    client.report_context(ALICE, region.clone()).await.unwrap();
    let state = proxy.state();
    assert_eq!(state.commands[0].args, ["user", "alice", "info"]);
    assert_eq!(state.registered, [registered]);
    assert_eq!(state.contexts[0].contexts, region);
}

#[tokio::test]
async fn change_listeners_receive_events_until_dropped() {
    let proxy = Proxy::start(5).await;
    let client = proxy.client(proxy.config()).await;
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let listener = client.on_change(Box::new(move |event| {
        let _ = sender.send(event.revision);
    }));
    proxy.publish(wire::EVENTS, &event(6, &[ALICE]));
    let revision = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap();
    assert_eq!(revision, Some(6));
    drop(listener);
    proxy.publish(wire::EVENTS, &event(7, &[ALICE]));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap(),
        None
    );
    // Events are for listeners; the replica follows `rift.perms.update`.
    assert_eq!(client.revision(), 5);
}
