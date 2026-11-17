//! A controller and a `rift node` agent talking over the real QUIC bus: named
//! servers on a remote node start, stop, take console commands, report logs,
//! recover from crashes, survive controller restarts and are reconciled.
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, Write},
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use rift::{
    config::Config,
    managed::{ManagedServers, Phase},
    messaging::{Broker, quic},
    node::{
        Nodes, agent,
        protocol::{self, Command},
    },
    players::PlayerRegistry,
};
use rustls::pki_types::PrivateKeyDer;

const TOKEN: &str = "rift-node-integration-token-with-32-bytes";
/// The second node, `node2`, for moves between nodes.
const TOKEN2: &str = "rift-node-integration-token-for-node2-32b";

/// A tiny game server, run by the agent from this test executable.
#[test]
fn node_server_helper() {
    let Ok(address) = fs::read_to_string("helper-address") else {
        return;
    };
    let listener = TcpListener::bind(address.trim()).unwrap();
    listener.set_nonblocking(true).unwrap();
    println!("helper stdout");
    // What a zero-configuration plugin would read; one line per start.
    let variable = |name: &str| std::env::var(name).unwrap_or_default();
    let mut record = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("bus-env")
        .unwrap();
    writeln!(
        record,
        "{}",
        serde_json::json!({
            "name": variable("RIFT_SERVER_NAME"),
            "token": variable("RIFT_BUS_TOKEN"),
            "address": variable("RIFT_BUS_ADDRESS"),
            "cert": variable("RIFT_BUS_CERT"),
        })
    )
    .unwrap();
    let stopping = Arc::new(AtomicBool::new(false));
    {
        let stopping = stopping.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let line = line.unwrap_or_default();
                if line == "stop" {
                    stopping.store(true, Ordering::Release);
                    break;
                }
                println!("command: {line}");
                fs::write("last-command", &line).unwrap();
            }
        });
    }
    while !stopping.load(Ordering::Acquire) {
        if fs::remove_file("crash").is_ok() {
            std::process::exit(3);
        }
        if fs::remove_file("finish").is_ok() {
            std::process::exit(0);
        }
        let _ = listener.accept();
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Fixture {
    root: PathBuf,
    config: Config,
    broker: Broker,
    nodes: Nodes,
    server: quic::Server,
    client_tls: quinn::ClientConfig,
    port: u16,
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Fixture {
    async fn new() -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "rift-node-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("agent/survival")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let port = free_port();
        fs::write(
            root.join("agent/survival/helper-address"),
            format!("127.0.0.1:{port}"),
        )
        .unwrap();
        let program = std::env::current_exe().unwrap();
        let config = Config::from_lua(
            &format!(
                "rift.config.listeners.public='127.0.0.1:0'
rift.config.backends.survival='127.0.0.1:{port}'
rift.config.routes.public='survival'
rift.config.messaging={{listen='127.0.0.1:0',certificate='c.pem',private_key='k.pem'}}
rift.config.nodes.node1={{address='127.0.0.1',memory_mb=4096,labels={{'games'}}}}
rift.config.nodes.node2={{address='127.0.0.1',memory_mb=4096,labels={{'games'}}}}
rift.config.managed_servers.survival={{node='node1',directory='survival',
  command={{{program:?},'--exact','node_server_helper','--nocapture'}},
  messaging={{preset='backend'}},
  start_timeout_ms=5000,stop_timeout_ms=2000,restart_delay_ms=100,restart_retries=2}}"
            ),
            "nodes.lua",
        )
        .unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let server_tls = quic::server_config_from_der(
            vec![cert.cert.der().clone()],
            PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
        )
        .unwrap();
        let client_tls = quic::client_config_from_der(vec![cert.cert.der().clone()]).unwrap();
        let broker = Broker::default();
        let identity = |node: &str, token: &str| {
            let (publish, subscribe) = protocol::principal_grants(node);
            quic::Identity {
                name: format!("node.{node}"),
                token: token.into(),
                publish,
                subscribe,
                control: false,
                players: false,
            }
        };
        let server = quic::Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_tls,
            broker.clone(),
            quic::AuthConfig {
                identities: vec![identity("node1", TOKEN), identity("node2", TOKEN2)],
                ..Default::default()
            },
        )
        .unwrap();
        // Agents publish every 2 s; a short timeout keeps outage tests fast.
        let nodes = Nodes::with_timeout(broker.clone(), &config, Duration::from_millis(2500));
        tokio::spawn(nodes.listen().unwrap());
        Self {
            root,
            config,
            broker,
            nodes,
            server,
            client_tls,
            port,
        }
    }

    fn agent(&self, web: Option<String>) -> Arc<agent::Agent> {
        self.agent_named("node1", web)
    }

    /// `node1` keeps its root in `agent`, `node2` in `agent2`.
    fn agent_named(&self, name: &str, web: Option<String>) -> Arc<agent::Agent> {
        let (token, root) = match name {
            "node1" => (TOKEN, "agent"),
            _ => (TOKEN2, "agent2"),
        };
        agent::Agent::new(agent::Options {
            controller: self.server.local_addr().unwrap().to_string(),
            server_name: "localhost".into(),
            ca: PathBuf::new(),
            name: name.into(),
            token: token.into(),
            root: self.root.join(root),
            web,
            allowed_programs: BTreeSet::from([std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned()]),
            forwarding_secret: Some("node secret".into()),
            forwarding_secret_env: "RIFT_FORWARDING_SECRET".into(),
        })
        .unwrap()
    }

    /// Connect an agent; dropping the handle disconnects it.
    async fn connect(&self, agent: &Arc<agent::Agent>) -> tokio::task::JoinHandle<()> {
        self.connect_with(agent, TOKEN).await
    }

    async fn connect_with(
        &self,
        agent: &Arc<agent::Agent>,
        token: &str,
    ) -> tokio::task::JoinHandle<()> {
        let client = quic::Client::connect(
            self.server.local_addr().unwrap(),
            "localhost",
            self.client_tls.clone(),
            token,
        )
        .await
        .unwrap();
        let agent = agent.clone();
        tokio::spawn(async move {
            let _ = agent.serve(client).await;
        })
    }

    fn managed(&self) -> ManagedServers {
        let managed = ManagedServers::with_nodes(
            &self.config,
            Arc::new(PlayerRegistry::default()),
            self.nodes.clone(),
        );
        managed.launch().unwrap();
        managed
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn phase(managed: &ManagedServers) -> Phase {
    managed.phase("survival").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn named_servers_on_a_node_start_stop_log_recover_and_reconcile() {
    let fixture = Fixture::new().await;
    let agent = fixture.agent(None);
    tokio::spawn(agent.clone().monitor());
    let connection = fixture.connect(&agent).await;
    eventually("node online", || fixture.nodes.online("node1")).await;
    let managed = fixture.managed();
    assert_eq!(managed.node_of("survival").as_deref(), Some("node1"));
    assert_eq!(managed.names_on("node1"), ["survival"]);

    managed.start("survival").await.unwrap();
    assert_eq!(phase(&managed), Phase::Running);
    let snapshot = managed.snapshot();
    assert_eq!(snapshot[0].node.as_deref(), Some("node1"));
    assert!(snapshot[0].pid.is_some());
    // The proxy reaches the server at the node address.
    std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();

    managed
        .console("survival", "say hello".into())
        .await
        .unwrap();
    let directory = fixture.root.join("agent/survival");
    eventually("console command", || {
        fs::read_to_string(directory.join("last-command")).is_ok_and(|c| c == "say hello")
    })
    .await;
    let logs = managed.logs("survival", Some(0)).await.unwrap();
    assert!(
        logs["text"].as_str().unwrap().contains("helper stdout"),
        "{logs}"
    );

    // A crash on the node is reported and recovered on the same node.
    let first_pid = managed.snapshot()[0].pid;
    fs::write(directory.join("crash"), "").unwrap();
    eventually("crash detected", || {
        managed.snapshot()[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("on node node1"))
    })
    .await;
    eventually("recovery", || {
        phase(&managed) == Phase::Running && managed.snapshot()[0].pid != first_pid
    })
    .await;
    // A clean exit on the node is a stop: no error, no retry spent.
    let attempts = managed.snapshot()[0].restart_attempts;
    fs::write(directory.join("finish"), "").unwrap();
    eventually("clean exit", || phase(&managed) == Phase::Stopped).await;
    let server = &managed.snapshot()[0];
    assert!(server.last_error.is_none(), "{server:?}");
    assert_eq!(server.restart_attempts, attempts);
    assert!(server.automatic_enabled);
    managed.start("survival").await.unwrap();

    managed.stop("survival").await.unwrap();
    assert_eq!(phase(&managed), Phase::Stopped);
    eventually("agent stopped the process", || {
        std::net::TcpStream::connect(("127.0.0.1", fixture.port)).is_err()
    })
    .await;

    // A controller restart keeps the remote server running and adopts it.
    managed.start("survival").await.unwrap();
    let pid = managed.snapshot()[0].pid;
    managed.shutdown().await;
    std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
    let restarted = fixture.managed();
    eventually("adoption after controller restart", || {
        phase(&restarted) == Phase::Running && restarted.snapshot()[0].pid == pid
    })
    .await;

    // Reconciliation stops servers the controller no longer assigns. Detach
    // this controller first; a controller that still manages the server
    // would correctly restart it after the unexpected exit.
    restarted.shutdown().await;
    std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
    let result = fixture
        .nodes
        .request(
            "node1",
            &Command::Reconcile { servers: vec![] },
            Duration::from_secs(20),
        )
        .await
        .unwrap();
    assert_eq!(result["stopped"], serde_json::json!(["survival"]));
    eventually("reconciled stop", || {
        std::net::TcpStream::connect(("127.0.0.1", fixture.port)).is_err()
    })
    .await;
    // Nothing restarts it afterwards.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(std::net::TcpStream::connect(("127.0.0.1", fixture.port)).is_err());
    assert!(agent.state().await.servers.is_empty());
    connection.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn servers_keep_running_while_the_node_is_unreachable_and_report_unknown() {
    let fixture = Fixture::new().await;
    let agent = fixture.agent(None);
    tokio::spawn(agent.clone().monitor());
    let connection = fixture.connect(&agent).await;
    eventually("node online", || fixture.nodes.online("node1")).await;
    let managed = fixture.managed();
    managed.start("survival").await.unwrap();
    let pid = managed.snapshot()[0].pid;
    connection.abort();
    eventually("unknown phase", || phase(&managed) == Phase::Unknown).await;
    assert!(!managed.can_connect("survival"));
    assert!(managed.start("survival").await.is_err());
    // The agent kept the process; the game server still accepts players.
    std::net::TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
    let connection = fixture.connect(&agent).await;
    eventually("running after reconnection", || {
        phase(&managed) == Phase::Running
    })
    .await;
    assert_eq!(managed.snapshot()[0].pid, pid);
    assert_eq!(managed.snapshot()[0].restart_attempts, 0);
    managed.stop("survival").await.unwrap();
    managed.shutdown().await;
    connection.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_principal_has_one_session_and_the_newest_wins() {
    let fixture = Fixture::new().await;
    let connect = || {
        quic::Client::connect(
            fixture.server.local_addr().unwrap(),
            "localhost",
            fixture.client_tls.clone(),
            TOKEN,
        )
    };
    let subject = protocol::command_subject("node1");
    let first = connect().await.unwrap();
    let mut first_commands = first.subscribe(&subject, None).await.unwrap();
    let second = connect().await.unwrap();
    let mut second_commands = second.subscribe(&subject, None).await.unwrap();
    // The older session is closed instead of receiving commands alongside.
    tokio::time::timeout(Duration::from_secs(5), first_commands.recv())
        .await
        .expect("the first session should be closed")
        .unwrap_err();
    fixture
        .broker
        .publish(&subject, bytes::Bytes::from_static(b"{}"))
        .unwrap();
    let message = tokio::time::timeout(Duration::from_secs(5), second_commands.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&message.payload[..], b"{}");
    assert!(first.subscribe(&subject, None).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_refuse_unlisted_programs_and_paths_outside_their_root() {
    let fixture = Fixture::new().await;
    let agent = fixture.agent(None);
    let connection = fixture.connect(&agent).await;
    eventually("node online", || fixture.nodes.online("node1")).await;
    let start = |command: Vec<String>, directory: &str| {
        Command::Start(protocol::Start {
            server: "survival".into(),
            command,
            directory: directory.into(),
            port: fixture.port,
            environment: Default::default(),
            bus_certificate: None,
            isolation: Default::default(),
            memory_limit: None,
        })
    };
    let wait = Duration::from_secs(5);
    let error = fixture
        .nodes
        .request(
            "node1",
            &start(vec!["/bin/sh".into(), "-c".into(), "id".into()], "survival"),
            wait,
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied,
        "{error}"
    );
    let program = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    for directory in ["../escape", "/tmp", "survival/../../escape"] {
        let error = fixture
            .nodes
            .request("node1", &start(vec![program.clone()], directory), wait)
            .await
            .unwrap_err();
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "{directory}: {error}"
        );
    }
    // A port held by something else is never adopted.
    let holder = TcpListener::bind(("0.0.0.0", fixture.port)).unwrap();
    let error = fixture
        .nodes
        .request("node1", &start(vec![program.clone()], "survival"), wait)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse, "{error}");
    drop(holder);
    // Remote file access stays inside the server directory.
    let files = |method: &str, path: &str, content: Option<&[u8]>| {
        Command::Files(protocol::Files {
            server: "survival".into(),
            directory: "survival".into(),
            method: method.into(),
            path: path.into(),
            folder: false,
            content: content.map(protocol::encode_base64),
        })
    };
    fixture
        .nodes
        .request(
            "node1",
            &files("PUT", "plugins/config.yml", Some(b"a: 1")),
            wait,
        )
        .await
        .unwrap();
    let read = fixture
        .nodes
        .request("node1", &files("GET", "plugins/config.yml", None), wait)
        .await
        .unwrap();
    assert_eq!(
        protocol::decode_base64(read["content"].as_str().unwrap()).unwrap(),
        b"a: 1"
    );
    for path in ["../x", ".rift-instance.json"] {
        assert!(
            fixture
                .nodes
                .request("node1", &files("PUT", path, Some(b"x")), wait)
                .await
                .is_err(),
            "{path}"
        );
    }
    connection.abort();
}

/// Serves `/api/files/templates` like the controller's web API, for agents.
async fn template_server(storage: PathBuf) -> SocketAddr {
    use axum::{
        Router,
        extract::{Path, RawQuery},
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
    };
    let app = Router::new().route(
        "/api/files/templates/{*path}",
        get(
            move |Path(path): Path<String>, RawQuery(query): RawQuery, headers: HeaderMap| {
                let storage = storage.clone();
                async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok())
                        != Some(&format!("Bearer {TOKEN}"))
                    {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let segments = rift::files::segments(&path).unwrap();
                    let target = rift::files::resolve(&storage, &segments).unwrap();
                    if query.as_deref() == Some("manifest") {
                        axum::Json(rift::files::manifest(&target).unwrap()).into_response()
                    } else {
                        fs::read(target).unwrap().into_response()
                    }
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_build_instances_from_fetched_templates_and_delete_only_disposable_ones() {
    let fixture = Fixture::new().await;
    let storage = fixture.root.join("templates");
    fs::create_dir_all(storage.join("Global/server/config")).unwrap();
    fs::create_dir_all(storage.join("bedwars/map/region")).unwrap();
    fs::write(
        storage.join("Global/server/server.properties"),
        "motd=global\n",
    )
    .unwrap();
    fs::write(storage.join("Global/server/config/a.yml"), "a: 1\n").unwrap();
    fs::write(storage.join("bedwars/server.jar"), b"jar bytes").unwrap();
    fs::write(storage.join("bedwars/game.jar"), b"plugin bytes").unwrap();
    fs::write(storage.join("bedwars/map/level.dat"), b"map").unwrap();
    fs::write(storage.join("bedwars/map/region/r.0.0.mca"), b"region").unwrap();
    let web = template_server(storage.clone()).await;
    let agent = fixture.agent(Some(format!("http://{web}")));
    let connection = fixture.connect(&agent).await;
    eventually("node online", || fixture.nodes.online("node1")).await;
    let prepare = |name: &str, storage| protocol::Prepare {
        server: name.into(),
        group: "bedwars".into(),
        directory: format!("temp/services/{name}").into(),
        template: Some("bedwars".into()),
        storage,
        port: 30000,
        bind: "127.0.0.1".into(),
        software: None,
        server_jar: Some("bedwars/server.jar".into()),
        configs: vec!["Global/server".into()],
        plugins: vec!["bedwars/game.jar".into()],
        map: Some("bedwars/map".into()),
        accept_eula: true,
        forwarding: Some(true),
        forwarding_mode: None,
        forwarding_secret: None,
        properties: vec![("online-mode".into(), "false".into(), true)],
    };
    let wait = Duration::from_secs(20);
    let disposable = rift::config::InstanceStorage::Disposable;
    let result = fixture
        .nodes
        .request(
            "node1",
            &Command::Prepare(Box::new(prepare("bedwars-1", disposable))),
            wait,
        )
        .await
        .unwrap();
    assert_eq!(result["created"], true, "{result}");
    let directory = fixture.root.join("agent/temp/services/bedwars-1");
    assert_eq!(
        fs::read(directory.join("server.jar")).unwrap(),
        b"jar bytes"
    );
    assert_eq!(
        fs::read(directory.join("plugins/game.jar")).unwrap(),
        b"plugin bytes"
    );
    assert_eq!(
        fs::read(directory.join("world/region/r.0.0.mca")).unwrap(),
        b"region"
    );
    assert_eq!(fs::read(directory.join("config/a.yml")).unwrap(), b"a: 1\n");
    assert!(
        fs::read_to_string(directory.join("eula.txt"))
            .unwrap()
            .contains("eula=true")
    );
    let properties = fs::read_to_string(directory.join("server.properties")).unwrap();
    assert!(properties.contains("motd=global"), "{properties}");
    assert!(
        properties.contains("server-port=30000\nserver-ip=127.0.0.1\n"),
        "{properties}"
    );
    assert!(properties.contains("online-mode=false"), "{properties}");
    assert!(
        fs::read_to_string(directory.join("config/paper-global.yml"))
            .unwrap()
            .contains("secret: \"node secret\"")
    );
    // Template changes reach the next instance; removed files disappear.
    fs::write(storage.join("Global/server/config/a.yml"), "a: 2\n").unwrap();
    fs::remove_file(storage.join("bedwars/map/region/r.0.0.mca")).unwrap();
    // The controller's sealed secret wins over the agent's own copy.
    let mut sealed = prepare("bedwars-2", disposable);
    sealed.forwarding_secret =
        Some(rift::node::seal::seal(TOKEN, "node1", b"controller secret").unwrap());
    fixture
        .nodes
        .request("node1", &Command::Prepare(Box::new(sealed)), wait)
        .await
        .unwrap();
    let second = fixture.root.join("agent/temp/services/bedwars-2");
    assert_eq!(fs::read(second.join("config/a.yml")).unwrap(), b"a: 2\n");
    assert!(!second.join("world/region/r.0.0.mca").exists());
    assert!(
        fs::read_to_string(second.join("config/paper-global.yml"))
            .unwrap()
            .contains("secret: \"controller secret\"")
    );
    // A secret sealed with another token never opens.
    let mut foreign = prepare("bedwars-3", disposable);
    foreign.forwarding_secret = Some(
        rift::node::seal::seal("another-node-token-with-at-least-32-bytes", "node1", b"x").unwrap(),
    );
    let error = fixture
        .nodes
        .request("node1", &Command::Prepare(Box::new(foreign)), wait)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("could not be opened"), "{error}");
    assert!(!fixture.root.join("agent/temp/services/bedwars-3").exists());
    // A disposable directory that already exists is never reused.
    assert!(
        fixture
            .nodes
            .request(
                "node1",
                &Command::Prepare(Box::new(prepare("bedwars-1", disposable))),
                wait,
            )
            .await
            .is_err()
    );
    let removed = fixture
        .nodes
        .request(
            "node1",
            &Command::Remove {
                server: "bedwars-1".into(),
                directory: "temp/services/bedwars-1".into(),
                storage: disposable,
            },
            wait,
        )
        .await
        .unwrap();
    assert_eq!(removed["removed"], true);
    assert!(!directory.exists());
    // Persistent data survives removal requests.
    let persistent = rift::config::InstanceStorage::Persistent;
    let mut world = prepare("lobby-1", persistent);
    world.directory = "local/services/lobby-1".into();
    fixture
        .nodes
        .request("node1", &Command::Prepare(Box::new(world.clone())), wait)
        .await
        .unwrap();
    let removed = fixture
        .nodes
        .request(
            "node1",
            &Command::Remove {
                server: "lobby-1".into(),
                directory: world.directory.clone(),
                storage: persistent,
            },
            wait,
        )
        .await
        .unwrap();
    assert_eq!(removed["removed"], false);
    assert!(
        fixture
            .root
            .join("agent/local/services/lobby-1/world")
            .is_dir()
    );
    // Reprovisioning a persistent world keeps it and refreshes the port.
    world.port = 30001;
    let again = fixture
        .nodes
        .request("node1", &Command::Prepare(Box::new(world)), wait)
        .await
        .unwrap();
    assert_eq!(again["created"], false);
    assert!(
        fs::read_to_string(
            fixture
                .root
                .join("agent/local/services/lobby-1/server.properties")
        )
        .unwrap()
        .contains("server-port=30001")
    );
    connection.abort();
    let _ = fixture.broker.subscription_count();
}

#[tokio::test(flavor = "multi_thread")]
async fn warming_fills_the_template_cache_before_the_first_instance() {
    let fixture = Fixture::new().await;
    let storage = fixture.root.join("templates");
    fs::create_dir_all(storage.join("Global/server")).unwrap();
    fs::create_dir_all(storage.join("bedwars/map")).unwrap();
    fs::write(storage.join("Global/server/server.properties"), "motd=x\n").unwrap();
    fs::write(storage.join("bedwars/server.jar"), b"jar bytes").unwrap();
    fs::write(storage.join("bedwars/game.jar"), b"plugin").unwrap();
    fs::write(storage.join("bedwars/map/level.dat"), b"map").unwrap();
    let web = template_server(storage.clone()).await;
    let agent = fixture.agent(Some(format!("http://{web}")));
    let connection = fixture.connect(&agent).await;
    eventually("node online", || fixture.nodes.online("node1")).await;
    let bedwars = protocol::TemplateAssets {
        template: "bedwars".into(),
        software: None,
        server_jar: Some("bedwars/server.jar".into()),
        configs: vec!["Global/server".into()],
        plugins: vec!["bedwars/game.jar".into()],
        map: Some("bedwars/map".into()),
    };
    let broken = protocol::TemplateAssets {
        template: "broken".into(),
        software: None,
        server_jar: Some("../outside.jar".into()),
        configs: vec![],
        plugins: vec![],
        map: None,
    };
    let warm = Command::Warm(protocol::Warm {
        templates: vec![broken, bedwars.clone()],
    });
    let wait = Duration::from_secs(20);
    let reply = fixture.nodes.request("node1", &warm, wait).await.unwrap();
    let results = reply["templates"].as_array().unwrap();
    // One bad template does not stop the others.
    assert_eq!(results[0]["template"], "broken");
    assert_eq!(results[0]["ok"], false, "{reply}");
    assert_eq!(results[1]["ok"], true, "{reply}");
    assert_eq!(results[1]["files"], 4, "{reply}");
    assert_eq!(results[1]["downloaded"], 4, "{reply}");
    assert_eq!(results[1]["bytes"], 9 + 6 + 3 + 7, "{reply}");
    let cache = fixture.root.join("agent/cache/templates");
    assert_eq!(fs::read(cache.join("bedwars/game.jar")).unwrap(), b"plugin");
    assert_eq!(
        fs::read(cache.join("bedwars/map/level.dat")).unwrap(),
        b"map"
    );
    // A second warm finds everything cached and downloads only changes.
    fs::write(storage.join("bedwars/game.jar"), b"plugin v2").unwrap();
    let warm = Command::Warm(protocol::Warm {
        templates: vec![bedwars],
    });
    let reply = fixture.nodes.request("node1", &warm, wait).await.unwrap();
    assert_eq!(reply["templates"][0]["downloaded"], 1, "{reply}");
    assert_eq!(reply["templates"][0]["bytes"], 9, "{reply}");
    assert_eq!(
        fs::read(cache.join("bedwars/game.jar")).unwrap(),
        b"plugin v2"
    );
    connection.abort();
}

#[test]
fn agent_options_require_a_token_and_default_to_java() {
    let args = |extra: &[&str]| -> Vec<String> {
        [
            "--controller",
            "10.0.0.2:7443",
            "--name",
            "node1",
            "--root",
            "/srv/rift",
            "--ca",
            "ca.pem",
            "--token-env",
            "RIFT_NODE_OPTIONS_TEST_TOKEN",
        ]
        .iter()
        .chain(extra)
        .map(|value| (*value).to_owned())
        .collect()
    };
    let error = agent::Options::parse(&args(&[])).unwrap_err();
    assert!(
        error.to_string().contains("RIFT_NODE_OPTIONS_TEST_TOKEN"),
        "{error}"
    );
    // SAFETY: the variable name is unique to this test.
    unsafe { std::env::set_var("RIFT_NODE_OPTIONS_TEST_TOKEN", TOKEN) };
    let options = agent::Options::parse(&args(&[])).unwrap();
    assert_eq!(options.server_name, "10.0.0.2");
    assert_eq!(
        options.allowed_programs,
        BTreeSet::from(["java".to_owned()])
    );
    let options = agent::Options::parse(&args(&[
        "--allow-program",
        "/usr/bin/java",
        "--web",
        "https://10.0.0.2:8443/",
    ]))
    .unwrap();
    assert_eq!(
        options.allowed_programs,
        BTreeSet::from(["/usr/bin/java".to_owned()])
    );
    assert_eq!(options.web.as_deref(), Some("https://10.0.0.2:8443"));
    // Plain HTTP would expose the token and template hashes; loopback only.
    for web in [
        "http://127.0.0.1:8080",
        "http://[::1]:8080",
        "http://localhost:8080",
    ] {
        agent::Options::parse(&args(&["--web", web])).unwrap();
    }
    let error = agent::Options::parse(&args(&["--web", "http://10.0.0.2:8080"])).unwrap_err();
    assert!(error.to_string().contains("https://"), "{error}");
    // A fallback forwarding secret, for controllers that send none.
    assert_eq!(options.forwarding_secret_env, "RIFT_FORWARDING_SECRET");
    let secret_env = ["--forwarding-secret-env", "RIFT_NODE_OPTIONS_TEST_SECRET"];
    let error = agent::Options::parse(&args(&secret_env)).unwrap_err();
    assert!(
        error.to_string().contains("RIFT_NODE_OPTIONS_TEST_SECRET"),
        "{error}"
    );
    // SAFETY: the variable name is unique to this test.
    unsafe { std::env::set_var("RIFT_NODE_OPTIONS_TEST_SECRET", "velocity") };
    let options = agent::Options::parse(&args(&secret_env)).unwrap();
    assert_eq!(options.forwarding_secret.as_deref(), Some("velocity"));
    for extra in [
        &["--web", "ftp://x"][..],
        &["--web", "http://rift.example"],
        &["--bogus", "1"],
        &["--name", "a"],
    ] {
        assert!(agent::Options::parse(&args(extra)).is_err(), "{extra:?}");
    }
    let _ = std::io::stdout().flush();
}

/// A server on a node gets its own bus login in its start command: the agent
/// writes the certificate and passes the token, which authenticates as that
/// server and no other, also after the controller recovers a crash. The node
/// never sees the key.
#[tokio::test(flavor = "multi_thread")]
async fn servers_on_a_node_start_with_their_own_bus_login() {
    use rift::messaging::quic::DynamicIdentities;
    let fixture = Fixture::new().await;
    let agent = fixture.agent(None);
    tokio::spawn(agent.clone().monitor());
    let connection = fixture.connect(&agent).await;
    eventually("node online", || fixture.nodes.online("node1")).await;
    let logins = Arc::new(rift::instance_login::Logins::with_key([9; 32]));
    logins.set_bus(rift::instance_login::Bus {
        address: "10.0.0.2:7443".into(),
        server_name: "rift.internal".into(),
        certificate: "-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n".into(),
    });
    let managed = fixture.managed();
    managed.set_logins(logins.clone(), &fixture.config).unwrap();
    managed.start("survival").await.unwrap();
    let directory = fixture.root.join("agent/survival");
    let records = || -> Vec<serde_json::Value> {
        fs::read_to_string(directory.join("bus-env"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    };
    eventually("the server started", || !records().is_empty()).await;
    let first = &records()[0];
    assert_eq!(first["name"], "survival");
    assert_eq!(first["address"], "10.0.0.2:7443");
    let token = first["token"].as_str().unwrap();
    // Static managed servers have generation 0; instances count creations.
    assert_eq!(token, logins.token("survival", 0));
    assert_eq!(logins.authenticate(token).unwrap().name, "server.survival");
    let cert = PathBuf::from(first["cert"].as_str().unwrap());
    assert!(cert.is_absolute(), "{cert:?}");
    assert_eq!(cert, directory.join(rift::instance_login::CERTIFICATE_FILE));
    assert!(
        fs::read_to_string(&cert)
            .unwrap()
            .contains("BEGIN CERTIFICATE")
    );
    assert!(
        !fs::read_dir(fixture.root.join("agent"))
            .unwrap()
            .any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("instance-key")
            })
    );
    // The controller recovers a crash with another start command, which
    // carries the token again.
    fs::write(directory.join("crash"), "").unwrap();
    eventually("recovered with its login", || records().len() >= 2).await;
    assert_eq!(records()[1]["token"], token);
    // Removing the login (the instance) makes the token useless.
    logins.unregister("survival");
    assert!(logins.authenticate(token).is_none());
    managed.stop("survival").await.unwrap();
    managed.shutdown().await;
    connection.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_servers_folder_moves_between_agents_through_the_controller() {
    use rift::node::transfer;
    let fixture = Fixture::new().await;
    let source_agent = fixture.agent_named("node1", None);
    let target_agent = fixture.agent_named("node2", None);
    let source_connection = fixture.connect_with(&source_agent, TOKEN).await;
    let target_connection = fixture.connect_with(&target_agent, TOKEN2).await;
    eventually("both nodes online", || {
        fixture.nodes.online("node1") && fixture.nodes.online("node2")
    })
    .await;
    let relative = std::path::Path::new("local/services/lobby-1");
    let source = fixture.root.join("agent").join(relative);
    let target = fixture.root.join("agent2").join(relative);
    // Several chunks, so the next read overlaps the previous write.
    let region: Vec<u8> = (0..protocol::MAX_REMOTE_FILE * 3 + 5)
        .map(|index| (index * 7 % 256) as u8)
        .collect();
    fs::create_dir_all(source.join("world/region")).unwrap();
    fs::create_dir_all(source.join("plugins/empty")).unwrap();
    fs::write(source.join("world/region/r.0.0.mca"), &region).unwrap();
    fs::write(source.join("world/level.dat"), b"level").unwrap();
    fs::write(source.join("server.properties"), b"server-port=30000\n").unwrap();
    fs::write(source.join("blank.txt"), b"").unwrap();
    let copied = transfer::copy(&fixture.nodes, "lobby-1", "node1", "node2", relative)
        .await
        .unwrap();
    assert_eq!(copied.files, 4);
    assert_eq!(copied.folders, 4);
    assert_eq!(copied.bytes, region.len() as u64 + 5 + 18);
    // Staged next to the destination until the controller commits.
    assert!(!target.exists());
    assert!(
        fixture
            .root
            .join("agent2/local/services/.rift-move-lobby-1")
            .is_dir()
    );
    transfer::commit(&fixture.nodes, "lobby-1", "node2", relative)
        .await
        .unwrap();
    assert_eq!(
        fs::read(target.join("world/region/r.0.0.mca")).unwrap(),
        region
    );
    assert_eq!(fs::read(target.join("world/level.dat")).unwrap(), b"level");
    assert_eq!(fs::read(target.join("blank.txt")).unwrap(), b"");
    assert!(target.join("plugins/empty").is_dir());
    assert!(target.join(protocol::MOVE_MARKER).exists());
    transfer::finish(&fixture.nodes, "lobby-1", "node2", relative)
        .await
        .unwrap();
    assert!(!target.join(protocol::MOVE_MARKER).exists());
    // A recorded folder is never replaced by another copy, and a failed
    // copy leaves no staging folder and the source untouched.
    let error = transfer::copy(&fixture.nodes, "lobby-1", "node1", "node2", relative)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
    assert!(
        !fixture
            .root
            .join("agent2/local/services/.rift-move-lobby-1")
            .exists()
    );
    assert!(source.join("world/level.dat").exists());
    // The old folder goes once the move is recorded; only its own server's.
    let discard = |server: &str| Command::Discard {
        server: server.into(),
        directory: relative.into(),
    };
    let wait = Duration::from_secs(20);
    assert!(
        fixture
            .nodes
            .request("node1", &discard("lobby-2"), wait)
            .await
            .is_err()
    );
    let removed = fixture
        .nodes
        .request("node1", &discard("lobby-1"), wait)
        .await
        .unwrap();
    assert_eq!(removed["removed"], true);
    assert!(!source.exists());
    assert!(target.join("world/region/r.0.0.mca").exists());
    source_connection.abort();
    target_connection.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_is_parked_for_a_move_and_never_starts_meanwhile() {
    let fixture = Fixture::new().await;
    let agent = fixture.agent(None);
    tokio::spawn(agent.clone().monitor());
    let connection = fixture.connect(&agent).await;
    eventually("node online", || fixture.nodes.online("node1")).await;
    let managed = fixture.managed();
    managed.start("survival").await.unwrap();
    assert_eq!(phase(&managed), Phase::Running);
    let parked = managed.park("survival").await.unwrap();
    assert!(parked.was_running() && parked.automatic());
    assert_eq!(phase(&managed), Phase::Stopped);
    assert!(managed.is_moving("survival"));
    assert!(managed.snapshot()[0].moving);
    // The agent no longer runs it, so its folder may be read for the move.
    let tree = Command::Tree {
        server: "survival".into(),
        directory: "survival".into(),
        offset: 0,
    };
    eventually("process gone on the node", || {
        fixture
            .nodes
            .observe("node1", "survival")
            .is_some_and(|o| o.running.is_none())
    })
    .await;
    fixture
        .nodes
        .request("node1", &tree, Duration::from_secs(5))
        .await
        .unwrap();
    // No start gets through while parked, and nothing else can take it.
    assert!(managed.start("survival").await.is_err());
    assert!(managed.request_scale_start("survival").is_err());
    assert!(managed.ensure_running("survival").await.is_err());
    assert!(!managed.can_connect("survival"));
    assert!(managed.park("survival").await.is_err());
    assert!(managed.remove("survival").await.is_err());
    // Abandoning the move starts it again; the stop was not an
    // administrator stop.
    parked.resume();
    assert!(!managed.is_moving("survival"));
    eventually("running again", || phase(&managed) == Phase::Running).await;
    // A running server's folder is never read for a move.
    let error = fixture
        .nodes
        .request("node1", &tree, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::ResourceBusy, "{error}");
    managed.stop("survival").await.unwrap();
    managed.shutdown().await;
    connection.abort();
}
