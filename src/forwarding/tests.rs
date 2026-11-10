use super::*;
use crate::auth::ProfileProperty;

fn profile() -> AuthenticatedProfile {
    AuthenticatedProfile {
        uuid: core::array::from_fn(|i| i as u8),
        name: "Alex_".into(),
        properties: vec![ProfileProperty {
            name: "textures".into(),
            value: "skin-payload".into(),
            signature: Some("signature".into()),
        }],
    }
}

fn velocity() -> Method {
    Method::Velocity {
        secret: Arc::from(&b"test-forwarding-secret"[..]),
    }
}

fn forwarding() -> Forwarding {
    forwarding_for(velocity(), None, 774)
}

fn forwarding_for(method: Method, key: Option<ProfileKey>, number: i32) -> Forwarding {
    Forwarding::new(
        method,
        "2001:db8::1".parse().unwrap(),
        profile(),
        key,
        ProtocolVersion::new(number).unwrap(),
    )
    .unwrap()
}

fn profile_key() -> ProfileKey {
    ProfileKey {
        expiry: 1_800_000_000_000,
        public_key: vec![0x30; 294],
        signature: vec![0x5a; 512],
    }
}

fn request(id: i32, channel: &str, payload: &[u8]) -> Packet {
    let mut data = Vec::new();
    write_varint(id, &mut data);
    write_string(channel, &mut data);
    data.extend_from_slice(payload);
    Packet::new(4, data)
}

#[test]
fn forwarding_mac_and_payload_match_independent_vector() {
    let response = forwarding().response(123, &[4]).unwrap();
    let mut bytes = response.data.as_slice();
    assert_eq!(read_varint(&mut bytes).unwrap(), 123);
    assert_eq!(bytes[0], 1);
    bytes = &bytes[1..];
    let mac: String = bytes[..32].iter().map(|b| format!("{b:02x}")).collect();
    // Python's hmac.digest(..., hashlib.sha256), independently serialized wire data.
    assert_eq!(
        mac,
        "838d900b74e9ef96721a94fd1ddb039a9ad66208caa37c198affe749cb1e2a5d"
    );
    let mut verifier = Hmac::<Sha256>::new_from_slice(b"test-forwarding-secret").unwrap();
    verifier.update(&bytes[32..]);
    verifier.verify_slice(&bytes[..32]).unwrap();
    let mut wrong = Hmac::<Sha256>::new_from_slice(b"wrong-secret").unwrap();
    wrong.update(&bytes[32..]);
    assert!(wrong.verify_slice(&bytes[..32]).is_err());
    let mut changed = bytes[32..].to_vec();
    changed[0] ^= 1;
    let mut verifier = Hmac::<Sha256>::new_from_slice(b"test-forwarding-secret").unwrap();
    verifier.update(&changed);
    assert!(verifier.verify_slice(&bytes[..32]).is_err());
    bytes = &bytes[32..];
    assert_eq!(read_varint(&mut bytes).unwrap(), 4);
    assert_eq!(read_string(&mut bytes, 255).unwrap(), "2001:db8::1");
    assert_eq!(&bytes[..16], &profile().uuid);
    bytes = &bytes[16..];
    assert_eq!(read_string(&mut bytes, 16).unwrap(), "Alex_");
    assert_eq!(read_varint(&mut bytes).unwrap(), 1);
    assert_eq!(read_string(&mut bytes, 32767).unwrap(), "textures");
    assert_eq!(read_string(&mut bytes, 32767).unwrap(), "skin-payload");
    assert_eq!(bytes[0], 1);
    bytes = &bytes[1..];
    assert_eq!(read_string(&mut bytes, 32767).unwrap(), "signature");
    assert!(bytes.is_empty());
}

#[test]
fn forwarding_negotiates_versions_and_rejects_malformed_requests() {
    for (request, expected) in [
        (&[][..], 1),
        (&[1][..], 1),
        (&[2][..], 1),
        (&[3][..], 1),
        (&[4][..], 4),
        (&[5][..], 4),
    ] {
        let response = forwarding().response(1, request).unwrap();
        assert_eq!(response.data[34], expected);
    }
    for payload in [&[0][..], &[255][..], &[4, 0][..], &[128, 1][..]] {
        assert!(forwarding().response(1, payload).is_err());
    }
    for method in [
        Method::Velocity {
            secret: Arc::from(&b""[..]),
        },
        Method::BungeeGuard {
            token: Arc::from(""),
        },
    ] {
        assert!(
            Forwarding::new(
                method,
                "127.0.0.1".parse().unwrap(),
                profile(),
                None,
                ProtocolVersion::new(774).unwrap()
            )
            .is_err()
        );
    }
}

#[test]
fn paper_signed_transaction_ids_are_echoed_and_remain_proxy_owned() {
    // Independent Minecraft VarInt byte vectors, including both negative edge
    // cases. Paper's ThreadLocalRandom.nextInt() covers the full signed range.
    for encoded in [
        &[0xff, 0xff, 0xff, 0xff, 0x0f][..],
        &[0x80, 0x80, 0x80, 0x80, 0x08][..],
        &[0xff, 0xff, 0xff, 0xff, 0x07][..],
        &[0][..],
    ] {
        let mut data = encoded.to_vec();
        write_string(CHANNEL, &mut data);
        data.push(4);
        let request = Packet::new(4, data);
        let forwarding = forwarding();
        let mut plugins = LoginPlugins::default();
        let response = plugins
            .request(&request, Some(&forwarding))
            .unwrap()
            .unwrap();
        assert_eq!(&response.data[..encoded.len()], encoded);
        assert_eq!(response.data[encoded.len()], 1);
        // The query ID is outside the signed forwarding payload, so changing
        // its sign must not alter the HMAC or the authenticated profile.
        let reference = forwarding.response(0, &[4]).unwrap();
        assert_eq!(&response.data[encoded.len() + 1..], &reference.data[2..]);
        plugins.require_forwarded(Some(&forwarding)).unwrap();
        assert!(plugins.request(&request, Some(&forwarding)).is_err());
        let mut spoof = encoded.to_vec();
        spoof.push(0);
        assert!(
            plugins
                .client_response(&Packet::new(2, spoof.clone()))
                .is_err()
        );

        let mut other = encoded.to_vec();
        write_string("mod:test", &mut other);
        let mut plugins = LoginPlugins::default();
        assert!(
            plugins
                .request(&Packet::new(4, other), None)
                .unwrap()
                .is_none()
        );
        plugins.client_response(&Packet::new(2, spoof)).unwrap();
    }
}

#[test]
fn plugin_queries_are_bounded_single_use_and_proxy_owned() {
    let forwarding = forwarding();
    let mut plugins = LoginPlugins::default();
    assert!(plugins.require_forwarded(Some(&forwarding)).is_err());
    assert!(
        plugins
            .request(&request(1, CHANNEL, &[4]), Some(&forwarding))
            .unwrap()
            .is_some()
    );
    plugins.require_forwarded(Some(&forwarding)).unwrap();
    assert!(
        plugins
            .client_response(&Packet::new(2, vec![1, 1]))
            .is_err()
    );
    assert!(
        plugins
            .request(&request(2, CHANNEL, &[4]), Some(&forwarding))
            .is_err()
    );
    assert!(
        LoginPlugins::default()
            .request(&request(1, CHANNEL, &[4]), None)
            .is_err()
    );
    assert!(
        plugins
            .request(&request(1, "mod:test", &[]), Some(&forwarding))
            .is_err()
    );
    assert!(
        plugins
            .request(&request(-1, "mod:test", &[]), Some(&forwarding))
            .unwrap()
            .is_none()
    );
    assert!(
        plugins
            .request(&Packet::new(4, vec![3, 128]), Some(&forwarding))
            .is_err()
    );
    assert!(
        plugins
            .request(&request(3, "mod:test", &[]), Some(&forwarding))
            .unwrap()
            .is_none()
    );
    plugins
        .client_response(&Packet::new(2, vec![3, 0]))
        .unwrap();
    assert!(
        plugins
            .client_response(&Packet::new(2, vec![3, 0]))
            .is_err()
    );
    assert!(
        plugins
            .client_response(&Packet::new(2, vec![99, 1]))
            .is_err()
    );
    let mut plugins = LoginPlugins::default();
    for id in 0..1024 {
        plugins
            .request(&request(id, "mod:test", &[]), None)
            .unwrap();
    }
    assert!(
        plugins
            .request(&request(1024, "mod:test", &[]), None)
            .is_err()
    );
}

#[test]
fn authenticated_packets_preserve_protocol_suffix_and_signed_properties() {
    for number in [
        47, 107, 340, 393, 404, 477, 578, 735, 754, 758, 759, 760, 761, 762, 763, 764, 766, 767,
        774, 775, 776, 777,
    ] {
        let version = ProtocolVersion::new(number).unwrap();
        let profile = profile();
        for key in [None, Some(profile_key())] {
            let start = login_start(&profile, version, key.as_ref());
            let identity = protocol::start_identity(version, &start).unwrap();
            assert_eq!(identity.name, profile.name);
            // 1.19.1 is the first to carry a UUID in Login Start.
            assert_eq!(
                identity.uuid,
                (number >= 760).then_some(profile.uuid),
                "{number}"
            );
            // Only 1.19–1.19.2 carry the client's chat key.
            assert_eq!(
                protocol::start_profile_key(version, &start).unwrap(),
                key.clone().filter(|_| matches!(number, 759..=760)),
                "{number}"
            );
        }
        let mut backend = Vec::new();
        if number < 735 {
            write_string("00000000-0000-0000-0000-000000000000", &mut backend);
        } else {
            backend.extend([0; 16]);
        }
        write_string("Player", &mut backend);
        if number >= 759 {
            backend.push(0);
        }
        if matches!(number, 766..=767) {
            backend.push(1);
        }
        if number >= 776 {
            backend.extend([9; 16]);
        }
        let success = login_success(&profile, version, &Packet::new(2, backend)).unwrap();
        let identity = protocol::success_identity(version, &success).unwrap();
        assert_eq!(identity.uuid, Some(profile.uuid));
        assert_eq!(identity.name, profile.name);
        if number < 735 {
            let mut bytes = success.data.as_slice();
            assert_eq!(
                read_string(&mut bytes, 36).unwrap(),
                "00010203-0405-0607-0809-0a0b0c0d0e0f"
            );
        }
        if matches!(number, 766..=767) {
            assert_eq!(success.data.last(), Some(&1));
        }
        if number >= 776 {
            assert!(success.data.ends_with(&[9; 16]));
        }
        // Login Success has carried profile properties since 1.19.
        assert_eq!(
            success.data.windows(9).any(|bytes| bytes == b"signature"),
            number >= 759,
            "{number}"
        );
    }
}

/// The forwarding version and the key fields Velocity appends for it.
fn forwarded_key(response: &Packet) -> (i32, Vec<u8>) {
    let mut bytes = &response.data[34..];
    let version = read_varint(&mut bytes).unwrap();
    read_string(&mut bytes, 255).unwrap();
    bytes = &bytes[16..];
    read_string(&mut bytes, 16).unwrap();
    let count = read_varint(&mut bytes).unwrap();
    for _ in 0..count {
        read_string(&mut bytes, 32767).unwrap();
        read_string(&mut bytes, 32767).unwrap();
        if bytes[0] == 1 {
            bytes = &bytes[1..];
            read_string(&mut bytes, 32767).unwrap();
        } else {
            bytes = &bytes[1..];
        }
    }
    (version, bytes.to_vec())
}

#[test]
fn velocity_forwards_1_19_chat_keys_like_velocity() {
    let mut key = Vec::new();
    profile_key().write(&mut key);
    let mut linked = key.clone();
    linked.push(1);
    linked.extend_from_slice(&profile().uuid);
    for (number, has_key, request, version, tail) in [
        // 1.19: a key is forwarded as version 2 to any backend that asks for more than 1.
        (759, true, &[2][..], 2, key.clone()),
        (759, true, &[3][..], 2, key.clone()),
        (759, true, &[1][..], 1, Vec::new()),
        (759, false, &[2][..], 1, Vec::new()),
        // 1.19.1–1.19.2: the linked key also names its owner, version 3 only.
        (760, true, &[3][..], 3, linked.clone()),
        (760, true, &[4][..], 3, linked),
        (760, true, &[2][..], 1, Vec::new()),
        (760, false, &[3][..], 1, Vec::new()),
        // Older and newer versions never forward a key.
        (754, false, &[][..], 1, Vec::new()),
        (754, false, &[4][..], 1, Vec::new()),
        (761, false, &[4][..], 4, Vec::new()),
        (761, false, &[3][..], 1, Vec::new()),
    ] {
        let forwarding = forwarding_for(velocity(), has_key.then(profile_key), number);
        let response = forwarding.response(7, request).unwrap();
        let mut bytes = &response.data[1..];
        let mut verifier = Hmac::<Sha256>::new_from_slice(b"test-forwarding-secret").unwrap();
        assert_eq!(bytes[0], 1);
        bytes = &bytes[1..];
        verifier.update(&bytes[32..]);
        verifier.verify_slice(&bytes[..32]).unwrap();
        assert_eq!(
            forwarded_key(&response),
            (version, tail),
            "{number} {request:?}"
        );
    }
}

fn split_forwarded(address: &str) -> (Vec<&str>, serde_json::Value) {
    let parts: Vec<&str> = address.split('\0').collect();
    let properties = serde_json::from_str(parts.get(3).copied().unwrap_or("[]")).unwrap();
    (parts, properties)
}

#[test]
fn bungeecord_forwarding_rewrites_the_handshake_host() {
    let bungee = forwarding_for(Method::BungeeCord, None, 47);
    assert!(!bungee.velocity());
    assert_eq!(bungee.minimum_protocol(), 0);
    let address = bungee
        .handshake_address("Play.Example.com")
        .unwrap()
        .unwrap();
    let (parts, properties) = split_forwarded(&address);
    // Spigot's layout: host, client address, undashed UUID, property JSON.
    assert_eq!(parts.len(), 4);
    assert_eq!(parts[0], "Play.Example.com");
    assert_eq!(parts[1], "2001:db8::1");
    assert_eq!(parts[2], "000102030405060708090a0b0c0d0e0f");
    assert_eq!(
        properties,
        serde_json::json!([{"name": "textures", "value": "skin-payload", "signature": "signature"}])
    );
    // A login plugin request for Velocity forwarding is a misconfigured backend.
    let mut plugins = LoginPlugins::default();
    let error = plugins
        .request(&request(1, CHANNEL, &[4]), Some(&bungee))
        .unwrap_err();
    assert!(error.to_string().contains("BungeeCord"), "{error}");
    // There is no reply to wait for; the identity check covers a backend that ignored it.
    LoginPlugins::default()
        .require_forwarded(Some(&bungee))
        .unwrap();
    // Velocity forwarding leaves the handshake alone.
    assert_eq!(
        forwarding().handshake_address("play.example.com").unwrap(),
        None
    );
}

#[test]
fn bungeeguard_adds_one_token_and_forge_data_travels_as_properties() {
    let mut profile = profile();
    profile.properties.push(ProfileProperty {
        name: BUNGEEGUARD_PROPERTY.into(),
        value: "spoofed".into(),
        signature: None,
    });
    profile.properties[0].signature = None;
    let forwarding = Forwarding::new(
        Method::BungeeGuard {
            token: Arc::from("guard-token"),
        },
        "203.0.113.9".parse().unwrap(),
        profile,
        None,
        ProtocolVersion::new(340).unwrap(),
    )
    .unwrap();
    let address = forwarding
        .handshake_address("play.example.com\0FML\0")
        .unwrap()
        .unwrap();
    let (parts, properties) = split_forwarded(&address);
    assert_eq!(parts.len(), 4, "{address:?}");
    assert_eq!(parts[0], "play.example.com");
    assert_eq!(parts[1], "203.0.113.9");
    assert_eq!(
        properties,
        serde_json::json!([
            {"name": "textures", "value": "skin-payload"},
            {"name": "forgeClient", "value": "true"},
            {"name": "extraData", "value": "\u{1}FML\u{1}", "signature": ""},
            {"name": "bungeeguard-token", "value": "guard-token", "signature": ""},
        ])
    );
}

#[test]
fn bungeecord_forwarding_refuses_a_host_spigot_cannot_read() {
    let mut profile = profile();
    profile.properties[0].value = "x".repeat(MAX_FORWARDED_HOST);
    let forwarding = Forwarding::new(
        Method::BungeeCord,
        "203.0.113.9".parse().unwrap(),
        profile,
        None,
        ProtocolVersion::new(47).unwrap(),
    )
    .unwrap();
    assert!(forwarding.handshake_address("play.example.com").is_err());
}
