use super::*;

const VALID: &str = include_str!("../../examples/rift.lua");

fn with_security(settings: &str) -> String {
    format!("{VALID}\nrift.setup {{ {settings} }}")
}

#[test]
fn online_authentication_and_velocity_are_validated_together() {
    let config = Config::from_lua(&with_security(
        "authentication = { online_mode = true, timeout_ms = 2500 }, forwarding = { mode = 'velocity', secret_env = 'PAPER_SECRET' }",
    ), "online.lua").unwrap();
    assert!(config.authentication.online_mode);
    assert_eq!(config.authentication.timeout, Duration::from_millis(2500));
    assert_eq!(config.forwarding.unwrap().secret_env, "PAPER_SECRET");
    let defaults = Config::from_lua(VALID, "offline.lua").unwrap();
    assert!(!defaults.authentication.online_mode);
    assert!(defaults.forwarding.is_none());
    // Validation does not read environment secrets or contact authentication services.
    let config = Config::from_lua(
        &with_security(
            "authentication = { online_mode = true }, forwarding = { mode = 'velocity' }",
        ),
        "online.lua",
    )
    .unwrap();
    assert_eq!(
        config.forwarding.unwrap().secret_env,
        "RIFT_FORWARDING_SECRET"
    );
}

#[test]
fn insecure_or_misspelled_authentication_settings_are_rejected() {
    for (settings, expected) in [
        (
            "authentication = { online_mode = true }",
            "must be enabled together",
        ),
        (
            "forwarding = { mode = 'velocity' }",
            "must be enabled together",
        ),
        (
            "authentication = { online_mode = 'true' }",
            "authentication.online_mode",
        ),
        (
            "authentication = { timeout_ms = 0 }",
            "authentication.timeout_ms",
        ),
        (
            "authentication = { timeout_ms = 60001 }",
            "authentication.timeout_ms",
        ),
        (
            "authentication = { timeout_ms = 1.5 }",
            "authentication.timeout_ms",
        ),
        (
            "authentication = { session_server = 'http://example.com' }",
            "unknown field",
        ),
        ("forwarding = { mode = 'legacy' }", "forwarding.mode"),
        (
            "forwarding = { mode = 'velocity', secret = 'literal' }",
            "unknown field",
        ),
        (
            "authentication = { online_mode = true }, forwarding = { mode = 'velocity', secret_env = 'BAD-NAME' }",
            "forwarding.secret_env",
        ),
    ] {
        let error = Config::from_lua(&with_security(settings), "bad.lua").unwrap_err();
        assert!(error.to_string().contains(expected), "{settings}: {error}");
    }
}

#[test]
fn lua_expressions_produce_typed_config_with_release_defaults() {
    let config = Config::from_lua(VALID, "test.lua").unwrap();
    assert_eq!(config.listeners["public"], "0.0.0.0:25565".parse().unwrap());
    assert_eq!(config.backends["lobby"], "127.0.0.1:25566".parse().unwrap());
    assert_eq!(config.routes["public"], Route::Direct("lobby".into()));
    assert_eq!(config.limits, Limits::default());
    assert_eq!(Config::default().limits, config.limits);
    let source = VALID
        .replace("max_connections = 1024", "max_connections = 7")
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
        let source = with_security(&format!("on_route = {value}"));
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
fn generated_configuration_errors_are_bounded_for_http_and_logs() {
    let error = Config::from_lua("error(string.rep('x', 1024 * 1024))", "bad.lua")
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("bad.lua:"));
    assert!(error.len() <= 2048);
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
        "max_connections = 1024",
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

fn with_options(options: &str) -> String {
    format!(
        "return {{ listeners = {{ public = '127.0.0.1:25565' }}, backends = {{ lobby = '127.0.0.1:25566' }}, routes = {{ public = 'lobby' }}, {options} }}"
    )
}

#[test]
fn http_services_are_independently_optional_and_have_loopback_defaults() {
    for options in [
        "",
        "web = false, status = false, metrics = false",
        "web = { enabled = false }, status = { enabled = false }",
    ] {
        let config = Config::from_lua(&with_options(options), "services.lua").unwrap();
        assert!(config.web.is_none());
        assert!(config.status.is_none());
        assert!(config.metrics.is_none());
        assert!(config.on_http.is_none());
    }
    let config = Config::from_lua(&with_options("web = {}, status = {}"), "services.lua").unwrap();
    assert_eq!(config.web.unwrap(), WebConfig::default());
    assert_eq!(config.status.unwrap(), StatusConfig::default());
    let config = Config::from_lua(&with_options("web = { listen = '[::1]:8081', api = false, ui = false }, status = { listen = '0.0.0.0:9091', metrics = false, ui = false }, on_http = function(req) return nil end"), "services.lua").unwrap();
    assert_eq!(
        config.web.unwrap(),
        WebConfig {
            listen: "[::1]:8081".parse().unwrap(),
            api: false,
            ui: false,
            token: None
        }
    );
    assert_eq!(
        config.status.unwrap(),
        StatusConfig {
            listen: "0.0.0.0:9091".parse().unwrap(),
            metrics: false,
            ui: false
        }
    );
    assert!(config.on_http.is_some());
}

#[test]
fn administration_tokens_and_service_field_types_are_validated() {
    for (options, expected) in [
        ("web = true", "web: expected a table"),
        ("status = true", "status: expected a table"),
        ("web = { api = 1 }", "web.api"),
        ("status = { enabled = 'false' }", "status.enabled"),
        ("status = { metrics = 1 }", "status.metrics"),
        ("web = { ui = 'yes' }", "web.ui"),
        ("web = { unknown = false }", "web.unknown"),
        ("status = { token = 'some-token' }", "status.token"),
        ("web = { listen = 'localhost:8080' }", "web.listen"),
        ("web = { listen = '0.0.0.0:8080' }", "web.token"),
        ("web = { listen = '[::]:8080' }", "web.token"),
        ("web = { token = 'short' }", "web.token"),
        (
            "web = { token = '0123456789abcdef\\r\\nHeader' }",
            "web.token",
        ),
        ("web = { token = '0123456789abcdef space' }", "web.token"),
        ("web = { token = 1234567890123456 }", "web.token"),
        ("on_http = true", "on_http: expected a function"),
    ] {
        let error = Config::from_lua(&with_options(options), "services.lua")
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{options}: {error}");
    }
    let config = Config::from_lua(
        &with_options("web = { listen = '0.0.0.0:8080', token = '0123456789abcdef' }"),
        "services.lua",
    )
    .unwrap();
    assert_eq!(
        config.web.unwrap().token.as_deref(),
        Some("0123456789abcdef")
    );
    assert!(
        Config::from_lua(
            &with_options("web = { enabled = false, listen = '0.0.0.0:8080' }"),
            "services.lua"
        )
        .is_ok()
    );
}

#[test]
fn services_cannot_collide_with_listeners_each_other_or_backend_targets() {
    for options in [
        "web = { listen = '127.0.0.1:25565' }",
        "web = {}, metrics = '127.0.0.1:8080'",
        "web = {}, status = { listen = '0.0.0.0:8080' }",
        "status = {}, metrics = '[::]:9090'",
        "status = { listen = '0.0.0.0:25565' }",
        "web = { listen = '127.0.0.1:25566' }",
        "status = { listen = '0.0.0.0:25566' }",
        "metrics = '[::]:25566'",
    ] {
        let error = Config::from_lua(&with_options(options), "services.lua")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("duplicate address") || error.contains("same socket"),
            "{options}: {error}"
        );
    }
    assert!(Config::from_lua(&with_options("web = { listen = '127.0.0.1:0' }, status = { listen = '127.0.0.1:0' }, metrics = '127.0.0.1:0'"), "services.lua").is_ok());
    for backend in [
        "localhost:8080",
        "LOCALHOST.:8080",
        "[::ffff:127.0.0.1]:8080",
    ] {
        let source = with_options("web = {}").replace("127.0.0.1:25566", backend);
        let error = Config::from_lua(&source, "services.lua").unwrap_err();
        assert!(error.to_string().contains("same socket"), "{error}");
    }
}

#[test]
fn programmatic_web_options_cannot_bypass_token_validation() {
    let mut config = Config {
        web: Some(WebConfig {
            listen: "0.0.0.0:8080".parse().unwrap(),
            ..WebConfig::default()
        }),
        ..Config::default()
    };
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("web.token")
    );
    config.web.as_mut().unwrap().token = Some("0123456789abcdef".into());
    config.validate().unwrap();
}

const NETWORK: &str = "return {
    listeners = { public = '127.0.0.1:0' },
    backends = { lobby = '127.0.0.1:25566', survival = '127.0.0.1:25567' },
    routes = { public = 'lobby' },
    network = {
        initial = { 'lobby', 'survival' }, hubs = { 'lobby' },
        access = { survival = { allow = { 'Alice', 'Bob' }, deny = { 'Bob' } } },
    },
}";

#[test]
fn network_order_and_explicit_access_rules_are_preserved() {
    let config = Config::from_lua(NETWORK, "network.lua").unwrap();
    assert_eq!(config.network.initial, ["lobby", "survival"]);
    assert_eq!(config.network.hubs, ["lobby"]);
    assert!(config.can_access("lobby", "Eve"));
    assert!(config.can_access("survival", "aLiCe"));
    assert!(!config.can_access("survival", "BOB"));
    assert!(!config.can_access("survival", "Eve"));
    assert!(!config.can_access("unknown", "Alice"));
    let deny_only = NETWORK.replace("allow = { 'Alice', 'Bob' }, ", "");
    let config = Config::from_lua(&deny_only, "network.lua").unwrap();
    assert!(config.can_access("survival", "Eve"));
    assert!(!config.can_access("survival", "Bob"));
    let closed = NETWORK.replace("allow = { 'Alice', 'Bob' }", "allow = {}");
    let config = Config::from_lua(&closed, "network.lua").unwrap();
    assert!(!config.can_access("survival", "Alice"));
    assert!(!config.can_access("survival", "Eve"));
}

#[test]
fn omitted_network_fields_keep_ordinary_routes_public() {
    let config = Config::from_lua(VALID, "network.lua").unwrap();
    assert_eq!(config.network, Network::default());
    assert!(config.can_access("lobby", "Alice"));
    for body in ["{}", "{ initial = {}, hubs = {}, access = {} }"] {
        let source = with_security(&format!("network = {body}"));
        assert_eq!(
            Config::from_lua(&source, "network.lua").unwrap().network,
            Network::default()
        );
    }
}

#[test]
fn bungeecord_is_explicitly_enabled_and_requires_a_boolean() {
    for (value, enabled) in [("true", true), ("false", false), ("nil", false)] {
        let source = NETWORK.replace(
            "network = {",
            &format!("network = {{ bungeecord = {value},"),
        );
        let config = Config::from_lua(&source, "bungeecord.lua").unwrap();
        assert_eq!(config.network.bungeecord, enabled);
        assert_eq!(config.network.initial, ["lobby", "survival"]);
    }
    for value in ["1", "'true'", "{}"] {
        let source = NETWORK.replace(
            "network = {",
            &format!("network = {{ bungeecord = {value},"),
        );
        assert!(
            Config::from_lua(&source, "bungeecord.lua")
                .unwrap_err()
                .to_string()
                .contains("network.bungeecord")
        );
    }
}

#[test]
fn bungeecord_rejects_case_ambiguous_backend_names_only_when_enabled() {
    let mut config = Config::from_lua(NETWORK, "bungeecord.lua").unwrap();
    config
        .backends
        .insert("Lobby".into(), "127.0.0.1:25568".parse().unwrap());
    config.validate().unwrap();
    config.network.bungeecord = true;
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("network.bungeecord")
    );
    config.backends.remove("lobby");
    config.network.initial[0] = "Lobby".into();
    config.network.hubs[0] = "Lobby".into();
    config
        .routes
        .insert("public".into(), Route::Direct("Lobby".into()));
    config.validate().unwrap();
}

#[test]
fn network_rejects_ambiguous_or_unknown_destinations_and_permissions() {
    for (from, to, expected) in [
        (
            "network = {",
            "network = { unknown = {},",
            "network.unknown",
        ),
        ("'lobby', 'survival'", "'lobby', 'lobby'", "network.initial"),
        ("'lobby', 'survival'", "'missing'", "network.initial"),
        ("'lobby', 'survival'", "[2] = 'lobby'", "dense array"),
        ("hubs = { 'lobby' }", "hubs = { 'missing' }", "network.hubs"),
        (
            "hubs = { 'lobby' }",
            "hubs = { 'lobby', 'lobby' }",
            "network.hubs",
        ),
        (
            "hubs = { 'lobby' }",
            "hubs = { name = 'lobby' }",
            "dense array",
        ),
        (
            "access = { survival",
            "access = { missing",
            "network.access.missing",
        ),
        (
            "allow = { 'Alice', 'Bob' }",
            "allow = { 'Alice', 'ALICE' }",
            "duplicate player",
        ),
        (
            "allow = { 'Alice', 'Bob' }",
            "allow = { '*' }",
            "invalid or duplicate player",
        ),
        (
            "allow = { 'Alice', 'Bob' }",
            "allow = { 'Alice Bob' }",
            "invalid or duplicate player",
        ),
        (
            "allow = { 'Alice', 'Bob' }",
            "allow = { '550e8400-e29b-41d4-a716-446655440000' }",
            "invalid or duplicate player",
        ),
        (
            "deny = { 'Bob' }",
            "deny = { 'Bob', 'BOB' }",
            "duplicate player",
        ),
        (
            "deny = { 'Bob' }",
            "deny = { 'too_long_username' }",
            "invalid or duplicate player",
        ),
        ("deny = { 'Bob' }", "deny = { true }", "expected a string"),
        ("deny = { 'Bob' }", "deny = false", "expected a table"),
        ("deny = { 'Bob' }", "denied = { 'Bob' }", "unknown field"),
    ] {
        let error = Config::from_lua(&NETWORK.replace(from, to), "network.lua")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("network.lua") && error.contains(expected),
            "{error}"
        );
    }
    let too_many = (0..17)
        .map(|index| format!("'lobby{index}'"))
        .collect::<Vec<_>>()
        .join(",");
    let error = Config::from_lua(
        &NETWORK.replace("'lobby', 'survival'", &too_many),
        "network.lua",
    )
    .unwrap_err();
    assert!(error.to_string().contains("at most 16"));
}

#[test]
fn programmatic_network_changes_receive_the_same_validation() {
    let mut config = Config::from_lua(NETWORK, "network.lua").unwrap();
    config.network.initial.push("lobby".into());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("network.initial")
    );
    config.network.initial.pop();
    config
        .network
        .access
        .get_mut("survival")
        .unwrap()
        .deny
        .push("*".into());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("network.access.survival.deny")
    );
}

fn with_admin_options(options: &str) -> String {
    format!(
        "return {{
        listeners = {{ public = '0.0.0.0:25565' }},
        backends = {{ lobby = '127.0.0.1:25566' }},
        routes = {{ public = 'lobby' }},
        {options}
    }}"
    )
}

#[test]
fn administrator_options_have_explicit_permissions_and_no_environment_dependency() {
    let defaults = Config::from_lua(&with_admin_options(""), "admin.lua").unwrap();
    assert!(!defaults.maintenance);
    assert!(defaults.draining.is_empty());
    assert!(defaults.admin.is_none());
    assert!(defaults.login_rate_limit.is_none());

    let config = Config::from_lua(
        &with_admin_options(
            "
        maintenance = true,
        draining = { 'lobby' },
        login_rate_limit = { per_ip_per_second = 3, per_ip_burst = 6 },
        admin = {
            listen = '127.0.0.1:9091',
            token_env = 'RIFT_TEST_TOKEN_NOT_SET',
            permissions = { 'status', 'reload' },
        },
    ",
        ),
        "admin.lua",
    )
    .unwrap();
    assert!(config.maintenance);
    assert_eq!(config.draining, BTreeSet::from(["lobby".into()]));
    assert_eq!(config.login_rate_limit.unwrap().per_ip_burst, 6);
    let admin = config.admin.unwrap();
    assert_eq!(admin.token_env, "RIFT_TEST_TOKEN_NOT_SET");
    assert_eq!(
        admin.permissions,
        BTreeSet::from(["status".into(), "reload".into()])
    );

    let config = Config::from_lua(
        &with_admin_options("admin = { listen = '[::1]:9091', permissions = { 'status' } }"),
        "admin.lua",
    )
    .unwrap();
    assert_eq!(config.admin.unwrap().token_env, "RIFT_ADMIN_TOKEN");
}

#[test]
fn malformed_administrator_options_are_actionable() {
    for (options, expected) in [
        ("maintenance = 'true'", "maintenance: expected a boolean"),
        ("draining = { 'missing' }", "draining: unknown backend"),
        (
            "draining = { 'lobby', 'lobby' }",
            "draining: duplicate name",
        ),
        (
            "draining = { [2] = 'lobby' }",
            "draining: expected a dense array",
        ),
        (
            "draining = { lobby = true }",
            "draining: expected a dense array",
        ),
        (
            "login_rate_limit = { per_ip_burst = 0 }",
            "login_rate_limit.per_ip_burst",
        ),
        (
            "login_rate_limit = { surprise = true }",
            "login_rate_limit.surprise",
        ),
        ("admin = {}", "admin.listen"),
        (
            "admin = { listen = '0.0.0.0:9091', permissions = { 'status' } }",
            "admin.listen: use a loopback",
        ),
        ("admin = { listen = '127.0.0.1:9091' }", "admin.permissions"),
        (
            "admin = { listen = '127.0.0.1:9091', permissions = {} }",
            "explicitly grant at least one permission",
        ),
        (
            "admin = { listen = '127.0.0.1:9091', permissions = { '*' } }",
            "unknown permission",
        ),
        (
            "admin = { listen = '127.0.0.1:9091', permissions = { 'status', 'status' } }",
            "duplicate name",
        ),
        (
            "admin = { listen = '127.0.0.1:9091', permissions = { [2] = 'status' } }",
            "dense array",
        ),
        (
            "admin = { listen = '127.0.0.1:9091', permissions = { 'status' }, token_env = 'TOKEN=secret' }",
            "admin.token_env",
        ),
        (
            "admin = { listen = '127.0.0.1:9091', permissions = { 'status' }, token_env = '' }",
            "admin.token_env",
        ),
        (
            "admin = { listen = '127.0.0.1:9091', permissions = { 'status' }, token_env = '1TOKEN' }",
            "admin.token_env",
        ),
        (
            "admin = { listen = '127.0.0.1:25565', permissions = { 'status' } }",
            "conflicts with listeners.public",
        ),
        (
            "admin = { listen = '127.0.0.1:25566', permissions = { 'status' } }",
            "admin.listen and backends.lobby",
        ),
        (
            "metrics = '0.0.0.0:9091', admin = { listen = '127.0.0.1:9091', permissions = { 'status' } }",
            "conflicts with metrics",
        ),
    ] {
        let error = Config::from_lua(&with_admin_options(options), "admin.lua")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("admin.lua") && error.contains(expected),
            "{options}: {error}"
        );
    }
}

#[test]
fn programmatic_administrator_configs_receive_the_same_validation() {
    let mut config = Config::default();
    config.draining.insert("unknown".into());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("draining")
    );
    config.draining.clear();
    config.admin = Some(Admin {
        listen: "127.0.0.1:9091".parse().unwrap(),
        token_env: "RIFT_ADMIN_TOKEN".into(),
        permissions: BTreeSet::from(["status".into()]),
    });
    config.validate().unwrap();
    config
        .admin
        .as_mut()
        .unwrap()
        .permissions
        .insert("unknown".into());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unknown permission")
    );
    config.admin.as_mut().unwrap().permissions.remove("unknown");
    config.login_rate_limit = Some(RateLimit {
        per_ip_per_second: 1,
        per_ip_burst: 1,
        global_per_second: 1,
        global_burst: 1,
        max_ips: 0,
    });
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("login_rate_limit.max_ips")
    );
}

#[test]
fn local_administration_and_http_services_keep_independent_options_and_safe_sockets() {
    let source = with_admin_options(
        "admin = { listen = '127.0.0.1:0', permissions = { 'status' } },
        web = { listen = '127.0.0.1:0' },
        status = { listen = '127.0.0.1:0' }, metrics = false,
        on_http = function(request) return nil end,
        maintenance = true, draining = { 'lobby' }, login_rate_limit = {}",
    );
    let config = Config::from_lua(&source, "combined.lua").unwrap();
    assert!(config.admin.is_some() && config.web.is_some() && config.status.is_some());
    assert!(config.metrics.is_none() && config.on_http.is_some());
    assert!(config.maintenance && config.draining.contains("lobby"));
    assert!(config.login_rate_limit.is_some());

    for (service, expected) in [
        ("web = { listen = '127.0.0.1:9091' }", "web.listen"),
        (
            "web = { listen = '[::ffff:127.0.0.1]:9091', token = '0123456789abcdef' }",
            "web.listen",
        ),
        ("status = { listen = '127.0.0.1:9091' }", "status.listen"),
        ("status = { listen = '0.0.0.0:9091' }", "status.listen"),
        ("status = { listen = '[::]:9091' }", "status.listen"),
    ] {
        let source = with_admin_options(&format!(
            "admin = {{ listen = '127.0.0.1:9091', permissions = {{ 'status' }} }}, {service}"
        ));
        let error = Config::from_lua(&source, "combined.lua")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("admin.listen: address conflicts with {expected}")),
            "{error}"
        );
    }

    for backend in [
        "localhost:9091",
        "LOCALHOST.:9091",
        "[::ffff:127.0.0.1]:9091",
    ] {
        let source =
            with_admin_options("admin = { listen = '127.0.0.1:9091', permissions = { 'status' } }")
                .replace("127.0.0.1:25566", backend);
        let error = Config::from_lua(&source, "combined.lua")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("admin.listen and backends.lobby must not point to the same socket"),
            "{error}"
        );
    }
}

#[test]
fn network_and_administrative_controls_keep_independent_access_policies() {
    let source = NETWORK.replacen(
        "return {",
        "return {
            admin = { listen = '127.0.0.1:0', permissions = { 'status', 'transfer' } },
            maintenance = true, draining = { 'survival' },
            rate_limit = { per_ip_burst = 40 },
            login_rate_limit = { per_ip_burst = 3 },",
        1,
    );
    let config = Config::from_lua(&source, "combined-network.lua").unwrap();
    assert_eq!(config.network.initial, ["lobby", "survival"]);
    assert_eq!(config.network.hubs, ["lobby"]);
    assert!(config.can_access("survival", "Alice"));
    assert!(!config.can_access("survival", "Bob"));
    assert!(config.maintenance && config.draining.contains("survival"));
    assert_eq!(config.rate_limit.unwrap().per_ip_burst, 40);
    assert_eq!(config.login_rate_limit.unwrap().per_ip_burst, 3);
    assert_eq!(
        config.admin.unwrap().permissions,
        BTreeSet::from(["status".into(), "transfer".into()])
    );
}

#[test]
fn messaging_is_optional_and_supports_local_only_plugins() {
    assert!(Config::default().messaging.is_none());
    assert!(
        Config::from_lua(&with_security("messaging = false"), "disabled.lua")
            .unwrap()
            .messaging
            .is_none()
    );
    let config = Config::from_lua(&with_security("messaging = {}"), "local.lua").unwrap();
    let messaging = config.messaging.unwrap();
    assert!(messaging.listen.is_none());
    assert!(messaging.principals.is_empty());
    assert_eq!(
        messaging.subscription_capacity,
        crate::messaging::BrokerConfig::default().subscription_capacity
    );
    let config = Config::from_lua(&with_security("messaging = { subscription_capacity = 32, subscriptions = {{subject = 'plugin.*', queue = 'workers'}} }, on_message = function(message) rift.publish(message.reply, message.payload) end"), "plugin.lua").unwrap();
    assert!(config.on_message.is_some());
    assert_eq!(
        config.messaging.unwrap().subscriptions[0].subject,
        "plugin.*"
    );
}

#[test]
fn messaging_quic_requires_tls_and_explicit_principal_acls() {
    let config = Config::from_lua(&with_security(r#"
        messaging = {
            listen = '127.0.0.1:4222', certificate = 'server.pem', private_key = 'server.key',
            principals = {{name = 'paper', token_env = 'PAPER_MESSAGE_TOKEN', publish = {'server.>', 'rift.control.status'}, subscribe = {'reply.*'}, control = true}},
            streams = {events = {storage_path = 'data/events', subjects = {'events.>'}, max_messages = 20, max_bytes = 2048, max_payload_bytes = 1024}},
        }
    "#), "quic.lua").unwrap();
    let messaging = config.messaging.unwrap();
    assert_eq!(messaging.listen.unwrap(), "127.0.0.1:4222".parse().unwrap());
    assert!(messaging.principals[0].control);
    assert_eq!(messaging.principals[0].token_env, "PAPER_MESSAGE_TOKEN");
    assert_eq!(messaging.streams["events"].config.max_messages, 20);
    assert!(messaging.streams["events"].config.sync_on_write);
    // Loading configs does not read private files or resolve environment secrets.
}

#[test]
fn messaging_malformed_settings_fail_closed() {
    for (settings, expected) in [
        ("messaging = true", "messaging"),
        ("messaging = { typo = 1 }", "unknown field"),
        (
            "messaging = { listen = '127.0.0.1:4222' }",
            "certificate and private_key",
        ),
        (
            "messaging = { listen = '127.0.0.1:4222', certificate = 'cert', private_key = 'key' }",
            "principals",
        ),
        ("messaging = { certificate = 'cert' }", "messaging.listen"),
        (
            "messaging = { subscription_capacity = 0 }",
            "subscription_capacity",
        ),
        (
            "messaging = { max_payload_bytes = 1.5 }",
            "max_payload_bytes",
        ),
        (
            "messaging = { subscriptions = {{subject = 'plugin.>'}} }",
            "on_message",
        ),
        ("on_message = function(message) end", "subscriptions"),
        ("on_message = 1", "expected a function"),
        (
            "messaging = { subscriptions = {{subject = 'plugin.>.bad'}} }, on_message = function(message) end",
            "invalid subject",
        ),
        (
            "messaging = { subscriptions = {{subject = 'plugin.*', queue = '*'}} }, on_message = function(message) end",
            "queue",
        ),
        (
            "messaging = { subscriptions = {{subject = 'plugin.*'}, {subject = 'plugin.*'}} }, on_message = function(message) end",
            "duplicate subscription",
        ),
        (
            "messaging = { subscriptions = {[2] = {subject = 'plugin.*'}} }, on_message = function(message) end",
            "dense array",
        ),
        (
            "messaging = { streams = {events = {subjects = {}}} }",
            "at least one pattern",
        ),
        (
            "messaging = { streams = {events = {max_bytes = 4, max_payload_bytes = 8}} }",
            "must fit",
        ),
        (
            "messaging = { streams = {events = {storage_path = ''}} }",
            "paths must be",
        ),
        (
            "messaging = { streams = {events = {unknown = true}} }",
            "unknown field",
        ),
        (
            "messaging = { listen = '127.0.0.1:4222', certificate = 'cert', private_key = 'key', principals = {{name = 'paper', token_env = 'BAD-NAME'}} }",
            "token_env",
        ),
        (
            "messaging = { listen = '127.0.0.1:4222', certificate = 'cert', private_key = 'key', principals = {{name = 'paper', token_env = 'PAPER_TOKEN', subscribe = {'foo.*bar'}}} }",
            "invalid subject",
        ),
    ] {
        let error =
            Config::from_lua(&with_security(settings), "invalid-messaging.lua").unwrap_err();
        assert!(error.to_string().contains(expected), "{settings}: {error}");
    }
}
