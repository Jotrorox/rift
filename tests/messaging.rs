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
