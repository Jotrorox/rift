//! BungeeCord queries use local presence and configured destinations. Requests
//! arrive only from the session's backend; transfers enter its bounded control
//! queue and use the ordinary policy and world-transition machinery.
use crate::runtime::Snapshot;
use rift::{
    bungeecord::{MAX_PAYLOAD_SIZE, Request, write_utf},
    players::Player,
    protocol::PlayerIdentity,
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
}
