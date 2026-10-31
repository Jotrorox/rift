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

/// A tiny game server, run by the agent from this test executable.
#[test]
fn node_server_helper() {
    let Ok(address) = fs::read_to_string("helper-address") else {
        return;
    };
    let listener = TcpListener::bind(address.trim()).unwrap();
    listener.set_nonblocking(true).unwrap();
    println!("helper stdout");
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
rift.config.managed_servers.survival={{node='node1',directory='survival',
  command={{{program:?},'--exact','node_server_helper','--nocapture'}},
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
        let (publish, subscribe) = protocol::principal_grants("node1");
        let server = quic::Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_tls,
            broker.clone(),
            quic::AuthConfig {
                identities: vec![quic::Identity {
                    name: "node.node1".into(),
                    token: TOKEN.into(),
                    publish,
                    subscribe,
                    control: false,
                }],
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
        agent::Agent::new(agent::Options {
            controller: self.server.local_addr().unwrap().to_string(),
            server_name: "localhost".into(),
            ca: PathBuf::new(),
            name: "node1".into(),
            token: TOKEN.into(),
            root: self.root.join("agent"),
            web,
            allowed_programs: BTreeSet::from([std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned()]),
        })
        .unwrap()
    }

    /// Connect an agent; dropping the handle disconnects it.
    async fn connect(&self, agent: &Arc<agent::Agent>) -> tokio::task::JoinHandle<()> {
        let client = quic::Client::connect(
            self.server.local_addr().unwrap(),
            "localhost",
            self.client_tls.clone(),
            TOKEN,
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
        forwarding: Some(("secret".into(), true)),
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
            .contains("secret: \"secret\"")
    );
    // Template changes reach the next instance; removed files disappear.
    fs::write(storage.join("Global/server/config/a.yml"), "a: 2\n").unwrap();
    fs::remove_file(storage.join("bedwars/map/region/r.0.0.mca")).unwrap();
    fixture
        .nodes
        .request(
            "node1",
            &Command::Prepare(Box::new(prepare("bedwars-2", disposable))),
            wait,
        )
        .await
        .unwrap();
    let second = fixture.root.join("agent/temp/services/bedwars-2");
    assert_eq!(fs::read(second.join("config/a.yml")).unwrap(), b"a: 2\n");
    assert!(!second.join("world/region/r.0.0.mca").exists());
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
        "http://10.0.0.2:8080/",
    ]))
    .unwrap();
    assert_eq!(
        options.allowed_programs,
        BTreeSet::from(["/usr/bin/java".to_owned()])
    );
    assert_eq!(options.web.as_deref(), Some("http://10.0.0.2:8080"));
    for extra in [
        &["--web", "ftp://x"][..],
        &["--bogus", "1"],
        &["--name", "a"],
    ] {
        assert!(agent::Options::parse(&args(extra)).is_err(), "{extra:?}");
    }
    let _ = std::io::stdout().flush();
}
