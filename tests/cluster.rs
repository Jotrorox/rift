//! Three real peers of a cluster on loopback (docs/clustering.md): created
//! with `rift cluster init` and `rift admin cluster add|promote`, replicating
//! permission changes and configuration deployments, surviving a leader
//! killed without warning, deciding simultaneous logins of one name on two
//! peers, restoring a backup into a new cluster, and draining a peer with
//! `rift admin cluster leave`, which sends a 1.20.5 client to another peer
//! with the Transfer packet and gives a 1.8 client a reconnect message.
//!
//! The first configuration places a named server on peer `a`'s own node, so
//! `rift cluster init` (also `--from-backup`) and every peer's start read a
//! configuration that names a peer-node. The server never starts.
//!
//! When the test fails, the fixture prints every running peer's last log
//! lines and its `GET /api/cluster` before it stops them.
use rift::protocol::{
    Codec, Handshake, MAX_FRAME_SIZE, NextState, Packet, read_string, read_varint, write_string,
    write_varint,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const ADMIN_TOKEN: &str = "rift-cluster-integration-admin-token-32-bytes";
const WEB_TOKEN: &str = "rift-cluster-integration-web-token";
const CLUSTER_TOKEN: &str = "rift-cluster-integration-cluster-token";
const TOKEN_ENV: &str = "RIFT_TEST_CLUSTER_TOKEN";
/// Every wait in this test: debug builds on shared CI runners are slow.
const WAIT: Duration = Duration::from_secs(90);
/// The new leader after the old one was killed is elected within this.
const ELECTION_BOUND: Duration = Duration::from_secs(45);
/// Play Disconnect of 1.8 and 1.20.5, and Play Transfer of 1.20.5.
const DISCONNECT_47: i32 = 0x40;
const DISCONNECT_766: i32 = 0x1d;
const TRANSFER_766: i32 = 0x73;

/// One peer: its directory and the ports its configuration gives it.
struct Peer {
    name: &'static str,
    directory: PathBuf,
    game: u16,
    web: u16,
    admin: u16,
    cluster: u16,
    process: Option<Process>,
}

struct Process {
    child: Child,
    logs: Arc<Mutex<Vec<String>>>,
}

impl Drop for Process {
    fn drop(&mut self) {
        // SIGKILL on Unix, TerminateProcess on Windows.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Fixture {
    root: PathBuf,
    peers: Vec<Peer>,
    backend: SocketAddr,
    /// The port of the named server on peer `a`'s node, which never starts.
    spare: u16,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("cluster test failed; the peers:{}", self.diagnostics());
        }
        for peer in &mut self.peers {
            peer.process.take();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Ports below every platform's ephemeral range that are free for TCP and
/// UDP right now (the cluster listens on UDP, everything else on TCP).
fn free_ports(count: usize) -> Vec<u16> {
    const RANGE: std::ops::Range<u16> = 20_000..32_000;
    let span = usize::from(RANGE.end - RANGE.start);
    let offset = std::process::id() as usize * 131 + 6_000;
    let mut ports = Vec::with_capacity(count);
    for step in 0..span {
        let port = RANGE.start + ((offset + step) % span) as u16;
        let tcp = TcpListener::bind(("127.0.0.1", port));
        let udp = UdpSocket::bind(("127.0.0.1", port));
        if tcp.is_ok() && udp.is_ok() {
            ports.push(port);
            if ports.len() == count {
                return ports;
            }
        }
    }
    panic!("no free fixture ports in {RANGE:?}");
}

/// A cluster CA, and a certificate and key for a peer at 127.0.0.1 signed
/// by it, usable for both server and client authentication.
struct Authority {
    issuer: rcgen::CertifiedIssuer<'static, rcgen::KeyPair>,
}

impl Authority {
    fn new() -> Self {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "rift cluster test CA");
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let issuer =
            rcgen::CertifiedIssuer::self_signed(params, rcgen::KeyPair::generate().unwrap())
                .unwrap();
        Self { issuer }
    }

    /// Write `ca.pem`, `peer.pem` and `peer.key` into `directory`.
    fn issue(&self, directory: &Path, name: &str) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params =
            rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned(), "localhost".to_owned()])
                .unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, format!("rift peer {name}"));
        let certificate = params.signed_by(&key, &*self.issuer).unwrap();
        fs::write(directory.join("ca.pem"), self.issuer.pem()).unwrap();
        fs::write(directory.join("peer.pem"), certificate.pem()).unwrap();
        fs::write(directory.join("peer.key"), key.serialize_pem()).unwrap();
    }
}

impl Fixture {
    /// Directories, certificates and configuration for peers `a`, `b`, `c`
    /// (one cluster) and `z` (a cluster restored from a backup later), and
    /// a fake backend every peer routes to.
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "rift-cluster-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let backend = TcpListener::bind("127.0.0.1:0").unwrap();
        let backend_address = backend.local_addr().unwrap();
        thread::spawn(move || serve_backends(backend));
        let names = ["a", "b", "c", "z"];
        let ports = free_ports(names.len() * 4 + 1);
        let spare = ports[names.len() * 4];
        let authority = Authority::new();
        let peers: Vec<Peer> = names
            .into_iter()
            .zip(ports.chunks(4))
            .map(|(name, ports)| {
                let directory = root.join(name);
                fs::create_dir_all(&directory).unwrap();
                authority.issue(&directory, name);
                Peer {
                    name,
                    directory,
                    game: ports[0],
                    web: ports[1],
                    admin: ports[2],
                    cluster: ports[3],
                    process: None,
                }
            })
            .collect();
        let fixture = Self {
            root,
            peers,
            backend: backend_address,
            spare,
        };
        let source = fixture.first_source();
        for peer in &fixture.peers {
            // `z` starts a cluster of its own and contacts nobody.
            let seeds: Vec<String> = fixture
                .peers
                .iter()
                .filter(|other| peer.name != "z" && other.name != "z" && other.name != peer.name)
                .map(|other| format!("\"127.0.0.1:{}\"", other.cluster))
                .collect();
            fs::write(
                peer.directory.join("cluster.toml"),
                format!(
                    "peer = \"{}\"\nlisten = \"127.0.0.1:{}\"\ncertificate = \"peer.pem\"\n\
                     private_key = \"peer.key\"\nca = \"ca.pem\"\ntoken_env = \"{TOKEN_ENV}\"\n\
                     seeds = [{}]\npublic_address = \"127.0.0.1:{}\"\n",
                    peer.name,
                    peer.cluster,
                    seeds.join(", "),
                    peer.game
                ),
            )
            .unwrap();
            if peer.name != "z" {
                fs::write(peer.directory.join("rift.lua"), &source).unwrap();
            }
        }
        fixture
    }

    /// The configuration `rift cluster init` reads and every peer starts
    /// with: [`Fixture::source`] with a named server on peer `a`'s own node
    /// (a peer-node), which neither starts by itself nor on a connection.
    fn first_source(&self) -> String {
        self.source_with_server("first configuration")
    }

    /// [`Fixture::source`] with the named server. Deployments keep the
    /// server: changing a managed server definition needs a restart.
    /// The server sits on peer `a`'s node; in the cluster that peer `z`
    /// restores from a backup, where `z` is the only peer, on `z`'s.
    fn source_with_server(&self, marker: &str) -> String {
        let server = format!(
            "config.backends.spare = '127.0.0.1:{}'\n\
             config.managed_servers = {{ spare = {{ node = rift.peer == 'z' and 'z' or 'a', directory = 'spare', \
             command = {{ {:?}, '--version' }}, autostart = false, start_on_connect = false }} }}\n\
             return config\n",
            self.spare,
            env!("CARGO_BIN_EXE_rift")
        );
        self.source(marker)
            .replacen("return {", "local config = {", 1)
            + &server
    }

    /// The configuration every peer runs; `marker` makes revisions differ.
    /// Listeners, the admin and the web listener are peer sections, so each
    /// peer picks its own ports by `rift.peer`.
    fn source(&self, marker: &str) -> String {
        let ports: String = self
            .peers
            .iter()
            .map(|peer| {
                format!(
                    "    {} = {{ game = {}, web = {}, admin = {} }},\n",
                    peer.name, peer.game, peer.web, peer.admin
                )
            })
            .collect();
        format!(
            "-- {marker}\n\
             local ports = {{\n{ports}}}\n\
             -- rift.peer is this peer's name, also while `rift cluster init` reads this file.\n\
             local own = ports[rift.peer] or ports.a\n\
             return {{\n\
             \x20   listeners = {{ public = '127.0.0.1:' .. own.game }},\n\
             \x20   backends = {{ lobby = '{}' }},\n\
             \x20   routes = {{ public = 'lobby' }},\n\
             \x20   translation = {{ detect = false }},\n\
             \x20   admin = {{ listen = '127.0.0.1:' .. own.admin, permissions = {{ 'status', 'cluster', 'shutdown' }} }},\n\
             \x20   web = {{ listen = '127.0.0.1:' .. own.web, token = '{WEB_TOKEN}' }},\n\
             \x20   cluster = {{ peer_timeout_ms = 4000 }},\n\
             \x20   shutdown_timeout_ms = 5000,\n\
             }}\n",
            self.backend
        )
    }

    fn peer(&self, name: &str) -> &Peer {
        self.peers.iter().find(|peer| peer.name == name).unwrap()
    }

    fn peer_mut(&mut self, name: &str) -> &mut Peer {
        self.peers
            .iter_mut()
            .find(|peer| peer.name == name)
            .unwrap()
    }

    fn command(directory: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rift"));
        command
            .current_dir(directory)
            .env(TOKEN_ENV, CLUSTER_TOKEN)
            .env("RIFT_ADMIN_TOKEN", ADMIN_TOKEN);
        command
    }

    /// Start `name`'s Rift (`rift` in its directory, which holds rift.lua
    /// and cluster.toml).
    fn start(&mut self, name: &str) {
        let peer = self.peer_mut(name);
        assert!(peer.process.is_none(), "{name} runs already");
        let mut child = Self::command(&peer.directory)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let logs = Arc::new(Mutex::new(Vec::new()));
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        collect(stdout, logs.clone(), "out: ");
        collect(stderr, logs.clone(), "");
        peer.process = Some(Process { child, logs });
    }

    /// Kill `name`'s Rift without warning.
    fn kill(&mut self, name: &str) {
        self.peer_mut(name).process.take();
    }

    /// The last log lines of every running peer, for failure messages.
    fn logs(&self) -> String {
        let mut text = String::new();
        for peer in &self.peers {
            if let Some(process) = &peer.process {
                text.push_str(&format!("\n--- {} ---\n", peer.name));
                text.push_str(&tail(&process.logs, 40));
            }
        }
        text
    }

    /// Every running peer's last log lines and its `GET /api/cluster`
    /// (asked briefly: a peer that hangs must not hang the report).
    fn diagnostics(&self) -> String {
        let mut text = String::new();
        for peer in &self.peers {
            let Some(process) = &peer.process else {
                continue;
            };
            let status = http_within(
                peer.web,
                "GET",
                "/api/cluster",
                None,
                Duration::from_secs(3),
            )
            .map_or_else(
                || "no answer".to_owned(),
                |reply| format!("{} {}", reply.status, reply.body),
            );
            text.push_str(&format!(
                "\n--- {} (game {}, web {}, cluster {}) ---\nGET /api/cluster: {status}\n",
                peer.name, peer.game, peer.web, peer.cluster
            ));
            text.push_str(&tail(&process.logs, 80));
        }
        text
    }

    /// Run a short-lived `rift` command in `name`'s directory.
    fn run(&self, name: &str, args: &[&str]) -> Output {
        let mut command = Self::command(&self.peer(name).directory);
        command.args(args);
        output_within(command, Duration::from_secs(120), &format!("rift {args:?}"))
    }

    /// `rift admin --address <name's admin> <args>`: its JSON, or what it
    /// printed on failure.
    fn admin(&self, name: &str, args: &[&str]) -> Result<Value, String> {
        let peer = self.peer(name);
        let mut command = Self::command(&peer.directory);
        command
            .args(["admin", "--address", &format!("127.0.0.1:{}", peer.admin)])
            .args(args);
        let output = output_within(command, Duration::from_secs(150), &format!("{args:?}"));
        if output.status.success() {
            serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
        } else {
            Err(format!(
                "{}{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            ))
        }
    }

    /// A request to `name`'s web API with the web token; `None` when the
    /// API does not answer.
    fn http(&self, name: &str, method: &str, path: &str, body: Option<&Value>) -> Option<Reply> {
        http(self.peer(name).web, method, path, body)
    }

    /// `GET /api/cluster` on `name`.
    fn status(&self, name: &str) -> Option<Value> {
        self.http(name, "GET", "/api/cluster", None)
            .filter(|reply| reply.status == 200)
            .map(|reply| reply.body)
    }

    /// Wait until `name` serves its web API and is a member with a leader.
    fn wait_joined(&self, name: &str) -> Value {
        self.wait_until(&format!("{name} to join"), || {
            self.status(name)
                .filter(|status| status["joined"] == true && status["leader"].is_string())
        })
    }

    /// The groups of `name`'s permission store.
    fn groups(&self, name: &str) -> Option<BTreeSet<String>> {
        let reply = self
            .http(name, "GET", "/api/perms/groups", None)
            .filter(|reply| reply.status == 200)?;
        Some(
            reply.body["groups"]
                .as_array()?
                .iter()
                .filter_map(|group| group["name"].as_str().map(str::to_owned))
                .collect(),
        )
    }

    /// Create permission group `group` through `name`, retrying while the
    /// cluster settles (a retry of a write that did apply finds it).
    fn create_group(&self, name: &str, group: &str) {
        self.wait_until(&format!("{name} to create group {group}"), || {
            let reply = self.http(
                name,
                "POST",
                "/api/perms/groups",
                Some(&json!({ "name": group })),
            )?;
            matches!(reply.status, 201 | 409).then_some(())
        });
    }

    fn wait_for_group(&self, name: &str, group: &str) {
        self.wait_until(&format!("group {group} on {name}"), || {
            self.groups(name)
                .filter(|groups| groups.contains(group))
                .map(|_| ())
        });
    }

    /// Poll `check` until it returns something, for at most [`WAIT`].
    fn wait_until<T>(&self, what: &str, mut check: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(value) = check() {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; logs:{}",
                self.logs()
            );
            thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Append every line `pipe` prints to `logs`, prefixed with `prefix`.
fn collect(pipe: impl Read + Send + 'static, logs: Arc<Mutex<Vec<String>>>, prefix: &'static str) {
    thread::spawn(move || {
        for line in BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            logs.lock().unwrap().push(format!("{prefix}{line}"));
        }
    });
}

/// The last `count` lines of `logs`, one per line.
fn tail(logs: &Mutex<Vec<String>>, count: usize) -> String {
    let logs = logs.lock().unwrap_or_else(|e| e.into_inner());
    let start = logs.len().saturating_sub(count);
    let mut text = String::new();
    for line in &logs[start..] {
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// Run `command` to completion, killing it after `limit`. Its output is
/// read while it runs, so a full pipe never blocks it.
fn output_within(mut command: Command, limit: Duration, what: &str) -> Output {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{what} did not finish within {limit:?}");
        }
        thread::sleep(Duration::from_millis(20));
    };
    Output {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    }
}

struct Reply {
    status: u16,
    body: Value,
}

fn http(port: u16, method: &str, path: &str, body: Option<&Value>) -> Option<Reply> {
    http_within(port, method, path, body, Duration::from_secs(60))
}

/// [`http`] that waits at most `limit` for each read.
fn http_within(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    limit: Duration,
) -> Option<Reply> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream.set_read_timeout(Some(limit)).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let body = body.map(Value::to_string).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
         Authorization: Bearer {WEB_TOKEN}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok()?;
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
    let status = head.split(' ').nth(1)?.parse().ok()?;
    let mut raw = response[split + 4..].to_vec();
    if head.contains("transfer-encoding: chunked") {
        raw = dechunk(&raw)?;
    }
    let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    Some(Reply { status, body })
}

fn dechunk(mut raw: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = raw.windows(2).position(|window| window == b"\r\n")?;
        let size =
            usize::from_str_radix(std::str::from_utf8(&raw[..line]).ok()?.trim(), 16).ok()?;
        raw = &raw[line + 2..];
        if size == 0 {
            return Some(body);
        }
        body.extend_from_slice(raw.get(..size)?);
        raw = raw.get(size + 2..)?;
    }
}

fn read_packet(stream: &mut TcpStream) -> std::io::Result<Packet> {
    let mut length = 0usize;
    for shift in (0..21).step_by(7) {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        length |= usize::from(byte[0] & 127) << shift;
        if byte[0] & 128 == 0 {
            if length > MAX_FRAME_SIZE {
                return Err(std::io::Error::other("oversized packet"));
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body)?;
            return Packet::from_body(&body);
        }
    }
    Err(std::io::Error::other("oversized packet length"))
}

fn send(stream: &mut TcpStream, packet: &Packet) -> std::io::Result<()> {
    stream.write_all(&Codec::default().encode(packet)?)
}

/// Every backend connection: a login for whatever version the proxy speaks
/// (1.8 or 1.20.5 here), then nothing but reading until the proxy closes.
/// Like a vanilla server without `accepts-transfers`, it refuses the
/// transfer intent, which the proxy must never pass on.
fn serve_backends(listener: TcpListener) {
    for socket in listener.incoming() {
        let Ok(socket) = socket else { continue };
        thread::spawn(move || {
            let _ = serve_backend(socket);
        });
    }
}

fn serve_backend(mut socket: TcpStream) -> std::io::Result<()> {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let handshake = Handshake::decode(&read_packet(&mut socket)?)?;
    if handshake.next_state != NextState::Login {
        return Ok(());
    }
    let start = read_packet(&mut socket)?;
    let mut fields = start.data.as_slice();
    let name = read_string(&mut fields, 16)?.to_owned();
    let mut success = Vec::new();
    if handshake.protocol < 764 {
        let identity = NEXT.fetch_add(1, Ordering::Relaxed);
        write_string(
            &format!("00000000-0000-0000-0000-{identity:012x}"),
            &mut success,
        );
        write_string(&name, &mut success);
        send(&mut socket, &Packet::new(2, success))?;
    } else {
        success.extend_from_slice(fields.get(..16).unwrap_or(&[7; 16]));
        write_string(&name, &mut success);
        write_varint(0, &mut success); // no profile properties
        success.push(0); // strict error handling off (1.20.5-1.21.1)
        send(&mut socket, &Packet::new(2, success))?;
        // Login Acknowledged, then Finish Configuration both ways.
        while read_packet(&mut socket)?.id != 3 {}
        send(&mut socket, &Packet::empty(3))?;
        while read_packet(&mut socket)?.id != 3 {}
    }
    loop {
        read_packet(&mut socket)?;
    }
}

enum Login {
    /// In play, on the proxy.
    Accepted(TcpStream),
    /// The login Disconnect's reason.
    Refused(String),
    Failed(String),
}

impl Login {
    fn describe(&self) -> String {
        match self {
            Self::Accepted(_) => "accepted".to_owned(),
            Self::Refused(reason) => format!("refused: {reason}"),
            Self::Failed(error) => format!("failed: {error}"),
        }
    }
}

/// An offline login of `name` with `protocol` (47 or 766) through the proxy
/// at `port`, with `intent` (login or transfer), into play.
fn login(port: u16, name: &str, protocol: i32, intent: NextState) -> Login {
    let attempt = || -> std::io::Result<Login> {
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
        socket.set_read_timeout(Some(Duration::from_secs(60)))?;
        socket.set_write_timeout(Some(Duration::from_secs(10)))?;
        let handshake = Handshake {
            protocol,
            address: "127.0.0.1".into(),
            port,
            next_state: intent,
        };
        send(&mut socket, &handshake.packet())?;
        let mut start = Vec::new();
        write_string(name, &mut start);
        if protocol >= 764 {
            let mut uuid = [0u8; 16];
            for (index, byte) in name.bytes().enumerate() {
                uuid[index % 16] ^= byte;
            }
            start.extend_from_slice(&uuid);
        }
        send(&mut socket, &Packet::new(0, start))?;
        let reply = read_packet(&mut socket)?;
        match reply.id {
            2 => {}
            0 => {
                let reason = read_string(&mut reply.data.as_slice(), 262_144)?.to_owned();
                return Ok(Login::Refused(reason));
            }
            id => return Ok(Login::Failed(format!("unexpected login packet {id:#x}"))),
        }
        if protocol >= 764 {
            // Login Acknowledged; the backend finishes configuration.
            send(&mut socket, &Packet::empty(3))?;
            while read_packet(&mut socket)?.id != 3 {}
            send(&mut socket, &Packet::empty(3))?;
        }
        Ok(Login::Accepted(socket))
    };
    attempt().unwrap_or_else(|error| Login::Failed(error.to_string()))
}

/// Log in until the proxy accepts: a peer refuses logins while it catches
/// up, and a name stays claimed until its last session's claim is released.
fn login_accepted(
    fixture: &Fixture,
    port: u16,
    name: &str,
    protocol: i32,
    intent: NextState,
) -> TcpStream {
    fixture.wait_until(&format!("{name} to log in on port {port}"), || match login(
        port, name, protocol, intent,
    ) {
        Login::Accepted(socket) => Some(socket),
        Login::Refused(_) | Login::Failed(_) => None,
    })
}

/// The next packet with one of `ids`, skipping everything else.
fn next_packet(socket: &mut TcpStream, ids: &[i32], what: &str) -> Packet {
    loop {
        let packet = read_packet(socket).unwrap_or_else(|error| panic!("{what}: {error}"));
        if ids.contains(&packet.id) {
            return packet;
        }
    }
}

/// Every running peer's status, once they agree on one leader that exactly
/// one of them reports to be.
fn agreed_leader(fixture: &Fixture, names: &[&str], not: Option<&str>) -> String {
    fixture.wait_until("one leader", || {
        let statuses: Vec<Value> = names
            .iter()
            .map(|name| fixture.status(name))
            .collect::<Option<_>>()?;
        let leader = statuses[0]["leader"].as_str()?.to_owned();
        let agreed = statuses
            .iter()
            .all(|status| status["leader"].as_str() == Some(leader.as_str()));
        let leading = statuses
            .iter()
            .filter(|status| status["is_leader"] == true)
            .count();
        (agreed && leading == 1 && not != Some(leader.as_str())).then_some(leader)
    })
}

#[test]
fn three_peers_replicate_fail_over_restore_and_drain() {
    let mut fixture = Fixture::new();
    let members = ["a", "b", "c"];

    // A one-voter cluster from `a`'s data, then `b` and `c` join and vote.
    let created = fixture.run("a", &["cluster", "init", "--peer", "a"]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    fixture.start("a");
    fixture.wait_joined("a");
    for name in ["b", "c"] {
        // A new peer waits, without listeners, until a member adds it.
        fixture.start(name);
        let address = format!("127.0.0.1:{}", fixture.peer(name).cluster);
        fixture.wait_until(&format!("{name} to be added"), || {
            match fixture.admin("a", &["cluster", "add", name, &address]) {
                Ok(_) => Some(()),
                Err(error) if error.contains("already a member") => Some(()),
                Err(_) => None,
            }
        });
        fixture.wait_joined(name);
        fixture.wait_until(&format!("{name} to be promoted"), || {
            match fixture.admin("a", &["cluster", "promote", name]) {
                Ok(_) => Some(()),
                Err(error) if error.contains("already votes") => Some(()),
                Err(_) => None,
            }
        });
    }

    // Every peer sees three voters that are up, one cluster and one leader.
    let mut cluster_ids = BTreeSet::new();
    for name in members {
        let status = fixture.wait_until(&format!("{name} to see three voters"), || {
            fixture.status(name).filter(|status| {
                let peers = status["peers"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                peers.len() == 3
                    && peers
                        .iter()
                        .all(|peer| peer["role"] == "voter" && peer["up"] == true)
            })
        });
        assert_eq!(status["peer"], name);
        cluster_ids.insert(status["cluster_id"].as_str().unwrap().to_owned());
    }
    assert_eq!(cluster_ids.len(), 1, "{cluster_ids:?}");
    agreed_leader(&fixture, &members, None);

    // A permission change through one peer's HTTP API reaches the others.
    fixture.create_group("b", "builders");
    for name in members {
        fixture.wait_for_group(name, "builders");
    }

    // A configuration deployment through one peer reaches every rift.lua.
    let revision = fixture
        .http("c", "GET", "/api/config", None)
        .filter(|reply| reply.status == 200)
        .expect("GET /api/config on c")
        .body["revision"]
        .clone();
    let deployed = fixture.source_with_server("deployed through c");
    let saved = fixture
        .http(
            "c",
            "PUT",
            "/api/config",
            Some(&json!({ "source": deployed, "revision": revision })),
        )
        .expect("PUT /api/config on c");
    assert!(
        (200..300).contains(&saved.status),
        "{}: {}",
        saved.status,
        saved.body
    );
    for name in members {
        let file = fixture.peer(name).directory.join("rift.lua");
        fixture.wait_until(&format!("the deployment in {name}'s rift.lua"), || {
            fs::read_to_string(&file)
                .ok()
                .filter(|source| source.contains("-- deployed through c"))
        });
    }

    // Two clients log in with one name on two peers at the same moment:
    // exactly one is accepted.
    let (first, second) = (fixture.peer("a").game, fixture.peer("b").game);
    let mut decided = false;
    let mut seen = Vec::new();
    for attempt in 0..30 {
        let name = format!("Twin{attempt}");
        let barrier = Arc::new(Barrier::new(2));
        let clients: Vec<_> = [first, second]
            .into_iter()
            .map(|port| {
                let barrier = barrier.clone();
                let name = name.clone();
                thread::spawn(move || {
                    barrier.wait();
                    login(port, &name, 47, NextState::Login)
                })
            })
            .collect();
        let outcomes: Vec<Login> = clients
            .into_iter()
            .map(|client| client.join().unwrap())
            .collect();
        let accepted = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Login::Accepted(_)))
            .count();
        let described: Vec<String> = outcomes.iter().map(Login::describe).collect();
        assert!(
            accepted <= 1,
            "two peers accepted {name}: {described:?}; earlier attempts: {seen:?}"
        );
        seen.push(described);
        let duplicate = outcomes.iter().any(|outcome| {
            matches!(outcome, Login::Refused(reason) if reason.contains("already connected"))
        });
        if accepted == 1 && duplicate {
            decided = true;
            break;
        }
        drop(outcomes);
        thread::sleep(Duration::from_millis(500));
    }
    assert!(
        decided,
        "no attempt ended with one login accepted and one refused as a duplicate: {seen:?}; logs:{}",
        fixture.logs()
    );

    // Kill the leader without warning: the others elect a new one in time,
    // keep every acknowledged write and accept new ones.
    let killed = agreed_leader(&fixture, &members, None);
    let survivors: Vec<&str> = members
        .iter()
        .copied()
        .filter(|name| *name != killed)
        .collect();
    let killed_at = Instant::now();
    fixture.kill(&killed);
    let leader = agreed_leader(&fixture, &survivors, Some(killed.as_str()));
    assert!(
        killed_at.elapsed() < ELECTION_BOUND,
        "a new leader took {:?}",
        killed_at.elapsed()
    );
    for name in &survivors {
        fixture.wait_for_group(name, "builders");
    }
    let follower = survivors
        .iter()
        .copied()
        .find(|name| *name != leader)
        .unwrap();
    fixture.create_group(follower, "after-failover");
    for name in &survivors {
        fixture.wait_for_group(name, "after-failover");
    }
    assert_eq!(
        agreed_leader(&fixture, &survivors, Some(killed.as_str())),
        leader
    );

    // The killed peer comes back and catches up.
    fixture.start(&killed);
    fixture.wait_joined(&killed);
    fixture.wait_for_group(&killed, "after-failover");
    let leader = agreed_leader(&fixture, &members, None);

    // A backup of the cluster starts a new cluster that holds its data.
    let backup = fixture.root.join("cluster-backup.sqlite3");
    let backed_up = fixture
        .admin(&leader, &["cluster", "backup", backup.to_str().unwrap()])
        .unwrap_or_else(|error| panic!("cluster backup: {error}"));
    assert!(backed_up["applied_index"].as_u64().is_some(), "{backed_up}");
    assert!(backup.is_file());
    // Init reads the first configuration, which places a server on peer
    // `a`'s node, as peer `z` would.
    fs::write(
        fixture.peer("z").directory.join("rift.lua"),
        fixture.first_source(),
    )
    .unwrap();
    let restored = fixture.run(
        "z",
        &[
            "cluster",
            "init",
            "--peer",
            "z",
            "--from-backup",
            backup.to_str().unwrap(),
        ],
    );
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    // The backup holds the deployment history: the new peer runs the last
    // deployed revision.
    fs::copy(
        fixture.peer("a").directory.join("rift.lua"),
        fixture.peer("z").directory.join("rift.lua"),
    )
    .unwrap();
    fixture.start("z");
    let status = fixture.wait_joined("z");
    assert_ne!(
        status["cluster_id"].as_str(),
        cluster_ids.iter().next().map(String::as_str)
    );
    assert_eq!(status["leader"], "z");
    assert_eq!(status["peers"].as_array().map(Vec::len), Some(1));
    fixture.wait_for_group("z", "builders");
    fixture.wait_for_group("z", "after-failover");
    fixture.kill("z");

    // `rift admin cluster leave` drains a follower: its 1.20.5 player is
    // sent to another peer, its 1.8 player gets a reconnect message, and
    // the peer exits.
    let drained = members
        .iter()
        .copied()
        .find(|name| *name != leader)
        .unwrap();
    let port = fixture.peer(drained).game;
    let mut modern = login_accepted(&fixture, port, "Modern", 766, NextState::Login);
    let mut legacy = login_accepted(&fixture, port, "Legacy", 47, NextState::Login);
    // A client has its Login Success before the peer lists the player (the
    // name's claim commits in between), and leaving hands off only the
    // players it lists.
    fixture.wait_until(&format!("{drained} to list both players"), || {
        let reply = fixture
            .http(drained, "GET", "/api/players", None)
            .filter(|reply| reply.status == 200)?;
        let players = reply.body["players"].as_array()?;
        ["Modern", "Legacy"]
            .iter()
            .all(|name| players.iter().any(|player| player["name"] == *name))
            .then_some(())
    });
    let left = fixture
        .admin(&leader, &["cluster", "leave", drained])
        .unwrap_or_else(|error| panic!("cluster leave: {error}"));
    assert_eq!(left["leaving"], drained, "{left}");
    let transfer = next_packet(
        &mut modern,
        &[TRANSFER_766, DISCONNECT_766],
        "the 1.20.5 player's transfer",
    );
    assert_eq!(transfer.id, TRANSFER_766, "{transfer:?}");
    let mut fields = transfer.data.as_slice();
    let host = read_string(&mut fields, 32767).unwrap().to_owned();
    let target = u16::try_from(read_varint(&mut fields).unwrap()).unwrap();
    assert!(fields.is_empty());
    assert_eq!(host, "127.0.0.1");
    let others: BTreeSet<u16> = members
        .iter()
        .filter(|name| **name != drained)
        .map(|name| fixture.peer(name).game)
        .collect();
    assert!(others.contains(&target), "{target} is not in {others:?}");
    let disconnect = next_packet(&mut legacy, &[DISCONNECT_47], "the 1.8 player's disconnect");
    let reason = read_string(&mut disconnect.data.as_slice(), 262_144)
        .unwrap()
        .to_owned();
    assert!(reason.contains("restarting"), "{reason}");
    fixture.wait_until(&format!("{drained} to exit"), || {
        let peer = fixture.peers.iter().find(|peer| peer.name == drained)?;
        let process = peer.process.as_ref()?;
        let logs = process.logs.lock().unwrap();
        logs.iter()
            .any(|line| line.contains("shutdown complete"))
            .then_some(())
    });
    assert!(
        fixture
            .peer(drained)
            .process
            .as_ref()
            .unwrap()
            .logs
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("cluster: leaving: transferred 1 players")),
        "logs:{}",
        fixture.logs()
    );
    // The transferred player arrives with the transfer intent, which the
    // target accepts and never passes on to its backend.
    drop(modern);
    let arrived = login_accepted(&fixture, target, "Modern", 766, NextState::Transfer);
    drop(arrived);
    fixture.kill(drained);
}
