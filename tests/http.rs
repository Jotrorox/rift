//! Black-box coverage for the embedded administration and observability servers.
//! The HTTP client intentionally uses only std so the test suite adds no dependencies.
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const TOKEN: &str = "test-administration-token";
const ADMIN_TOKEN: &str = "test-local-administration-token-32-bytes";

struct Fixture {
    directory: PathBuf,
    web: SocketAddr,
    status: SocketAddr,
    metrics: SocketAddr,
    _serial: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        // These cases rebind fixed ports during reload. Serialize their fixtures
        // so another case cannot reserve a released port before its child binds.
        static SERIAL: Mutex<()> = Mutex::new(());
        let serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "rift-http-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        // Reserve all addresses together so none can be selected twice.
        let reservations: Vec<_> = (0..3)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        Self {
            directory,
            web: reservations[0].local_addr().unwrap(),
            status: reservations[1].local_addr().unwrap(),
            metrics: reservations[2].local_addr().unwrap(),
            _serial: serial,
        }
    }

    fn source(&self, web_options: &str, extra: &str) -> String {
        format!(
            "-- Preserve comments and custom Lua exactly.\nreturn {{\n\
             listeners = {{ main = '127.0.0.1:0' }},\n\
             backends = {{ primary = '127.0.0.1:1' }},\n\
             routes = {{ main = 'primary' }},\n\
             web = {{ listen = '{}', {web_options} }},\n\
             {extra}\n}}\n",
            self.web
        )
    }

    fn write(&self, source: &str) {
        fs::write(self.directory.join("rift.lua"), source).unwrap();
    }

    fn read(&self) -> String {
        fs::read_to_string(self.directory.join("rift.lua")).unwrap()
    }

    fn start(&self, source: &str) -> Process {
        self.write(source);
        let mut child = Command::new(env!("CARGO_BIN_EXE_rift"))
            .current_dir(&self.directory)
            .args(["--config", "rift.lua"])
            .env("RIFT_ADMIN_TOKEN", ADMIN_TOKEN)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let logs = Arc::new(Mutex::new(Vec::new()));
        let output = logs.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                output.lock().unwrap().push(line);
            }
        });
        let mut process = Process { child, logs };
        process.wait_for(self.web);
        process
    }

    fn client(&self) -> Client {
        Client::new(self.web)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).ok();
    }
}

struct Process {
    child: Child,
    logs: Arc<Mutex<Vec<String>>>,
}

impl Process {
    fn wait_for(&mut self, address: SocketAddr) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "HTTP service at {address} never started: {:?}",
                self.logs.lock().unwrap()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

#[derive(Clone, Copy)]
struct Client {
    address: SocketAddr,
    token: Option<&'static str>,
}

impl Client {
    fn new(address: SocketAddr) -> Self {
        Self {
            address,
            token: None,
        }
    }

    fn authenticated(self) -> Self {
        Self {
            token: Some(TOKEN),
            ..self
        }
    }

    fn get(&self, path: &str) -> Response {
        self.send("GET", path, &[], "")
    }

    fn json(&self, method: &str, path: &str, value: Value) -> Response {
        self.send(
            method,
            path,
            &[("Content-Type", "application/json")],
            &value.to_string(),
        )
    }

    fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Response {
        let mut stream = connect(self.address);
        let default_host = self.address.to_string();
        let host = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("host"))
            .map_or(default_host.as_str(), |(_, value)| *value);
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some(token) = self.token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case("host") {
                request.push_str(&format!("{name}: {value}\r\n"));
            }
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        let read = stream.read_to_string(&mut response);
        let (headers, body) = response.split_once("\r\n\r\n").unwrap_or_else(|| {
            panic!("{method} {path}: incomplete HTTP response ({read:?}): {response:?}")
        });
        let headers = headers.to_ascii_lowercase();
        if let Err(error) = &read {
            // A rejected oversized request can close with unread request bytes.
            // Accept that reset only after checking the entire response framing.
            assert_eq!(
                error.kind(),
                io::ErrorKind::ConnectionReset,
                "{method} {path}: {error}"
            );
        }
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = if headers.contains("transfer-encoding: chunked") {
            decode_chunks(body)
        } else {
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "));
            if let Some(length) = length {
                assert_eq!(
                    body.len(),
                    length.parse::<usize>().unwrap(),
                    "{method} {path}: truncated response"
                );
            } else {
                assert!(
                    read.is_ok(),
                    "{method} {path}: reset without a framed response"
                );
            }
            body.to_owned()
        };
        Response {
            status,
            headers,
            body,
        }
    }
}

struct Response {
    status: u16,
    headers: String,
    body: String,
}

impl Response {
    fn expect(self, status: u16) -> Self {
        assert_eq!(self.status, status, "{}\n{}", self.headers, self.body);
        self
    }

    fn value(self) -> Value {
        serde_json::from_str(&self.body).expect(&self.body)
    }
}

fn decode_chunks(mut body: &str) -> String {
    let mut result = String::new();
    loop {
        let (length, rest) = body.split_once("\r\n").unwrap();
        let length = usize::from_str_radix(length.split(';').next().unwrap(), 16).unwrap();
        if length == 0 {
            return result;
        }
        result.push_str(&rest[..length]);
        body = &rest[length + 2..];
    }
}

fn connect(address: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
}

fn save(client: Client, source: &str, revision: &Value) -> Response {
    client.json(
        "PUT",
        "/api/config",
        json!({ "source": source, "revision": revision }),
    )
}

fn revision(client: Client) -> Value {
    client.get("/api/config").expect(200).value()["revision"].clone()
}

fn eventually_closed(address: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_err() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "service at {address} remained enabled"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn bearer_auth_origin_and_json_requirements_protect_administration() {
    let fixture = Fixture::new();
    let source = fixture.source(&format!("token = '{TOKEN}',"), "");
    let _process = fixture.start(&source);
    let public = fixture.client();
    let admin = public.authenticated();
    let page = public.get("/").expect(200);
    assert!(page.headers.contains("text/html"));
    assert!(
        !page.body.contains(TOKEN),
        "token leaked into the public shell"
    );
    for path in ["/api/status", "/api/servers", "/api/config", "/ext/private"] {
        public.get(path).expect(401);
        Client {
            token: Some("incorrect-token"),
            ..public
        }
        .get(path)
        .expect(401);
    }
    admin.get("/api/status").expect(200);
    admin
        .send(
            "GET",
            "/api/config",
            &[("Origin", "https://attacker.example")],
            "",
        )
        .expect(403);
    admin
        .send(
            "GET",
            "/api/config",
            &[("Sec-Fetch-Site", "cross-site")],
            "",
        )
        .expect(403);
    admin.send("POST", "/api/reload", &[], "{}").expect(415);
    admin
        .send(
            "POST",
            "/api/reload",
            &[("Content-Type", "text/plain")],
            "{}",
        )
        .expect(415);
    admin
        .send(
            "PUT",
            "/api/config",
            &[("Content-Type", "application/json")],
            "{broken",
        )
        .expect(400);
    let origin = format!("http://{}", fixture.web);
    admin
        .send("GET", "/api/config", &[("Origin", &origin)], "")
        .expect(200);
    assert_eq!(fixture.read(), source);
}

#[test]
fn server_operations_enforce_auth_origin_json_and_backend_validation() {
    let fixture = Fixture::new();
    let source = fixture.source(&format!("token = '{TOKEN}',"), "");
    let _process = fixture.start(&source);
    let public = fixture.client();
    let admin = public.authenticated();
    assert_eq!(
        admin.get("/api/servers").expect(200).value()["servers"],
        json!([])
    );
    let discovery = admin.get("/api").expect(200).value();
    assert!(
        discovery["endpoints"]
            .as_array()
            .unwrap()
            .contains(&json!("POST /api/servers/{name}/start"))
    );
    for operation in ["start", "stop"] {
        let path = format!("/api/servers/primary/{operation}");
        public.json("POST", &path, json!({})).expect(401);
        admin
            .send(
                "POST",
                &path,
                &[
                    ("Origin", "https://attacker.example"),
                    ("Content-Type", "application/json"),
                ],
                "{}",
            )
            .expect(403);
        admin
            .send(
                "POST",
                &path,
                &[
                    ("Sec-Fetch-Site", "cross-site"),
                    ("Content-Type", "application/json"),
                ],
                "{}",
            )
            .expect(403);
        admin.send("POST", &path, &[], "{}").expect(415);
        admin
            .json("POST", &path, json!({"command":["unexpected"]}))
            .expect(400);
        let error = admin.json("POST", &path, json!({})).expect(400).value();
        assert!(error["error"].as_str().unwrap().contains("not managed"));
        admin
            .json(
                "POST",
                &format!("/api/servers/missing/{operation}"),
                json!({}),
            )
            .expect(404);
        admin.get(&path).expect(405);
    }
    assert_eq!(fixture.read(), source);
}

#[test]
fn managed_state_and_accepted_operations_are_private_and_restart_bound() {
    let fixture = Fixture::new();
    let extra = format!(
        "status = {{ listen = '{}' }}, managed_servers = {{ primary = {{ command = {{'rift-test-executable-that-does-not-exist', 'private-process-argument'}}, directory = '.', start_timeout_ms = 1000 }} }},",
        fixture.status
    );
    let source = fixture.source(&format!("token = '{TOKEN}',"), &extra);
    let mut process = fixture.start(&source);
    process.wait_for(fixture.status);
    let admin = fixture.client().authenticated();
    let initial = admin.get("/api/status").expect(200).value();
    assert_eq!(initial["managed_servers"][0]["name"], "primary");
    assert_eq!(initial["managed_servers"][0]["state"], "stopped");
    assert!(!initial.to_string().contains("private-process-argument"));
    let public = Client::new(fixture.status)
        .get("/status")
        .expect(200)
        .value();
    assert!(public.get("managed_servers").is_none());
    assert!(!public.to_string().contains("private-process-argument"));
    let stopped = admin
        .json("POST", "/api/servers/primary/stop", json!({}))
        .expect(202)
        .value();
    assert_eq!(stopped["accepted"], true);
    assert_eq!(
        admin.get("/api/servers").expect(200).value()["servers"][0]["automatic_start"],
        false
    );
    admin
        .json("POST", "/api/servers/primary/start", json!({}))
        .expect(202);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = admin.get("/api/servers").expect(200).value();
        assert!(!state.to_string().contains("private-process-argument"));
        assert!(
            !state
                .to_string()
                .contains("rift-test-executable-that-does-not-exist")
        );
        if state["servers"][0]["state"] == "failed" {
            assert!(state["servers"][0]["last_error"].is_string());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "failed start was not reported: {state}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let candidate = source.replace("start_timeout_ms = 1000", "start_timeout_ms = 2000");
    let result = admin
        .json("POST", "/api/config/validate", json!({"source":candidate}))
        .expect(400)
        .value();
    assert!(result["error"].as_str().unwrap().contains("restart"));
    save(admin, &candidate, &revision(admin)).expect(400);
    assert_eq!(fixture.read(), source);
}

#[test]
fn bearer_token_rotation_takes_effect_without_restarting_the_listener() {
    let fixture = Fixture::new();
    let source = fixture.source(&format!("token = '{TOKEN}',"), "");
    let _process = fixture.start(&source);
    let original = fixture.client().authenticated();
    let updated = source.replace(TOKEN, "rotated-administration-token");
    save(original, &updated, &revision(original)).expect(200);
    original.get("/api/config").expect(401);
    let rotated = Client {
        token: Some("rotated-administration-token"),
        ..original
    };
    assert_eq!(
        rotated.get("/api/config").expect(200).value()["source"],
        updated
    );
    fixture.client().get("/").expect(200);
}

#[test]
fn moving_to_tokenless_loopback_cannot_authorize_a_preaccepted_public_socket() {
    let fixture = Fixture::new();
    let public_bind = format!("0.0.0.0:{}", fixture.web.port());
    let source = fixture
        .source(&format!("token = '{TOKEN}',"), "")
        .replace(&fixture.web.to_string(), &public_bind);
    let mut process = fixture.start(&source);
    let admin = fixture.client().authenticated();
    let mut stale = connect(fixture.web);
    stale
        .write_all(b"GET /api/config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n")
        .unwrap();
    // This request gives the server an opportunity to accept the partial one.
    admin.get("/api/status").expect(200);
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let replacement = reserved.local_addr().unwrap();
    let updated = source
        .replace(&public_bind, &replacement.to_string())
        .replace(&format!("token = '{TOKEN}',"), "");
    drop(reserved);
    save(admin, &updated, &revision(admin)).expect(200);
    process.wait_for(replacement);
    Client::new(replacement).get("/api/config").expect(200);
    match stale.write_all(b"\r\n") {
        Ok(()) => (),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) =>
        {
            return;
        }
        Err(error) => panic!("unexpected socket error: {error}"),
    }
    let mut response = String::new();
    if let Err(error) = stale.read_to_string(&mut response) {
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    }
    assert!(
        response.is_empty() || response.starts_with("HTTP/1.1 404 "),
        "retired public listener must not inherit tokenless credentials: {response}"
    );
}

#[test]
fn configuration_roundtrips_exactly_and_rejects_invalid_or_stale_updates() {
    let fixture = Fixture::new();
    let source = fixture.source("", "");
    let _process = fixture.start(&source);
    let client = fixture.client();
    let original = client.get("/api/config").expect(200).value();
    client
        .send("GET", "/api/config", &[("Host", "attacker.example")], "")
        .expect(403);
    assert_eq!(original["source"], source);
    assert_eq!(original["writable"], true);
    assert!(!original["revision"].is_null());
    let updated = source.replace("127.0.0.1:1", "127.0.0.1:2") + "-- Edited in the browser.\n";
    client
        .json("POST", "/api/config/validate", json!({"source": updated}))
        .expect(200);
    assert_eq!(fixture.read(), source, "validation must not persist");
    assert_eq!(revision(client), original["revision"]);
    let oversized_source = "-".repeat(256 * 1024 + 1);
    client
        .json(
            "POST",
            "/api/config/validate",
            json!({"source": oversized_source}),
        )
        .expect(413);
    save(client, &oversized_source, &original["revision"]).expect(413);
    assert_eq!(fixture.read(), source);
    for invalid in ["return {", "error('invalid config')", "while true do end"] {
        client
            .json("POST", "/api/config/validate", json!({"source": invalid}))
            .expect(400);
        save(client, invalid, &original["revision"]).expect(400);
        assert_eq!(fixture.read(), source);
        assert_eq!(revision(client), original["revision"]);
    }
    save(client, &updated, &original["revision"]).expect(200);
    assert_eq!(fixture.read(), updated);
    let current = client.get("/api/config").expect(200).value();
    assert_eq!(current["source"], updated);
    assert_ne!(current["revision"], original["revision"]);
    save(client, &source, &original["revision"]).expect(409);
    assert_eq!(fixture.read(), updated);
    assert_eq!(
        client.get("/api/status").expect(200).value()["backends"][0]["address"],
        "127.0.0.1:2"
    );
}

#[test]
fn modular_scripts_validate_save_and_reload_without_rereading_live_callback_files() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.directory.join("lua")).unwrap();
    fs::create_dir_all(fixture.directory.join("plugins/web")).unwrap();
    fs::write(fixture.directory.join("lua/settings.lua"), format!(
        "rift.setup {{ listeners = {{ main = '127.0.0.1:0' }}, backends = {{ primary = '127.0.0.1:1' }}, routes = {{ main = 'primary' }}, web = {{ listen = '{}' }} }}", fixture.web
    )).unwrap();
    let plugin = fixture.directory.join("plugins/web/init.lua");
    fs::write(
        &plugin,
        "rift.on('http', function() return { body = 'original' } end)",
    )
    .unwrap();
    let source = "require('settings')\nrift.plugin('web')\n";
    let _process = fixture.start(source);
    let client = fixture.client();
    assert_eq!(client.get("/ext/plugin").expect(200).body, "original");
    let edited = format!("{source}-- Saved through the editor.\n");
    client
        .json("POST", "/api/config/validate", json!({"source": edited}))
        .expect(200);
    save(client, &edited, &revision(client)).expect(200);
    assert_eq!(fixture.read(), edited);
    fs::write(
        &plugin,
        "rift.on('http', function() return { body = 'updated' } end)",
    )
    .unwrap();
    assert_eq!(client.get("/ext/plugin").expect(200).body, "original");
    client.json("POST", "/api/reload", json!({})).expect(200);
    assert_eq!(client.get("/ext/plugin").expect(200).body, "updated");
    fs::write(&plugin, "error('invalid plugin update')").unwrap();
    client
        .json("POST", "/api/config/validate", json!({"source": edited}))
        .expect(400);
    client.json("POST", "/api/reload", json!({})).expect(400);
    assert_eq!(client.get("/ext/plugin").expect(200).body, "updated");
}

#[test]
fn external_edits_conflict_until_reloaded_and_invalid_reload_preserves_runtime() {
    let fixture = Fixture::new();
    let source = fixture.source("", "");
    let _process = fixture.start(&source);
    let client = fixture.client();
    let before = revision(client);
    let edited = source.replace("127.0.0.1:1", "127.0.0.1:3");
    fixture.write(&edited);
    save(client, &(source.clone() + "-- browser edit\n"), &before).expect(409);
    assert_eq!(fixture.read(), edited);
    client.json("POST", "/api/reload", json!({})).expect(200);
    let reloaded = client.get("/api/config").expect(200).value();
    assert_eq!(reloaded["source"], edited);
    assert_ne!(reloaded["revision"], before);
    assert_eq!(
        client.get("/api/status").expect(200).value()["backends"][0]["address"],
        "127.0.0.1:3"
    );
    fixture.write("return { broken syntax");
    client.json("POST", "/api/reload", json!({})).expect(400);
    assert_eq!(
        client.get("/api/status").expect(200).value()["backends"][0]["address"],
        "127.0.0.1:3"
    );
    assert_eq!(revision(client), reloaded["revision"]);
}

#[test]
fn status_and_metrics_servers_expose_only_their_enabled_surfaces() {
    let fixture = Fixture::new();
    let extra = format!(
        "status = {{ listen = '{}', metrics = false }}, metrics = '{}',",
        fixture.status, fixture.metrics
    );
    let source = fixture.source(&format!("token = '{TOKEN}', ui = false,"), &extra);
    let mut process = fixture.start(&source);
    process.wait_for(fixture.status);
    process.wait_for(fixture.metrics);
    fixture.client().get("/").expect(404);
    fixture
        .client()
        .authenticated()
        .get("/api/status")
        .expect(200);
    let status = Client::new(fixture.status);
    assert!(status.get("/").expect(200).headers.contains("text/html"));
    let state = status.get("/status").expect(200).value();
    assert_eq!(state["listeners"][0]["name"], "main");
    assert_eq!(state["backends"][0]["name"], "primary");
    assert!(state["metrics"]["accepted"].is_number());
    assert!(!state.to_string().contains(TOKEN));
    assert!(state.get("managed_servers").is_none());
    for path in [
        "/metrics",
        "/api/config",
        "/api/status",
        "/api/servers",
        "/ext/example",
    ] {
        status.get(path).expect(404);
    }
    let metrics = Client::new(fixture.metrics);
    let scrape = metrics.get("/metrics").expect(200);
    assert!(scrape.body.contains("rift_connections_accepted_total"));
    assert!(scrape.headers.contains("text/plain"));
    for path in ["/", "/status", "/api/config"] {
        metrics.get(path).expect(404);
    }
}

#[test]
fn observability_services_can_be_enabled_reconfigured_and_disabled_live() {
    let fixture = Fixture::new();
    let source = fixture.source("", "status = false, metrics = false,");
    let mut process = fixture.start(&source);
    let client = fixture.client();
    let enabled = fixture.source(
        "",
        &format!(
            "status = {{ listen = '{}', ui = false, metrics = true }}, metrics = '{}',",
            fixture.status, fixture.metrics
        ),
    );
    save(client, &enabled, &revision(client)).expect(200);
    process.wait_for(fixture.status);
    process.wait_for(fixture.metrics);
    Client::new(fixture.status).get("/").expect(404);
    Client::new(fixture.status).get("/status").expect(200);
    Client::new(fixture.status).get("/metrics").expect(200);
    let replacement = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = replacement.local_addr().unwrap();
    drop(replacement);
    let rebound = enabled.replace(&fixture.status.to_string(), &address.to_string());
    save(client, &rebound, &revision(client)).expect(200);
    process.wait_for(address);
    eventually_closed(fixture.status);
    Client::new(address).get("/status").expect(200);
    save(client, &source, &revision(client)).expect(200);
    eventually_closed(address);
    eventually_closed(fixture.metrics);
    client.get("/api/status").expect(200);
}

#[test]
fn occupied_service_bind_and_listener_topology_changes_roll_back_completely() {
    let fixture = Fixture::new();
    let source = fixture.source(
        "",
        &format!("status = {{ listen = '{}' }},", fixture.status),
    );
    let mut process = fixture.start(&source);
    process.wait_for(fixture.status);
    let client = fixture.client();
    let before = revision(client);
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied_source = source
        .replace(
            &fixture.status.to_string(),
            &occupied.local_addr().unwrap().to_string(),
        )
        .replace("127.0.0.1:1", "127.0.0.1:9");
    let topology_change = source
        .replace(
            "listeners = { main = '127.0.0.1:0' }",
            "listeners = { main = '127.0.0.1:0', other = '127.0.0.1:0' }",
        )
        .replace(
            "routes = { main = 'primary' }",
            "routes = { main = 'primary', other = 'primary' }",
        );
    for (candidate, validation_status) in [(occupied_source, 200), (topology_change, 400)] {
        // Validation checks topology but never binds new sockets.
        client
            .json("POST", "/api/config/validate", json!({"source": candidate}))
            .expect(validation_status);
        save(client, &candidate, &before).expect(400);
        assert_eq!(fixture.read(), source);
        assert_eq!(revision(client), before);
        let state = client.get("/api/status").expect(200).value();
        assert_eq!(state["backends"][0]["address"], "127.0.0.1:1");
        assert_eq!(state["listeners"].as_array().unwrap().len(), 1);
        Client::new(fixture.status).get("/status").expect(200);
    }
}

#[test]
fn lua_extensions_receive_requests_and_bound_failures_without_harming_admin() {
    let fixture = Fixture::new();
    let hook = r#"
        on_http = function(req)
            if req.path == '/ext/echo' then
                assert(req.headers.authorization == nil)
                assert(req.headers.cookie == nil)
                assert(type(req.context.version) == 'string')
                return { status = 201, content_type = 'text/plain',
                    body = req.method .. '|' .. req.query .. '|' .. req.body }
            elseif req.path == '/ext/error' then
                error('extension failed')
            elseif req.path == '/ext/loop' then
                while true do end
            elseif req.path == '/ext/huge' then
                return { body = string.rep('x', 262145) }
            end
        end,
    "#;
    let source = fixture.source(&format!("token = '{TOKEN}',"), hook);
    let _process = fixture.start(&source);
    let client = fixture.client().authenticated();
    let echoed = client
        .send(
            "POST",
            "/ext/echo?a=1",
            &[("Cookie", "private=value")],
            "hello Lua",
        )
        .expect(201);
    assert_eq!(echoed.body, "POST|a=1|hello Lua");
    assert!(echoed.headers.contains("text/plain"));
    client.get("/ext/missing").expect(404);
    client.get("/ext/error").expect(500);
    let started = Instant::now();
    let looping = client.get("/ext/loop");
    assert!([500, 504].contains(&looping.status), "{}", looping.body);
    assert!(started.elapsed() < Duration::from_secs(2));
    client.get("/ext/huge").expect(500);
    let oversized = "x".repeat(262145);
    client
        .send("POST", "/ext/echo", &[], &oversized)
        .expect(413);
    client.get("/api/status").expect(200);
}

#[test]
fn disabled_api_leaves_ui_and_lua_extensions_available() {
    let fixture = Fixture::new();
    let source = fixture.source(
        "api = false,",
        "on_http = function(req) return { body = 'custom page' } end,",
    );
    let _process = fixture.start(&source);
    fixture.client().get("/").expect(200);
    fixture.client().get("/api/config").expect(404);
    fixture.client().get("/api/status").expect(404);
    assert_eq!(
        fixture.client().get("/ext/custom").expect(200).body,
        "custom page"
    );
}

#[cfg(unix)]
#[test]
fn main_service_can_disable_itself_and_be_enabled_again_from_lua() {
    let fixture = Fixture::new();
    let source = fixture.source("", "");
    let mut process = fixture.start(&source);
    let client = fixture.client();
    let without_ui = fixture.source("ui = false,", "");
    save(client, &without_ui, &revision(client)).expect(200);
    client.get("/").expect(404);
    client.get("/api/status").expect(200);
    let disabled = without_ui.replace(
        &format!("web = {{ listen = '{}', ui = false, }},", fixture.web),
        "web = false,",
    );
    assert!(disabled.contains("web = false"));
    save(client, &disabled, &revision(client)).expect(200);
    eventually_closed(fixture.web);
    assert_eq!(fixture.read(), disabled);
    fixture.write(&source);
    assert!(
        Command::new("kill")
            .args(["-HUP", &process.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    process.wait_for(fixture.web);
    client.get("/").expect(200);
    assert_eq!(
        client.get("/api/config").expect(200).value()["source"],
        source
    );
}

fn exchange(front: SocketAddr, backend: &TcpListener, message: &[u8]) {
    use rift::protocol::{Codec, Handshake, NextState, Packet, write_string};

    fn packet(stream: &mut TcpStream) -> Packet {
        let mut length = 0usize;
        for shift in (0..21).step_by(7) {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            length |= usize::from(byte[0] & 127) << shift;
            if byte[0] & 128 == 0 {
                assert!(length <= rift::protocol::MAX_FRAME_SIZE);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                return Packet::from_body(&body).unwrap();
            }
        }
        panic!("oversized fixture packet");
    }

    let codec = Codec::default();
    let mut client = connect(front);
    let handshake = Handshake {
        protocol: 47,
        address: "localhost".into(),
        port: front.port(),
        next_state: NextState::Login,
    };
    client
        .write_all(&codec.encode(&handshake.packet()).unwrap())
        .unwrap();
    let mut login = Vec::new();
    write_string("Player", &mut login);
    client
        .write_all(&codec.encode(&Packet::new(0, login)).unwrap())
        .unwrap();
    backend.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut server = loop {
        match backend.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "proxy did not reach expected backend"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    server.set_nonblocking(false).unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    assert_eq!(
        Handshake::decode(&packet(&mut server)).unwrap().next_state,
        NextState::Login
    );
    assert_eq!(packet(&mut server).id, 0);
    let mut success = Vec::new();
    write_string("00000000-0000-0000-0000-000000000001", &mut success);
    write_string("Player", &mut success);
    server
        .write_all(&codec.encode(&Packet::new(2, success)).unwrap())
        .unwrap();
    assert_eq!(packet(&mut client).id, 2);
    // Carry test bytes in opaque play packets after the offline login completes.
    client
        .write_all(&codec.encode(&Packet::new(0x7f, message.to_vec())).unwrap())
        .unwrap();
    let received = packet(&mut server);
    assert_eq!(received.id, 0x7f);
    assert_eq!(received.data, message);
    server
        .write_all(
            &codec
                .encode(&Packet::new(0x7f, b"response".to_vec()))
                .unwrap(),
        )
        .unwrap();
    let reply = packet(&mut client);
    assert_eq!(reply.id, 0x7f);
    assert_eq!(reply.data, b"response");
}

#[test]
fn saving_network_configuration_changes_routing_and_live_metrics() {
    let fixture = Fixture::new();
    let first = TcpListener::bind("127.0.0.1:0").unwrap();
    let second = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = fixture
        .source("", "")
        .replace("127.0.0.1:1", &first.local_addr().unwrap().to_string());
    let _process = fixture.start(&source);
    let client = fixture.client();
    let state = client.get("/api/status").expect(200).value();
    let front: SocketAddr = state["listeners"][0]["address"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    exchange(front, &first, b"first backend");
    let updated = source.replace(
        &first.local_addr().unwrap().to_string(),
        &second.local_addr().unwrap().to_string(),
    );
    save(client, &updated, &revision(client)).expect(200);
    exchange(front, &second, b"second backend");
    let state = client.get("/api/status").expect(200).value();
    assert_eq!(state["metrics"]["accepted"], 2);
    assert_eq!(fixture.read(), updated);
}

#[test]
fn http_and_cli_controls_share_revisions_and_preserve_runtime_overrides() {
    let fixture = Fixture::new();
    let source = fixture.source(
        &format!("token = '{TOKEN}'"),
        "admin = { listen = '127.0.0.1:0', permissions = { 'status', 'maintenance', 'drain', 'reload' } }, metrics = false,",
    );
    let process = fixture.start(&source);
    let address: SocketAddr = process
        .logs
        .lock()
        .unwrap()
        .iter()
        .find_map(|line| line.strip_prefix("rift: admin on "))
        .unwrap()
        .parse()
        .unwrap();
    let admin = |args: &[&str]| -> Value {
        let mut stream = connect(address);
        writeln!(stream, "{}", json!({"token":ADMIN_TOKEN,"args":args})).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let client = fixture.client().authenticated();
    let original = client.get("/api/config").expect(200).value();
    assert_eq!(admin(&["maintenance", "on"])["ok"], true);
    assert_eq!(admin(&["drain", "primary", "on"])["ok"], true);

    // HTTP commits hot-bind metrics while retaining the local control socket,
    // its credentials and the operator's runtime maintenance/draining overrides.
    let updated = source.replace("metrics = false", "metrics = '127.0.0.1:0'");
    let saved = client
        .json(
            "PUT",
            "/api/config",
            json!({"source":updated,"revision":original["revision"]}),
        )
        .expect(200)
        .value();
    assert_eq!(fixture.read(), updated);
    let status = admin(&["status"]);
    assert_eq!(status["ok"], true);
    assert_eq!(status["data"]["maintenance"], true);
    assert_eq!(status["data"]["backends"][0]["draining"], true);
    let metrics_address: SocketAddr =
        client.get("/api/status").expect(200).value()["services"]["metrics"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
    let metrics = Client::new(metrics_address).get("/metrics").expect(200);
    assert!(metrics.body.contains("rift_maintenance_mode 1"));
    assert!(
        metrics
            .body
            .contains("rift_backend_draining{backend=\"primary\"} 1")
    );

    // CLI reload updates the same active source/revision used by HTTP saves.
    let next = format!("{updated}\n-- Edited on disk and reloaded through the CLI.\n");
    fixture.write(&next);
    let reloaded = admin(&["reload"]);
    assert_eq!(reloaded["ok"], true, "{reloaded}");
    let current = client.get("/api/config").expect(200).value();
    assert_eq!(current["source"], next);
    assert_eq!(current["revision"], reloaded["data"]["revision"]);
    assert_ne!(current["revision"], saved["revision"]);
    client
        .json(
            "PUT",
            "/api/config",
            json!({"source":updated,"revision":saved["revision"]}),
        )
        .expect(409);

    // Both HTTP preflight and application reject restart-only CLI settings.
    let invalid = next.replace("'status', 'maintenance', 'drain', 'reload'", "'status'");
    client
        .json("POST", "/api/config/validate", json!({"source":invalid}))
        .expect(400);
    client
        .json(
            "PUT",
            "/api/config",
            json!({"source":invalid,"revision":current["revision"]}),
        )
        .expect(400);
    assert_eq!(fixture.read(), next);
    assert_eq!(admin(&["status"])["data"]["maintenance"], true);

    // A rejected file reload leaves the HTTP document and live controls intact.
    fixture.write("return { broken = true }");
    assert_eq!(admin(&["reload"])["ok"], false);
    assert_eq!(
        client.get("/api/config").expect(200).value()["source"],
        next
    );
    assert_eq!(admin(&["maintenance", "off"])["ok"], true);
}
