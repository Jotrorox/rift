use super::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

fn source(extension: &str) -> String {
    format!(
        "return {{ listeners = {{ public = '127.0.0.1:0' }}, backends = {{ lobby = '127.0.0.1:25001', game = '127.0.0.1:25002' }}, routes = {{ public = 'lobby' }}, authentication = {{ online_mode = true }}, forwarding = {{ mode = 'velocity', secret_env = 'TEST_SECRET' }}, extensions = {{ {extension} }} }}"
    )
}

fn config(extension: &str) -> Config {
    Config::from_lua(&source(extension), "extension-v2-test.lua").unwrap()
}

fn runtime(extension: &str) -> Extensions {
    Extensions::new(
        &config(extension),
        Broker::new(Default::default()).unwrap(),
        None,
    )
    .unwrap()
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

struct StateFile(PathBuf);

impl StateFile {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(std::env::temp_dir().join(format!(
            "rift-extension-v2-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn lua_path(&self) -> String {
        serde_json::to_string(&self.0.to_string_lossy()).unwrap()
    }
}

impl Drop for StateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn v2_schema_rejects_unsupported_versions_and_malformed_jobs() {
    for invalid in [
        "api_version=4",
        "api_version=1, jobs={}",
        "api_version=1, storage={path='state.json'}",
        "api_version=1, integrations={}",
        "api_version=2, typo=true",
        "api_version=2, jobs={tick={every_ms=99,run=function() end}}",
        "api_version=2, jobs={tick={every_ms=86400001,run=function() end}}",
        "api_version=2, jobs={tick={every_ms=100.5,run=function() end}}",
        "api_version=2, jobs={tick={every_ms=100}}",
        "api_version=2, jobs={tick={every_ms=100,run=true}}",
        "api_version=2, jobs={tick={every_ms=100,run=function() end,typo=true}}",
        "api_version=2, jobs={['Bad Name']={every_ms=100,run=function() end}}",
        "api_version=2, jobs={[1]={every_ms=100,run=function() end}}",
    ] {
        assert!(
            Config::from_lua(&source(invalid), "invalid-v2.lua").is_err(),
            "{invalid}"
        );
    }
    config("api_version=2, jobs={tick={every_ms=100,run=function() end}}");
}

#[test]
fn loading_configuration_cannot_execute_runtime_side_effects() {
    let state = StateFile::new();
    for operation in [
        "rift.store.set('test','key','value')",
        "rift.permissions.user('01010101010101010101010101010101'):set('route.game',true)",
        "rift.http.request('account',{})",
        "rift.players.broadcast('hello')",
    ] {
        let extension = format!(
            "api_version=2, storage={{path={}}}, integrations={{account={{url='http://127.0.0.1:9/'}}}}",
            state.lua_path()
        );
        let script = format!("{operation}; {}", source(&extension));
        assert!(
            Config::from_lua(&script, "side-effect.lua").is_err(),
            "{operation}"
        );
        assert!(!state.0.exists());
    }
}

#[test]
fn reload_rejects_storage_version_and_capacity_changes_before_opening_new_storage() {
    let old = runtime("api_version=2");
    let state = StateFile::new();
    for extension in [
        "api_version=1".to_owned(),
        "api_version=2, queues={game=1}".to_owned(),
        format!("api_version=2, storage={{path={}}}", state.lua_path()),
    ] {
        assert!(Extensions::new(&config(&extension), old.broker.clone(), Some(&old)).is_err());
        assert!(!state.0.exists());
    }
}

#[tokio::test]
async fn v1_callbacks_cannot_access_v2_host_capabilities() {
    for operation in [
        "rift.store.set('test','key','value')",
        "rift.permissions.check('01010101010101010101010101010101','route.game')",
        "rift.http.request('account',{})",
        "rift.players.list()",
    ] {
        let runtime = runtime(&format!("api_version=1,login=function() {operation} end"));
        assert!(
            runtime
                .session(context(1))
                .unwrap()
                .decision("login")
                .await
                .is_err(),
            "{operation}"
        );
    }
}

#[tokio::test]
async fn captured_store_functions_preserve_binary_values_across_calls_and_reload() {
    let cfg = Config::from_lua(
        &format!(
            "local get,set,increment=rift.store.get,rift.store.set,rift.store.increment; {}",
            source(
                r#"api_version=2,
        login=function(ctx)
            assert(ctx.api_version == 2)
            local count=increment('visits',ctx.uuid,1)
            set('profiles',ctx.uuid,ctx.name..string.char(0,255)..tostring(count))
        end,
        initial_server=function(ctx)
            local value=get('profiles',ctx.uuid)
            assert(value==ctx.name..string.char(0,255)..'2')
            assert(get('visits',ctx.uuid)=='2')
            assert(get('other',ctx.uuid)==nil)
            return {server='game'}
        end"#
            )
        ),
        "captured-store.lua",
    )
    .unwrap();
    let old = Extensions::new(&cfg, Broker::new(Default::default()).unwrap(), None).unwrap();
    let player = old.session(context(1)).unwrap();
    player.decision("login").await.unwrap();
    player.decision("login").await.unwrap();
    let next = Extensions::new(&cfg, old.broker.clone(), Some(&old)).unwrap();
    next.activate();
    assert_eq!(
        next.session(context(1))
            .unwrap()
            .decision("initial_server")
            .await
            .unwrap(),
        Action::Server("game".into())
    );
}

#[test]
fn durable_store_survives_restart_and_delete_persists() {
    // Disk sync latency on shared runners can exceed the session's 50 ms budget.
    // Test persistence with an explicit budget; timeout behavior is tested separately.
    let login = |runtime: &Extensions| {
        assert_eq!(
            runtime
                .script
                .as_ref()
                .unwrap()
                .evaluate(
                    "login",
                    &context(1),
                    Instant::now() + Duration::from_secs(30),
                    runtime,
                )
                .unwrap(),
            Action::Continue
        );
    };
    let state = StateFile::new();
    let settings = format!("api_version=2, storage={{path={}}}", state.lua_path());
    {
        let runtime = runtime(&format!(
            r#"{settings}, login=function()
            rift.store.set('profiles','player','member'..string.char(0,255))
            rift.store.set('flags','disabled','false')
        end"#
        ));
        login(&runtime);
    }
    assert!(state.0.is_file());
    {
        let runtime = runtime(&format!(
            r#"{settings}, login=function()
            local value=rift.store.get('profiles','player')
            assert(value=='member'..string.char(0,255))
            assert(rift.store.get('flags','disabled')=='false')
            rift.store.delete('profiles','player')
        end"#
        ));
        login(&runtime);
    }
    let runtime = runtime(&format!(
        r#"{settings}, login=function()
        assert(rift.store.get('profiles','player')==nil)
        assert(rift.store.get('flags','disabled')=='false')
    end"#
    ));
    login(&runtime);
}

#[tokio::test]
async fn store_increment_is_atomic_across_concurrent_sessions() {
    let runtime = runtime(
        r#"api_version=2,
        login=function() rift.store.increment('counts','visits',1) end,
        initial_server=function()
            assert(rift.store.get('counts','visits')=='40')
            return {server='game'}
        end"#,
    );
    let mut tasks = Vec::new();
    for id in 1..=4 {
        let session = runtime.session(context(id)).unwrap();
        tasks.push(tokio::spawn(async move {
            for _ in 0..10 {
                session.decision("login").await.unwrap();
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    runtime
        .session(context(10))
        .unwrap()
        .decision("initial_server")
        .await
        .unwrap();
}

#[tokio::test]
async fn live_permission_writes_apply_within_the_same_session() {
    let runtime = runtime(
        r#"api_version=3,
        groups={default={permissions={['route.game']=true,['permissions.manage']=true}}},
        initial_server=function(ctx)
            assert(ctx.has_permission('route.game')==(rift.permissions.check(ctx.uuid,'route.game')==true))
            return {server=ctx.has_permission('route.game') and 'game' or 'lobby'}
        end,
        commands={
            game={permission='route.game',run=function() return {server='game'} end},
            access={permission='permissions.manage',run=function(ctx)
                local user=rift.permissions.user(ctx.uuid)
                if ctx.args=='revoke' then user:set('route.game',false)
                elseif ctx.args=='grant' then user:set('route.game',true)
                elseif ctx.args=='reset' then user:unset('route.game')
                end
                return {message=tostring(ctx.has_permission('route.game'))}
            end}}
    "#,
    );
    let player = runtime.session(context(1)).unwrap();
    assert_eq!(
        player.command("game").await.unwrap(),
        Action::Server("game".into())
    );
    for (operation, granted) in [
        ("revoke", false),
        ("reset", true),
        ("revoke", false),
        ("grant", true),
        ("reset", true),
    ] {
        assert_eq!(
            player
                .command(&format!("access {operation}"))
                .await
                .unwrap(),
            Action::Message(granted.to_string().into())
        );
        assert_eq!(
            player.decision("initial_server").await.unwrap(),
            Action::Server(if granted { "game" } else { "lobby" }.into())
        );
        let action = player.command("game").await.unwrap();
        if granted {
            assert_eq!(action, Action::Server("game".into()));
        } else {
            assert!(matches!(action, Action::Message(_)));
        }
    }
    let log = runtime.permissions().log(&Default::default()).unwrap();
    assert!(
        log.iter()
            .all(|event| event.actor.to_string() == "lua:command.access"
                || event.actor.to_string() == "system")
    );
}

#[tokio::test]
async fn live_revocation_blocks_all_transfer_reasons_before_capacity_reservation() {
    let runtime = runtime(
        r#"api_version=3, queues={game=1},
        groups={default={permissions={['route.game']=true,['permissions.manage']=true}}},
        before_transfer=function(ctx)
            if ctx.target=='game' and not ctx.has_permission('route.game') then
                return {deny='Access revoked'}
            end
        end,
        commands={access={permission='permissions.manage',run=function(ctx)
            rift.permissions.user(ctx.uuid):set('route.game',ctx.args=='grant')
        end}}
    "#,
    );
    let mut session = runtime.session(context(1)).unwrap();
    session.reserve("lobby").unwrap();
    session.ready("lobby").await;
    session.enqueue("game").unwrap();
    session.command("access revoke").await.unwrap();
    for reason in ["command", "admin", "queue", "recovery", "bungeecord"] {
        let error = session.before_transfer("game", reason).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(session.pending.is_none());
        assert_eq!(
            runtime.host.lock().unwrap().leases[&1],
            BTreeSet::from(["lobby".into()])
        );
    }
    session.command("access grant").await.unwrap();
    session.before_transfer("game", "queue").await.unwrap();
    assert!(runtime.host.lock().unwrap().leases[&1].contains("game"));
    session.transfer_failed().await;
}

#[tokio::test]
async fn malformed_permission_updates_fail_without_revoking_existing_grants() {
    for call in [
        "rift.permissions.user(ctx.uuid):set('route.game',1)",
        "rift.permissions.user(ctx.uuid):set('route.game','false')",
        "rift.permissions.user('not-a-uuid'):set('route.game',false)",
        "rift.permissions.user(ctx.uuid):set('bad node',false)",
        "rift.permissions.user(ctx.uuid):set('route.game',false,{expires_in=-1})",
        "rift.permissions.user(ctx.uuid):set('route.game',false,{typo=1})",
        "rift.permissions.user(ctx.uuid):set('route.game',false,{contexts={server={'a','b'}}})",
        "rift.permissions.group('missing'):set('route.game',false)",
        "rift.permissions.group('default'):delete()",
        "rift.permissions.has(ctx.uuid,'route.game')",
    ] {
        let runtime = runtime(&format!(
            r#"api_version=3,
            groups={{default={{permissions={{['route.game']=true}}}}}},
            login=function(ctx) {call} end,
            initial_server=function(ctx)
                assert(ctx.has_permission('route.game'))
                return {{server='game'}}
            end"#
        ));
        let player = runtime.session(context(1)).unwrap();
        assert!(player.decision("login").await.is_err(), "{call}");
        assert_eq!(
            player.decision("initial_server").await.unwrap(),
            Action::Server("game".into())
        );
    }
}

#[tokio::test]
async fn permission_reload_mirrors_groups_and_keeps_callback_generations_pinned() {
    let old = runtime(
        r#"api_version=2,
        groups={default={permissions={inspect=true,['route.game']=true}}},
        commands={check={permission='inspect',run=function(ctx)
            return {message='old:'..tostring(ctx.has_permission('route.game'))}
        end}}"#,
    );
    let existing = old.session(context(1)).unwrap();
    let cfg = config(
        r#"api_version=2,
        groups={default={permissions={inspect=true}}},
        commands={check={permission='inspect',run=function(ctx)
            return {message='new:'..tostring(ctx.has_permission('route.game'))}
        end}}"#,
    );
    let revision = old.permissions().revision();
    let rejected = Extensions::new(&cfg, old.broker.clone(), Some(&old)).unwrap();
    assert_eq!(
        existing.command("check").await.unwrap(),
        Action::Message("old:true".into())
    );
    drop(rejected);
    assert_eq!(old.permissions().revision(), revision);
    let next = Extensions::new(&cfg, old.broker.clone(), Some(&old)).unwrap();
    next.activate();
    assert_eq!(old.permissions().revision(), revision + 1);
    // Activating the same groups again changes nothing.
    next.activate();
    assert_eq!(old.permissions().revision(), revision + 1);
    assert_eq!(
        existing.command("check").await.unwrap(),
        Action::Message("old:false".into())
    );
    assert_eq!(
        next.session(context(2))
            .unwrap()
            .command("check")
            .await
            .unwrap(),
        Action::Message("new:false".into())
    );
}

#[tokio::test]
async fn v1_commands_are_authorized_by_the_permission_store() {
    let old = runtime(
        r#"api_version=1,
        commands={game={permission='route.game',run=function(ctx)
            assert(ctx.api_version==1 and ctx.has_permission('route.game'))
            return {server='game'}
        end}}"#,
    );
    let existing = old.session(context(1)).unwrap();
    assert!(matches!(
        existing.command("game").await.unwrap(),
        Action::Message(_)
    ));
    let grant = rift_permissions::Change::SetNode {
        subject: SubjectRef::User(Uuid::parse(&context(1).uuid).unwrap()),
        node: Node::permission("route.game", true).unwrap(),
    };
    old.permissions().apply(Actor::Cli, grant).unwrap();
    let next = Extensions::new(&config(&old_source()), old.broker.clone(), Some(&old)).unwrap();
    next.activate();
    for session in [&existing, &next.session(context(1)).unwrap()] {
        assert_eq!(
            session.command("game").await.unwrap(),
            Action::Server("game".into())
        );
    }
    assert!(matches!(
        next.session(context(2))
            .unwrap()
            .command("game")
            .await
            .unwrap(),
        Action::Message(_)
    ));
}

fn old_source() -> String {
    r#"api_version=1,
        commands={game={permission='route.game',run=function() return {server='game'} end}}"#
        .into()
}

#[tokio::test]
async fn scheduler_uses_job_context_delays_first_run_and_stops_on_shutdown() {
    let runtime = runtime(
        r#"api_version=2,jobs={tick={every_ms=100,run=function(ctx)
        assert(ctx.api_version==2 and ctx.job=='tick')
        assert(ctx.uuid==nil and ctx.name==nil and ctx.authenticated==nil and ctx.connection_id==nil)
        local count=rift.store.increment('jobs',ctx.job,1)
        rift.publish('ticks',tostring(count))
    end}}"#,
    );
    let mut events = runtime.broker.subscribe("ticks", None).unwrap();
    let scheduler = runtime.scheduler();
    assert!(
        timeout(Duration::from_millis(40), events.recv())
            .await
            .is_err()
    );
    for expected in [b"1", b"2"] {
        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.payload.as_ref(), expected);
    }
    scheduler.shutdown().await;
    assert!(
        timeout(Duration::from_millis(220), events.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn scheduled_jobs_use_their_own_workers_without_backlog() {
    let runtime = runtime(
        r#"api_version=2,jobs={tick={every_ms=100,run=function()
        rift.publish('ticks','tick')
    end}}"#,
    );
    let mut events = runtime.broker.subscribe("ticks", None).unwrap();
    // Busy player callbacks do not hold jobs back...
    let players = runtime
        .slots
        .clone()
        .acquire_many_owned(script::MAX_CONCURRENT as u32)
        .await
        .unwrap();
    let scheduler = runtime.scheduler();
    timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    // ...and while every job worker is busy, ticks are skipped, not queued.
    let jobs = runtime
        .job_slots
        .clone()
        .acquire_many_owned(scheduler::MAX_CONCURRENT_JOBS as u32)
        .await
        .unwrap();
    while timeout(Duration::from_millis(50), events.recv())
        .await
        .is_ok()
    {}
    assert!(
        timeout(Duration::from_millis(220), events.recv())
            .await
            .is_err()
    );
    drop(jobs);
    timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        timeout(Duration::from_millis(40), events.recv())
            .await
            .is_err()
    );
    drop(players);
    scheduler.shutdown().await;
}

#[tokio::test]
async fn same_named_job_cannot_overlap_across_scheduler_replacement() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, blocked) = tokio::sync::oneshot::channel();
    let (accepted, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 1024];
        assert_ne!(stream.read(&mut request).await.unwrap(), 0);
        accepted.send(()).unwrap();
        blocked.await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });
    let old = runtime(&format!(
        r#"api_version=2,
        integrations={{slow={{url='http://{address}/',timeout_ms=3000}}}},
        jobs={{tick={{every_ms=100,run=function()
            rift.publish('ticks','old.start')
            rift.http.request('slow',{{}})
            rift.publish('ticks','old.done')
        end}}}}"#
    ));
    let mut events = old.broker.subscribe("ticks", None).unwrap();
    let old_scheduler = old.scheduler();
    let first = timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.payload.as_ref(), b"old.start");
    timeout(Duration::from_secs(2), received)
        .await
        .unwrap()
        .unwrap();
    assert!(
        timeout(Duration::from_millis(220), events.recv())
            .await
            .is_err()
    );
    old_scheduler.shutdown().await;

    let next = Extensions::new(
        &config(
            r#"api_version=2,
        jobs={tick={every_ms=100,run=function() rift.publish('ticks','new') end}}"#,
        ),
        old.broker.clone(),
        Some(&old),
    )
    .unwrap();
    next.activate();
    let next_scheduler = next.scheduler();
    // Aborting the old scheduler must retain its running worker's job-name gate.
    assert!(
        timeout(Duration::from_millis(220), events.recv())
            .await
            .is_err()
    );
    release.send(()).unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    for expected in [b"old.done".as_slice(), b"new".as_slice()] {
        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.payload.as_ref(), expected);
    }
    assert!(
        timeout(Duration::from_millis(40), events.recv())
            .await
            .is_err()
    );
    next_scheduler.shutdown().await;
}

#[tokio::test]
async fn configured_http_integration_is_available_through_captured_lua_api() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            let count = stream.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8(request).unwrap();
        assert!(
            request.starts_with("GET /api/status?user=player HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-network: integration-test\r\n")
        );
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-Result: ready\r\nConnection: close\r\n\r\nready").await.unwrap();
    });
    let cfg = Config::from_lua(&format!(
        "local request=rift.http.request; {}", source(&format!(r#"api_version=2,
        integrations={{account={{url='http://{address}/api/',timeout_ms=1000,headers={{['X-Network']='integration-test'}}}}}},
        login=function()
            local response=request('account',{{path='status?user=player'}})
            assert(response.status==200 and response.body=='ready' and response.headers['x-result']=='ready')
        end"#))
    ), "http-v2.lua").unwrap();
    let runtime = Extensions::new(&cfg, Broker::new(Default::default()).unwrap(), None).unwrap();
    timeout(
        Duration::from_secs(2),
        runtime.session(context(1)).unwrap().decision("login"),
    )
    .await
    .unwrap()
    .unwrap();
    timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn startup_and_reload_require_headers_env_variables() {
    let cfg = Config::from_lua(
        &source(
            "api_version=2, integrations={account={url='http://127.0.0.1:9/',headers_env={Authorization='RIFT_TEST_UNSET_ACCOUNT_TOKEN'}}}",
        ),
        "headers-env.lua",
    )
    .unwrap();
    let error = Extensions::new(&cfg, Broker::new(Default::default()).unwrap(), None)
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains("headers_env") && error.contains("set RIFT_TEST_UNSET_ACCOUNT_TOKEN"),
        "{error}"
    );
}

#[test]
fn permission_group_schema_is_validated_and_legacy_grants_rejected() {
    for invalid in [
        "api_version=1, groups={}",
        "api_version=2, groups={staff=true}",
        "api_version=2, groups={Staff={}}",
        "api_version=2, groups={staff={typo=true}}",
        "api_version=2, groups={staff={inherits='default'}}",
        "api_version=2, groups={staff={inherits={x='default'}}}",
        "api_version=2, groups={default={}, staff={inherits={'default', [3]='default', y=1}}}",
        "api_version=2, groups={staff={inherits={'Bad Name'}}}",
        "api_version=2, groups={staff={permissions={['bad node']=true}}}",
        "api_version=2, groups={staff={permissions={['network.*.ban']=true}}}",
        "api_version=2, groups={staff={permissions={['a.b']=true, ['A.b']=false}}}",
        "api_version=2, groups={staff={permissions={['network.staff']='yes'}}}",
        "api_version=2, groups={staff={weight='high'}}",
        "api_version=2, groups={staff={weight=1.5}}",
        "api_version=2, groups={staff={prefix=1}}",
        "api_version=2, groups={staff={prefix={priority=1}}}",
        "api_version=2, groups={staff={suffix={text='x', typo=1}}}",
        "api_version=2, groups={staff={meta={homes={}}}}",
        "api_version=2, groups={staff={meta={['bad key']='1'}}}",
        "api_version=2, on_permission_change=function() end",
        "api_version=3, on_permission_change=true",
        "api_version=3, context_providers={server=function() end}",
        "api_version=3, context_providers={region=true}",
        // The removed grants table, in either version, is refused with a pointer.
        "api_version=1, permissions={['*']={['network.staff']=true}}",
        "api_version=2, permissions={['01010101010101010101010101010101']={['network.*']=true}}",
        "api_version=3, permissions={}",
    ] {
        assert!(
            Config::from_lua(&source(invalid), "invalid-groups.lua").is_err(),
            "{invalid}"
        );
    }
    let error = Config::from_lua(
        &source("api_version=3, permissions={['*']={['chat.send']=true}}"),
        "legacy.lua",
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("extensions.permissions was removed")
            && error.contains("rift.permissions.user(uuid):set"),
        "{error}"
    );
    let many = (0..65)
        .map(|i| format!("g{i}={{}}"))
        .collect::<Vec<_>>()
        .join(",");
    let limited = source(&format!("api_version=2, groups={{{many}}}")).replace(
        "routes = {",
        "permissions = { limits = { groups = 64 } }, routes = {",
    );
    assert!(
        Config::from_lua(&limited, "many.lua")
            .unwrap_err()
            .to_string()
            .contains("permissions.limits.groups")
    );
    let config = config(
        r#"api_version=3,
        groups={
            default={permissions={['chat.*']=true}},
            staff={inherits={'default'}, weight=10, prefix='&c[Staff] ', meta={homes=5},
                permissions={['Network.Staff']=true, ['chat.color']=false}},
            admin={inherits={'staff','missing','admin'}, suffix={priority=3, text=' *'}},
        },
        on_permission_change=function() end,
        context_providers={region=function() return 'spawn' end}"#,
    );
    let groups = config
        .extensions
        .as_ref()
        .unwrap()
        .permission_groups()
        .unwrap();
    assert_eq!(
        groups["staff"],
        serde_json::json!({
            "inherits": ["default"], "weight": 10, "prefix": "&c[Staff] ",
            "meta": {"homes": "5"},
            "permissions": {"chat.color": false, "network.staff": true},
        })
    );
    assert_eq!(
        groups["admin"]["suffix"],
        serde_json::json!({"priority": 3, "text": " *"})
    );
}

/// Run a probe command with a generous budget: disk sync latency on shared
/// runners can exceed the session's 50 ms deadline.
fn probe(runtime: &Extensions, command: &str, args: &str) -> io::Result<String> {
    let mut input = context(1);
    input.command = Some(command.into());
    input.args = args.into();
    let action = runtime.script.as_ref().unwrap().evaluate(
        "command",
        &input,
        Instant::now() + Duration::from_secs(30),
        runtime,
    )?;
    Ok(match action {
        Action::Message(message) => message.to_string(),
        Action::Continue => String::new(),
        action => panic!("unexpected {action:?}"),
    })
}

const GROUP_PROBES: &str = r#"commands={
        add={permission='probe',run=function(ctx)
            return {message=tostring(rift.permissions.user(ctx.uuid):add_parent(ctx.args).changed)}
        end},
        remove={permission='probe',run=function(ctx)
            return {message=tostring(rift.permissions.user(ctx.uuid):remove_parent(ctx.args).changed)}
        end},
        groups={permission='probe',run=function(ctx)
            local names = {}
            for _, node in ipairs(rift.permissions.user(ctx.uuid):nodes()) do
                if node.kind == 'inherit' then names[#names + 1] = node.group end
            end
            return {message='['..table.concat(names,',')..']'}
        end},
        has={permission='probe',run=function(ctx) return {message=tostring(ctx.has_permission(ctx.args))} end},
    }"#;

#[test]
fn group_memberships_survive_config_reloads_that_drop_the_group() {
    let settings = |groups: &str| format!("api_version=3, {GROUP_PROBES}, groups={{{groups}}}");
    let full = settings(
        "default={permissions={probe=true,['network.queue']=true}},
         staff={inherits={'default'}, permissions={['network.*']=true}}",
    );
    let runtime = runtime(&full);
    assert_eq!(probe(&runtime, "has", "network.queue").unwrap(), "true");
    assert_eq!(probe(&runtime, "has", "network.staff").unwrap(), "false");
    assert_eq!(probe(&runtime, "add", "staff").unwrap(), "true");
    assert_eq!(probe(&runtime, "add", "staff").unwrap(), "false");
    assert_eq!(probe(&runtime, "has", "network.staff").unwrap(), "true");
    assert_eq!(probe(&runtime, "groups", "").unwrap(), "[staff]");
    assert!(probe(&runtime, "add", "bad name").is_err());
    // A reload that drops the group removes its configured nodes; the
    // membership stays, grants nothing, and can be removed.
    let reloaded = Extensions::new(
        &config(&settings("default={permissions={probe=true}}")),
        runtime.broker.clone(),
        Some(&runtime),
    )
    .unwrap();
    reloaded.activate();
    assert_eq!(probe(&reloaded, "has", "network.staff").unwrap(), "false");
    assert_eq!(probe(&reloaded, "groups", "").unwrap(), "[staff]");
    assert_eq!(probe(&reloaded, "remove", "staff").unwrap(), "true");
    assert_eq!(probe(&reloaded, "remove", "staff").unwrap(), "false");
    assert_eq!(probe(&reloaded, "groups", "").unwrap(), "[]");
}

#[tokio::test]
async fn commands_honor_group_and_wildcard_grants() {
    let runtime = runtime(
        r#"api_version=3,
        groups={staff={permissions={['network.*']=true}}, ranks={permissions={['ranks.manage']=true}}},
        commands={
            staff={permission='network.staff',run=function() return {server='game'} end},
            rank={permission='ranks.manage',run=function(ctx)
                local user=rift.permissions.user(string.rep('02',16))
                if ctx.args=='add' then user:add_parent('staff') else user:remove_parent('staff') end
            end},
        }"#,
    );
    let admin = rift_permissions::Change::SetNode {
        subject: SubjectRef::User(Uuid::parse(&context(1).uuid).unwrap()),
        node: Node::inherit("ranks", true).unwrap(),
    };
    runtime.permissions().apply(Actor::Cli, admin).unwrap();
    let admin = runtime.session(context(1)).unwrap();
    let player = runtime.session(context(2)).unwrap();
    assert!(matches!(
        player.command("staff").await.unwrap(),
        Action::Message(_)
    ));
    assert!(matches!(
        player.command("rank add").await.unwrap(),
        Action::Message(_)
    ));
    assert_eq!(admin.command("rank add").await.unwrap(), Action::Continue);
    assert_eq!(
        player.command("staff").await.unwrap(),
        Action::Server("game".into())
    );
    assert_eq!(
        admin.command("rank remove").await.unwrap(),
        Action::Continue
    );
    assert!(matches!(
        player.command("staff").await.unwrap(),
        Action::Message(_)
    ));
}

fn joined(runtime: &Extensions, id: u8, name: &str, server: &str) -> Joined {
    runtime
        .players()
        .register(id.into(), &[id; 16], name, 774, server)
}

type Joined = (PlayerHandle, tokio::sync::mpsc::Receiver<PlayerAction>);

fn drain(queue: &mut tokio::sync::mpsc::Receiver<PlayerAction>) -> Vec<PlayerAction> {
    std::iter::from_fn(|| queue.try_recv().ok()).collect()
}

#[tokio::test]
async fn players_api_lists_and_queues_actions_for_joined_players() {
    let runtime = runtime(
        r#"api_version=2, login=function(ctx)
        local players = require('rift.players')
        local list = players.list()
        assert(#list == 2 and list[1].name == 'Alice' and list[1].server == 'lobby')
        assert(list[1].uuid == '09090909090909090909090909090909')
        assert(list[1].connection_id == '9' and list[1].protocol == 774)
        assert(#players.list('game') == 1 and players.list('game')[1].name == 'Bob')
        assert(players.get('aLiCe').name == 'Alice' and players.get('Nobody') == nil)
        local names = players.names()
        assert(#names == 2 and names[1] == 'Alice' and names[2] == 'Bob')
        assert(#players.names('b') == 1 and players.names('b')[1] == 'Bob')
        assert(#players.names('Carol') == 0)
        assert(players.message('alice', 'hello'))
        assert(players.action_bar('0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a', 'bar'))
        assert(players.title('Alice', { title = 'Big', subtitle = 'small',
            fade_in_ms = 0, stay_ms = 1000, fade_out_ms = 99 }))
        assert(players.title('Alice', 'Plain'))
        assert(players.title('Alice', { subtitle = 'Only' }))
        assert(players.transfer('Alice', 'game'))
        assert(players.kick('Bob', 'Bye'))
        assert(not players.message('Nobody', 'lost'))
        assert(players.broadcast('everyone') == 2)
        assert(players.broadcast('lobby only', 'lobby') == 1)
    end"#,
    );
    let (_alice, mut alice) = joined(&runtime, 9, "Alice", "lobby");
    let (bob_handle, mut bob) = joined(&runtime, 10, "Bob", "lobby");
    bob_handle.set_server("game");
    runtime
        .session(context(1))
        .unwrap()
        .decision("login")
        .await
        .unwrap();
    let title = |title: &str, subtitle: Option<&str>, times| PlayerAction::Title {
        title: title.into(),
        subtitle: subtitle.map(Into::into),
        times,
    };
    assert_eq!(
        drain(&mut alice),
        [
            PlayerAction::Message("hello".into()),
            title("Big", Some("small"), [0, 20, 1]),
            title("Plain", None, [10, 70, 20]),
            title("", Some("Only"), [10, 70, 20]),
            PlayerAction::Transfer("game".into()),
            PlayerAction::Message("everyone".into()),
            PlayerAction::Message("lobby only".into()),
        ]
    );
    assert_eq!(
        drain(&mut bob),
        [
            PlayerAction::ActionBar("bar".into()),
            PlayerAction::Kick("Bye".into()),
            PlayerAction::Message("everyone".into()),
        ]
    );
}

#[tokio::test]
async fn players_api_takes_chat_component_tables_for_rich_text() {
    use crate::text::Text;
    let runtime = runtime(
        r#"api_version=2, login=function(ctx)
        local players = require('rift.players')
        local accept = { text = '[Accept]', color = 'green',
            click_event = { action = 'run_command', command = '/party accept' } }
        assert(players.message('Alice', { 'Join? ', accept }))
        assert(players.action_bar('Alice', { text = 'bar', bold = true }))
        assert(players.broadcast({ text = 'all', color = 'gold' }) == 1)
        assert(players.message('Alice', 42))
        assert(players.kick('Alice', { text = 'Banned', color = 'red' }))
    end"#,
    );
    let (_alice, mut alice) = joined(&runtime, 9, "Alice", "lobby");
    runtime
        .session(context(1))
        .unwrap()
        .decision("login")
        .await
        .unwrap();
    let component = |value| Text::Component(value);
    assert_eq!(
        drain(&mut alice),
        [
            PlayerAction::Message(component(serde_json::json!(["Join? ", {
                "text": "[Accept]", "color": "green",
                "click_event": {"action": "run_command", "command": "/party accept"},
            }]))),
            PlayerAction::ActionBar(component(serde_json::json!({"text": "bar", "bold": true}))),
            PlayerAction::Message(component(
                serde_json::json!({"text": "all", "color": "gold"})
            )),
            PlayerAction::Message("42".into()),
            PlayerAction::Kick(component(
                serde_json::json!({"text": "Banned", "color": "red"})
            )),
        ]
    );
}

#[tokio::test]
async fn invalid_player_actions_fail_without_queueing() {
    for operation in [
        "rift.players.message('Alice', '')",
        "rift.players.message('Alice', string.rep('x', 1025))",
        "rift.players.message('Alice', {})",
        "rift.players.message('Alice', { text = function() end })",
        "rift.players.message('Alice', { text = string.rep('x', 33000) })",
        "rift.players.kick('Alice', true)",
        "rift.players.broadcast({ [1] = 'a', x = 'b' })",
        "rift.players.message('Alice Smith', 'hello')",
        "rift.players.message('0909090909090909090909090909090G', 'hello')",
        "rift.players.get('*')",
        "rift.players.transfer('Alice', 'missing')",
        "rift.players.list('missing')",
        "rift.players.broadcast('hello', 'missing')",
        "rift.players.title('Alice', {})",
        "rift.players.title('Alice', 5)",
        "rift.players.title('Alice', { title = 'x', typo = 1 })",
        "rift.players.title('Alice', { title = 'x', stay_ms = 60001 })",
        "rift.players.title('Alice', { title = 'x', fade_in_ms = -1 })",
        "rift.players.title('Alice', { title = 'x', fade_out_ms = 1.5 })",
        "rift.players.tab_list('Alice', { header = 1 })",
        "rift.players.tab_list('Alice', { header = {} })",
        "rift.players.tab_list('Alice', { header = 'x', typo = 1 })",
        "rift.players.tab_list('Alice', 'header')",
        "rift.players.boss_bar('Alice', 'Event', { text = 'x' })",
        "rift.players.boss_bar('Alice', 'event', { progress = 1 })",
        "rift.players.boss_bar('Alice', 'event', { text = 'x', progress = 1.5 })",
        "rift.players.boss_bar('Alice', 'event', { text = 'x', progress = 'full' })",
        "rift.players.boss_bar('Alice', 'event', { text = 'x', color = 'orange' })",
        "rift.players.boss_bar('Alice', 'event', { text = 'x', style = 'notched_7' })",
        "rift.players.boss_bar('Alice', 'event', { text = 'x', typo = 1 })",
    ] {
        let runtime = runtime(&format!("api_version=2, login=function() {operation} end"));
        let (_alice, mut alice) = joined(&runtime, 9, "Alice", "lobby");
        assert!(
            runtime
                .session(context(1))
                .unwrap()
                .decision("login")
                .await
                .is_err(),
            "{operation}"
        );
        assert!(drain(&mut alice).is_empty(), "{operation}");
    }
}

#[tokio::test]
async fn player_queues_are_bounded_and_actions_are_budgeted_per_callback() {
    let runtime = runtime(
        "api_version=2, login=function() for i = 1, 257 do rift.players.message('Alice', 'x') end end",
    );
    let (alice_handle, mut alice) = joined(&runtime, 9, "Alice", "lobby");
    let error = runtime
        .session(context(1))
        .unwrap()
        .decision("login")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("budget"), "{error}");
    // A full queue reports false rather than waiting for the session.
    assert_eq!(drain(&mut alice).len(), 32);
    let players = runtime.players();
    for _ in 0..32 {
        assert!(
            players
                .send("Alice", PlayerAction::Message("x".into()))
                .unwrap()
        );
    }
    assert!(
        !players
            .send("Alice", PlayerAction::Message("x".into()))
            .unwrap()
    );
    alice.try_recv().unwrap();
    assert!(
        players
            .send("alice", PlayerAction::Message("x".into()))
            .unwrap()
    );
    drop(alice_handle);
    assert!(
        !players
            .send("Alice", PlayerAction::Message("x".into()))
            .unwrap()
    );
    assert_eq!(drain(&mut alice).len(), 32);
    assert!(alice.recv().await.is_none(), "disconnect closes the queue");
}

#[test]
fn player_filters_and_transfers_accept_service_groups() {
    let players = Players::new(
        Arc::new(Mutex::new(
            ["lobby", "games", "games-1"].map(String::from).into(),
        )),
        Arc::new(Mutex::new(BTreeMap::from([(
            "games".to_owned(),
            BTreeSet::from(["games-1".to_owned()]),
        )]))),
        Broker::new(Default::default()).unwrap(),
        Arc::new(Authority::memory(&Default::default())),
    );
    let (_alice, mut alice) = players.register(1, &[1; 16], "Alice", 774, "games-1");
    let (_bob, mut bob) = players.register(2, &[2; 16], "Bob", 774, "lobby");
    let lua = mlua::Lua::new();
    let runtime = lua.create_table().unwrap();
    players
        .install(&lua, &runtime, Instant::now() + Duration::from_secs(5))
        .unwrap();
    lua.globals()
        .set("players", runtime.get::<Table>("players").unwrap())
        .unwrap();
    lua.load(
        "assert(#players.list('games') == 1 and players.list('games')[1].name == 'Alice')
        assert(players.broadcast('games only', 'games') == 1)
        assert(players.transfer('Bob', 'games'))",
    )
    .exec()
    .unwrap();
    assert_eq!(
        drain(&mut alice),
        [PlayerAction::Message("games only".into())]
    );
    assert_eq!(drain(&mut bob), [PlayerAction::Transfer("games".into())]);
}

#[tokio::test]
async fn message_and_http_handlers_reach_players_only_with_api_v2() {
    for version in [1, 2] {
        let config = Config::from_lua(
            &format!(
                "return {{ listeners = {{ public = '127.0.0.1:0' }}, backends = {{ lobby = '127.0.0.1:25001' }},
                routes = {{ public = 'lobby' }}, authentication = {{ online_mode = true }},
                forwarding = {{ mode = 'velocity', secret_env = 'TEST_SECRET' }},
                extensions = {{ api_version = {version} }},
                messaging = {{ subscriptions = {{ {{ subject = 'chat' }} }} }},
                on_message = function(message) rift.players.broadcast(message.payload) end,
                on_http = function() return {{ body = tostring(#rift.players.list()) }} end }}"
            ),
            "players-handlers.lua",
        )
        .unwrap();
        let broker = Broker::new(Default::default()).unwrap();
        let runtime = Extensions::new(&config, broker.clone(), None).unwrap();
        let (_alice, mut alice) = joined(&runtime, 9, "Alice", "lobby");
        let request = crate::http_script::HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            query: String::new(),
            body: String::new(),
            headers: BTreeMap::new(),
            context: serde_json::Value::Null,
        };
        let response = config
            .on_http
            .as_ref()
            .unwrap()
            .execute_with_host(request, None, runtime.player_api())
            .await;
        let handler = crate::message_script::MessageHandler::new(&config, broker.clone())
            .unwrap()
            .unwrap()
            .with_players(runtime.player_api());
        let worker = tokio::spawn(handler.run());
        broker
            .publish("chat", bytes::Bytes::from_static(b"cross-server"))
            .unwrap();
        if version == 2 {
            assert_eq!(response.unwrap().unwrap().body, "1");
            assert_eq!(
                timeout(Duration::from_secs(2), alice.recv()).await.unwrap(),
                Some(PlayerAction::Message("cross-server".into()))
            );
        } else {
            assert!(response.unwrap_err().to_string().contains("on_http"));
            assert!(
                timeout(Duration::from_millis(200), alice.recv())
                    .await
                    .is_err()
            );
        }
        worker.abort();
    }
}

#[tokio::test]
async fn players_api_sets_and_restores_tab_lists() {
    let runtime = runtime(
        r#"api_version=2, login=function(ctx)
        local players = require('rift.players')
        assert(players.tab_list('Alice', { header = 'Top', footer = { text = 'Bottom', color = 'gray' } }))
        assert(players.tab_list('Alice', { footer = 'Only footer' }))
        assert(players.tab_list('Alice'))
        assert(not players.tab_list('Nobody', { header = 'x' }))
    end"#,
    );
    let (_alice, mut alice) = joined(&runtime, 9, "Alice", "lobby");
    runtime
        .session(context(1))
        .unwrap()
        .decision("login")
        .await
        .unwrap();
    let custom = |header: Option<serde_json::Value>, footer| {
        PlayerAction::TabList(Some(crate::tab_list::Override { header, footer }))
    };
    assert_eq!(
        drain(&mut alice),
        [
            custom(
                Some(serde_json::json!("Top")),
                Some(serde_json::json!({ "text": "Bottom", "color": "gray" }))
            ),
            custom(None, Some(serde_json::json!("Only footer"))),
            PlayerAction::TabList(None),
        ]
    );
}

#[tokio::test]
async fn players_api_shows_changes_and_removes_boss_bars() {
    let runtime = runtime(
        r#"api_version=2, login=function(ctx)
        local players = require('rift.players')
        assert(players.boss_bar('Alice', 'event', { text = 'Event' }))
        assert(players.boss_bar('Alice', 'event', {
            text = { text = 'Event', color = 'gold' }, progress = 0.25, color = 'yellow',
            style = 'notched_10', darken_screen = true, create_fog = true,
        }))
        assert(players.boss_bar('Alice', 'event'))
        assert(not players.boss_bar('Nobody', 'event', { text = 'x' }))
        assert(not players.boss_bar('Legacy', 'event', { text = 'x' }))
        -- Removing and replacing bars keeps a player within the limit.
        for i = 1, 8 do assert(players.boss_bar('Alice', 'bar' .. i, { text = 'x' })) end
        assert(players.boss_bar('Alice', 'bar1', { text = 'y' }))
        assert(players.boss_bar('Alice', 'bar1'))
        assert(players.boss_bar('Alice', 'bar9', { text = 'x' }))
        players.boss_bar('Alice', 'bar10', { text = 'x' })
    end"#,
    );
    let (_alice, mut alice) = joined(&runtime, 9, "Alice", "lobby");
    // 1.8.9 has no boss bars.
    let (_legacy, mut legacy) = runtime
        .players()
        .register(10, &[10; 16], "Legacy", 47, "lobby");
    let error = runtime
        .session(context(1))
        .unwrap()
        .decision("login")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("at most 8 boss bars"), "{error}");
    let actions = drain(&mut alice);
    assert_eq!(actions.len(), 14);
    assert_eq!(
        actions[..3],
        [
            PlayerAction::BossBar {
                name: "event".into(),
                bar: Some(crate::protocol::BossBar {
                    text: serde_json::json!("Event"),
                    progress: 1.0,
                    color: 0,
                    style: 0,
                    flags: 0,
                }),
            },
            PlayerAction::BossBar {
                name: "event".into(),
                bar: Some(crate::protocol::BossBar {
                    text: serde_json::json!({ "text": "Event", "color": "gold" }),
                    progress: 0.25,
                    color: 4,
                    style: 2,
                    flags: 5,
                }),
            },
            PlayerAction::BossBar {
                name: "event".into(),
                bar: None,
            },
        ]
    );
    assert!(drain(&mut legacy).is_empty());
}

#[test]
fn upgrade_copies_group_memberships_and_legacy_grants_without_touching_the_store() {
    let state = StateFile::new();
    let extension = format!(
        "api_version=2, storage={{path={}}}, groups={{staff={{permissions={{['network.staff']=true}}}}}},
        permissions={{['*']={{['chat.send']=true}}, ['{}']={{['network.*']=true, ['network.ban']=false}}}}",
        state.lua_path(),
        "01".repeat(16),
    );
    let path = std::env::temp_dir().join("rift-upgrade.lua");
    // Startup parses before validating, so the removed field is still readable.
    let config = Config::from_lua_at_unrestored(&source(&extension), &path).unwrap();
    assert!(
        config.validate().is_err(),
        "validation refuses the removed field"
    );
    let storage = config.extensions.as_ref().unwrap().storage.as_ref();
    {
        let store = storage::Store::open(storage).unwrap();
        store
            .mutate_then(
                Instant::now() + Duration::from_secs(30),
                |entries| {
                    entries.insert((MEMBERSHIPS.into(), "01".repeat(16)), b"staff,vip".to_vec());
                    entries.insert((MEMBERSHIPS.into(), "corrupt".into()), b"staff".to_vec());
                    Ok(())
                },
                |_| {},
            )
            .unwrap();
    }
    let authority = Authority::memory(&config.permissions);
    let applied = authority
        .migrate_once(|| upgrade_nodes(&config))
        .unwrap()
        .unwrap();
    assert_eq!(applied.ops, 5);
    assert!(authority.migrate_once(|| unreachable!()).unwrap().is_none());
    let alice = Uuid::parse(&"01".repeat(16)).unwrap();
    let parents: Vec<String> = authority
        .user(alice)
        .unwrap()
        .nodes
        .iter()
        .filter(|node| node.kind() == rift_permissions::NodeKind::Inherit)
        .map(|node| node.payload.key())
        .collect();
    assert_eq!(parents, ["staff", "vip"]);
    let global = ContextSet::new();
    assert_eq!(
        authority.check(alice, "network.kick", &global),
        Tristate::True
    );
    assert_eq!(
        authority.check(alice, "network.ban", &global),
        Tristate::False
    );
    let other = Uuid::from_u128(7);
    assert_eq!(authority.check(other, "chat.send", &global), Tristate::True);
    // Nothing is deleted from the extension store, so a downgrade still works.
    let store = storage::Store::open(storage).unwrap();
    assert_eq!(store.namespace(MEMBERSHIPS).len(), 2);
}
