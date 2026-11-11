//! Log a bot in through a local stand-in for Mojang's session server. Only a
//! build with the `test-session-server` feature reads `RIFT_TEST_SESSION_URL`.
#![cfg(feature = "test-session-server")]
use rift::{
    auth::CryptoStream,
    protocol::{
        Codec, Handshake, NextState, Packet, Reader, read_string, read_varint, write_string,
        write_varint,
    },
};
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

const VARIABLE: &str = "RIFT_TEST_SESSION_URL";
const VERIFIED_ID: &str = "07070707070707070707070707070707";
const WARNING: &str = "rift: warning: RIFT_TEST_SESSION_URL is set";

struct Process {
    child: Child,
    lines: mpsc::Receiver<String>,
    _directory: Directory,
}

struct Directory(PathBuf);

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An online-mode proxy in front of `backend`. BungeeCord forwarding puts the
/// verified identity in the backend handshake, where the test can read it.
fn proxy(backend: SocketAddr, session_url: Option<&str>) -> Process {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "rift-session-server-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    let directory = Directory(fs::canonicalize(path).unwrap());
    fs::write(
        directory.0.join("rift.lua"),
        format!(
            "return {{
            listeners = {{ public = '127.0.0.1:0' }},
            backends = {{ paper = '{backend}' }}, routes = {{ public = 'paper' }},
            authentication = {{ online_mode = true, timeout_ms = 5000 }},
            forwarding = {{ mode = 'bungeecord' }},
        }}"
        ),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_rift"));
    command.env_remove(VARIABLE);
    if let Some(url) = session_url {
        command.env(VARIABLE, url);
    }
    let mut child = command
        .current_dir(&directory.0)
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
    Process {
        child,
        lines,
        _directory: directory,
    }
}

impl Process {
    /// The listener address and every line logged before it.
    fn listener(&self) -> (SocketAddr, Vec<String>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| {
                    panic!("missing listener startup log ({error}); stderr: {output:?}")
                });
            if let Some(address) = line.strip_prefix("rift: listening on ") {
                let address = address.split(" -> ").next().unwrap().parse().expect(&line);
                return (address, output);
            }
            output.push(line);
        }
    }

    fn failure(mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "rift did not exit");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(!status.success());
        self.lines.iter().collect::<Vec<_>>().join("\n")
    }
}

fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                let timeout = Some(Duration::from_secs(10));
                stream.set_read_timeout(timeout).unwrap();
                stream.set_write_timeout(timeout).unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "no connection arrived");
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

/// Answers one `hasJoined` request with `status` and returns the request head.
fn session_server(listener: TcpListener, status: u16, body: String) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut socket = accept(&listener);
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8192);
        }
        let response = format!(
            "HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).unwrap();
        String::from_utf8(request).unwrap()
    })
}

fn read_packet(stream: &mut TcpStream) -> Packet {
    let mut length = 0usize;
    for shift in (0..21).step_by(7) {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        length |= usize::from(byte[0] & 127) << shift;
        if byte[0] & 128 == 0 {
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            return Packet::from_body(&body).unwrap();
        }
    }
    panic!("oversized fixture packet");
}

/// A 1.8 backend with BungeeCord forwarding: it takes the identity from the
/// handshake hostname, completes the login with it and returns the hostname.
fn backend(listener: TcpListener) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut socket = accept(&listener);
        let handshake = read_packet(&mut socket);
        assert_eq!(handshake.id, 0);
        let mut data = handshake.data.as_slice();
        read_varint(&mut data).unwrap();
        let hostname = read_string(&mut data, 32767).unwrap().to_owned();
        let login = read_packet(&mut socket);
        assert_eq!(login.id, 0);
        let name = read_string(&mut login.data.as_slice(), 16)
            .unwrap()
            .to_owned();
        let id = hostname.split('\0').nth(2).expect(&hostname);
        let mut success = Vec::new();
        write_string(
            &format!(
                "{}-{}-{}-{}-{}",
                &id[..8],
                &id[8..12],
                &id[12..16],
                &id[16..20],
                &id[20..]
            ),
            &mut success,
        );
        write_string(&name, &mut success);
        socket
            .write_all(&Codec::default().encode(&Packet::new(2, success)).unwrap())
            .unwrap();
        // Hold the connection until the bot has read its Login Success.
        let _ = socket.read(&mut [0]);
        hostname
    })
}

/// A 1.8 client's login up to the first packet after encryption starts.
async fn login(address: SocketAddr, name: &str) -> Packet {
    use aws_lc_rs::rsa::{Pkcs1PublicEncryptingKey, PublicEncryptingKey};
    let mut stream = CryptoStream::new(tokio::net::TcpStream::connect(address).await.unwrap());
    let mut codec = Codec::default();
    let mut reader = Reader::default();
    let handshake = Handshake {
        protocol: 47,
        address: "localhost".into(),
        port: address.port(),
        next_state: NextState::Login,
    };
    codec.write(&mut stream, &handshake.packet()).await.unwrap();
    let mut start = Vec::new();
    write_string(name, &mut start);
    codec
        .write(&mut stream, &Packet::new(0, start))
        .await
        .unwrap();
    let request = reader.read(&mut stream, codec).await.unwrap().unwrap();
    assert_eq!(request.id, 1, "{request:?}");
    let mut data = request.data.as_slice();
    assert_eq!(read_string(&mut data, 20).unwrap(), "");
    let size = read_varint(&mut data).unwrap() as usize;
    let public =
        Pkcs1PublicEncryptingKey::new(PublicEncryptingKey::from_der(&data[..size]).unwrap())
            .unwrap();
    data = &data[size..];
    let size = read_varint(&mut data).unwrap() as usize;
    let secret = [0x42; 16];
    let mut response = Vec::new();
    for plaintext in [&secret[..], &data[..size]] {
        let mut ciphertext = vec![0; public.ciphertext_size()];
        let ciphertext = public.encrypt(plaintext, &mut ciphertext).unwrap();
        write_varint(ciphertext.len() as i32, &mut response);
        response.extend_from_slice(ciphertext);
    }
    codec
        .write(&mut stream, &Packet::new(1, response))
        .await
        .unwrap();
    stream.enable_encryption(secret).unwrap();
    loop {
        let packet = reader.read(&mut stream, codec).await.unwrap().unwrap();
        if packet.id != 3 {
            return packet;
        }
        codec.set_compression(read_varint(&mut packet.data.as_slice()).unwrap());
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(20), future)
                .await
                .expect("the login did not finish")
        })
}

fn local_listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    (listener, address)
}

#[test]
fn bot_logs_in_through_a_local_session_server() {
    let (sessions, session_address) = local_listener();
    let (paper, paper_address) = local_listener();
    let url = format!("http://{session_address}/session/minecraft/hasJoined");
    let body = serde_json::json!({
        "id": VERIFIED_ID, "name": "Player",
        "properties": [{"name": "textures", "value": "fixture-skin", "signature": "fixture-signature"}],
    })
    .to_string();
    let sessions = session_server(sessions, 200, body);
    let paper = backend(paper);
    let process = proxy(paper_address, Some(&url));
    let (listener, startup) = process.listener();
    assert!(
        startup
            .iter()
            .any(|line| line.starts_with(WARNING) && line.contains(&url)),
        "{startup:?}"
    );

    let success = block_on(login(listener, "Player"));
    assert_eq!(success.id, 2, "{success:?}");
    let mut data = success.data.as_slice();
    assert_eq!(
        read_string(&mut data, 36).unwrap().replace('-', ""),
        VERIFIED_ID
    );
    assert_eq!(read_string(&mut data, 16).unwrap(), "Player");

    let request = sessions.join().unwrap();
    assert!(
        request.starts_with("GET /session/minecraft/hasJoined?username=Player&serverId="),
        "{request}"
    );
    // The backend receives the session server's identity, not a client claim.
    let hostname = paper.join().unwrap();
    let fields: Vec<_> = hostname.split('\0').collect();
    assert_eq!(fields[..3], ["localhost", "127.0.0.1", VERIFIED_ID]);
    assert!(fields[3].contains("fixture-skin"), "{hostname:?}");
}

#[test]
fn a_session_the_local_server_rejects_never_reaches_a_backend() {
    let (sessions, session_address) = local_listener();
    let (paper, paper_address) = local_listener();
    paper.set_nonblocking(true).unwrap();
    let url = format!("http://{session_address}/session/minecraft/hasJoined");
    let sessions = session_server(sessions, 204, String::new());
    let process = proxy(paper_address, Some(&url));
    let (listener, _) = process.listener();

    let disconnect = block_on(login(listener, "Player"));
    assert_eq!(disconnect.id, 0, "{disconnect:?}");
    sessions.join().unwrap();
    assert_eq!(
        paper.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn an_override_outside_loopback_fails_startup() {
    let (_paper, paper_address) = local_listener();
    for url in [
        "not a url",
        "http://example.com/session/minecraft/hasJoined",
        "https://sessionserver.mojang.com/session/minecraft/hasJoined",
        "http://localhost:8080/session/minecraft/hasJoined",
        "http://192.168.1.10:8080/session/minecraft/hasJoined",
        "http://127.0.0.1.example.com/session/minecraft/hasJoined",
        "http://user@127.0.0.1:8080/session/minecraft/hasJoined",
        "ftp://127.0.0.1/session/minecraft/hasJoined",
        "file:///session/minecraft/hasJoined",
    ] {
        let output = proxy(paper_address, Some(url)).failure();
        assert!(
            output.contains("RIFT_TEST_SESSION_URL: expected an http:// or https:// URL on a loopback IP address"),
            "{url:?}: {output:?}"
        );
        assert!(!output.contains(WARNING), "{url:?}: {output:?}");
        assert!(!output.contains("panicked"), "{url:?}: {output:?}");
    }
}

#[test]
fn without_the_variable_startup_is_silent_about_it() {
    let (_paper, paper_address) = local_listener();
    let process = proxy(paper_address, None);
    let (_, startup) = process.listener();
    assert!(
        !startup.iter().any(|line| line.contains(VARIABLE)),
        "{startup:?}"
    );
}
