use super::*;
use crate::bungeecord::{self, Request};

fn fields(values: &[&str]) -> Vec<u8> {
    let mut data = Vec::new();
    for value in values {
        bungeecord::write_utf(value, &mut data).unwrap();
    }
    data
}

fn custom(id: i32, channel: &str, payload: &[u8]) -> Packet {
    let mut data = Vec::new();
    write_string(channel, &mut data);
    data.extend_from_slice(payload);
    Packet::new(id, data)
}

async fn playing(
    number: i32,
) -> (
    Session<DuplexStream, DuplexStream>,
    DuplexStream,
    DuplexStream,
) {
    let (mut session, client, backend) = session().await;
    session.handshake.protocol = number;
    session.version = Some(ProtocolVersion::new(number).unwrap());
    session.client.state = ConnectionState::new(State::Play);
    session.backend.as_mut().unwrap().state = ConnectionState::new(State::Play);
    session.joined = true;
    (session, client, backend)
}

#[tokio::test]
async fn every_supported_protocol_reserves_aliases_registers_and_replies_to_backend() {
    timeout(Duration::from_secs(5), async {
        // Independent expected wire IDs, including release-specific shifts.
        for (protocols, serverbound, clientbound) in [
            (vec![47], 0x17, 0x3f),
            (
                vec![107, 108, 109, 110, 210, 315, 316, 338, 340],
                0x09,
                0x18,
            ),
            (vec![335], 0x0a, 0x18),
            (vec![393, 401, 404], 0x0a, 0x19),
            (vec![477, 480, 485, 490, 498, 735, 736], 0x0b, 0x18),
            (vec![573, 575, 578], 0x0b, 0x19),
            (vec![751, 753, 754], 0x0b, 0x17),
            (vec![755, 756, 757, 758], 0x0a, 0x18),
            (vec![759, 761], 0x0c, 0x15),
            (vec![760], 0x0d, 0x16),
            (vec![762, 763], 0x0d, 0x17),
            (vec![764], 0x0f, 0x18),
            (vec![765], 0x10, 0x18),
            (vec![766, 767], 0x12, 0x19),
            (vec![768, 769], 0x14, 0x19),
            (vec![770], 0x14, 0x18),
            (vec![771, 772, 773, 774], 0x15, 0x18),
            (vec![775, 776, 777], 0x16, 0x18),
        ] {
            for number in protocols {
                let (mut session, mut client, mut backend) = playing(number).await;
                session.enable_bungeecord();
                let codec = Codec::default();
                let (register, channel) = if number < 393 {
                    ("REGISTER", "BungeeCord")
                } else {
                    ("minecraft:register", "bungeecord:main")
                };
                assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
                assert_eq!(
                    receive(&mut backend, codec).await,
                    custom(serverbound, register, channel.as_bytes()),
                    "registration for protocol {number}"
                );

                for alias in ["BungeeCord", "bungeecord:main"] {
                    let request = fields(&["PlayerList", "ALL"]);
                    codec
                        .write(&mut backend, &custom(clientbound, alias, &request))
                        .await
                        .unwrap();
                    assert_eq!(
                        session.forward().await.unwrap(),
                        SessionEvent::BungeeCord(Request::PlayerList("ALL".into()))
                    );

                    // Neither a well-formed request nor a malformed spoof from
                    // the client may reach a backend plugin listening here.
                    for payload in [&request[..], &[0, 7, 1][..]] {
                        codec
                            .write(&mut client, &custom(serverbound, alias, payload))
                            .await
                            .unwrap();
                        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
                    }
                    let unrelated = custom(serverbound, "test:other", b"opaque");
                    codec.write(&mut client, &unrelated).await.unwrap();
                    session.forward().await.unwrap();
                    assert_eq!(receive(&mut backend, codec).await, unrelated);

                    for payload in [vec![], fields(&["Connect"]), fields(&["Unknown", "opaque"])] {
                        codec
                            .write(&mut backend, &custom(clientbound, alias, &payload))
                            .await
                            .unwrap();
                        assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
                    }
                    let unrelated = custom(clientbound, "test:other", b"opaque");
                    codec.write(&mut backend, &unrelated).await.unwrap();
                    session.forward().await.unwrap();
                    assert_eq!(receive(&mut client, codec).await, unrelated);
                }
                let reply = fields(&["PlayerList", "ALL", "Player, Friend"]);
                session.send_bungeecord(&reply).await.unwrap();
                assert_eq!(
                    receive(&mut backend, codec).await,
                    custom(serverbound, channel, &reply)
                );
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn disabled_compatibility_keeps_both_directions_opaque() {
    timeout(Duration::from_secs(2), async {
        let (mut session, mut client, mut backend) = playing(774).await;
        let codec = Codec::default();
        for alias in ["BungeeCord", "bungeecord:main"] {
            let request = custom(0x18, alias, &fields(&["GetServer"]));
            codec.write(&mut backend, &request).await.unwrap();
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            assert_eq!(receive(&mut client, codec).await, request);
            let request = custom(0x15, alias, &fields(&["Connect", "lobby"]));
            codec.write(&mut client, &request).await.unwrap();
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            assert_eq!(receive(&mut backend, codec).await, request);
        }
        assert!(
            session
                .send_bungeecord(&fields(&["GetServer", "lobby"]))
                .await
                .is_err()
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn configuration_and_unjoined_requests_are_consumed_without_becoming_actions() {
    timeout(Duration::from_secs(2), async {
        for (number, serverbound, clientbound) in
            [(764, 1, 0), (765, 1, 0), (766, 2, 1), (777, 2, 1)]
        {
            let (mut session, mut client, mut backend) = playing(number).await;
            session.enable_bungeecord();
            session.client.state = ConnectionState::new(State::Configuration);
            session.backend.as_mut().unwrap().state = ConnectionState::new(State::Configuration);
            let codec = Codec::default();
            for alias in ["BungeeCord", "bungeecord:main"] {
                let request = fields(&["Connect", "lobby"]);
                codec
                    .write(&mut backend, &custom(clientbound, alias, &request))
                    .await
                    .unwrap();
                assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
                codec
                    .write(&mut client, &custom(serverbound, alias, &request))
                    .await
                    .unwrap();
                assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            }
            assert!(session.send_bungeecord(&[]).await.is_err());
            session.client.state = ConnectionState::new(State::Play);
            session.backend.as_mut().unwrap().state = ConnectionState::new(State::Play);
            session.joined = false;
            session.forward().await.unwrap();
            receive(&mut backend, codec).await; // Proxy channel registration.
            codec
                .write(
                    &mut backend,
                    &custom(
                        0x18 + i32::from(number == 766),
                        "bungeecord:main",
                        &fields(&["GetServers"]),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            assert!(session.send_bungeecord(&[]).await.is_err());
            let sentinel = Packet::new(0x7f, vec![42]);
            codec.write(&mut backend, &sentinel).await.unwrap();
            session.forward().await.unwrap();
            assert_eq!(receive(&mut client, codec).await, sentinel);
            codec.write(&mut client, &sentinel).await.unwrap();
            session.forward().await.unwrap();
            assert_eq!(receive(&mut backend, codec).await, sentinel);
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn response_cancellation_preserves_pending_bytes_and_payload_limit() {
    timeout(Duration::from_secs(2), async {
        let (mut session, _client, mut backend) = playing(774).await;
        session.enable_bungeecord();
        session.forward().await.unwrap();
        receive(&mut backend, Codec::default()).await;
        let (server, mut replacement) = tokio::io::duplex(16);
        session.backend = Some(Connection::new(server, State::Play));
        // Simulate an already registered replacement with backpressure.
        session.backend.as_mut().unwrap().bungeecord_registered = true;
        let payload = fields(&["PlayerList", "ALL", &"P".repeat(30000)]);
        assert!(
            timeout(Duration::from_millis(20), session.send_bungeecord(&payload))
                .await
                .is_err()
        );
        assert!(session.backend.as_ref().unwrap().pending.is_some());
        let (event, packet) = tokio::join!(
            session.forward(),
            receive(&mut replacement, Codec::default())
        );
        assert_eq!(event.unwrap(), SessionEvent::Packet);
        assert_eq!(packet, custom(0x15, "bungeecord:main", &payload));
        assert!(
            session
                .send_bungeecord(&vec![0; bungeecord::MAX_PAYLOAD_SIZE + 1])
                .await
                .is_err()
        );
        assert!(session.backend.as_ref().unwrap().pending.is_none());
    })
    .await
    .unwrap();
}

async fn finish_with_registration(
    session: &mut Session<DuplexStream, DuplexStream>,
    client: &mut DuplexStream,
    backend: &mut DuplexStream,
    client_codec: Codec,
    backend_codec: Codec,
) {
    backend_codec
        .write(backend, &Packet::empty(3))
        .await
        .unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(client, client_codec).await, Packet::empty(3));
    client_codec.write(client, &Packet::empty(3)).await.unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(backend, backend_codec).await, Packet::empty(3));
    session.forward().await.unwrap();
    assert_eq!(
        receive(backend, backend_codec).await,
        custom(0x15, "minecraft:register", b"bungeecord:main")
    );
    backend_codec.write(backend, &join_game(42)).await.unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(client, client_codec).await, join_game(42));
    assert!(session.can_switch());
}

#[tokio::test]
async fn compressed_login_and_replacement_register_each_backend_and_keep_queries_local() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut backend) = session().await;
        session.enable_bungeecord();
        let plain = Codec::default();
        plain
            .write(&mut backend, &Packet::new(3, vec![0]))
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, plain).await, Packet::new(3, vec![0]));
        let mut compressed = Codec::default();
        compressed.set_compression(0);
        compressed
            .write(&mut backend, &login_success())
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, compressed).await, login_success());
        compressed
            .write(&mut client, &Packet::empty(3))
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut backend, compressed).await, Packet::empty(3));
        finish_with_registration(
            &mut session,
            &mut client,
            &mut backend,
            compressed,
            compressed,
        )
        .await;

        let (server, mut replacement) = tokio::io::duplex(65536);
        let peers = async {
            assert_eq!(
                Handshake::decode(&receive(&mut replacement, plain).await)
                    .unwrap()
                    .protocol,
                774
            );
            assert_eq!(receive(&mut replacement, plain).await, login_start());
            plain
                .write(&mut replacement, &login_success())
                .await
                .unwrap();
            assert_eq!(receive(&mut replacement, plain).await, Packet::empty(3));
            assert_eq!(receive(&mut client, compressed).await, Packet::empty(0x74));
            compressed
                .write(&mut client, &Packet::empty(0x0f))
                .await
                .unwrap();
            assert_eq!(
                receive(&mut client, compressed).await,
                Packet::new(8, vec![0])
            );
        };
        let (result, ()) = tokio::join!(session.connect_backend(server), peers);
        result.unwrap();
        finish_with_registration(
            &mut session,
            &mut client,
            &mut replacement,
            compressed,
            plain,
        )
        .await;
        plain
            .write(
                &mut replacement,
                &custom(0x18, "BungeeCord", &fields(&["GetServer"])),
            )
            .await
            .unwrap();
        assert_eq!(
            session.forward().await.unwrap(),
            SessionEvent::BungeeCord(Request::GetServer)
        );
        let response = fields(&["GetServer", "survival"]);
        session.send_bungeecord(&response).await.unwrap();
        assert_eq!(
            receive(&mut replacement, plain).await,
            custom(0x15, "bungeecord:main", &response)
        );
        let sentinel = Packet::new(0x7f, vec![42]);
        plain.write(&mut replacement, &sentinel).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, compressed).await, sentinel);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn legacy_replacement_consumes_early_requests_registers_once_and_preserves_denials() {
    timeout(Duration::from_secs(3), async {
        for denied in [false, true] {
            let codec = Codec::default();
            let (mut client, input) = tokio::io::duplex(65536);
            let handshake = Handshake {
                protocol: 47,
                address: "play.test".into(),
                port: 25565,
                next_state: NextState::Login,
            };
            codec.write(&mut client, &handshake.packet()).await.unwrap();
            let mut session = Session::accept(input).await.unwrap();
            session.enable_bungeecord();
            let (server, mut old) = tokio::io::duplex(65536);
            session.connect_backend(server).await.unwrap();
            assert_eq!(receive(&mut old, codec).await, handshake.packet());
            let mut name = Vec::new();
            write_string("Player", &mut name);
            let start = Packet::new(0, name);
            codec.write(&mut client, &start).await.unwrap();
            session.forward().await.unwrap();
            assert_eq!(receive(&mut old, codec).await, start);
            let mut profile = Vec::new();
            write_string("00000000-0000-0000-0000-000000000007", &mut profile);
            write_string("Player", &mut profile);
            let success = Packet::new(2, profile);
            codec.write(&mut old, &success).await.unwrap();
            // The client success and backend registration may flush in either order.
            session.forward().await.unwrap();
            session.forward().await.unwrap();
            assert_eq!(receive(&mut client, codec).await, success);
            assert_eq!(
                receive(&mut old, codec).await,
                custom(0x17, "REGISTER", b"BungeeCord")
            );
            let mut world = 1_i32.to_be_bytes().to_vec();
            world.extend([1, 0, 0, 20]);
            write_string("flat", &mut world);
            world.push(0);
            let join = Packet::new(1, world);
            codec.write(&mut old, &join).await.unwrap();
            session.forward().await.unwrap();
            assert_eq!(receive(&mut client, codec).await, join);
            assert!(session.can_switch());

            let (server, mut replacement) = tokio::io::duplex(65536);
            let peers = async {
                assert_eq!(receive(&mut replacement, codec).await, handshake.packet());
                assert_eq!(receive(&mut replacement, codec).await, start);
                codec.write(&mut replacement, &success).await.unwrap();
                for (channel, payload) in [
                    ("BungeeCord", vec![0, 7, 1]),
                    ("bungeecord:main", fields(&["Connect", "premature"])),
                    ("BungeeCord", fields(&["Unknown", "opaque"])),
                ] {
                    codec
                        .write(&mut replacement, &custom(0x3f, channel, &payload))
                        .await
                        .unwrap();
                }
                if denied {
                    codec
                        .write(
                            &mut replacement,
                            &protocol::disconnect(
                                Some(ProtocolVersion::new(47).unwrap()),
                                State::Play,
                                "banned",
                            )
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    return;
                }
                codec.write(&mut replacement, &join).await.unwrap();
                let barrier = receive(&mut client, codec).await;
                assert_eq!(barrier.id, 0);
                codec.write(&mut client, &barrier).await.unwrap();
            };
            let (result, ()) = tokio::join!(session.connect_backend(server), peers);
            if denied {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
                assert!(!session.switch_in_progress());
                assert!(session.can_switch());
                let sentinel = Packet::new(0x7f, vec![42]);
                codec.write(&mut old, &sentinel).await.unwrap();
                session.forward().await.unwrap();
                assert_eq!(receive(&mut client, codec).await, sentinel);
                continue;
            }
            result.unwrap();
            assert!(session.can_switch());
            for id in [0x47, 0x45, 1, 7] {
                assert_eq!(receive(&mut client, codec).await.id, id);
            }
            session.forward().await.unwrap();
            assert_eq!(
                receive(&mut replacement, codec).await,
                custom(0x17, "REGISTER", b"BungeeCord")
            );
            codec
                .write(
                    &mut replacement,
                    &custom(0x3f, "BungeeCord", &fields(&["GetServer"])),
                )
                .await
                .unwrap();
            assert_eq!(
                session.forward().await.unwrap(),
                SessionEvent::BungeeCord(Request::GetServer)
            );
            let response = fields(&["GetServer", "survival"]);
            session.send_bungeecord(&response).await.unwrap();
            assert_eq!(
                receive(&mut replacement, codec).await,
                custom(0x17, "BungeeCord", &response)
            );
            let sentinel = Packet::new(0x7f, vec![42]);
            codec.write(&mut replacement, &sentinel).await.unwrap();
            session.forward().await.unwrap();
            assert_eq!(receive(&mut client, codec).await, sentinel);
        }
    })
    .await
    .unwrap();
}
