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
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(10))
            .expect("startup log");
        line.strip_prefix("rift: ")
            .unwrap()
            .split(" -> ")
            .next()
            .unwrap()
            .parse()
            .expect(&line)
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
