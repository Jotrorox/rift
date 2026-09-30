use super::*;
use crate::protocol::{State, disconnect, read_string};
use aws_lc_rs::rsa::{Pkcs1PublicEncryptingKey, PublicEncryptingKey};
use std::{
    future::poll_fn,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn unhex(value: &str) -> Vec<u8> {
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap())
        .collect()
}

fn profile() -> serde_json::Value {
    serde_json::json!({
        "id": "069a79f444e94726a5befca90e38aaf5",
        "name": "Notch",
        "properties": [{"name": "textures", "value": "skin-value", "signature": "skin-signature"}],
    })
}

#[test]
fn mojang_signed_sha1_matches_java_biginteger() {
    assert_eq!(
        server_hash(b"Notch", b""),
        "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"
    );
    assert_eq!(
        server_hash(b"jeb_", b""),
        "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"
    );
    assert_eq!(
        server_hash(b"simon", b""),
        "88e16a1019277b15d58faf0541e11910eb756f6"
    );
    assert_eq!(signed_hex(&[0, 0]), "0");
    assert_eq!(signed_hex(&[255, 255]), "-1");
    assert_eq!(signed_hex(&[128, 0]), "-8000");
}

#[tokio::test]
async fn aes_cfb8_matches_nist_vector_across_packet_boundaries() {
    // NIST SP 800-38A F.3.7 AES-128 CFB8 example; IV differs from the key here
    // solely to check the transport against this independent published vector.
    let key: [u8; 16] = unhex("2b7e151628aed2a6abf7158809cf4f3c")
        .try_into()
        .unwrap();
    let iv: [u8; 16] = unhex("000102030405060708090a0b0c0d0e0f")
        .try_into()
        .unwrap();
    let plaintext = unhex("6bc1bee22e409f96e93d7e117393172a");
    let ciphertext = unhex("3b79424c9c0dd436bace9e0ed4586a4f");
    let mut stream = CryptoStream::new(Vec::new());
    stream.encryption = Some(cfb8::Encryptor::new(&key.into(), &iv.into()));
    for chunk in plaintext.chunks(3) {
        stream.write_all(chunk).await.unwrap();
    }
    assert_eq!(stream.inner, ciphertext);
    let mut stream = CryptoStream::new(ciphertext.as_slice());
    stream.decryption = Some(cfb8::Decryptor::new(&key.into(), &iv.into()));
    let mut actual = vec![0; plaintext.len()];
    for chunk in actual.chunks_mut(5) {
        stream.read_exact(chunk).await.unwrap();
    }
    assert_eq!(actual, plaintext);
}

#[derive(Default)]
struct ShortWriter {
    bytes: Vec<u8>,
    pending: bool,
    maximum: usize,
}

impl AsyncWrite for ShortWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.pending = true;
        let n = self.maximum.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..n]);
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn cancelled_and_short_writes_do_not_advance_unsent_ciphertext() {
    let secret = [17; 16];
    for (maximum, length) in [(1, 97), (7, 97), (23, 130), (1024, CRYPTO_CHUNK + 300)] {
        let mut stream = CryptoStream::new(ShortWriter {
            pending: true,
            maximum,
            ..Default::default()
        });
        stream.enable_encryption(secret).unwrap();
        // Poll once and cancel; the next write intentionally supplies different
        // data, exposing accidental cipher advancement on Pending.
        let first =
            poll_fn(|cx| Poll::Ready(Pin::new(&mut stream).poll_write(cx, b"cancelled plaintext")))
                .await;
        assert!(first.is_pending());
        let plaintext: Vec<_> = (0..length).map(|n| n as u8).collect();
        stream.write_all(&plaintext).await.unwrap();
        let mut actual = stream.inner.bytes;
        cfb8::Decryptor::<Aes128>::new(&secret.into(), &secret.into()).decrypt(&mut actual);
        assert_eq!(actual, plaintext);
    }
}

#[tokio::test]
async fn encryption_is_continuous_through_compression_and_read_cancellation() {
    let secret = [19; 16];
    let (left, right) = tokio::io::duplex(100_000);
    let mut sender = CryptoStream::new(left);
    let mut receiver = CryptoStream::new(right);
    sender.enable_encryption(secret).unwrap();
    receiver.enable_encryption(secret).unwrap();
    assert!(receiver.enable_encryption(secret).is_err());
    let packet = Packet::new(3, vec![0x55; 900]);
    let codec = Codec::default();
    let encoded = codec.encode(&packet).unwrap();
    sender.write_all(&encoded[..7]).await.unwrap();
    let mut reader = Reader::default();
    assert!(
        tokio::time::timeout(Duration::from_millis(5), reader.read(&mut receiver, codec))
            .await
            .is_err()
    );
    sender.write_all(&encoded[7..]).await.unwrap();
    assert_eq!(
        reader.read(&mut receiver, codec).await.unwrap().unwrap(),
        packet
    );
    let mut codec = codec;
    codec.set_compression(16);
    codec.write(&mut sender, &packet).await.unwrap();
    assert_eq!(
        reader.read(&mut receiver, codec).await.unwrap().unwrap(),
        packet
    );
}

#[test]
fn authenticated_profile_uses_canonical_name_and_signed_properties() {
    let actual = parse_profile(&serde_json::to_vec(&profile()).unwrap(), "notch").unwrap();
    assert_eq!(actual.name, "Notch");
    assert_eq!(
        actual.uuid.to_vec(),
        unhex("069a79f444e94726a5befca90e38aaf5")
    );
    assert_eq!(
        actual.properties[0],
        ProfileProperty {
            name: "textures".into(),
            value: "skin-value".into(),
            signature: Some("skin-signature".into())
        }
    );
    assert!(parse_profile(&serde_json::to_vec(&profile()).unwrap(), "SomeoneElse").is_err());
}

#[test]
fn malformed_profiles_and_unbounded_properties_are_rejected() {
    let mut cases = vec![
        serde_json::json!({}),
        serde_json::json!({"error": "blocked"}),
    ];
    for (field, value) in [
        ("id", serde_json::json!("00000000000000000000000000000000")),
        ("id", serde_json::json!("a".repeat(31))),
        ("id", serde_json::json!("g".repeat(32))),
        ("name", serde_json::json!("Notch\u{0000}")),
        (
            "properties",
            serde_json::json!([{"name":"textures", "value": 4}]),
        ),
        (
            "properties",
            serde_json::json!([{"name":"textures", "value": "x", "signature": null}]),
        ),
        (
            "properties",
            serde_json::json!([{"name":"textures", "value": "x".repeat(MAX_PROPERTY_LENGTH + 1)}]),
        ),
        (
            "properties",
            serde_json::json!(vec![
                serde_json::json!({"name":"textures", "value":"x"});
                MAX_PROPERTIES + 1
            ]),
        ),
    ] {
        let mut value_profile = profile();
        value_profile[field] = value;
        cases.push(value_profile);
    }
    for value in cases {
        assert!(
            parse_profile(&serde_json::to_vec(&value).unwrap(), "Notch").is_err(),
            "{value}"
        );
    }
    assert!(parse_profile(b"{broken", "Notch").is_err());
}

async fn http_fixture(
    status: u16,
    body: String,
    delay: Duration,
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/session/minecraft/hasJoined",
        listener.local_addr().unwrap()
    );
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut byte = [0];
            socket.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
            assert!(request.len() < 8192);
        }
        tokio::time::sleep(delay).await;
        let response = format!(
            "HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        String::from_utf8(request).unwrap()
    });
    (url, task)
}

/// Minimal independent protocol peer using the public key received on the wire.
async fn answer_challenge(
    stream: &mut CryptoStream<tokio::io::DuplexStream>,
    version: i32,
    token_valid: bool,
    secret_length: usize,
    trailing: bool,
) -> ([u8; 16], Vec<u8>) {
    let packet = Reader::default()
        .read(stream, Codec::default())
        .await
        .unwrap()
        .unwrap();
    let (packet, secret, public_key) =
        challenge_response(packet, version, token_valid, secret_length, trailing);
    Codec::default().write(stream, &packet).await.unwrap();
    (secret, public_key)
}

fn challenge_response(
    packet: Packet,
    version: i32,
    token_valid: bool,
    secret_length: usize,
    trailing: bool,
) -> (Packet, [u8; 16], Vec<u8>) {
    assert_eq!(packet.id, 1);
    let mut data = packet.data.as_slice();
    assert_eq!(read_string(&mut data, 20).unwrap(), "");
    let size = read_varint(&mut data).unwrap() as usize;
    let public_key = data[..size].to_vec();
    data = &data[size..];
    let size = read_varint(&mut data).unwrap() as usize;
    let mut token = data[..size].to_vec();
    data = &data[size..];
    assert_eq!(data, if version >= 766 { &[1][..] } else { &[][..] });
    if !token_valid {
        token[0] ^= 1;
    }
    let public =
        Pkcs1PublicEncryptingKey::new(PublicEncryptingKey::from_der(&public_key).unwrap()).unwrap();
    let secret = [0x37; 16];
    let mut data = Vec::new();
    for plaintext in [&vec![0x37; secret_length][..], &token] {
        let mut ciphertext = vec![0; public.ciphertext_size()];
        let ciphertext = public.encrypt(plaintext, &mut ciphertext).unwrap();
        write_bytes(ciphertext, &mut data);
    }
    if trailing {
        data.push(0);
    }
    (Packet::new(1, data), secret, public_key)
}

#[tokio::test]
async fn captured_encryption_response_cannot_replay_an_authenticated_session() {
    let (url, http) = http_fixture(200, profile().to_string(), Duration::ZERO).await;
    let auth = Arc::new(Authenticator::for_test_session_server(
        url,
        Duration::from_secs(3),
    ));
    let mut captured = None;
    for replay in [false, true] {
        let (left, mut right) = tokio::io::duplex(4096);
        let auth = auth.clone();
        let task = tokio::spawn(async move {
            auth.authenticate(
                &mut CryptoStream::new(left),
                ProtocolVersion::new(774).unwrap(),
                "Notch",
            )
            .await
        });
        let request = Reader::default()
            .read(&mut right, Codec::default())
            .await
            .unwrap()
            .unwrap();
        if !replay {
            captured = Some(challenge_response(request, 774, true, 16, false).0);
        }
        Codec::default()
            .write(&mut right, captured.as_ref().unwrap())
            .await
            .unwrap();
        let result = task.await.unwrap();
        if replay {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        } else {
            assert_eq!(result.unwrap().name, "Notch");
        }
    }
    http.await.unwrap();
}

#[tokio::test]
async fn online_handshake_authenticates_profile_and_preserves_encrypted_pipelined_bytes() {
    for version in [47, 761, 765, 766, 774, 775, 776, 777] {
        let (url, http) = http_fixture(200, profile().to_string(), Duration::ZERO).await;
        let auth = Authenticator::for_test_session_server(url, Duration::from_secs(3));
        let (left, right) = tokio::io::duplex(4096);
        let mut server = CryptoStream::new(left);
        let mut client = CryptoStream::new(right);
        let task = tokio::spawn(async move {
            let result = auth
                .authenticate(&mut server, ProtocolVersion::new(version).unwrap(), "Notch")
                .await
                .unwrap();
            let packet = Reader::default()
                .read(&mut server, Codec::default())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(packet, Packet::new(2, vec![12, 34]));
            Codec::default().write(&mut server, &packet).await.unwrap();
            result
        });
        let (secret, public_key) = answer_challenge(&mut client, version, true, 16, false).await;
        client.enable_encryption(secret).unwrap();
        Codec::default()
            .write(&mut client, &Packet::new(2, vec![12, 34]))
            .await
            .unwrap();
        let packet = Reader::default()
            .read(&mut client, Codec::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(packet, Packet::new(2, vec![12, 34]));
        assert_eq!(
            task.await.unwrap(),
            parse_profile(&serde_json::to_vec(&profile()).unwrap(), "Notch").unwrap()
        );
        let request = http.await.unwrap();
        assert!(request.starts_with(&format!(
            "GET /session/minecraft/hasJoined?username=Notch&serverId={} HTTP/1.1",
            server_hash(&secret, &public_key)
        )));
    }
}

#[tokio::test]
async fn invalid_challenge_secret_and_packet_do_not_contact_authentication_service() {
    let connections = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let count = connections.clone();
    let http = tokio::spawn(async move {
        while listener.accept().await.is_ok() {
            count.fetch_add(1, Ordering::SeqCst);
        }
    });
    let auth = Arc::new(Authenticator::for_test_session_server(
        url,
        Duration::from_millis(100),
    ));
    for (token_valid, length, trailing) in [
        (false, 16, false),
        (true, 15, false),
        (true, 17, false),
        (true, 16, true),
    ] {
        let (left, right) = tokio::io::duplex(4096);
        let auth = auth.clone();
        let task = tokio::spawn(async move {
            let mut server = CryptoStream::new(left);
            let err = auth
                .authenticate(&mut server, ProtocolVersion::new(774).unwrap(), "Notch")
                .await
                .unwrap_err();
            assert!(!server.is_encrypted());
            err
        });
        answer_challenge(
            &mut CryptoStream::new(right),
            774,
            token_valid,
            length,
            trailing,
        )
        .await;
        assert_eq!(task.await.unwrap().kind(), io::ErrorKind::PermissionDenied);
    }
    assert_eq!(connections.load(Ordering::SeqCst), 0);
    http.abort();
}

#[tokio::test]
async fn session_rejection_outage_and_mismatch_fail_closed_after_encryption() {
    for (status, body, kind) in [
        (204, "".into(), io::ErrorKind::PermissionDenied),
        (403, "".into(), io::ErrorKind::PermissionDenied),
        (429, "rate limited".into(), io::ErrorKind::ConnectionRefused),
        (503, "unavailable".into(), io::ErrorKind::ConnectionRefused),
        (302, "".into(), io::ErrorKind::ConnectionRefused),
        (200, "{".into(), io::ErrorKind::ConnectionRefused),
        (
            200,
            "x".repeat(MAX_SESSION_BODY + 1),
            io::ErrorKind::ConnectionRefused,
        ),
        (
            200,
            {
                let mut value = profile();
                value["name"] = serde_json::json!("Impostor");
                value.to_string()
            },
            io::ErrorKind::PermissionDenied,
        ),
    ] {
        let (url, http) = http_fixture(status, body, Duration::ZERO).await;
        let auth = Authenticator::for_test_session_server(url, Duration::from_secs(3));
        let (left, right) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            let mut server = CryptoStream::new(left);
            let version = ProtocolVersion::new(774).unwrap();
            let err = auth
                .authenticate(&mut server, version, "Notch")
                .await
                .unwrap_err();
            assert!(server.is_encrypted());
            Codec::default()
                .write(
                    &mut server,
                    &disconnect(Some(version), State::Login, &err.to_string()).unwrap(),
                )
                .await
                .unwrap();
            err.kind()
        });
        let mut client = CryptoStream::new(right);
        let (secret, _) = answer_challenge(&mut client, 774, true, 16, false).await;
        client.enable_encryption(secret).unwrap();
        let packet = Reader::default()
            .read(&mut client, Codec::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(packet.id, 0);
        assert_eq!(task.await.unwrap(), kind);
        http.await.unwrap();
    }
}

#[tokio::test]
async fn authentication_service_timeout_is_bounded_and_does_not_fallback() {
    let (url, http) = http_fixture(200, profile().to_string(), Duration::from_millis(200)).await;
    let auth = Authenticator::for_test_session_server(url, Duration::from_millis(30));
    let error = auth.has_joined("Notch", "hash").await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    assert!(
        error
            .to_string()
            .contains("authentication servers are unavailable")
    );
    http.await.unwrap();
}

#[tokio::test]
async fn session_body_without_content_length_is_bounded_and_timed() {
    for slow in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let http = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await
                .unwrap();
            if slow {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            // No Content-Length: the streaming cap must also enforce the limit.
            let body = "x".repeat(MAX_SESSION_BODY + 1);
            let _ = stream
                .write_all(format!("{:x}\r\n{}\r\n0\r\n\r\n", body.len(), body).as_bytes())
                .await;
        });
        let auth = Authenticator::for_test_session_server(
            url,
            if slow {
                Duration::from_millis(30)
            } else {
                Duration::from_secs(3)
            },
        );
        let error = auth.has_joined("Notch", "hash").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        http.await.unwrap();
    }
}

#[tokio::test]
async fn malformed_and_unexpected_encryption_response_frames_are_bounded() {
    let auth = Arc::new(Authenticator::new(Duration::from_millis(100)).unwrap());
    for bytes in [
        vec![0],                // empty frame
        vec![0xff, 0xff, 0xff], // VarInt21 violation
        vec![0x81, 0x08],       // 1025-byte frame, no body needed for rejection
        Codec::default().encode(&Packet::empty(0)).unwrap(),
        Codec::default().encode(&Packet::new(1, vec![0])).unwrap(),
    ] {
        let (left, mut right) = tokio::io::duplex(4096);
        let auth = auth.clone();
        let task = tokio::spawn(async move {
            auth.authenticate(
                &mut CryptoStream::new(left),
                ProtocolVersion::new(774).unwrap(),
                "Notch",
            )
            .await
        });
        Reader::default()
            .read(&mut right, Codec::default())
            .await
            .unwrap()
            .unwrap();
        right.write_all(&bytes).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
}
