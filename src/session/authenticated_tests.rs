//! Deterministic encrypted wire fixtures. Profile keys, signatures, pack URLs,
//! and the session-server response are synthetic: these tests do not certify
//! Mojang authentication, signed-message validation, or a real Paper/plugin build.
use super::*;

type EncryptedSession = Session<DuplexStream, DuplexStream>;
type EncryptedClient = CryptoStream<DuplexStream>;

// Independent wire IDs match the release fixtures in tests/minecraft.py.
fn signed_command_id(number: i32) -> i32 {
    match number {
        761..=765 => 0x04,
        766..=767 => 0x05,
        768..=770 => 0x06,
        771..=774 => 0x07,
        775..=777 => 0x08,
        _ => panic!("unmapped authenticated fixture protocol"),
    }
}

fn chat_session(number: i32, generation: u8) -> Packet {
    let mut data = vec![generation; 16];
    data.extend(4_102_444_800_000_i64.to_be_bytes());
    write_varint(3, &mut data);
    data.extend(b"key");
    write_varint(256, &mut data);
    data.extend([generation; 256]);
    Packet::new(
        if number == 761 {
            0x20
        } else {
            signed_command_id(number) + 2
        },
        data,
    )
}

fn signed_chat(number: i32, generation: u8) -> Packet {
    let mut data = Vec::new();
    write_string("Authenticated wire fixture", &mut data);
    data.extend(1_800_000_000_000_i64.to_be_bytes());
    data.extend(i64::from(generation).to_be_bytes());
    data.push(1); // signature present
    data.extend([generation; 256]);
    write_varint(19, &mut data); // last-seen offset
    data.extend([0x55, 0xaa, 0x05]); // last-seen bitset
    if number >= 770 {
        data.push(0x35); // acknowledgement checksum
    }
    Packet::new(signed_command_id(number) + 1, data)
}

// Player-chat layouts: release protocol schemas used by tests/minecraft.py;
// 26.2/26.3 checked against the checksum-pinned Mojang GameProtocols and
// ClientboundPlayerChatPacket classes (global index, holder, NBT component).
fn player_chat(number: i32, generation: u8) -> Packet {
    let mut data = Vec::new();
    if number >= 770 {
        write_varint(130, &mut data); // global chat index
    }
    data.extend([7; 16]); // verified sender UUID
    write_varint(i32::from(generation), &mut data);
    data.push(1); // signature present
    data.extend([generation; 256]);
    write_string("Authenticated wire fixture", &mut data);
    data.extend(1_800_000_000_000_i64.to_be_bytes());
    data.extend(i64::from(generation).to_be_bytes());
    data.extend([0, 0, 0]); // no preceding messages, unsigned decoration, filtering
    data.push(u8::from(number >= 767)); // first chat type: holder ID since 1.21
    if number >= 765 {
        data.extend([8, 0, 6]); // anonymous NBT string component
        data.extend(b"Player");
    } else {
        write_string(r#"{"text":"Player"}"#, &mut data);
    }
    data.push(0); // optional target name absent
    let id = match number {
        761 => 0x31,
        762..=763 => 0x35,
        764..=765 => 0x37,
        766..=767 => 0x39,
        768..=769 => 0x3b,
        770..=772 => 0x3a,
        773..=774 => 0x3f,
        775..=776 => 0x41,
        777 => 0x42,
        _ => panic!("unmapped player-chat fixture protocol"),
    };
    Packet::new(id, data)
}

fn signed_command(number: i32, signatures: bool) -> Packet {
    let mut data = Vec::new();
    write_string("server survival", &mut data);
    data.extend(1_800_000_000_000_i64.to_be_bytes());
    data.extend(0x1234_5678_i64.to_be_bytes());
    data.push(u8::from(signatures));
    if signatures {
        write_string("message", &mut data);
        data.extend([0xa5; 256]);
    }
    write_varint(130, &mut data); // multi-byte offset must be acknowledged intact
    data.extend([0x55, 0xaa, 0x05]);
    if number >= 770 {
        data.push(0x35);
    }
    Packet::new(signed_command_id(number), data)
}

fn acknowledgement(number: i32) -> Packet {
    let mut data = Vec::new();
    write_varint(130, &mut data);
    Packet::new(
        if number < 766 {
            3
        } else {
            signed_command_id(number) - 2
        },
        data,
    )
}

async fn serverbound(
    session: &mut EncryptedSession,
    client: &mut EncryptedClient,
    backend: &mut DuplexStream,
    packet: &Packet,
) {
    session.client.codec.write(client, packet).await.unwrap();
    assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
    assert_eq!(
        receive(backend, session.backend().unwrap().codec).await,
        *packet
    );
}

async fn clientbound(
    session: &mut EncryptedSession,
    client: &mut EncryptedClient,
    backend: &mut DuplexStream,
    packet: &Packet,
) {
    session
        .backend()
        .unwrap()
        .codec
        .write(backend, packet)
        .await
        .unwrap();
    assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
    assert_eq!(receive(client, session.client.codec).await, *packet);
}

async fn compression(
    session: &mut EncryptedSession,
    client: &mut EncryptedClient,
    backend: &mut DuplexStream,
    threshold: i32,
) {
    let mut data = Vec::new();
    write_varint(threshold, &mut data);
    let packet = Packet::new(3, data);
    Codec::default().write(backend, &packet).await.unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(client, Codec::default()).await, packet);
}

#[tokio::test]
async fn encrypted_signed_chat_session_and_command_matrix() {
    for number in 761..=777 {
        timeout(Duration::from_secs(5), async {
            let (mut session, mut client, mut backend) = authenticated_session_for(number).await;
            // Exercises both compressed signatures and uncompressed short control packets.
            compression(&mut session, &mut client, &mut backend, 256).await;
            verified_play(&mut session, &mut client, &mut backend).await;
            assert!(session.client.io.is_encrypted());
            assert_eq!(session.identity().unwrap().uuid, Some([7; 16]));
            for packet in [
                chat_session(number, 1),
                signed_chat(number, 1),
                signed_command(number, true),
                acknowledgement(number),
            ] {
                serverbound(&mut session, &mut client, &mut backend, &packet).await;
            }
            clientbound(
                &mut session,
                &mut client,
                &mut backend,
                &player_chat(number, 1),
            )
            .await;
            // A proxy command without signed arguments must preserve the backend's
            // acknowledgement offset without forwarding the consumed command.
            session
                .client
                .codec
                .write(&mut client, &signed_command(number, false))
                .await
                .unwrap();
            assert_eq!(
                session.forward().await.unwrap(),
                SessionEvent::ProxyCommand("server survival".into())
            );
            assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            assert_eq!(
                receive(&mut backend, session.backend().unwrap().codec).await,
                acknowledgement(number)
            );
            // A following frame proves no duplicate command/ack was queued.
            serverbound(
                &mut session,
                &mut client,
                &mut backend,
                &signed_chat(number, 2),
            )
            .await;
        })
        .await
        .unwrap_or_else(|_| panic!("authenticated signed-chat protocol {number} timed out"));
    }
}

fn pack_push(number: i32, generation: u8, required: bool) -> Packet {
    let mut data = if number >= 765 {
        vec![generation; 16]
    } else {
        Vec::new()
    };
    write_string(
        &format!("https://packs.invalid/fixture-{generation}.zip"),
        &mut data,
    );
    write_string("0123456789abcdef0123456789abcdef01234567", &mut data);
    data.extend([u8::from(required), 0]); // required; optional prompt absent
    Packet::new(
        match number {
            764 => 6,
            765 => 7,
            _ => 9,
        },
        data,
    )
}

fn pack_response(number: i32, generation: u8, status: u8) -> Packet {
    let mut data = if number >= 765 {
        vec![generation; 16]
    } else {
        Vec::new()
    };
    data.push(status);
    Packet::new(if number < 766 { 5 } else { 6 }, data)
}

async fn configuration(number: i32) -> (EncryptedSession, EncryptedClient, DuplexStream) {
    let (mut session, mut client, mut backend) = authenticated_session_for(number).await;
    compression(&mut session, &mut client, &mut backend, 256).await;
    let codec = session.client.codec;
    codec
        .write(&mut backend, &velocity_request(i32::MAX))
        .await
        .unwrap();
    session.forward().await.unwrap();
    assert!(validate_velocity_response(
        &receive(&mut backend, codec).await,
        i32::MAX,
        b"shared-secret"
    ));
    let success = verified_login_success(number);
    codec.write(&mut backend, &success).await.unwrap();
    session.forward().await.unwrap();
    let received = receive(&mut client, codec).await;
    // Profile properties come only from the fixture session service, while
    // strict-error handling and 26.2+ session metadata belong to the backend.
    assert_verified_login_success(number, &received);
    serverbound(&mut session, &mut client, &mut backend, &Packet::empty(3)).await;
    assert!(session.client.state.settled(State::Configuration));
    (session, client, backend)
}

async fn finish_configuration(
    number: i32,
    session: &mut EncryptedSession,
    client: &mut EncryptedClient,
    backend: &mut DuplexStream,
    entity: i32,
) {
    let finish = Packet::empty(if number < 766 { 2 } else { 3 });
    clientbound(session, client, backend, &finish).await;
    serverbound(session, client, backend, &finish).await;
    clientbound(
        session,
        client,
        backend,
        &verified_join_game(number, entity),
    )
    .await;
    assert!(session.can_switch());
}

fn start_configuration(number: i32) -> i32 {
    match number {
        764 => 0x65,
        765 => 0x67,
        766..=767 => 0x69,
        768..=769 => 0x70,
        770..=772 => 0x6f,
        773..=774 => 0x74,
        775..=776 => 0x76,
        777 => 0x78,
        _ => panic!("unmapped configuration fixture protocol"),
    }
}

fn configuration_ack(number: i32) -> i32 {
    match number {
        764..=765 => 0x0b,
        766..=767 => 0x0c,
        768..=770 => 0x0e,
        771..=774 => 0x0f,
        775..=777 => 0x10,
        _ => panic!("unmapped configuration fixture protocol"),
    }
}

#[tokio::test]
async fn encrypted_switch_discards_retired_chat_and_resets_resource_pack_stack_matrix() {
    for number in 764..=777 {
        timeout(Duration::from_secs(5), async {
            let (mut session, mut client, mut first) = configuration(number).await;
            // Two stacked packs exercise pop-all, not a pop of one UUID.
            for generation in 1..=if number == 764 { 1 } else { 2 } {
                clientbound(
                    &mut session,
                    &mut client,
                    &mut first,
                    &pack_push(number, generation, false),
                )
                .await;
                for status in [3, 0] {
                    // accepted, successfully loaded
                    serverbound(
                        &mut session,
                        &mut client,
                        &mut first,
                        &pack_response(number, generation, status),
                    )
                    .await;
                }
            }
            finish_configuration(number, &mut session, &mut client, &mut first, 1).await;
            serverbound(
                &mut session,
                &mut client,
                &mut first,
                &chat_session(number, 1),
            )
            .await;
            serverbound(
                &mut session,
                &mut client,
                &mut first,
                &signed_chat(number, 1),
            )
            .await;
            let (replacement, mut second) = tokio::io::duplex(65536);
            let client_codec = session.client.codec;
            let mut backend_codec = Codec::default();
            backend_codec.set_compression(0);
            let (result, ()) = tokio::join!(session.connect_backend(replacement), async {
                let handshake = receive(&mut second, Codec::default()).await;
                assert_eq!(Handshake::decode(&handshake).unwrap().protocol, number);
                assert_eq!(receive(&mut second, Codec::default()).await, login_start());
                Codec::default()
                    .write(&mut second, &Packet::new(3, vec![0]))
                    .await
                    .unwrap();
                backend_codec
                    .write(&mut second, &velocity_request(i32::MIN))
                    .await
                    .unwrap();
                assert!(validate_velocity_response(
                    &receive(&mut second, backend_codec).await,
                    i32::MIN,
                    b"shared-secret"
                ));
                backend_codec
                    .write(&mut second, &verified_login_success(number))
                    .await
                    .unwrap();
                assert_eq!(receive(&mut second, backend_codec).await, Packet::empty(3));
                assert_eq!(
                    receive(&mut client, client_codec).await,
                    Packet::empty(start_configuration(number))
                );
                // These already-in-flight packets belong to the old message chain.
                for stale in [
                    chat_session(number, 99),
                    signed_chat(number, 99),
                    signed_command(number, true),
                    acknowledgement(number),
                ] {
                    client_codec.write(&mut client, &stale).await.unwrap();
                }
                client_codec
                    .write(&mut client, &Packet::empty(configuration_ack(number)))
                    .await
                    .unwrap();
                if number >= 765 {
                    assert_eq!(
                        receive(&mut client, client_codec).await,
                        Packet::new(if number == 765 { 6 } else { 8 }, vec![0])
                    );
                }
            });
            result.unwrap();
            assert_eq!(session.client.codec.threshold(), Some(256));
            assert_eq!(session.backend().unwrap().codec.threshold(), Some(0));
            assert!(session.client.io.is_encrypted());
            assert_eq!(session.authenticated_profile(), Some(&verified_profile()));
            // Exact next-frame assertions detect any leaked stale chat/session/ack,
            // and place old-pack cleanup strictly before the replacement pack.
            clientbound(
                &mut session,
                &mut client,
                &mut second,
                &pack_push(number, 3, true),
            )
            .await;
            serverbound(
                &mut session,
                &mut client,
                &mut second,
                &pack_response(number, 3, 3),
            )
            .await;
            serverbound(
                &mut session,
                &mut client,
                &mut second,
                &pack_response(number, 3, 0),
            )
            .await;
            finish_configuration(number, &mut session, &mut client, &mut second, 2).await;
            clientbound(
                &mut session,
                &mut client,
                &mut second,
                &player_chat(number, 2),
            )
            .await;
            for packet in [
                chat_session(number, 2),
                signed_chat(number, 2),
                signed_command(number, true),
            ] {
                serverbound(&mut session, &mut client, &mut second, &packet).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("authenticated pack/chat switch protocol {number} timed out"));
    }
}

#[tokio::test]
async fn encrypted_required_pack_rejection_is_authoritative_matrix() {
    for number in [764, 765, 766, 767, 770, 774, 776, 777] {
        for status in [1, 2] {
            // declined, failed download
            timeout(Duration::from_secs(5), async {
                let (mut session, mut client, mut backend) = configuration(number).await;
                clientbound(
                    &mut session,
                    &mut client,
                    &mut backend,
                    &pack_push(number, 1, true),
                )
                .await;
                serverbound(
                    &mut session,
                    &mut client,
                    &mut backend,
                    &pack_response(number, 1, status),
                )
                .await;
                let disconnect = protocol::disconnect(
                    session.version,
                    State::Configuration,
                    "Required resource pack rejected",
                )
                .unwrap();
                session
                    .backend()
                    .unwrap()
                    .codec
                    .write(&mut backend, &disconnect)
                    .await
                    .unwrap();
                assert_eq!(session.forward().await.unwrap(), SessionEvent::Disconnected);
                assert_eq!(receive(&mut client, session.client.codec).await, disconnect);
                assert!(session.client.state.settled(State::Closed));
                assert!(!session.can_switch());
                assert!(!session.reset_initial_backend());
            })
            .await
            .unwrap_or_else(|_| {
                panic!("authenticated pack rejection protocol {number}, status {status} timed out")
            });
        }
    }
}

#[tokio::test]
async fn encrypted_failed_replacement_preserves_resource_packs_and_chat_matrix() {
    for number in [764, 765, 766, 767, 774, 776, 777] {
        for failure in ["missing_forwarding", "changed_uuid", "changed_name"] {
            timeout(Duration::from_secs(5), async {
                let (mut session, mut client, mut first) = configuration(number).await;
                clientbound(&mut session, &mut client, &mut first, &pack_push(number, 1, false)).await;
                serverbound(&mut session, &mut client, &mut first, &pack_response(number, 1, 0)).await;
                finish_configuration(number, &mut session, &mut client, &mut first, 1).await;
                let (replacement, mut second) = tokio::io::duplex(65536);
                let (result, ()) = tokio::join!(session.connect_backend(replacement), async {
                    receive(&mut second, Codec::default()).await;
                    assert_eq!(receive(&mut second, Codec::default()).await, login_start());
                    if failure != "missing_forwarding" {
                        Codec::default().write(&mut second, &velocity_request(-17)).await.unwrap();
                        assert!(validate_velocity_response(&receive(&mut second, Codec::default()).await, -17, b"shared-secret"));
                    }
                    let mut success = verified_login_success(number);
                    match failure {
                        "changed_uuid" => success.data[0] ^= 1,
                        "changed_name" => success.data[17] = b'X',
                        _ => {}
                    }
                    Codec::default().write(&mut second, &success).await.unwrap();
                });
                let error = result.unwrap_err().to_string();
                assert!(error.contains(if failure == "missing_forwarding" { "did not request Velocity" } else { "changed the player identity" }), "{error}");
                assert!(!session.switch_in_progress());
                assert!(session.can_switch());
                assert!(session.client.io.is_encrypted());
                assert_eq!(session.authenticated_profile(), Some(&verified_profile()));
                // No start-configuration or resource-pop may precede this message.
                let message = player_chat(number, 2);
                clientbound(&mut session, &mut client, &mut first, &message).await;
                serverbound(&mut session, &mut client, &mut first, &signed_chat(number, 2)).await;
                serverbound(&mut session, &mut client, &mut first, &chat_session(number, 2)).await;
            }).await.unwrap_or_else(|_| panic!("authenticated failed replacement protocol {number}, failure {failure} timed out"));
        }
    }
}
