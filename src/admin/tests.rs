use super::*;
use rift::protocol::{Codec, Handshake, NextState, Packet, Reader, write_string};

struct SessionFixture {
    client: TcpStream,
    backend: TcpStream,
    second: TcpListener,
    snapshot: Arc<Snapshot>,
    metrics: Arc<Metrics>,
    task: tokio::task::JoinHandle<io::Result<(u64, u64)>>,
    id: u64,
    codec: Codec,
}

async fn receive(stream: &mut TcpStream, codec: Codec) -> Packet {
    Reader::default()
        .read(stream, codec)
        .await
        .unwrap()
        .unwrap()
}

fn login_success(identity: u8) -> Packet {
    let mut data = vec![identity; 16];
    write_string("Player", &mut data);
    data.push(0);
    Packet::new(2, data)
}

impl SessionFixture {
    async fn new() -> Self {
        let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let frontend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config =
            Config::from_addresses("127.0.0.1:0", &first.local_addr().unwrap().to_string())
                .unwrap();
        config.backends.insert(
            "second".into(),
            second.local_addr().unwrap().to_string().parse().unwrap(),
        );
        let snapshot = Arc::new(Snapshot::new(config, None).unwrap());
        let metrics = Arc::new(Metrics::default());
        let mut client = TcpStream::connect(frontend.local_addr().unwrap())
            .await
            .unwrap();
        let (mut accepted, peer) = frontend.accept().await.unwrap();
        let mut event = events::Connection::new("default", peer);
        let id = event.id();
        let state = snapshot.clone();
        let counters = metrics.clone();
        let task = tokio::spawn(async move {
            crate::runtime::handle(&mut accepted, "default", state, &[], counters, &mut event).await
        });
        let handshake = Handshake {
            protocol: 774,
            address: "play.test".into(),
            port: 25565,
            next_state: NextState::Login,
        }
        .packet();
        Codec::default()
            .write(&mut client, &handshake)
            .await
            .unwrap();
        let mut data = Vec::new();
        write_string("Player", &mut data);
        data.extend([7; 16]);
        let start = Packet::new(0, data);
        Codec::default().write(&mut client, &start).await.unwrap();
        let (mut backend, _) = first.accept().await.unwrap();
        assert_eq!(receive(&mut backend, Codec::default()).await, handshake);
        assert_eq!(receive(&mut backend, Codec::default()).await, start);
        Codec::default()
            .write(&mut backend, &Packet::new(3, vec![0]))
            .await
            .unwrap();
        assert_eq!(
            receive(&mut client, Codec::default()).await,
            Packet::new(3, vec![0])
        );
        let mut codec = Codec::default();
        codec.set_compression(0);
        codec.write(&mut backend, &login_success(7)).await.unwrap();
        assert_eq!(receive(&mut client, codec).await, login_success(7));
        codec.write(&mut client, &Packet::empty(3)).await.unwrap();
        assert_eq!(receive(&mut backend, codec).await, Packet::empty(3));
        codec.write(&mut backend, &Packet::empty(3)).await.unwrap();
        assert_eq!(receive(&mut client, codec).await, Packet::empty(3));
        codec.write(&mut client, &Packet::empty(3)).await.unwrap();
        assert_eq!(receive(&mut backend, codec).await, Packet::empty(3));
        // The forwarding write completes before registration on another worker.
        while metrics.players.get() == 0 || snapshot.control.players.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        Self {
            client,
            backend,
            second,
            snapshot,
            metrics,
            task,
            id,
            codec,
        }
    }
}

#[tokio::test]
async fn admin_transfer_keeps_compressed_client_and_uses_current_backend_configuration() {
    timeout(Duration::from_secs(5), async {
        let mut fixture = SessionFixture::new().await;
        let mut config = fixture.snapshot.config.clone();
        config.routes.insert(
            "default".into(),
            rift::config::Route::Direct("second".into()),
        );
        config.draining.insert("default".into());
        let current = Arc::new(Snapshot::new(config, Some(&fixture.snapshot)).unwrap());
        // A route reload and draining leave the established session attached.
        let packet = Packet::new(0x7f, vec![42; 1000]);
        fixture
            .codec
            .write(&mut fixture.client, &packet)
            .await
            .unwrap();
        assert_eq!(receive(&mut fixture.backend, fixture.codec).await, packet);
        let args = vec!["transfer".into(), fixture.id.to_string(), "second".into()];
        let (commands, _) = mpsc::channel(1);
        let operation = execute(&args, current.clone(), &fixture.metrics, &commands);
        let peers = async {
            let (mut second, _) = fixture.second.accept().await.unwrap();
            assert_eq!(
                receive(&mut fixture.client, fixture.codec).await,
                Packet::empty(0x74)
            );
            fixture
                .codec
                .write(&mut fixture.client, &Packet::empty(0x0f))
                .await
                .unwrap();
            assert_eq!(
                Handshake::decode(&receive(&mut second, Codec::default()).await)
                    .unwrap()
                    .next_state,
                NextState::Login
            );
            assert_eq!(receive(&mut second, Codec::default()).await.id, 0);
            Codec::default()
                .write(&mut second, &login_success(7))
                .await
                .unwrap();
            assert_eq!(
                receive(&mut second, Codec::default()).await,
                Packet::empty(3)
            );
            Codec::default()
                .write(&mut second, &Packet::empty(3))
                .await
                .unwrap();
            assert_eq!(
                receive(&mut fixture.client, fixture.codec).await,
                Packet::empty(3)
            );
            fixture
                .codec
                .write(&mut fixture.client, &Packet::empty(3))
                .await
                .unwrap();
            assert_eq!(
                receive(&mut second, Codec::default()).await,
                Packet::empty(3)
            );
            second
        };
        let (result, mut second) = tokio::join!(operation, peers);
        assert_eq!(result.unwrap()["backend"], "second");
        assert_eq!(fixture.metrics.players.get(), 1);
        assert_eq!(fixture.metrics.transfers.get(), 1);
        assert_eq!(
            current.control.status(&current, &fixture.metrics)["players"][0]["backend"],
            "second"
        );
        fixture
            .codec
            .write(&mut fixture.client, &packet)
            .await
            .unwrap();
        assert_eq!(receive(&mut second, Codec::default()).await, packet);
        Codec::default().write(&mut second, &packet).await.unwrap();
        assert_eq!(receive(&mut fixture.client, fixture.codec).await, packet);
        drop(fixture.client);
        drop(second);
        fixture.task.await.unwrap().unwrap();
        assert_eq!(fixture.metrics.players.get(), 0);
        assert!(current.control.players.lock().unwrap().is_empty());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn refused_transfer_preserves_original_session() {
    timeout(Duration::from_secs(5), async {
        let mut fixture = SessionFixture::new().await;
        drop(fixture.second);
        let args = vec!["transfer".into(), fixture.id.to_string(), "second".into()];
        let (commands, _) = mpsc::channel(1);
        assert!(
            execute(&args, fixture.snapshot.clone(), &fixture.metrics, &commands)
                .await
                .unwrap_err()
                .contains("original backend")
        );
        let packet = Packet::new(0x7f, vec![12; 300]);
        fixture
            .codec
            .write(&mut fixture.client, &packet)
            .await
            .unwrap();
        assert_eq!(receive(&mut fixture.backend, fixture.codec).await, packet);
        assert_eq!(fixture.metrics.players.get(), 1);
        assert_eq!(fixture.metrics.transfers.get(), 0);
        assert_eq!(fixture.metrics.transfer_failures.get(), 1);
        assert_eq!(
            fixture
                .snapshot
                .control
                .status(&fixture.snapshot, &fixture.metrics)["players"][0]["backend"],
            "default"
        );
        fixture.task.abort();
        let _ = fixture.task.await;
        assert_eq!(fixture.metrics.players.get(), 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn replacement_identity_mismatch_closes_session_and_reports_failure() {
    timeout(Duration::from_secs(5), async {
        let mut fixture = SessionFixture::new().await;
        let args = vec!["transfer".into(), fixture.id.to_string(), "second".into()];
        let (commands, _) = mpsc::channel(1);
        let operation = execute(&args, fixture.snapshot.clone(), &fixture.metrics, &commands);
        let peers = async {
            let (mut second, _) = fixture.second.accept().await.unwrap();
            assert_eq!(
                receive(&mut fixture.client, fixture.codec).await,
                Packet::empty(0x74)
            );
            fixture
                .codec
                .write(&mut fixture.client, &Packet::empty(0x0f))
                .await
                .unwrap();
            receive(&mut second, Codec::default()).await;
            receive(&mut second, Codec::default()).await;
            Codec::default()
                .write(&mut second, &login_success(8))
                .await
                .unwrap();
            assert!(
                Reader::default()
                    .read(&mut fixture.client, fixture.codec)
                    .await
                    .unwrap()
                    .is_none()
            );
        };
        let (result, ()) = tokio::join!(operation, peers);
        assert!(result.unwrap_err().contains("identity"));
        assert!(fixture.task.await.unwrap().is_err());
        assert_eq!(fixture.metrics.players.get(), 0);
        assert_eq!(fixture.metrics.transfer_failures.get(), 1);
        assert!(fixture.snapshot.control.players.lock().unwrap().is_empty());
    })
    .await
    .unwrap();
}
#[test]
fn permission_checks_precede_execution_and_do_not_echo_secrets() {
    let settings = Admin {
        listen: "127.0.0.1:9091".parse().unwrap(),
        token_env: "RIFT_ADMIN_TOKEN".into(),
        permissions: ["status".into()].into(),
    };
    let secret = "a-secret-that-must-never-be-logged";
    assert!(
        request(
            &json!({"token":secret,"args":["status"]}),
            &settings,
            secret
        )
        .is_ok()
    );
    assert!(
        request(
            &json!({"token":secret,"args":["shutdown"]}),
            &settings,
            secret
        )
        .unwrap_err()
        .contains("permission denied")
    );
    for token in ["", "wrong", "a-secret-that-must-never-be-loggeX"] {
        let error =
            request(&json!({"token":token,"args":["status"]}), &settings, secret).unwrap_err();
        assert!(!error.contains(secret));
        assert!(error.contains("authentication failed"));
    }
}
