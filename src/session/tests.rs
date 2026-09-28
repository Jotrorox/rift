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
    session_with_backend(|stream| stream).await
}

async fn session_with_backend<B: AsyncRead + AsyncWrite + Unpin>(
    wrap: impl FnOnce(DuplexStream) -> B,
) -> (Session<DuplexStream, B>, DuplexStream, DuplexStream) {
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
    session.connect_backend(wrap(server)).await.unwrap();
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
async fn play<B: AsyncRead + AsyncWrite + Unpin>(
    session: &mut Session<DuplexStream, B>,
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
    let join = join_game(1);
    codec.write(backend, &join).await.unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(client, codec).await, join);
}

fn join_game(entity: i32) -> Packet {
    let mut data = entity.to_be_bytes().to_vec();
    data.push(0); // hardcore
    data.push(1);
    write_string("minecraft:overworld", &mut data);
    data.extend([20, 8, 8, 0, 1, 0, 0]); // limits, flags, dimension type
    write_string("minecraft:overworld", &mut data);
    data.extend([0; 8]); // seed
    data.extend([0, 255, 0, 0, 0, 0, 63, 0]); // game modes, flags, death, cooldown, sea level, secure
    Packet::new(0x30, data)
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
            assert_eq!(
                receive(&mut client, client_codec).await,
                Packet::empty(0x74)
            );
            client_codec
                .write(&mut client, &Packet::empty(0x0f))
                .await
                .unwrap();
            assert_eq!(
                receive(&mut client, client_codec).await,
                Packet::new(8, vec![0])
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
async fn replacement_preflight_failure_preserves_the_old_backend_and_world() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut backend) = session().await;
        play(&mut session, &mut client, &mut backend, Codec::default()).await;
        let (replacement, mut second) = tokio::io::duplex(65536);
        let operation = session.connect_backend(replacement);
        let peers = async {
            receive(&mut second, Codec::default()).await;
            receive(&mut second, Codec::default()).await;
            Codec::default()
                .write(&mut second, &Packet::empty(1))
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(operation, peers);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert!(session.client.state.settled(State::Play));
        assert!(session.backend().unwrap().state.settled(State::Play));
        assert!(!session.switch_in_progress());
        Codec::default()
            .write(&mut backend, &Packet::new(0x7e, vec![42]))
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(
            receive(&mut client, Codec::default()).await,
            Packet::new(0x7e, vec![42])
        );
        session
            .disconnect("Unable to join replacement")
            .await
            .unwrap();
        assert_eq!(
            receive(&mut client, Codec::default()).await,
            protocol::disconnect(session.version, State::Play, "Unable to join replacement")
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

#[tokio::test]
async fn replacement_login_ban_does_not_transition_or_detach_player() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut backend) = session().await;
        session.enable_network().unwrap();
        play(&mut session, &mut client, &mut backend, Codec::default()).await;
        let (replacement, mut second) = tokio::io::duplex(65536);
        let (result, ()) = tokio::join!(session.connect_backend(replacement), async {
            receive(&mut second, Codec::default()).await;
            receive(&mut second, Codec::default()).await;
            let rejection = protocol::disconnect(
                Some(ProtocolVersion::new(774).unwrap()),
                State::Login,
                "Banned from survival",
            )
            .unwrap();
            Codec::default()
                .write(&mut second, &rejection)
                .await
                .unwrap();
        });
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("Banned from survival"));
        assert!(session.can_switch());
        assert!(!session.switch_in_progress());
        assert!(
            timeout(
                Duration::from_millis(10),
                receive(&mut client, Codec::default())
            )
            .await
            .is_err()
        );
        Codec::default()
            .write(&mut backend, &Packet::new(0x7f, vec![42]))
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(
            receive(&mut client, Codec::default()).await,
            Packet::new(0x7f, vec![42])
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn replacement_timeout_keeps_forwarding_old_backend_and_can_resume() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut backend) = session().await;
        play(&mut session, &mut client, &mut backend, Codec::default()).await;
        let (replacement, mut second) = tokio::io::duplex(65536);
        let (result, ()) = tokio::join!(
            timeout(
                Duration::from_millis(40),
                session.connect_backend(replacement)
            ),
            async {
                receive(&mut second, Codec::default()).await;
                receive(&mut second, Codec::default()).await;
                let keepalive = Packet::new(0x2b, 123_i64.to_be_bytes().to_vec());
                Codec::default()
                    .write(&mut backend, &keepalive)
                    .await
                    .unwrap();
                assert_eq!(receive(&mut client, Codec::default()).await, keepalive);
            }
        );
        assert!(result.is_err());
        assert!(session.can_switch());
        assert!(session.backend().is_some());
        assert!(!session.switch_in_progress());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn current_backend_ban_during_preflight_is_terminal() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut backend) = session().await;
        play(&mut session, &mut client, &mut backend, Codec::default()).await;
        let (replacement, mut second) = tokio::io::duplex(65536);
        let banned = protocol::disconnect(session.version, State::Play, "Banned").unwrap();
        let (result, ()) = tokio::join!(session.connect_backend(replacement), async {
            receive(&mut second, Codec::default()).await;
            receive(&mut second, Codec::default()).await;
            Codec::default().write(&mut backend, &banned).await.unwrap();
            assert_eq!(receive(&mut client, Codec::default()).await, banned);
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(session.client.state.settled(State::Closed));
        assert!(!session.can_switch());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn commands_are_intercepted_only_in_network_mode_without_signed_arguments() {
    let (mut session, mut client, mut backend) = session().await;
    session.enable_network().unwrap();
    play(&mut session, &mut client, &mut backend, Codec::default()).await;
    let mut command = Vec::new();
    write_string("server survival", &mut command);
    Codec::default()
        .write(&mut client, &Packet::new(6, command.clone()))
        .await
        .unwrap();
    assert_eq!(
        session.forward().await.unwrap(),
        SessionEvent::ProxyCommand("server survival".into())
    );
    assert!(
        timeout(
            Duration::from_millis(10),
            receive(&mut backend, Codec::default())
        )
        .await
        .is_err()
    );
    command.extend([0; 16]);
    command.push(1);
    write_string("message", &mut command);
    command.extend([42; 256]);
    command.extend([0; 5]);
    let signed = Packet::new(7, command);
    Codec::default().write(&mut client, &signed).await.unwrap();
    assert_eq!(session.forward().await.unwrap(), SessionEvent::Packet);
    assert_eq!(receive(&mut backend, Codec::default()).await, signed);
}

#[tokio::test]
async fn initial_retry_is_disabled_after_backend_login_plugin_exchange() {
    let (mut session, mut client, mut backend) = session().await;
    let request = Packet::new(4, vec![1, 0]);
    Codec::default()
        .write(&mut backend, &request)
        .await
        .unwrap();
    session.forward().await.unwrap();
    assert_eq!(receive(&mut client, Codec::default()).await, request);
    drop(backend);
    assert_eq!(
        session.forward().await.unwrap(),
        SessionEvent::BackendClosed
    );
    assert!(!session.reset_initial_backend());
}

// Make the old backend's queued packet readable precisely when replacement
// Login Acknowledged is written. Whichever select branch is polled first, login
// completes before the old packet can be observed in that select. This makes
// the cutover race deterministic instead of depending on Tokio's random order.
struct CutoverStream {
    io: DuplexStream,
    read_gate: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    open_gate_on_login_ack: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}
impl AsyncRead for CutoverStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self
            .read_gate
            .as_ref()
            .is_some_and(|gate| !gate.load(std::sync::atomic::Ordering::SeqCst))
        {
            return Poll::Pending;
        }
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}
impl AsyncWrite for CutoverStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.io).poll_write(cx, buf);
        if buf == [1, 3]
            && matches!(result, Poll::Ready(Ok(2)))
            && let Some(gate) = &self.open_gate_on_login_ack
        {
            gate.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        result
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn ready_replacement_cannot_win_over_a_buffered_current_backend_ban() {
    timeout(Duration::from_secs(3), async {
        let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (mut session, mut client, mut old) = session_with_backend(|io| CutoverStream {
            io,
            read_gate: Some(gate.clone()),
            open_gate_on_login_ack: None,
        })
        .await;
        play(&mut session, &mut client, &mut old, Codec::default()).await;
        gate.store(false, std::sync::atomic::Ordering::SeqCst);
        let banned =
            protocol::disconnect(session.version, State::Play, "Banned before cutover").unwrap();
        Codec::default().write(&mut old, &banned).await.unwrap();
        let (replacement, mut second) = tokio::io::duplex(65536);
        Codec::default()
            .write(&mut second, &login_success())
            .await
            .unwrap();
        let replacement = CutoverStream {
            io: replacement,
            read_gate: None,
            open_gate_on_login_ack: Some(gate),
        };
        let (result, received) = tokio::join!(session.connect_backend(replacement), async {
            let first = receive(&mut client, Codec::default()).await;
            if first.id == 0x74 {
                // Let the broken implementation complete, so the assertion
                // identifies ban bypass rather than a generic test timeout.
                Codec::default()
                    .write(&mut client, &Packet::empty(0x0f))
                    .await
                    .unwrap();
                receive(&mut client, Codec::default()).await;
            }
            first
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(received, banned);
        assert!(!session.switch_in_progress());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn old_backend_eof_at_cutover_does_not_prevent_a_ready_replacement() {
    timeout(Duration::from_secs(3), async {
        let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (mut session, mut client, mut old) = session_with_backend(|io| CutoverStream {
            io,
            read_gate: Some(gate.clone()),
            open_gate_on_login_ack: None,
        })
        .await;
        play(&mut session, &mut client, &mut old, Codec::default()).await;
        gate.store(false, std::sync::atomic::Ordering::SeqCst);
        drop(old);
        let (replacement, mut second) = tokio::io::duplex(65536);
        Codec::default()
            .write(&mut second, &login_success())
            .await
            .unwrap();
        let replacement = CutoverStream {
            io: replacement,
            read_gate: None,
            open_gate_on_login_ack: Some(gate),
        };
        let (result, ()) = tokio::join!(session.connect_backend(replacement), async {
            assert_eq!(
                receive(&mut client, Codec::default()).await,
                Packet::empty(0x74)
            );
            Codec::default()
                .write(&mut client, &Packet::empty(0x0f))
                .await
                .unwrap();
            assert_eq!(
                receive(&mut client, Codec::default()).await,
                Packet::new(8, vec![0])
            );
        });
        result.unwrap();
        assert!(session.backend().is_some());
        assert!(session.client.state.settled(State::Configuration));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn busy_old_backend_bounds_cutover_work_and_remains_attached() {
    let (mut session, mut client, mut old) = session().await;
    play(&mut session, &mut client, &mut old, Codec::default()).await;
    // More than one cooperative budget of packets must not be mistaken for
    // quiescence, nor drained forever. Every byte fits in the duplex buffers.
    for _ in 0..300 {
        Codec::default()
            .write(&mut old, &Packet::empty(0x7f))
            .await
            .unwrap();
    }
    let error = timeout(Duration::from_secs(1), session.drain_before_switch())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert!(session.can_switch());
    assert!(session.backend().is_some());
    assert!(!session.switch_in_progress());
    assert_eq!(
        receive(&mut client, Codec::default()).await,
        Packet::empty(0x7f)
    );
}

#[tokio::test]
async fn partial_backend_packet_is_not_a_quiescent_cutover() {
    let (mut session, mut client, mut old) = session().await;
    play(&mut session, &mut client, &mut old, Codec::default()).await;
    let banned = protocol::disconnect(session.version, State::Play, "Fragmented ban").unwrap();
    let frame = Codec::default().encode(&banned).unwrap();
    old.write_all(&frame[..2]).await.unwrap();
    assert_eq!(
        session.drain_before_switch().await.unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(session.can_switch());
    old.write_all(&frame[2..]).await.unwrap();
    assert_eq!(session.forward().await.unwrap(), SessionEvent::Disconnected);
    assert_eq!(receive(&mut client, Codec::default()).await, banned);
}

#[tokio::test]
async fn backpressured_old_frame_does_not_hide_the_ban_behind_it() {
    timeout(Duration::from_secs(3), async {
        let (mut session, mut client, mut old) = session().await;
        play(&mut session, &mut client, &mut old, Codec::default()).await;
        let frame = Packet::new(0x7f, vec![42; 262144]);
        let banned = protocol::disconnect(
            session.version,
            State::Play,
            "Banned behind a pending frame",
        )
        .unwrap();
        let ((), blocked) = tokio::join!(
            async {
                Codec::default().write(&mut old, &frame).await.unwrap();
                Codec::default().write(&mut old, &banned).await.unwrap();
            },
            timeout(Duration::from_millis(10), session.forward())
        );
        assert!(blocked.is_err());
        assert!(
            session
                .client
                .pending
                .as_ref()
                .is_some_and(|write| write.offset < write.frame.len())
        );
        let (result, ()) = tokio::join!(session.drain_before_switch(), async {
            assert_eq!(receive(&mut client, Codec::default()).await, frame);
            assert_eq!(receive(&mut client, Codec::default()).await, banned);
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(!session.switch_in_progress());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn initial_backend_write_failure_retries_with_the_cached_login_start() {
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
    Codec::default()
        .write(&mut client, &login_start())
        .await
        .unwrap();
    let mut session = Session::accept(input).await.unwrap();
    assert_eq!(session.read_login_start().await.unwrap().name, "Player");
    let (failed, closed) = tokio::io::duplex(65536);
    drop(closed);
    assert_eq!(
        session.connect_backend(failed).await.unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert!(session.reset_initial_backend());
    assert!(session.client.state.settled(State::Login));
    assert!(session.identity().is_none());
    let (next, mut peer) = tokio::io::duplex(65536);
    session.connect_backend(next).await.unwrap();
    assert_eq!(
        receive(&mut peer, Codec::default()).await,
        handshake.packet()
    );
    assert_eq!(receive(&mut peer, Codec::default()).await, login_start());
    play(&mut session, &mut client, &mut peer, Codec::default()).await;
    assert!(session.can_switch());
}

#[tokio::test]
async fn ordinary_relay_forwards_opaque_join_but_does_not_make_it_transfer_ready() {
    let (mut session, mut client, mut backend) = session().await;
    play(&mut session, &mut client, &mut backend, Codec::default()).await;
    assert!(session.can_switch());
    let mut secure_join = join_game(21);
    *secure_join.data.last_mut().unwrap() = 1;
    for opaque_join in [Packet::new(0x30, vec![42]), secure_join] {
        Codec::default()
            .write(&mut backend, &opaque_join)
            .await
            .unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, Codec::default()).await, opaque_join);
        assert!(!session.can_switch());
        let join = join_game(22);
        Codec::default().write(&mut backend, &join).await.unwrap();
        session.forward().await.unwrap();
        assert_eq!(receive(&mut client, Codec::default()).await, join);
        assert!(session.can_switch());
    }
}
