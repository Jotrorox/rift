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

/// Serve `config` through the production accept loop on an ephemeral port.
async fn accept_loop(
    config: Config,
) -> (
    std::net::SocketAddr,
    Arc<metrics::Metrics>,
    tokio::sync::watch::Sender<bool>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut snapshot = runtime::Snapshot::new(config, None).unwrap();
    snapshot.addresses = Arc::new(vec![address]);
    let (_current, current) = tokio::sync::watch::channel(Arc::new(snapshot));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let metrics = Arc::new(metrics::Metrics::default());
    tokio::spawn(runtime::accept(
        listener,
        "default".into(),
        current,
        stopped,
        metrics.clone(),
        Default::default(),
    ));
    (address, metrics, stop)
}

/// Request status after `header` and return whether the proxy answered.
async fn status_after(address: std::net::SocketAddr, header: &[u8]) -> bool {
    let mut client = TcpStream::connect(address).await.unwrap();
    let codec = Codec::default();
    client.write_all(header).await.unwrap();
    codec
        .write(&mut client, &handshake(NextState::Status, -1))
        .await
        .unwrap();
    codec.write(&mut client, &Packet::empty(0)).await.unwrap();
    matches!(Reader::default().read(&mut client, codec).await, Ok(Some(response)) if response.id == 0)
}

#[tokio::test]
async fn proxy_protocol_reports_the_client_address_and_rejects_untrusted_sources() {
    timeout(Duration::from_secs(10), async {
        // The hook only admits the client named in the PROXY header, so a
        // status response proves both the address and an intact handshake.
        let source = |trusted: &str| {
            Config::from_lua(
                &format!(
                    "return {{ listeners = {{ default = '127.0.0.1:0' }},
                    backends = {{ target = '127.0.0.1:1' }}, routes = {{ default = 'target' }},
                    proxy_protocol = {{ trusted_proxies = {{ '{trusted}' }} }},
                    on_route = function(c)
                        if c.peer_ip ~= '203.0.113.7' then return {{ reject = true }} end
                    end }}"
                ),
                "proxy.lua",
            )
            .unwrap()
        };
        let (address, metrics, _stop) = accept_loop(source("127.0.0.0/8")).await;
        assert!(status_after(address, b"PROXY TCP4 203.0.113.7 192.0.2.1 51234 25565\r\n").await);
        let mut v2 = b"\r\n\r\n\0\r\nQUIT\n\x21\x11\x00\x0c".to_vec();
        v2.extend([203, 0, 113, 7, 192, 0, 2, 1, 0xc8, 0x22, 0x63, 0xdd]);
        assert!(status_after(address, &v2).await);
        assert!(
            !status_after(
                address,
                b"PROXY TCP4 198.51.100.1 192.0.2.1 51234 25565\r\n"
            )
            .await
        );
        assert_eq!(metrics.route_rejected.get(), 1);
        // A client that reaches the listener directly sends no header.
        assert!(!status_after(address, b"").await);
        assert_eq!(metrics.proxy_rejected.get(), 1);

        let (address, metrics, _stop) = accept_loop(source("192.0.2.0/24")).await;
        assert!(!status_after(address, b"PROXY TCP4 203.0.113.7 192.0.2.1 51234 25565\r\n").await);
        assert_eq!(metrics.proxy_rejected.get(), 1);
        assert_eq!(metrics.route_rejected.get(), 0);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn proxy_protocol_rate_limits_clients_instead_of_the_load_balancer() {
    timeout(Duration::from_secs(10), async {
        let config = Config::from_lua(
            "return { listeners = { default = '127.0.0.1:0' },
            backends = { target = '127.0.0.1:1' }, routes = { default = 'target' },
            proxy_protocol = { trusted_proxies = { '127.0.0.1', '::1' } },
            rate_limit = { per_ip_per_second = 1, per_ip_burst = 1 } }",
            "proxy.lua",
        )
        .unwrap();
        let (address, metrics, _stop) = accept_loop(config).await;
        for client in ["203.0.113.7", "203.0.113.8"] {
            let header = format!("PROXY TCP4 {client} 192.0.2.1 51234 25565\r\n");
            assert!(status_after(address, header.as_bytes()).await);
        }
        assert!(!status_after(address, b"PROXY TCP4 203.0.113.7 192.0.2.1 51235 25565\r\n").await);
        assert_eq!(metrics.rate_rejected.get(), 1);
    })
    .await
    .unwrap();
}

async fn ping_json(config: Config, host: &str) -> serde_json::Value {
    let (mut client, proxy) = runtime_connection(config).await;
    let codec = Codec::default();
    let handshake = Handshake {
        protocol: 774,
        address: host.into(),
        port: 25565,
        next_state: NextState::Status,
    };
    codec.write(&mut client, &handshake.packet()).await.unwrap();
    codec.write(&mut client, &Packet::empty(0)).await.unwrap();
    client.shutdown().await.unwrap();
    let response = Reader::default()
        .read(&mut client, codec)
        .await
        .unwrap()
        .unwrap();
    let _ = proxy.await.unwrap();
    serde_json::from_str(read_string(&mut response.data.as_slice(), 32767).unwrap()).unwrap()
}

fn server_list_config(backend: &str, routes: &str, extra: &str) -> Config {
    Config::from_lua(
        &format!(
            "rift.config.listeners.default = '127.0.0.1:0'
            rift.config.backends.lobby = '{backend}'
            rift.config.routes.default = {routes}
            {extra}"
        ),
        "status.lua",
    )
    .unwrap()
}

#[tokio::test]
async fn server_list_answers_per_hostname_without_contacting_the_backend() {
    timeout(Duration::from_secs(5), async {
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = backend.local_addr().unwrap().to_string();
        let list = "rift.config.server_list = {
            motd = { text = 'Network', color = 'gold' },
            maintenance_motd = 'Back soon',
            version = 'Rift Network',
            max_players = 500,
            sample = { 'Hello' },
            hosts = { ['*.play.test'] = { motd = 'Europe', max_players = 50 } },
        }";
        let mut config = server_list_config(&address, "'lobby'", list);
        let json = ping_json(config.clone(), "play.test").await;
        assert_eq!(
            json["description"],
            serde_json::json!({ "text": "Network", "color": "gold" })
        );
        assert_eq!(
            json["version"],
            serde_json::json!({ "name": "Rift Network", "protocol": 774 })
        );
        assert_eq!(json["players"]["max"], 500);
        assert_eq!(json["players"]["online"], 0);
        assert_eq!(json["players"]["sample"][0]["name"], "Hello");
        let json = ping_json(config.clone(), "EU.Play.Test.").await;
        assert_eq!(json["description"]["text"], "Europe");
        assert_eq!(json["players"]["max"], 50);
        assert_eq!(json["version"]["name"], "Rift Network");
        config.maintenance = true;
        let json = ping_json(config, "play.test").await;
        assert_eq!(json["description"]["text"], "Back soon");
        assert!(
            timeout(Duration::from_millis(50), backend.accept())
                .await
                .is_err()
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn status_hook_overrides_fields_and_falls_back_on_nil_or_failure() {
    timeout(Duration::from_secs(5), async {
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = backend.local_addr().unwrap().to_string();
        let hook = "rift.config.server_list = { motd = 'Static', version = 'Net' }
        rift.on('status', function(ping)
            if ping.hostname == 'nil.test' then return nil end
            if ping.hostname == 'error.test' then error('boom') end
            if ping.hostname == 'bad.test' then return { colour = 'red' } end
            return {
                motd = table.concat({ ping.listener, ping.hostname, tostring(ping.backend),
                    tostring(ping.protocol), tostring(ping.maintenance), ping.peer_ip }, ' '),
                online = ping.online + 7,
                max_players = ping.max_players + 1,
                protocol = -1,
                version = 'Maintenance',
                sample = { 'a', 'b' },
            }
        end)";
        let routes = "{ ['*.test'] = 'lobby' }";
        let config = server_list_config(&address, routes, hook);
        let json = ping_json(config.clone(), "play.test").await;
        assert_eq!(
            json["description"]["text"],
            "default play.test lobby 774 false 127.0.0.1"
        );
        assert_eq!(json["players"]["online"], 7);
        assert_eq!(json["players"]["max"], 1025);
        assert_eq!(
            json["version"],
            serde_json::json!({ "name": "Maintenance", "protocol": -1 })
        );
        assert_eq!(json["players"]["sample"].as_array().unwrap().len(), 2);
        for host in ["nil.test", "error.test", "bad.test"] {
            let json = ping_json(config.clone(), host).await;
            assert_eq!(json["description"]["text"], "Static", "{host}");
            assert_eq!(json["version"]["name"], "Net", "{host}");
        }
        // Unmatched hostnames skip server_list, but the hook still sees them.
        let json = ping_json(config.clone(), "unknown.example").await;
        assert_eq!(
            json["description"]["text"],
            "default unknown.example nil 774 false 127.0.0.1"
        );
        let config = server_list_config(
            &address,
            routes,
            "rift.config.server_list = { motd = 'Static' }",
        );
        let json = ping_json(config, "unknown.example").await;
        assert_eq!(
            json["description"]["text"],
            "No server is configured for this hostname."
        );
        assert!(
            timeout(Duration::from_millis(50), backend.accept())
                .await
                .is_err()
        );
    })
    .await
    .unwrap();
}
