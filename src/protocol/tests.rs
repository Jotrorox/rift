use super::*;
use tokio::io::AsyncWriteExt;

#[test]
fn varints_cover_signed_values_and_reject_overflows() {
    for value in [0, 127, 128, 255, i32::MAX, i32::MIN, -1] {
        let mut bytes = Vec::new();
        write_varint(value, &mut bytes);
        let mut slice = bytes.as_slice();
        assert_eq!(read_varint(&mut slice).unwrap(), value);
        assert!(slice.is_empty());
        for cut in 0..bytes.len() {
            assert!(read_varint(&mut &bytes[..cut]).is_err());
        }
    }
    for mut bytes in [&[0xff, 0xff, 0xff, 0xff, 0x10][..], &[0x80; 5]] {
        assert!(read_varint(&mut bytes).is_err());
    }
}

#[tokio::test]
async fn framing_is_fragmented_cancel_safe_and_does_not_consume_the_next_packet() {
    let packet = Packet::new(0x80, vec![42; 260]);
    let frame = Codec::default().encode(&packet).unwrap();
    let (mut writer, mut input) = tokio::io::duplex(1024);
    let mut reader = Reader::default();
    for &byte in &frame[..frame.len() - 1] {
        writer.write_all(&[byte]).await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(1),
                reader.read(&mut input, Codec::default())
            )
            .await
            .is_err()
        );
    }
    writer.write_all(&frame[frame.len() - 1..]).await.unwrap();
    writer.write_all(&[1, 0]).await.unwrap();
    assert_eq!(
        reader.read(&mut input, Codec::default()).await.unwrap(),
        Some(packet)
    );
    assert_eq!(
        reader.read(&mut input, Codec::default()).await.unwrap(),
        Some(Packet::empty(0))
    );
    writer.shutdown().await.unwrap();
    assert_eq!(
        reader.read(&mut input, Codec::default()).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn framing_bounds_lengths_and_distinguishes_clean_eof_from_truncation() {
    for bytes in [
        vec![0],
        vec![0x80; 3],
        vec![0xff; 5],
        vec![0x80],
        vec![3, 0, 1],
    ] {
        assert!(
            Reader::default()
                .read(&mut bytes.as_slice(), Codec::default())
                .await
                .is_err()
        );
    }
    let mut bytes = Vec::new();
    write_varint(2049, &mut bytes);
    assert_eq!(
        Reader::default()
            .read_frame(&mut bytes.as_slice(), 2048)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn zlib_decodes_independent_stored_fixed_and_dynamic_streams() {
    let raw: Vec<u8> = b"Minecraft compression fixture: "
        .iter()
        .copied()
        .chain(0..=255)
        .collect::<Vec<_>>()
        .repeat(400);
    for bytes in [
        &include_bytes!("../../tests/fixtures/fixed.zlib")[..],
        &include_bytes!("../../tests/fixtures/dynamic.zlib")[..],
    ] {
        assert_eq!(compression::inflate(bytes, raw.len()).unwrap(), raw);
        assert!(compression::inflate(bytes, raw.len() - 1).is_err());
        assert!(compression::inflate(bytes, raw.len() + 1).is_err());
        let mut bad = bytes.to_vec();
        *bad.last_mut().unwrap() ^= 1;
        assert!(compression::inflate(&bad, raw.len()).is_err());
        let mut trailing = bytes.to_vec();
        trailing.insert(trailing.len() - 4, 0);
        assert!(compression::inflate(&trailing, raw.len()).is_err());
        for cut in [0, 1, 2, 5, bytes.len() - 1] {
            assert!(compression::inflate(&bytes[..cut], raw.len()).is_err());
        }
    }
    assert_eq!(
        compression::inflate(include_bytes!("../../tests/fixtures/stored.zlib"), 256).unwrap(),
        (0..=255).collect::<Vec<u8>>()
    );
    let encoded = compression::deflate(&raw);
    assert!(encoded.len() < raw.len() / 10);
    assert_eq!(compression::inflate(&encoded, raw.len()).unwrap(), raw);
}

#[tokio::test]
async fn compression_thresholds_and_declared_lengths_are_enforced() {
    for threshold in [-1, 0, 1, 32, 1024] {
        let mut codec = Codec::default();
        codec.set_compression(threshold);
        for size in [0, 1, 30, 31, 32, 4096] {
            let packet = Packet::new(0, vec![42; size]);
            let encoded = codec.encode(&packet).unwrap();
            assert_eq!(
                Reader::default()
                    .read(&mut encoded.as_slice(), codec)
                    .await
                    .unwrap(),
                Some(packet)
            );
        }
    }
    let mut codec = Codec::default();
    codec.set_compression(10);
    assert!(codec.decode(&[0; 11]).is_err());
    let mut bad = Vec::new();
    write_varint(MAX_PACKET_SIZE as i32 + 1, &mut bad);
    assert!(codec.decode(&bad).is_err());
    bad.clear();
    write_varint(-1, &mut bad);
    assert!(codec.decode(&bad).is_err());
    bad.clear();
    write_varint(1, &mut bad);
    bad.extend(compression::deflate(&[0]));
    assert!(codec.decode(&bad).is_err());
}

#[test]
fn handshake_preserves_fields_and_bounds_utf16_address_length() {
    let handshake = Handshake {
        protocol: 774,
        address: "PLAY.Example.COM.\0FML3\0".into(),
        port: 25565,
        next_state: NextState::Transfer,
    };
    assert_eq!(Handshake::decode(&handshake.packet()).unwrap(), handshake);
    assert_eq!(handshake.hostname(), "play.example.com");
    for address in [
        "".into(),
        "\0metadata".into(),
        "a".repeat(256),
        "😀".repeat(128),
    ] {
        let mut bad = handshake.clone();
        bad.address = address;
        assert!(Handshake::decode(&bad.packet()).is_err());
    }
    let mut good = handshake;
    good.address = "é".repeat(255);
    assert!(Handshake::decode(&good.packet()).is_ok());
}

#[test]
fn login_success_validates_session_uuid_without_changing_player_identity() {
    let version = ProtocolVersion::new(777).unwrap();
    let mut identity = vec![1; 16];
    write_string("Player", &mut identity);
    let mut data = identity.clone();
    data.push(0); // No profile properties.
    let session_offset = data.len();
    data.extend_from_slice(&[2; 16]);
    for length in session_offset..data.len() {
        assert!(login_identity(version, &Packet::new(2, data[..length].to_vec())).is_err());
    }
    assert_eq!(
        login_identity(version, &Packet::new(2, data.clone())).unwrap(),
        identity
    );
    data[session_offset..].fill(3);
    assert_eq!(
        login_identity(version, &Packet::new(2, data.clone())).unwrap(),
        identity
    );
    assert!(
        login_identity(
            ProtocolVersion::new(775).unwrap(),
            &Packet::new(2, data.clone())
        )
        .is_err()
    );
    data.push(0);
    assert!(login_identity(version, &Packet::new(2, data)).is_err());
}

#[test]
fn packet_ids_and_disconnect_encoding_follow_the_version_and_phase() {
    for (number, finish, kick, start, ack) in [
        (764, 2, 0x1b, 0x65, 0x0b),
        (765, 2, 0x1b, 0x67, 0x0b),
        (767, 3, 0x1d, 0x69, 0x0c),
        (774, 3, 0x20, 0x74, 0x0f),
        (777, 3, 0x20, 0x78, 0x10),
    ] {
        let version = ProtocolVersion::new(number).unwrap();
        assert_eq!(
            version.kind(State::Configuration, Direction::Serverbound, finish),
            PacketKind::FinishConfiguration
        );
        assert_eq!(
            version.kind(State::Play, Direction::Clientbound, kick),
            PacketKind::Disconnect
        );
        assert_eq!(version.start_configuration(), Some(start));
        assert_eq!(version.configuration_acknowledged(), Some(ack));
        let login = disconnect(Some(version), State::Login, "quoted \"message\"\n").unwrap();
        let json = read_string(&mut login.data.as_slice(), 32767).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(json).unwrap()["text"],
            "quoted \"message\"\n"
        );
        let play = disconnect(Some(version), State::Play, "hello").unwrap();
        if number >= 765 {
            assert_eq!(play.data, b"\x08\x00\x05hello");
        } else {
            assert_eq!(
                read_string(&mut play.data.as_slice(), 32767).unwrap(),
                "{\"text\":\"hello\"}"
            );
        }
    }
    assert!(ProtocolVersion::new(-1).is_err());
    assert!(ProtocolVersion::new(9999).is_err());
    assert!(
        ProtocolVersion::new(47)
            .unwrap()
            .start_configuration()
            .is_none()
    );
}

#[test]
fn state_transitions_require_acknowledgements_and_track_each_direction() {
    let version = ProtocolVersion::new(774).unwrap();
    let mut state = ConnectionState::new(State::Login);
    assert!(
        state
            .observe(version, Direction::Serverbound, &Packet::empty(3))
            .is_err()
    );
    let mut data = Vec::new();
    write_string("Player", &mut data);
    data.extend([0; 16]);
    state
        .observe(version, Direction::Serverbound, &Packet::new(0, data))
        .unwrap();
    let mut success = vec![0; 16];
    write_string("Player", &mut success);
    success.push(0);
    state
        .observe(version, Direction::Clientbound, &Packet::new(2, success))
        .unwrap();
    assert_eq!(state.phase(Direction::Clientbound), State::Configuration);
    assert_eq!(state.phase(Direction::Serverbound), State::Login);
    state
        .observe(version, Direction::Serverbound, &Packet::empty(3))
        .unwrap();
    assert!(state.settled(State::Configuration));
    assert!(
        state
            .observe(version, Direction::Serverbound, &Packet::empty(3))
            .is_err()
    );
    state
        .observe(version, Direction::Clientbound, &Packet::empty(3))
        .unwrap();
    assert_eq!(state.phase(Direction::Serverbound), State::Configuration);
    state
        .observe(version, Direction::Serverbound, &Packet::empty(3))
        .unwrap();
    assert!(state.settled(State::Play));
    state
        .observe(version, Direction::Clientbound, &Packet::empty(0x74))
        .unwrap();
    assert_eq!(state.phase(Direction::Serverbound), State::Play);
    state
        .observe(version, Direction::Serverbound, &Packet::empty(0x0f))
        .unwrap();
    assert!(state.settled(State::Configuration));
}

#[test]
fn handshake_and_legacy_login_have_explicit_transitions() {
    let version = ProtocolVersion::new(47).unwrap();
    let handshake = Handshake {
        protocol: 47,
        address: "test".into(),
        port: 25565,
        next_state: NextState::Login,
    };
    let mut state = ConnectionState::default();
    state
        .observe(version, Direction::Serverbound, &handshake.packet())
        .unwrap();
    assert!(state.settled(State::Login));
    assert!(state.accept_handshake(&handshake).is_err());
    let mut start = Vec::new();
    write_string("Player", &mut start);
    state
        .observe(version, Direction::Serverbound, &Packet::new(0, start))
        .unwrap();
    let mut success = Vec::new();
    write_string("00000000-0000-0000-0000-000000000001", &mut success);
    write_string("Player", &mut success);
    state
        .observe(version, Direction::Clientbound, &Packet::new(2, success))
        .unwrap();
    assert!(state.settled(State::Play));
}

#[test]
fn malformed_compression_streams_fail_without_panics_or_unbounded_output() {
    for bytes in [
        &[0x78, 0x01, 0x07, 0, 0, 0, 1][..], // Reserved DEFLATE block type.
        &[0x78, 0x01, 0x01, 0x01, 0, 0, 0, 0, 0, 0, 1], // Bad stored-block complement.
        &[0x78, 0x20, 0, 0, 0, 0, 0, 0, 0],  // Preset dictionary.
    ] {
        assert!(compression::inflate(bytes, 256).is_err());
    }
    let reference = include_bytes!("../../tests/fixtures/dynamic.zlib");
    for index in 0..reference.len() {
        let mut mutated = reference.to_vec();
        mutated[index] ^= 0x80;
        if let Ok(output) = compression::inflate(&mutated, 114400) {
            assert_eq!(output.len(), 114400);
        }
    }
}

#[test]
fn network_commands_preserve_redirects_and_override_signed_backend_literals() {
    for number in [772, 774] {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        // root -> backend "server" -> signable minecraft:message, and root -> "help".
        let mut data = vec![4, 0, 2, 1, 3, 5, 1, 2];
        write_string("server", &mut data);
        data.extend([6, 0]);
        write_string("message", &mut data);
        data.push(20);
        data.extend([13, 0, 1]);
        write_string("help", &mut data); // executable literal, redirects to server
        data.push(0);
        let expanded = caps
            .network_commands(&Packet::new(0x10, data.clone()))
            .unwrap();
        let mut bytes = expanded.data.as_slice();
        assert_eq!(read_varint(&mut bytes).unwrap(), 7);
        assert_eq!(&bytes[..5], &[0, 3, 3, 4, 6]); // help preserved; proxy server and hub appended
        // Backend nodes and redirect index were copied unchanged.
        assert_eq!(&bytes[5..5 + data.len() - 6], &data[5..data.len() - 1]);
        assert!(expanded.data.windows(6).any(|b| b == b"server"));
        assert!(expanded.data.windows(3).any(|b| b == b"hub"));
        assert_eq!(*expanded.data.last().unwrap(), 0);
    }
}

#[test]
fn signed_proxy_command_without_signatures_preserves_acknowledgement_offset() {
    for number in [772, 774] {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        let mut data = Vec::new();
        write_string("hub", &mut data);
        data.extend([0; 16]);
        data.push(0);
        write_varint(7, &mut data);
        data.extend([0; 4]);
        let (command, acknowledgement) =
            caps.proxy_command(&Packet::new(7, data)).unwrap().unwrap();
        assert_eq!(command, "hub");
        assert_eq!(acknowledgement, Some(Packet::new(5, vec![7])));
    }
}

#[test]
fn secure_profile_join_requires_authenticated_identity_and_valid_packet() {
    for number in [772, 774] {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        let mut data = 1_i32.to_be_bytes().to_vec();
        data.extend([0, 1]); // hardcore, world count
        write_string("minecraft:overworld", &mut data);
        data.extend([20, 8, 8, 0, 1, 0, 0]); // limits, flags, dimension type
        write_string("minecraft:overworld", &mut data);
        data.extend([0; 8]); // seed
        data.extend([0, 255, 0, 0, 0, 0, 63, 1]); // game modes, flags, death, cooldown, sea level, secure
        let mut packet = Packet::new(if number == 772 { 0x2b } else { 0x30 }, data);
        caps.validate_join(&packet, true).unwrap();
        let denied = caps.validate_join(&packet, false).unwrap_err();
        assert_eq!(denied.kind(), std::io::ErrorKind::Unsupported);
        assert!(denied.to_string().contains("Unauthenticated"));
        *packet.data.last_mut().unwrap() = 0;
        caps.validate_join(&packet, false).unwrap();
        caps.validate_join(&packet, true).unwrap();
        *packet.data.last_mut().unwrap() = 2;
        assert!(caps.validate_join(&packet, true).is_err());
        assert!(caps.validate_join(&packet, false).is_err());
        packet.data.pop();
        assert!(caps.validate_join(&packet, true).is_err());
    }
}

#[test]
fn switching_requires_an_explicit_capability_and_real_server_fixture() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/servers.json")).unwrap();
    let tested: std::collections::BTreeSet<i32> = fixtures
        .as_object()
        .unwrap()
        .values()
        .filter(|fixture| fixture["switchable"] == true)
        .map(|fixture| fixture["protocol"].as_i64().unwrap() as i32)
        .collect();
    let supported: std::collections::BTreeSet<i32> = (0..=800)
        .filter(|number| {
            ProtocolVersion::new(*number).is_ok_and(ProtocolVersion::supports_switching)
        })
        .collect();
    assert_eq!(
        supported, tested,
        "every switching capability needs real-server switch/recovery coverage"
    );
    assert_eq!(supported, [772, 774].into_iter().collect());
    for number in [47, 761, 764, 769, 770, 771, 773, 775, 777] {
        let version = ProtocolVersion::new(number).unwrap();
        assert!(version.switching().is_none());
        assert!(system_message(version, "test").is_err());
        for id in [0x2b, 0x30] {
            assert_ne!(
                version.kind(State::Play, Direction::Clientbound, id),
                PacketKind::JoinGame
            );
        }
    }
    for (number, join, chat) in [(772, 0x2b, 0x72), (774, 0x30, 0x77)] {
        let version = ProtocolVersion::new(number).unwrap();
        assert_eq!(
            version.kind(State::Play, Direction::Clientbound, join),
            PacketKind::JoinGame
        );
        let message = system_message(version, "test").unwrap();
        assert_eq!(message.id, chat);
        assert_eq!(message.data, b"\x08\x00\x04test\x00");
    }
}

#[test]
fn switching_commands_reject_malformed_data_and_preserve_signed_arguments() {
    for number in [772, 774] {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        assert!(
            caps.proxy_command(&Packet::new(8, vec![]))
                .unwrap()
                .is_none()
        );
        let mut data = Vec::new();
        write_string("server survival", &mut data);
        data.extend([0; 16]);
        data.push(1);
        write_string("name", &mut data);
        data.extend([0; 256]);
        data.extend([7, 0, 0, 0, 1]);
        assert!(
            caps.proxy_command(&Packet::new(7, data.clone()))
                .unwrap()
                .is_none()
        );
        for cut in [data.len() - 1, data.len() - 4, 5] {
            assert!(
                caps.proxy_command(&Packet::new(7, data[..cut].to_vec()))
                    .is_err()
            );
        }
        let mut unsigned = Vec::new();
        write_string("hub", &mut unsigned);
        unsigned.push(0);
        assert!(caps.proxy_command(&Packet::new(6, unsigned)).is_err());
        // Unknown parser IDs must not be treated as property-free nodes.
        let mut tree = vec![2, 0, 1, 1, 2, 0];
        write_string("arg", &mut tree);
        tree.extend([57, 0]);
        assert!(caps.network_commands(&Packet::new(0x10, tree)).is_err());
    }
}
