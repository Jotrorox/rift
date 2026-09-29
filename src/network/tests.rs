use super::*;
use rift::config::Config;
use std::sync::Arc;

fn config() -> Config {
    Config::from_lua(
        "return {
            listeners = { public = '127.0.0.1:0' },
            backends = {
                lobby = '127.0.0.1:25001',
                backup = '127.0.0.1:25002',
                survival = '127.0.0.1:25003',
                restricted = '127.0.0.1:25004',
            },
            routes = { public = 'survival' },
            fallbacks = {
                survival = { 'restricted', 'backup', 'lobby' },
                restricted = { 'lobby' },
                backup = { 'survival' },
            },
            network = {
                initial = { 'restricted', 'lobby', 'backup' },
                hubs = { 'lobby', 'backup' },
                access = {
                    restricted = { allow = { 'Alice' }, deny = { 'Bob' } },
                },
            },
        }",
        "network-policy.lua",
    )
    .unwrap()
}

#[test]
fn explicit_initial_order_filters_access_without_using_backend_fallbacks() {
    let snapshot = Snapshot::new(config(), None).unwrap();
    let metrics = Metrics::default();
    let network = Network {
        snapshot: &snapshot,
        addresses: &[],
        metrics: &metrics,
    };
    assert_eq!(
        network
            .initial_candidates("survival", true, "ALICE")
            .unwrap(),
        ["restricted", "lobby", "backup"]
    );
    assert_eq!(
        network.initial_candidates("survival", true, "Bob").unwrap(),
        ["lobby", "backup"]
    );
    // An explicit initial list replaces the direct primary, including its ACL.
    assert_eq!(
        network
            .initial_candidates("restricted", true, "Eve")
            .unwrap(),
        ["lobby", "backup"]
    );
}

#[test]
fn selected_route_permissions_are_terminal_and_cannot_be_bypassed_by_fallbacks() {
    let snapshot = Snapshot::new(config(), None).unwrap();
    let metrics = Metrics::default();
    let network = Network {
        snapshot: &snapshot,
        addresses: &[],
        metrics: &metrics,
    };
    let error = network
        .initial_candidates("restricted", false, "Bob")
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        network
            .initial_candidates("restricted", false, "Alice")
            .unwrap(),
        ["restricted", "lobby"]
    );
    assert_eq!(
        network
            .initial_candidates("missing", false, "Alice")
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn ordinary_routes_keep_their_order_and_filter_restricted_fallbacks() {
    let snapshot = Snapshot::new(config(), None).unwrap();
    let metrics = Metrics::default();
    let network = Network {
        snapshot: &snapshot,
        addresses: &[],
        metrics: &metrics,
    };
    assert_eq!(
        network
            .initial_candidates("survival", false, "Eve")
            .unwrap(),
        ["survival", "backup", "lobby"]
    );
    assert_eq!(
        network
            .initial_candidates("survival", false, "Alice")
            .unwrap(),
        ["survival", "restricted", "backup", "lobby"]
    );
}

#[test]
fn recovery_prefers_hubs_then_flat_fallbacks_excluding_the_current_server() {
    let mut config = config();
    config.network.hubs = vec!["lobby".into(), "survival".into(), "backup".into()];
    let snapshot = Snapshot::new(config, None).unwrap();
    let metrics = Metrics::default();
    let network = Network {
        snapshot: &snapshot,
        addresses: &[],
        metrics: &metrics,
    };
    assert_eq!(
        network.recovery_candidates("survival"),
        ["lobby", "backup", "restricted"]
    );
    assert_eq!(network.recovery_candidates("lobby"), ["survival", "backup"]);
    assert_eq!(network.recovery_candidates("backup"), ["lobby", "survival"]);
}

#[test]
fn no_configured_recovery_destinations_means_no_implicit_cross_server_access() {
    let mut config = config();
    config.network.hubs.clear();
    config.fallbacks.clear();
    let snapshot = Snapshot::new(config, None).unwrap();
    let metrics = Metrics::default();
    let network = Network {
        snapshot: &snapshot,
        addresses: &[],
        metrics: &metrics,
    };
    assert!(network.recovery_candidates("survival").is_empty());
}

#[test]
fn reloads_share_registry_and_old_session_cleanup_releases_new_snapshot_reservations() {
    let original = Snapshot::new(config(), None).unwrap();
    let guard = original
        .players
        .register([1; 16], "Alice", "lobby")
        .unwrap();
    let mut changed = config();
    changed.network.access.get_mut("restricted").unwrap().allow = Some(Vec::new());
    let updated = Snapshot::new(changed, Some(&original)).unwrap();
    assert!(Arc::ptr_eq(&original.players, &updated.players));
    assert_eq!(updated.players.get(&[1; 16]).unwrap().server, "lobby");
    assert_eq!(
        updated
            .players
            .register([2; 16], "aLiCe", "backup")
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    guard.set_server("survival");
    assert_eq!(updated.players.get(&[1; 16]).unwrap().server, "survival");
    drop(original);
    drop(guard);
    assert!(updated.players.is_empty());
    let _new_guard = updated
        .players
        .register([2; 16], "Alice", "backup")
        .unwrap();
    assert_eq!(updated.players.len(), 1);
}

// These peers exercise the real TCP attachment/command path. Authentication
// itself is covered by auth/session tests; this fixture supplies its verified
// profile directly without contacting Mojang or needing account credentials.
#[tokio::test]
async fn queue_command_waits_for_capacity_and_transfer_policy_runs_before_target_tcp() {
    use rift::{
        auth::AuthenticatedProfile,
        extensions::{Context, Extensions},
        protocol::{Codec, Handshake, NextState, Packet, ProtocolVersion, Reader, write_string},
        session::SessionEvent,
    };
    use tokio::{
        io::{AsyncRead, DuplexStream},
        net::TcpListener,
        time::{Duration, timeout},
    };
    async fn read(stream: &mut (impl AsyncRead + Unpin)) -> Packet {
        Reader::default()
            .read(stream, Codec::default())
            .await
            .unwrap()
            .unwrap()
    }
    timeout(Duration::from_secs(5), async {
        let lobby = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let game = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut cfg = config();
        cfg.backends.insert("lobby".into(), rift::routing::Backend::parse(&lobby.local_addr().unwrap().to_string()).unwrap());
        cfg.backends.insert("survival".into(), rift::routing::Backend::parse(&game.local_addr().unwrap().to_string()).unwrap());
        let snapshot = Snapshot::new(cfg.clone(), None).unwrap();
        let extension_config = Config::from_lua(r#"return {
            listeners={public='127.0.0.1:0'}, backends={lobby='127.0.0.1:1',survival='127.0.0.1:2'}, routes={public='lobby'},
            authentication={online_mode=true},forwarding={mode='velocity',secret_env='TEST_SECRET'},
            extensions={api_version=1, queues={survival=1}, permissions={['*']={['queue.use']=true}},
                commands={queue={permission='queue.use',run=function() return {queue='survival'} end}},
                before_transfer=function(ctx)
                    if ctx.reason ~= 'queue' then return {deny='Use the queue'} end
                end,
                after_transfer=function(ctx) rift.publish('observed', tostring(ctx.success)) end
            }
        }"#, "network-extensions.lua").unwrap();
        let extensions = Extensions::new(&extension_config, snapshot.messaging.clone(), None);
        let mut observations = snapshot.messaging.subscribe("observed", None).unwrap();
        let profile = AuthenticatedProfile { uuid: [7;16], name: "Player".into(), properties: vec![] };
        let mut occupant = extensions.session(Context::authenticated(2, &profile)).unwrap();
        occupant.reserve("survival").unwrap(); occupant.ready("survival").await;
        let metrics = Metrics::default();
        let network = Network { snapshot: &snapshot, addresses: &[], metrics: &metrics };
        let (mut client, input) = tokio::io::duplex(65536);
        let handshake = Handshake { protocol:774, address:"play.test".into(),port:25565,next_state:NextState::Login };
        Codec::default().write(&mut client, &handshake.packet()).await.unwrap();
        let mut session: Session<DuplexStream, TcpStream> = Session::accept(input).await.unwrap();
        session.enable_network().unwrap();
        session.set_extension(extensions.session(Context::authenticated(1, &profile)).unwrap());
        let mut login = Vec::new(); write_string("Player", &mut login); login.extend([7;16]);
        Codec::default().write(&mut client, &Packet::new(0, login)).await.unwrap();
        session.read_login_start().await.unwrap();
        let mut event = Connection::new("public", "127.0.0.1:1234".parse().unwrap());
        network.connect_initial(&mut session, &["lobby".into()], Instant::now()+Duration::from_secs(1), &mut event).await.unwrap();
        let (mut backend, _) = lobby.accept().await.unwrap();
        read(&mut backend).await; read(&mut backend).await;
        let mut success = vec![7;16]; write_string("Player", &mut success); success.push(0);
        Codec::default().write(&mut backend, &Packet::new(2, success)).await.unwrap();
        session.forward().await.unwrap(); read(&mut client).await;
        Codec::default().write(&mut client, &Packet::empty(3)).await.unwrap();
        session.forward().await.unwrap(); read(&mut backend).await;
        Codec::default().write(&mut backend, &Packet::empty(3)).await.unwrap();
        session.forward().await.unwrap(); read(&mut client).await;
        Codec::default().write(&mut client, &Packet::empty(3)).await.unwrap();
        session.forward().await.unwrap(); read(&mut backend).await;
        let mut join = vec![0,0,0,1,0,1]; write_string("minecraft:overworld", &mut join);
        join.extend([20,8,8,0,1,0,0]); write_string("minecraft:overworld", &mut join);
        join.extend([0;8]); join.extend([0,255,0,0,0,0,63,0]);
        Codec::default().write(&mut backend, &Packet::new(0x30,join)).await.unwrap();
        session.forward().await.unwrap(); read(&mut client).await;
        session.extension.as_mut().unwrap().ready("lobby").await;
        assert!(session.can_switch());

        // A manually typed built-in route cannot bypass the extension policy.
        assert!(network.command(&mut session,"lobby","server survival",&mut event).await.unwrap().is_none());
        assert_eq!(read(&mut client).await.id, 0x77);
        assert!(timeout(Duration::from_millis(10), game.accept()).await.is_err());
        let mut command=Vec::new(); write_string("queue", &mut command);
        Codec::default().write(&mut client, &Packet::new(6,command)).await.unwrap();
        let SessionEvent::ProxyCommand(command) = session.forward().await.unwrap() else { panic!("queue was forwarded") };
        assert!(network.command(&mut session,"lobby",&command,&mut event).await.unwrap().is_none());
        assert_eq!(read(&mut client).await.id, 0x77);
        assert!(session.extension.as_ref().unwrap().queued_target().unwrap().is_none());
        drop(occupant);
        let target=session.extension.as_ref().unwrap().queued_target().unwrap().unwrap();
        assert_eq!(target,"survival");
        let replacement=async {
            let (mut target, _) = game.accept().await.unwrap();
            read(&mut target).await; read(&mut target).await;
            let rejection=rift::protocol::disconnect(Some(ProtocolVersion::new(774).unwrap()),State::Login,"Target whitelist").unwrap();
            Codec::default().write(&mut target,&rejection).await.unwrap();
        };
        let targets = [target];
        let (result, ())=tokio::join!(network.switch_for(&mut session,"lobby",&targets,&mut event,"queue"),replacement);
        assert_eq!(result.unwrap_err().kind(),io::ErrorKind::PermissionDenied);
        assert!(session.can_switch());
        assert_eq!(observations.recv().await.unwrap().payload.as_ref(),b"false");
        // The queued preflight failure released the target slot; the original
        // attachment can still relay packets, with no second client connection.
        session.extension.as_ref().unwrap().leave_queue();
        let next=extensions.session(Context::authenticated(3,&profile)).unwrap();
        next.reserve("survival").unwrap();
        Codec::default().write(&mut backend,&Packet::new(0x42,vec![1,2,3])).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(read(&mut client).await,Packet::new(0x42,vec![1,2,3]));
    }).await.unwrap();
}
