use super::*;
use crate::messaging::{BrokerConfig, stream::StreamConfig};
use bytes::Bytes;
use rustls::pki_types::PrivateKeyDer;

fn identity() -> Identity {
    Identity {
        name: "plugin".into(),
        token: "test-only-long-random-token".into(),
        publish: vec!["game.>".into(), "_INBOX.>".into()],
        subscribe: vec!["game.>".into(), "_INBOX.>".into()],
        control: false,
        players: false,
    }
}
fn configs() -> (quinn::ServerConfig, quinn::ClientConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let server = server_config_from_der(
        vec![cert.cert.der().clone()],
        PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
    )
    .unwrap();
    let client = client_config_from_der(vec![cert.cert.der().clone()]).unwrap();
    (server, client)
}
async fn fixture(streams: HashMap<String, Stream>) -> (Server, Client, Broker) {
    let (server_tls, client_tls) = configs();
    let broker = Broker::new(BrokerConfig::default()).unwrap();
    let auth = AuthConfig {
        identities: vec![identity()],
        ..Default::default()
    };
    let server = Server::bind_with_streams(
        "127.0.0.1:0".parse().unwrap(),
        server_tls,
        broker.clone(),
        auth,
        streams,
    )
    .unwrap();
    let client = Client::connect(
        server.local_addr().unwrap(),
        "localhost",
        client_tls,
        &identity().token,
    )
    .await
    .unwrap();
    (server, client, broker)
}

#[test]
fn wildcard_permissions_require_containment_not_overlap() {
    assert!(covers("game.>", "game.region.*"));
    assert!(!covers("game.region.*", "game.>"));
    assert!(!covers("game.*", "game.>"));
    assert!(!covers("game.>", "game"));
    assert!(covers(">", "game.>"));
    assert!(!covers("game.private", "game.*"));
    assert!(overlaps("*.control.>", "rift.control.>"));
    let mut peer = identity();
    peer.publish = vec![">".into()];
    peer.subscribe = vec![">".into()];
    assert!(peer.can_subscribe(">").is_err());
    assert!(peer.can_subscribe("rift.*").is_err());
    assert!(peer.can_subscribe("rift.control").is_err());
    assert!(peer.can_subscribe("game.>").is_ok());
    assert!(peer.can_publish("rift.control", None).is_err());
    assert!(peer.can_publish("rift.control.reload", None).is_err());
    assert!(peer.can_publish("rift.events.player", None).is_err());
    assert!(
        peer.can_publish("game.request", Some("rift.control.reload"))
            .is_err()
    );
    peer.control = true;
    assert!(peer.can_subscribe(">").is_ok());
    assert!(peer.can_publish("rift.control.reload", None).is_ok());
    assert!(peer.can_publish("rift.events.player", None).is_err());
}

#[test]
fn the_player_api_needs_its_own_grant_and_only_control_may_answer_it() {
    let mut peer = identity();
    peer.publish = vec![
        "rift.players.*".into(),
        "rift.control.*".into(),
        "_INBOX.>".into(),
    ];
    peer.subscribe = vec![">".into()];
    // A publish grant alone is not enough, as for `rift.control`.
    assert!(peer.can_publish("rift.players.kick", None).is_err());
    peer.players = true;
    assert!(
        peer.can_publish("rift.players.kick", Some("_INBOX.a"))
            .is_ok()
    );
    assert!(
        peer.can_publish("rift.players.kick", Some("game.x"))
            .is_err()
    );
    // `players` is not `control`.
    assert!(peer.can_publish("rift.control.stop", None).is_err());
    // Receiving player requests would expose every caller's actions and
    // let the subscriber answer in Rift's place.
    for pattern in [
        "rift.players.kick",
        "rift.players.*",
        "rift.players.>",
        "rift.*.kick",
        "rift.players",
    ] {
        assert!(peer.can_subscribe(pattern).is_err(), "{pattern}");
    }
    peer.control = true;
    assert!(peer.can_subscribe("rift.players.*").is_ok());
    peer.players = false;
    assert!(peer.can_publish("rift.players.kick", None).is_err());
}

#[test]
fn only_the_proxy_publishes_permission_updates_or_reads_requests() {
    let mut peer = identity();
    peer.publish = vec!["rift.perms.>".into()];
    peer.subscribe = vec!["rift.perms.>".into()];
    for subject in [
        "rift.perms.update",
        "rift.perms.update.0123456789abcdef0123456789abcdef",
        "rift.perms.update.resync",
        "rift.perms.changes",
    ] {
        assert!(peer.can_publish(subject, None).is_err(), "{subject}");
    }
    assert!(peer.can_publish("rift.perms.hello", None).is_ok());
    assert!(peer.can_subscribe("rift.perms.update.>").is_ok());
    assert!(peer.can_subscribe("rift.perms.update").is_ok());
    for pattern in [
        "rift.perms.>",
        "rift.perms.*",
        "rift.perms.subject.*",
        "rift.perms.hello",
    ] {
        assert!(peer.can_subscribe(pattern).is_err(), "{pattern}");
    }
    peer.control = true;
    assert!(peer.can_subscribe("rift.perms.>").is_ok());
    assert!(peer.can_publish("rift.perms.update", None).is_err());
}

#[tokio::test]
async fn remote_messages_name_their_principal() {
    let (_server, client, broker) = fixture(HashMap::new()).await;
    let mut local = broker.subscribe("game.sender", None).unwrap();
    client
        .publish("game.sender", Bytes::from_static(b"remote"))
        .await
        .unwrap();
    let message = timeout(Duration::from_secs(2), local.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.sender.as_deref(), Some("plugin"));
    broker
        .publish("game.sender", Bytes::from_static(b"local"))
        .unwrap();
    let message = local.recv().await.unwrap();
    assert_eq!(message.sender, None);
}

#[tokio::test]
async fn real_quic_pubsub_pipeline_and_cancellation() {
    let (server, client, _broker) = fixture(HashMap::new()).await;
    let mut sub = client.subscribe("game.position.*", None).await.unwrap();
    // Cancelling a read while no frame is present does not corrupt framing.
    assert!(timeout(Duration::from_millis(5), sub.recv()).await.is_err());
    let mut publisher = client.publisher().await.unwrap();
    for index in 0..200u32 {
        publisher
            .publish(
                "game.position.42",
                None,
                Bytes::copy_from_slice(&index.to_be_bytes()),
            )
            .await
            .unwrap();
    }
    publisher.flush().await.unwrap();
    for index in 0..200u32 {
        let message = timeout(Duration::from_secs(2), sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&*message.subject, "game.position.42");
        assert_eq!(message.payload, index.to_be_bytes().as_slice());
    }
    client.close();
    server.shutdown().await;
}

#[tokio::test]
async fn real_quic_request_reply_and_queue_groups() {
    let (server, client, _broker) = fixture(HashMap::new()).await;
    let mut worker = client
        .subscribe("game.echo", Some("workers"))
        .await
        .unwrap();
    let response_client = client.clone();
    let task = tokio::spawn(async move {
        let request = worker.recv().await.unwrap();
        response_client
            .publish(request.reply.as_deref().unwrap(), request.payload)
            .await
            .unwrap();
    });
    let reply = client
        .request(
            "game.echo",
            Bytes::from_static(b"hello"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert_eq!(reply.payload, b"hello".as_slice());
    task.await.unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn authentication_and_server_certificate_are_verified() {
    let (server_tls, client_tls) = configs();
    let broker = Broker::new(BrokerConfig::default()).unwrap();
    let server = Server::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_tls,
        broker,
        AuthConfig {
            identities: vec![identity()],
            ..Default::default()
        },
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    assert!(
        Client::connect(addr, "localhost", client_tls.clone(), "incorrect-token")
            .await
            .is_err()
    );
    assert!(
        Client::connect(
            addr,
            "wrong-host.example",
            client_tls.clone(),
            &identity().token
        )
        .await
        .is_err()
    );
    let (_, untrusted_tls) = configs();
    assert!(
        Client::connect(addr, "localhost", untrusted_tls, &identity().token)
            .await
            .is_err()
    );
    let client = Client::connect(addr, "localhost", client_tls, &identity().token)
        .await
        .unwrap();
    assert!(client.subscribe(">", None).await.is_err());
    assert!(client.subscribe("game.*.>", None).await.is_ok());
    assert!(
        client
            .publish("rift.control.reload", Bytes::new())
            .await
            .is_err()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn malformed_frames_are_rejected_without_killing_other_streams() {
    let (server, client, _broker) = fixture(HashMap::new()).await;
    for malformed in [
        vec![0, 0, 0, 0],
        ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes().to_vec(),
        vec![0, 0, 0, 1, 255],
        vec![0, 0, 0, 3, SUBSCRIBE, 0, 30],
    ] {
        let (mut send, mut recv) = client.connection().open_bi().await.unwrap();
        send.write_all(&malformed).await.unwrap();
        let mut frame = timeout(
            Duration::from_secs(2),
            FrameReader::default().read(&mut recv),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(take_u8(&mut frame).unwrap(), ERROR);
    }
    let mut sub = client.subscribe("game.health", None).await.unwrap();
    client
        .publish("game.health", Bytes::from_static(b"alive"))
        .await
        .unwrap();
    assert_eq!(sub.recv().await.unwrap().payload, b"alive".as_slice());
    server.shutdown().await;
}

#[tokio::test]
async fn retained_consumer_replays_and_acknowledges_over_quic() {
    let stream = Stream::memory(StreamConfig {
        subjects: vec!["game.>".into()],
        ..Default::default()
    })
    .unwrap();
    let (server, client, _) = fixture(HashMap::from([("events".into(), stream)])).await;
    let first = client
        .stream_publish("events", "game.score", None, Bytes::from_static(b"one"))
        .await
        .unwrap();
    let second = client
        .stream_publish("events", "game.score", None, Bytes::from_static(b"two"))
        .await
        .unwrap();
    assert_eq!(second.sequence, first.sequence + 1);
    client
        .consumer(
            "events",
            "scoreboard",
            ConsumerConfig {
                filter_subject: "game.score".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let records = client.fetch("events", "scoreboard", 10).await.unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].record.message.payload, b"one".as_slice());
    for record in records {
        client
            .ack("events", "scoreboard", record.record.sequence)
            .await
            .unwrap();
    }
    assert!(
        client
            .fetch("events", "scoreboard", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        client
            .consumer("events", "forbidden", ConsumerConfig::default())
            .await
            .is_err()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn dropping_server_closes_existing_connections() {
    let (server, client, _) = fixture(HashMap::new()).await;
    drop(server);
    timeout(Duration::from_secs(2), client.connection().closed())
        .await
        .unwrap();
}

#[tokio::test]
async fn dropped_clients_do_not_leave_a_publisher_reference_cycle() {
    let (_server, client, _) = fixture(HashMap::new()).await;
    client.publish("game.cycle", Bytes::new()).await.unwrap();
    // The connection closes only when the last reference to the client drops.
    let connection = client.connection().clone();
    drop(client);
    timeout(Duration::from_secs(2), connection.closed())
        .await
        .unwrap();
}

#[tokio::test]
async fn dropping_remote_subscription_unregisters_the_broker_subscription() {
    let (server, client, broker) = fixture(HashMap::new()).await;
    let baseline = broker.subscription_count();
    let sub = client.subscribe("game.lifecycle", None).await.unwrap();
    assert_eq!(broker.subscription_count(), baseline + 1);
    drop(sub);
    timeout(Duration::from_secs(2), async {
        while broker.subscription_count() != baseline {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn maximum_payload_round_trips_and_oversized_publish_fails_locally() {
    let (server, client, _) = fixture(HashMap::new()).await;
    let mut subscription = client.subscribe("game.large", None).await.unwrap();
    let payload = Bytes::from((0..MAX_PAYLOAD_BYTES).map(|i| i as u8).collect::<Vec<_>>());
    client.publish("game.large", payload.clone()).await.unwrap();
    let received = timeout(Duration::from_secs(5), subscription.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received.payload, payload);
    assert!(
        client
            .publish("game.large", Bytes::from(vec![0; MAX_PAYLOAD_BYTES + 1]))
            .await
            .is_err()
    );
    client
        .publish("game.large", Bytes::from_static(b"still usable"))
        .await
        .unwrap();
    assert_eq!(
        subscription.recv().await.unwrap().payload,
        b"still usable".as_slice()
    );
    server.shutdown().await;
}

#[tokio::test]
async fn remote_consumer_names_are_isolated_by_authenticated_identity() {
    let (server_tls, client_tls) = configs();
    let mut second_identity = identity();
    second_identity.name = "other_plugin".into();
    second_identity.token = "second-test-token".into();
    let stream = Stream::memory(StreamConfig {
        subjects: vec!["game.>".into()],
        ..Default::default()
    })
    .unwrap();
    let server = Server::bind_with_streams(
        "127.0.0.1:0".parse().unwrap(),
        server_tls,
        Broker::default(),
        AuthConfig {
            identities: vec![identity(), second_identity.clone()],
            ..Default::default()
        },
        HashMap::from([("events".into(), stream)]),
    )
    .unwrap();
    let first = Client::connect(
        server.local_addr().unwrap(),
        "localhost",
        client_tls.clone(),
        &identity().token,
    )
    .await
    .unwrap();
    let second = Client::connect(
        server.local_addr().unwrap(),
        "localhost",
        client_tls,
        &second_identity.token,
    )
    .await
    .unwrap();
    let record = first
        .stream_publish("events", "game.score", None, Bytes::from_static(b"one"))
        .await
        .unwrap();
    let config = ConsumerConfig {
        filter_subject: "game.score".into(),
        ..Default::default()
    };
    first
        .consumer("events", "worker", config.clone())
        .await
        .unwrap();
    assert_eq!(first.fetch("events", "worker", 1).await.unwrap().len(), 1);
    assert!(second.fetch("events", "worker", 1).await.is_err());
    assert!(
        second
            .ack("events", "worker", record.sequence)
            .await
            .is_err()
    );
    first
        .ack("events", "worker", record.sequence)
        .await
        .unwrap();
    second.consumer("events", "worker", config).await.unwrap();
    assert_eq!(second.fetch("events", "worker", 1).await.unwrap().len(), 1);
    assert!(first.fetch("events", "worker", 1).await.unwrap().is_empty());
    server.shutdown().await;
}

#[tokio::test]
async fn plugin_connections_are_capped_per_identity_and_never_crowd_out_nodes() {
    let (server_tls, client_tls) = configs();
    let other = Identity {
        name: "other".into(),
        token: "other-test-only-long-random-token".into(),
        ..identity()
    };
    let node = Identity {
        name: "node.node1".into(),
        token: "node-test-only-long-random-token".into(),
        publish: vec![],
        subscribe: vec![],
        control: false,
        players: false,
    };
    let server = Server::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_tls,
        Broker::default(),
        AuthConfig {
            identities: vec![identity(), other.clone(), node.clone()],
            max_connections: 2,
            max_connections_per_identity: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    let connect = |token: String| {
        let client_tls = client_tls.clone();
        async move { Client::connect(addr, "localhost", client_tls, &token).await }
    };
    let first = connect(identity().token).await.unwrap();
    let error = connect(identity().token).await.err().unwrap();
    assert!(
        error
            .to_string()
            .contains("too many connections for this identity"),
        "{error}"
    );
    let _other = connect(other.token.clone()).await.unwrap();
    // Both plugin slots are taken; a node agent still connects.
    let _node = connect(node.token.clone()).await.unwrap();
    drop(first);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while connect(identity().token).await.is_err() {
        assert!(tokio::time::Instant::now() < deadline, "slot not released");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    server.shutdown().await;
}

#[tokio::test]
async fn connections_are_capped_per_address() {
    let (server_tls, client_tls) = configs();
    let server = Server::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_tls,
        Broker::default(),
        AuthConfig {
            identities: vec![identity()],
            max_connections_per_address: 2,
            ..Default::default()
        },
    )
    .unwrap();
    let addr = server.local_addr().unwrap();
    let token = identity().token;
    let connect = || Client::connect(addr, "localhost", client_tls.clone(), &token);
    let _first = connect().await.unwrap();
    let _second = connect().await.unwrap();
    assert!(connect().await.is_err());
    server.shutdown().await;
}

#[test]
fn ipv6_hosts_are_counted_per_64() {
    let key = |ip: &str| address_key(ip.parse().unwrap());
    assert_eq!(key("2001:db8:1:2:aaaa::1"), key("2001:db8:1:2:bbbb::2"));
    assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
    assert_eq!(key("::ffff:192.0.2.7"), key("192.0.2.7"));
    assert_ne!(key("192.0.2.7"), key("192.0.2.8"));
}

#[test]
fn pem_helpers_preserve_certificate_chains_and_report_file_errors() {
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let mut random = [0u8; 8];
    aws_lc_rs::rand::fill(&mut random).unwrap();
    let directory = Directory(
        std::env::temp_dir().join(format!("rift-quic-pem-{:016x}", u64::from_be_bytes(random))),
    );
    std::fs::create_dir(&directory.0).unwrap();
    let certificate = directory.0.join("chain.pem");
    let private_key = directory.0.join("key.pem");
    let first = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let second = rcgen::generate_simple_self_signed(vec!["other.localhost".into()]).unwrap();
    std::fs::write(
        &certificate,
        format!("{}{}", first.cert.pem(), second.cert.pem()),
    )
    .unwrap();
    std::fs::write(&private_key, first.signing_key.serialize_pem()).unwrap();
    assert_eq!(
        read_certificates(&certificate).unwrap(),
        vec![first.cert.der().clone(), second.cert.der().clone()]
    );
    assert!(tls_server_config(&certificate, &private_key).is_ok());
    assert!(tls_client_config(&certificate).is_ok());
    assert_eq!(
        read_certificates(&directory.0.join("missing.pem"))
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    std::fs::write(&private_key, "").unwrap();
    let error = tls_server_config(&certificate, &private_key).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("private key PEM"));
    std::fs::write(
        &certificate,
        "-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    assert_eq!(
        read_certificates(&certificate).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    std::fs::write(&certificate, "").unwrap();
    assert_eq!(
        read_certificates(&certificate).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

/// The first datagram the server sends to a new peer is a stateless Retry,
/// so a spoofed Initial never reaches connection admission or holds a slot.
#[tokio::test]
async fn unvalidated_peers_get_a_retry_before_any_handshake() {
    use std::sync::Mutex;
    use tokio::net::UdpSocket;

    let (server_tls, client_tls) = configs();
    let server = Server::bind(
        "127.0.0.1:0".parse().unwrap(),
        server_tls,
        Broker::default(),
        AuthConfig {
            identities: vec![identity()],
            ..Default::default()
        },
    )
    .unwrap();
    // Relay client <-> server and record the long-header packet type of every
    // server datagram (QUIC v1, RFC 9000 17.2: 0 Initial, 2 Handshake, 3 Retry).
    let relay = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let upstream = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    upstream
        .connect(server.local_addr().unwrap())
        .await
        .unwrap();
    let peer = Arc::new(Mutex::new(None));
    let types = Arc::new(Mutex::new(Vec::new()));
    let forward = {
        let (relay, upstream, peer) = (relay.clone(), upstream.clone(), peer.clone());
        tokio::spawn(async move {
            let mut buffer = vec![0; 65536];
            loop {
                let (length, from) = relay.recv_from(&mut buffer).await.unwrap();
                *peer.lock().unwrap() = Some(from);
                let _ = upstream.send(&buffer[..length]).await;
            }
        })
    };
    let backward = {
        let (relay, upstream, peer, types) = (relay.clone(), upstream, peer, types.clone());
        tokio::spawn(async move {
            let mut buffer = vec![0; 65536];
            loop {
                let length = upstream.recv(&mut buffer).await.unwrap();
                if buffer[0] & 0x80 != 0 && buffer[1..5] == [0, 0, 0, 1] {
                    types.lock().unwrap().push((buffer[0] >> 4) & 0x03);
                }
                let to = peer.lock().unwrap().unwrap();
                let _ = relay.send_to(&buffer[..length], to).await;
            }
        })
    };
    let client = Client::connect(
        relay.local_addr().unwrap(),
        "localhost",
        client_tls,
        &identity().token,
    )
    .await
    .unwrap();
    let types = types.lock().unwrap().clone();
    assert_eq!(types.first(), Some(&3), "server packet types: {types:?}");
    assert_eq!(types.iter().filter(|&&kind| kind == 3).count(), 1);
    assert!(
        types.contains(&0),
        "the handshake continues after the Retry"
    );
    drop(client);
    forward.abort();
    backward.abort();
    server.shutdown().await;
}
