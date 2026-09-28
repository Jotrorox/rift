use super::*;

const VALID: &str = include_str!("../../examples/rift.lua");

#[test]
fn lua_expressions_produce_typed_config_with_original_defaults() {
    let config = Config::from_lua(VALID, "test.lua").unwrap();
    assert_eq!(config.listeners["public"], "0.0.0.0:25565".parse().unwrap());
    assert_eq!(config.backends["lobby"], "127.0.0.1:25566".parse().unwrap());
    assert_eq!(config.routes["public"], Route::Direct("lobby".into()));
    assert_eq!(config.limits, Limits::default());
    assert_eq!(Config::default().limits, config.limits);
    let source = VALID
        .replace("max_connections = 4096", "max_connections = 7")
        .replace("connect_timeout_ms = 5000", "connect_timeout_ms = 123")
        .replace("buffer_size = 32 * 1024", "buffer_size = 1024");
    assert_eq!(
        Config::from_lua(&source, "test.lua").unwrap().limits,
        Limits {
            max_connections: 7,
            connect_timeout: Duration::from_millis(123),
            buffer_size: 1024,
        }
    );
}

#[test]
fn limits_and_individual_limit_fields_are_optional() {
    for limits in ["", ", limits = {}", ", limits = { buffer_size = 32768 }"] {
        let source = format!(
            "return {{ listeners = {{ a = '[::1]:25565' }}, backends = {{ b = '[::1]:25566' }}, routes = {{ a = 'b' }} {limits} }}"
        );
        assert_eq!(
            Config::from_lua(&source, "test.lua").unwrap().limits,
            Limits::default()
        );
    }
}

#[test]
fn hook_is_optional_but_must_be_a_function() {
    assert!(
        Config::from_lua(VALID, "test.lua")
            .unwrap()
            .on_route
            .is_none()
    );
    for value in ["true", "42", "'hook'", "{}"] {
        let source = VALID.replacen("return {", &format!("return {{ on_route = {value},"), 1);
        let error = Config::from_lua(&source, "bad.lua").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("bad.lua: config.on_route: expected a function"),
            "{error}"
        );
    }
}

#[test]
fn startup_execution_and_source_size_are_bounded() {
    for (source, expected) in [
        ("while true do end".to_owned(), "instruction limit"),
        (
            " ".repeat(crate::script::MAX_SOURCE_BYTES + 1),
            "source limit",
        ),
    ] {
        let error = Config::from_lua(&source, "bad.lua")
            .unwrap_err()
            .to_string();
        assert!(error.contains("bad.lua"), "{error}");
        assert!(
            error.contains(expected) || error.contains("deadline"),
            "{error}"
        );
    }
}

#[test]
fn malformed_lua_and_wrong_shapes_have_context() {
    for (source, expected) in [
        ("return {", "syntax error"),
        ("error('broken configuration')", "broken configuration"),
        ("return 42", "expected a table"),
        ("return {}", "listeners"),
        ("return { listeners = {} }", "listeners: must not be empty"),
        ("return { listeners = { '127.0.0.1:1' } }", "listeners key"),
        (
            "return { listeners = { [''] = '127.0.0.1:1' } }",
            "names must not be empty",
        ),
    ] {
        let error = Config::from_lua(source, "bad.lua").unwrap_err().to_string();
        assert!(error.contains("bad.lua"), "{error}");
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn invalid_values_and_references_are_rejected() {
    for (from, to, expected) in [
        ("listeners =", "listners =", "unknown field"),
        (
            "max_connections =",
            "max_connection =",
            "limits.max_connection: unknown field",
        ),
        (
            "\"0.0.0.0:25565\"",
            "42",
            "listeners.public: expected a string",
        ),
        (
            "127.0.0.1:25566",
            "bad backend:25566",
            "backends.lobby: invalid backend",
        ),
        ("127.0.0.1:25566", "127.0.0.1:25565", "same socket"),
        (
            "public = \"lobby\"",
            "public = \"missing\"",
            "unknown backend",
        ),
        (
            "public = \"lobby\"",
            "public = \"lobby\", missing = \"lobby\"",
            "unknown listener",
        ),
        (
            "public = \"0.0.0.0:25565\"",
            "public = \"0.0.0.0:25565\", other = \"127.0.0.1:25567\"",
            "missing route",
        ),
        (
            "public = \"0.0.0.0:25565\"",
            "public = \"0.0.0.0:25565\", other = \"0.0.0.0:25565\"",
            "missing route",
        ),
    ] {
        let error = Config::from_lua(&VALID.replace(from, to), "bad.lua")
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{error}");
    }
    for field in [
        "max_connections = 4096",
        "connect_timeout_ms = 5000",
        "buffer_size = 32 * 1024",
    ] {
        let key = field.split(" = ").next().unwrap();
        for invalid in [
            "0",
            "-1",
            "1.5",
            "'10'",
            "true",
            "{}",
            "0/0",
            "math.huge",
            "1e30",
        ] {
            let source = VALID.replace(field, &format!("{key} = {invalid}"));
            let error = Config::from_lua(&source, "bad.lua")
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("limits.{key}")), "{error}");
        }
    }
}

#[test]
fn multiple_listeners_cannot_loop_through_each_other_or_share_an_address() {
    let source = "return {
        listeners = { a = '127.0.0.1:10001', b = '127.0.0.1:10002' },
        backends = { one = '127.0.0.1:10003', two = '127.0.0.1:10004' },
        routes = { a = 'one', b = 'two' }
    }";
    assert!(Config::from_lua(source, "test.lua").is_ok());
    for (from, to, expected) in [
        ("10003", "10002", "same socket"),
        ("10002", "10001", "duplicate address"),
    ] {
        let error = Config::from_lua(&source.replace(from, to), "test.lua").unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn hostname_tables_accept_dns_backends_and_reject_invalid_routes() {
    let source = "return {
        listeners = { public = '127.0.0.1:0' },
        backends = { lobby = 'localhost:25566', creative = '127.0.0.1:25567' },
        routes = { public = { ['play.example.com'] = 'lobby', ['*.example.com'] = 'creative', ['*'] = 'lobby' } }
    }";
    let config = Config::from_lua(source, "routes.lua").unwrap();
    let Mode::Routed(routes) = config.mode("public").unwrap() else {
        panic!("expected hostname routes")
    };
    assert_eq!(
        routes.select("play.example.com").unwrap(),
        &Backend::parse("localhost:25566").unwrap()
    );
    assert_eq!(
        routes.select("other.example.com").unwrap(),
        &Backend::parse("127.0.0.1:25567").unwrap()
    );
    assert_eq!(
        routes.select("unknown.test").unwrap(),
        &Backend::parse("localhost:25566").unwrap()
    );
    for (from, to, expected) in [
        (
            "['play.example.com'] = 'lobby'",
            "['play.example.com'] = 'missing'",
            "unknown backend",
        ),
        (
            "['play.example.com']",
            "['foo.*.example.com']",
            "invalid route hostname",
        ),
        (
            "['play.example.com'] = 'lobby'",
            "['play.example.com'] = 42",
            "expected a string",
        ),
        (
            "['play.example.com'] = 'lobby'",
            "['play.example.com'] = 'lobby', ['PLAY.EXAMPLE.COM.'] = 'creative'",
            "duplicate route",
        ),
    ] {
        let error = Config::from_lua(&source.replace(from, to), "routes.lua")
            .unwrap_err()
            .to_string();
        assert!(error.contains("routes.lua"), "{error}");
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn operational_options_are_validated_with_context() {
    let source = "return {
        listeners = { public = '127.0.0.1:0' },
        backends = { a = '127.0.0.1:1234', b = 'localhost:1235' },
        routes = { public = 'a' },
        fallbacks = { a = { 'b' } },
        rate_limit = {}, health_check = {}, status_cache = {},
        metrics = '127.0.0.1:9090', shutdown_timeout_ms = 1000,
    }";
    let config = Config::from_lua(source, "ops.lua").unwrap();
    assert_eq!(config.fallbacks["a"], ["b"]);
    assert_eq!(config.rate_limit.unwrap().per_ip_burst, 40);
    assert_eq!(config.health_check.unwrap().unhealthy_threshold, 2);
    assert_eq!(config.status_cache.unwrap().max_entries, 1024);
    assert_eq!(config.shutdown_timeout, Duration::from_secs(1));
    for (from, to, expected) in [
        ("{ 'b' }", "{}", "fallbacks.a"),
        ("{ 'b' }", "{ 'b', 'b' }", "fallbacks.a"),
        ("{ 'b' }", "{ 'a' }", "fallbacks.a"),
        ("{ 'b' }", "{ 'missing' }", "fallbacks.a"),
        ("{ 'b' }", "{ [2] = 'b' }", "dense array"),
        ("{ 'b' }", "{ name = 'b' }", "dense array"),
        (
            "fallbacks = { a",
            "fallbacks = { missing",
            "unknown backend",
        ),
        (
            "rate_limit = {}",
            "rate_limit = { per_ip_burst = 0 }",
            "rate_limit.per_ip_burst",
        ),
        (
            "health_check = {}",
            "health_check = { timeout_ms = -1 }",
            "health_check.timeout_ms",
        ),
        (
            "status_cache = {}",
            "status_cache = { max_entries = 1.5 }",
            "status_cache.max_entries",
        ),
        (
            "status_cache = {}",
            "status_cache = { ttl_ms = '10' }",
            "status_cache.ttl_ms",
        ),
        (
            "health_check = {}",
            "health_check = { unknown = 1 }",
            "unknown field",
        ),
        (
            "shutdown_timeout_ms = 1000",
            "shutdown_timeout_ms = 0",
            "shutdown_timeout_ms",
        ),
        ("metrics = '127.0.0.1:9090'", "metrics = true", "metrics"),
        (
            "metrics = '127.0.0.1:9090'",
            "metrics = 'localhost:9090'",
            "metrics",
        ),
        (
            "metrics = '127.0.0.1:9090'",
            "metrics = '127.0.0.1:1234'",
            "same socket",
        ),
    ] {
        let error = Config::from_lua(&source.replace(from, to), "ops.lua")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("ops.lua") && error.contains(expected),
            "{error}"
        );
    }
}
