use super::*;
use tokio::time::timeout;

fn source(extension: &str) -> String {
    format!(
        "return {{ listeners = {{ public = '127.0.0.1:0' }}, backends = {{ lobby = '127.0.0.1:25001', game = '127.0.0.1:25002' }}, routes = {{ public = 'lobby' }}, authentication = {{ online_mode = true }}, forwarding = {{ mode = 'velocity', secret_env = 'TEST_SECRET' }}, extensions = {{ {extension} }} }}"
    )
}
fn config(extension: &str) -> Config {
    Config::from_lua(&source(extension), "extension-test.lua").unwrap()
}
fn runtime(extension: &str) -> Extensions {
    Extensions::new(
        &config(extension),
        Broker::new(Default::default()).unwrap(),
        None,
    )
    .unwrap()
}
/// Grant `nodes` in the runtime's permission store to a player UUID, or with
/// `*` to the `default` group everyone inherits.
fn grant(runtime: &Extensions, who: &str, nodes: &[&str]) {
    let subject = if who == "*" {
        SubjectRef::Group(rift_permissions::DEFAULT_GROUP.into())
    } else {
        SubjectRef::User(Uuid::parse(who).unwrap())
    };
    for node in nodes {
        let node = Node::permission(node, true).unwrap();
        let change = rift_permissions::Change::SetNode {
            subject: subject.clone(),
            node,
        };
        runtime.permissions().apply(Actor::Cli, change).unwrap();
    }
}
fn context(id: u64) -> Context {
    Context::authenticated(
        id,
        &AuthenticatedProfile {
            uuid: [id as u8; 16],
            name: format!("Player{id}"),
            properties: vec![],
        },
    )
}

#[test]
fn queue_pressure_excludes_expired_cancelled_and_disconnected_tickets() {
    let runtime = runtime("api_version=1, queues={game=1}");
    let first = runtime.session(context(1)).unwrap();
    let second = runtime.session(context(2)).unwrap();
    first.enqueue("game").unwrap();
    second.enqueue("game").unwrap();
    assert_eq!(runtime.queue_lengths()["game"], 2);
    runtime.host.lock().unwrap().queues.get_mut("game").unwrap()[0].1 =
        Instant::now() - Duration::from_secs(301);
    assert_eq!(runtime.queue_lengths()["game"], 1);
    assert!(first.queued_target().is_err()); // retain the existing expiry notification
    assert!(first.queued_target().unwrap().is_none());
    second.leave_queue();
    assert_eq!(runtime.queue_lengths()["game"], 0);
    second.enqueue("game").unwrap();
    drop(second);
    assert_eq!(runtime.queue_lengths()["game"], 0);
}

#[tokio::test]
async fn group_queue_capacity_and_fifo_follow_live_instance_membership() {
    let source = "return {listeners={public='127.0.0.1:0'},backends={},routes={public='game'},authentication={online_mode=true},forwarding={mode='velocity',secret_env='TEST_SECRET'},service_groups={game={command={'unused'},directory='instances/{name}',port_range={26000,26003},scaling={capacity_per_instance=1,min_instances=0}}},extensions={api_version=1,queues={game=1}}}";
    let mut config = Config::from_lua(source, "group-queue.lua").unwrap();
    let runtime = Extensions::new(&config, Broker::new(Default::default()).unwrap(), None).unwrap();
    let mut first = runtime.session(context(1)).unwrap();
    let second = runtime.session(context(2)).unwrap();
    first.enqueue("game").unwrap();
    second.enqueue("game").unwrap();
    assert_eq!(runtime.queue_lengths()["game"], 2);
    assert!(first.queued_target().unwrap().is_none()); // group can scale from zero
    config.add_instance("game", "game-1", 26000).unwrap();
    runtime.update_destinations(&config);
    assert_eq!(first.queued_target().unwrap().as_deref(), Some("game"));
    assert!(second.reserve("game-1").is_err()); // group FIFO cannot be bypassed
    first.before_transfer("game-1", "queue").await.unwrap();
    first.ready("game-1").await;
    assert!(first.enqueue("game").is_err()); // already in the group
    assert_eq!(runtime.queue_lengths()["game"], 1);
    assert!(second.queued_target().unwrap().is_none());
    assert!(second.reserve("game-1").is_err()); // full across concrete names
    config.add_instance("game", "game-2", 26001).unwrap();
    runtime.update_destinations(&config);
    assert_eq!(second.queued_target().unwrap().as_deref(), Some("game"));
    second.reserve("game-2").unwrap();
    assert!(first.reserve("game-2").is_ok()); // moving within the group uses its existing slot
    drop(first);
    drop(second);
    assert_eq!(runtime.queue_lengths()["game"], 0);
}

#[tokio::test]
async fn players_on_an_instance_carry_its_service_group_as_a_context() {
    let source = "return {listeners={public='127.0.0.1:0'},backends={hub='127.0.0.1:25001'},routes={public='hub'},service_groups={lobby={command={'unused'},directory='instances/{name}',port_range={26010,26013}}}}";
    let mut config = Config::from_lua(source, "group-context.lua").unwrap();
    config.add_instance("lobby", "lobby-3", 26012).unwrap();
    let runtime = Extensions::new(&config, Broker::new(Default::default()).unwrap(), None).unwrap();
    runtime.update_destinations(&config);
    let uuid = Uuid::from_bytes([9; 16]);
    let (player, _actions) = runtime
        .players()
        .register(9, &[9; 16], "Alice", 774, "lobby-3");
    let contexts = || runtime.permissions().contexts(uuid).to_string();
    assert_eq!(contexts(), "group=lobby;server=lobby-3");
    player.set_server("hub");
    assert_eq!(contexts(), "server=hub");
    player.set_server("lobby-3");
    assert_eq!(contexts(), "group=lobby;server=lobby-3");
    // The import mapping knows the groups of the live configuration.
    assert_eq!(
        runtime.permissions().service_groups(),
        ["lobby".to_owned()].into()
    );
}

/// The visibility rule end to end in the directories: a player whose
/// `rift.visible-to` is set is missing from another server's tab list, from
/// every public and backend count and from completion, is visible to a viewer
/// holding the node, stays hidden across a switch and a reconnect, and still
/// counts for the capacity check that keeps their server from stopping.
#[tokio::test]
async fn a_hidden_player_is_missing_for_viewers_without_the_node() {
    use crate::players::Viewer;
    let source = "return {listeners={public='127.0.0.1:0'},backends={},routes={public='lobby'},service_groups={lobby={command={'unused'},directory='instances/{name}',port_range={26020,26023},idle_timeout_ms=1}}}";
    let mut config = Config::from_lua(source, "visibility.lua").unwrap();
    config.add_instance("lobby", "lobby-1", 26020).unwrap();
    config.add_instance("lobby", "lobby-2", 26021).unwrap();
    let runtime = Extensions::new(&config, Broker::new(Default::default()).unwrap(), None).unwrap();
    runtime.update_destinations(&config);
    let players = runtime.players().clone();
    let roster = players.roster().clone();
    let authority = runtime.permissions().clone();
    let (staff, alice, moderator) = ([1; 16], [2; 16], [3; 16]);
    authority
        .apply(
            rift_permissions::Actor::Cli,
            rift_permissions::Change::SetNode {
                subject: SubjectRef::User(Uuid::from_bytes(staff)),
                node: Node::meta(crate::players::VISIBLE_TO, "Vanish.See").unwrap(),
            },
        )
        .unwrap();
    grant(&runtime, &"03".repeat(16), &["vanish.see"]);
    // Logins in the order a session makes them: the roster record with its
    // rule first, the directory once the player is in a world.
    let join = |uuid: [u8; 16], name: &str, server: &str| {
        let rule = players.visibility(&uuid, server);
        let record = roster.register_visible(uuid, name, server, rule).unwrap();
        let (handle, actions) = players.register(u64::from(uuid[0]), &uuid, name, 774, server);
        (record, handle, actions)
    };
    let hidden = join(staff, "Staff", "lobby-1");
    let _alice = join(alice, "Alice", "lobby-2");
    let _moderator = join(moderator, "Moderator", "lobby-2");
    assert_eq!(
        players.visibility(&staff, "lobby-1").as_deref(),
        Some("vanish.see")
    );
    let holds = |uuid: [u8; 16]| {
        let authority = authority.clone();
        move |node: &str| {
            authority.check_active(Uuid::from_bytes(uuid), node) == rift_permissions::Tristate::True
        }
    };
    let (alice_holds, moderator_holds) = (holds(alice), holds(moderator));
    let as_alice = Viewer::Player {
        uuid: alice,
        holds: &alice_holds,
    };
    let as_moderator = Viewer::Player {
        uuid: moderator,
        holds: &moderator_holds,
    };
    let tab = crate::tab_list::TabList {
        header: Some(serde_json::json!("{online}/{server_online}")),
        network: true,
        ..Default::default()
    };
    let check_hidden = |server: &str| {
        let view = crate::tab_list::view(&roster, as_alice, Some(&tab), None, "Alice", "lobby-2");
        let names: Vec<_> = view.players.unwrap().into_iter().map(|e| e.name).collect();
        assert!(!names.contains(&"Staff".to_owned()), "{names:?}");
        assert_eq!(view.header.unwrap().0, serde_json::json!("2/2"));
        let seen = crate::tab_list::view(
            &roster,
            as_moderator,
            Some(&tab),
            None,
            "Moderator",
            "lobby-2",
        );
        let expected: usize = if server == "lobby-2" { 0 } else { 1 };
        assert_eq!(
            seen.players.unwrap().len(),
            expected,
            "the moderator sees Staff on {server}"
        );
        assert_eq!(
            seen.header.unwrap().0,
            serde_json::json!(format!("3/{}", 2 + (1 - expected)))
        );
        assert_eq!(roster.count_for(Viewer::Public, None), 2);
        assert_eq!(
            roster.count_for(Viewer::Backend, Some(server)),
            usize::from(server == "lobby-2") * 2
        );
        assert!(!players.names_for(as_alice).contains(&"Staff".to_owned()));
        assert!(
            players
                .names_for(as_moderator)
                .contains(&"Staff".to_owned())
        );
        assert_eq!(
            roster.count_on_server(server),
            1 + usize::from(server == "lobby-2") * 2
        );
    };
    check_hidden("lobby-1");
    // The capacity check behind idle and manual stops counts the hidden player.
    let managed = crate::managed::ManagedServers::new(&config, roster.clone());
    managed.launch().unwrap();
    let refused = managed.stop("lobby-1").await.unwrap_err();
    assert_eq!(
        refused.kind(),
        std::io::ErrorKind::ResourceBusy,
        "{refused}"
    );
    managed.shutdown().await;
    // A switch keeps the rule, resolved again for the new server.
    hidden.1.set_server("lobby-2");
    hidden.0.set_server("lobby-2");
    assert_eq!(
        players.get("Staff").unwrap().unwrap().visible_to.as_deref(),
        Some("vanish.see")
    );
    check_hidden("lobby-2");
    // So does a reconnect: the rule lives in the permission store.
    drop(hidden);
    let _again = join(staff, "Staff", "lobby-1");
    check_hidden("lobby-1");
}

#[test]
fn version_schema_and_authenticated_mode_are_required() {
    for invalid in [
        "",
        "api_version = 4",
        "api_version = 1, typo = true",
        "api_version = 1, login = true",
        "api_version = 1, commands = { hub = { permission = 'use', run = function() end } }",
        "api_version = 1, commands = { game = { run = function() end } }",
        "api_version = 1, commands = { game = { permission = 'mod.*', run = function() end } }",
        "api_version = 1, commands = { game = { permission = 'Mod.Use', run = function() end } }",
        "api_version = 1, commands = { game = { permission = 'use', run = function() end, suggest = 'worlds' } }",
        "api_version = 1, commands = { game = { permission = 'use', run = function() end, suggest = true } }",
        "api_version = 1, permissions = { Player = { use = true } }",
        "api_version = 1, queues = { missing = 1 }",
        "api_version = 1, queues = { game = 0 }",
    ] {
        assert!(
            Config::from_lua(&source(invalid), "invalid.lua").is_err(),
            "{invalid}"
        );
    }
    let offline = source("api_version = 1").replace("online_mode = true", "online_mode = false");
    assert!(
        Config::from_lua(&offline, "offline.lua")
            .unwrap_err()
            .to_string()
            .contains("requires authentication")
    );
}

#[tokio::test]
async fn verified_identity_permissions_and_command_arguments_reach_lua() {
    let runtime = runtime(
        r#"api_version = 1,
        login = function(ctx)
            assert(ctx.api_version == 1 and ctx.authenticated)
            assert(ctx.uuid == '01010101010101010101010101010101' and ctx.name == 'Player1')
            assert(ctx.server == nil)
        end,
        initial_server = function(ctx) if ctx.has_permission('route.game') then return {server='game'} end end,
        commands = { game = { permission='route.game', run=function(ctx)
            assert(ctx.command == 'game' and ctx.args == 'one two')
            return {server='game'}
        end } }
    "#,
    );
    grant(
        &runtime,
        "01010101010101010101010101010101",
        &["route.game"],
    );
    let allowed = runtime.session(context(1)).unwrap();
    assert_eq!(allowed.decision("login").await.unwrap(), Action::Continue);
    assert_eq!(
        allowed.decision("initial_server").await.unwrap(),
        Action::Server("game".into())
    );
    assert_eq!(
        allowed.command("game one two").await.unwrap(),
        Action::Server("game".into())
    );
    let denied = runtime.session(context(2)).unwrap();
    // The callback would error on these arguments if permission checking ran later.
    assert!(matches!(
        denied.command("game wrong").await.unwrap(),
        Action::Message(_)
    ));
}

#[tokio::test]
async fn command_suggestions_follow_permissions_and_complete_the_last_word() {
    let runtime = runtime(
        r#"api_version = 1,
        commands = {
            ban = { permission = 'mod.ban', run = function() end, suggest = function(ctx)
                assert(ctx.command == 'ban' and ctx.name == 'Player1')
                if ctx.position == 2 then
                    assert(ctx.args == 'Alice 1' and ctx.word == '1')
                    return { { text = '1d', tooltip = 'One day' }, '1h', '30m' }
                end
                assert(ctx.position == 1 and ctx.args == ctx.word)
                return { 'Alice', 'alfred', 'Bob', 'Alice' }
            end },
            go = { permission = 'mod.go', run = function() end, suggest = 'servers' },
            who = { permission = 'mod.who', run = function() end, suggest = 'players' },
            plain = { permission = 'mod.go', run = function() end },
        },
    "#,
    );
    grant(
        &runtime,
        "01010101010101010101010101010101",
        &["mod.ban", "mod.go"],
    );
    let texts = |suggestions: Vec<Suggestion>| -> Vec<String> {
        suggestions.into_iter().map(|s| s.text).collect()
    };
    let servers = || vec!["lobby".to_owned(), "Game".to_owned(), "lounge".to_owned()];
    let allowed = runtime.session(context(1)).unwrap();
    assert_eq!(
        allowed.visible_commands(),
        [
            ("ban".into(), true),
            ("go".into(), true),
            ("plain".into(), false)
        ]
    );
    assert_eq!(
        texts(allowed.suggest("ban", "AL", servers).await.unwrap()),
        ["Alice", "alfred"]
    );
    assert_eq!(
        texts(allowed.suggest("ban", "", servers).await.unwrap()).len(),
        3
    );
    assert_eq!(
        allowed.suggest("ban", "Alice 1", servers).await.unwrap(),
        [
            Suggestion {
                text: "1d".into(),
                tooltip: Some("One day".into())
            },
            Suggestion {
                text: "1h".into(),
                tooltip: None
            },
        ]
    );
    assert_eq!(
        texts(allowed.suggest("go", "", servers).await.unwrap()),
        ["Game", "lobby", "lounge"]
    );
    assert_eq!(
        texts(allowed.suggest("go", "lo", servers).await.unwrap()),
        ["lobby", "lounge"]
    );
    assert!(
        allowed
            .suggest("plain", "", servers)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        allowed
            .suggest("missing", "", servers)
            .await
            .unwrap()
            .is_empty()
    );
    // Without the permission there are neither tree entries nor suggestions,
    // and the suggest function never runs.
    assert!(
        allowed
            .suggest("who", "", servers)
            .await
            .unwrap()
            .is_empty()
    );
    let denied = runtime.session(context(2)).unwrap();
    assert!(denied.visible_commands().is_empty());
    assert!(denied.suggest("ban", "", servers).await.unwrap().is_empty());
    assert!(denied.suggest("go", "", servers).await.unwrap().is_empty());
}

#[tokio::test]
async fn commands_with_an_empty_permission_are_open_to_every_player() {
    let runtime = runtime(
        r#"api_version = 1,
        commands = {
            version = { permission = '', run = function(ctx)
                return { message = 'v1 ' .. ctx.args }
            end, suggest = function() return { 'short', 'long' } end },
            staff = { permission = 'mod.staff', run = function() end },
        },
    "#,
    );
    // The player holds no node at all.
    let player = runtime.session(context(2)).unwrap();
    assert_eq!(player.visible_commands(), [("version".into(), true)]);
    let suggestions = player.suggest("version", "sh", Vec::new).await.unwrap();
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].text, "short");
    assert_eq!(
        player.command("version long").await.unwrap(),
        Action::Message("v1 long".into())
    );
    assert!(matches!(
        player.command("staff").await.unwrap(),
        Action::Message(_)
    ));
    // The empty permission is no node: `/perms doctor` and `rift check` only
    // hear about commands that need one, and nothing registers it.
    assert_eq!(
        runtime.command_permissions(),
        [("staff".to_owned(), "mod.staff".to_owned())]
    );
    assert!(
        runtime
            .permissions()
            .registered()
            .unwrap()
            .iter()
            .all(|registered| !registered.permission.node.is_empty())
    );
}

#[tokio::test]
async fn declarative_player_suggestions_list_joined_players() {
    let runtime = runtime(
        r#"api_version = 1,
        commands = { msg = { permission = 'use', run = function() end, suggest = 'players' } }"#,
    );
    grant(&runtime, "*", &["use"]);
    let _bob = runtime.players().register(3, &[3; 16], "bob", 774, "lobby");
    let _alice = runtime
        .players()
        .register(4, &[4; 16], "Alice", 774, "game");
    let session = runtime.session(context(1)).unwrap();
    let texts = |suggestions: Vec<Suggestion>| -> Vec<String> {
        suggestions.into_iter().map(|s| s.text).collect()
    };
    assert_eq!(
        texts(session.suggest("msg", "", Vec::new).await.unwrap()),
        ["Alice", "bob"]
    );
    // Every word completes, as Bukkit's default does.
    assert_eq!(
        texts(session.suggest("msg", "Alice B", Vec::new).await.unwrap()),
        ["bob"]
    );
}

#[tokio::test]
async fn malformed_or_slow_suggestions_fail_without_suggesting() {
    for body in [
        "return 'Alice'",
        "return { 1 }",
        "return { '' }",
        "return { 'a\\nb' }",
        "return { string.rep('a', 257) }",
        "return { { text = 'a', typo = 1 } }",
        "return { { tooltip = 'a' } }",
        "return { first = 'a' }",
        "local t = {} for i = 1, 1025 do t[i] = 'a' end return t",
        "while true do end",
        "error('broken')",
    ] {
        let runtime = runtime(&format!(
            "api_version = 1, commands = {{ ban = {{ permission = 'use', run = function() end, suggest = function() {body} end }} }}"
        ));
        grant(&runtime, "*", &["use"]);
        let session = runtime.session(context(1)).unwrap();
        assert!(
            timeout(Duration::from_secs(1), session.suggest("ban", "", Vec::new))
                .await
                .unwrap()
                .is_err(),
            "{body}"
        );
    }
    let runtime = runtime(
        "api_version = 1, commands = { ban = { permission = 'use', run = function() end, suggest = function() local t = {} for i = 1, 1000 do t[i] = 'p' .. i end return t end } }",
    );
    grant(&runtime, "*", &["use"]);
    let session = runtime.session(context(1)).unwrap();
    assert_eq!(
        session.suggest("ban", "", Vec::new).await.unwrap().len(),
        MAX_SUGGESTIONS
    );
    assert!(
        session
            .suggest("ban", "", Vec::new)
            .await
            .unwrap()
            .iter()
            .all(|s| s.tooltip.is_none())
    );
    assert!(
        session
            .suggest("ban", "nil", Vec::new)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn malformed_decisions_unknown_servers_and_runaway_callbacks_fail_closed() {
    for result in [
        "true",
        "{}",
        "{server='missing'}",
        "{server='game', deny='no'}",
        "{server=string.rep('a', 1025)}",
        "{queue='game'}",
    ] {
        let runtime = runtime(&format!(
            "api_version=1, initial_server=function() return {result} end"
        ));
        assert!(
            runtime
                .session(context(1))
                .unwrap()
                .decision("initial_server")
                .await
                .is_err(),
            "{result}"
        );
    }
    for body in [
        "while true do end",
        "return {server='game'}",
        "error('closed')",
    ] {
        let runtime = runtime(&format!("api_version=1, login=function() {body} end"));
        assert!(
            timeout(
                Duration::from_secs(1),
                runtime.session(context(1)).unwrap().decision("login")
            )
            .await
            .unwrap()
            .is_err()
        );
    }
}

#[tokio::test]
async fn deny_reasons_and_messages_may_be_chat_component_tables() {
    use crate::text::Text;
    let runtime = runtime(
        r#"api_version=1,
        login=function(ctx)
            if ctx.name == 'Player1' then
                return { deny = { text = 'Banned', color = 'red', extra = { { text = '\nspam', italic = true } } } }
            end
            if ctx.name == 'Player2' then return { deny = {} } end
            if ctx.name == 'Player3' then return { deny = { text = function() end } } end
            if ctx.name == 'Player4' then return { deny = 'plain' } end
        end,
        before_transfer=function(ctx) return { deny = { { text = 'Not now', color = 'gold' } } } end"#,
    );
    let banned = Text::Component(serde_json::json!({
        "text": "Banned", "color": "red", "extra": [{"text": "\nspam", "italic": true}],
    }));
    assert_eq!(
        runtime
            .session(context(1))
            .unwrap()
            .decision("login")
            .await
            .unwrap(),
        Action::Deny(banned)
    );
    for id in [2, 3] {
        let error = runtime
            .session(context(id))
            .unwrap()
            .decision("login")
            .await;
        assert!(error.is_err(), "Player{id}");
    }
    assert_eq!(
        runtime
            .session(context(4))
            .unwrap()
            .decision("login")
            .await
            .unwrap(),
        Action::Deny("plain".into())
    );
    let error = runtime
        .session(context(1))
        .unwrap()
        .before_transfer("game", "command")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "Not now");
    assert_eq!(
        crate::text::reason(&error),
        Text::Component(serde_json::json!([{"text": "Not now", "color": "gold"}]))
    );
}

#[tokio::test]
async fn fifo_capacity_cannot_be_bypassed_and_drop_releases_tickets_and_leases() {
    let runtime = runtime("api_version=1, queues={game=1}");
    let mut occupant = runtime.session(context(1)).unwrap();
    occupant.reserve("game").unwrap();
    occupant.ready("game").await;
    let mut first = runtime.session(context(2)).unwrap();
    let second = runtime.session(context(3)).unwrap();
    assert_eq!(first.enqueue("game").unwrap(), 1);
    assert_eq!(first.enqueue("game").unwrap(), 1);
    assert_eq!(second.enqueue("game").unwrap(), 2);
    assert!(first.reserve("game").is_err());
    drop(occupant);
    assert!(second.reserve("game").is_err());
    first.before_transfer("game", "queue").await.unwrap();
    assert!(second.reserve("game").is_err());
    first.ready("game").await;
    assert!(second.reserve("game").is_err());
    drop(first);
    second.reserve("game").unwrap();
    drop(second);
    let host = runtime.host.lock().unwrap();
    assert!(host.leases.is_empty());
    assert!(host.queues.values().all(VecDeque::is_empty));
}

#[tokio::test]
async fn aborted_transfer_releases_only_target_and_notifications_are_ordered() {
    let runtime = runtime(
        r#"api_version=1, queues={game=1},
        join=function(ctx) rift.publish('events', 'join:' .. ctx.server) end,
        before_transfer=function(ctx)
            if ctx.target == 'blocked' then return {deny='No access'} end
            rift.publish('events', 'before:' .. ctx.server .. ':' .. ctx.reason)
        end,
        after_transfer=function(ctx) rift.publish('events', 'after:' .. tostring(ctx.success)) end,
        disconnect=function(ctx) rift.publish('events', 'disconnect:' .. ctx.server) end
    "#,
    );
    let mut events = runtime.broker.subscribe("events", None).unwrap();
    let mut player = runtime.session(context(1)).unwrap();
    player.reserve("lobby").unwrap();
    player.ready("lobby").await;
    assert!(player.before_transfer("blocked", "command").await.is_err());
    player.before_transfer("game", "admin").await.unwrap();
    player.transfer_failed().await;
    assert_eq!(
        runtime.host.lock().unwrap().leases[&1],
        BTreeSet::from(["lobby".into()])
    );
    player.before_transfer("game", "queue").await.unwrap();
    player.ready("game").await;
    // Cancellation in the middle of another transfer still emits its failed
    // completion before disconnect, and releases leases synchronously.
    player.before_transfer("lobby", "recovery").await.unwrap();
    drop(player);
    assert!(runtime.host.lock().unwrap().leases.is_empty());
    for expected in [
        "join:lobby",
        "before:lobby:admin",
        "after:false",
        "before:lobby:queue",
        "after:true",
        "before:game:recovery",
        "after:false",
        "disconnect:game",
    ] {
        let message = timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.payload.as_ref(), expected.as_bytes());
    }
}

#[tokio::test]
async fn reload_pins_callbacks_and_permissions_but_shares_capacity_and_fifo() {
    let old = runtime(
        "api_version=1, queues={game=1}, initial_server=function() return {server='lobby'} end",
    );
    let next = Extensions::new(
        &config(
            "api_version=1, queues={game=1}, initial_server=function() return {server='game'} end",
        ),
        old.broker.clone(),
        Some(&old),
    )
    .unwrap();
    let existing = old.session(context(1)).unwrap();
    let new = next.session(context(2)).unwrap();
    existing.enqueue("game").unwrap();
    assert_eq!(new.enqueue("game").unwrap(), 2);
    assert_eq!(
        existing.decision("initial_server").await.unwrap(),
        Action::Server("lobby".into())
    );
    assert_eq!(
        new.decision("initial_server").await.unwrap(),
        Action::Server("game".into())
    );
    let permits = old
        .slots
        .clone()
        .acquire_many_owned(script::MAX_CONCURRENT as u32)
        .await
        .unwrap();
    assert!(new.decision("initial_server").await.is_err());
    drop(existing); // host cleanup does not need Lua worker capacity
    assert!(new.reserve("game").is_ok());
    drop(permits);
}

#[tokio::test]
async fn local_lua_state_does_not_survive_invocations_and_expired_queue_cleans_up() {
    let cfg = Config::from_lua(&format!("local calls=0; {}", source("api_version=1, queues={game=1}, login=function() calls=calls+1; assert(calls==1) end")), "state.lua").unwrap();
    let runtime = Extensions::new(&cfg, Broker::new(Default::default()).unwrap(), None).unwrap();
    let player = runtime.session(context(1)).unwrap();
    for _ in 0..2 {
        player.decision("login").await.unwrap();
    }
    player.enqueue("game").unwrap();
    runtime.host.lock().unwrap().queues.get_mut("game").unwrap()[0].1 =
        Instant::now() - Duration::from_secs(301);
    assert!(player.queued_target().is_err());
    assert!(player.queued_target().unwrap().is_none());
}

#[test]
fn timeout_and_cancellation_retain_worker_limits_and_cannot_reorder_teardown() {
    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    tokio_runtime.block_on(async {
        for cancel in [false, true] {
            let runtime = runtime("api_version=1, queues={game=1}, login=function() rift.publish('event','login') end, disconnect=function() rift.publish('event','disconnect') end");
            let mut events = runtime.broker.subscribe("event", None).unwrap();
            let (release, held) = std::sync::mpsc::channel();
            let (ready, started) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || { ready.send(()).unwrap(); held.recv_timeout(Duration::from_secs(5)).unwrap(); });
            started.recv_timeout(Duration::from_secs(1)).unwrap();
            let session = runtime.session(context(1)).unwrap();
            session.reserve("game").unwrap();
            let task = tokio::spawn(async move { session.decision("login").await });
            timeout(Duration::from_secs(1), async {
                while runtime.slots.available_permits() == script::MAX_CONCURRENT { tokio::task::yield_now().await; }
            }).await.unwrap();
            if cancel { task.abort(); assert!(task.await.unwrap_err().is_cancelled()); }
            else { assert!(task.await.unwrap().is_err()); }
            assert!(runtime.host.lock().unwrap().leases.is_empty());
            assert_eq!(runtime.slots.available_permits(), script::MAX_CONCURRENT - 1);
            // Let the deadline expire while the actual worker is still queued.
            tokio::time::sleep(script::EXECUTION_TIMEOUT).await;
            release.send(()).unwrap(); blocker.await.unwrap();
            timeout(Duration::from_secs(1), async {
                while runtime.slots.available_permits() != script::MAX_CONCURRENT { tokio::task::yield_now().await; }
            }).await.unwrap();
            // The expired login cannot publish, and teardown did not overtake it.
            assert!(timeout(Duration::from_millis(10), events.recv()).await.is_err());
        }
    });
}

#[test]
fn concurrent_admissions_never_overbook_the_last_slot() {
    let runtime = runtime("api_version=1, queues={game=1}");
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let attempts: Vec<_> = (1..=2)
        .map(|id| {
            let session = runtime.session(context(id)).unwrap();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let result = session.reserve("game");
                (session, result)
            })
        })
        .collect();
    let results: Vec<_> = attempts.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    drop(results);
    assert!(runtime.host.lock().unwrap().leases.is_empty());
}

fn dynamic_config(api_version: u32) -> Config {
    let extension = format!(
        "api_version={api_version}, commands={{warp={{permission='route.dynamic',run=function(ctx) return {{server=ctx.args}} end}}}}"
    );
    let source = source(&extension).replace(
        "routes = { public = 'lobby' }",
        "routes = { public = 'lobby' }, service_groups = { arena = { directory='instances/{name}', command={'java','--port','{port}'}, port_range={25010,25011} } }",
    );
    Config::from_lua(&source, "dynamic-extensions.lua").unwrap()
}

#[tokio::test]
async fn existing_extension_sessions_follow_committed_instance_destinations() {
    for api_version in [1, 2] {
        let config = dynamic_config(api_version);
        let runtime =
            Extensions::new(&config, Broker::new(Default::default()).unwrap(), None).unwrap();
        grant(&runtime, "*", &["route.dynamic"]);
        let existing = runtime.session(context(1)).unwrap();
        assert!(existing.command("warp arena-1").await.is_err());
        assert_eq!(
            existing.command("warp arena").await.unwrap(),
            Action::Server("arena".into())
        );

        let mut added = config.clone();
        added.add_instance("arena", "arena-1", 25010).unwrap();
        let candidate = Extensions::new(&added, runtime.broker.clone(), Some(&runtime)).unwrap();
        // Preparing a transaction must not publish its backend set to sessions.
        assert!(existing.command("warp arena-1").await.is_err());
        assert!(
            candidate
                .session(context(2))
                .unwrap()
                .command("warp arena-1")
                .await
                .is_err()
        );
        drop(candidate);
        assert!(existing.command("warp arena-1").await.is_err());

        let committed = Extensions::new(&added, runtime.broker.clone(), Some(&runtime)).unwrap();
        committed.update_destinations(&added);
        assert_eq!(
            existing.command("warp arena-1").await.unwrap(),
            Action::Server("arena-1".into())
        );
        assert_eq!(
            committed
                .session(context(3))
                .unwrap()
                .command("warp arena-1")
                .await
                .unwrap(),
            Action::Server("arena-1".into())
        );

        let removed = Extensions::new(&config, runtime.broker.clone(), Some(&committed)).unwrap();
        assert_eq!(
            existing.command("warp arena-1").await.unwrap(),
            Action::Server("arena-1".into())
        );
        removed.update_destinations(&config);
        assert!(existing.command("warp arena-1").await.is_err());
        assert!(
            committed
                .session(context(4))
                .unwrap()
                .command("warp arena-1")
                .await
                .is_err()
        );
        // The stable service-group name remains eligible after its last removal.
        assert_eq!(
            existing.command("warp arena").await.unwrap(),
            Action::Server("arena".into())
        );
    }
}

#[tokio::test]
async fn rejected_extension_candidate_cannot_publish_new_instance_destinations() {
    for api_version in [1, 2] {
        let config = dynamic_config(api_version);
        let runtime =
            Extensions::new(&config, Broker::new(Default::default()).unwrap(), None).unwrap();
        grant(&runtime, "*", &["route.dynamic"]);
        let existing = runtime.session(context(1)).unwrap();
        let mut rejected = config.clone();
        rejected.add_instance("arena", "arena-1", 25010).unwrap();
        rejected
            .extensions
            .as_mut()
            .unwrap()
            .queues
            .insert("game".into(), 1);
        rejected.validate().unwrap();
        assert!(Extensions::new(&rejected, runtime.broker.clone(), Some(&runtime)).is_err());
        assert!(existing.command("warp arena-1").await.is_err());
        assert_eq!(
            existing.command("warp lobby").await.unwrap(),
            Action::Server("lobby".into())
        );
    }
}
