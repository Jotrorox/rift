//! Black-box Minecraft login routing through a real Rift process.
use std::{
    io::{BufRead, BufReader},
    net::SocketAddr,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

struct Proxy(Child, SocketAddr);

impl Proxy {
    fn start(options: &[String]) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_rift"))
            .arg("127.0.0.1:0")
            .args(options)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut proxy = Self(child, "127.0.0.1:0".parse().unwrap());
        let stderr = proxy.0.stderr.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stderr).lines();
            sender.send(lines.next().unwrap().unwrap()).ok();
            // Drain errors too so a full stderr pipe cannot stall the proxy.
            for line in lines {
                if line.is_err() {
                    break;
                }
            }
        });
        let ready = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        proxy.1 = ready
            .strip_prefix("rift: listening on ")
            .unwrap()
            .parse()
            .unwrap();
        proxy
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.0.kill().ok();
        self.0.wait().ok();
    }
}

fn varint(mut value: usize) -> Vec<u8> {
    let mut result = Vec::new();
    while value > 127 {
        result.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    result.push(value as u8);
    result
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut result = varint(body.len());
    result.extend(body);
    result
}

fn handshake(host: &str, state: u8) -> Vec<u8> {
    let mut body = vec![0, 0x86, 0x06];
    body.extend(varint(host.len()));
    body.extend(host.as_bytes());
    body.extend([0x63, 0xdd, state]);
    frame(&body)
}

async fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut length = 0;
    for shift in (0..35).step_by(7) {
        let byte = stream.read_u8().await.unwrap();
        length |= usize::from(byte & 127) << shift;
        if byte & 128 == 0 {
            break;
        }
    }
    assert!(length < 4096);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    body
}

fn login_rejection(name: &str) -> Vec<u8> {
    let json = serde_json::json!({"text": name}).to_string();
    let mut response = vec![0];
    response.extend(varint(json.len()));
    response.extend(json.as_bytes());
    response
}

async fn login_server(listener: TcpListener, name: &'static str, hosts: Vec<&'static str>) {
    for host in hosts {
        let (mut stream, _) = listener.accept().await.unwrap();
        let received = read_frame(&mut stream).await;
        assert_eq!(frame(&received), handshake(host, 2), "handshake changed");
        stream
            .write_all(&frame(&login_rejection(name)))
            .await
            .unwrap();
    }
}

async fn query(address: SocketAddr, host: &str, expected: &str, fragmented: bool) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let request = handshake(host, 2);
    if fragmented {
        for byte in request {
            stream.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    } else {
        stream.write_all(&request).await.unwrap();
    }
    assert_eq!(read_frame(&mut stream).await, login_rejection(expected));
    assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
}

#[tokio::test]
async fn two_domains_reach_different_minecraft_servers_through_one_port() {
    timeout(Duration::from_secs(15), async {
        let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_addr = first.local_addr().unwrap();
        // Exercise DNS and fallback between IPv6/IPv4 localhost addresses.
        let second_addr = format!("localhost:{}", second.local_addr().unwrap().port());
        let proxy = Proxy::start(&[
            "--route".into(),
            format!("survival.example.com={first_addr}"),
            "--route".into(),
            format!("creative.example.com={second_addr}"),
            "--route".into(),
            format!("*.example.com={first_addr}"),
            "--route".into(),
            format!("*.games.example.com={second_addr}"),
            "--default".into(),
            second_addr,
        ]);
        let server_a = tokio::spawn(login_server(
            first,
            "survival",
            vec!["SURVIVAL.Example.COM.\0FML3\0", "other.example.com"],
        ));
        let server_b = tokio::spawn(login_server(
            second,
            "creative",
            vec![
                "creative.example.com",
                "pvp.games.example.com",
                "example.com",
                "badexample.com",
            ],
        ));
        tokio::join!(
            query(proxy.1, "SURVIVAL.Example.COM.\0FML3\0", "survival", true),
            query(proxy.1, "creative.example.com", "creative", false),
        );
        for (host, expected) in [
            ("other.example.com", "survival"),
            ("pvp.games.example.com", "creative"),
            ("example.com", "creative"),
            ("badexample.com", "creative"),
        ] {
            query(proxy.1, host, expected, false).await;
        }
        server_a.await.unwrap();
        server_b.await.unwrap();
    })
    .await
    .unwrap();
}

async fn assert_closed(stream: &mut TcpStream) {
    match timeout(Duration::from_secs(2), stream.read(&mut [0]))
        .await
        .unwrap()
    {
        Ok(0) => (),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => (),
        other => panic!("expected closed connection: {other:?}"),
    }
}

#[tokio::test]
async fn bad_or_unmatched_handshakes_never_connect_to_the_backend() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = Proxy::start(&[
        "--route".into(),
        format!("known.test={}", backend.local_addr().unwrap()),
    ]);
    for bytes in [vec![0x81, 0x10], vec![0x80; 5], vec![1, 1]] {
        let mut client = TcpStream::connect(proxy.1).await.unwrap();
        client.write_all(&bytes).await.unwrap();
        assert_closed(&mut client).await;
    }
    let mut client = TcpStream::connect(proxy.1).await.unwrap();
    client
        .write_all(&handshake("unknown.test", 2))
        .await
        .unwrap();
    assert_eq!(
        read_frame(&mut client).await,
        login_rejection("No server is configured for this hostname.")
    );
    assert_closed(&mut client).await;
    assert!(
        timeout(Duration::from_millis(100), backend.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn handshake_deadline_is_total_even_when_the_client_keeps_sending() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = Proxy::start(&[
        "--default".into(),
        backend.local_addr().unwrap().to_string(),
    ]);
    let client = TcpStream::connect(proxy.1).await.unwrap();
    let (mut reader, mut writer) = client.into_split();
    writer.write_all(&[64]).await.unwrap(); // Length, followed by a slow body.
    let trickle = tokio::spawn(async move {
        loop {
            sleep(Duration::from_millis(200)).await;
            if writer.write_all(&[0]).await.is_err() {
                break;
            }
        }
    });
    let closed = timeout(Duration::from_secs(7), reader.read(&mut [0])).await;
    trickle.abort();
    assert_eq!(closed.unwrap().unwrap(), 0);
    assert!(
        timeout(Duration::from_millis(100), backend.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn login_and_transfer_handshakes_start_independent_backend_connections() {
    timeout(Duration::from_secs(10), async {
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = Proxy::start(&[
            "--route".into(),
            format!("*={}", backend.local_addr().unwrap()),
        ]);
        for state in [2, 3] {
            let mut client = TcpStream::connect(proxy.1).await.unwrap();
            let mut payload = handshake("login.test\0metadata", state);
            let mut start = vec![0, 6];
            start.extend(b"Player");
            start.extend([7; 16]);
            payload.extend(frame(&start));
            client.write_all(&payload).await.unwrap();
            let (mut upstream, _) = backend.accept().await.unwrap();
            assert_eq!(
                frame(&read_frame(&mut upstream).await),
                handshake("login.test\0metadata", state)
            );
            assert_eq!(read_frame(&mut upstream).await, start);
            upstream
                .write_all(&frame(&login_rejection("maintenance")))
                .await
                .unwrap();
            assert_eq!(
                read_frame(&mut client).await,
                login_rejection("maintenance")
            );
            assert_closed(&mut client).await;
        }
    })
    .await
    .unwrap();
}
