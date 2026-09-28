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

fn forwarding() -> Forwarding {
    Forwarding::new(
        Arc::from(&b"test-forwarding-secret"[..]),
        "2001:db8::1".parse().unwrap(),
        profile(),
    )
    .unwrap()
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
    assert!(Forwarding::new(Arc::from(&b""[..]), "127.0.0.1".parse().unwrap(), profile()).is_err());
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
        plugins.require_forwarded(true).unwrap();
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
    assert!(plugins.require_forwarded(true).is_err());
    assert!(
        plugins
            .request(&request(1, CHANNEL, &[4]), Some(&forwarding))
            .unwrap()
            .is_some()
    );
    plugins.require_forwarded(true).unwrap();
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
    for number in [47, 761, 762, 763, 764, 766, 767, 774, 777] {
        let version = ProtocolVersion::new(number).unwrap();
        let profile = profile();
        let start = login_start(&profile, version);
        let identity = protocol::start_identity(version, &start).unwrap();
        assert_eq!(identity.name, profile.name);
        assert_eq!(
            identity.uuid,
            if number == 47 {
                None
            } else {
                Some(profile.uuid)
            }
        );
        let mut backend = Vec::new();
        if number == 47 {
            write_string("00000000-0000-0000-0000-000000000000", &mut backend);
        } else {
            backend.extend([0; 16]);
        }
        write_string("Player", &mut backend);
        if number >= 761 {
            backend.push(0);
        }
        if matches!(number, 766..=767) {
            backend.push(1);
        }
        if number >= 777 {
            backend.extend([9; 16]);
        }
        let success = login_success(&profile, version, &Packet::new(2, backend)).unwrap();
        let identity = protocol::success_identity(version, &success).unwrap();
        assert_eq!(identity.uuid, Some(profile.uuid));
        assert_eq!(identity.name, profile.name);
        if matches!(number, 766..=767) {
            assert_eq!(success.data.last(), Some(&1));
        }
        if number >= 777 {
            assert!(success.data.ends_with(&[9; 16]));
        }
        if number >= 761 {
            assert!(success.data.windows(9).any(|bytes| bytes == b"signature"));
        }
    }
}
