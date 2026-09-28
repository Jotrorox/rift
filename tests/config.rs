//! Exercise config selection, validation, routing and limits through the executable.
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "rift-config-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn write(&self, source: &str) {
        fs::write(self.0.join("rift.lua"), source).unwrap();
    }

    fn spawn(&self, args: &[&str]) -> Process {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rift"))
            .current_dir(&self.0)
            .args(args)
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
        Process { child, lines }
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
}

impl Process {
    fn listener(&self) -> SocketAddr {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!("missing listener startup log ({error}); stderr: {output:?}")
                });
            // A previously started listener may log connection diagnostics
            // before the next listener's readiness message reaches stderr.
            if let Some(address) = line.strip_prefix("rift: listening on ") {
                return address.split(" -> ").next().unwrap().parse().expect(&line);
            }
            output.push(line);
        }
    }

    fn failure(&mut self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "rift did not exit");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(!status.success());
        let output = self.lines.iter().collect::<Vec<_>>().join("\n");
        assert!(
            output.contains(expected),
            "expected {expected:?}, got {output:?}"
        );
        assert!(!output.contains("panicked"), "{output}");
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn config(backends: &[SocketAddr], limits: &str) -> String {
    let listeners = backends
        .iter()
        .enumerate()
        .map(|(i, _)| format!("l{i} = '127.0.0.1:0'"))
        .collect::<Vec<_>>()
        .join(",");
    let routes = backends
        .iter()
        .enumerate()
        .map(|(i, _)| format!("l{i} = 'b{i}'"))
        .collect::<Vec<_>>()
        .join(",");
    let backends = backends
        .iter()
        .enumerate()
        .map(|(i, addr)| format!("b{i} = '{addr}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "return {{ listeners = {{ {listeners} }}, backends = {{ {backends} }}, routes = {{ {routes} }}, limits = {{ {limits} }} }}"
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
                // Accepted sockets may inherit the listener's nonblocking mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                return stream;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "backend connection timed out");
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("{e}"),
        }
    }
}

#[test]
fn auto_loaded_and_explicit_configs_route_to_named_backends() {
    for args in [vec![], vec!["--config", "custom.lua"]] {
        let fixture = Fixture::new();
        let backends: Vec<_> = (0..2)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        fixture.write(&config(
            &backends
                .iter()
                .map(|b| b.local_addr().unwrap())
                .collect::<Vec<_>>(),
            "buffer_size = 3, connect_timeout_ms = 1000",
        ));
        if !args.is_empty() {
            fs::rename(fixture.0.join("rift.lua"), fixture.0.join("custom.lua")).unwrap();
            fixture.write("error('explicit path must take precedence')");
        }
        let process = fixture.spawn(&args);
        for (i, backend) in backends.iter().enumerate() {
            let mut client = connect(process.listener());
            client.write_all(&[i as u8; 17]).unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            let mut server = accept(backend);
            let mut bytes = Vec::new();
            server.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, vec![i as u8; 17]);
            server.write_all(b"response").unwrap();
            server.shutdown(std::net::Shutdown::Write).unwrap();
            bytes.clear();
            client.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"response");
        }
    }
}

#[test]
fn connection_limit_is_shared_across_listeners_and_released() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&config(
        &[backend.local_addr().unwrap(); 2],
        "max_connections = 1",
    ));
    let process = fixture.spawn(&[]);
    let first = process.listener();
    let second = process.listener();
    let client = connect(first);
    let server = accept(&backend);
    // Backend acceptance proves the first connection has acquired its permit.
    let mut excess = connect(second);
    match excess.read(&mut [0]) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("overload was not rejected: {result:?}"),
    }
    drop(client);
    drop(server);
    let responder = thread::spawn(move || {
        let mut server = accept(&backend);
        server.write_all(b"ready").unwrap();
        let mut bytes = [0; 5];
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"probe");
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut client = connect(second);
        let mut bytes = [0; 5];
        match client.read_exact(&mut bytes) {
            Ok(()) => {
                assert_eq!(&bytes, b"ready");
                client.write_all(b"probe").unwrap();
                break;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
                ) => {}
            Err(error) => panic!("{error}"),
        }
        assert!(
            Instant::now() < deadline,
            "connection permit was not released"
        );
        thread::sleep(Duration::from_millis(10));
    }
    responder.join().unwrap();
}

#[test]
fn invalid_configs_exit_with_source_and_useful_errors() {
    let fixture = Fixture::new();
    for (source, expected) in [
        ("return {", "syntax error"),
        ("error('bad setting')", "bad setting"),
        ("return { listeners = {} }", "listeners: must not be empty"),
        ("return { listners = {} }", "unknown field"),
    ] {
        fixture.write(source);
        let mut process = fixture.spawn(&[]);
        process.failure(expected);
    }
    fixture
        .spawn(&["--config", "missing.lua"])
        .failure("missing.lua");
    fixture.write("return {}");
    fixture
        .spawn(&["--config", "rift.lua"])
        .failure("rift.lua: listeners");
}

#[test]
fn explicit_cli_addresses_bypass_config() {
    let fixture = Fixture::new();
    fixture.write("error('must not be loaded')");
    let process = fixture.spawn(&["127.0.0.1:0", "127.0.0.1:25566"]);
    assert!(process.listener().ip().is_loopback());
}

#[test]
fn failure_to_bind_any_listener_aborts_startup() {
    let fixture = Fixture::new();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&format!("return {{ listeners = {{ a = '127.0.0.1:0', b = '{}' }}, backends = {{ target = '127.0.0.1:0' }}, routes = {{ a = 'target', b = 'target' }} }}", occupied.local_addr().unwrap()));
    fixture.spawn(&[]).failure("listeners.b");
}

#[test]
fn lua_hostname_routes_reach_two_backends_on_one_listener() {
    for hook in [
        "",
        "on_route = function(connection) assert(connection.default_backend == 'survival'); return nil end,",
    ] {
        let fixture = Fixture::new();
        let backends: Vec<_> = (0..2)
            .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
            .collect();
        fixture.write(&format!("return {{{hook}
        listeners = {{ public = '127.0.0.1:0' }},
        backends = {{ survival = '{}', creative = 'localhost:{}' }},
        routes = {{ public = {{ ['survival.example.test'] = 'survival', ['creative.example.test'] = 'creative', ['*.games.example.test'] = 'creative', ['*'] = 'survival' }} }},
        limits = {{ buffer_size = 3 }}
    }}", backends[0].local_addr().unwrap(), backends[1].local_addr().unwrap().port()));
        let process = fixture.spawn(&[]);
        let address = process.listener();
        for (host, index) in [
            ("survival.example.test", 0),
            ("creative.example.test", 1),
            ("pvp.games.example.test", 1),
            ("unmatched.test", 0),
        ] {
            let mut client = connect(address);
            let mut body = vec![0, 0x86, 0x06, host.len() as u8];
            body.extend(host.as_bytes());
            body.extend([0x63, 0xdd, 1]);
            let mut request = vec![body.len() as u8];
            request.extend(body);
            request.extend([1, 0]); // Pipelined Minecraft status request.
            client.write_all(&request).unwrap();
            client.shutdown(std::net::Shutdown::Write).unwrap();
            let mut server = accept(&backends[index]);
            let mut received = Vec::new();
            server.read_to_end(&mut received).unwrap();
            assert_eq!(received, request);
            server.write_all(&[index as u8]).unwrap();
            server.shutdown(std::net::Shutdown::Write).unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).unwrap();
            assert_eq!(response, [index as u8]);
        }
    }
}

fn assert_closed(client: &mut TcpStream) {
    match client.read(&mut [0]) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("connection was not closed: {result:?}"),
    }
}

fn assert_no_connection(backend: &TcpListener) {
    backend.set_nonblocking(true).unwrap();
    assert_eq!(
        backend.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn lua_selects_a_backend_or_rejects_before_connecting_upstream() {
    for hostname_routes in [false, true] {
        let fixture = Fixture::new();
        let default = TcpListener::bind("127.0.0.1:0").unwrap();
        let selected = TcpListener::bind("127.0.0.1:0").unwrap();
        let source = config(
            &[
                default.local_addr().unwrap(),
                selected.local_addr().unwrap(),
            ],
            "",
        )
        .replace("b1 = '127.0.0.1:", "b1 = 'localhost:");
        let source = if hostname_routes {
            source
                .replace("l0 = 'b0'", "l0 = { ['example.test'] = 'b0' }")
                .replace("l1 = 'b1'", "l1 = { ['example.test'] = 'b1' }")
        } else {
            source
        };
        let source = source.replacen(
            "return {",
            "return { on_route = function(connection)
        assert(connection.peer_ip == '127.0.0.1')
        assert(connection.peer_port > 0 and connection.local_port > 0)
        if connection.listener == 'l0' then
            assert(connection.default_backend == 'b0')
            return { backend = 'b1' }
        end
        return { reject = true, reason = 'closed' }
    end,",
            1,
        );
        fixture.write(&if hostname_routes {
            source.replace(
                "assert(connection.default_backend == 'b0')",
                "assert(connection.default_backend == nil)",
            )
        } else {
            source
        });
        let process = fixture.spawn(&[]);
        let first = process.listener();
        let second = process.listener();
        // The source snapshot remains usable after the file changes.
        fixture.write("error('must not reread the configuration')");
        let mut client = connect(first);
        let mut server = accept(&selected);
        client.write_all(b"routed").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut bytes = Vec::new();
        server.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"routed");
        server.write_all(b"reply").unwrap();
        server.shutdown(std::net::Shutdown::Write).unwrap();
        bytes.clear();
        client.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"reply");
        assert_closed(&mut connect(second));
        assert_no_connection(&default);
        assert_no_connection(&selected);
    }
}

#[test]
fn hook_failures_close_only_the_affected_connection_and_release_its_permit() {
    for failing_hook in [
        "error('broken hook')",
        "return { backend = 'missing' }",
        "return { backend = false }",
        "while true do end",
        "return { reject = true }",
    ] {
        let fixture = Fixture::new();
        let backend = TcpListener::bind("127.0.0.1:0").unwrap();
        let source = config(&[backend.local_addr().unwrap(); 2], "max_connections = 1");
        fixture.write(&source.replacen(
            "return {",
            &format!(
                "return {{ on_route = function(connection)
            if connection.listener == 'l0' then {failing_hook} end
            return nil
        end,"
            ),
            1,
        ));
        let process = fixture.spawn(&[]);
        let failing = process.listener();
        let healthy = process.listener();
        for _ in 0..6 {
            assert_closed(&mut connect(failing));
        }
        assert_no_connection(&backend);
        let mut client = connect(healthy);
        let mut server = accept(&backend);
        server.write_all(b"healthy").unwrap();
        let mut response = [0; 7];
        client.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"healthy");
    }
}

#[path = "support/operations.rs"]
mod operations;
