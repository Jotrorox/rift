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
        "api_version=3",
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
        "rift.permissions.set('01010101010101010101010101010101','route.game',true)",
        "rift.http.request('account',{})",
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
        "rift.permissions.set('01010101010101010101010101010101','route.game',true)",
        "rift.http.request('account',{})",
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
async fn live_permissions_revoke_defaults_and_reset_within_the_same_session() {
    let runtime = runtime(
        r#"api_version=2,
        permissions={['*']={['route.game']=true,['permissions.manage']=true}},
        initial_server=function(ctx)
            assert(ctx.has_permission('route.game')==rift.permissions.has(ctx.uuid,'route.game'))
            return {server=ctx.has_permission('route.game') and 'game' or 'lobby'}
        end,
        commands={
            game={permission='route.game',run=function() return {server='game'} end},
            access={permission='permissions.manage',run=function(ctx)
                if ctx.args=='revoke' then rift.permissions.set(ctx.uuid,'route.game',false)
                elseif ctx.args=='grant' then rift.permissions.set(ctx.uuid,'route.game',true)
                elseif ctx.args=='reset' then rift.permissions.set(ctx.uuid,'route.game',nil)
                elseif ctx.args=='revoke-all' then rift.permissions.set('*','route.game',false)
                elseif ctx.args=='reset-all' then rift.permissions.set('*','route.game',nil)
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
        ("revoke-all", false),
        ("grant", true),
        ("reset", false),
        ("reset-all", true),
    ] {
        assert_eq!(
            player
                .command(&format!("access {operation}"))
                .await
                .unwrap(),
            Action::Message(granted.to_string())
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
}

#[tokio::test]
async fn live_revocation_blocks_all_transfer_reasons_before_capacity_reservation() {
    let runtime = runtime(
        r#"api_version=2, queues={game=1},
        permissions={['*']={['route.game']=true,['permissions.manage']=true}},
        before_transfer=function(ctx)
            if ctx.target=='game' and not ctx.has_permission('route.game') then
                return {deny='Access revoked'}
            end
        end,
        commands={access={permission='permissions.manage',run=function(ctx)
            rift.permissions.set(ctx.uuid,'route.game',ctx.args=='grant')
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
    for arguments in [
        "ctx.uuid,'route.game',1",
        "ctx.uuid,'route.game','false'",
        "'not-a-uuid','route.game',false",
        "'ABCDEF0123456789ABCDEF0123456789','route.game',false",
        "ctx.uuid,'bad node',false",
    ] {
        let runtime = runtime(&format!(
            r#"api_version=2,
            permissions={{['*']={{['route.game']=true}}}},
            login=function(ctx) rift.permissions.set({arguments}) end,
            initial_server=function(ctx)
                assert(ctx.has_permission('route.game'))
                return {{server='game'}}
            end"#
        ));
        let player = runtime.session(context(1)).unwrap();
        assert!(player.decision("login").await.is_err(), "{arguments}");
        assert_eq!(
            player.decision("initial_server").await.unwrap(),
            Action::Server("game".into())
        );
    }
}

#[tokio::test]
async fn permission_reload_is_transactional_and_keeps_callback_generations_pinned() {
    let old = runtime(
        r#"api_version=2,
        permissions={['*']={inspect=true,['route.game']=true}},
        commands={check={permission='inspect',run=function(ctx)
            return {message='old:'..tostring(ctx.has_permission('route.game'))}
        end}}"#,
    );
    let existing = old.session(context(1)).unwrap();
    let cfg = config(
        r#"api_version=2,
        permissions={['*']={inspect=true}},
        commands={check={permission='inspect',run=function(ctx)
            return {message='new:'..tostring(ctx.has_permission('route.game'))}
        end}}"#,
    );
    let rejected = Extensions::new(&cfg, old.broker.clone(), Some(&old)).unwrap();
    assert_eq!(
        existing.command("check").await.unwrap(),
        Action::Message("old:true".into())
    );
    drop(rejected);
    assert_eq!(
        existing.command("check").await.unwrap(),
        Action::Message("old:true".into())
    );
    let next = Extensions::new(&cfg, old.broker.clone(), Some(&old)).unwrap();
    next.activate();
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
async fn v1_sessions_keep_static_grants_after_reload() {
    let old = runtime(
        r#"api_version=1, permissions={['*']={['route.game']=true}},
        commands={game={permission='route.game',run=function(ctx)
            assert(ctx.api_version==1 and ctx.has_permission('route.game'))
            return {server='game'}
        end}}"#,
    );
    let existing = old.session(context(1)).unwrap();
    let next = Extensions::new(
        &config(
            r#"api_version=1,
        commands={game={permission='route.game',run=function() return {server='game'} end}}"#,
        ),
        old.broker.clone(),
        Some(&old),
    )
    .unwrap();
    next.activate();
    assert!(matches!(
        next.session(context(2))
            .unwrap()
            .command("game")
            .await
            .unwrap(),
        Action::Message(_)
    ));
    assert_eq!(
        existing.command("game").await.unwrap(),
        Action::Server("game".into())
    );
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
async fn scheduled_jobs_share_the_session_worker_limit_without_backlog() {
    let runtime = runtime(
        r#"api_version=2,jobs={tick={every_ms=100,run=function()
        rift.publish('ticks','tick')
    end}}"#,
    );
    let mut events = runtime.broker.subscribe("ticks", None).unwrap();
    let permits = runtime
        .slots
        .clone()
        .acquire_many_owned(script::MAX_CONCURRENT as u32)
        .await
        .unwrap();
    let scheduler = runtime.scheduler();
    assert!(
        timeout(Duration::from_millis(220), events.recv())
            .await
            .is_err()
    );
    drop(permits);
    timeout(Duration::from_secs(2), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        timeout(Duration::from_millis(40), events.recv())
            .await
            .is_err()
    );
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
fn permission_group_and_wildcard_schema_is_validated() {
    for invalid in [
        "api_version=1, groups={}",
        "api_version=1, permissions={['*']={['network.*']=true}}",
        "api_version=1, permissions={['*']={['network.staff']=false}}",
        "api_version=1, permissions={['*']={['network.staff']='false'}}",
        "api_version=2, permissions={['*']={['network.*.ban']=true}}",
        "api_version=2, permissions={['*']={['*.ban']=true}}",
        "api_version=2, permissions={['*']={['.*']=true}}",
        "api_version=2, permissions={['*']={['network.staff']='yes'}}",
        "api_version=2, groups={staff=true}",
        "api_version=2, groups={Staff={}}",
        "api_version=2, groups={staff={typo=true}}",
        "api_version=2, groups={staff={inherits='default'}}",
        "api_version=2, groups={staff={inherits={x='default'}}}",
        "api_version=2, groups={default={}, staff={inherits={'default', [3]='default', y=1}}}",
        "api_version=2, groups={staff={inherits={'missing'}}}",
        "api_version=2, groups={staff={inherits={'staff'}}}",
        "api_version=2, groups={a={inherits={'b'}}, b={inherits={'c'}}, c={inherits={'a'}}}",
        "api_version=2, groups={staff={permissions={['bad node']=true}}}",
    ] {
        assert!(
            Config::from_lua(&source(invalid), "invalid-groups.lua").is_err(),
            "{invalid}"
        );
    }
    let many = (0..65)
        .map(|i| format!("g{i}={{}}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(
        Config::from_lua(
            &source(&format!("api_version=2, groups={{{many}}}")),
            "many.lua"
        )
        .is_err()
    );
    config(
        r#"api_version=2,
        permissions={['*']={['*']=false, ['network.*']=true}},
        groups={
            default={permissions={['chat.*']=true}},
            staff={inherits={'default'}, permissions={['network.staff']=true, ['chat.color']=false}},
            admin={inherits={'staff','default'}},
        }"#,
    );
}

const GROUP_PROBES: &str = r#"permissions={['*']={probe=true}},
    commands={
        add={permission='probe',run=function(ctx) return {message=tostring(rift.permissions.add_group(ctx.uuid,ctx.args))} end},
        remove={permission='probe',run=function(ctx) return {message=tostring(rift.permissions.remove_group(ctx.uuid,ctx.args))} end},
        groups={permission='probe',run=function(ctx) return {message='['..table.concat(rift.permissions.groups(ctx.uuid),',')..']'} end},
        has={permission='probe',run=function(ctx) return {message=tostring(ctx.has_permission(ctx.args))} end},
        revoke={permission='probe',run=function(ctx) rift.permissions.set(ctx.uuid,ctx.args,false) end},
        everyone={permission='probe',run=function(ctx) rift.permissions.add_group('*',ctx.args) end},
        reserved={permission='probe',run=function(ctx) rift.store.get('rift.groups',ctx.uuid) end},
    }"#;

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
        Action::Message(message) => message,
        Action::Continue => String::new(),
        action => panic!("unexpected {action:?}"),
    })
}

#[test]
fn group_memberships_persist_across_restart_and_survive_group_removal() {
    let state = StateFile::new();
    let storage = format!(
        "api_version=2, storage={{path={}}}, {GROUP_PROBES}",
        state.lua_path()
    );
    let settings = format!(
        r#"{storage}, groups={{
            default={{permissions={{['network.queue']=true}}}},
            staff={{inherits={{'default'}}, permissions={{['network.*']=true}}}},
        }}"#
    );
    {
        let runtime = runtime(&settings);
        assert_eq!(probe(&runtime, "has", "network.queue").unwrap(), "true");
        assert_eq!(probe(&runtime, "has", "network.staff").unwrap(), "false");
        assert_eq!(probe(&runtime, "add", "staff").unwrap(), "true");
        assert_eq!(probe(&runtime, "add", "staff").unwrap(), "false");
        assert_eq!(probe(&runtime, "has", "network.staff").unwrap(), "true");
        assert_eq!(probe(&runtime, "groups", "").unwrap(), "[staff]");
        for (command, args) in [
            ("add", "missing"),
            ("add", "default"),
            ("add", "Staff"),
            ("everyone", "staff"),
            ("reserved", ""),
        ] {
            assert!(probe(&runtime, command, args).is_err(), "{command} {args}");
        }
        assert_eq!(probe(&runtime, "groups", "").unwrap(), "[staff]");
        // Live wildcard overrides still revoke group grants.
        probe(&runtime, "revoke", "network.*").unwrap();
        assert_eq!(probe(&runtime, "has", "network.staff").unwrap(), "false");
    }
    // Memberships are durable; overrides are process-local.
    let restarted = runtime(&settings);
    assert_eq!(probe(&restarted, "has", "network.staff").unwrap(), "true");
    assert_eq!(probe(&restarted, "groups", "").unwrap(), "[staff]");
    // A reload that drops the group leaves the membership dormant but removable.
    let reloaded = Extensions::new(
        &config(&format!("{storage}, groups={{default={{}}}}")),
        restarted.broker.clone(),
        Some(&restarted),
    )
    .unwrap();
    reloaded.activate();
    assert_eq!(probe(&reloaded, "has", "network.staff").unwrap(), "false");
    assert_eq!(probe(&reloaded, "groups", "").unwrap(), "[staff]");
    assert_eq!(probe(&reloaded, "remove", "staff").unwrap(), "true");
    assert_eq!(probe(&reloaded, "remove", "staff").unwrap(), "false");
    assert_eq!(probe(&reloaded, "groups", "").unwrap(), "[]");
    drop((restarted, reloaded));
    let restarted = runtime(&settings);
    assert_eq!(probe(&restarted, "has", "network.staff").unwrap(), "false");
}

#[tokio::test]
async fn commands_honor_group_and_wildcard_grants() {
    let runtime = runtime(
        r#"api_version=2,
        permissions={['01010101010101010101010101010101']={['ranks.manage']=true}},
        groups={staff={permissions={['network.*']=true}}},
        commands={
            staff={permission='network.staff',run=function() return {server='game'} end},
            rank={permission='ranks.manage',run=function(ctx)
                local uuid=string.rep('02',16)
                if ctx.args=='add' then rift.permissions.add_group(uuid,'staff')
                else rift.permissions.remove_group(uuid,'staff') end
            end},
        }"#,
    );
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
