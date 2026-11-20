//! Presence across the peers of a simulated cluster
//! ([`super::memory::MemoryCluster`]).
use super::{
    Admission, PlayerRegistration, Viewer,
    attributes::{ApplyOutcome, AttributeUpdate, UpdateBody},
    claims::{ClaimChange, Claims},
    memory::{self, MemoryCluster},
};
use crate::{
    dialog::{Dialog, Owner},
    extensions::{
        Context, PlayerAction, PlayerHandle, Players,
        routing::{RouteFuture, SessionHooks, WireBody, WireRequest, WireResult},
    },
    messaging::Broker,
    permissions::Authority,
    text::{
        Text, Unrendered,
        catalog::{self, Notice},
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Barrier, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;

/// Claim changes as `players` sees them from now on.
fn subscribe(players: &Players) -> tokio::sync::broadcast::Receiver<ClaimChange> {
    Claims::subscribe(&*players.roster().claims())
}

/// Clients new enough for dialogs and boss bars.
const PROTOCOL: i32 = 774;

fn directory() -> Players {
    Players::new(
        Arc::new(Mutex::new(
            ["lobby", "survival", "games", "games-1"]
                .map(String::from)
                .into(),
        )),
        Arc::new(Mutex::new(BTreeMap::from([(
            "games".to_owned(),
            BTreeSet::from(["games-1".to_owned()]),
        )]))),
        Broker::new(Default::default()).unwrap(),
        Arc::new(Authority::memory(&Default::default())),
    )
}

/// Peer `id` of `cluster`, started as `incarnation`.
fn peer(cluster: &MemoryCluster, id: u16, incarnation: u64) -> Players {
    cluster.join(id, incarnation);
    let players = directory();
    cluster.attach(id, &players);
    players
}

struct Session {
    connection: u64,
    _registration: PlayerRegistration,
    _handle: PlayerHandle,
    actions: mpsc::Receiver<PlayerAction>,
}

/// Log a player in through `players` as a session does: name claim, then
/// UUID claim, then the join.
async fn login(
    players: &Players,
    counter: u64,
    uuid: u8,
    name: &str,
    server: &str,
) -> io::Result<Session> {
    login_with(players, counter, uuid, name, server, PROTOCOL).await
}

/// [`login`] with a client on `protocol`.
async fn login_with(
    players: &Players,
    counter: u64,
    uuid: u8,
    name: &str,
    server: &str,
    protocol: i32,
) -> io::Result<Session> {
    let roster = players.roster().clone();
    let connection = roster.identity().connection_id(counter);
    let reservation = roster.claim_name(name, connection).await?;
    let registration = reservation
        .claim_player([uuid; 16], server, None, false)
        .await?;
    let (handle, actions) = players.register(connection, &[uuid; 16], name, protocol, server);
    Ok(Session {
        connection,
        _registration: registration,
        _handle: handle,
        actions,
    })
}

/// The next action a session received, waiting for one sent without
/// waiting for the answer.
async fn next(session: &mut Session) -> PlayerAction {
    tokio::time::timeout(Duration::from_secs(5), session.actions.recv())
        .await
        .expect("an action arrives")
        .expect("the session is open")
}

#[derive(Default)]
struct Hooks {
    clicks: Mutex<Vec<(Owner, Context)>>,
    menus: Mutex<Vec<(Owner, Context)>>,
}

impl SessionHooks for Hooks {
    fn transfer(
        &self,
        connection: u64,
        backend: String,
        admin: bool,
    ) -> RouteFuture<Result<Value, String>> {
        Box::pin(std::future::ready(Ok(
            json!({"connection_id": connection, "backend": backend, "admin": admin}),
        )))
    }

    fn dialog(&self, context: Context, owner: Owner) -> RouteFuture<io::Result<()>> {
        self.clicks.lock().unwrap().push((owner, context));
        Box::pin(std::future::ready(Ok(())))
    }

    fn menu(&self, context: Context, owner: Owner) -> RouteFuture<io::Result<()>> {
        self.menus.lock().unwrap().push((owner, context));
        Box::pin(std::future::ready(Ok(())))
    }
}

#[test]
fn simultaneous_logins_on_two_peers_let_exactly_one_win() {
    let cluster = MemoryCluster::new();
    cluster.join(1, 1);
    cluster.join(2, 1);
    let peers = [memory::registry(&cluster, 1), memory::registry(&cluster, 2)];
    // One name in two spellings (offline mode, two UUIDs), then one UUID
    // under two names.
    for (names, uuids) in [(["Alice", "aLiCe"], [1u8, 2]), (["Bob", "Robert"], [3, 3])] {
        let barrier = Arc::new(Barrier::new(2));
        let attempts: Vec<_> = peers
            .iter()
            .zip(names)
            .zip(uuids)
            .map(|((registry, name), uuid)| {
                let (registry, barrier) = (registry.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    registry
                        .reserve_name(name)
                        .and_then(|reservation| reservation.register([uuid; 16], "lobby"))
                })
            })
            .collect();
        let results: Vec<_> = attempts
            .into_iter()
            .map(|attempt| attempt.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let error = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(cluster.live().len(), 1);
        drop(results);
        assert!(cluster.live().is_empty());
    }
}

#[tokio::test]
async fn the_network_cap_holds_on_every_peer_unless_bypassed() {
    let cluster = MemoryCluster::new();
    cluster.join(1, 1);
    cluster.join(2, 1);
    let (a, b) = (memory::registry(&cluster, 1), memory::registry(&cluster, 2));
    for registry in [&a, &b] {
        registry.set_max_players(Some(2));
    }
    let _first = a.register([1; 16], "First", "lobby").unwrap();
    // A pending login is not a player yet.
    let pending = b.reserve_name("Pending").unwrap();
    let _second = b.register([2; 16], "Second", "lobby").unwrap();
    let full = a.register([3; 16], "Third", "lobby").unwrap_err();
    assert_eq!(full.kind(), io::ErrorKind::QuotaExceeded);
    assert_eq!(
        crate::text::reason(&full),
        catalog::notice(Notice::ClusterMaxPlayers)
    );
    assert_eq!(
        pending
            .claim_player([4; 16], "lobby", None, false)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::QuotaExceeded
    );
    // The failed upgrade released the pending name.
    assert!(b.reserve_name("Pending").is_ok());
    let staff = a
        .claim_player([5; 16], "Staff", "lobby", None, 7, true)
        .await
        .unwrap();
    assert_eq!(staff.connection(), 7);
    assert_eq!(
        cluster
            .live()
            .iter()
            .filter(|record| record.uuid.is_some())
            .count(),
        3
    );
}

#[test]
fn expiring_a_peer_frees_its_names_for_good() {
    let cluster = MemoryCluster::new();
    cluster.join(1, 1);
    cluster.join(2, 1);
    let (a, b) = (memory::registry(&cluster, 1), memory::registry(&cluster, 2));
    let on_b = b.register([1; 16], "Alice", "lobby").unwrap();
    assert_eq!(
        a.reserve_name("alice").unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    // B is cut off and the leader expires it.
    cluster.partition(2);
    assert_eq!(
        b.register([2; 16], "Bob", "lobby").unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    cluster.expire(2);
    let on_a = a
        .reserve_name("Alice")
        .unwrap()
        .register([1; 16], "lobby")
        .unwrap();
    // B's late release cannot free the claim A holds now.
    drop(on_b);
    cluster.heal(2);
    let live = cluster.live();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].holder.peer, 1);
    drop(on_a);
    assert!(cluster.live().is_empty());
}

#[tokio::test]
async fn a_restarted_peer_frees_names_at_once_and_refuses_the_old_incarnation() {
    let cluster = MemoryCluster::new();
    let a = peer(&cluster, 1, 1);
    let b = peer(&cluster, 2, 1);
    let mut changes = subscribe(&a);
    let old = login(&b, 1, 1, "Alice", "lobby").await.unwrap();
    cluster.pump();
    let remote = a.roster().remote_by_name("Alice").unwrap();
    assert_eq!((remote.player.peer, remote.incarnation), (2, 1));
    assert_eq!(remote.player.connection, old.connection);
    // B crashes and starts again: joining as incarnation 2 releases what
    // incarnation 1 held, and the new process holds nothing.
    let restarted = directory();
    cluster.join(2, 2);
    cluster.attach(2, &restarted);
    while let Ok(change) = changes.try_recv() {
        a.apply_claim_change(&change);
    }
    assert!(a.roster().remote_by_name("Alice").is_none());
    let mut again = login(&restarted, 1, 1, "Alice", "lobby").await.unwrap();
    // The counter restarted too, yet the id differs: it carries the
    // incarnation.
    assert_ne!(again.connection, old.connection);
    for incarnation in [1, 2] {
        let stale = restarted
            .handle_remote(WireRequest {
                incarnation,
                body: WireBody::Action {
                    connection: old.connection,
                    uuid: [1; 16],
                    action: PlayerAction::Message("for the old session".into()),
                },
            })
            .await;
        assert_eq!(stale, WireResult::Stale);
    }
    assert!(again.actions.try_recv().is_err());
    // The old process's leftovers change nothing.
    drop(old);
    assert_eq!(cluster.live().len(), 1);
    // A routes to the new session once it has its attributes.
    cluster.pump();
    assert_eq!(a.roster().remote_by_name("Alice").unwrap().incarnation, 2);
    assert!(
        a.deliver("Alice", PlayerAction::Message("hello".into()))
            .await
            .unwrap()
    );
    assert_eq!(
        next(&mut again).await,
        PlayerAction::Message("hello".into())
    );
}

#[tokio::test]
async fn actions_for_another_peers_players_reach_their_sessions() {
    let cluster = MemoryCluster::new();
    let a = peer(&cluster, 1, 1);
    let b = peer(&cluster, 2, 1);
    let hooks = Arc::new(Hooks::default());
    b.set_session_hooks(hooks.clone());
    let mut alice = login(&b, 1, 1, "Alice", "lobby").await.unwrap();
    let mut bob = login(&a, 1, 2, "Bob", "games-1").await.unwrap();
    cluster.pump();

    // Waiting for the answer, by name and by UUID.
    assert!(
        a.deliver("alice", PlayerAction::Message("hi".into()))
            .await
            .unwrap()
    );
    assert_eq!(next(&mut alice).await, PlayerAction::Message("hi".into()));
    assert!(
        a.deliver(&"01".repeat(16), PlayerAction::ActionBar("bar".into()))
            .await
            .unwrap()
    );
    assert_eq!(
        next(&mut alice).await,
        PlayerAction::ActionBar("bar".into())
    );
    assert!(
        !a.deliver("Nobody", PlayerAction::Message("x".into()))
            .await
            .unwrap()
    );
    // The bus and HTTP form of a transfer, which waits.
    let transferred = crate::players_service::Request::Transfer {
        target: "Alice".into(),
        server: "survival".into(),
    }
    .run(&a)
    .await
    .unwrap();
    assert_eq!(transferred, Value::Bool(true));
    assert_eq!(
        next(&mut alice).await,
        PlayerAction::Transfer("survival".into())
    );
    // Synchronous callers (Lua, plugins, BungeeCord) hand it to the peer.
    assert!(a.send("Alice", PlayerAction::Kick("bye".into())).unwrap());
    assert_eq!(next(&mut alice).await, PlayerAction::Kick("bye".into()));
    assert!(a.send_named("ALICE", PlayerAction::Message("named".into())));
    assert_eq!(
        next(&mut alice).await,
        PlayerAction::Message("named".into())
    );
    assert!(a.forward_to_player("Alice", b"data"));
    assert_eq!(
        next(&mut alice).await,
        PlayerAction::Forward {
            server: "lobby".into(),
            data: b"data".to_vec()
        }
    );
    // Lobby has players only on B; games-1 has Bob here.
    assert_eq!(a.forward_to_servers(|_| true, b"all"), 2);
    assert!(
        matches!(next(&mut alice).await, PlayerAction::Forward { server, .. } if server == "lobby")
    );
    assert!(
        matches!(next(&mut bob).await, PlayerAction::Forward { server, .. } if server == "games-1")
    );
    // Boss bars: the owning peer keeps the limit.
    assert!(a.boss_bar("Alice", "event", None).unwrap());
    assert!(
        matches!(next(&mut alice).await, PlayerAction::BossBar { name, bar: None } if name == "event")
    );

    // Broadcasts reach every peer; the routed form counts them all.
    let unrendered = |text: &Text| Unrendered::new(text.clone(), None, "message").unwrap();
    let everyone = Text::from("everyone");
    assert_eq!(
        a.broadcast_routed(&unrendered(&everyone), None)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        next(&mut alice).await,
        PlayerAction::Message(everyone.clone())
    );
    assert_eq!(next(&mut bob).await, PlayerAction::Message(everyone));
    let games = Text::from("games");
    assert_eq!(
        b.broadcast_routed(&unrendered(&games), Some("games"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(next(&mut bob).await, PlayerAction::Message(games));
    assert!(alice.actions.try_recv().is_err());
    // The synchronous broadcast counts this peer and sends the rest on.
    assert_eq!(b.broadcast(&unrendered(&"sync".into()), None).unwrap(), 1);
    assert_eq!(next(&mut alice).await, PlayerAction::Message("sync".into()));
    assert_eq!(next(&mut bob).await, PlayerAction::Message("sync".into()));
    // Text that names a message reaches each player in their own language,
    // whichever peer holds them.
    catalog::install_test_catalog();
    alice._handle.set_locale(Some("de_de"));
    cluster.pump();
    let greeting = Unrendered::new(
        Text::Component(json!({"message": "party:greet", "args": {"name": "all"}})),
        None,
        "message",
    )
    .unwrap();
    let words = |action: PlayerAction| match action {
        PlayerAction::Message(text) => text.to_string(),
        other => panic!("{other:?}"),
    };
    assert_eq!(a.broadcast_routed(&greeting, None).await.unwrap(), 2);
    assert_eq!(words(next(&mut alice).await), "Hallo all.");
    assert_eq!(words(next(&mut bob).await), "Hello all.");
    assert_eq!(b.broadcast(&greeting, None).unwrap(), 1);
    assert_eq!(words(next(&mut alice).await), "Hallo all.");
    assert_eq!(words(next(&mut bob).await), "Hello all.");

    // `rift admin transfer` of another peer's connection id.
    let result = a
        .route_transfer(alice.connection, "survival", true)
        .await
        .unwrap();
    assert_eq!(result["backend"], "survival");
    assert_eq!(result["connection_id"], alice.connection);
    assert_eq!(
        a.route_transfer(alice.connection + 1, "survival", true)
            .await
            .unwrap_err(),
        "player is no longer online; refresh status"
    );
    assert!(a.transfer_remote("Alice", "survival"));

    // A dialog shown from A brings its clicks back to A.
    let a_hooks = Arc::new(Hooks::default());
    a.set_session_hooks(a_hooks.clone());
    let dialog = Dialog::parse(&json!({"type": "notice", "title": "x"})).unwrap();
    assert!(
        a.dialog("Alice", Some(dialog), Owner::Plugin("menu".into()))
            .unwrap()
    );
    let PlayerAction::Dialog { owner, .. } = next(&mut alice).await else {
        panic!("expected a dialog");
    };
    assert_eq!(
        owner,
        Owner::Remote {
            peer: 1,
            incarnation: 1,
            plugin: Some("menu".into())
        }
    );
    let Owner::Remote {
        peer,
        incarnation,
        plugin,
    } = owner
    else {
        unreachable!();
    };
    let context = Context {
        id: alice.connection,
        name: "Alice".into(),
        ..Context::default()
    };
    b.route_dialog_click(peer, incarnation, plugin.clone(), context)
        .await
        .unwrap();
    let clicks = a_hooks.clicks.lock().unwrap().clone();
    assert_eq!(clicks.len(), 1);
    assert_eq!(clicks[0].0, Owner::Plugin("menu".into()));
    assert_eq!(clicks[0].1.name, "Alice");
    assert_eq!(
        b.route_dialog_click(peer, incarnation + 1, plugin, Context::default())
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(hooks.clicks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn menus_for_another_peers_players_bring_their_events_back() {
    use crate::menu::{Click, CloseReason, Menu, MenuEvent, Patch};
    let cluster = MemoryCluster::new();
    let a = peer(&cluster, 1, 1);
    let b = peer(&cluster, 2, 1);
    let a_hooks = Arc::new(Hooks::default());
    a.set_session_hooks(a_hooks.clone());
    let mut alice = login(&b, 1, 1, "Alice", "lobby").await.unwrap();
    let mut old = login_with(&b, 2, 3, "Old", "lobby", 316).await.unwrap();
    cluster.pump();
    let remote = Owner::Remote {
        peer: 1,
        incarnation: 1,
        plugin: Some("shop".into()),
    };
    let spec = json!({
        "id": "shop", "rows": 1, "title": "Shop", "state": {"page": 1},
        "slots": [
            {"slot": 0, "item": "diamond", "id": "buy", "payload": {"price": 5}},
            {"slot": 1, "item": "player_head", "head": {"player": "Alice"}},
        ],
    });
    let menu = Menu::parse(&spec).unwrap();
    let owner = Owner::Plugin("shop".into());

    // Shown from A to a player of B: the menu crosses whole.
    assert!(
        a.menu("Alice", Some(menu.clone()), owner.clone(), None)
            .unwrap()
    );
    let PlayerAction::Menu(shown) = next(&mut alice).await else {
        panic!("expected a menu");
    };
    assert_eq!(shown.owner, remote);
    assert_eq!(*shown.menu, menu);
    // A client older than 1.12 gets none.
    assert!(
        !a.menu("Old", Some(menu.clone()), owner.clone(), None)
            .unwrap()
    );

    // A patch and a close cross too.
    let patch = Patch::parse(&json!({"title": "Sold", "state": {"page": 2}})).unwrap();
    assert!(
        a.update_menu("Alice", "shop", patch.clone(), owner.clone())
            .unwrap()
    );
    let PlayerAction::UpdateMenu(update) = next(&mut alice).await else {
        panic!("expected an update");
    };
    assert_eq!(
        (update.owner.clone(), update.id.as_str(), &*update.patch),
        (remote.clone(), "shop", &patch)
    );
    assert!(!a.update_menu("Old", "shop", patch, owner.clone()).unwrap());
    assert!(
        a.menu("Alice", None, owner.clone(), Some("shop".into()))
            .unwrap()
    );
    assert_eq!(
        next(&mut alice).await,
        PlayerAction::CloseMenu {
            owner: remote.clone(),
            id: Some("shop".into())
        }
    );

    // B's session reports the events to the owner it was given: back to A.
    let Owner::Remote {
        peer,
        incarnation,
        plugin,
    } = remote
    else {
        unreachable!();
    };
    for event in [
        MenuEvent::click(&menu, 0, Click::Left, None),
        MenuEvent::close(&menu, CloseReason::Plugin),
    ] {
        let context = Context {
            id: alice.connection,
            name: "Alice".into(),
            menu: Some(event),
            ..Context::default()
        };
        b.route_menu_event(peer, incarnation, plugin.clone(), context)
            .await
            .unwrap();
    }
    let events = a_hooks.menus.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|(owner, context)| {
        *owner == Owner::Plugin("shop".into()) && context.name == "Alice"
    }));
    let click = events[0].1.menu.as_ref().unwrap();
    assert_eq!(
        (click.action.as_deref(), &click.payload, &click.state),
        (
            Some("buy"),
            &Some(json!({"price": 5})),
            &Some(json!({"page": 1}))
        )
    );
    assert_eq!(
        events[1].1.menu.as_ref().unwrap().reason,
        Some(CloseReason::Plugin)
    );
    // A restarted A no longer gets them.
    assert_eq!(
        b.route_menu_event(peer, incarnation + 1, plugin, Context::default())
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(a_hooks.clicks.lock().unwrap().is_empty());
    assert!(old.actions.try_recv().is_err());
}

#[tokio::test]
async fn every_peer_counts_and_lists_the_whole_network() {
    let cluster = MemoryCluster::new();
    let a = peer(&cluster, 1, 1);
    let b = peer(&cluster, 2, 1);
    let _alice = login(&b, 1, 1, "Alice", "lobby").await.unwrap();
    let _bob = login(&a, 1, 2, "Bob", "lobby").await.unwrap();
    let carol = login(&b, 2, 3, "Carol", "survival").await.unwrap();
    // Hidden players stay hidden on other peers.
    b.roster()
        .set_visibility(&[3; 16], Some("staff.see".into()));
    let generation = a.roster().generation();
    cluster.pump();
    assert!(a.roster().generation() > generation);
    for players in [&a, &b] {
        let roster = players.roster();
        assert_eq!(roster.len(), 3);
        assert_eq!(roster.count_for(Viewer::System, None), 3);
        assert_eq!(roster.count_for(Viewer::Public, None), 2);
        assert_eq!(roster.count_for(Viewer::System, Some("lobby")), 2);
        assert_eq!(roster.server_counts()["lobby"], 2);
        assert_eq!(roster.count_on_server("survival"), 1);
        assert_eq!(
            roster
                .server_counts_for(Viewer::Backend)
                .get("survival")
                .copied(),
            None
        );
        let names: Vec<_> = roster
            .snapshot()
            .into_iter()
            .map(|player| player.name)
            .collect();
        assert_eq!(names, ["Alice", "Bob", "Carol"]);
        let entries = roster.tab_entries("survival", Viewer::Public, |_| true);
        assert_eq!(entries.len(), 2);
        let mut listed: Vec<_> = players
            .list(None)
            .unwrap()
            .into_iter()
            .map(|info| (info.name, info.peer))
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            [
                ("Alice".to_owned(), Some("2".to_owned())),
                ("Bob".to_owned(), Some("1".to_owned())),
                ("Carol".to_owned(), Some("2".to_owned())),
            ]
        );
        assert_eq!(players.list(Some("survival")).unwrap().len(), 1);
        assert_eq!(players.names_with_prefix("c"), ["Carol"]);
        assert_eq!(
            players.get("carol").unwrap().unwrap().connection_id,
            carol.connection
        );
        assert_eq!(players.names().len(), 3);
    }
    // A player that leaves leaves everywhere.
    drop(carol);
    cluster.pump();
    assert_eq!(a.roster().len(), 2);
    assert!(a.get("Carol").unwrap().is_none());
}

#[test]
fn attributes_resync_after_a_gap_and_a_new_incarnation() {
    let cluster = MemoryCluster::new();
    cluster.join(1, 1);
    cluster.join(2, 1);
    let (a, b) = (memory::registry(&cluster, 1), memory::registry(&cluster, 2));
    let alice = b.register([1; 16], "Alice", "lobby").unwrap();
    alice.set_server("survival");
    let resync = b.full_resync();
    let gap = AttributeUpdate {
        peer: 2,
        incarnation: 1,
        sequence: resync.sequence + 2,
        body: UpdateBody::Changes(Vec::new()),
    };
    // Changes before any resync, or after a gap, ask for one.
    assert_eq!(a.apply_remote(gap.clone()), ApplyOutcome::NeedResync);
    assert_eq!(a.apply_remote(resync.clone()), ApplyOutcome::Applied);
    assert_eq!(a.get(&[1; 16]).unwrap().server, "survival");
    assert_eq!(a.apply_remote(gap), ApplyOutcome::NeedResync);
    // Own updates and older incarnations are ignored.
    assert_eq!(b.apply_remote(resync.clone()), ApplyOutcome::Ignored);
    let newer = AttributeUpdate {
        incarnation: 2,
        body: UpdateBody::Resync(Vec::new()),
        ..resync.clone()
    };
    assert_eq!(a.apply_remote(newer), ApplyOutcome::Applied);
    assert!(a.is_empty());
    assert_eq!(a.apply_remote(resync), ApplyOutcome::Ignored);
    assert!(a.is_empty());
    drop(alice);
}

#[tokio::test]
async fn a_fenced_peer_refuses_logins_and_disconnects_after_the_grace() {
    let cluster = MemoryCluster::new();
    let a = peer(&cluster, 1, 1);
    let mut alice = login(&a, 1, 1, "Alice", "lobby").await.unwrap();
    a.roster()
        .set_admission(Admission::Fenced { message: None });
    let Err(error) = login(&a, 2, 2, "Bob", "lobby").await else {
        panic!("a fenced peer admitted a login");
    };
    assert_eq!(
        crate::text::reason(&error),
        catalog::notice(Notice::ClusterPartition)
    );
    // Without a quorum no claim is decided, fenced or not.
    a.roster().set_admission(Admission::Open);
    cluster.partition(1);
    let Err(error) = login(&a, 3, 2, "Bob", "lobby").await else {
        panic!("a partitioned peer admitted a login");
    };
    assert_eq!(
        crate::text::reason(&error),
        catalog::notice(Notice::LoginUnavailable)
    );
    // After `partition_grace` the cluster step disconnects everyone.
    let message = catalog::notice(Notice::ClusterPartition);
    assert_eq!(a.disconnect_all(message.clone()).await, 1);
    assert_eq!(next(&mut alice).await, PlayerAction::Kick(message));
}

#[tokio::test]
async fn a_draining_peer_spreads_transferable_players_and_disconnects_the_others() {
    use crate::extensions::HandOff;
    let cluster = MemoryCluster::new();
    let a = peer(&cluster, 1, 1);
    let mut alice = login(&a, 1, 1, "Alice", "lobby").await.unwrap();
    let mut bob = login(&a, 2, 2, "Bob", "lobby").await.unwrap();
    // 1.8 knows no Transfer packet.
    let mut carol = login_with(&a, 3, 3, "Carol", "lobby", 47).await.unwrap();
    assert_eq!(a.local_sessions(), 3);
    let message = catalog::notice(Notice::ClusterLeaving);
    let targets = vec![
        ("10.0.0.2".to_owned(), 25565),
        ("10.0.0.3".to_owned(), 25566),
    ];
    let done = a.hand_off(&targets, message.clone()).await;
    assert_eq!(
        done,
        HandOff {
            transferred: 2,
            disconnected: 1
        }
    );
    // One target each, in turn.
    assert_eq!(
        next(&mut alice).await,
        PlayerAction::Handoff {
            host: "10.0.0.2".into(),
            port: 25565,
            fallback: message.clone(),
        }
    );
    assert_eq!(
        next(&mut bob).await,
        PlayerAction::Handoff {
            host: "10.0.0.3".into(),
            port: 25566,
            fallback: message.clone(),
        }
    );
    assert_eq!(next(&mut carol).await, PlayerAction::Kick(message.clone()));
    // Without a peer to go to, everyone is asked to reconnect.
    let done = a.hand_off(&[], message.clone()).await;
    assert_eq!(
        done,
        HandOff {
            transferred: 0,
            disconnected: 3
        }
    );
    for session in [&mut alice, &mut bob, &mut carol] {
        assert_eq!(next(session).await, PlayerAction::Kick(message.clone()));
    }
}

/// Peer 2 of a cluster held Alice and Dave, was cut off and expired, and
/// Alice logged in again through peer 1 meanwhile; now peer 2 is back.
struct Rejoined {
    _a: Players,
    b: Players,
    /// Peer 2's subscription to the claims, with what it saw before the
    /// partition drained.
    changes: tokio::sync::broadcast::Receiver<ClaimChange>,
    alice: Session,
    dave: Session,
    _alice_on_a: Session,
}

async fn partitioned_while_alice_moved(cluster: &MemoryCluster) -> Rejoined {
    let a = peer(cluster, 1, 1);
    let b = peer(cluster, 2, 1);
    let mut changes = subscribe(&b);
    let alice = login(&b, 1, 1, "Alice", "lobby").await.unwrap();
    let dave = login(&b, 2, 4, "Dave", "lobby").await.unwrap();
    while changes.try_recv().is_ok() {}
    cluster.partition(2);
    cluster.expire(2);
    let alice_on_a = login(&a, 1, 1, "Alice", "lobby").await.unwrap();
    cluster.heal(2);
    Rejoined {
        _a: a,
        b,
        changes,
        alice,
        dave,
        _alice_on_a: alice_on_a,
    }
}

#[tokio::test]
async fn a_rejoining_peer_learns_from_the_log_whom_it_lost() {
    let cluster = MemoryCluster::new();
    let Rejoined {
        _a: _peer_a,
        b,
        mut changes,
        mut alice,
        mut dave,
        _alice_on_a: _moved,
    } = partitioned_while_alice_moved(&cluster).await;
    // B catches up with the claims it missed: Alice is disconnected, once.
    let mut disconnected = 0;
    while let Ok(change) = changes.try_recv() {
        disconnected += b.apply_claim_change(&change);
    }
    assert_eq!(disconnected, 1);
    let kicked = PlayerAction::Kick(catalog::notice(Notice::LoginAlreadyConnected));
    assert_eq!(next(&mut alice).await, kicked);
    // Dave's claim was released, not taken: claiming it anew keeps him.
    assert_eq!(b.reclaim().await, 0);
    assert_eq!(b.reconcile_claims(), 0);
    assert!(alice.actions.try_recv().is_err());
    assert!(dave.actions.try_recv().is_err());
    let live = cluster.live();
    assert!(
        live.iter()
            .any(|record| record.key == "dave" && record.holder.peer == 2)
    );
    assert!(
        live.iter()
            .any(|record| record.key == "alice" && record.holder.peer == 1)
    );
}

#[tokio::test]
async fn a_rejoining_peer_disconnects_players_its_fresh_claims_lose() {
    let cluster = MemoryCluster::new();
    let Rejoined {
        _a: _peer_a,
        b,
        mut changes,
        mut alice,
        mut dave,
        _alice_on_a: _moved,
    } = partitioned_while_alice_moved(&cluster).await;
    // B claims its players again before it has read the log.
    assert_eq!(b.reclaim().await, 1);
    let kicked = PlayerAction::Kick(catalog::notice(Notice::LoginAlreadyConnected));
    assert_eq!(next(&mut alice).await, kicked);
    assert!(dave.actions.try_recv().is_err());
    // The log says the same afterwards; nobody is disconnected twice.
    let mut disconnected = 0;
    while let Ok(change) = changes.try_recv() {
        disconnected += b.apply_claim_change(&change);
    }
    assert_eq!(disconnected, 0);
    assert!(alice.actions.try_recv().is_err());
    assert!(dave.actions.try_recv().is_err());
}

#[test]
fn player_actions_and_their_answers_cross_the_wire() {
    let dialog = Arc::new(Dialog::parse(&json!({"type": "notice", "title": "x"})).unwrap());
    for action in [
        PlayerAction::Message("hello".into()),
        PlayerAction::Title {
            title: Some("title".into()),
            subtitle: None,
            times: [1, 2, 3],
        },
        PlayerAction::Forward {
            server: "lobby".into(),
            data: vec![0, 255],
        },
        PlayerAction::TabList(Some(crate::tab_list::Override {
            header: Some(json!({"text": "header"})),
            footer: None,
        })),
        PlayerAction::BossBar {
            name: "event".into(),
            bar: Some(crate::protocol::BossBar {
                text: json!("x"),
                progress: 0.5,
                color: 1,
                style: 2,
                flags: 3,
            }),
        },
        PlayerAction::BossBar {
            name: "event".into(),
            bar: None,
        },
        PlayerAction::Dialog {
            dialog: dialog.clone(),
            owner: Owner::Remote {
                peer: 1,
                incarnation: 2,
                plugin: None,
            },
        },
        PlayerAction::ClearDialog,
        PlayerAction::Menu(crate::menu::Shown {
            menu: Arc::new(
                crate::menu::Menu::parse(&json!({
                    "id": "shop", "type": "hopper", "title": {"text": "Shop", "bold": true},
                    "state": {"page": 1},
                    "slots": [{
                        "slot": 0, "item": "leather_helmet", "name": "Hat", "lore": ["a", "b"],
                        "glint": true, "model": {"floats": [1.5], "strings": ["x"]},
                        "item_model": "rift:hat", "color": "#112233", "details": true,
                        "id": "buy", "payload": {"price": 5}, "clicks": "all", "close": true,
                    }, {
                        "slot": 1, "item": "player_head",
                        "head": {"texture": "abc", "signature": "def"},
                    }],
                }))
                .unwrap(),
            ),
            owner: Owner::Plugin("shop".into()),
            heads: Arc::new(BTreeMap::from([(
                "Alice".to_owned(),
                crate::menu::Textures {
                    value: "v".into(),
                    signature: None,
                },
            )])),
        }),
        PlayerAction::UpdateMenu(crate::menu::Update {
            owner: Owner::Script,
            id: "shop".into(),
            patch: Arc::new(
                crate::menu::Patch::parse(&json!({
                    "slots": [{"slots": [1, 2], "item": "stone"}, {"slot": 3, "empty": true}],
                    "keys": {"#": {"item": "apple"}},
                    "title": "New", "state": [1, 2],
                }))
                .unwrap(),
            ),
            heads: Arc::default(),
        }),
        PlayerAction::UpdateMenu(crate::menu::Update {
            owner: Owner::Script,
            id: "shop".into(),
            patch: Arc::new(crate::menu::Patch::parse(&json!({"title": ""})).unwrap()),
            heads: Arc::default(),
        }),
        PlayerAction::CloseMenu {
            owner: Owner::Script,
            id: None,
        },
    ] {
        let request = WireRequest {
            incarnation: 3,
            body: WireBody::Action {
                connection: 9,
                uuid: [1; 16],
                action,
            },
        };
        let text = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<WireRequest>(&text).unwrap(), request);
    }
    for result in [
        WireResult::Sent(true),
        WireResult::Count(4),
        WireResult::Transfer(Ok(json!({"backend": "lobby"}))),
        WireResult::Transfer(Err("no".into())),
        WireResult::Error("limit".into()),
        WireResult::Stale,
    ] {
        let text = serde_json::to_string(&result).unwrap();
        assert_eq!(serde_json::from_str::<WireResult>(&text).unwrap(), result);
    }
}

#[tokio::test]
async fn events_of_a_menu_shown_from_another_peer_count_against_the_limit_there() {
    use crate::menu::{Click, CloseReason, MAX_PENDING_EVENTS, Menu, MenuEvent};
    let cluster = MemoryCluster::new();
    let a = peer(&cluster, 1, 1);
    let b = peer(&cluster, 2, 1);
    let a_hooks = Arc::new(Hooks::default());
    a.set_session_hooks(a_hooks.clone());
    let alice = login(&b, 1, 1, "Alice", "lobby").await.unwrap();
    cluster.pump();
    let menu = Menu::parse(&json!({"id": "shop", "slots": [
        {"slot": 0, "item": "diamond", "id": "buy"},
    ]}))
    .unwrap();
    let route = |event| {
        let context = Context {
            id: alice.connection,
            uuid: "01".repeat(16),
            name: "Alice".into(),
            menu: Some(event),
            ..Context::default()
        };
        b.route_menu_event(1, 1, Some("shop".into()), context)
    };
    // A's plugin has not taken any of them yet: B's session counted each
    // only until it crossed, so A counts them while they wait.
    for _ in 0..MAX_PENDING_EVENTS + 8 {
        route(MenuEvent::click(&menu, 0, Click::Left, None))
            .await
            .unwrap();
    }
    assert_eq!(a_hooks.menus.lock().unwrap().len(), MAX_PENDING_EVENTS);
    // An ending always arrives.
    route(MenuEvent::close(&menu, CloseReason::Player))
        .await
        .unwrap();
    assert_eq!(a_hooks.menus.lock().unwrap().len(), MAX_PENDING_EVENTS + 1);
    // Once the plugin has them, clicks arrive again.
    a_hooks.menus.lock().unwrap().clear();
    route(MenuEvent::click(&menu, 0, Click::Left, None))
        .await
        .unwrap();
    assert_eq!(a_hooks.menus.lock().unwrap().len(), 1);
}
