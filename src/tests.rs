use super::*;
use rift::protocol::{Codec, Handshake, NextState, Packet, Reader, read_string};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

#[test]
fn lua_timeout_and_overload_retain_distinct_connection_diagnostics() {
    // Queue Lua work behind one occupied worker. Timed-out jobs retain their
    // permits, so overload is deterministic without a race between clients.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (ready, started) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    let blocker = runtime.spawn_blocking(move || {
        ready.send(()).unwrap();
        held.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    runtime.block_on(async {
        let config = Config::from_lua(
            "return { listeners = { public = '127.0.0.1:0' },
            backends = { target = '127.0.0.1:1' }, routes = { public = 'target' },
            on_route = function() return nil end }",
            "overload.lua",
        )
        .unwrap();
        let snapshot = Arc::new(runtime::Snapshot::new(config, None).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metrics = Arc::new(metrics::Metrics::default());
        for expected in [
            "script_timeout",
            "script_timeout",
            "script_timeout",
            "script_timeout",
            "lua_overload",
        ] {
            let _client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (mut accepted, peer) = listener.accept().await.unwrap();
            let mut event = events::Connection::new("public", peer);
            assert!(
                runtime::handle(
                    &mut accepted,
                    "public",
                    snapshot.clone(),
                    &[],
                    metrics.clone(),
                    &mut event
                )
                .await
                .is_err()
            );
            assert_eq!(event.stage, "on_route");
            assert_eq!(event.failure, expected);
            assert!(event.backend.is_none());
        }
        assert_eq!(metrics.route_rejected.get(), 5);
        release.send(()).unwrap();
        blocker.await.unwrap();
    });
}

async fn runtime_connection(
    config: Config,
) -> (TcpStream, tokio::task::JoinHandle<io::Result<(u64, u64)>>) {
    let frontend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = frontend.local_addr().unwrap();
    let client = TcpStream::connect(address).await.unwrap();
    let (mut accepted, peer) = frontend.accept().await.unwrap();
    let snapshot = Arc::new(runtime::Snapshot::new(config, None).unwrap());
    let proxy = tokio::spawn(async move {
        runtime::handle(
            &mut accepted,
            "default",
            snapshot,
            &[address],
            Arc::new(metrics::Metrics::default()),
            &mut events::Connection::new("default", peer),
        )
        .await
    });
    (client, proxy)
}

fn handshake(next_state: NextState, protocol: i32) -> Packet {
    Handshake {
        protocol,
        address: "play.test".into(),
        port: 25565,
        next_state,
    }
    .packet()
}

#[tokio::test]
async fn all_routing_modes_answer_status_locally_without_contacting_a_backend() {
    timeout(Duration::from_secs(5), async {
        for mode in ["direct", "hostname", "lua"] {
            let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut config = Config::from_addresses("127.0.0.1:0", &backend.local_addr().unwrap().to_string()).unwrap();
            if mode == "hostname" {
                config.routes.insert("default".into(), Route::Hostnames(BTreeMap::from([("play.test".into(), "default".into())])));
            } else if mode == "lua" {
                config = Config::from_lua(&format!("return {{listeners={{default='127.0.0.1:0'}},backends={{target='{}'}},routes={{default='target'}},on_route=function() return {{backend='target'}} end}}", backend.local_addr().unwrap()), "test.lua").unwrap();
            }
            let (mut client, proxy) = runtime_connection(config).await;
            let codec = Codec::default();
            codec.write(&mut client, &handshake(NextState::Status, -1)).await.unwrap();
            codec.write(&mut client, &Packet::empty(0)).await.unwrap();
            let ping = Packet::new(1, i64::MIN.to_be_bytes().to_vec());
            codec.write(&mut client, &ping).await.unwrap();
            let response = Reader::default().read(&mut client, codec).await.unwrap().unwrap();
            assert_eq!(response.id, 0);
            let json: serde_json::Value = serde_json::from_str(read_string(&mut response.data.as_slice(), 32767).unwrap()).unwrap();
            assert_eq!(json["description"]["text"], "Rift");
            assert_eq!(Reader::default().read(&mut client, codec).await.unwrap(), Some(ping));
            proxy.await.unwrap().unwrap();
            assert!(timeout(Duration::from_millis(50), backend.accept()).await.is_err());
        }
    }).await.unwrap();
}

#[tokio::test]
async fn unavailable_backend_returns_a_login_disconnect_and_status_stays_available() {
    timeout(Duration::from_secs(5), async {
        let reserved = tokio::net::TcpSocket::new_v4().unwrap();
        reserved.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let mut config =
            Config::from_addresses("127.0.0.1:0", &reserved.local_addr().unwrap().to_string())
                .unwrap();
        config.limits.connect_timeout = Duration::from_millis(100);
        let (mut client, proxy) = runtime_connection(config.clone()).await;
        let codec = Codec::default();
        codec
            .write(&mut client, &handshake(NextState::Login, 774))
            .await
            .unwrap();
        let packet = Reader::default()
            .read(&mut client, codec)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(packet.id, 0);
        assert!(
            read_string(&mut packet.data.as_slice(), 32767)
                .unwrap()
                .contains("unavailable")
        );
        assert!(proxy.await.unwrap().is_err());
        config.status_cache = Some(rift::config::StatusCache {
            ttl: Duration::from_secs(1),
            max_entries: 8,
            max_response_bytes: 65536,
        });
        let (mut client, proxy) = runtime_connection(config).await;
        codec
            .write(&mut client, &handshake(NextState::Status, 774))
            .await
            .unwrap();
        codec.write(&mut client, &Packet::empty(0)).await.unwrap();
        client.shutdown().await.unwrap();
        let packet = Reader::default()
            .read(&mut client, codec)
            .await
            .unwrap()
            .unwrap();
        assert!(
            read_string(&mut packet.data.as_slice(), 32767)
                .unwrap()
                .contains("unavailable")
        );
        proxy.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn unsupported_version_gets_a_disconnect_before_any_backend_connection() {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config =
        Config::from_addresses("127.0.0.1:0", &backend.local_addr().unwrap().to_string()).unwrap();
    let (mut client, proxy) = runtime_connection(config).await;
    Codec::default()
        .write(&mut client, &handshake(NextState::Login, 9999))
        .await
        .unwrap();
    let packet = Reader::default()
        .read(&mut client, Codec::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(packet.id, 0);
    assert!(
        read_string(&mut packet.data.as_slice(), 32767)
            .unwrap()
            .contains("Unsupported Minecraft version")
    );
    assert!(proxy.await.unwrap().is_err());
    assert!(
        timeout(Duration::from_millis(50), backend.accept())
            .await
            .is_err()
    );
}
