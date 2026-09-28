//! Exercise authenticated administration against a running proxy and real sessions.
use rift::protocol::{Codec, Handshake, NextState, Packet, read_string};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[path = "support/game.rs"]
mod game;
use game::{GameStream, accept_game, connect_game};

const SECRET: &str = "rift-integration-test-secret-32-bytes";
const PERMISSIONS: &str = "'status', 'maintenance', 'drain', 'transfer', 'reload', 'shutdown'";

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rift-admin-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn write(&self, source: &str) {
        fs::write(self.0.join("rift.lua"), source).unwrap();
    }

    fn spawn(&self) -> Process {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rift"))
            .current_dir(&self.0)
            .env("RIFT_ADMIN_TOKEN", SECRET)
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
        let mut front = None;
        let mut admin = None;
        let mut logs = Vec::new();
        while front.is_none() || admin.is_none() {
            let line = lines
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|error| panic!("missing startup message ({error}); {logs:?}"));
            if let Some(address) = line.strip_prefix("rift: listening on ") {
                front = Some(address.parse().unwrap());
            }
            if let Some(address) = line.strip_prefix("rift: admin on ") {
                admin = Some(address.parse().unwrap());
            }
            logs.push(line);
        }
        Process {
            child,
            lines,
            front: front.unwrap(),
            admin: admin.unwrap(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

struct Process {
    child: Child,
    lines: mpsc::Receiver<String>,
    front: SocketAddr,
    admin: SocketAddr,
}
impl Process {
    fn command(&self, args: &[&str]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rift"))
            .args(["admin", "--address", &self.admin.to_string()])
            .args(args)
            .env("RIFT_ADMIN_TOKEN", SECRET)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("admin command did not finish: {args:?}");
            }
            thread::sleep(Duration::from_millis(5));
        }
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn request(&self, request: Value) -> Value {
        let mut stream = connect(self.admin);
        writeln!(stream, "{request}").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn message(&self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut logs = Vec::new();
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("missing {expected:?} ({error}); {logs:?}"));
            if line.contains(expected) {
                return;
            }
            logs.push(line);
        }
    }

    fn exited(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "Rift did not drain and exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn config(backends: &[&TcpListener], options: &str, permissions: &str) -> String {
    let backends = backends
        .iter()
        .enumerate()
        .map(|(i, backend)| format!("b{i} = '{}'", backend.local_addr().unwrap()))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "return {{
        listeners = {{ public = '127.0.0.1:0' }},
        backends = {{ {backends} }}, routes = {{ public = 'b0' }},
        admin = {{ listen = '127.0.0.1:0', permissions = {{ {permissions} }} }},
        shutdown_timeout_ms = 3000,
        {options}
    }}"
    )
}

fn connect(address: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "no connection to backend");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    }
}
fn assert_no_connection(backend: &TcpListener) {
    backend.set_nonblocking(true).unwrap();
    assert_eq!(
        backend.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
fn exchange(client: &mut GameStream, server: &mut GameStream) {
    client.write_all(b"ping").unwrap();
    let mut bytes = [0; 4];
    server.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"ping");
    server.write_all(b"pong").unwrap();
    client.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"pong");
}
fn rejected_login(front: SocketAddr, reason: &str) {
    let mut client = connect(front);
    client.write_all(&game::setup()).unwrap();
    let packet = game::read_packet(&mut client).unwrap();
    assert_eq!(packet.id, 0);
    let message = read_string(&mut packet.data.as_slice(), 32767).unwrap();
    assert!(message.contains(reason), "{message}");
}
fn status(front: SocketAddr) -> Value {
    let mut client = connect(front);
    let codec = Codec::default();
    let handshake = Handshake {
        protocol: 47,
        address: "localhost".into(),
        port: front.port(),
        next_state: NextState::Status,
    };
    client
        .write_all(&codec.encode(&handshake.packet()).unwrap())
        .unwrap();
    client
        .write_all(&codec.encode(&Packet::new(0, vec![])).unwrap())
        .unwrap();
    let response = game::read_packet(&mut client).unwrap();
    assert_eq!(response.id, 0);
    let value =
        serde_json::from_str(read_string(&mut response.data.as_slice(), 32767).unwrap()).unwrap();
    let ping = Packet::new(1, 42i64.to_be_bytes().to_vec());
    client.write_all(&codec.encode(&ping).unwrap()).unwrap();
    assert_eq!(game::read_packet(&mut client).unwrap(), ping);
    value
}

#[test]
fn authentication_and_permissions_reject_actions_without_changing_state() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&config(&[&backend], "", "'status'"));
    let process = fixture.spawn();
    for request in [
        json!({"args":["maintenance", "on"]}),
        json!({"token":"wrong-token", "args":["shutdown"]}),
    ] {
        let response = process.request(request);
        assert_eq!(response["ok"], false);
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("authentication failed")
        );
    }
    for args in [
        vec!["maintenance", "on"],
        vec!["drain", "b0", "on"],
        vec!["reload"],
        vec!["shutdown"],
    ] {
        let output = process.command(&args);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("permission denied"), "{error}");
        assert!(!error.contains(SECRET));
    }
    let result = process.ok(&["status"]);
    assert_eq!(result["maintenance"], false);
    assert_eq!(result["backends"][0]["draining"], false);
    let mut client = connect_game(process.front);
    let mut server = accept_game(&backend);
    exchange(&mut client, &mut server);
}

#[test]
fn maintenance_preserves_players_exposes_status_and_can_be_disabled() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&config(&[&backend], "", PERMISSIONS));
    let process = fixture.spawn();
    let mut client = connect_game(process.front);
    let mut server = accept_game(&backend);
    exchange(&mut client, &mut server);
    assert_eq!(process.ok(&["maintenance", "on"])["maintenance"], true);
    rejected_login(process.front, "maintenance");
    let response = status(process.front);
    assert_eq!(response["players"]["online"], 1);
    assert!(
        response["description"]["text"]
            .as_str()
            .unwrap()
            .contains("maintenance")
    );
    assert_no_connection(&backend);
    exchange(&mut client, &mut server);
    assert_eq!(process.ok(&["maintenance", "off"])["maintenance"], false);
    let mut newcomer = connect_game(process.front);
    let mut upstream = accept_game(&backend);
    exchange(&mut newcomer, &mut upstream);
}

#[test]
fn draining_uses_fallback_and_status_identifies_live_players() {
    let fixture = Fixture::new();
    let primary = TcpListener::bind("127.0.0.1:0").unwrap();
    let fallback = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&config(
        &[&primary, &fallback],
        "fallbacks = { b0 = { 'b1' } },",
        PERMISSIONS,
    ));
    let process = fixture.spawn();
    let mut old_client = connect_game(process.front);
    let mut old_server = accept_game(&primary);
    exchange(&mut old_client, &mut old_server);
    assert_eq!(process.ok(&["drain", "b0", "on"])["draining"], true);
    let mut new_client = connect_game(process.front);
    let mut new_server = accept_game(&fallback);
    exchange(&mut new_client, &mut new_server);
    exchange(&mut old_client, &mut old_server);
    assert_no_connection(&primary);
    let result = process.ok(&["status"]);
    assert_eq!(result["players_online"], 2);
    let players = result["players"].as_array().unwrap();
    assert_eq!(players.len(), 2);
    assert!(
        players
            .iter()
            .all(|player| player["connection_id"].as_u64().unwrap() > 0)
    );
    assert_ne!(players[0]["connection_id"], players[1]["connection_id"]);
    assert!(players.iter().any(|player| player["backend"] == "b0"));
    assert!(players.iter().any(|player| player["backend"] == "b1"));
    assert!(
        result["backends"]
            .as_array()
            .unwrap()
            .iter()
            .any(|backend| backend["backend"] == "b0" && backend["draining"] == true)
    );
    process.ok(&["drain", "b0", "off"]);
    let mut restored = connect_game(process.front);
    let mut upstream = accept_game(&primary);
    exchange(&mut restored, &mut upstream);
}

#[test]
fn login_rate_limits_do_not_charge_or_block_server_list_status() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&config(
        &[&backend],
        "login_rate_limit = { per_ip_per_second = 1, per_ip_burst = 1 },",
        PERMISSIONS,
    ));
    let process = fixture.spawn();
    for _ in 0..3 {
        assert_eq!(status(process.front)["players"]["online"], 0);
    }
    let mut client = connect_game(process.front);
    let mut server = accept_game(&backend);
    exchange(&mut client, &mut server);
    rejected_login(process.front, "Too many login attempts");
    assert_eq!(status(process.front)["players"]["online"], 1);
    assert_no_connection(&backend);
    exchange(&mut client, &mut server);
}

#[test]
fn pending_pre_reload_handshake_cannot_refill_new_login_limits() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = config(
        &[&backend],
        "login_rate_limit = { per_ip_per_second = 1, per_ip_burst = 10 },",
        PERMISSIONS,
    );
    fixture.write(&source);
    let process = fixture.spawn();
    let mut pending = connect(process.front);
    // Wait for the proxy to pin the old snapshot before publishing new limits.
    let deadline = Instant::now() + Duration::from_secs(2);
    while process.ok(&["status"])["connections"] != 1 {
        assert!(
            Instant::now() < deadline,
            "pending connection was not accepted"
        );
        thread::sleep(Duration::from_millis(5));
    }
    fixture.write(&source.replace("per_ip_burst = 10", "per_ip_burst = 1"));
    process.ok(&["reload"]);
    let mut client = connect_game(process.front);
    let mut server = accept_game(&backend);
    exchange(&mut client, &mut server);
    // The old snapshot must use the newer shared policy without resetting its
    // consumed token bucket. New arrivals must observe that same bucket.
    pending.write_all(&game::setup()).unwrap();
    let rejection = game::read_packet(&mut pending).unwrap();
    assert_eq!(rejection.id, 0);
    assert!(
        read_string(&mut rejection.data.as_slice(), 32767)
            .unwrap()
            .contains("Too many login attempts")
    );
    rejected_login(process.front, "Too many login attempts");
    assert_no_connection(&backend);
    exchange(&mut client, &mut server);
}

#[test]
fn validated_admin_reload_changes_new_routes_and_retains_sessions_on_rejection() {
    let fixture = Fixture::new();
    let first = TcpListener::bind("127.0.0.1:0").unwrap();
    let second = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = config(&[&first, &second], "", PERMISSIONS);
    fixture.write(&source);
    let process = fixture.spawn();
    let mut old_client = connect_game(process.front);
    let mut old_server = accept_game(&first);
    exchange(&mut old_client, &mut old_server);
    let updated = source.replace("public = 'b0'", "public = 'b1'");
    fixture.write(&updated);
    let result = process.ok(&["reload"]);
    assert_eq!(result["reloaded"], true);
    assert_eq!(result["existing_sessions"], "preserved");
    let mut new_client = connect_game(process.front);
    let mut new_server = accept_game(&second);
    exchange(&mut new_client, &mut new_server);
    for invalid in [
        "return {".to_owned(),
        updated.replace("public = 'b1'", "public = 'missing'"),
        updated.replace("listen = '127.0.0.1:0'", "listen = '127.0.0.1:9091'"),
    ] {
        fixture.write(&invalid);
        let result = process.command(&["reload"]);
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(error.contains("previous configuration retained"), "{error}");
        exchange(&mut old_client, &mut old_server);
        exchange(&mut new_client, &mut new_server);
        let mut newcomer = connect_game(process.front);
        let mut upstream = accept_game(&second);
        exchange(&mut newcomer, &mut upstream);
        assert_no_connection(&first);
    }
}

#[test]
fn shutdown_acknowledges_then_drains_live_sessions() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&config(&[&backend], "", PERMISSIONS));
    let mut process = fixture.spawn();
    let mut client = connect_game(process.front);
    let mut server = accept_game(&backend);
    exchange(&mut client, &mut server);
    assert_eq!(process.ok(&["shutdown"])["shutdown"], "draining");
    process.message("draining 1 connections");
    assert!(process.child.try_wait().unwrap().is_none());
    assert!(TcpStream::connect_timeout(&process.front, Duration::from_millis(100)).is_err());
    let reload = process.command(&["reload"]);
    assert!(!reload.status.success());
    assert!(String::from_utf8_lossy(&reload.stderr).contains("shutting down"));
    exchange(&mut client, &mut server);
    drop(client);
    drop(server);
    process.exited();
}
