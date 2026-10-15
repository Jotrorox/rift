//! BungeeCord queries use local presence and configured destinations. Requests
//! arrive only from the session's backend; transfers enter its bounded control
//! queue and use the ordinary policy and world-transition machinery. Messages,
//! kicks and forwarded plugin messages enter the target players' bounded action
//! queues, which their own sessions drain; a full queue drops the request.
use crate::runtime::Snapshot;
use rift::{
    bungeecord::{MAX_PAYLOAD_SIZE, Request, write_utf},
    extensions::PlayerAction,
    players::Player,
    protocol::{PlayerIdentity, validate_component},
};
use std::{io, net::SocketAddr, sync::Arc};

fn server<'a>(snapshot: &'a Snapshot, name: &str) -> Option<&'a str> {
    snapshot
        .config
        .backends
        .get_key_value(name)
        .or_else(|| {
            snapshot
                .config
                .backends
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
        })
        .map(|(name, _)| name.as_str())
}

fn player(snapshot: &Snapshot, name: &str) -> Option<Player> {
    snapshot
        .players
        .snapshot()
        .into_iter()
        .find(|player| player.name.eq_ignore_ascii_case(name))
}

/// Deliver to a case-insensitive player name, or to every player for uppercase `ALL`.
fn deliver(snapshot: &Snapshot, target: &str, action: PlayerAction) {
    let players = snapshot.extensions.players();
    if target == "ALL" {
        players.send_all(&action);
    } else {
        players.send_named(target, action);
    }
}

/// Parse a backend-supplied JSON text component; invalid components are dropped.
fn component(json: &str) -> Option<serde_json::Value> {
    let component = serde_json::from_str(json).ok()?;
    validate_component(&component).ok()?;
    Some(component)
}

fn strings(fields: &[&str]) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    for field in fields {
        write_utf(field, &mut data)?;
    }
    Ok(data)
}

fn uuid(uuid: &[u8; 16]) -> String {
    uuid.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn handle(
    snapshot: &Arc<Snapshot>,
    current: &str,
    identity: &PlayerIdentity,
    peer: SocketAddr,
    request: Request,
) -> io::Result<Option<Vec<u8>>> {
    let data = match request {
        Request::Connect(target) => {
            if let Some(target) = server(snapshot, &target) {
                snapshot
                    .control
                    .plugin_transfer(&identity.name, target, snapshot.clone());
            }
            return Ok(None);
        }
        Request::ConnectOther {
            player,
            server: target,
        } => {
            if let Some(target) = server(snapshot, &target) {
                snapshot
                    .control
                    .plugin_transfer(&player, target, snapshot.clone());
            }
            return Ok(None);
        }
        Request::Forward {
            server: target,
            data,
        } => {
            let players = snapshot.extensions.players();
            // As in Velocity, ALL and ONLINE both exclude the sending server and
            // skip servers without a player to carry the message.
            if target == "ALL" || target == "ONLINE" {
                players.forward_to_servers(|server| server != current, &data);
            } else if let Some(target) = server(snapshot, &target) {
                players.forward_to_servers(|server| server == target, &data);
            }
            return Ok(None);
        }
        Request::ForwardToPlayer { player, data } => {
            snapshot
                .extensions
                .players()
                .forward_to_player(&player, &data);
            return Ok(None);
        }
        Request::Message { player, message } => {
            deliver(snapshot, &player, PlayerAction::Message(message));
            return Ok(None);
        }
        Request::MessageRaw { player, message } => {
            if let Some(message) = component(&message) {
                deliver(snapshot, &player, PlayerAction::RawMessage(message));
            }
            return Ok(None);
        }
        Request::KickPlayer { player, reason } => {
            snapshot
                .extensions
                .players()
                .send_named(&player, PlayerAction::Kick(reason));
            return Ok(None);
        }
        Request::KickPlayerRaw { player, reason } => {
            if let Some(reason) = component(&reason) {
                snapshot
                    .extensions
                    .players()
                    .send_named(&player, PlayerAction::RawKick(reason));
            }
            return Ok(None);
        }
        Request::Ip => {
            let mut data = strings(&["IP", &peer.ip().to_canonical().to_string()])?;
            data.extend_from_slice(&i32::from(peer.port()).to_be_bytes());
            data
        }
        Request::IpOther(name) => {
            let Some(player) = player(snapshot, &name) else {
                return Ok(None);
            };
            let Some(peer) = snapshot.control.player_peer(&player.name) else {
                return Ok(None);
            };
            let mut data = strings(&[
                "IPOther",
                &player.name,
                &peer.ip().to_canonical().to_string(),
            ])?;
            data.extend_from_slice(&i32::from(peer.port()).to_be_bytes());
            data
        }
        query @ (Request::PlayerCount(_) | Request::PlayerList(_)) => {
            let (target, count) = match &query {
                Request::PlayerCount(target) => (target, true),
                Request::PlayerList(target) => (target, false),
                _ => unreachable!(),
            };
            let all = target == "ALL";
            let target = if all {
                "ALL"
            } else {
                let Some(target) = server(snapshot, target) else {
                    return Ok(None);
                };
                target
            };
            let mut names: Vec<_> = snapshot
                .players
                .snapshot()
                .into_iter()
                .filter(|player| all || player.server == target)
                .map(|player| player.name)
                .collect();
            let mut data = strings(&[if count { "PlayerCount" } else { "PlayerList" }, target])?;
            if count {
                let count = i32::try_from(names.len()).map_err(io::Error::other)?;
                data.extend_from_slice(&count.to_be_bytes());
            } else {
                names.sort();
                write_utf(&names.join(", "), &mut data)?;
            }
            data
        }
        Request::GetServers => strings(&[
            "GetServers",
            &snapshot
                .config
                .backends
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        ])?,
        Request::GetServer => strings(&["GetServer", current])?,
        Request::Uuid => {
            let Some(id) = identity.uuid else {
                return Ok(None);
            };
            strings(&["UUID", &uuid(&id)])?
        }
        Request::UuidOther(name) => {
            let Some(player) = player(snapshot, &name) else {
                return Ok(None);
            };
            strings(&["UUIDOther", &player.name, &uuid(&player.uuid)])?
        }
        Request::GetPlayerServer(name) => {
            let Some(player) = player(snapshot, &name) else {
                return Ok(None);
            };
            strings(&["GetPlayerServer", &player.name, &player.server])?
        }
        Request::ServerIp(target) => {
            let Some(target) = server(snapshot, &target) else {
                return Ok(None);
            };
            let address = snapshot.config.backends[target].address();
            // Backend parsing already validates this address. DNS names are
            // returned as configured, without issuing a lookup for a query.
            let (host, port) = if let Ok(address) = address.parse::<SocketAddr>() {
                (address.ip().to_string(), address.port())
            } else {
                let (host, port) = address.rsplit_once(':').expect("validated backend address");
                (
                    host.to_owned(),
                    port.parse::<u16>().expect("validated backend port"),
                )
            };
            let mut data = strings(&["ServerIP", target, &host])?;
            data.extend_from_slice(&port.to_be_bytes());
            data
        }
    };
    // Bukkit's serverbound custom-payload limit also applies to the combined
    // fields. Never truncate a list or wrap its Java UTF length prefix.
    Ok((data.len() <= MAX_PAYLOAD_SIZE).then_some(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift::{bungeecord::read_utf, config::Config};

    fn snapshot() -> Arc<Snapshot> {
        let mut config = Config::from_addresses("127.0.0.1:0", "127.0.0.1:25566").unwrap();
        config.network.bungeecord = true;
        for (name, address) in [
            ("Lobby", "Example.invalid:65535"),
            ("IPv6", "[::1]:25567"),
            ("all", "127.0.0.1:1"),
        ] {
            config
                .backends
                .insert(name.into(), address.parse().unwrap());
        }
        Arc::new(Snapshot::new(config, None).unwrap())
    }

    fn query(snapshot: &Arc<Snapshot>, request: Request) -> Option<Vec<u8>> {
        let identity = PlayerIdentity {
            name: "Alice".into(),
            uuid: Some([1; 16]),
        };
        handle(
            snapshot,
            "default",
            &identity,
            "[::ffff:127.0.0.1]:54321".parse().unwrap(),
            request,
        )
        .unwrap()
    }

    #[test]
    fn address_queries_preserve_dns_names_ipv6_and_port_widths() {
        let snapshot = snapshot();
        assert_eq!(
            query(&snapshot, Request::Ip).unwrap(),
            b"\x00\x02IP\x00\x09127.0.0.1\x00\x00\xd4\x31"
        );
        for (requested, name, host, port) in [
            ("LOBBY", "Lobby", "Example.invalid", 65535u16),
            ("ipv6", "IPv6", "::1", 25567),
        ] {
            let data = query(&snapshot, Request::ServerIp(requested.into())).unwrap();
            let mut bytes = data.as_slice();
            assert_eq!(read_utf(&mut bytes).unwrap(), "ServerIP");
            assert_eq!(read_utf(&mut bytes).unwrap(), name);
            assert_eq!(read_utf(&mut bytes).unwrap(), host);
            assert_eq!(bytes, port.to_be_bytes());
        }
    }

    #[test]
    fn uppercase_all_is_special_and_missing_lookups_are_silent() {
        let snapshot = snapshot();
        let _alice = snapshot
            .players
            .register([1; 16], "Alice", "Lobby")
            .unwrap();
        let _bob = snapshot.players.register([2; 16], "Bob", "all").unwrap();
        assert_eq!(
            query(&snapshot, Request::PlayerCount("ALL".into())).unwrap(),
            b"\0\x0bPlayerCount\0\x03ALL\0\0\0\x02"
        );
        assert_eq!(
            query(&snapshot, Request::PlayerCount("all".into())).unwrap(),
            b"\0\x0bPlayerCount\0\x03all\0\0\0\x01"
        );
        for request in [
            Request::PlayerCount("missing".into()),
            Request::PlayerList("missing".into()),
            Request::ServerIp("missing".into()),
            Request::UuidOther("missing".into()),
            Request::GetPlayerServer("missing".into()),
            Request::IpOther("Alice".into()),
        ] {
            assert_eq!(query(&snapshot, request), None);
        }
    }

    #[test]
    fn queries_track_committed_presence_and_disconnects_across_reload() {
        let snapshot = snapshot();
        let alice = snapshot
            .players
            .register([1; 16], "Alice", "Lobby")
            .unwrap();
        let current = Arc::new(Snapshot::new(snapshot.config.clone(), Some(&snapshot)).unwrap());
        alice.set_server("IPv6");
        assert_eq!(
            query(&current, Request::GetPlayerServer("aLiCe".into())).unwrap(),
            b"\0\x0fGetPlayerServer\0\x05Alice\0\x04IPv6"
        );
        assert_eq!(
            query(&snapshot, Request::PlayerList("lobby".into())).unwrap(),
            b"\0\x0aPlayerList\0\x05Lobby\0\0"
        );
        drop(alice);
        assert_eq!(query(&current, Request::UuidOther("Alice".into())), None);
    }

    #[test]
    fn a_server_named_all_does_not_turn_case_insensitive_lookup_into_global_selection() {
        let mut config = snapshot().config.clone();
        config.backends.remove("all");
        config
            .backends
            .insert("ALL".into(), "127.0.0.1:25568".parse().unwrap());
        let snapshot = Arc::new(Snapshot::new(config, None).unwrap());
        let _alice = snapshot.players.register([1; 16], "Alice", "ALL").unwrap();
        let _bob = snapshot
            .players
            .register([2; 16], "Bob", "default")
            .unwrap();
        assert_eq!(
            query(&snapshot, Request::PlayerCount("all".into())).unwrap(),
            b"\0\x0bPlayerCount\0\x03ALL\0\0\0\x01"
        );
        assert_eq!(
            query(&snapshot, Request::PlayerCount("ALL".into())).unwrap(),
            b"\0\x0bPlayerCount\0\x03ALL\0\0\0\x02"
        );
        assert_eq!(
            query(&snapshot, Request::PlayerList("all".into())).unwrap(),
            b"\0\x0aPlayerList\0\x03ALL\0\x05Alice"
        );
    }

    #[test]
    fn oversized_lists_are_omitted_without_wrapping_or_truncation() {
        let snapshot = snapshot();
        let registrations: Vec<_> = (0u128..2000)
            .map(|index| {
                snapshot
                    .players
                    .register(index.to_be_bytes(), format!("P{index:015}"), "default")
                    .unwrap()
            })
            .collect();
        assert_eq!(query(&snapshot, Request::PlayerList("ALL".into())), None);
        assert!(query(&snapshot, Request::PlayerCount("ALL".into())).is_some());
        drop(registrations);
        assert_eq!(
            query(&snapshot, Request::PlayerList("ALL".into())).unwrap(),
            b"\0\x0aPlayerList\0\x03ALL\0\0"
        );
    }

    #[tokio::test]
    async fn backend_transfers_use_bounded_player_queue_and_no_reply_receiver() {
        let snapshot = snapshot();
        let peer = "127.0.0.1:54321".parse().unwrap();
        let (_guard, mut receiver) =
            snapshot
                .control
                .register(1, "default".into(), 774, "Alice".into(), peer);
        assert_eq!(snapshot.control.player_peer("ALICE"), Some(peer));
        assert_eq!(
            query(
                &snapshot,
                Request::ConnectOther {
                    player: "aLiCe".into(),
                    server: "lobby".into()
                }
            ),
            None
        );
        query(&snapshot, Request::Connect("IPv6".into()));
        let request = receiver.try_recv().unwrap();
        assert_eq!(request.backend, "Lobby");
        assert_eq!(request.reply.reason(), "bungeecord");
        assert!(!request.reply.is_closed());
        assert!(request.reply.send(Ok(serde_json::json!({}))).is_ok());
        assert!(
            receiver.try_recv().is_err(),
            "a second queued transfer must be dropped"
        );
        query(&snapshot, Request::Connect("default".into()));
        query(&snapshot, Request::Connect("missing".into()));
        assert!(receiver.try_recv().is_err());
    }

    fn joined(
        snapshot: &Snapshot,
        id: u64,
        name: &str,
        server: &str,
    ) -> (
        rift::extensions::PlayerHandle,
        tokio::sync::mpsc::Receiver<PlayerAction>,
    ) {
        snapshot
            .extensions
            .players()
            .register(id, &[id as u8; 16], name, 774, server)
    }

    fn drain(queue: &mut tokio::sync::mpsc::Receiver<PlayerAction>) -> Vec<PlayerAction> {
        std::iter::from_fn(|| queue.try_recv().ok()).collect()
    }

    fn forward(server: &str, data: &[u8]) -> PlayerAction {
        PlayerAction::Forward {
            server: server.into(),
            data: data.to_vec(),
        }
    }

    #[test]
    fn forwards_reach_one_player_per_server_and_all_excludes_the_sender() {
        let snapshot = snapshot();
        // Alice sends from "default"; Bob and Carol share Lobby, the oldest
        // connection carries the message, and Dave is on IPv6.
        let (_alice, mut alice) = joined(&snapshot, 1, "Alice", "default");
        let (_bob, mut bob) = joined(&snapshot, 2, "Bob", "Lobby");
        let (_carol, mut carol) = joined(&snapshot, 3, "Carol", "Lobby");
        let (_dave, mut dave) = joined(&snapshot, 4, "Dave", "IPv6");
        for target in ["ALL", "ONLINE"] {
            let data = target.as_bytes();
            assert_eq!(
                query(
                    &snapshot,
                    Request::Forward {
                        server: target.into(),
                        data: data.to_vec()
                    }
                ),
                None
            );
            assert!(drain(&mut alice).is_empty());
            assert_eq!(drain(&mut bob), [forward("Lobby", data)]);
            assert!(drain(&mut carol).is_empty());
            assert_eq!(drain(&mut dave), [forward("IPv6", data)]);
        }
        for (target, data) in [("lobby", b"one"), ("default", b"own"), ("all", b"low")] {
            query(
                &snapshot,
                Request::Forward {
                    server: target.into(),
                    data: data.to_vec(),
                },
            );
        }
        assert_eq!(drain(&mut bob), [forward("Lobby", b"one")]);
        assert_eq!(drain(&mut alice), [forward("default", b"own")]);
        // "all" is a configured server without players, not a selector.
        assert!(drain(&mut dave).is_empty() && drain(&mut carol).is_empty());

        // A full queue passes the message to the next player on that server.
        while snapshot
            .extensions
            .players()
            .send_named("Bob", PlayerAction::Message("fill".into()))
        {}
        query(
            &snapshot,
            Request::Forward {
                server: "Lobby".into(),
                data: b"next".to_vec(),
            },
        );
        assert_eq!(drain(&mut carol), [forward("Lobby", b"next")]);

        query(
            &snapshot,
            Request::ForwardToPlayer {
                player: "dAVE".into(),
                data: b"direct".to_vec(),
            },
        );
        query(
            &snapshot,
            Request::ForwardToPlayer {
                player: "missing".into(),
                data: b"lost".to_vec(),
            },
        );
        assert_eq!(drain(&mut dave), [forward("IPv6", b"direct")]);
    }

    #[test]
    fn messages_and_kicks_queue_for_named_or_all_players_and_drop_invalid_components() {
        let snapshot = snapshot();
        let (_alice, mut alice) = joined(&snapshot, 1, "Alice", "default");
        let (_bob, mut bob) = joined(&snapshot, 2, "Bob", "Lobby");
        let message = |player: &str, message: &str| Request::Message {
            player: player.into(),
            message: message.into(),
        };
        let raw = |player: &str, message: &str| Request::MessageRaw {
            player: player.into(),
            message: message.into(),
        };
        for request in [
            message("ALL", "§aeveryone"),
            message("bob", "only Bob"),
            message("all", "a player named all"),
            raw("ALL", r#"{"text":"raw","color":"red"}"#),
            raw("Alice", "not json"),
            raw("Alice", "42"),
            raw("Alice", r#"["a",null]"#),
            Request::KickPlayer {
                player: "ALICE".into(),
                reason: "Bye".into(),
            },
            Request::KickPlayerRaw {
                player: "Bob".into(),
                reason: r#""Raw bye""#.into(),
            },
            Request::KickPlayerRaw {
                player: "Bob".into(),
                reason: "{".into(),
            },
            Request::KickPlayer {
                player: "ALL".into(),
                reason: "Not a selector".into(),
            },
        ] {
            assert_eq!(query(&snapshot, request), None);
        }
        let everyone = PlayerAction::Message("§aeveryone".into());
        let raw = PlayerAction::RawMessage(serde_json::json!({"text": "raw", "color": "red"}));
        assert_eq!(
            drain(&mut alice),
            [
                everyone.clone(),
                raw.clone(),
                PlayerAction::Kick("Bye".into())
            ]
        );
        assert_eq!(
            drain(&mut bob),
            [
                everyone,
                PlayerAction::Message("only Bob".into()),
                raw,
                PlayerAction::RawKick(serde_json::json!("Raw bye")),
            ]
        );
    }
}
