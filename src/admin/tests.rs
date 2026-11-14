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
        Self::build(access, network_enabled, None).await
    }

    async fn with_tab_list(tab_list: rift::tab_list::TabList) -> Self {
        Self::build(None, true, Some(tab_list)).await
    }

    async fn build(
        access: Option<rift::config::ServerAccess>,
        network_enabled: bool,
        tab_list: Option<rift::tab_list::TabList>,
    ) -> Self {
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
        config.tab_list = tab_list;
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
        let operation = execute(&args, current.clone(), &fixture.metrics, &commands, "test");
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
            execute(
                &args,
                fixture.snapshot.clone(),
                &fixture.metrics,
                &commands,
                "test"
            )
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
        let operation = execute(
            &args,
            fixture.snapshot.clone(),
            &fixture.metrics,
            &commands,
            "test",
        );
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

#[test]
fn service_group_commands_share_the_servers_permission() {
    let mut settings = Admin {
        listen: "127.0.0.1:9091".parse().unwrap(),
        token_env: "RIFT_ADMIN_TOKEN".into(),
        permissions: ["servers".into()].into(),
    };
    let secret = "test-service-group-administration-secret";
    for args in [
        json!(["groups"]),
        json!(["create", "lobby"]),
        json!(["remove", "lobby-1"]),
    ] {
        let value = json!({"token":secret,"args":args});
        assert!(request(&value, &settings, secret).is_ok());
        settings.permissions.clear();
        assert!(
            request(&value, &settings, secret)
                .unwrap_err()
                .contains("permission denied: servers")
        );
        settings.permissions.insert("servers".into());
    }
}

#[test]
fn plugin_restarts_need_plugins_logs_need_logs_and_reads_need_status() {
    let mut settings = Admin {
        listen: "127.0.0.1:9091".parse().unwrap(),
        token_env: "RIFT_ADMIN_TOKEN".into(),
        permissions: ["status".into()].into(),
    };
    let secret = "test-plugin-administration-secret";
    let value = |args: Value| json!({"token":secret,"args":args});
    for args in [
        json!(["plugins", "list"]),
        json!(["plugins", "status", "a"]),
    ] {
        assert!(request(&value(args), &settings, secret).is_ok());
    }
    // The log tail needs logs, as it does over HTTP.
    let logs = value(json!(["plugins", "logs", "a"]));
    assert!(
        request(&logs, &settings, secret)
            .unwrap_err()
            .contains("permission denied: logs")
    );
    settings.permissions.insert("logs".into());
    assert!(request(&logs, &settings, secret).is_ok());
    let restart = value(json!(["plugins", "restart", "a"]));
    assert!(
        request(&restart, &settings, secret)
            .unwrap_err()
            .contains("permission denied: plugins")
    );
    settings.permissions = ["plugins".into()].into();
    assert!(request(&restart, &settings, secret).is_ok());
    assert!(request(&value(json!(["plugins", "list"])), &settings, secret).is_err());
    let args = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        audited(&args(&["plugins", "restart", "hello-lua"])),
        ("plugins restart".to_owned(), "hello-lua".to_owned())
    );
    assert_eq!(
        audited(&args(&["plugins", "list"])),
        ("plugins list".to_owned(), String::new())
    );
    assert_eq!(
        audited(&args(&["drain", "lobby", "on"])),
        ("drain".to_owned(), String::new())
    );
}

#[tokio::test]
async fn plugin_commands_report_unknown_plugins_and_usage() {
    let snapshot = Snapshot::new(
        Config::from_addresses("127.0.0.1:0", "127.0.0.1:1").unwrap(),
        None,
    )
    .unwrap();
    let args = |values: &[&str]| values.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        plugins(&snapshot, &args(&["list"]), "test").unwrap(),
        json!({"plugins": []})
    );
    for command in ["status", "logs", "restart"] {
        let error = plugins(&snapshot, &args(&[command, "ghost"]), "test").unwrap_err();
        assert!(error.contains("ghost"), "{error}");
    }
    for invalid in [&["stop", "a"][..], &["list", "a"], &[]] {
        assert!(
            plugins(&snapshot, &args(invalid), "test")
                .unwrap_err()
                .starts_with("usage: plugins")
        );
    }
}

#[tokio::test]
async fn instance_commands_wait_for_runtime_completion_and_report_queue_failure() {
    let snapshot = Arc::new(
        Snapshot::new(
            Config::from_addresses("127.0.0.1:0", "127.0.0.1:1").unwrap(),
            None,
        )
        .unwrap(),
    );
    let metrics = Metrics::default();
    let (commands, mut receiver) = mpsc::channel(1);
    let args = vec!["create".into(), "lobby".into()];
    let (result, ()) = tokio::join!(
        execute(&args, snapshot.clone(), &metrics, &commands, "test"),
        async {
            match receiver.recv().await.unwrap() {
                Command::CreateInstance { group, reply } => {
                    assert_eq!(group, "lobby");
                    reply
                        .send(Ok(json!({"name":"lobby-1","port":25600})))
                        .unwrap();
                }
                _ => panic!("expected create command"),
            }
        }
    );
    assert_eq!(result.unwrap()["name"], "lobby-1");
    let args = vec!["remove".into(), "lobby-1".into()];
    let (result, ()) = tokio::join!(
        execute(&args, snapshot.clone(), &metrics, &commands, "test"),
        async {
            match receiver.recv().await.unwrap() {
                Command::RemoveInstance { name, reply } => {
                    assert_eq!(name, "lobby-1");
                    reply.send(Err("instance is occupied".into())).unwrap();
                }
                _ => panic!("expected remove command"),
            }
        }
    );
    assert_eq!(result.unwrap_err(), "instance is occupied");
    drop(receiver);
    assert!(
        execute(&args, snapshot, &metrics, &commands, "test")
            .await
            .unwrap_err()
            .contains("queue")
    );
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
            ..Default::default()
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
            let result = execute(&args, current, &fixture.metrics, &commands, "test").await;
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
        let operation = execute(
            &args,
            fixture.snapshot.clone(),
            &fixture.metrics,
            &commands,
            "test",
        );
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
async fn admin_transfer_accepts_every_switching_protocol() {
    timeout(Duration::from_secs(5), async {
        let fixture = SessionFixture::new().await;
        let args = vec!["transfer".into(), fixture.id.to_string(), "second".into()];
        let (commands, _) = mpsc::channel(1);
        let (sender, mut receiver) = mpsc::channel::<Transfer>(1);
        for protocol in (0..=777).filter(|number| {
            rift::protocol::ProtocolVersion::new(*number)
                .is_ok_and(rift::protocol::ProtocolVersion::supports_switching)
        }) {
            {
                let mut players = fixture.snapshot.control.players.lock().unwrap();
                let entry = players.get_mut(&fixture.id).unwrap();
                entry.protocol = protocol;
                entry.transfer = sender.clone();
            }
            let (result, ()) = tokio::join!(
                execute(
                    &args,
                    fixture.snapshot.clone(),
                    &fixture.metrics,
                    &commands,
                    "test"
                ),
                async {
                    let request = receiver.recv().await.unwrap();
                    assert_eq!(request.backend, "second");
                    request.reply.send(Err("reached session".into())).unwrap();
                }
            );
            assert_eq!(
                result.unwrap_err(),
                "reached session",
                "protocol {protocol}"
            );
        }
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
        let result = execute(
            &args,
            fixture.snapshot.clone(),
            &fixture.metrics,
            &commands,
            "test",
        )
        .await;
        assert!(result.unwrap_err().contains("draining"));
        fixture
            .snapshot
            .control
            .draining
            .lock()
            .unwrap()
            .insert("second".into(), false);
        for protocol in [46, 48, 106, 778, 1073742100] {
            fixture
                .snapshot
                .control
                .players
                .lock()
                .unwrap()
                .get_mut(&fixture.id)
                .unwrap()
                .protocol = protocol;
            let result = execute(
                &args,
                fixture.snapshot.clone(),
                &fixture.metrics,
                &commands,
                "test",
            )
            .await;
            assert!(result.unwrap_err().contains("1.8.9–26.3"));
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

#[tokio::test]
async fn player_actions_reach_the_client_and_kick_closes_the_session() {
    use rift::extensions::PlayerAction;
    timeout(Duration::from_secs(5), async {
        let mut fixture = SessionFixture::new().await;
        let players = fixture.snapshot.extensions.players().clone();
        let send = |action| players.send("player", action).unwrap();
        let version = rift::protocol::ProtocolVersion::new(774).unwrap();
        // The directory joins right after the admin registration, on the session task.
        while !send(PlayerAction::Message("hello".into())) {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            receive(&mut fixture.client, fixture.codec).await,
            rift::protocol::system_message(version, "hello").unwrap()
        );
        assert!(send(PlayerAction::ActionBar("bar".into())));
        assert_eq!(
            receive(&mut fixture.client, fixture.codec).await,
            rift::protocol::action_bar(version, "bar").unwrap()
        );
        assert!(send(PlayerAction::Title {
            title: Some("Big".into()),
            subtitle: Some("small".into()),
            times: [1, 2, 3],
        }));
        let (big, small) = ("Big".into(), "small".into());
        for packet in rift::protocol::title(version, Some(&big), Some(&small), [1, 2, 3]).unwrap() {
            assert_eq!(receive(&mut fixture.client, fixture.codec).await, packet);
        }
        // An unavailable target leaves the player on the original backend.
        drop(fixture.second);
        assert!(send(PlayerAction::Transfer("second".into())));
        assert!(send(PlayerAction::Message("still here".into())));
        assert_eq!(
            receive(&mut fixture.client, fixture.codec).await,
            rift::protocol::system_message(version, "still here").unwrap()
        );
        let packet = Packet::new(0x7f, vec![12; 300]);
        fixture
            .codec
            .write(&mut fixture.client, &packet)
            .await
            .unwrap();
        assert_eq!(receive(&mut fixture.backend, fixture.codec).await, packet);
        assert_eq!(fixture.metrics.transfers.get(), 0);
        assert!(send(PlayerAction::Kick("Bye".into())));
        assert_eq!(
            receive(&mut fixture.client, fixture.codec).await,
            rift::protocol::disconnect(Some(version), rift::protocol::State::Play, "Bye").unwrap()
        );
        fixture.task.await.unwrap().unwrap();
        assert!(!send(PlayerAction::Message("gone".into())));
        assert_eq!(fixture.metrics.players.get(), 0);
        assert!(fixture.snapshot.control.players.lock().unwrap().is_empty());
    })
    .await
    .unwrap();
}

/// 1.21.11 tab list packets: NBT string components, then Player Info entries.
fn tab_header(header: &str, footer: &str) -> Packet {
    let mut data = Vec::new();
    for text in [header, footer] {
        data.push(8);
        data.extend((text.len() as u16).to_be_bytes());
        data.extend(text.as_bytes());
    }
    Packet::new(0x78, data)
}

fn tab_add(uuid: u8, name: &str) -> Packet {
    let mut data = vec![9, 1];
    data.extend([uuid; 16]);
    write_string(name, &mut data);
    data.extend([0, 1]);
    Packet::new(0x44, data)
}

#[tokio::test]
async fn tab_list_shows_header_footer_and_players_on_other_servers() {
    use rift::extensions::PlayerAction;
    timeout(Duration::from_secs(10), async {
        let mut fixture = SessionFixture::with_tab_list(rift::tab_list::TabList {
            header: Some(serde_json::json!("Hi {player}")),
            footer: Some(serde_json::json!("{online} on {server}")),
            network: true,
        })
        .await;
        let codec = fixture.codec;
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            tab_header("Hi Player", "1 on default")
        );
        // A player elsewhere joins the list and the count.
        let remote = fixture
            .snapshot
            .players
            .register([8; 16], "Remote", "second")
            .unwrap();
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            tab_header("Hi Player", "2 on default")
        );
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            tab_add(8, "Remote")
        );
        // The backend's own header is dropped while Rift owns it; its entry for
        // the same player replaces Rift's without a removal.
        let mut backend_add = vec![9, 1];
        backend_add.extend([8; 16]);
        write_string("Remote", &mut backend_add);
        backend_add.extend([0, 1]);
        for packet in [
            tab_header("Backend", "header"),
            Packet::new(0x44, backend_add.clone()),
        ] {
            codec.write(&mut fixture.backend, &packet).await.unwrap();
        }
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            Packet::new(0x44, backend_add)
        );
        remote.set_server("default");
        drop(remote);
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            tab_header("Hi Player", "1 on default")
        );
        // Rift removes only its own entries once their player leaves.
        let other = fixture
            .snapshot
            .players
            .register([9; 16], "Other", "second")
            .unwrap();
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            tab_header("Hi Player", "2 on default")
        );
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            tab_add(9, "Other")
        );
        drop(other);
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            tab_header("Hi Player", "1 on default")
        );
        let mut removal = vec![1];
        removal.extend([9; 16]);
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            Packet::new(0x43, removal)
        );
        // A per-player override from rift.players replaces configured text.
        let players = fixture.snapshot.extensions.players().clone();
        assert!(
            players
                .send(
                    "player",
                    PlayerAction::TabList(Some(rift::tab_list::Override {
                        header: Some(serde_json::json!("VIP {player}")),
                        footer: None,
                    })),
                )
                .unwrap()
        );
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            tab_header("VIP Player", "1 on default")
        );
        assert!(players.send("player", PlayerAction::TabList(None)).unwrap());
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            tab_header("Hi Player", "1 on default")
        );
        fixture.task.abort();
        let _ = fixture.task.await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn boss_bars_survive_reconfiguration_and_can_be_removed() {
    use rift::extensions::PlayerAction;
    timeout(Duration::from_secs(10), async {
        let mut fixture = SessionFixture::new().await;
        let codec = fixture.codec;
        let players = fixture.snapshot.extensions.players().clone();
        let bar = rift::protocol::BossBar {
            text: serde_json::json!("Event"),
            progress: 0.5,
            color: 1,
            style: 0,
            flags: 0,
        };
        assert!(
            players
                .send(
                    "player",
                    PlayerAction::BossBar {
                        name: "event".into(),
                        bar: Some(bar),
                    },
                )
                .unwrap()
        );
        // 1.21.11 Boss Event: UUID, Add, NBT string, progress, color, style, flags.
        let added = receive(&mut fixture.client, codec).await;
        assert_eq!(added.id, 0x09);
        let mut body = vec![0, 8, 0, 5];
        body.extend(b"Event");
        body.extend(0.5f32.to_be_bytes());
        body.extend([1, 0, 0]);
        assert_eq!(added.data[16..], body);
        // A backend-initiated reconfiguration clears the client's boss bars.
        codec
            .write(&mut fixture.backend, &Packet::empty(0x74))
            .await
            .unwrap();
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            Packet::empty(0x74)
        );
        codec
            .write(&mut fixture.client, &Packet::empty(0x0f))
            .await
            .unwrap();
        assert_eq!(
            receive(&mut fixture.backend, codec).await,
            Packet::empty(0x0f)
        );
        codec
            .write(&mut fixture.backend, &Packet::empty(3))
            .await
            .unwrap();
        assert_eq!(receive(&mut fixture.client, codec).await, Packet::empty(3));
        codec
            .write(&mut fixture.client, &Packet::empty(3))
            .await
            .unwrap();
        assert_eq!(receive(&mut fixture.backend, codec).await, Packet::empty(3));
        codec
            .write(&mut fixture.backend, &join_game(12))
            .await
            .unwrap();
        assert_eq!(receive(&mut fixture.client, codec).await, join_game(12));
        assert_eq!(receive(&mut fixture.client, codec).await, added);
        assert!(
            players
                .send(
                    "player",
                    PlayerAction::BossBar {
                        name: "event".into(),
                        bar: None,
                    },
                )
                .unwrap()
        );
        let mut removal = added.data[..16].to_vec();
        removal.push(1);
        assert_eq!(
            receive(&mut fixture.client, codec).await,
            Packet::new(0x09, removal)
        );
        fixture.task.abort();
        let _ = fixture.task.await;
    })
    .await
    .unwrap();
}

/// A ban service whose login may only publish `rift.players.kick-json` and
/// `rift.players.message-json` sends a player a clickable message, kicks
/// them with a multi-line coloured screen, cannot use any other player
/// operation or stop a server, and both actions are in the audit log under
/// its own login.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_players_only_login_messages_and_kicks_with_rich_text_and_is_audited() {
    use rift::messaging::quic;
    use rustls::pki_types::PrivateKeyDer;
    timeout(Duration::from_secs(10), async {
        let mut fixture = SessionFixture::new().await;
        let broker = fixture.snapshot.messaging.clone();
        let (current, receiver) = tokio::sync::watch::channel(fixture.snapshot.clone());
        let audit = Arc::new(crate::operator::Store::open(None).unwrap());
        let (commands, _requests) = tokio::sync::mpsc::channel(1);
        let players = crate::messaging_runtime::PlayerRequests::new(broker.clone()).unwrap();
        let controls = crate::messaging_runtime::Controls::new(broker.clone()).unwrap();
        let services = [
            tokio::spawn(players.run(receiver.clone(), audit.clone())),
            tokio::spawn(controls.run(receiver, fixture.metrics.clone(), commands, audit.clone())),
        ];
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let server_tls = quic::server_config_from_der(
            vec![cert.cert.der().clone()],
            PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
        )
        .unwrap();
        let token = "relaxoban-test-token-with-at-least-32-bytes";
        let server = quic::Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_tls,
            broker.clone(),
            quic::AuthConfig {
                identities: vec![quic::Identity {
                    name: "relaxoban".into(),
                    token: token.into(),
                    publish: vec![
                        "rift.players.kick-json".into(),
                        "rift.players.message-json".into(),
                        // Even a grant for every control subject needs `control`.
                        "rift.control.*".into(),
                        "_INBOX.>".into(),
                    ],
                    subscribe: vec!["_INBOX.>".into()],
                    control: false,
                    players: true,
                }],
                ..Default::default()
            },
        )
        .unwrap();
        let client = quic::Client::connect(
            server.local_addr().unwrap(),
            "localhost",
            quic::client_config_from_der(vec![cert.cert.der().clone()]).unwrap(),
            token,
        )
        .await
        .unwrap();
        let request = |subject: &'static str, payload: Value| {
            let client = &client;
            async move {
                let reply = client
                    .request(
                        subject,
                        bytes::Bytes::from(payload.to_string()),
                        Duration::from_secs(5),
                    )
                    .await?;
                Ok::<Value, rift::messaging::Error>(serde_json::from_slice(&reply.payload).unwrap())
            }
        };
        let version = rift::protocol::ProtocolVersion::new(774).unwrap();

        let appeal = json!({
            "text": "Appeal here",
            "color": "aqua",
            "underlined": true,
            "clickEvent": {"action": "open_url", "value": "https://example.org/appeal"},
            "hoverEvent": {"action": "show_text", "contents": "Opens the appeal form"},
        });
        let reply = request(
            "rift.players.message-json",
            json!({"target": "Player", "text": appeal}),
        )
        .await
        .unwrap();
        assert_eq!(reply, json!({"ok": true, "data": true}));
        assert_eq!(
            receive(&mut fixture.client, fixture.codec).await,
            rift::protocol::system_component(version, &appeal).unwrap()
        );

        // Grants are per subject: the plain operations are not included.
        let refused = request(
            "rift.players.kick",
            json!({"target": "Player", "reason": "x"}),
        )
        .await;
        assert!(refused.is_err(), "kick was not granted: {refused:?}");
        let refused = request("rift.control.stop", json!({"name": "default"})).await;
        assert!(refused.is_err(), "control was not granted: {refused:?}");

        let screen = json!({"text": "", "extra": [
            {"text": "You are banned from this network\n", "color": "red", "bold": true},
            {"text": "Reason: ", "color": "gray"},
            {"text": "Cheating\n\n", "color": "white"},
            {"text": "Appeal at example.org/appeal", "color": "aqua"},
        ]});
        let reply = request(
            "rift.players.kick-json",
            json!({"target": "Player", "reason": screen}),
        )
        .await
        .unwrap();
        assert_eq!(reply, json!({"ok": true, "data": true}));
        assert_eq!(
            receive(&mut fixture.client, fixture.codec).await,
            rift::protocol::disconnect_component(
                Some(version),
                rift::protocol::State::Play,
                &screen
            )
            .unwrap()
        );
        fixture.task.await.unwrap().unwrap();
        // The player is gone; the action still answers, with `false`.
        let reply = request(
            "rift.players.message-json",
            json!({"target": "Player", "text": "gone"}),
        )
        .await
        .unwrap();
        assert_eq!(reply, json!({"ok": true, "data": false}));

        let records = audit.audits();
        let actions: Vec<_> = records["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| {
                (
                    record["actor"].as_str().unwrap().to_owned(),
                    record["action"].as_str().unwrap().to_owned(),
                    record["target"].as_str().unwrap().to_owned(),
                    record["outcome"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let has = |action: &str| {
            actions.iter().any(|(actor, recorded, target, outcome)| {
                actor == "bus:relaxoban"
                    && recorded == action
                    && target == "Player"
                    && outcome == "ok"
            })
        };
        assert!(has("players.message"), "{actions:?}");
        assert!(has("players.kick"), "{actions:?}");
        // A refused publication never reaches Rift's handlers.
        assert!(
            !actions
                .iter()
                .any(|(_, action, ..)| action.starts_with("control."))
        );
        drop(current);
        server.shutdown().await;
        for service in services {
            service.abort();
        }
    })
    .await
    .unwrap();
}
