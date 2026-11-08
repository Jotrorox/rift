//! The permission bus end to end: the real Rift executable with messaging
//! enabled, and the real `rift-permissions-client` playing two backends.
use bytes::Bytes;
use rift::protocol::{Codec, Handshake, NextState, Packet, read_string, write_string};
use rift_permissions::{
    Change, ChangeEvent, ContextSet, Node, SubjectRef, Tristate, Uuid,
    wire::{ErrorCode, PermissionDefault, RegisteredPermission},
};
use rift_permissions_client::{Client, Config, Error, RiftPermissions};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const SURVIVAL_TOKEN: &str = "rift-permissions-e2e-survival-token-32-bytes";
const LOBBY_TOKEN: &str = "rift-permissions-e2e-lobby-token-32-bytes!!";
const ADMIN_TOKEN: &str = "rift-permissions-e2e-admin-token-32-bytes!!";

struct Fixture {
    dir: PathBuf,
    backend: TcpListener,
}

impl Fixture {
    fn new() -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rift-permissions-e2e-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        fs::write(dir.join("cert.pem"), cert.cert.pem()).unwrap();
        fs::write(dir.join("key.pem"), cert.signing_key.serialize_pem()).unwrap();
        let backend = TcpListener::bind("127.0.0.1:0").unwrap();
        fs::write(
            dir.join("rift.lua"),
            format!(
                r#"
return {{
    listeners = {{ main = '127.0.0.1:0' }},
    backends = {{ ['survival-1'] = '{}', lobby = '127.0.0.1:1' }},
    routes = {{ main = 'survival-1' }},
    permissions = {{ contexts = {{ external = {{ 'region' }} }} }},
    admin = {{ listen = '127.0.0.1:0', permissions = {{ 'status', 'perms' }} }},
    messaging = {{
        listen = '127.0.0.1:0', certificate = 'cert.pem', private_key = 'key.pem',
        principals = {{
            {{ name = 'server.survival-1', token_env = 'RIFT_SURVIVAL_TOKEN', preset = 'backend' }},
            {{ name = 'server.lobby', token_env = 'RIFT_LOBBY_TOKEN', preset = 'backend' }},
        }},
    }},
}}
"#,
                backend.local_addr().unwrap()
            ),
        )
        .unwrap();
        Self { dir, backend }
    }

    fn start(&self) -> Proxy {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rift"))
            .current_dir(&self.dir)
            .args(["--config", "rift.lua"])
            .env("RIFT_SURVIVAL_TOKEN", SURVIVAL_TOKEN)
            .env("RIFT_LOBBY_TOKEN", LOBBY_TOKEN)
            .env("RIFT_ADMIN_TOKEN", ADMIN_TOKEN)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let (mut front, mut admin, mut bus) = (None, None, None);
        let mut logs = Vec::new();
        while front.is_none() || admin.is_none() || bus.is_none() {
            let line = lines
                .recv_timeout(Duration::from_secs(15))
                .unwrap_or_else(|error| panic!("startup ({error}): {logs:?}"));
            if let Some(address) = line.strip_prefix("rift: listening on ") {
                front = address.parse().ok();
            }
            if let Some(address) = line.strip_prefix("rift: admin on ") {
                admin = address.parse().ok();
            }
            if let Some(address) = line.strip_prefix("rift: messaging QUIC on ") {
                bus = address.parse().ok();
            }
            logs.push(line);
        }
        Proxy {
            child,
            logs: lines,
            front: front.unwrap(),
            admin: admin.unwrap(),
            bus: bus.unwrap(),
            ca: self.dir.join("cert.pem"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct Proxy {
    child: Child,
    /// Kept open: Rift must be able to write to stderr.
    logs: mpsc::Receiver<String>,
    front: SocketAddr,
    admin: SocketAddr,
    bus: SocketAddr,
    ca: PathBuf,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Proxy {
    fn admin(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_rift"))
            .args(["admin", "--address", &self.admin.to_string(), "perms"])
            .args(args)
            .env("RIFT_ADMIN_TOKEN", ADMIN_TOKEN)
            .output()
            .unwrap()
    }

    /// `rift admin perms <args>`, which must succeed; its text.
    fn perms(&self, args: &[&str]) -> String {
        let output = self.admin(args);
        assert!(
            output.status.success(),
            "perms {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn config(&self, server: &str) -> Config {
        let mut config = Config::new(
            self.bus.to_string(),
            "localhost",
            self.ca.clone(),
            "UNUSED",
            server,
        );
        config.join_timeout = Duration::from_secs(2);
        config.request_timeout = Duration::from_secs(5);
        config
    }

    async fn client(&self, server: &str, token: &str) -> Client {
        let client = Client::start_with_token(self.config(server), token).unwrap();
        assert!(
            client.wait_online(Duration::from_secs(10)).await,
            "{server} did not come online"
        );
        client
    }
}

fn uuid_of(name: &str) -> Uuid {
    Uuid::from_bytes(rift::players::offline_uuid(name))
}

fn read_packet(stream: &mut TcpStream) -> Packet {
    let mut length = 0usize;
    for shift in (0..21).step_by(7) {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        length |= usize::from(byte[0] & 127) << shift;
        if byte[0] & 128 == 0 {
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            return Packet::from_body(&body).unwrap();
        }
    }
    panic!("oversized packet");
}

/// A Minecraft 1.8 client logging in as `name`, left open.
fn connect_player(front: SocketAddr, name: &str) -> TcpStream {
    let mut socket = TcpStream::connect(front).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let handshake = Handshake {
        protocol: 47,
        address: "localhost".into(),
        port: 25565,
        next_state: NextState::Login,
    };
    let mut bytes = Codec::default().encode(&handshake.packet()).unwrap();
    let mut data = Vec::new();
    write_string(name, &mut data);
    bytes.extend(Codec::default().encode(&Packet::new(0, data)).unwrap());
    socket.write_all(&bytes).unwrap();
    socket
}

/// Accept the proxy's backend connection for `name` and answer Login
/// Success with the offline UUID, as an offline-mode Paper server would.
/// `before_login` runs once the connection is accepted, before the backend
/// reads anything.
fn accept_player(listener: &TcpListener, before_login: impl FnOnce()) -> TcpStream {
    let (mut socket, _) = listener.accept().unwrap();
    before_login();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert_eq!(
        Handshake::decode(&read_packet(&mut socket))
            .unwrap()
            .next_state,
        NextState::Login
    );
    let login = read_packet(&mut socket);
    let name = read_string(&mut login.data.as_slice(), 16)
        .unwrap()
        .to_owned();
    let mut data = Vec::new();
    write_string(&uuid_of(&name).hyphenated(), &mut data);
    write_string(&name, &mut data);
    socket
        .write_all(&Codec::default().encode(&Packet::new(2, data)).unwrap())
        .unwrap();
    socket
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn set_node(uuid: Uuid, node: Node) -> Change {
    Change::SetNode {
        subject: SubjectRef::User(uuid),
        node,
    }
}

fn rejected(result: Result<impl std::fmt::Debug, Error>, code: ErrorCode) -> String {
    match result {
        Err(Error::Rejected(reply)) => {
            assert_eq!(reply.code, code, "{}", reply.error);
            reply.error
        }
        other => panic!("expected a {code:?} rejection, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backends_replicate_act_and_resync_through_the_real_proxy() {
    let fixture = Fixture::new();
    let proxy = fixture.start();
    let alice = uuid_of("Alice");

    // hello: both backends come online; the proxy saw each one.
    let survival = proxy.client("survival-1", SURVIVAL_TOKEN).await;
    let lobby = proxy.client("lobby", LOBBY_TOKEN).await;
    let backends = proxy.perms(&["backends"]);
    assert!(
        backends.contains("server.survival-1 (survival-1): revision 0 at hello"),
        "{backends}"
    );
    assert!(backends.contains("server.lobby (lobby)"), "{backends}");
    // A principal may only speak for its own server.
    let impostor = Client::start_with_token(proxy.config("survival-1"), LOBBY_TOKEN).unwrap();
    assert!(!impostor.wait_online(Duration::from_secs(2)).await);
    drop(impostor);

    let events: Arc<Mutex<Vec<ChangeEvent>>> = Arc::default();
    let recorded = events.clone();
    let _listener = survival.on_change(Box::new(move |event| {
        recorded.lock().unwrap().push(event.clone());
    }));

    // Data for a player who never joined, addressed by UUID.
    let text = proxy.perms(&[
        "user",
        &alice.to_string(),
        "permission",
        "set",
        "essentials.fly",
    ]);
    assert!(text.contains("Set essentials.fly to true"), "{text}");
    eventually("the change event", || survival.revision() == 1).await;

    // join: the snapshot is pushed before the connection reaches the backend.
    let front = proxy.front;
    let player = tokio::task::spawn_blocking(move || connect_player(front, "Alice"));
    let listener = fixture.backend.try_clone().unwrap();
    let pushed = {
        let client = survival.clone();
        tokio::task::spawn_blocking(move || {
            let mut seen = false;
            let socket = accept_player(&listener, || {
                // Nobody asked for Alice: only the push can have brought her.
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    if client.replica().get(alice).is_some() {
                        seen = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            });
            (socket, seen)
        })
    };
    let (_backend_socket, seen) = pushed.await.unwrap();
    let _player_socket = player.await.unwrap();
    assert!(
        seen,
        "Alice's snapshot was not pushed before her connection"
    );
    assert!(survival.join(alice).await);
    assert_eq!(
        survival.check(alice, "essentials.fly", &ContextSet::new()),
        Tristate::True
    );
    let info = loop {
        let info = proxy.perms(&["user", "Alice", "info"]);
        if info.contains("Online on survival-1") {
            break info;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(info.contains("essentials.fly = true"), "{info}");

    // apply: a backend without control cannot write as itself, and a player
    // needs the verb node and may not grant what they lack.
    let chat = Node::permission("chat.color", true).unwrap();
    let error = rejected(
        survival.apply(None, set_node(alice, chat.clone())).await,
        ErrorCode::Forbidden,
    );
    assert!(error.contains("control = true"), "{error}");
    let error = rejected(
        survival
            .apply(Some(alice), set_node(alice, chat.clone()))
            .await,
        ErrorCode::Forbidden,
    );
    assert!(error.contains("rift.perms.user.permission.set"), "{error}");
    proxy.perms(&["user", "Alice", "permission", "set", "rift.perms.user.*"]);
    let error = rejected(
        survival
            .apply(Some(alice), set_node(alice, chat.clone()))
            .await,
        ErrorCode::Forbidden,
    );
    assert!(error.contains("rift.perms.escalate"), "{error}");
    // The lobby cannot act for a player on survival-1.
    let error = rejected(
        lobby
            .apply(Some(alice), set_node(alice, chat.clone()))
            .await,
        ErrorCode::Forbidden,
    );
    assert!(error.contains("not online"), "{error}");
    proxy.perms(&["user", "Alice", "permission", "set", "chat.*"]);
    let applied = survival
        .apply(Some(alice), set_node(alice, chat.clone()))
        .await
        .unwrap();
    assert_eq!(applied.ops, 1);
    eventually("the applied snapshot", || {
        survival
            .replica()
            .get(alice)
            .is_some_and(|subject| subject.revision == applied.revision)
    })
    .await;
    eventually("the replica revision", || {
        survival.revision() == applied.revision
    })
    .await;
    assert_eq!(
        survival.check(alice, "chat.color", &ContextSet::new()),
        Tristate::True
    );
    let log = proxy.perms(&["log", "--actor", "Alice"]);
    assert!(log.contains(&format!("by uuid:{alice}")), "{log}");

    // Revision ordering: every change arrives once, in order.
    for index in 0..5 {
        proxy.perms(&[
            "group",
            "default",
            "permission",
            "set",
            &format!("test.n{index}"),
        ]);
    }
    let current = applied.revision + 5;
    eventually("five more revisions", || survival.revision() == current).await;
    eventually("five more events", || {
        events
            .lock()
            .unwrap()
            .last()
            .is_some_and(|e| e.revision == current)
    })
    .await;
    let revisions: Vec<u64> = events.lock().unwrap().iter().map(|e| e.revision).collect();
    assert!(
        revisions.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{revisions:?}"
    );
    assert_eq!(
        survival.check(alice, "test.n4", &ContextSet::new()),
        Tristate::True
    );

    // register: stored under the backend identity.
    survival
        .register(&[RegisteredPermission {
            node: "essentials.home".into(),
            description: Some("Teleport home".into()),
            default: PermissionDefault::Op(None),
            plugin: Some("Essentials".into()),
        }])
        .await
        .unwrap();
    eventually("the registered permission", || {
        proxy.perms(&["registered"]).contains(
            "essentials.home (default op) from server.survival-1/Essentials: Teleport home",
        )
    })
    .await;

    // command forwarding: answered as a component, never escalating.
    let reply = survival
        .command(
            Some(alice),
            ["user", "Alice", "permission", "set", "chat.color", "false"]
                .map(String::from)
                .to_vec(),
        )
        .await
        .unwrap();
    let text = reply.component.to_string();
    assert!(text.contains("Set chat.color to false for Alice"), "{text}");
    assert_eq!(reply.revision, current + 1);
    let reply = survival
        .command(
            Some(alice),
            ["user", "Alice", "permission", "set", "server.admin"]
                .map(String::from)
                .to_vec(),
        )
        .await
        .unwrap();
    assert!(
        reply.component.to_string().contains("rift.perms.escalate"),
        "{}",
        reply.component
    );
    let reply = survival
        .command(
            Some(alice),
            ["group", "vip", "create"].map(String::from).to_vec(),
        )
        .await
        .unwrap();
    assert!(
        reply
            .component
            .to_string()
            .contains("rift.perms.group.create"),
        "{}",
        reply.component
    );
    let error = rejected(
        survival.command(None, vec!["listgroups".into()]).await,
        ErrorCode::Forbidden,
    );
    assert!(error.contains("control = true"), "{error}");
    rejected(
        lobby.command(Some(alice), vec!["listgroups".into()]).await,
        ErrorCode::Forbidden,
    );

    // context: allowlisted keys from the player's own backend only.
    proxy.perms(&[
        "user",
        "Alice",
        "permission",
        "set",
        "warp.spawn",
        "true",
        "region=spawn",
    ]);
    let check = || proxy.perms(&["check", "Alice", "warp.spawn"]);
    assert!(check().contains("undefined"), "{}", check());
    survival
        .report_context(alice, ContextSet::of("region", "spawn").unwrap())
        .await
        .unwrap();
    eventually("the reported context", || check().contains("= true")).await;
    survival
        .report_context(
            alice,
            ContextSet::of("region", "market")
                .unwrap()
                .with("evil", "x")
                .unwrap(),
        )
        .await
        .unwrap();
    eventually("the replaced context", || check().contains("undefined")).await;
    assert!(
        proxy
            .logs
            .try_iter()
            .any(|line| line.contains("dropped: evil")),
        "the proxy logs dropped context keys"
    );
    assert!(!check().contains("evil"), "{}", check());
    lobby
        .report_context(alice, ContextSet::of("region", "spawn").unwrap())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        check().contains("undefined"),
        "the lobby cannot report for Alice"
    );

    // The retained stream holds every change.
    let tls = rift::messaging::quic::tls_client_config(&proxy.ca).unwrap();
    let raw = rift::messaging::quic::Client::connect(proxy.bus, "localhost", tls, SURVIVAL_TOKEN)
        .await
        .unwrap();
    raw.consumer(
        "rift.perms.changes",
        "e2e",
        rift::messaging::ConsumerConfig {
            filter_subject: "rift.perms.changes".into(),
            start_sequence: 1,
            ack_wait: Duration::from_secs(30),
            max_ack_pending: 256,
        },
    )
    .await
    .unwrap();
    let records = raw.fetch("rift.perms.changes", "e2e", 256).await.unwrap();
    let streamed: Vec<u64> = records
        .iter()
        .map(|record| {
            serde_json::from_slice::<ChangeEvent>(&record.record.message.payload)
                .unwrap()
                .revision
        })
        .collect();
    assert_eq!(streamed.first(), Some(&1));
    assert!(
        streamed.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{streamed:?}"
    );
    assert!(
        raw.publish("rift.perms.update", Bytes::from_static(b"{}"))
            .await
            .is_err(),
        "only the proxy publishes updates"
    );

    // sync: every backend says hello again.
    let synced = proxy.perms(&["sync"]);
    assert!(synced.contains("Asked every backend to resync"), "{synced}");
    assert!(synced.contains("backends said hello again"), "{synced}");
    assert!(!synced.contains("0 backends said hello again"), "{synced}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rift_dev_serves_a_backend_with_one_command() {
    let dir = std::env::temp_dir().join(format!("rift-dev-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let mut child = Command::new(env!("CARGO_BIN_EXE_rift"))
        .args(["dev", "--listen", "127.0.0.1:0", "--backend", "127.0.0.1:1"])
        .args([
            "--bus",
            "127.0.0.1:0",
            "--admin",
            "127.0.0.1:0",
            "--server",
            "paper",
        ])
        .arg("--dir")
        .arg(&dir)
        .env_remove("RIFT_DEV_TOKEN")
        .env_remove("RIFT_ADMIN_TOKEN")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (sender, lines) = mpsc::channel();
    let streams: [Box<dyn Read + Send>; 2] = [
        Box::new(child.stdout.take().unwrap()),
        Box::new(child.stderr.take().unwrap()),
    ];
    for stream in streams {
        let sender = sender.clone();
        thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
    }
    let (mut token, mut bus, mut admin, mut preset) = (None, None, None, false);
    let mut logs = Vec::new();
    while token.is_none() || bus.is_none() || admin.is_none() {
        let line = lines
            .recv_timeout(Duration::from_secs(15))
            .unwrap_or_else(|error| panic!("rift dev ({error}): {logs:?}"));
        if let Some(value) = line.strip_prefix("rift dev: export RIFT_DEV_TOKEN=") {
            token = Some(value.to_owned());
        }
        if let Some(address) = line.strip_prefix("rift: messaging QUIC on ") {
            bus = address.parse::<SocketAddr>().ok();
        }
        if let Some(address) = line.strip_prefix("rift: admin on ") {
            admin = address.parse::<SocketAddr>().ok();
        }
        preset |= line.contains("preset = \"backend\"");
        logs.push(line);
    }
    let token = token.unwrap();
    assert!(preset, "rift dev prints the ACL preset: {logs:?}");
    let mut config = Config::new(
        bus.unwrap().to_string(),
        "localhost",
        dir.join("ca.pem"),
        "UNUSED",
        "paper",
    );
    config.request_timeout = Duration::from_secs(5);
    let client = Client::start_with_token(config, &token).unwrap();
    assert!(client.wait_online(Duration::from_secs(10)).await);
    // The dev principal has control, so a plugin can test console writes.
    let reply = client
        .command(
            None,
            ["group", "vip", "create", "10"].map(String::from).to_vec(),
        )
        .await
        .unwrap();
    assert!(
        reply.component.to_string().contains("Created group vip"),
        "{}",
        reply.component
    );
    let output = Command::new(env!("CARGO_BIN_EXE_rift"))
        .args(["admin", "--address", &admin.unwrap().to_string()])
        .args(["perms", "listgroups"])
        .env("RIFT_ADMIN_TOKEN", &token)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("vip (weight 10)"));
    let _ = child.kill();
    let _ = child.wait();
    drop(lines);
    let _ = fs::remove_dir_all(&dir);
}
