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

fn join_game(entity: i32) -> Packet {
    let mut data = entity.to_be_bytes().to_vec();
    data.extend([0, 1]);
    write_string("minecraft:overworld", &mut data);
    data.extend([20, 8, 8, 0, 1, 0, 0]);
    write_string("minecraft:overworld", &mut data);
    data.extend([0; 8]);
    data.extend([0, 255, 0, 0, 0, 0, 63, 0]);
    Packet::new(0x30, data)
}

impl SessionFixture {
    async fn new() -> Self {
        Self::with_access(None).await
    }

    async fn with_access(access: Option<rift::config::ServerAccess>) -> Self {
        Self::configured(access, true).await
    }

    async fn configured(access: Option<rift::config::ServerAccess>, network_enabled: bool) -> Self {
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
        if network_enabled {
            config.network.hubs = vec!["default".into()];
        }
        if let Some(access) = access {
            config.network.access.insert("second".into(), access);
        }
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
        codec.write(&mut backend, &join_game(11)).await.unwrap();
        assert_eq!(receive(&mut client, codec).await, join_game(11));
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
async fn admin_transfer_without_network_keeps_options_bundles_and_current_configuration() {
    timeout(Duration::from_secs(5), async {
        let mut fixture = SessionFixture::configured(None, false).await;
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
        let mut settings = Vec::new();
        write_string("en_us", &mut settings);
        settings.extend([2, 0, 1, 127, 1, 0, 1, 2]);
        let mut brand = Vec::new();
        write_string("minecraft:brand", &mut brand);
        write_string("admin-test", &mut brand);
        for client_packet in [
            Packet::new(0x0d, settings.clone()),
            Packet::new(0x15, brand.clone()),
        ] {
            fixture
                .codec
                .write(&mut fixture.client, &client_packet)
                .await
                .unwrap();
            assert_eq!(
                receive(&mut fixture.backend, fixture.codec).await,
                client_packet
            );
        }
        // Omitting network must not expose player commands to every backend.
        let mut command = Vec::new();
        write_string("server second", &mut command);
        let command = Packet::new(6, command);
        fixture
            .codec
            .write(&mut fixture.client, &command)
            .await
            .unwrap();
        assert_eq!(receive(&mut fixture.backend, fixture.codec).await, command);
        fixture
            .codec
            .write(&mut fixture.backend, &Packet::empty(0))
            .await
            .unwrap();
        assert_eq!(
            receive(&mut fixture.client, fixture.codec).await,
            Packet::empty(0)
        );
        let args = vec!["transfer".into(), fixture.id.to_string(), "second".into()];
        let (commands, _) = mpsc::channel(1);
        let operation = execute(&args, current.clone(), &fixture.metrics, &commands);
        let peers = async {
            let (mut second, _) = fixture.second.accept().await.unwrap();
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
            assert_eq!(
                receive(&mut fixture.client, fixture.codec).await,
                Packet::empty(0),
                "old bundle must close before Start Configuration"
            );
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
                receive(&mut fixture.client, fixture.codec).await,
                Packet::new(8, vec![0])
            );
            assert_eq!(
                receive(&mut second, Codec::default()).await,
                Packet::new(0, settings)
            );
            assert_eq!(
                receive(&mut second, Codec::default()).await,
                Packet::new(2, brand)
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
            tokio::task::yield_now().await;
            assert_eq!(
                fixture.metrics.transfers.get(),
                0,
                "configuration finish is not world entry"
            );
            assert_eq!(
                current.control.status(&current, &fixture.metrics)["players"][0]["backend"],
                "default"
            );
            assert_eq!(current.players.get(&[7; 16]).unwrap().server, "default");
            Codec::default()
                .write(&mut second, &join_game(22))
                .await
                .unwrap();
            assert_eq!(
                receive(&mut fixture.client, fixture.codec).await,
                join_game(22)
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
async fn replacement_identity_mismatch_preserves_session_and_reports_failure() {
    timeout(Duration::from_secs(5), async {
        let mut fixture = SessionFixture::new().await;
        let args = vec!["transfer".into(), fixture.id.to_string(), "second".into()];
        let (commands, _) = mpsc::channel(1);
        let operation = execute(&args, fixture.snapshot.clone(), &fixture.metrics, &commands);
        let peers = async {
            let (mut second, _) = fixture.second.accept().await.unwrap();
            receive(&mut second, Codec::default()).await;
            receive(&mut second, Codec::default()).await;
            Codec::default()
                .write(&mut second, &login_success(8))
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(operation, peers);
        assert!(result.unwrap_err().contains("identity"));
        assert_original_session(&mut fixture).await;
        assert_eq!(fixture.metrics.transfer_failures.get(), 1);
        fixture.task.abort();
        let _ = fixture.task.await;
        assert_eq!(fixture.metrics.players.get(), 0);
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

async fn assert_original_session(fixture: &mut SessionFixture) {
    let packet = Packet::new(0x7f, vec![12; 300]);
    fixture
        .codec
        .write(&mut fixture.client, &packet)
        .await
        .unwrap();
    assert_eq!(receive(&mut fixture.backend, fixture.codec).await, packet);
    fixture
        .codec
        .write(&mut fixture.backend, &packet)
        .await
        .unwrap();
    assert_eq!(receive(&mut fixture.client, fixture.codec).await, packet);
    assert_eq!(fixture.metrics.players.get(), 1);
    assert_eq!(fixture.metrics.transfers.get(), 0);
    assert_eq!(
        fixture
            .snapshot
            .control
            .status(&fixture.snapshot, &fixture.metrics)["players"][0]["backend"],
        "default"
    );
    assert_eq!(
        fixture.snapshot.players.get(&[7; 16]).unwrap().server,
        "default"
    );
}

#[tokio::test]
async fn admin_transfer_obeys_both_original_and_current_player_access_rules() {
    timeout(Duration::from_secs(5), async {
        let denied = || rift::config::ServerAccess {
            allow: Some(Vec::new()),
            deny: Vec::new(),
        };
        for deny_original in [false, true] {
            let mut fixture = SessionFixture::with_access(deny_original.then(denied)).await;
            let mut config = fixture.snapshot.config.clone();
            if deny_original {
                config.network.access.clear();
            } else {
                config.network.access.insert("second".into(), denied());
            }
            let current = Arc::new(Snapshot::new(config, Some(&fixture.snapshot)).unwrap());
            let args = vec!["transfer".into(), fixture.id.to_string(), "second".into()];
            let (commands, _) = mpsc::channel(1);
            let result = execute(&args, current, &fixture.metrics, &commands).await;
            assert!(
                result.is_err(),
                "admin transfer bypassed a player's access rule"
            );
            assert!(
                timeout(Duration::from_millis(10), fixture.second.accept())
                    .await
                    .is_err(),
                "denied backend was contacted"
            );
            assert_original_session(&mut fixture).await;
            fixture.task.abort();
            let _ = fixture.task.await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn admin_transfer_keeps_the_original_backend_after_an_explicit_target_ban() {
    timeout(Duration::from_secs(5), async {
        let mut fixture = SessionFixture::new().await;
        let args = vec!["transfer".into(), fixture.id.to_string(), "second".into()];
        let (commands, _) = mpsc::channel(1);
        let operation = execute(&args, fixture.snapshot.clone(), &fixture.metrics, &commands);
        let peers = async {
            let (mut second, _) = fixture.second.accept().await.unwrap();
            assert_eq!(receive(&mut second, Codec::default()).await.id, 0);
            assert_eq!(receive(&mut second, Codec::default()).await.id, 0);
            let mut data = Vec::new();
            write_string("{\"text\":\"Explicit target ban\"}", &mut data);
            Codec::default()
                .write(&mut second, &Packet::new(0, data))
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(operation, peers);
        assert!(result.unwrap_err().contains("Explicit target ban"));
        assert_original_session(&mut fixture).await;
        assert_eq!(fixture.metrics.transfer_failures.get(), 1);
        fixture.task.abort();
        let _ = fixture.task.await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn admin_transfer_rejects_draining_backends_and_unmapped_protocols_before_contact() {
    timeout(Duration::from_secs(5), async {
        let mut fixture = SessionFixture::new().await;
        let args = vec!["transfer".into(), fixture.id.to_string(), "second".into()];
        let (commands, _) = mpsc::channel(1);
        fixture
            .snapshot
            .control
            .draining
            .lock()
            .unwrap()
            .insert("second".into(), true);
        let result = execute(&args, fixture.snapshot.clone(), &fixture.metrics, &commands).await;
        assert!(result.unwrap_err().contains("draining"));
        fixture
            .snapshot
            .control
            .draining
            .lock()
            .unwrap()
            .insert("second".into(), false);
        for protocol in [47, 764, 773, 775, 777] {
            fixture
                .snapshot
                .control
                .players
                .lock()
                .unwrap()
                .get_mut(&fixture.id)
                .unwrap()
                .protocol = protocol;
            let result =
                execute(&args, fixture.snapshot.clone(), &fixture.metrics, &commands).await;
            assert!(result.unwrap_err().contains("774"));
        }
        assert!(
            timeout(Duration::from_millis(10), fixture.second.accept())
                .await
                .is_err()
        );
        assert_original_session(&mut fixture).await;
        fixture.task.abort();
        let _ = fixture.task.await;
    })
    .await
    .unwrap();
}
