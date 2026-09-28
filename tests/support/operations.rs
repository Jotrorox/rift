use super::*;

fn options(source: &str, extra: &str) -> String {
    source.replacen("return {", &format!("return {{ {extra},"), 1)
}

impl Process {
    fn message(&self, expected: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect(expected);
            if line.contains(expected) {
                return line;
            }
        }
    }

    fn metrics_address(&self) -> SocketAddr {
        self.message("rift: metrics on ")
            .strip_prefix("rift: metrics on ")
            .unwrap()
            .parse()
            .unwrap()
    }

    #[cfg(unix)]
    fn signal(&self, signal: &str) {
        assert!(
            Command::new("kill")
                .args([signal, &self.child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
    }

    #[cfg(unix)]
    fn exited(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            assert!(Instant::now() < deadline, "proxy did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn scrape(address: SocketAddr) -> String {
    let mut client = connect(address);
    client
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    response
}

fn metric(address: SocketAddr, name: &str) -> u64 {
    let text = scrape(address);
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("rift_{name} ")))
        .expect(&text)
        .parse()
        .unwrap()
}

fn await_metric(address: SocketAddr, name: &str, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if metric(address, name) == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "metric {name} did not reach {expected}: {}",
            scrape(address)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn exchange(client: &mut TcpStream, server: &mut TcpStream) {
    client.write_all(b"hello").unwrap();
    let mut bytes = [0; 5];
    server.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"hello");
    server.write_all(b"world").unwrap();
    client.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"world");
}

#[test]
fn check_validates_without_binding_or_connecting() {
    let fixture = Fixture::new();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(
        &config(&[occupied.local_addr().unwrap()], "")
            .replace(
                "l0 = '127.0.0.1:0'",
                &format!("l0 = '{}'", occupied.local_addr().unwrap()),
            )
            .replace(
                &format!("b0 = '{}'", occupied.local_addr().unwrap()),
                "b0 = '127.0.0.1:1'",
            ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_rift"))
        .current_dir(&fixture.0)
        .args(["--check", "rift.lua"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_no_connection(&occupied);
    fixture.write("return { on_route = false }");
    fixture.spawn(&["--check", "rift.lua"]).failure("rift.lua");
}

#[test]
fn fallback_reaches_backup_before_forwarding_and_does_not_migrate_sessions() {
    let fixture = Fixture::new();
    let primary = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = primary.local_addr().unwrap();
    drop(primary);
    let backup = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&options(
        &config(
            &[address, backup.local_addr().unwrap()],
            "connect_timeout_ms = 1000",
        ),
        "fallbacks = { b0 = { 'b1' } }, metrics = '127.0.0.1:0'",
    ));
    let process = fixture.spawn(&[]);
    let front = process.listener();
    process.listener();
    let metrics = process.metrics_address();
    let mut client = connect(front);
    let mut server = accept(&backup);
    exchange(&mut client, &mut server);
    assert_eq!(metric(metrics, "fallbacks_total"), 1);
    assert_eq!(metric(metrics, "backend_connect_failures_total"), 1);
    let primary = TcpListener::bind(address).unwrap();
    let mut new_client = connect(front);
    let mut new_server = accept(&primary);
    exchange(&mut new_client, &mut new_server);
    // A recovered primary does not change the already established backup relay.
    exchange(&mut client, &mut server);
    drop(new_client);
    drop(new_server);
    drop(server);
    assert_closed(&mut client);
    assert_no_connection(&primary);
    assert_no_connection(&backup);
}

#[test]
fn health_checks_detect_outage_skip_primary_and_recover() {
    let fixture = Fixture::new();
    let primary = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = primary.local_addr().unwrap();
    drop(primary);
    let backup = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&options(&config(&[address, backup.local_addr().unwrap()], "connect_timeout_ms = 1000"),
        "fallbacks = { b0 = { 'b1' } }, metrics = '127.0.0.1:0', health_check = { interval_ms = 40, timeout_ms = 100, unhealthy_threshold = 1, healthy_threshold = 1 }"));
    let process = fixture.spawn(&[]);
    let front = process.listener();
    process.listener();
    let metrics = process.metrics_address();
    await_metric(metrics, "backend_up{backend=\"b0\"}", 0);
    let mut client = connect(front);
    client.write_all(b"hello").unwrap();
    // Health probes connect and close without application bytes.
    let mut server = loop {
        let mut server = accept(&backup);
        let mut bytes = [0; 5];
        if server.read_exact(&mut bytes).is_ok() {
            assert_eq!(&bytes, b"hello");
            break server;
        }
    };
    server.write_all(b"world").unwrap();
    let mut bytes = [0; 5];
    client.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"world");
    assert_eq!(metric(metrics, "backend_connect_failures_total"), 0);
    assert_eq!(metric(metrics, "fallbacks_total"), 1);
    let primary = TcpListener::bind(address).unwrap();
    await_metric(metrics, "backend_up{backend=\"b0\"}", 1);
    let mut recovered = connect(front);
    recovered.write_all(b"again").unwrap();
    loop {
        let mut server = accept(&primary);
        if server.read_exact(&mut bytes).is_ok() {
            assert_eq!(&bytes, b"again");
            break;
        }
    }
    exchange(&mut client, &mut server);
}

#[test]
fn rate_limits_are_shared_across_listeners_and_leave_active_traffic_alive() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&options(&config(&[backend.local_addr().unwrap(); 2], ""),
        "metrics = '127.0.0.1:0', rate_limit = { per_ip_per_second = 1, per_ip_burst = 1, global_per_second = 100, global_burst = 100 }"));
    let process = fixture.spawn(&[]);
    let first = process.listener();
    let second = process.listener();
    let metrics = process.metrics_address();
    let mut client = connect(first);
    let mut server = accept(&backend);
    assert_closed(&mut connect(second));
    assert_eq!(metric(metrics, "connections_rate_limited_total"), 1);
    exchange(&mut client, &mut server);
    assert_eq!(metric(metrics, "connections_active"), 1);
    thread::sleep(Duration::from_millis(1100));
    let mut allowed = connect(second);
    let mut upstream = accept(&backend);
    exchange(&mut allowed, &mut upstream);
    assert_eq!(metric(metrics, "client_bytes_read_total"), 10);
    assert_eq!(metric(metrics, "client_bytes_written_total"), 10);
    drop(allowed);
    drop(upstream);
    drop(client);
    drop(server);
    await_metric(metrics, "connections_active", 0);
    assert_eq!(metric(metrics, "connections_completed_total"), 2);
}

#[cfg(unix)]
#[test]
fn reloads_are_atomic_validate_scripts_and_preserve_sessions_and_capacity() {
    let fixture = Fixture::new();
    let first = TcpListener::bind("127.0.0.1:0").unwrap();
    let second = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = options(
        &config(
            &[first.local_addr().unwrap(), second.local_addr().unwrap()],
            "max_connections = 4",
        ),
        "metrics = '127.0.0.1:0'",
    );
    fixture.write(&source);
    let process = fixture.spawn(&[]);
    let front = process.listener();
    process.listener();
    let metrics = process.metrics_address();
    let mut old_client = connect(front);
    let mut old_server = accept(&first);
    let updated = options(
        &source,
        "on_route = function(c) return { backend = 'b1' } end",
    );
    fixture.write(&updated);
    process.signal("-HUP");
    process.message("configuration reloaded");
    let mut new_client = connect(front);
    let mut new_server = accept(&second);
    exchange(&mut old_client, &mut old_server);
    exchange(&mut new_client, &mut new_server);
    for invalid in [
        "return {".to_owned(),
        "while true do end".to_owned(),
        updated.replace(
            "on_route = function(c) return { backend = 'b1' } end",
            "on_route = false",
        ),
        updated.replace("l0 = 'b0'", "l0 = 'missing'"),
        updated.replace("l0 = '127.0.0.1:0'", "l0 = '127.0.0.1:1'"),
        updated.replace("metrics = '127.0.0.1:0'", "metrics = '127.0.0.1:1'"),
        updated.replace(&first.local_addr().unwrap().to_string(), &front.to_string()),
    ] {
        fixture.write(&invalid);
        process.signal("-HUP");
        process.message("reload rejected");
        exchange(&mut old_client, &mut old_server);
        exchange(&mut new_client, &mut new_server);
        assert_eq!(metric(metrics, "connections_active"), 2);
    }
    assert_eq!(metric(metrics, "reloads_total"), 1);
    assert_eq!(metric(metrics, "reload_failures_total"), 7);
    fixture.write(&source.replace("max_connections = 4", "max_connections = 1"));
    process.signal("-HUP");
    process.message("configuration reloaded");
    assert_closed(&mut connect(front));
    exchange(&mut old_client, &mut old_server);
    exchange(&mut new_client, &mut new_server);
    drop(new_client);
    drop(new_server);
    await_metric(metrics, "connections_active", 1);
    assert_closed(&mut connect(front));
    drop(old_client);
    drop(old_server);
    await_metric(metrics, "connections_active", 0);
    // Removing the script returns new sessions to the original route.
    let mut restored = connect(front);
    let mut upstream = accept(&first);
    exchange(&mut restored, &mut upstream);
}

#[cfg(unix)]
#[test]
fn graceful_shutdown_stops_accepting_and_drains_live_sessions() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&options(
        &config(&[backend.local_addr().unwrap()], ""),
        "shutdown_timeout_ms = 3000",
    ));
    let mut process = fixture.spawn(&[]);
    let front = process.listener();
    let mut client = connect(front);
    let mut server = accept(&backend);
    process.signal("-TERM");
    process.message("draining 1 connections");
    let deadline = Instant::now() + Duration::from_secs(2);
    while TcpStream::connect_timeout(&front, Duration::from_millis(50)).is_ok() {
        assert!(
            Instant::now() < deadline,
            "listener still accepting during drain"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(process.child.try_wait().unwrap().is_none());
    exchange(&mut client, &mut server);
    client.shutdown(std::net::Shutdown::Write).unwrap();
    assert_closed(&mut server);
    server.write_all(b"last").unwrap();
    server.shutdown(std::net::Shutdown::Write).unwrap();
    let mut tail = Vec::new();
    client.read_to_end(&mut tail).unwrap();
    assert_eq!(tail, b"last");
    process.exited();
}

#[cfg(unix)]
#[test]
fn shutdown_deadline_and_second_signal_close_stalled_sessions() {
    for second_signal in [false, true] {
        let fixture = Fixture::new();
        let backend = TcpListener::bind("127.0.0.1:0").unwrap();
        fixture.write(&options(
            &config(&[backend.local_addr().unwrap()], ""),
            &format!(
                "shutdown_timeout_ms = {}",
                if second_signal { 30000 } else { 100 }
            ),
        ));
        let mut process = fixture.spawn(&[]);
        let mut client = connect(process.listener());
        let _server = accept(&backend);
        process.signal("-TERM");
        process.message("draining 1 connections");
        if second_signal {
            process.signal("-INT");
        }
        process.exited();
        assert_closed(&mut client);
    }
}

fn handshake(host: &str, protocol: u8, state: u8) -> Vec<u8> {
    let mut body = vec![0, protocol, host.len() as u8];
    body.extend(host.as_bytes());
    body.extend([0x63, 0xdd, state]);
    let mut packet = vec![body.len() as u8];
    packet.extend(body);
    packet
}

fn query(address: SocketAddr, packet: Vec<u8>, payload: i64) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut client = connect(address);
        client.write_all(&packet).unwrap();
        client.write_all(&[1, 0]).unwrap();
        let mut length = [0];
        client.read_exact(&mut length).unwrap();
        let mut response = vec![0; length[0] as usize];
        client.read_exact(&mut response).unwrap();
        let mut ping = vec![9, 1];
        ping.extend(payload.to_be_bytes());
        client.write_all(&ping).unwrap();
        let mut pong = [0; 10];
        client.read_exact(&mut pong).unwrap();
        assert_eq!(pong.as_slice(), ping);
        assert_closed(&mut client);
        response
    })
}

fn answer_status(backend: &TcpListener, packet: &[u8], description: u8) {
    let mut server = accept(backend);
    let mut actual = vec![0; packet.len() + 2];
    server.read_exact(&mut actual).unwrap();
    let mut expected = packet.to_vec();
    expected.extend([1, 0]);
    assert_eq!(actual, expected);
    let response = [
        11,
        0,
        9,
        b'{',
        b'"',
        b'x',
        b'"',
        b':',
        b'"',
        description,
        b'"',
        b'}',
    ];
    server.write_all(&response).unwrap();
}

#[test]
fn status_cache_preserves_ping_isolates_protocol_and_host_and_expires() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    let source =
        config(&[backend.local_addr().unwrap()], "").replace("l0 = 'b0'", "l0 = { ['*'] = 'b0' }");
    fixture.write(&options(&source, "metrics = '127.0.0.1:0', status_cache = { ttl_ms = 500, max_entries = 8, max_response_bytes = 1024 }"));
    let process = fixture.spawn(&[]);
    let front = process.listener();
    let metrics = process.metrics_address();
    let packet = handshake("play.test", 1, 1);
    let first = query(front, packet.clone(), 42);
    answer_status(&backend, &packet, b'a');
    let response = first.join().unwrap();
    assert_eq!(
        query(front, packet.clone(), i64::MIN).join().unwrap(),
        response
    );
    assert_no_connection(&backend);
    for other in [handshake("other.test", 1, 1), handshake("play.test", 2, 1)] {
        let next = query(front, other.clone(), -123);
        answer_status(&backend, &other, b'b');
        next.join().unwrap();
    }
    assert_eq!(metric(metrics, "status_cache_hits_total"), 1);
    thread::sleep(Duration::from_millis(550));
    let next = query(front, packet.clone(), 0);
    answer_status(&backend, &packet, b'c');
    assert_ne!(next.join().unwrap(), response);
    assert_eq!(metric(metrics, "status_cache_misses_total"), 4);
    // Login and transfer still preserve every byte and never use the status cache.
    for state in [2, 3] {
        let packet = handshake("play.test", 1, state);
        let mut client = connect(front);
        client.write_all(&packet).unwrap();
        let mut server = accept(&backend);
        let mut bytes = vec![0; packet.len()];
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, packet);
        exchange(&mut client, &mut server);
    }
}

#[cfg(unix)]
#[test]
fn status_cache_is_invalidated_on_reload_and_can_cover_a_brief_backend_outage() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = backend.local_addr().unwrap();
    let source = options(
        &config(&[address], "").replace("l0 = 'b0'", "l0 = { ['*'] = 'b0' }"),
        "status_cache = { ttl_ms = 30000 }",
    );
    fixture.write(&source);
    let process = fixture.spawn(&[]);
    let front = process.listener();
    let packet = handshake("play.test", 1, 1);
    let first = query(front, packet.clone(), 1);
    answer_status(&backend, &packet, b'a');
    let response = first.join().unwrap();
    drop(backend);
    assert_eq!(query(front, packet.clone(), 2).join().unwrap(), response);
    let backend = TcpListener::bind(address).unwrap();
    fixture.write(&source);
    process.signal("-HUP");
    process.message("configuration reloaded");
    let next = query(front, packet.clone(), 3);
    answer_status(&backend, &packet, b'b');
    assert_ne!(next.join().unwrap(), response);
}

#[test]
fn concurrent_status_misses_share_one_fill_and_bad_responses_do_not_poison_cache() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&options(
        &config(&[backend.local_addr().unwrap()], "").replace("l0 = 'b0'", "l0 = { ['*'] = 'b0' }"),
        "metrics = '127.0.0.1:0', status_cache = { ttl_ms = 30000, max_response_bytes = 64 }",
    ));
    let process = fixture.spawn(&[]);
    let front = process.listener();
    let metrics = process.metrics_address();
    let packet = handshake("play.test", 1, 1);
    // A malformed fill closes only that request, releases the fill lock, and is never cached.
    for malformed in [&[3, 0, 1, b'{'][..], &[0x80, 0x20][..]] {
        let mut client = connect(front);
        client.write_all(&packet).unwrap();
        client.write_all(&[1, 0]).unwrap();
        let mut server = accept(&backend);
        let mut request = vec![0; packet.len() + 2];
        server.read_exact(&mut request).unwrap();
        server.write_all(malformed).unwrap();
        assert_closed(&mut client);
    }
    let queries: Vec<_> = (0..12).map(|i| query(front, packet.clone(), i)).collect();
    await_metric(metrics, "connections_active", 12);
    answer_status(&backend, &packet, b'a');
    let mut responses = queries.into_iter().map(|q| q.join().unwrap());
    let expected = responses.next().unwrap();
    assert!(responses.all(|response| response == expected));
    assert_no_connection(&backend);
    assert_eq!(metric(metrics, "status_cache_misses_total"), 3);
    assert_eq!(metric(metrics, "status_cache_hits_total"), 11);
    // Malformed requests cannot use a cached response or touch the backend.
    let mut client = connect(front);
    client.write_all(&packet).unwrap();
    client.write_all(&[2, 0, 1]).unwrap();
    assert_closed(&mut client);
    assert_no_connection(&backend);
}

#[test]
fn routed_fallback_preserves_handshake_and_fails_closed_when_all_backends_are_down() {
    let fixture = Fixture::new();
    let unavailable = TcpListener::bind("127.0.0.1:0").unwrap();
    let primary = unavailable.local_addr().unwrap();
    drop(unavailable);
    let backup = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = options(
        &config(
            &[primary, backup.local_addr().unwrap()],
            "connect_timeout_ms = 500",
        )
        .replace("l0 = 'b0'", "l0 = { ['*'] = 'b0' }"),
        "fallbacks = { b0 = { 'b1' } }",
    );
    fixture.write(&source);
    let process = fixture.spawn(&[]);
    let front = process.listener();
    process.listener();
    let mut packet = handshake("Play.Test.\0metadata", 1, 2);
    packet.extend(b"pipelined login");
    let mut client = connect(front);
    client.write_all(&packet).unwrap();
    let mut server = accept(&backup);
    let mut bytes = vec![0; packet.len()];
    server.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, packet);
    exchange(&mut client, &mut server);
    drop(backup);
    drop(server);
    drop(client);
    let mut client = connect(front);
    client.write_all(&packet).unwrap();
    assert_closed(&mut client);
}

#[cfg(unix)]
#[test]
fn unchanged_rate_policy_keeps_depleted_buckets_across_reload() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = options(
        &config(&[backend.local_addr().unwrap()], ""),
        "rate_limit = { per_ip_per_second = 1, per_ip_burst = 1 }",
    );
    fixture.write(&source);
    let process = fixture.spawn(&[]);
    let front = process.listener();
    let mut client = connect(front);
    let mut server = accept(&backend);
    process.signal("-HUP");
    process.message("configuration reloaded");
    assert_closed(&mut connect(front));
    exchange(&mut client, &mut server);
}
