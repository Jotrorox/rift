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
const OPERATOR_TOKEN: &str = "test-scoped-operator-token";
const LOCAL_TOKEN_LOG: &str = "rift: web token for this run: ";

struct Fixture {
    directory: PathBuf,
    web: SocketAddr,
    status: SocketAddr,
    metrics: SocketAddr,
    /// The token Rift generated and logged at startup when `web` has no
    /// credentials configured; `fixture.client()` sends it.
    local_token: Mutex<Option<&'static str>>,
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
            local_token: Mutex::new(None),
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
        // The generated token, if any, is logged before the listener line.
        process.wait_for_log("rift: web on ");
        *self.local_token.lock().unwrap() = process.logged_token();
        process
    }

    fn client(&self) -> Client {
        Client {
            token: *self.local_token.lock().unwrap(),
            ..Client::new(self.web)
        }
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
    fn wait_for_log(&mut self, prefix: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let value = self
                .logs
                .lock()
                .unwrap()
                .iter()
                .find_map(|line| line.strip_prefix(prefix).map(str::to_owned));
            if let Some(value) = value {
                return value;
            }
            assert!(
                self.child.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "missing startup log {prefix:?}: {:?}",
                self.logs.lock().unwrap()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn logged_token(&self) -> Option<&'static str> {
        self.logs.lock().unwrap().iter().find_map(|line| {
            let token = line.strip_prefix(LOCAL_TOKEN_LOG)?.split(' ').next()?;
            Some(&*Box::leak(token.to_owned().into_boxed_str()))
        })
    }

    /// Wait for the generated token, which a reload to a configuration
    /// without credentials may log after startup.
    fn local_token(&mut self) -> &'static str {
        self.wait_for_log(LOCAL_TOKEN_LOG);
        self.logged_token().unwrap()
    }

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

    // Resolve the asynchronous API to its operation result for existing
    // lifecycle assertions. Tests of the wire contract use json() directly.
    fn instance(&self, method: &str, path: &str, value: Value) -> Response {
        let response = self.json(method, path, value);
        if response.status != 202 {
            return response;
        }
        let accepted = response.value();
        let path = accepted["poll"].as_str().unwrap();
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            let response = self.get(path).expect(200);
            let operation = serde_json::from_str::<Value>(&response.body).unwrap();
            match operation["status"].as_str().unwrap() {
                "pending" => {
                    assert!(
                        Instant::now() < deadline,
                        "operation never completed: {operation}"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                "succeeded" | "failed" => {
                    return Response {
                        status: operation["http_status"].as_u64().unwrap() as u16,
                        raw: Vec::new(),
                        headers: response.headers,
                        body: if operation["status"] == "succeeded" {
                            operation["result"].to_string()
                        } else {
                            json!({"error":operation["error"]}).to_string()
                        },
                    };
                }
                other => panic!("unexpected operation status: {other}"),
            }
        }
    }

    fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Response {
        self.send_bytes(method, path, headers, body.as_bytes())
    }

    fn send_bytes(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Response {
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
        let mut request = request.into_bytes();
        request.extend_from_slice(body);
        stream.write_all(&request).unwrap();
        let mut response = Vec::new();
        let read = stream.read_to_end(&mut response);
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap_or_else(|| {
                panic!(
                    "{method} {path}: incomplete HTTP response ({read:?}): {:?}",
                    String::from_utf8_lossy(&response)
                )
            });
        let raw = response[split + 4..].to_vec();
        let headers = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
        let body = String::from_utf8_lossy(&raw).into_owned();
        let body = body.as_str();
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
                    raw.len(),
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
            raw,
        }
    }
}

struct Response {
    status: u16,
    headers: String,
    body: String,
    /// The body bytes as received (before chunked decoding).
    raw: Vec<u8>,
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
fn service_instances_register_concurrently_reload_and_remove_without_proxy_restart() {
    let fixture = Fixture::new();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();
    let (first, second) = (0..128)
        .find_map(|_| {
            let first = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = first.local_addr().unwrap().port();
            let second = TcpListener::bind(("127.0.0.1", port.checked_add(1)?)).ok()?;
            Some((first, second))
        })
        .expect("could not reserve two adjacent test ports");
    let port = first.local_addr().unwrap().port();
    let extra = format!(
        "service_groups = {{ lobby = {{ directory = 'servers/{{name}}', command = {{'private-group-command', 'private-group-argument', '{{port}}'}}, port_range = {{{port}, {}}} }}, blocked = {{ directory = 'blocked/{{name}}', command = {{'private-group-command'}}, port_range = {{{occupied_port}, {occupied_port}}} }} }},",
        port + 1
    );
    let source = fixture.source(&format!("token = '{TOKEN}',"), &extra);
    drop((first, second));
    let mut process = fixture.start(&source);
    let pid = process.child.id();
    let public = fixture.client();
    let admin = public.authenticated();
    public.get("/api/groups").expect(401);
    for (method, path) in [
        ("POST", "/api/groups/lobby/instances"),
        ("DELETE", "/api/instances/lobby-1"),
    ] {
        public.json(method, path, json!({})).expect(401);
        admin
            .send(
                method,
                path,
                &[
                    ("Origin", "https://attacker.example"),
                    ("Content-Type", "application/json"),
                ],
                "{}",
            )
            .expect(403);
        admin.send(method, path, &[], "{}").expect(415);
        admin.json(method, path, json!({"port":port})).expect(400);
    }
    let initial = admin.get("/api/groups").expect(200).value();
    assert_eq!(initial["groups"][1]["name"], "lobby");
    assert_eq!(initial["groups"][1]["instances"], json!([]));
    assert!(!initial.to_string().contains("private-group"));
    assert!(!initial.to_string().contains("servers/"));
    let create = move || {
        admin
            .instance("POST", "/api/groups/lobby/instances", json!({}))
            .expect(201)
            .value()
    };
    let first = thread::spawn(create);
    let second = thread::spawn(create);
    let first = first.join().unwrap();
    let second = second.join().unwrap();
    assert_ne!(first["name"], second["name"]);
    assert_ne!(first["port"], second["port"]);
    assert_eq!(first["group"], "lobby");
    assert_eq!(second["group"], "lobby");
    for instance in [&first, &second] {
        assert!(
            (u64::from(port)..=u64::from(port + 1)).contains(&instance["port"].as_u64().unwrap())
        );
    }
    admin
        .instance("POST", "/api/groups/lobby/instances", json!({}))
        .expect(409);
    admin
        .instance("POST", "/api/groups/blocked/instances", json!({}))
        .expect(409);
    admin
        .instance("POST", "/api/groups/missing/instances", json!({}))
        .expect(404);
    let servers = admin.get("/api/servers").expect(200).value();
    assert_eq!(servers["servers"].as_array().unwrap().len(), 2);
    assert_eq!(servers["servers"][0]["state"], "stopped");
    assert_eq!(servers["servers"][0]["group"], "lobby");
    assert!(
        servers["servers"][0]["address"]
            .as_str()
            .unwrap()
            .starts_with("127.0.0.1:")
    );
    assert!(!servers.to_string().contains("private-group"));
    admin
        .json("POST", "/api/config/validate", json!({"source":source}))
        .expect(200);
    admin.json("POST", "/api/reload", json!({})).expect(200);
    assert_eq!(
        admin.get("/api/servers").expect(200).value()["servers"],
        servers["servers"]
    );
    assert_eq!(
        admin.get("/api/groups").expect(200).value()["groups"][1]["instances"],
        json!(["lobby-1", "lobby-2"])
    );
    let changed = source.replace("private-group-argument", "changed-group-argument");
    admin
        .json("POST", "/api/config/validate", json!({"source":changed}))
        .expect(400);
    save(admin, &changed, &revision(admin)).expect(400);
    assert_eq!(fixture.read(), source);
    let explicit = source
        .replace(
            "routes = { main = 'primary' }",
            "routes = { main = 'lobby-1' }",
        )
        .replace(
            "service_groups =",
            "network = { access = { ['lobby-1'] = { deny = {'Blocked'} } } }, service_groups =",
        );
    assert_ne!(explicit, source);
    admin
        .json("POST", "/api/config/validate", json!({"source":explicit}))
        .expect(200);
    save(admin, &explicit, &revision(admin)).expect(200);
    admin.json("POST", "/api/reload", json!({})).expect(200);
    assert_eq!(
        admin.get("/api/status").expect(200).value()["routes"]["main"],
        "lobby-1"
    );
    assert_eq!(
        admin.get("/api/servers").expect(200).value()["servers"],
        servers["servers"]
    );
    admin
        .instance("DELETE", "/api/instances/lobby-1", json!({}))
        .expect(409);
    save(admin, &source, &revision(admin)).expect(200);
    let name = first["name"].as_str().unwrap();
    let removed = admin
        .instance("DELETE", &format!("/api/instances/{name}"), json!({}))
        .expect(200)
        .value();
    assert_eq!(removed["name"], name);
    assert_eq!(removed["removed"], true);
    assert_eq!(
        admin.get("/api/servers").expect(200).value()["servers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(process.child.id(), pid);
    assert!(process.child.try_wait().unwrap().is_none());
}

/// A real child process that takes longer than either HTTP deadline to stop.
#[test]
fn http_slow_removal_child() {
    let Ok(properties) = fs::read_to_string("server.properties") else {
        return;
    };
    let port = properties
        .lines()
        .find_map(|line| line.strip_prefix("server-port="))
        .unwrap();
    let _listener = TcpListener::bind(format!("127.0.0.1:{port}")).unwrap();
    let mut command = String::new();
    std::io::stdin().lock().read_line(&mut command).unwrap();
    assert_eq!(command, "stop\n");
    thread::sleep(Duration::from_secs(16));
}

#[test]
fn instance_removal_outlasts_http_and_dashboard_deadlines() {
    let fixture = Fixture::new();
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reservation.local_addr().unwrap().port();
    let helper =
        serde_json::to_string(&std::env::current_exe().unwrap().to_string_lossy()).unwrap();
    fs::write(fixture.directory.join("helper.jar"), "test template").unwrap();
    let source = fixture.source(&format!("token = '{TOKEN}',"), &format!(
        "templates = {{ test = {{ server_jar = 'helper.jar' }} }},
         service_groups = {{ lobby = {{ template = 'test', storage = 'disposable',
         directory = 'servers/{{name}}', command = {{{helper}, '--exact', 'http_slow_removal_child', '--nocapture'}},
         port_range = {{{port},{port}}}, start_timeout_ms = 5000, stop_timeout_ms = 30000 }} }},"
    ));
    drop(reservation);
    let mut process = fixture.start(&source);
    let admin = fixture.client().authenticated();
    admin
        .instance("POST", "/api/groups/lobby/instances", json!({}))
        .expect(201);
    admin
        .json("POST", "/api/servers/lobby-1/start", json!({}))
        .expect(202);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let servers = admin.get("/api/servers").expect(200).value();
        if servers["servers"][0]["state"] == "running" {
            break;
        }
        assert!(Instant::now() < deadline, "child did not start: {servers}");
        thread::sleep(Duration::from_millis(20));
    }
    let started = Instant::now();
    let response = admin
        .json("DELETE", "/api/instances/lobby-1", json!({}))
        .expect(202);
    assert!(started.elapsed() < Duration::from_secs(3));
    let accepted = serde_json::from_str::<Value>(&response.body).unwrap();
    let path = accepted["poll"].as_str().unwrap();
    assert!(response.headers.contains(&format!("location: {path}")));
    assert_eq!(
        path,
        format!(
            "/api/operations/{}",
            accepted["operation_id"].as_str().unwrap()
        )
    );
    fixture.client().get(path).expect(401);
    // Each poll is a fresh HTTP connection with the usual short timeout.
    while started.elapsed() < Duration::from_millis(15500) {
        assert_eq!(admin.get(path).expect(200).value()["status"], "pending");
        assert!(process.child.try_wait().unwrap().is_none());
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(admin.get(path).expect(200).value()["status"], "pending");
    let operation = loop {
        let operation = admin.get(path).expect(200).value();
        if operation["status"] != "pending" {
            break operation;
        }
        assert!(started.elapsed() < Duration::from_secs(25));
        thread::sleep(Duration::from_millis(100));
    };
    assert!(started.elapsed() >= Duration::from_secs(16));
    assert_eq!(operation["status"], "succeeded", "{operation}");
    assert_eq!(operation["http_status"], 200);
    assert_eq!(operation["result"]["removed"], true);
    assert_eq!(operation["result"]["files_removed"], true);
    assert!(!fixture.directory.join("servers/lobby-1").exists());
    assert!(
        admin.get("/api/servers").expect(200).value()["servers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Result retrieval must keep working after removal changes the snapshot.
    assert_eq!(admin.get(path).expect(200).value(), operation);
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
    Client::new(replacement).get("/api/config").expect(401);
    Client {
        token: Some(process.local_token()),
        ..Client::new(replacement)
    }
    .get("/api/config")
    .expect(200);
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
    let mut process = fixture.start(&source);
    let address: SocketAddr = process.wait_for_log("rift: admin on ").parse().unwrap();
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

#[test]
fn named_operators_enforce_permissions_and_group_scope_and_record_denials() {
    let fixture = Fixture::new();
    let source = fixture.source(&format!("token = '{TOKEN}', operators = {{ lobby_staff = {{ token = '{OPERATOR_TOKEN}', permissions = {{'read','logs','servers','audit'}}, groups = {{'lobby'}} }} }},"),
        "service_groups = { lobby = { command = {'unused'}, directory = 'lobby/{name}', port_range = {26300,26302} }, survival = { command = {'unused'}, directory = 'survival/{name}', port_range = {26303,26305} } },");
    let _process = fixture.start(&source);
    let staff = Client {
        token: Some(OPERATOR_TOKEN),
        ..fixture.client()
    };
    let admin = fixture.client().authenticated();
    assert_eq!(
        staff.get("/api/access").expect(200).value()["name"],
        "lobby_staff"
    );
    fixture.client().get("/api/status").expect(401);
    for path in [
        "/api/config",
        "/api/definitions",
        "/api/deployments",
        "/ext/hello",
    ] {
        staff.get(path).expect(403);
    }
    staff
        .instance("POST", "/api/groups/survival/instances", json!({}))
        .expect(403);
    let survival = admin
        .json("POST", "/api/groups/survival/instances", json!({}))
        .expect(202)
        .value();
    let lobby = staff
        .json("POST", "/api/groups/lobby/instances", json!({}))
        .expect(202)
        .value();
    let lobby_path = lobby["poll"].as_str().unwrap();
    let survival_path = survival["poll"].as_str().unwrap();
    staff.get(survival_path).expect(403);
    fixture.client().get(lobby_path).expect(401);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let lobby_result = staff.get(lobby_path).expect(200).value();
        let survival_result = admin.get(survival_path).expect(200).value();
        if lobby_result["status"] == "succeeded" && survival_result["status"] == "succeeded" {
            break;
        }
        assert!(Instant::now() < deadline, "operations did not complete");
        thread::sleep(Duration::from_millis(10));
    }
    staff.get(survival_path).expect(403);
    admin.get("/api/operations/unknown").expect(404);
    let state = staff.get("/api/status").expect(200).value();
    assert_eq!(state["managed_servers"].as_array().unwrap().len(), 1);
    assert_eq!(state["service_groups"].as_array().unwrap().len(), 1);
    assert_eq!(state["backends"][0]["name"], "lobby-1");
    assert!(state.get("config_path").is_none() && state.get("routes").is_none());
    staff.get("/api/servers/survival-1/logs").expect(403);
    staff.get("/api/servers/%73urvival-1/logs").expect(403);
    staff.get("/api/servers/lobby-1/logs").expect(200);
    staff
        .get("/api/servers/lobby-1/logs?cursor=bad")
        .expect(400);
    staff
        .json(
            "POST",
            "/api/servers/lobby-1/console",
            json!({"command":"secret command"}),
        )
        .expect(403);
    staff
        .instance("DELETE", "/api/instances/survival-1", json!({}))
        .expect(403);
    let audit = staff.get("/api/audit").expect(200).value();
    assert!(
        audit["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["actor"] == "lobby_staff")
    );
    assert!(
        audit["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["outcome"] == "denied")
    );
    assert!(
        audit["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["outcome"] == "accepted")
    );
    assert!(
        !audit.to_string().contains(OPERATOR_TOKEN)
            && !audit.to_string().contains("secret command")
    );
    let removed = staff
        .json("DELETE", "/api/instances/lobby-1", json!({}))
        .expect(202)
        .value();
    let removal_path = removed["poll"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let operation = staff.get(removal_path).expect(200).value();
        if operation["status"] == "succeeded" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "removal did not complete: {operation}"
        );
        thread::sleep(Duration::from_millis(10));
    }
    // Authorization uses retained group metadata after the instance disappears.
    assert!(
        staff.get("/api/servers").expect(200).value()["servers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    staff.get(removal_path).expect(200);
    let rescoped = source.replace("groups = {'lobby'}", "groups = {'survival'}");
    save(admin, &rescoped, &revision(admin)).expect(200);
    staff.get(lobby_path).expect(403);
    staff.get(removal_path).expect(403);
    staff.get(survival_path).expect(200);
    let restricted = source.replace("'read','logs','servers','audit'", "'read'");
    save(admin, &restricted, &revision(admin)).expect(200);
    staff.get(lobby_path).expect(403);
    staff.get(removal_path).expect(403);
    staff
        .instance("POST", "/api/groups/lobby/instances", json!({}))
        .expect(403);
    let renamed = restricted.replace(OPERATOR_TOKEN, "replacement-operator-token");
    save(admin, &renamed, &revision(admin)).expect(200);
    staff.get("/api/status").expect(401);
    staff.get(lobby_path).expect(401);
}

#[test]
fn loopback_without_credentials_requires_the_generated_token_and_denials_are_summarised() {
    let fixture = Fixture::new();
    let source = fixture.source("", "");
    let mut process = fixture.start(&source);
    let token = process.local_token();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    // A local process without the token, such as a plugin on a managed
    // server, is refused even though it connects over loopback.
    let anonymous = Client::new(fixture.web);
    anonymous.get("/api/status").expect(401);
    anonymous.get("/api/config").expect(401);
    anonymous.json("POST", "/api/reload", json!({})).expect(401);
    // The HTML shell still loads, so the token can be entered there.
    anonymous.get("/").expect(200);
    let client = fixture.client();
    let access = client.get("/api/access").expect(200).value();
    assert_eq!(access["name"], "local");
    assert!(access["permissions"].is_null());
    // The generated token is only valid with a loopback Host header.
    client
        .send("GET", "/api/status", &[("Host", "rift.example:8080")], "")
        .expect(403);
    client
        .json("POST", "/api/config/validate", json!({"source": source}))
        .expect(200);
    for _ in 0..200 {
        anonymous.get("/api/status").expect(401);
    }
    let audit = client.get("/api/audit").expect(200).value();
    let records = audit["records"].as_array().unwrap();
    let denials: Vec<_> = records
        .iter()
        .filter(|record| record["actor"] == "anonymous")
        .collect();
    assert_eq!(denials.len(), 1, "{audit}");
    assert_eq!(denials[0]["count"], 1);
    assert!(audit["unrecorded_denials"].as_u64().unwrap() >= 202);
    assert!(records.iter().any(|record| record["actor"] == "local"));
    // The token is printed once and stays the same for the whole run.
    client.json("POST", "/api/reload", json!({})).expect(200);
    client.get("/api/status").expect(200);
    let printed = process
        .logs
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.starts_with(LOCAL_TOKEN_LOG))
        .count();
    assert_eq!(printed, 1);
}

#[test]
fn deployments_rollback_atomically_and_history_and_audit_survive_restart() {
    let fixture = Fixture::new();
    let source = fixture.source(&format!("token = '{TOKEN}',"), "");
    let process = fixture.start(&source);
    let client = fixture.client().authenticated();
    let startup = client.get("/api/deployments").expect(200).value();
    let id = startup["deployments"][0]["id"].as_u64().unwrap();
    assert!(startup["deployments"][0].get("source").is_none());
    assert!(!startup.to_string().contains(TOKEN));
    let edited = source.replace("127.0.0.1:1", "127.0.0.1:3");
    save(client, &edited, &revision(client)).expect(200);
    let path = format!("/api/deployments/{id}/rollback");
    client
        .json("POST", &path, json!({"revision":"stale"}))
        .expect(409);
    assert_eq!(fixture.read(), edited);
    client
        .json("POST", &path, json!({"revision":revision(client)}))
        .expect(200);
    assert_eq!(fixture.read(), source);
    assert_eq!(
        client.get("/api/status").expect(200).value()["backends"][0]["address"],
        "127.0.0.1:1"
    );
    let deployments = client.get("/api/deployments").expect(200).value();
    assert_eq!(deployments["deployments"][0]["action"], "rollback");
    assert_eq!(deployments["deployments"][0]["actor"], "admin");
    client
        .json(
            "POST",
            "/api/deployments/9999/rollback",
            json!({"revision":revision(client)}),
        )
        .expect(404);
    let audit = client.get("/api/audit").expect(200).value();
    assert!(
        audit["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == 409)
    );
    drop(process);
    let _process = fixture.start(&source);
    assert!(
        client.get("/api/deployments").expect(200).value()["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["id"] == id)
    );
    assert_eq!(client.get("/api/audit").expect(200).value(), audit);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(
                fixture
                    .directory
                    .join("rift.lua.operators/deployments.json")
            )
            .unwrap()
            .permissions()
            .mode()
                & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn explicit_records_directory_survives_web_disable_and_reenable() {
    let fixture = Fixture::new();
    let source = fixture.source("records_directory = 'records',", "");
    let mut process = fixture.start(&source);
    let client = fixture.client();
    assert_eq!(
        client.get("/api/deployments").expect(200).value()["durable"],
        true
    );
    let changed = source.replace(
        "records_directory = 'records'",
        "records_directory = 'different-records'",
    );
    save(client, &changed, &revision(client)).expect(400);
    let disabled = source.replace(
        &format!(
            "web = {{ listen = '{}', records_directory = 'records', }},",
            fixture.web
        ),
        "web = false,",
    );
    save(client, &disabled, &revision(client)).expect(200);
    eventually_closed(fixture.web);
    fixture.write(&source);
    assert!(
        Command::new("kill")
            .args(["-HUP", &process.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    process.wait_for(fixture.web);
    assert_eq!(
        client.get("/api/deployments").expect(200).value()["durable"],
        true
    );
}

#[test]
fn structured_definitions_preserve_lua_and_apply_safe_group_and_template_edits() {
    let fixture = Fixture::new();
    let source = fixture.source(
        "",
        "on_http = function() return { body = 'hook preserved' } end,",
    );
    let _process = fixture.start(&source);
    let client = fixture.client();
    let definition = json!({"directory":"servers/{name}","command":["unused","{port}"],"port_range":[26400,26404]});
    client
        .json(
            "PUT",
            "/api/definitions/service_groups/lobby",
            json!({"revision":revision(client),"definition":definition}),
        )
        .expect(200);
    assert!(fixture.read().contains(&source));
    assert_eq!(client.get("/ext/test").expect(200).body, "hook preserved");
    client
        .instance("POST", "/api/groups/lobby/instances", json!({}))
        .expect(201);
    let before = revision(client);
    let mut changed = definition.clone();
    changed["command"] = json!(["different"]);
    client
        .json(
            "PUT",
            "/api/definitions/service_groups/lobby",
            json!({"revision":before,"definition":changed}),
        )
        .expect(400);
    assert_eq!(revision(client), before);
    changed = definition.clone();
    changed["scaling"] =
        json!({"min_instances":1,"max_instances":2,"capacity_per_instance":20,"cooldown_ms":60000});
    client
        .json(
            "PUT",
            "/api/definitions/service_groups/lobby",
            json!({"revision":before,"definition":changed}),
        )
        .expect(200);
    let values = client.get("/api/definitions").expect(200).value();
    assert_eq!(
        values["service_groups"]["lobby"]["scaling"]["capacity_per_instance"],
        20
    );
    let template = json!({"server_jar":"assets/server'\"é.jar","plugins":["assets/p.jar"]});
    client
        .json(
            "PUT",
            "/api/definitions/templates/game",
            json!({"revision":revision(client),"definition":template}),
        )
        .expect(200);
    assert!(
        client.get("/api/definitions").expect(200).value()["templates"]["game"]["server_jar"]
            .as_str()
            .unwrap()
            .ends_with("server'\"é.jar")
    );
    let snapshot = fixture.read();
    client
        .json(
            "PUT",
            "/api/definitions/templates/game",
            json!({"revision":before,"definition":template}),
        )
        .expect(409);
    client
        .json(
            "PUT",
            "/api/definitions/templates/game",
            json!({"revision":revision(client),"definition":{"unknown":"bad"}}),
        )
        .expect(400);
    client
        .json(
            "PUT",
            "/api/definitions/service_groups/lobby",
            json!({"revision":revision(client),"definition":null}),
        )
        .expect(400);
    assert_eq!(fixture.read(), snapshot);
    client
        .json(
            "PUT",
            "/api/definitions/templates/game",
            json!({"revision":revision(client),"definition":null}),
        )
        .expect(200);
    assert_eq!(
        fixture
            .read()
            .matches("local __rift_operator_config")
            .count(),
        1
    );
    assert_eq!(client.get("/ext/test").expect(200).body, "hook preserved");
}

#[test]
fn console_settings_edit_permission_groups_memberships_tab_list_and_server_list() {
    let fixture = Fixture::new();
    // The forwarding secret only has to exist; reuse a variable the fixture sets.
    let source = fixture.source(
        "",
        "authentication = { online_mode = true },
         forwarding = { mode = 'velocity', secret_env = 'RIFT_ADMIN_TOKEN' },
         extensions = { api_version = 2, groups = { default = { permissions = { ['chat.send'] = true } } } },
         on_http = function() return { body = 'hook preserved' } end,",
    );
    let _process = fixture.start(&source);
    let client = fixture.client();
    let values = client.get("/api/definitions").expect(200).value();
    assert_eq!(
        values["permission_groups"]["default"],
        json!({"inherits": [], "permissions": {"chat.send": true}})
    );
    assert!(values["tab_list"].is_null() && values["server_list"].is_null());
    let put = |path: &str, definition: Value| {
        client.json(
            "PUT",
            path,
            json!({"revision": revision(client), "definition": definition}),
        )
    };
    put(
        "/api/definitions/permission_groups/staff",
        json!({"inherits": ["default"], "permissions": {"network.*": true, "network.ban": false}}),
    )
    .expect(200);
    put(
        "/api/definitions/permission_groups/broken",
        json!({"permissions": {"bad node": true}}),
    )
    .expect(400);
    put("/api/definitions/permission_groups/Staff", json!({})).expect(400);
    put(
        "/api/definitions/tab_list",
        json!({"header": "Hi {player}", "footer": {"text": "{online} online", "color": "gray"}, "players": "network"}),
    )
    .expect(200);
    put(
        "/api/definitions/server_list",
        json!({"motd": {"text": "Welcome", "color": "gold"}, "max_players": 50, "sample": ["one"]}),
    )
    .expect(200);
    put(
        "/api/definitions/server_list",
        json!({"motd": "x", "favicon": "icon.png"}),
    )
    .expect(400);
    let values = client.get("/api/definitions").expect(200).value();
    assert_eq!(
        values["permission_groups"]["staff"],
        json!({"inherits": ["default"], "permissions": {"network.*": true, "network.ban": false}})
    );
    assert_eq!(
        values["tab_list"],
        json!({"header": "Hi {player}", "footer": {"text": "{online} online", "color": "gray"}, "players": "network"})
    );
    assert_eq!(values["server_list"]["motd"]["color"], "gold");
    assert_eq!(values["server_list"]["max_players"], 50);
    assert_eq!(values["server_list"]["hosts"], json!({}));
    assert!(fixture.read().contains(&source));
    assert_eq!(client.get("/ext/test").expect(200).body, "hook preserved");

    // Memberships are runtime state in the permission store, not configuration.
    let uuid = "0123456789abcdef0123456789abcdef";
    let member = |method: &str, group: &str| {
        client.json(
            method,
            &format!("/api/permissions/members/{uuid}/{group}"),
            json!({}),
        )
    };
    let before = revision(client);
    assert_eq!(member("POST", "staff").expect(200).value()["changed"], true);
    assert_eq!(
        member("POST", "staff").expect(200).value()["changed"],
        false
    );
    member("POST", "unknown").expect(400);
    member("POST", "default").expect(400);
    client
        .json("POST", "/api/permissions/members/Offline/staff", json!({}))
        .expect(404);
    assert_eq!(
        client
            .json(
                "POST",
                &format!("/api/permissions/members/{}/staff", uuid.to_uppercase()),
                json!({}),
            )
            .expect(200)
            .value()["changed"],
        false
    );
    assert_eq!(revision(client), before);
    assert_eq!(
        client.get("/api/permissions").expect(200).value(),
        json!({"enabled": true, "members": [{"uuid": uuid, "name": null, "groups": ["staff"]}], "truncated": false})
    );
    assert_eq!(
        member("DELETE", "staff").expect(200).value()["changed"],
        true
    );
    assert_eq!(
        client.get("/api/permissions").expect(200).value()["members"],
        json!([])
    );
    let audit = client.get("/api/audit").expect(200).value();
    assert!(audit["records"].as_array().unwrap().iter().any(|record| {
        record["action"] == "POST group_membership" && record["target"] == format!("{uuid}/staff")
    }));

    put("/api/definitions/server_list", Value::Null).expect(200);
    put("/api/definitions/tab_list", Value::Null).expect(200);
    let values = client.get("/api/definitions").expect(200).value();
    assert!(values["tab_list"].is_null() && values["server_list"].is_null());
}

#[test]
fn permission_group_definitions_require_extensions() {
    let fixture = Fixture::new();
    let _process = fixture.start(&fixture.source("", ""));
    let client = fixture.client();
    assert_eq!(
        client.get("/api/permissions").expect(200).value(),
        json!({"enabled": true, "members": [], "truncated": false})
    );
    assert!(client.get("/api/definitions").expect(200).value()["permission_groups"].is_null());
    client
        .json(
            "PUT",
            "/api/definitions/permission_groups/staff",
            json!({"revision": revision(client), "definition": {}}),
        )
        .expect(409);
    client
        .json(
            "POST",
            "/api/permissions/members/0123456789abcdef0123456789abcdef/staff",
            json!({}),
        )
        .expect(400);
}

#[test]
fn templates_list_software_and_files_serve_template_and_server_folders() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.directory.join("assets")).unwrap();
    fs::write(fixture.directory.join("assets/server.jar"), b"jar").unwrap();
    let source = fixture.source(&format!("token = '{TOKEN}', operators = {{ lobby_staff = {{ token = '{OPERATOR_TOKEN}', permissions = {{'read','servers','files'}}, groups = {{'lobby'}} }} }},"),
        "templates = { local_jar = { server_jar = 'assets/server.jar' }, paper = { software = { type = 'paper', version = '1.21.8' }, configs = { 'local/templates/Lobby/default' }, accept_eula = true } },\n\
         service_groups = { lobby = { command = {'unused'}, directory = 'lobby/{name}', port_range = {26310,26312} }, games = { template = 'paper', storage = 'disposable', port_range = {26313,26315} } },");
    let _process = fixture.start(&source);
    let admin = fixture.client().authenticated();
    let staff = Client {
        token: Some(OPERATOR_TOKEN),
        ..fixture.client()
    };

    let templates = admin.get("/api/templates").expect(200).value();
    let paper = templates["templates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|template| template["name"] == "paper")
        .unwrap()
        .clone();
    assert_eq!(paper["software"]["type"], "paper");
    assert_eq!(paper["software"]["version"], "1.21.8");
    assert_eq!(paper["software"]["cached"], false);
    assert_eq!(paper["groups"], json!(["games"]));
    assert!(!templates.to_string().contains("assets/server.jar"));
    let definitions = admin.get("/api/definitions").expect(200).value();
    assert_eq!(
        definitions["templates"]["paper"]["software"],
        json!({"type":"paper","version":"1.21.8"})
    );
    // Generated defaults stay implicit so structured edits keep them.
    let games = &definitions["service_groups"]["games"];
    assert!(games.get("directory").is_none() && games.get("command").is_none());
    assert_eq!(games["memory_mb"], 1024);
    assert!(definitions["service_groups"]["lobby"]["command"].is_array());
    admin
        .json("POST", "/api/templates/local_jar/download", json!({}))
        .expect(409);
    admin
        .json("POST", "/api/templates/missing/download", json!({}))
        .expect(404);
    admin
        .json(
            "POST",
            "/api/templates/paper/download",
            json!({"refresh":"no"}),
        )
        .expect(400);
    admin.get("/api/software/spigot/versions").expect(404);
    staff
        .json("POST", "/api/templates/paper/download", json!({}))
        .expect(403);
    // Scoped listings omit templates and groups outside the operator's groups.
    assert_eq!(
        staff.get("/api/templates").expect(200).value()["templates"],
        json!([])
    );

    // Template folders: write, list, read, create folders and delete.
    admin.get("/api/files/templates").expect(200);
    admin
        .send(
            "PUT",
            "/api/files/templates/Lobby/default/server.properties",
            &[("Content-Type", "application/octet-stream")],
            "motd=from web",
        )
        .expect(200);
    assert_eq!(
        fs::read_to_string(
            fixture
                .directory
                .join("local/templates/Lobby/default/server.properties")
        )
        .unwrap(),
        "motd=from web"
    );
    let listing = admin
        .get("/api/files/templates/Lobby/default")
        .expect(200)
        .value();
    assert_eq!(listing["entries"][0]["name"], "server.properties");
    assert_eq!(listing["entries"][0]["type"], "file");
    let file = admin
        .get("/api/files/templates/Lobby/default/server.properties")
        .expect(200);
    assert_eq!(file.body, "motd=from web");
    assert!(file.headers.contains("content-disposition: attachment"));
    admin
        .send(
            "PUT",
            "/api/files/templates/Lobby/default/plugins?directory",
            &[],
            "",
        )
        .expect(201);
    assert!(
        fixture
            .directory
            .join("local/templates/Lobby/default/plugins")
            .is_dir()
    );
    // Uploads may exceed the configuration editor's request limit.
    let large = "x".repeat(3 * 1024 * 1024);
    admin
        .send(
            "PUT",
            "/api/files/templates/Lobby/default/plugins/big.jar",
            &[],
            &large,
        )
        .expect(200);
    admin
        .send(
            "DELETE",
            "/api/files/templates/Lobby/default/plugins",
            &[],
            "",
        )
        .expect(200);
    assert!(
        !fixture
            .directory
            .join("local/templates/Lobby/default/plugins")
            .exists()
    );
    admin
        .get("/api/files/templates/%2e%2e/rift.lua")
        .expect(400);
    admin
        .get("/api/files/templates/Lobby/missing.txt")
        .expect(404);
    admin
        .send("DELETE", "/api/files/templates", &[], "")
        .expect(400);
    staff.get("/api/files/templates").expect(403);

    // Server folders follow group scope; ownership markers are protected.
    admin
        .instance("POST", "/api/groups/lobby/instances", json!({}))
        .expect(201);
    staff
        .send("PUT", "/api/files/servers/lobby-1/ops.json", &[], "[]")
        .expect(200);
    assert_eq!(
        fs::read_to_string(fixture.directory.join("lobby/lobby-1/ops.json")).unwrap(),
        "[]"
    );
    staff.get("/api/files/servers/lobby-1").expect(200);
    staff
        .send(
            "PUT",
            "/api/files/servers/lobby-1/.rift-instance.json",
            &[],
            "{}",
        )
        .expect(403);
    staff.get("/api/files/servers/primary").expect(403);
    admin.get("/api/files/servers/primary").expect(404);
    let audit = admin.get("/api/audit").expect(200).body;
    assert!(audit.contains("PUT files"), "{audit}");
}

#[test]
fn nodes_are_listed_and_node_tokens_only_read_template_files() {
    const NODE_TOKEN: &str = "test-node-token-with-at-least-32-bytes";
    let fixture = Fixture::new();
    let directory = &fixture.directory;
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    fs::write(directory.join("cert.pem"), cert.cert.pem()).unwrap();
    fs::write(directory.join("key.pem"), cert.signing_key.serialize_pem()).unwrap();
    fs::create_dir_all(directory.join("local/templates/game")).unwrap();
    fs::write(directory.join("local/templates/game/server.jar"), "hello").unwrap();
    // SAFETY: the variable name is unique to this test; children inherit it.
    unsafe { std::env::set_var("RIFT_NODE_HTTPNODE_TOKEN", NODE_TOKEN) };
    let source = format!(
        "return {{
  listeners = {{ main = '127.0.0.1:0' }},
  backends = {{ primary = '127.0.0.1:1', survival = '10.255.0.11:25570' }},
  routes = {{ main = 'primary' }},
  web = {{ listen = '{}', token = '{TOKEN}' }},
  messaging = {{ listen = '127.0.0.1:0', certificate = 'cert.pem', private_key = 'key.pem' }},
  nodes = {{ httpnode = {{ address = '10.255.0.11', memory_mb = 8192, labels = {{ 'games' }} }} }},
  managed_servers = {{ survival = {{ node = 'httpnode', directory = 'survival', command = {{ 'java', '-jar', 'server.jar' }} }} }},
  templates = {{ game = {{ server_jar = 'local/templates/game/server.jar' }} }},
  service_groups = {{ game = {{ template = 'game', storage = 'disposable', port_range = {{ 30000, 30009 }}, nodes = {{ labels = {{ 'games' }} }} }} }},
}}
",
        fixture.web
    );
    let _process = fixture.start(&source);
    let admin = fixture.client().authenticated();
    let nodes = admin.get("/api/nodes").expect(200).value();
    let node = &nodes["nodes"][0];
    assert_eq!(node["name"], "httpnode");
    assert_eq!(node["state"], "never_connected");
    assert_eq!(node["address"], "10.255.0.11");
    assert_eq!(node["labels"], json!(["games"]));
    assert_eq!(node["instances"], 1);
    assert!(!nodes.to_string().contains(NODE_TOKEN));
    let servers = admin.get("/api/servers").expect(200).value();
    let survival = servers["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|server| server["name"] == "survival")
        .unwrap()
        .clone();
    assert_eq!(survival["node"], "httpnode");
    assert_eq!(survival["state"], "stopped");
    assert_eq!(
        admin.get("/api/status").expect(200).value()["nodes"][0]["name"],
        "httpnode"
    );
    // Starting a server on an unreachable node reports why.
    admin
        .json("POST", "/api/servers/survival/start", json!({}))
        .expect(202);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let servers = admin.get("/api/servers").expect(200).value();
        let error = servers["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|server| server["name"] == "survival")
            .unwrap()["last_error"]
            .clone();
        if error
            .as_str()
            .is_some_and(|error| error.contains("unreachable"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "missing node error: {servers}");
        thread::sleep(Duration::from_millis(10));
    }
    // Placement refuses to create capacity while no node is online.
    let created = admin.instance("POST", "/api/groups/game/instances", json!({}));
    assert_eq!(created.status, 409, "{}", created.body);
    assert!(created.body.contains("online"), "{}", created.body);
    // A node token reads templates, with a manifest, and nothing else.
    let agent = Client {
        address: fixture.web,
        token: Some(NODE_TOKEN),
    };
    let manifest = agent
        .get("/api/files/templates/game?manifest")
        .expect(200)
        .value();
    assert_eq!(manifest["files"][0]["path"], "server.jar");
    assert_eq!(
        manifest["files"][0]["sha256"],
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    assert_eq!(
        agent
            .get("/api/files/templates/game/server.jar")
            .expect(200)
            .body,
        "hello"
    );
    agent.get("/api/servers").expect(403);
    agent.get("/api/nodes").expect(403);
    agent
        .send("PUT", "/api/files/templates/game/evil.jar", &[], "x")
        .expect(403);
    assert!(!directory.join("local/templates/game/evil.jar").exists());
}

const PERMS_TOKEN: &str = "test-permission-viewer-token";
const EDITOR_TOKEN: &str = "test-permission-editor-token";
const READER_TOKEN: &str = "test-status-reader-token-01";
const PLAYER: &str = "0123456789abcdef0123456789abcdef";

/// Admin, a `perms` viewer, a `config` editor and a `read`-only operator.
fn permission_fixture() -> (Fixture, Process) {
    let fixture = Fixture::new();
    let source = fixture.source(
        &format!(
            "token = '{TOKEN}', operators = {{ \
             viewer = {{ token = '{PERMS_TOKEN}', permissions = {{'perms'}} }}, \
             editor = {{ token = '{EDITOR_TOKEN}', permissions = {{'config'}} }}, \
             reader = {{ token = '{READER_TOKEN}', permissions = {{'read'}} }} }},"
        ),
        "",
    );
    let process = fixture.start(&source);
    (fixture, process)
}

fn as_operator(fixture: &Fixture, token: &'static str) -> Client {
    Client {
        token: Some(token),
        ..Client::new(fixture.web)
    }
}

/// Percent-encode one path segment.
fn segment(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn error_code(response: Response, status: u16) -> String {
    let value = response.expect(status).value();
    assert!(value["error"].is_string(), "{value}");
    value["code"].as_str().unwrap().to_owned()
}

#[test]
fn permission_api_authorizes_reads_and_writes_by_operator_permission() {
    let (fixture, _process) = permission_fixture();
    let viewer = as_operator(&fixture, PERMS_TOKEN);
    let editor = as_operator(&fixture, EDITOR_TOKEN);
    let reader = as_operator(&fixture, READER_TOKEN);
    let anonymous = Client::new(fixture.web);

    anonymous.get("/api/perms/revision").expect(401);
    anonymous
        .json("POST", "/api/perms/groups", json!({"name": "vip"}))
        .expect(401);
    for path in [
        "/api/perms/revision",
        "/api/perms/groups",
        "/api/perms/users",
        "/api/permissions",
    ] {
        reader.get(path).expect(403);
        viewer.get(path).expect(200);
        editor.get(path).expect(200);
    }
    // Reads are open to `perms`, writes need `config`.
    viewer
        .json("POST", "/api/perms/groups", json!({"name": "vip"}))
        .expect(403);
    viewer
        .json(
            "POST",
            &format!("/api/permissions/members/{PLAYER}/vip"),
            json!({}),
        )
        .expect(403);
    viewer.get("/api/perms/export").expect(200);
    editor
        .json("POST", "/api/perms/groups", json!({"name": "vip"}))
        .expect(201);
    // A trace is a read even though it is a POST.
    let trace = viewer
        .json(
            "POST",
            "/api/perms/trace",
            json!({"user": PLAYER, "node": "group.vip"}),
        )
        .expect(200)
        .value();
    assert_eq!(trace["result"], Value::Null);
    reader
        .json(
            "POST",
            "/api/perms/trace",
            json!({"user": PLAYER, "node": "a"}),
        )
        .expect(403);
    // Writes are recorded with the operator; reads (including the trace) are not.
    let audit = fixture
        .client()
        .authenticated()
        .get("/api/audit")
        .expect(200)
        .value();
    let records = audit["records"].as_array().unwrap();
    assert!(records.iter().any(|r| r["actor"] == "editor"
        && r["action"] == "POST perms_create_group"
        && r["outcome"] == "accepted"));
    assert!(records.iter().any(|r| r["actor"] == "viewer"
        && r["action"] == "POST perms_create_group"
        && r["outcome"] == "denied"));
    assert!(
        !records
            .iter()
            .any(|r| r["action"].as_str().unwrap().contains("trace") && r["actor"] == "viewer")
    );
    let events = viewer.get("/api/perms/audit").expect(200).value()["events"].clone();
    assert_eq!(events[0]["actor"], "operator:editor");

    // `perms` is network-wide, so group-scoped operators cannot hold it.
    let scoped = fixture.source(
        &format!("token = '{TOKEN}', operators = {{ viewer = {{ token = '{PERMS_TOKEN}', permissions = {{'perms'}}, groups = {{'lobby'}} }} }},"),
        "",
    );
    let admin = fixture.client().authenticated();
    let rejected = admin
        .json("POST", "/api/config/validate", json!({"source": scoped}))
        .value();
    assert!(
        rejected
            .to_string()
            .contains("perms require unrestricted group access"),
        "{rejected}"
    );
    let unknown = fixture.source(
        &format!("token = '{TOKEN}', operators = {{ viewer = {{ token = '{PERMS_TOKEN}', permissions = {{'permissions'}} }} }},"),
        "",
    );
    let rejected = admin
        .json("POST", "/api/config/validate", json!({"source": unknown}))
        .value();
    assert!(rejected.to_string().contains("files, perms"), "{rejected}");
}

#[test]
fn permission_api_edits_groups_users_tracks_and_nodes() {
    let (fixture, _process) = permission_fixture();
    let admin = fixture.client().authenticated();
    let editor = as_operator(&fixture, EDITOR_TOKEN);

    // Groups.
    let created = editor
        .json(
            "POST",
            "/api/perms/groups",
            json!({"name": "VIP", "weight": 10, "display_name": "VIP"}),
        )
        .expect(201)
        .value();
    assert_eq!(created["name"], "vip");
    assert!(created["revision"].as_u64().unwrap() > 0);
    assert_eq!(
        error_code(
            editor.json("POST", "/api/perms/groups", json!({"name": "vip"})),
            409
        ),
        "conflict"
    );
    assert_eq!(
        error_code(
            editor.json("POST", "/api/perms/groups", json!({"name": "bad name"})),
            400
        ),
        "invalid_request"
    );
    editor
        .json(
            "POST",
            "/api/perms/groups",
            json!({"name": "x", "colour": 1}),
        )
        .expect(400);
    editor
        .send(
            "POST",
            "/api/perms/groups",
            &[("Content-Type", "text/plain")],
            "{\"name\":\"x\"}",
        )
        .expect(415);
    editor
        .json(
            "POST",
            "/api/perms/groups",
            json!({"name": "staff", "weight": 50}),
        )
        .expect(201);
    editor
        .json(
            "PATCH",
            "/api/perms/groups/vip",
            json!({"weight": 20, "reason": "rebalance"}),
        )
        .expect(200);
    editor
        .json("PATCH", "/api/perms/groups/vip", json!({}))
        .expect(400);
    assert_eq!(
        error_code(
            editor.json("PATCH", "/api/perms/groups/ghost", json!({"weight": 1})),
            404
        ),
        "not_found"
    );
    let put = |path: &str, node: Value| editor.json("PUT", path, node).expect(200).value();
    let kit = put(
        "/api/perms/groups/vip/nodes",
        json!({"kind": "permission", "key": "Kit.VIP", "value": true}),
    );
    assert_eq!(kit["id"], "permission:kit.vip");
    assert_eq!(kit["ops"], 1);
    put(
        "/api/perms/groups/vip/nodes",
        json!({"kind": "prefix", "priority": 100, "text": "&6[VIP] "}),
    );
    put(
        "/api/perms/groups/staff/nodes",
        json!({"kind": "inherit", "group": "vip", "value": true}),
    );
    assert_eq!(
        error_code(
            editor.json(
                "PUT",
                "/api/perms/groups/ghost/nodes",
                json!({"kind": "permission", "key": "a", "value": true}),
            ),
            404
        ),
        "not_found"
    );
    assert_eq!(
        error_code(
            editor.json(
                "PUT",
                "/api/perms/groups/vip/nodes",
                json!({"kind": "permission", "key": "bad node", "value": true}),
            ),
            400
        ),
        "invalid_request"
    );
    let groups = admin.get("/api/perms/groups").expect(200).value();
    let names: Vec<_> = groups["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["default", "staff", "vip"]);
    assert_eq!(groups["groups"][1]["parents"][0]["group"], "vip");
    assert_eq!(groups["groups"][2]["weight"], 20);
    assert_eq!(groups["groups"][2]["display_name"], "VIP");
    let vip = admin.get("/api/perms/groups/vip").expect(200).value();
    assert!(
        vip["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == "prefix:100" && n["node"]["text"] == "&6[VIP] ")
    );
    admin.get("/api/perms/groups/ghost").expect(404);
    assert!(
        admin
            .get("/api/perms/groups/staff/nodes")
            .expect(200)
            .value()["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == "inherit:vip")
    );

    // Users: nodes with contexts and expiry, by UUID (either form).
    let user = format!("/api/perms/users/{PLAYER}");
    put(
        &format!("{user}/nodes"),
        json!({"kind": "inherit", "group": "staff", "value": true}),
    );
    let fly = put(
        "/api/perms/users/01234567-89ab-cdef-0123-456789abcdef/nodes",
        json!({"kind": "permission", "key": "essentials.fly", "value": true,
               "contexts": {"server": "survival"}, "expires_at": 4_000_000_000u64}),
    );
    let fly_id = fly["id"].as_str().unwrap().to_owned();
    assert_eq!(fly_id, "permission:essentials.fly@server=survival");
    assert_eq!(fly["changed_users"], json!([PLAYER]));
    assert_eq!(
        error_code(
            editor.json(
                "PUT",
                &format!("{user}/nodes"),
                json!({"kind": "weight", "weight": 1})
            ),
            400
        ),
        "invalid_request"
    );
    editor
        .json(
            "PUT",
            "/api/perms/users/Nobody/nodes",
            json!({"kind": "permission", "key": "a", "value": true}),
        )
        .expect(404);
    editor
        .json(
            "PUT",
            "/api/perms/users/not%20a%20name/nodes",
            json!({"kind": "permission", "key": "a", "value": true}),
        )
        .expect(400);
    let info = admin.get(&user).expect(200).value();
    assert_eq!(info["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(info["effective_primary_group"], "staff");
    assert_eq!(info["primary_group"], Value::Null);
    assert_eq!(info["online"], false);
    editor
        .json("PATCH", &user, json!({"primary_group": "vip"}))
        .expect(200);
    assert_eq!(admin.get(&user).expect(200).value()["primary_group"], "vip");
    editor
        .json("PATCH", &user, json!({"primary_group": null}))
        .expect(200);
    editor.json("PATCH", &user, json!({})).expect(400);

    // Search users by UUID prefix and by group.
    let found = admin
        .get("/api/perms/users?q=01234567-89")
        .expect(200)
        .value();
    assert_eq!(found["users"][0]["uuid"], PLAYER);
    assert_eq!(found["users"][0]["groups"], json!(["staff"]));
    assert_eq!(
        admin
            .get("/api/perms/users?group=staff")
            .expect(200)
            .value()["users"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        admin.get("/api/perms/users?group=vip").expect(200).value()["users"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    admin.get("/api/perms/users?limit=0").expect(400);
    admin.get("/api/perms/users?colour=red").expect(400);

    // The compiled subject, and the meta in the requested contexts.
    let compiled = admin
        .get(&format!("{user}/compiled?server=survival"))
        .expect(200)
        .value();
    assert_eq!(compiled["subject"]["uuid"], PLAYER);
    assert_eq!(compiled["contexts"], json!({"server": "survival"}));
    assert_eq!(compiled["contexts_from"], "request");
    assert_eq!(compiled["meta"]["prefix"], "&6[VIP] ");
    assert_eq!(compiled["groups"], json!(["staff", "vip", "default"]));
    assert!(
        compiled["subject"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["key"] == "essentials.fly" && e["rank"] == 0)
    );
    let offline = admin.get(&format!("{user}/compiled")).expect(200).value();
    assert_eq!(offline["contexts_from"], "proxy");
    assert_eq!(offline["contexts"], json!({}));
    admin.get(&format!("{user}/compiled?a%3Bb=x")).expect(400);

    // Trace: the winner, the losers and which subject each rank is.
    let trace = |node: &str, contexts: Value| {
        admin
            .json(
                "POST",
                "/api/perms/trace",
                json!({"user": PLAYER, "node": node, "contexts": contexts}),
            )
            .expect(200)
            .value()
    };
    let traced = trace("essentials.fly", json!({"server": "survival"}));
    assert_eq!(traced["result"], true);
    assert_eq!(traced["winner"]["entry"]["rank"], 0);
    assert_eq!(
        traced["ranks"][0],
        json!({"rank": 0, "subject": format!("user:{PLAYER}")})
    );
    let elsewhere = trace("essentials.fly", json!({"server": "lobby"}));
    assert_eq!(elsewhere["result"], Value::Null);
    assert_eq!(elsewhere["losers"][0]["outcome"], "context_mismatch");
    let inherited = trace("kit.vip", json!({}));
    assert_eq!(inherited["result"], true);
    let rank = inherited["winner"]["entry"]["rank"].clone();
    assert!(
        inherited["ranks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["rank"] == rank && r["subject"] == "group:vip"),
        "{inherited}"
    );
    admin
        .json(
            "POST",
            "/api/perms/trace",
            json!({"user": PLAYER, "node": "bad node"}),
        )
        .expect(400);

    // Moving a node to another context keeps one node; unknown ids are 404.
    let moved = editor
        .json(
            "PUT",
            &format!("{user}/nodes/{}", segment(&fly_id)),
            json!({"kind": "permission", "key": "essentials.fly", "value": true,
                   "contexts": {"server": "lobby"}}),
        )
        .expect(200)
        .value();
    assert_eq!(moved["id"], "permission:essentials.fly@server=lobby");
    let ids: Vec<_> = admin.get(&format!("{user}/nodes")).expect(200).value()["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        ["inherit:staff", "permission:essentials.fly@server=lobby"]
    );
    editor
        .json(
            "PUT",
            &format!("{user}/nodes/{}", segment("permission:missing")),
            json!({"kind": "permission", "key": "missing", "value": true}),
        )
        .expect(404);
    editor
        .json(
            "DELETE",
            &format!("{user}/nodes/{}", segment("nonsense")),
            json!({}),
        )
        .expect(400);
    let lobby = segment("permission:essentials.fly@server=lobby");
    let removed = editor
        .json(
            "DELETE",
            &format!("{user}/nodes/{lobby}"),
            json!({"reason": "cleanup"}),
        )
        .expect(200)
        .value();
    assert_eq!(removed["ops"], 1);
    let again = editor
        .json("DELETE", &format!("{user}/nodes/{lobby}"), json!({}))
        .expect(200)
        .value();
    assert_eq!(again["ops"], 0);

    // Tracks, promotion and demotion.
    let second = "00000000000000000000000000000002";
    editor
        .json(
            "PUT",
            "/api/perms/tracks/ranks",
            json!({"groups": ["default", "vip", "staff"]}),
        )
        .expect(200);
    editor
        .json(
            "PUT",
            "/api/perms/tracks/ranks",
            json!({"groups": ["vip", "vip"]}),
        )
        .expect(400);
    assert_eq!(
        admin.get("/api/perms/tracks").expect(200).value()["tracks"],
        json!([{"name": "ranks", "groups": ["default", "vip", "staff"]}])
    );
    admin.get("/api/perms/tracks/ranks").expect(200);
    admin.get("/api/perms/tracks/ghost").expect(404);
    let promote = |direction: &str| {
        editor.json(
            "POST",
            &format!("/api/perms/tracks/ranks/{direction}"),
            json!({"user": second}),
        )
    };
    promote("promote").expect(200);
    let promoted = promote("promote").expect(200).value();
    assert_eq!(promoted["user"], second);
    assert_eq!(
        admin
            .get(&format!("/api/perms/users/{second}"))
            .expect(200)
            .value()["nodes"][0]["id"],
        "inherit:staff"
    );
    assert_eq!(error_code(promote("promote"), 409), "conflict");
    promote("demote").expect(200);
    assert_eq!(
        admin
            .get(&format!("/api/perms/users/{second}"))
            .expect(200)
            .value()["tracks"],
        json!(["ranks"])
    );
    editor
        .json(
            "POST",
            "/api/perms/tracks/ghost/promote",
            json!({"user": second}),
        )
        .expect(404);
    editor
        .json("DELETE", "/api/perms/tracks/ranks", json!({}))
        .expect(200);
    admin.get("/api/perms/tracks/ranks").expect(404);

    // Registered nodes, revision and backends.
    assert_eq!(
        admin.get("/api/perms/registered").expect(200).value(),
        json!({"registered": []})
    );
    let revision = admin.get("/api/perms/revision").expect(200).value()["revision"]
        .as_u64()
        .unwrap();
    assert!(revision > 5);
    assert_eq!(
        admin.get("/api/perms/backends").expect(200).value(),
        json!({"revision": revision, "backends": []})
    );

    // Who holds a node.
    let holders = admin
        .get("/api/perms/search?node=kit.vip")
        .expect(200)
        .value();
    assert_eq!(holders["holders"][0]["subject"], json!({"group": "vip"}));
    assert_eq!(holders["holders"][0]["id"], "permission:kit.vip");
    let members = admin
        .get("/api/perms/search?kind=inherit&node=staff&subjects=users")
        .expect(200)
        .value();
    assert_eq!(members["holders"][0]["subject"], json!({"user": PLAYER}));
    admin.get("/api/perms/search?subjects=nobody").expect(400);
    admin.get("/api/perms/search?node=a&like=b").expect(400);

    // Raw changes and bulk edits with a dry run.
    let applied = editor
        .json(
            "POST",
            "/api/perms/apply",
            json!({"change": {"op": "set_node", "subject": {"group": "vip"},
                              "node": {"kind": "meta", "key": "homes", "value": "5"}},
                   "reason": "from a script"}),
        )
        .expect(200)
        .value();
    assert_eq!(applied["ops"], 1);
    editor
        .json(
            "POST",
            "/api/perms/apply",
            json!({"change": {"op": "explode"}}),
        )
        .expect(400);
    editor
        .json(
            "POST",
            "/api/perms/apply",
            json!({"change": {"op": "delete_group", "name": "default"}}),
        )
        .expect(400);
    let before = admin.get("/api/perms/revision").expect(200).value()["revision"].clone();
    let filter = json!({"kind": "permission", "key_like": "kit.%"});
    let patch = json!({"value": false});
    let preview = editor
        .json(
            "POST",
            "/api/perms/bulk",
            json!({"filter": filter, "patch": patch, "dry_run": true}),
        )
        .expect(200)
        .value();
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["count"], 1);
    assert_eq!(preview["ops"][0]["after"]["value"], false);
    assert_eq!(
        admin.get("/api/perms/revision").expect(200).value()["revision"],
        before
    );
    let done = editor
        .json(
            "POST",
            "/api/perms/bulk",
            json!({"filter": filter, "patch": patch}),
        )
        .expect(200)
        .value();
    assert_eq!(done["dry_run"], false);
    assert_eq!(done["ops"], 1);
    editor
        .json(
            "POST",
            "/api/perms/bulk",
            json!({"filter": {}, "patch": {}}),
        )
        .expect(400);

    // Audit filters.
    let audit = |query: &str| {
        admin
            .get(&format!("/api/perms/audit{query}"))
            .expect(200)
            .value()["events"]
            .as_array()
            .unwrap()
            .clone()
    };
    let all = audit("?limit=1000");
    assert!(
        all.windows(2)
            .all(|w| w[0]["revision"].as_u64() < w[1]["revision"].as_u64())
    );
    assert!(all.iter().all(|e| e["actor"] == "operator:editor"));
    assert_eq!(audit("?limit=2").len(), 2);
    let since = all[2]["revision"].as_u64().unwrap();
    let after = audit(&format!("?since={since}&limit=1"));
    assert_eq!(after[0]["revision"].as_u64().unwrap(), since + 1);
    let vip_events = audit("?subject=group:vip");
    assert!(!vip_events.is_empty());
    assert!(
        vip_events
            .iter()
            .all(|e| e["groups"].as_array().unwrap().contains(&json!("vip")))
    );
    let user_events = audit(&format!("?subject={PLAYER}"));
    assert!(!user_events.is_empty());
    assert!(
        user_events
            .iter()
            .all(|e| e["changed"].as_array().unwrap().contains(&json!(PLAYER)))
    );
    assert!(audit("?actor=operator:nobody").is_empty());
    assert_eq!(audit("?actor=operator:editor").len(), all.len().min(100));
    let reasons: Vec<_> = all.iter().filter_map(|e| e["reason"].as_str()).collect();
    for reason in ["rebalance", "from a script", "cleanup"] {
        assert!(reasons.contains(&reason), "{reasons:?}");
    }
    admin.get("/api/perms/audit?actor=nobody").expect(400);
    admin.get("/api/perms/audit?since=x").expect(400);

    // Clearing and deleting.
    editor
        .json("DELETE", &format!("{user}/nodes"), json!({}))
        .expect(200);
    assert!(
        admin.get(&user).expect(200).value()["nodes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    editor
        .json("DELETE", "/api/perms/groups/default", json!({}))
        .expect(400);
    editor
        .json("DELETE", "/api/perms/groups/staff", json!({}))
        .expect(200);
    admin.get("/api/perms/groups/staff").expect(404);
    // Each write left an operator audit record naming its target.
    let records = admin.get("/api/audit").expect(200).value()["records"].clone();
    let records = records.as_array().unwrap();
    for (action, target) in [
        ("PUT perms_group_nodes", "vip".to_owned()),
        ("PATCH perms_group", "vip".to_owned()),
        (
            "DELETE perms_user_nodes",
            format!("{PLAYER}/permission:essentials.fly@server=lobby"),
        ),
        ("POST perms_promote", "ranks".to_owned()),
        ("POST perms_bulk", String::new()),
    ] {
        assert!(
            records.iter().any(|r| r["action"] == action
                && r["target"] == target.as_str()
                && r["outcome"] == "accepted"),
            "{action} {target}: {records:?}"
        );
    }
    // The old membership routes still work on the same store.
    admin
        .json(
            "POST",
            &format!("/api/permissions/members/{PLAYER}/vip"),
            json!({}),
        )
        .expect(200);
    assert_eq!(
        admin.get(&user).expect(200).value()["nodes"][0]["id"],
        "inherit:vip"
    );
}

#[test]
fn permission_api_imports_and_exports_luckperms_files() {
    use flate2::{Compression, read::GzDecoder, write::GzEncoder};
    let (fixture, _process) = permission_fixture();
    let admin = fixture.client().authenticated();
    let editor = as_operator(&fixture, EDITOR_TOKEN);
    let viewer = as_operator(&fixture, PERMS_TOKEN);
    let file = fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/luckperms-export.json"
    ))
    .unwrap();
    let import = |client: Client, query: &str, kind: &str, body: &[u8]| {
        client.send_bytes(
            "POST",
            &format!("/api/perms/import{query}"),
            &[("Content-Type", kind)],
            body,
        )
    };
    import(viewer, "", "application/json", &file).expect(403);
    import(editor, "", "text/plain", &file).expect(415);
    assert_eq!(
        error_code(
            import(editor, "", "application/json", b"{\"groups\": 7}"),
            400
        ),
        "invalid_request"
    );
    let report = import(editor, "", "application/json", &file)
        .expect(200)
        .value();
    assert_eq!(report["groups"], 4);
    assert_eq!(report["tracks"], 1);
    assert!(report["ops"].as_u64().unwrap() > 10);
    assert!(
        !report["skipped"].as_array().unwrap().is_empty(),
        "{report}"
    );
    let groups = admin.get("/api/perms/groups").expect(200).value();
    let vip = groups["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "vip")
        .unwrap()
        .clone();
    assert_eq!(vip["weight"], 10);
    assert_eq!(vip["display_name"], "VIP");
    assert_eq!(
        admin.get("/api/perms/tracks/staff").expect(200).value()["groups"],
        json!(["default", "vip", "mod", "admin"])
    );
    let event = admin.get("/api/perms/audit?limit=1").expect(200).value()["events"][0].clone();
    assert_eq!(event["actor"], "operator:editor");
    assert_eq!(event["reason"], "LuckPerms import");

    // Export, then import the gzipped export with replace: nothing changes and
    // a second export is identical.
    let exported = viewer.get("/api/perms/export").expect(200);
    assert!(exported.headers.contains("content-type: application/json"));
    let exported = exported.value();
    assert!(exported["groups"]["vip"]["nodes"].as_array().unwrap().len() > 3);
    let gzipped = viewer.get("/api/perms/export?gzip").expect(200);
    assert!(gzipped.headers.contains("content-type: application/gzip"));
    assert!(gzipped.headers.contains("luckperms-export.json.gz"));
    let mut text = String::new();
    GzDecoder::new(&gzipped.raw[..])
        .read_to_string(&mut text)
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), exported);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(text.as_bytes()).unwrap();
    let gzip = encoder.finish().unwrap();
    // Additive: every node is already in its slot.
    let again = import(editor, "", "application/gzip", &gzip)
        .expect(200)
        .value();
    assert_eq!(again["ops"], 0, "{again}");
    // Replacing rewrites the imported nodes to the same result.
    import(editor, "?replace=true", "application/gzip", &gzip).expect(200);
    assert_eq!(admin.get("/api/perms/export").expect(200).value(), exported);
    admin.get("/api/perms/export?gzip=maybe").expect(400);
    import(editor, "?replace=perhaps", "application/json", &file).expect(400);
}
