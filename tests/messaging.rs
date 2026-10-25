//! End-to-end plugin messaging against the real Rift executable over QUIC.
use bytes::Bytes;
use rift::messaging::{ConsumerConfig, quic};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader},
    net::SocketAddr,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

const TOKEN: &str = "rift-messaging-integration-secret-32-bytes";

struct Fixture {
    path: PathBuf,
    tls: quinn::ClientConfig,
}
impl Fixture {
    fn new() -> Self {
        static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rift-messaging-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        fs::write(path.join("cert.pem"), cert.cert.pem()).unwrap();
        fs::write(path.join("key.pem"), cert.signing_key.serialize_pem()).unwrap();
        let tls = quic::client_config_from_der(vec![cert.cert.der().clone()]).unwrap();
        Self { path, tls }
    }

    fn write(&self, reply: &str, extra: &str) {
        fs::write(
            self.path.join("rift.lua"),
            format!(
                r#"
return {{
    listeners = {{ main = '127.0.0.1:0' }},
    backends = {{ lobby = '127.0.0.1:1' }},
    routes = {{ main = 'lobby' }},
    messaging = {{
        listen = '127.0.0.1:0', certificate = 'cert.pem', private_key = 'key.pem',
        principals = {{ {{ name = 'plugin', token_env = 'RIFT_MESSAGING_TEST_TOKEN',
            publish = {{ 'plugin.>', 'rift.control.*', '_INBOX.>' }},
            subscribe = {{ 'plugin.>', 'rift.events.>', '_INBOX.>' }}, control = true }} }},
        subscriptions = {{ {{ subject = 'plugin.echo', queue = 'lua' }} }},
        streams = {{ jobs = {{ subjects = {{ 'plugin.jobs' }}, storage_path = 'stream-data' }} }},
        {extra}
    }},
    on_message = function(message) rift.publish(message.reply, {reply}) end,
}}
"#
            ),
        )
        .unwrap();
    }

    fn start(&self) -> Process {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rift"))
            .current_dir(&self.path)
            .args(["--config", "rift.lua"])
            .env("RIFT_MESSAGING_TEST_TOKEN", TOKEN)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut process = Process {
            child,
            address: "127.0.0.1:0".parse().unwrap(),
            _logs: receiver,
        };
        let mut lines = Vec::new();
        loop {
            let line = process
                ._logs
                .recv_timeout(Duration::from_secs(15))
                .unwrap_or_else(|error| panic!("messaging startup: {error}; {lines:?}"));
            if let Some(address) = line.strip_prefix("rift: messaging QUIC on ") {
                process.address = address.parse().unwrap();
                return process;
            }
            lines.push(line);
        }
    }
    async fn connect(&self, process: &Process) -> quic::Client {
        quic::Client::connect(process.address, "localhost", self.tls.clone(), TOKEN)
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
struct Process {
    child: Child,
    address: SocketAddr,
    _logs: mpsc::Receiver<String>,
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn control(client: &quic::Client, subject: &str, value: Value) -> Value {
    let message = client
        .request(
            subject,
            Bytes::from(value.to_string()),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    serde_json::from_slice(&message.payload).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugins_controls_reload_and_durable_consumers_share_one_runtime() {
    let fixture = Fixture::new();
    fixture.write("message.payload", "");
    let process = fixture.start();
    let client = fixture.connect(&process).await;
    let payload = Bytes::from_static(b"\0\xffbinary plugin payload");
    assert_eq!(
        client
            .request("plugin.echo", payload.clone(), Duration::from_secs(5))
            .await
            .unwrap()
            .payload,
        payload
    );
    let mut events = client.subscribe("rift.events.control", None).await.unwrap();
    assert_eq!(
        control(&client, "rift.control.maintenance", json!({"enabled":true})).await["ok"],
        true
    );
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&event.payload).unwrap()["command"],
        "maintenance"
    );
    assert_eq!(
        control(&client, "rift.control.status", json!({})).await["data"]["maintenance"],
        true
    );
    assert_eq!(
        control(
            &client,
            "rift.control.drain",
            json!({"backend":"lobby","enabled":true})
        )
        .await["ok"],
        true
    );
    assert_eq!(
        control(
            &client,
            "rift.control.transfer",
            json!({"backend":"missing","connection_id":1})
        )
        .await["ok"],
        false
    );
    assert!(
        client
            .publish("rift.events.control", Bytes::from_static(b"spoof"))
            .await
            .is_err()
    );

    fixture.write("'reloaded'", "");
    assert_eq!(
        control(&client, "rift.control.reload", json!({})).await["ok"],
        true
    );
    assert_eq!(
        client
            .request("plugin.echo", Bytes::new(), Duration::from_secs(5))
            .await
            .unwrap()
            .payload,
        "reloaded"
    );
    // A configuration that requires new broker limits must not replace live state.
    fixture.write("'invalid replacement'", "subscription_capacity = 2048,");
    assert_eq!(
        control(&client, "rift.control.reload", json!({})).await["ok"],
        false
    );
    assert_eq!(
        client
            .request("plugin.echo", Bytes::new(), Duration::from_secs(5))
            .await
            .unwrap()
            .payload,
        "reloaded"
    );
    fixture.write("message.payload", "");

    let first = client
        .stream_publish("jobs", "plugin.jobs", None, Bytes::from_static(b"first"))
        .await
        .unwrap();
    let second = client
        .stream_publish("jobs", "plugin.jobs", None, Bytes::from_static(b"second"))
        .await
        .unwrap();
    client
        .consumer(
            "jobs",
            "worker",
            ConsumerConfig {
                filter_subject: "plugin.jobs".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let batch = client.fetch("jobs", "worker", 2).await.unwrap();
    assert_eq!(batch.len(), 2);
    client.ack("jobs", "worker", first.sequence).await.unwrap();
    client.close();
    drop(events);
    drop(client);
    drop(process);

    let process = fixture.start();
    let client = fixture.connect(&process).await;
    client
        .consumer(
            "jobs",
            "worker",
            ConsumerConfig {
                filter_subject: "plugin.jobs".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let batch = client.fetch("jobs", "worker", 2).await.unwrap();
    assert_eq!(
        batch.len(),
        1,
        "only the unacknowledged record is replayed after restart"
    );
    assert_eq!(batch[0].record.sequence, second.sequence);
    assert_eq!(batch[0].record.message.payload, "second");
    client.ack("jobs", "worker", second.sequence).await.unwrap();
    client.close();
}

/// Serves one artifact over loopback HTTP. `delay` drips the body slowly.
fn artifact_server(body: Vec<u8>, delay: Option<Duration>) -> (u16, std::sync::Arc<AtomicUsize>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            counter.fetch_add(1, Ordering::SeqCst);
            let body = body.clone();
            thread::spawn(move || {
                let mut request = [0; 4096];
                let _ = stream.read(&mut request);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if stream.write_all(head.as_bytes()).is_err() {
                    return;
                }
                for chunk in body.chunks(if delay.is_some() {
                    1024
                } else {
                    body.len().max(1)
                }) {
                    if stream.write_all(chunk).is_err() {
                        return;
                    }
                    if let Some(delay) = delay {
                        thread::sleep(delay);
                    }
                }
            });
        }
    });
    (port, hits)
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A managed server stand-in: listens on its provisioned port until `stop`.
#[test]
fn messaging_instance_child() {
    let Ok(properties) = fs::read_to_string("server.properties") else {
        return;
    };
    assert_eq!(fs::read("server.jar").unwrap(), b"downloaded server jar");
    let port = properties
        .lines()
        .find_map(|line| line.strip_prefix("server-port="))
        .unwrap();
    let _listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).unwrap();
    let mut command = String::new();
    std::io::stdin().read_line(&mut command).unwrap();
}

fn write_instance_config(fixture: &Fixture, url: &str, sha256: &str, port: u16) {
    let helper =
        serde_json::to_string(&std::env::current_exe().unwrap().to_string_lossy()).unwrap();
    fs::write(
        fixture.path.join("rift.lua"),
        format!(
            r#"
rift.config.listeners.main = '127.0.0.1:0'
rift.config.backends = {{}}
rift.config.routes.main = 'game'
rift.config.messaging = {{
    listen = '127.0.0.1:0', certificate = 'cert.pem', private_key = 'key.pem',
    principals = {{ {{ name = 'plugin', token_env = 'RIFT_MESSAGING_TEST_TOKEN',
        publish = {{ 'rift.control.*', '_INBOX.>' }},
        subscribe = {{ 'rift.events.>', '_INBOX.>' }}, control = true }} }},
}}
rift.config.templates.game = {{
    software = {{ type = 'url', url = '{url}', sha256 = '{sha256}' }},
    accept_eula = true,
}}
rift.config.service_groups.game = {{
    template = 'game', storage = 'disposable', port_range = {{ {port}, {port} }},
    command = {{ {helper}, '--exact', 'messaging_instance_child', '--nocapture' }},
    start_timeout_ms = 10000, stop_timeout_ms = 10000,
}}
"#
        ),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn channel_requests_download_create_start_stop_and_remove_instances() {
    let fixture = Fixture::new();
    let jar = b"downloaded server jar".to_vec();
    let (http, hits) = artifact_server(jar.clone(), None);
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    write_instance_config(
        &fixture,
        &format!("http://127.0.0.1:{http}/server.jar"),
        &sha256(&jar),
        port,
    );
    let process = fixture.start();
    let client = fixture.connect(&process).await;
    let mut events = client
        .subscribe("rift.events.instances", None)
        .await
        .unwrap();
    let next_event = async |events: &mut quic::RemoteSubscription| {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        serde_json::from_slice::<Value>(&event.payload).unwrap()
    };

    let templates = control(&client, "rift.control.templates", json!({})).await;
    assert_eq!(
        templates["data"]["templates"][0]["software"]["cached"],
        false
    );
    let downloaded = control(&client, "rift.control.download", json!({"template":"game"})).await;
    assert_eq!(downloaded["ok"], true, "{downloaded}");
    assert_eq!(downloaded["data"]["software"]["sha256"], sha256(&jar));
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let created = control(&client, "rift.control.create", json!({"group":"game"})).await;
    assert_eq!(created["ok"], true, "{created}");
    assert_eq!(created["data"]["name"], "game-1");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "creation reuses the cached jar"
    );
    let event = next_event(&mut events).await;
    assert_eq!(event["event"], "created");
    assert_eq!(event["name"], "game-1");
    let directory = fixture.path.join("temp/services/game-1");
    assert!(
        fs::read_to_string(directory.join("eula.txt"))
            .unwrap()
            .contains("eula=true")
    );
    assert_eq!(
        control(&client, "rift.control.groups", json!({})).await["data"]["groups"][0]["instances"],
        json!(["game-1"])
    );

    let state = async |client: &quic::Client| {
        control(client, "rift.control.servers", json!({})).await["data"]["servers"][0]["state"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let wait_for = async |client: &quic::Client, expected: &str| {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if state(client).await == expected {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "server never became {expected}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let started = control(&client, "rift.control.start", json!({"name":"game-1"})).await;
    assert_eq!(started["ok"], true, "{started}");
    wait_for(&client, "running").await;
    let stopped = control(&client, "rift.control.stop", json!({"name":"game-1"})).await;
    assert_eq!(stopped["ok"], true, "{stopped}");
    wait_for(&client, "stopped").await;

    let removed = control(&client, "rift.control.remove", json!({"name":"game-1"})).await;
    assert_eq!(removed["ok"], true, "{removed}");
    assert_eq!(removed["data"]["files_removed"], true);
    let event = next_event(&mut events).await;
    assert_eq!(
        (event["event"].as_str(), event["name"].as_str()),
        (Some("removed"), Some("game-1"))
    );
    assert!(!directory.exists());

    let unknown = control(&client, "rift.control.create", json!({"group":"missing"})).await;
    assert_eq!(unknown["ok"], false);
    let refreshed = control(
        &client,
        "rift.control.download",
        json!({"template":"game","refresh":true}),
    )
    .await;
    assert_eq!(refreshed["ok"], true, "{refreshed}");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "refresh downloads again");
    client.close();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_cancels_a_download_without_publishing_a_jar() {
    let fixture = Fixture::new();
    // Roughly 40 seconds at this pace: shutdown must not wait for it.
    let jar = vec![7u8; 400 * 1024];
    let (http, hits) = artifact_server(jar.clone(), Some(Duration::from_millis(100)));
    write_instance_config(
        &fixture,
        &format!("http://127.0.0.1:{http}/slow.jar"),
        &sha256(&jar),
        1,
    );
    let mut process = fixture.start();
    let client = fixture.connect(&process).await;
    let request = tokio::spawn(async move {
        let reply = client
            .request(
                "rift.control.download",
                Bytes::from_static(br#"{"template":"game"}"#),
                Duration::from_secs(60),
            )
            .await;
        (client, reply)
    });
    let cache = fixture.path.join("local/versioncache");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let entry = loop {
        let partial = fs::read_dir(&cache).ok().and_then(|entries| {
            entries.flatten().find_map(|entry| {
                let entry = entry.path();
                fs::read_dir(&entry)
                    .ok()?
                    .flatten()
                    .any(|file| file.file_name().to_string_lossy().contains(".part-"))
                    .then_some(entry)
            })
        });
        if let Some(entry) = partial {
            break entry;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "download never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let interrupted = std::time::Instant::now();
    assert!(
        Command::new("kill")
            .args(["-INT", &process.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let status = loop {
        if let Some(status) = process.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            interrupted.elapsed() < Duration::from_secs(10),
            "shutdown waited for the download"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(status.success(), "{status}");
    assert!(!entry.join("server.jar").exists());
    let leftovers: Vec<_> = fs::read_dir(&entry)
        .unwrap()
        .flatten()
        .map(|file| file.file_name())
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let (_client, reply) = request.await.unwrap();
    if let Ok(reply) = reply {
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.payload).unwrap()["ok"],
            false
        );
    }
}
