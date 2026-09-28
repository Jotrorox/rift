use super::*;
use crate::protocol::{read_string, write_string};
use tokio::{
    io::DuplexStream,
    time::{Duration, timeout},
};

fn login_start() -> Packet {
    let mut data = Vec::new();
    write_string("Player", &mut data);
    data.extend([7; 16]);
    Packet::new(0, data)
}
fn login_success() -> Packet {
    let mut data = vec![7; 16];
    write_string("Player", &mut data);
    data.push(0);
    Packet::new(2, data)
}
async fn receive(stream: &mut DuplexStream, codec: Codec) -> Packet {
    Reader::default()
        .read(stream, codec)
        .await
        .unwrap()
        .unwrap()
}
async fn session() -> (
    Session<DuplexStream, DuplexStream>,
    DuplexStream,
    DuplexStream,
) {
    let (mut client, input) = tokio::io::duplex(65536);
    let handshake = Handshake {
        protocol: 774,
        address: "play.test".into(),
        port: 25565,
        next_state: NextState::Login,
    };
    Codec::default()
        .write(&mut client, &handshake.packet())
        .await
        .unwrap();
    let mut session = Session::accept(input).await.unwrap();
    let (server, mut backend) = tokio::io::duplex(65536);
    session.connect_backend(server).await.unwrap();
    assert_eq!(
        receive(&mut backend, Codec::default()).await,
        handshake.packet()
    );
    Codec::default()
        .write(&mut client, &login_start())
        .await
        .unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(&mut backend, Codec::default()).await, login_start());
    (session, client, backend)
}
async fn play(
    session: &mut Session<DuplexStream, DuplexStream>,
    client: &mut DuplexStream,
    backend: &mut DuplexStream,
    codec: Codec,
) {
    codec.write(backend, &login_success()).await.unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(client, codec).await, login_success());
    codec.write(client, &Packet::empty(3)).await.unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(backend, codec).await, Packet::empty(3));
    codec.write(backend, &Packet::empty(3)).await.unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(client, codec).await, Packet::empty(3));
    codec.write(client, &Packet::empty(3)).await.unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(backend, codec).await, Packet::empty(3));
    assert!(session.client.state.settled(State::Play));
}

#[tokio::test]
async fn cancelled_forward_resumes_partial_writes_without_duplicating_bytes() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut backend) = session().await;
        play(&mut session, &mut client, &mut backend, Codec::default()).await;
        let packet = Packet::new(0x7f, vec![42; 262144]);
        let ((), stalled) = tokio::join!(
            async {
                Codec::default().write(&mut client, &packet).await.unwrap();
            },
            timeout(Duration::from_millis(20), session.forward()),
        );
        assert!(stalled.is_err());
        let pending = session.backend.as_ref().unwrap().pending.as_ref().unwrap();
        assert!(pending.offset > 0 && pending.offset < pending.frame.len());
        let (event, received) =
            tokio::join!(session.forward(), receive(&mut backend, Codec::default()),);
        assert_eq!(event.unwrap(), SessionEvent::Packet);
        assert_eq!(received, packet);
        // Verify the following frame still starts at the correct byte.
        Codec::default()
            .write(&mut client, &Packet::empty(0x7e))
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(
            receive(&mut backend, Codec::default()).await,
            Packet::empty(0x7e)
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn echo_backpressure_does_not_block_the_opposite_direction() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut backend) = session().await;
        play(&mut session, &mut client, &mut backend, Codec::default()).await;
        let packet = Packet::new(0x7f, vec![17; 262144]);
        let relay = async {
            for _ in 0..64 {
                assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
            }
        };
        let (mut read, mut write) = tokio::io::split(&mut client);
        let send = async {
            for _ in 0..32 {
                Codec::default().write(&mut write, &packet).await.unwrap();
            }
        };
        let receive = async {
            let mut reader = Reader::default();
            for _ in 0..32 {
                assert_eq!(
                    reader.read(&mut read, Codec::default()).await.unwrap(),
                    Some(packet.clone())
                );
            }
        };
        let backend_peer = async {
            let mut reader = Reader::default();
            for _ in 0..32 {
                let echo = reader
                    .read(&mut backend, Codec::default())
                    .await
                    .unwrap()
                    .unwrap();
                Codec::default().write(&mut backend, &echo).await.unwrap();
            }
        };
        tokio::join!(relay, send, receive, backend_peer);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn compressed_login_configuration_play_and_backend_loss_keep_client_owned() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut backend) = session().await;
        let compression = Packet::new(3, vec![0]);
        Codec::default()
            .write(&mut backend, &compression)
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, Codec::default()).await, compression);
        let mut codec = Codec::default();
        codec.set_compression(0);
        play(&mut session, &mut client, &mut backend, codec).await;
        let packet = Packet::new(0x7f, vec![42; 2000]);
        codec.write(&mut backend, &packet).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, codec).await, packet);
        drop(backend);
        assert_eq!(
            session.forward().await.unwrap(),
            SessionEvent::BackendClosed
        );
        assert!(session.backend().is_none());
        assert!(session.client.state.settled(State::Play));
        session.disconnect("Backend unavailable").await.unwrap();
        assert_eq!(
            receive(&mut client, codec).await,
            protocol::disconnect(session.version, State::Play, "Backend unavailable").unwrap()
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn backend_switch_relogs_backend_and_retains_client_socket_and_compression() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut backend) = session().await;
        Codec::default()
            .write(&mut backend, &Packet::new(3, vec![0]))
            .await
            .unwrap();
        session.forward().await.unwrap();
        receive(&mut client, Codec::default()).await;
        let mut client_codec = Codec::default();
        client_codec.set_compression(0);
        play(&mut session, &mut client, &mut backend, client_codec).await;
        let (replacement, mut second) = tokio::io::duplex(65536);
        let switch = session.connect_backend(replacement);
        let peers = async {
            assert_eq!(
                receive(&mut client, client_codec).await,
                Packet::empty(0x74)
            );
            client_codec
                .write(&mut client, &Packet::empty(0x0f))
                .await
                .unwrap();
            let handshake = receive(&mut second, Codec::default()).await;
            assert_eq!(
                Handshake::decode(&handshake).unwrap().next_state,
                NextState::Login
            );
            assert_eq!(receive(&mut second, Codec::default()).await, login_start());
            Codec::default()
                .write(&mut second, &login_success())
                .await
                .unwrap();
            assert_eq!(
                receive(&mut second, Codec::default()).await,
                Packet::empty(3)
            );
        };
        let (result, ()) = tokio::join!(switch, peers);
        result.unwrap();
        assert_eq!(session.client.codec.threshold(), Some(0));
        assert_eq!(session.backend().unwrap().codec.threshold(), None);
        assert!(session.client.state.settled(State::Configuration));
        assert!(
            session
                .backend()
                .unwrap()
                .state
                .settled(State::Configuration)
        );
        // The client receives configuration, never a second login success.
        Codec::default()
            .write(&mut second, &Packet::new(7, vec![42; 100]))
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(
            receive(&mut client, client_codec).await,
            Packet::new(7, vec![42; 100])
        );
        client_codec
            .write(&mut client, &Packet::new(2, vec![7; 100]))
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(
            receive(&mut second, Codec::default()).await,
            Packet::new(2, vec![7; 100])
        );
        assert_eq!(
            Reader::default()
                .read(&mut backend, client_codec)
                .await
                .unwrap(),
            None
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn encrypted_backend_is_rejected_before_the_client_enters_encryption() {
    let (mut session, mut client, mut backend) = session().await;
    Codec::default()
        .write(&mut backend, &Packet::empty(1))
        .await
        .unwrap();
    let error = session.forward().await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    session.disconnect(&error.to_string()).await.unwrap();
    let packet = receive(&mut client, Codec::default()).await;
    assert_eq!(packet.id, 0);
    assert!(
        read_string(&mut packet.data.as_slice(), 32767)
            .unwrap()
            .contains("offline-mode")
    );
}

#[tokio::test]
async fn pipelined_compression_and_login_success_use_the_new_codec() {
    let (mut session, mut client, mut backend) = session().await;
    let compression = Packet::new(3, vec![0]);
    let mut codec = Codec::default();
    codec.set_compression(0);
    let mut frames = Codec::default().encode(&compression).unwrap();
    frames.extend(codec.encode(&login_success()).unwrap());
    backend.write_all(&frames).await.unwrap();
    session.forward().await.unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(&mut client, Codec::default()).await, compression);
    assert_eq!(receive(&mut client, codec).await, login_success());
}

#[tokio::test]
async fn replacement_failure_can_disconnect_in_the_clients_configuration_state() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut backend) = session().await;
        play(&mut session, &mut client, &mut backend, Codec::default()).await;
        let (replacement, mut second) = tokio::io::duplex(65536);
        let operation = session.connect_backend(replacement);
        let peers = async {
            assert_eq!(receive(&mut client, Codec::default()).await.id, 0x74);
            Codec::default()
                .write(&mut client, &Packet::empty(0x0f))
                .await
                .unwrap();
            receive(&mut second, Codec::default()).await;
            receive(&mut second, Codec::default()).await;
            Codec::default()
                .write(&mut second, &Packet::empty(1))
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(operation, peers);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert!(session.client.state.settled(State::Configuration));
        assert!(session.backend().unwrap().state.settled(State::Login));
        session
            .disconnect("Unable to join replacement")
            .await
            .unwrap();
        assert_eq!(
            receive(&mut client, Codec::default()).await,
            protocol::disconnect(
                session.version,
                State::Configuration,
                "Unable to join replacement"
            )
            .unwrap()
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn simultaneous_large_packets_and_slow_reads_preserve_both_directions() {
    timeout(Duration::from_secs(10), async {
        let (mut session, mut client, mut backend) = session().await;
        session.set_read_chunk_size(997);
        play(&mut session, &mut client, &mut backend, Codec::default()).await;
        let proxy = tokio::spawn(async move {
            loop {
                match session.forward().await.unwrap() {
                    SessionEvent::Packet | SessionEvent::ClientClosed => {}
                    SessionEvent::BackendClosed => break,
                    other => panic!("unexpected session event: {other:?}"),
                }
            }
        });
        let (mut cr, mut cw) = tokio::io::split(client);
        let (mut br, mut bw) = tokio::io::split(backend);
        let upload = Packet::new(0x7f, (0..131077).map(|n| (n % 251) as u8).collect());
        let download = Packet::new(0x7e, (0..131083).map(|n| (n % 239) as u8).collect());
        tokio::join!(
            async {
                for _ in 0..20 {
                    Codec::default().write(&mut cw, &upload).await.unwrap();
                }
                cw.shutdown().await.unwrap();
            },
            async {
                for _ in 0..20 {
                    Codec::default().write(&mut bw, &download).await.unwrap();
                }
            },
            async {
                let mut reader = Reader::new(509);
                for _ in 0..20 {
                    assert_eq!(
                        reader
                            .read(&mut br, Codec::default())
                            .await
                            .unwrap()
                            .unwrap(),
                        upload
                    );
                    tokio::task::yield_now().await;
                }
            },
            async {
                let mut reader = Reader::new(503);
                for _ in 0..20 {
                    assert_eq!(
                        reader
                            .read(&mut cr, Codec::default())
                            .await
                            .unwrap()
                            .unwrap(),
                        download
                    );
                    tokio::task::yield_now().await;
                }
            },
        );
        bw.shutdown().await.unwrap();
        proxy.await.unwrap();
    })
    .await
    .unwrap();
}
