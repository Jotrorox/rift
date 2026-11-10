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

fn noise(length: usize, mut seed: u32) -> Vec<u8> {
    (0..length)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 24) as u8
        })
        .collect()
}

#[tokio::test]
async fn incompressible_packets_are_stored_and_fit_the_frame_limit() {
    // Every byte at or above 144 costs 9 bits as a fixed-Huffman literal.
    let high: Vec<u8> = noise(1 << 16, 7).iter().map(|byte| byte | 0x90).collect();
    for raw in [noise(1 << 16, 1), high, Vec::new(), vec![0xff]] {
        let encoded = compression::deflate(&raw);
        let blocks = raw.len().div_ceil(16 * 1024).max(1);
        assert!(encoded.len() <= raw.len() + 6 + 5 * blocks, "{}", raw.len());
        assert_eq!(compression::inflate(&encoded, raw.len()).unwrap(), raw);
    }
    // Mixed data keeps compressing where it can.
    let mut mixed = noise(40_000, 3);
    mixed.extend(vec![0; 40_000]);
    mixed.extend(noise(40_000, 4));
    let encoded = compression::deflate(&mixed);
    assert!(encoded.len() < 82_000);
    assert_eq!(compression::inflate(&encoded, mixed.len()).unwrap(), mixed);

    let packet = Packet::new(0x27, noise(1_900 * 1024, 9));
    let mut codec = Codec::default();
    codec.set_compression(256);
    let frame = codec.encode(&packet).unwrap();
    assert!(frame.len() <= MAX_FRAME_SIZE + 3);
    assert_eq!(
        Reader::default()
            .read(&mut frame.as_slice(), codec)
            .await
            .unwrap(),
        Some(packet)
    );
}

#[test]
fn inflate_accepts_long_codes_and_overlapping_references() {
    // Codes up to 15 bits, longer than the single-lookup table, from zlib.
    let ruler: Vec<u8> = (1..=16384u32).map(|i| i.trailing_zeros() as u8).collect();
    let long = include_bytes!("../../tests/fixtures/long_codes.zlib");
    assert_eq!(compression::inflate(long, ruler.len()).unwrap(), ruler);
    for cut in 1..8 {
        assert!(compression::inflate(&long[..long.len() - cut], ruler.len()).is_err());
    }
    // Run-length references copy bytes the same reference just wrote.
    let runs: Vec<u8> = [vec![1; 1000], [2, 3].repeat(700), vec![9; 3]].concat();
    let encoded = compression::deflate(&runs);
    assert!(encoded.len() < 100);
    assert_eq!(compression::inflate(&encoded, runs.len()).unwrap(), runs);
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
fn handshake_keeps_only_the_hostname_and_forge_markers() {
    let decode = |address: &str| {
        Handshake::decode(
            &Handshake {
                protocol: 774,
                address: address.into(),
                port: 25565,
                next_state: NextState::Login,
            }
            .packet(),
        )
        .unwrap()
        .address
    };
    for marker in ["\0FML\0", "\0FML2\0", "\0FML3\0", "\0FORGE", "\0FORGE41"] {
        assert_eq!(
            decode(&format!("Play.Test.{marker}")),
            format!("Play.Test.{marker}")
        );
    }
    // Forge reads the list in any order; only the marker survives.
    for (address, expected) in [
        ("play.test\0FML3\x00203.0.113.7", "play.test\0FML3\0"),
        ("play.test\x00203.0.113.7\0FORGE", "play.test\0FORGE"),
        ("play.test\0extra\0FORGE2\0more", "play.test\0FORGE2"),
    ] {
        assert_eq!(decode(address), expected, "{address:?}");
    }
    // BungeeCord IP forwarding: host, client IP, UUID and profile properties.
    let spoofed = "play.test\x00203.0.113.7\x00069a79f444e94726a5befca90e38aaf5\x00[{\"name\":\"textures\",\"value\":\"x\",\"signature\":\"y\"}]";
    for address in [
        spoofed,
        "play.test\0metadata",
        "play.test\0FML4\0",
        "play.test\0FORGE12345",
        "play.test\0FORGEx",
        "play.test\0",
    ] {
        assert_eq!(decode(address), "play.test", "{address:?}");
    }
}

#[test]
fn login_success_validates_session_uuid_without_changing_player_identity() {
    for number in [776, 777] {
        let version = ProtocolVersion::new(number).unwrap();
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
}

#[test]
fn packet_ids_and_disconnect_encoding_follow_the_version_and_phase() {
    for (number, finish, kick, start, ack) in [
        (764, 2, 0x1b, 0x65, 0x0b),
        (765, 2, 0x1b, 0x67, 0x0b),
        (767, 3, 0x1d, 0x69, 0x0c),
        (768, 3, 0x1d, 0x70, 0x0e),
        (769, 3, 0x1d, 0x70, 0x0e),
        (770, 3, 0x1c, 0x6f, 0x0e),
        (771, 3, 0x1c, 0x6f, 0x0f),
        (772, 3, 0x1c, 0x6f, 0x0f),
        (773, 3, 0x20, 0x74, 0x0f),
        (774, 3, 0x20, 0x74, 0x0f),
        (775, 3, 0x20, 0x76, 0x10),
        (776, 3, 0x20, 0x76, 0x10),
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

fn command_id(number: i32) -> i32 {
    match number {
        767 => 4,
        768..=770 => 5,
        771..=774 => 6,
        775..=777 => 7,
        _ => panic!("unmapped test protocol"),
    }
}
fn command_tree_id(number: i32) -> i32 {
    if number < 770 { 0x11 } else { 0x10 }
}
fn chat_update(number: i32) -> Vec<u8> {
    vec![0; if number < 770 { 3 } else { 4 }]
}

#[test]
fn network_commands_preserve_redirects_and_override_signed_backend_literals() {
    for number in 767..=777 {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        // root -> backend "server" -> signable minecraft:message, and root -> "help".
        let mut data = vec![4, 0, 2, 1, 3, 5, 1, 2];
        write_string("server", &mut data);
        data.extend([6, 0]);
        write_string("message", &mut data);
        data.push(if number < 771 { 19 } else { 20 });
        data.extend([13, 0, 1]);
        write_string("help", &mut data); // executable literal, redirects to server
        data.push(0);
        let expanded = caps
            .network_commands(&Packet::new(command_tree_id(number), data.clone()))
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
    for number in 767..=777 {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        let mut data = Vec::new();
        write_string("hub", &mut data);
        data.extend([0; 16]);
        data.push(0);
        write_varint(7, &mut data);
        data.extend(chat_update(number));
        let (command, acknowledgement) = caps
            .proxy_command(&Packet::new(command_id(number) + 1, data))
            .unwrap()
            .unwrap();
        assert_eq!(command, "hub");
        assert_eq!(
            acknowledgement,
            Some(Packet::new(command_id(number) - 1, vec![7]))
        );
    }
}

#[test]
fn secure_profile_join_requires_authenticated_identity_and_valid_packet() {
    for number in 767..=777 {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        let mut data = 1_i32.to_be_bytes().to_vec();
        data.extend([0, 1]); // hardcore, world count
        write_string("minecraft:overworld", &mut data);
        data.extend([20, 8, 8, 0, 1, 0, 0]); // limits, flags, dimension type
        write_string("minecraft:overworld", &mut data);
        data.extend([0; 8]); // seed
        data.extend([0, if number >= 777 { 0 } else { 255 }, 0, 0, 0, 0]);
        if number >= 768 {
            data.push(63);
        } // Sea level.
        if number >= 776 {
            data.push(0);
        } // Backend online mode.
        data.push(1); // Enforce secure profile.
        let mut packet = Packet::new(caps.join_game, data);
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
    assert_eq!(supported.len(), 50);
    assert_eq!(supported.first(), Some(&47));
    assert_eq!(supported.last(), Some(&777));
    for number in &supported {
        let version = ProtocolVersion::new(*number).unwrap();
        let caps = version.switching().unwrap();
        assert_eq!(
            version.kind(State::Play, Direction::Clientbound, caps.join_game),
            PacketKind::JoinGame
        );
        assert!(system_message(version, "test").is_ok());
    }
    for number in [46, 48, 106, 111, 339, 7590, 778] {
        assert!(ProtocolVersion::new(number).is_err());
    }
    for (number, join, chat) in [
        (767, 0x2b, 0x6c),
        (768, 0x2c, 0x73),
        (769, 0x2c, 0x73),
        (770, 0x2b, 0x72),
        (771, 0x2b, 0x72),
        (772, 0x2b, 0x72),
        (773, 0x30, 0x77),
        (774, 0x30, 0x77),
        (775, 0x31, 0x79),
        (776, 0x31, 0x79),
        (777, 0x32, 0x7c),
    ] {
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
fn action_bars_and_titles_use_verified_play_packets() {
    for (number, subtitle, heading, times) in [
        (761, 0x59, 0x5b, 0x5c),
        (762, 0x5d, 0x5f, 0x60),
        (763, 0x5d, 0x5f, 0x60),
        (764, 0x5f, 0x61, 0x62),
        (765, 0x61, 0x63, 0x64),
        (766, 0x63, 0x65, 0x66),
        (767, 0x63, 0x65, 0x66),
        (768, 0x6a, 0x6c, 0x6d),
        (769, 0x6a, 0x6c, 0x6d),
        (770, 0x69, 0x6b, 0x6c),
        (771, 0x69, 0x6b, 0x6c),
        (772, 0x69, 0x6b, 0x6c),
        (773, 0x6e, 0x70, 0x71),
        (774, 0x6e, 0x70, 0x71),
        (775, 0x70, 0x72, 0x73),
        (776, 0x70, 0x72, 0x73),
        (777, 0x72, 0x74, 0x75),
    ] {
        let version = ProtocolVersion::new(number).unwrap();
        let component = |text| disconnect(Some(version), State::Play, text).unwrap().data;
        assert_eq!(
            title(version, Some(&"Hello".into()), None, [10, 70, 20]).unwrap(),
            [
                Packet::new(times, vec![0, 0, 0, 10, 0, 0, 0, 70, 0, 0, 0, 20]),
                Packet::new(subtitle, component("")),
                Packet::new(heading, component("Hello")),
            ],
            "protocol {number}"
        );
        assert_eq!(
            title(version, None, Some(&"Sub".into()), [0, 1, 2]).unwrap()[1],
            Packet::new(subtitle, component("Sub"))
        );
        // Session state tracking must treat these as ordinary play packets.
        for id in [subtitle, heading, times] {
            assert_eq!(
                version.kind(State::Play, Direction::Clientbound, id),
                PacketKind::Unknown
            );
        }
        let chat = system_message(version, "bar").unwrap();
        let bar = action_bar(version, "bar").unwrap();
        assert_eq!(bar.id, chat.id);
        let (overlay, text) = bar.data.split_last().unwrap();
        assert_eq!((*overlay, text), (1, chat.data.split_last().unwrap().1));
    }
    // Before 1.17, one Title packet per action: times, subtitle, title.
    for (number, id, times_action) in [
        (47, 0x45, 2),
        (210, 0x45, 2),
        (315, 0x45, 3),
        (335, 0x47, 3),
        (340, 0x48, 3),
        (404, 0x4b, 3),
        (498, 0x4f, 3),
        (578, 0x50, 3),
        (736, 0x4f, 3),
        (754, 0x4f, 3),
    ] {
        let version = ProtocolVersion::new(number).unwrap();
        let action = |action: u8, body: &[u8]| {
            let mut data = vec![action];
            data.extend(body);
            Packet::new(id, data)
        };
        let json = |text: &str| {
            let mut data = Vec::new();
            write_string(&serde_json::json!({ "text": text }).to_string(), &mut data);
            data
        };
        assert_eq!(
            title(version, Some(&"Hello".into()), None, [10, 70, 20]).unwrap(),
            [
                action(times_action, &[0, 0, 0, 10, 0, 0, 0, 70, 0, 0, 0, 20]),
                action(1, &json("")),
                action(0, &json("Hello")),
            ],
            "protocol {number}"
        );
    }
    // 1.17 to 1.19.2: separate packets, JSON text.
    for (number, subtitle, heading, times) in [
        (755, 0x57, 0x59, 0x5a),
        (756, 0x57, 0x59, 0x5a),
        (757, 0x58, 0x5a, 0x5b),
        (758, 0x58, 0x5a, 0x5b),
        (759, 0x58, 0x5a, 0x5b),
        (760, 0x5b, 0x5d, 0x5e),
    ] {
        let version = ProtocolVersion::new(number).unwrap();
        let packets = title(
            version,
            Some(&"Hello".into()),
            Some(&"Sub".into()),
            [1, 2, 3],
        )
        .unwrap();
        let ids: Vec<_> = packets.iter().map(|packet| packet.id).collect();
        assert_eq!(ids, [times, subtitle, heading], "protocol {number}");
        let mut expected = Vec::new();
        write_string(r#"{"text":"Hello"}"#, &mut expected);
        assert_eq!(packets[2].data, expected, "protocol {number}");
    }
    // Components are rendered for the client: hex becomes a named colour on 1.8.
    let version = ProtocolVersion::new(47).unwrap();
    let rich = crate::text::Text::Component(serde_json::json!({"text": "Hi", "color": "#ff8800"}));
    let packets = title(version, Some(&rich), None, [1, 2, 3]).unwrap();
    let mut expected = vec![0];
    write_string(r#"{"color":"gold","text":"Hi"}"#, &mut expected);
    assert_eq!(packets[2].data, expected);
}

#[test]
fn switching_commands_reject_malformed_data_and_preserve_signed_arguments() {
    for number in 767..=777 {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        assert!(
            caps.proxy_command(&Packet::new(0x7fff, vec![]))
                .unwrap()
                .is_none()
        );
        let mut data = Vec::new();
        write_string("server survival", &mut data);
        data.extend([0; 16]);
        data.push(1);
        write_string("name", &mut data);
        data.extend([0; 256]);
        data.push(7);
        data.extend(chat_update(number));
        assert!(
            caps.proxy_command(&Packet::new(command_id(number) + 1, data.clone()))
                .unwrap()
                .is_none()
        );
        for cut in [data.len() - 1, data.len() - 4, 5] {
            assert!(
                caps.proxy_command(&Packet::new(command_id(number) + 1, data[..cut].to_vec()))
                    .is_err()
            );
        }
        let mut unsigned = Vec::new();
        write_string("hub", &mut unsigned);
        unsigned.push(0);
        assert!(
            caps.proxy_command(&Packet::new(command_id(number), unsigned))
                .is_err()
        );
        // Unknown parser IDs must not be treated as property-free nodes.
        let mut tree = vec![2, 0, 1, 1, 2, 0];
        write_string("arg", &mut tree);
        tree.extend([127, 0]);
        assert!(
            caps.network_commands(&Packet::new(command_tree_id(number), tree))
                .is_err()
        );
    }
}

#[test]
fn extension_commands_replace_backend_roots_and_keep_unsigned_arguments() {
    for number in 767..=777 {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        let mut data = vec![2, 0, 1, 1, 5, 0];
        write_string("queue", &mut data);
        data.push(0);
        let names = ["queue".to_owned(), "staff".to_owned()];
        let tree: Vec<_> = names.iter().map(|name| (name.clone(), false)).collect();
        let commands = |name: &str| names.iter().any(|known| known == name);
        let expanded = caps
            .network_commands_with(&Packet::new(command_tree_id(number), data.clone()), &tree)
            .unwrap();
        let mut bytes = expanded.data.as_slice();
        assert_eq!(read_varint(&mut bytes).unwrap(), 9);
        assert_eq!(&bytes[..6], &[0, 4, 2, 4, 5, 7]);
        assert_eq!(&bytes[6..6 + data.len() - 5], &data[4..data.len() - 1]);
        // An ordinary unsigned command is intercepted, while unknown commands
        // and commands with signed arguments retain their backend semantics.
        let mut unsigned = Vec::new();
        write_string("queue game", &mut unsigned);
        assert_eq!(
            caps.proxy_command_with(
                &Packet::new(command_id(number), unsigned.clone()),
                &commands
            )
            .unwrap()
            .unwrap()
            .0,
            "queue game"
        );
        assert!(
            caps.proxy_command_with(&Packet::new(command_id(number), unsigned.clone()), &|_| {
                false
            })
            .unwrap()
            .is_none()
        );
        unsigned.extend([0; 16]);
        unsigned.push(1);
        write_string("args", &mut unsigned);
        unsigned.extend([42; 256]);
        unsigned.push(0);
        unsigned.extend(chat_update(number));
        assert!(
            caps.proxy_command_with(&Packet::new(command_id(number) + 1, unsigned), &commands)
                .unwrap()
                .is_none()
        );
    }
}

/// Command Suggestions Request and Response IDs from PrismarineJS
/// minecraft-data through 26.1, and from the 26.2 and 26.3 server artifacts'
/// `GameProtocols` registration order (javap).
fn suggestion_ids(number: i32) -> (i32, i32) {
    match number {
        761 => (0x08, 0x0d),
        762 | 763 => (0x09, 0x0f),
        764 | 765 => (0x0a, 0x10),
        766 | 767 => (0x0b, 0x10),
        768 | 769 => (0x0d, 0x10),
        770 => (0x0d, 0x0f),
        771..=774 => (0x0e, 0x0f),
        775..=777 => (0x0f, 0x0f),
        _ => panic!("unmapped test protocol"),
    }
}

#[test]
fn proxy_suggestion_requests_are_taken_only_for_proxy_command_arguments() {
    for number in 761..=777 {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        let (request_id, _) = suggestion_ids(number);
        let request = |text: &str| {
            let mut data = Vec::new();
            write_varint(300, &mut data);
            write_string(text, &mut data);
            Packet::new(request_id, data)
        };
        let commands = |name: &str| name == "ban";
        for text in ["/server lo", "/hub ", "/ban Al", "ban Al", "/ban Alice 1d"] {
            assert_eq!(
                caps.proxy_suggestion_request(&request(text), &commands)
                    .unwrap(),
                Some((300, text.to_owned())),
                "protocol {number}: {text}"
            );
        }
        // Backend commands, unregistered names and root names being typed
        // belong to the backend.
        for text in ["/give Al", "/serverx a", "/server", "/ban", "/kick Al", ""] {
            assert_eq!(
                caps.proxy_suggestion_request(&request(text), &commands)
                    .unwrap(),
                None,
                "protocol {number}: {text}"
            );
        }
        let mut other = request("/server lo");
        other.id = request_id - 1;
        assert_eq!(
            caps.proxy_suggestion_request(&other, &commands).unwrap(),
            None
        );
        let mut trailing = request("/server lo");
        trailing.data.push(0);
        assert!(caps.proxy_suggestion_request(&trailing, &commands).is_err());
        let truncated = Packet::new(request_id, request("/server lo").data[..4].to_vec());
        assert!(
            caps.proxy_suggestion_request(&truncated, &commands)
                .is_err()
        );
    }
}

#[test]
fn rich_tooltips_and_titles_are_rendered_for_the_client() {
    let component = serde_json::json!({"text": "Chill", "color": "#ff8800"});
    let rich = crate::text::Text::Component(component.clone());
    let matches = [Suggestion {
        text: "lounge".into(),
        tooltip: Some(rich.clone()),
    }];
    for number in [764, 770] {
        let version = ProtocolVersion::new(number).unwrap();
        let packet = version
            .switching()
            .unwrap()
            .suggestions(version, 1, 0, 0, &matches)
            .unwrap();
        let mut expected = Vec::new();
        write_varint(1, &mut expected);
        expected.extend([0, 0, 1]);
        write_string("lounge", &mut expected);
        expected.push(1);
        if version.nbt_components() {
            crate::protocol::nbt::component(&component, &mut expected).unwrap();
        } else {
            write_string(&component.to_string(), &mut expected);
        }
        assert_eq!(packet.data, expected, "protocol {number}");
    }
    let version = ProtocolVersion::new(770).unwrap();
    let packets = title(version, Some(&rich), None, [1, 2, 3]).unwrap();
    let mut expected = Vec::new();
    crate::protocol::nbt::component(&component, &mut expected).unwrap();
    assert_eq!(packets[2].data, expected);
}

#[test]
fn suggestion_responses_use_each_versions_id_and_tooltip_encoding() {
    for number in 761..=777 {
        let version = ProtocolVersion::new(number).unwrap();
        let caps = version.switching().unwrap();
        let (_, response_id) = suggestion_ids(number);
        let matches = [
            Suggestion {
                text: "lobby".into(),
                tooltip: None,
            },
            Suggestion {
                text: "lounge".into(),
                tooltip: Some("Chill".into()),
            },
        ];
        let packet = caps.suggestions(version, 300, 8, 2, &matches).unwrap();
        let mut expected = Vec::new();
        write_varint(300, &mut expected);
        expected.extend([8, 2, 2]);
        write_string("lobby", &mut expected);
        expected.push(0);
        write_string("lounge", &mut expected);
        expected.push(1);
        if number >= 765 {
            expected.extend([8, 0, 5]); // Anonymous NBT TAG_String.
            expected.extend(b"Chill");
        } else {
            write_string(r#"{"text":"Chill"}"#, &mut expected);
        }
        assert_eq!(
            packet,
            Packet::new(response_id, expected),
            "protocol {number}"
        );
        assert_eq!(
            version.kind(State::Play, Direction::Clientbound, response_id),
            PacketKind::Unknown
        );
        assert_eq!(
            caps.suggestions(version, 1, 0, 0, &[]).unwrap().data,
            [1, 0, 0, 0]
        );
    }
    for number in [47, 340, 758, 760] {
        let version = ProtocolVersion::new(number).unwrap();
        let caps = version.switching().unwrap();
        assert_eq!(
            caps.suggestions(version, 1, 0, 0, &[]).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
        for id in 0..0x80 {
            let mut data = Vec::new();
            write_varint(1, &mut data);
            write_string("/server lo", &mut data);
            assert_eq!(
                caps.proxy_suggestion_request(&Packet::new(id, data), &|_| false)
                    .unwrap(),
                None
            );
        }
    }
}

#[test]
fn proxy_command_arguments_ask_the_server_only_where_rift_answers() {
    for number in [758, 760, 761, 765, 767, 774, 777] {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        let root = Packet::new(caps.commands, vec![1, 0, 0, 0]);
        let commands = [("ban".to_owned(), true), ("staff".to_owned(), false)];
        let expanded = caps.network_commands_with(&root, &commands).unwrap();
        // Rift's own parser accepts the suggestion type it wrote.
        caps.network_commands_with(&expanded, &commands).unwrap();
        let find = |needle: &[u8], from: usize| {
            expanded.data[from..]
                .windows(needle.len())
                .position(|window| window == needle)
                .map(|at| at + from)
        };
        let ask = b"minecraft:ask_server";
        if number < 761 {
            assert_eq!(find(ask, 0), None, "protocol {number}");
            continue;
        }
        // `/server <name>` and `/ban <args>` ask; `/staff <args>` does not.
        let server = find(ask, 0).unwrap();
        let ban = find(b"ban", 0).unwrap();
        let staff = find(b"staff", 0).unwrap();
        assert!(server < ban, "protocol {number}");
        let second = find(ask, server + 1).unwrap();
        assert!(ban < second && second < staff, "protocol {number}");
        assert_eq!(find(ask, second + 1), None, "protocol {number}");
        // Suggesting arguments carry flag 0x10 on their executable argument node.
        let args = find(b"\x04args", ban).unwrap();
        assert_eq!(
            expanded.data[args - 2..args],
            [0x16, 0],
            "protocol {number}"
        );
        let args = find(b"\x04args", staff).unwrap();
        assert_eq!(
            expanded.data[args - 2..args],
            [0x06, 0],
            "protocol {number}"
        );
    }
}

#[test]
fn command_parser_properties_follow_each_registry() {
    for number in 767..=777 {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        let score = if number < 771 { 30 } else { 31 };
        let time = if number < 771 { 42 } else { 43 };
        let resource_end = if number < 770 {
            46
        } else if number == 770 {
            47
        } else {
            48
        };
        let last = match number {
            767..=769 => 53,
            770 => 54,
            771..=776 => 56,
            777 => 61,
            _ => unreachable!(),
        };
        for parser in 0..=last {
            let mut data = vec![2, 0, 1, 1, 2, 0];
            write_string("arg", &mut data);
            data.push(parser);
            match parser {
                1..=4 => {
                    data.push(3); // Both minimum and maximum.
                    data.extend(vec![0; if parser == 2 || parser == 4 { 16 } else { 8 }]);
                }
                5 => data.push(2),
                p if p == 6 || p == score => data.push(1),
                p if p == time => data.extend([0; 4]),
                p if (time + 1..=resource_end).contains(&p) => {
                    write_string("minecraft:worldgen/biome", &mut data)
                }
                _ => {}
            }
            data.push(0); // Root index.
            let packet = Packet::new(command_tree_id(number), data);
            assert!(
                caps.network_commands(&packet).is_ok(),
                "protocol {number}, parser {parser}"
            );
        }
    }
}

#[test]
fn components_spell_hover_and_click_events_for_the_client() {
    let typed = serde_json::json!({
        "text": "[Accept]",
        "click_event": {"action": "run_command", "command": "/party accept"},
    });
    let legacy = serde_json::json!({
        "text": "[Accept]",
        "clickEvent": {"action": "run_command", "value": "/party accept"},
    });
    let json = |packet: Packet| {
        let text = read_string(&mut packet.data.as_slice(), 262144).unwrap();
        serde_json::from_str::<serde_json::Value>(text).unwrap()
    };
    // 1.20.2 reads JSON in play; it gets the legacy spelling.
    let old = ProtocolVersion::new(764).unwrap();
    assert_eq!(json(system_component(old, &typed).unwrap()), legacy);
    assert_eq!(json(action_bar_component(old, &typed).unwrap()), legacy);
    assert_eq!(
        json(disconnect_component(Some(old), State::Play, &typed).unwrap()),
        legacy
    );
    // Login is JSON in every version: 1.21.5 gets the typed spelling.
    let new = ProtocolVersion::new(770).unwrap();
    assert_eq!(
        json(disconnect_component(Some(new), State::Login, &legacy).unwrap()),
        typed
    );
    assert_eq!(
        json(disconnect_component(Some(new), State::Login, &typed).unwrap()),
        typed
    );
}

#[test]
fn json_components_keep_json_until_1_20_3_and_then_follow_vanilla_nbt_conversion() {
    let component = serde_json::json!({"extra": ["a", {"bold": true}, 7, 2.5, 10_000_000_000_i64]});
    let json = component.to_string();
    for number in [47, 340, 764] {
        let version = ProtocolVersion::new(number).unwrap();
        for packet in [
            system_component(version, &component).unwrap(),
            disconnect_component(Some(version), State::Play, &component).unwrap(),
        ] {
            assert_eq!(
                read_string(&mut packet.data.as_slice(), 262144).unwrap(),
                json
            );
        }
    }
    let version = ProtocolVersion::new(765).unwrap();
    // Login packets use JSON in every version.
    let login = disconnect_component(Some(version), State::Login, &component).unwrap();
    assert_eq!(
        read_string(&mut login.data.as_slice(), 262144).unwrap(),
        json
    );

    let utf = |text: &str| [&(text.len() as u16).to_be_bytes()[..], text.as_bytes()].concat();
    let wrapped = |tag: u8, payload: &[u8]| [&[tag, 0, 0][..], payload, &[0]].concat();
    let mut expected = vec![10, 9];
    expected.extend(utf("extra"));
    // A mixed list holds compounds; non-compounds are wrapped under "".
    expected.extend([10, 0, 0, 0, 5]);
    expected.extend(wrapped(8, &utf("a")));
    expected.extend([1, 0, 4, b'b', b'o', b'l', b'd', 1, 0]);
    expected.extend(wrapped(3, &7_i32.to_be_bytes()));
    expected.extend(wrapped(6, &2.5_f64.to_be_bytes()));
    expected.extend(wrapped(4, &10_000_000_000_i64.to_be_bytes()));
    expected.push(0);
    for state in [State::Configuration, State::Play] {
        assert_eq!(
            disconnect_component(Some(version), state, &component)
                .unwrap()
                .data,
            expected
        );
    }
    let mut chat = expected.clone();
    chat.push(0);
    assert_eq!(system_component(version, &component).unwrap().data, chat);

    // Strings match literal text; homogeneous and empty lists keep their type.
    // Object nulls are omitted, as JsonOps does.
    for (component, expected) in [
        (serde_json::json!("hi"), vec![8, 0, 2, b'h', b'i']),
        (
            serde_json::json!(["a", "b"]),
            [&[9, 8, 0, 0, 0, 2][..], &utf("a"), &utf("b")].concat(),
        ),
        (serde_json::json!([]), vec![9, 0, 0, 0, 0, 0]),
        (
            serde_json::json!({"color": null, "text": ""}),
            [&[10, 8][..], &utf("text"), &utf(""), &[0]].concat(),
        ),
    ] {
        validate_component(&component).unwrap();
        assert_eq!(
            disconnect_component(Some(version), State::Play, &component)
                .unwrap()
                .data,
            expected
        );
    }
    assert_eq!(
        disconnect(Some(version), State::Play, "hi").unwrap().data,
        [8, 0, 2, b'h', b'i']
    );
    for invalid in [
        serde_json::json!(null),
        serde_json::json!(1),
        serde_json::json!(true),
        serde_json::json!(["a", null]),
        serde_json::json!({"text": "x".repeat(65536)}),
    ] {
        assert!(validate_component(&invalid).is_err(), "{invalid}");
    }
}

#[tokio::test]
async fn skewed_packets_over_the_frame_limit_compress_below_it() {
    // Few repeats but a skewed byte distribution: fixed codes would spend 8
    // bits per byte (about 4 MiB); dynamic codes need about 3.
    let data: Vec<u8> = noise(4 << 20, 11)
        .iter()
        .map(|byte| byte.trailing_zeros() as u8 + b'a')
        .collect();
    let packet = Packet::new(0x27, data);
    let mut codec = Codec::default();
    codec.set_compression(256);
    let frame = codec.encode(&packet).unwrap();
    assert!(frame.len() < MAX_FRAME_SIZE - 128 * 1024, "{}", frame.len());
    assert_eq!(
        Reader::default()
            .read(&mut frame.as_slice(), codec)
            .await
            .unwrap(),
        Some(packet)
    );
}

/// 1.20.2+ spawn info naming `world` (also the dimension type before 1.20.5)
/// and game mode `mode`, with a death location so every optional field is
/// exercised.
fn spawn_info(number: i32, world: &str, mode: u8) -> Vec<u8> {
    let mut data = Vec::new();
    if number < 766 {
        write_string(world, &mut data);
    } else {
        data.push(2); // Dimension type registry ID.
    }
    write_string(world, &mut data);
    data.extend([0; 8]); // seed
    data.extend([mode, if number >= 777 { 0 } else { 255 }]);
    data.extend([0, 0, 1]); // debug, flat, death location
    write_string("minecraft:overworld", &mut data);
    data.extend([0; 8]);
    data.push(0); // portal cooldown
    if number >= 768 {
        data.push(63);
    } // Sea level.
    data
}

fn modern_join(number: i32, id: i32, world: &str, mode: u8) -> Packet {
    let mut data = 1_i32.to_be_bytes().to_vec();
    data.extend([0, 1]); // hardcore, world count
    write_string("minecraft:overworld", &mut data);
    data.extend([20, 8, 8, 0, 1, 0]); // limits, flags
    data.extend(spawn_info(number, world, mode));
    if number >= 776 {
        data.push(0);
    } // Backend online mode.
    if number >= 766 {
        data.push(0);
    } // Enforce secure profile.
    Packet::new(id, data)
}

fn game_event(id: i32, event: u8, value: f32) -> Packet {
    let mut data = vec![event];
    data.extend(value.to_be_bytes());
    Packet::new(id, data)
}

fn spawn(dimension: &str, gamemode: GameMode) -> Option<switching::WorldUpdate> {
    Some(switching::WorldUpdate::Spawn {
        dimension: Some(dimension.to_owned()),
        gamemode: Some(gamemode),
    })
}

#[test]
fn modern_join_respawn_and_game_events_name_the_dimension_and_game_mode() {
    // Clientbound Login, Respawn and Game Event IDs: minecraft-data
    // protocol.json through 26.1; GameProtocols registration order of the
    // official 26.2 and 26.3 server jars (javap) after that.
    let versions = [
        (764, 0x29, 0x43, 0x20),
        (765, 0x29, 0x45, 0x20),
        (766, 0x2b, 0x47, 0x22),
        (767, 0x2b, 0x47, 0x22),
        (768, 0x2c, 0x4c, 0x23),
        (769, 0x2c, 0x4c, 0x23),
        (770, 0x2b, 0x4b, 0x22),
        (771, 0x2b, 0x4b, 0x22),
        (772, 0x2b, 0x4b, 0x22),
        (773, 0x30, 0x50, 0x26),
        (774, 0x30, 0x50, 0x26),
        (775, 0x31, 0x52, 0x26),
        (776, 0x31, 0x52, 0x26),
        (777, 0x32, 0x54, 0x27),
    ];
    for (number, join, respawn, event) in versions {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        assert_eq!(caps.join_game, join, "protocol {number}");
        let packet = modern_join(number, join, "minecraft:the_nether", 1);
        caps.validate_join(&packet, false).unwrap();
        assert_eq!(
            caps.world_update(&packet),
            spawn("minecraft:the_nether", GameMode::Creative),
            "protocol {number}"
        );
        // A Join Game Rift cannot read still starts a new world.
        let mut broken = packet.clone();
        broken.data.push(0);
        assert_eq!(
            caps.world_update(&broken),
            Some(switching::WorldUpdate::Spawn {
                dimension: None,
                gamemode: None,
            })
        );

        let mut data = spawn_info(number, "minecraft:the_end", 3);
        data.push(1); // data kept
        let packet = Packet::new(respawn, data);
        assert_eq!(
            caps.world_update(&packet),
            spawn("minecraft:the_end", GameMode::Spectator),
            "protocol {number}"
        );
        // A custom world keeps its own key; an unknown game mode is unknown.
        let mut data = spawn_info(number, "plugin:arena", 7);
        data.push(0);
        assert_eq!(
            caps.world_update(&Packet::new(respawn, data)),
            Some(switching::WorldUpdate::Spawn {
                dimension: Some("plugin:arena".into()),
                gamemode: None,
            })
        );
        // Anything else with the Respawn ID is skipped, never an error.
        for cut in 0..packet.data.len() {
            let cut = Packet::new(respawn, packet.data[..cut].to_vec());
            assert_eq!(caps.world_update(&cut), None, "protocol {number}");
        }
        let mut trailing = packet.clone();
        trailing.data.push(0);
        assert_eq!(caps.world_update(&trailing), None);

        assert_eq!(
            caps.world_update(&game_event(event, 3, 2.0)),
            Some(switching::WorldUpdate::GameMode(GameMode::Adventure)),
            "protocol {number}"
        );
        for skipped in [
            game_event(event, 3, 1.5),
            game_event(event, 3, 4.0),
            game_event(event, 3, -1.0),
            game_event(event, 3, f32::NAN),
            game_event(event, 7, 1.0), // Rain level.
            Packet::new(event, vec![3, 0, 0]),
            Packet::empty(event),
            game_event(0x7e, 3, 1.0),
        ] {
            assert_eq!(caps.world_update(&skipped), None, "protocol {number}");
        }
    }
}

#[test]
fn every_switching_version_reads_the_game_mode_game_event() {
    // minecraft-data protocol.json `game_state_change` (reason 3,
    // `change_game_mode`, with a float value) for each release through
    // 26.1; the 26.2 and 26.3 server jars' GameProtocols after that.
    let versions = [
        (47, 0x2b),
        (107, 0x1e),
        (108, 0x1e),
        (109, 0x1e),
        (110, 0x1e),
        (210, 0x1e),
        (315, 0x1e),
        (316, 0x1e),
        (335, 0x1e),
        (338, 0x1e),
        (340, 0x1e),
        (393, 0x20),
        (401, 0x20),
        (404, 0x20),
        (477, 0x1e),
        (480, 0x1e),
        (485, 0x1e),
        (490, 0x1e),
        (498, 0x1e),
        (573, 0x1f),
        (575, 0x1f),
        (578, 0x1f),
        (735, 0x1e),
        (736, 0x1e),
        (751, 0x1d),
        (753, 0x1d),
        (754, 0x1d),
        (755, 0x1e),
        (756, 0x1e),
        (757, 0x1e),
        (758, 0x1e),
        (759, 0x1b),
        (760, 0x1d),
        (761, 0x1c),
        (762, 0x1f),
        (763, 0x1f),
        (764, 0x20),
        (765, 0x20),
        (766, 0x22),
        (767, 0x22),
        (768, 0x23),
        (769, 0x23),
        (770, 0x22),
        (771, 0x22),
        (772, 0x22),
        (773, 0x26),
        (774, 0x26),
        (775, 0x26),
        (776, 0x26),
        (777, 0x27),
    ];
    let supported: Vec<i32> = (0..=800)
        .filter(|number| {
            ProtocolVersion::new(*number).is_ok_and(ProtocolVersion::supports_switching)
        })
        .collect();
    assert_eq!(
        versions
            .iter()
            .map(|(number, _)| *number)
            .collect::<Vec<_>>(),
        supported
    );
    for (number, event) in versions {
        let caps = ProtocolVersion::new(number).unwrap().switching().unwrap();
        for (value, mode) in [
            (0.0, GameMode::Survival),
            (1.0, GameMode::Creative),
            (2.0, GameMode::Adventure),
            (3.0, GameMode::Spectator),
        ] {
            assert_eq!(
                caps.world_update(&game_event(event, 3, value)),
                Some(switching::WorldUpdate::GameMode(mode)),
                "protocol {number}"
            );
        }
        assert_eq!(caps.world_update(&game_event(event, 1, 0.0)), None);
    }
}

#[test]
fn player_world_reports_only_real_changes() {
    let mut world = PlayerWorld::default();
    let nether = spawn("minecraft:the_nether", GameMode::Survival).unwrap();
    assert!(world.apply(nether.clone()));
    assert!(!world.apply(nether));
    assert!(world.apply(switching::WorldUpdate::GameMode(GameMode::Creative)));
    assert!(!world.apply(switching::WorldUpdate::GameMode(GameMode::Creative)));
    assert_eq!(
        world,
        PlayerWorld {
            dimension: Some("minecraft:the_nether".into()),
            gamemode: Some(GameMode::Creative),
        }
    );
    assert!(world.apply(switching::WorldUpdate::Spawn {
        dimension: None,
        gamemode: None,
    }));
    assert_eq!(world, PlayerWorld::default());
    assert_eq!(GameMode::Spectator.name(), "spectator");
}
