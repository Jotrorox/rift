use super::*;
use serde_json::Value;

fn event(process: &Process, expected: &str, peer: SocketAddr) -> Value {
    let line = process.lines.recv_timeout(Duration::from_secs(10)).unwrap();
    let value: Value = serde_json::from_str(&line).expect(&line);
    assert_eq!(value["event"], expected, "{line}");
    assert_eq!(value["peer"], peer.to_string());
    assert_eq!(value["listener"], "l0");
    assert!(value["connection_id"].as_u64().unwrap() > 0);
    assert!(value["timestamp_unix_ms"].as_u64().unwrap() > 0);
    assert!(value["duration_ms"].as_f64().unwrap() >= 0.0);
    assert!(!value["message"].as_str().unwrap().is_empty());
    value
}

#[test]
fn handshake_errors_and_missing_routes_have_no_selected_backend() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(
        &config(&[backend.local_addr().unwrap()], "")
            .replace("l0 = 'b0'", "l0 = { ['play.example.com'] = 'b0' }"),
    );
    let process = fixture.spawn(&[]);
    let front = process.listener();
    for (packet, stage, failure, kind) in [
        (vec![0], "handshake", "handshake_error", "InvalidData"),
        (vec![2, 0], "handshake", "handshake_error", "UnexpectedEof"),
        (
            handshake("missing.example.com", 47, 2),
            "route",
            "no_route",
            "NotFound",
        ),
    ] {
        let mut client = connect(front);
        let peer = client.local_addr().unwrap();
        client.write_all(&packet).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        if stage == "route" {
            let packet = game::read_packet(&mut client).unwrap();
            assert_eq!(packet.id, 0);
        }
        assert_closed(&mut client);
        let failure_event = event(&process, "connection_failed", peer);
        assert_eq!(failure_event["stage"], stage);
        assert_eq!(failure_event["failure"], failure);
        assert_eq!(failure_event["error_kind"], kind);
        assert!(failure_event["backend"].is_null());
        assert!(failure_event["backend_address"].is_null());
    }
    assert_no_connection(&backend);
}

#[test]
fn handshake_timeout_reports_elapsed_time() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(
        &config(&[backend.local_addr().unwrap()], "").replace("l0 = 'b0'", "l0 = { ['*'] = 'b0' }"),
    );
    let process = fixture.spawn(&[]);
    let mut client = connect(process.listener());
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let peer = client.local_addr().unwrap();
    assert_closed(&mut client);
    let failure = event(&process, "connection_failed", peer);
    assert_eq!(failure["stage"], "handshake");
    assert_eq!(failure["error_kind"], "TimedOut");
    assert!(failure["duration_ms"].as_f64().unwrap() >= 4900.0);
}

#[test]
fn dns_and_connect_failures_include_the_attempted_backend_and_correlate_with_the_connection() {
    let unused = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = unused.local_addr().unwrap();
    drop(unused);
    for (target, stage) in [
        ("rift-does-not-exist.invalid:25565".into(), "dns"),
        (address.to_string(), "connect"),
    ] {
        let fixture = Fixture::new();
        fixture.write(
            &config(&[address], "connect_timeout_ms = 1000").replace(&address.to_string(), &target),
        );
        let process = fixture.spawn(&[]);
        let mut client = connect(process.listener());
        let peer = client.local_addr().unwrap();
        client.write_all(&game::setup()).unwrap();
        let packet = game::read_packet(&mut client).unwrap();
        assert_eq!(packet.id, 0);
        assert!(
            rift::protocol::read_string(&mut packet.data.as_slice(), 32767)
                .unwrap()
                .contains("unavailable")
        );
        assert_closed(&mut client);
        let attempt = event(&process, "backend_attempt_failed", peer);
        let failure = event(&process, "connection_failed", peer);
        for value in [&attempt, &failure] {
            assert_eq!(value["stage"], stage);
            assert_eq!(value["failure"], format!("{stage}_error"));
            assert_eq!(value["backend"], "b0");
            assert_eq!(value["backend_address"], target);
        }
        assert_eq!(attempt["connection_id"], failure["connection_id"]);
        assert!(
            failure["duration_ms"].as_f64().unwrap() >= attempt["duration_ms"].as_f64().unwrap()
        );
    }
}

#[test]
fn script_errors_invalid_decisions_and_explicit_rejections_are_distinct_json_events() {
    for (hook, name, category, message) in [
        (
            r#"error('broken "hook"\nnext line')"#,
            "connection_failed",
            "script_error",
            "broken \"hook\"\nnext line",
        ),
        (
            "return { backend = 'missing' }",
            "connection_failed",
            "unknown_backend",
            "unknown backend",
        ),
        (
            "return { backend = false }",
            "connection_failed",
            "script_error",
            "on_route",
        ),
        (
            "return { reject = true, reason = 'maintenance' }",
            "connection_rejected",
            "route_rejected",
            "maintenance",
        ),
        (
            "error(string.rep('é', 4096))",
            "connection_failed",
            "script_error",
            "rift.lua",
        ),
    ] {
        let fixture = Fixture::new();
        let backend = TcpListener::bind("127.0.0.1:0").unwrap();
        fixture.write(&options(
            &config(&[backend.local_addr().unwrap()], ""),
            &format!("on_route = function() {hook} end"),
        ));
        let process = fixture.spawn(&[]);
        let mut client = connect(process.listener());
        let peer = client.local_addr().unwrap();
        assert_closed(&mut client);
        let failure = event(&process, name, peer);
        assert_eq!(failure["stage"], "on_route");
        assert_eq!(failure["failure"], category);
        assert!(failure["backend"].is_null());
        assert!(failure["message"].as_str().unwrap().contains(message));
        assert!(failure["message"].as_str().unwrap().chars().count() <= 2048);
        assert_no_connection(&backend);
    }
}

#[test]
fn failure_after_fallback_reports_the_backup_instead_of_the_primary() {
    let fixture = Fixture::new();
    let primary = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = primary.local_addr().unwrap();
    drop(primary);
    let backup = TcpListener::bind("127.0.0.1:0").unwrap();
    let source = config(
        &[address, backup.local_addr().unwrap()],
        "connect_timeout_ms = 1000",
    )
    .replace("l0 = 'b0'", "l0 = { ['*'] = 'b0' }");
    fixture.write(&options(
        &source,
        "fallbacks = { b0 = { 'b1' } }, status_cache = { ttl_ms = 1000 }",
    ));
    let process = fixture.spawn(&[]);
    let front = process.listener();
    process.listener();
    let mut client = connect(front);
    let peer = client.local_addr().unwrap();
    let mut packet = handshake("play.example.com", 1, 1);
    packet.extend([1, 0]);
    client.write_all(&packet).unwrap();
    let mut server = accept(&backup);
    let mut received = vec![0; packet.len()];
    server.read_exact(&mut received).unwrap();
    assert_eq!(received, packet);
    server.write_all(&[1, 0]).unwrap(); // Invalid status response: missing JSON.
    assert_closed(&mut client);
    let attempt = event(&process, "backend_attempt_failed", peer);
    assert_eq!(attempt["backend"], "b0");
    let failure = event(&process, "connection_failed", peer);
    assert_eq!(failure["backend"], "b1");
    assert_eq!(
        failure["backend_address"],
        backup.local_addr().unwrap().to_string()
    );
    assert_eq!(failure["stage"], "status_upstream");
    assert_eq!(failure["failure"], "io_error");
    assert_eq!(failure["connection_id"], attempt["connection_id"]);
}

#[test]
fn admission_rejections_are_logged_without_changing_error_metrics() {
    let fixture = Fixture::new();
    let backend = TcpListener::bind("127.0.0.1:0").unwrap();
    fixture.write(&options(
        &config(&[backend.local_addr().unwrap()], "max_connections = 1"),
        "metrics = '127.0.0.1:0'",
    ));
    let process = fixture.spawn(&[]);
    let front = process.listener();
    let metrics = process.metrics_address();
    let _allowed = connect_game(front);
    let _server = accept_game(&backend);
    let mut rejected = connect(front);
    let peer = rejected.local_addr().unwrap();
    assert_closed(&mut rejected);
    let rejection = event(&process, "connection_rejected", peer);
    assert_eq!(rejection["stage"], "admission");
    assert_eq!(rejection["failure"], "capacity_exhausted");
    assert!(rejection["backend"].is_null());
    assert_eq!(metric(metrics, "connection_errors_total"), 0);
    assert_eq!(metric(metrics, "connections_capacity_rejected_total"), 1);
}
