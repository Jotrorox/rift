use super::*;

const MINECRAFT: &[u8] = b"\x10\x00minecraft";

async fn parse(header: &[u8]) -> (io::Result<Option<SocketAddr>>, Vec<u8>) {
    let input = [header, MINECRAFT].concat();
    let mut stream = input.as_slice();
    let result = read_header(&mut stream).await;
    (result, stream.to_vec())
}

fn v2_header(command: u8, family: u8, body: &[u8]) -> Vec<u8> {
    let mut header = V2_SIGNATURE.to_vec();
    header.extend([0x20 | command, family]);
    header.extend((body.len() as u16).to_be_bytes());
    header.extend(body);
    header
}

#[tokio::test]
async fn v1_reads_tcp4_and_tcp6_sources_without_consuming_client_bytes() {
    for (header, expected) in [
        (
            &b"PROXY TCP4 203.0.113.7 192.0.2.1 51234 25565\r\n"[..],
            "203.0.113.7:51234",
        ),
        (
            b"PROXY TCP6 2001:db8::7 2001:db8::1 40000 25565\r\n",
            "[2001:db8::7]:40000",
        ),
    ] {
        let (result, rest) = parse(header).await;
        assert_eq!(result.unwrap(), Some(expected.parse().unwrap()));
        assert_eq!(rest, MINECRAFT);
    }
}

#[tokio::test]
async fn v1_unknown_keeps_the_socket_peer() {
    for header in [
        &b"PROXY UNKNOWN\r\n"[..],
        b"PROXY UNKNOWN ff::1 ff::2 1 2\r\n",
    ] {
        let (result, rest) = parse(header).await;
        assert_eq!(result.unwrap(), None);
        assert_eq!(rest, MINECRAFT);
    }
}

#[tokio::test]
async fn v2_reads_ipv4_and_ipv6_sources_and_skips_tlvs() {
    let mut ipv4 = vec![203, 0, 113, 7, 192, 0, 2, 1];
    ipv4.extend(51234u16.to_be_bytes());
    ipv4.extend(25565u16.to_be_bytes());
    ipv4.extend([0x04, 0x00, 0x02, b'o', b'k']); // A NOOP TLV.
    let (result, rest) = parse(&v2_header(1, 0x11, &ipv4)).await;
    assert_eq!(result.unwrap(), Some("203.0.113.7:51234".parse().unwrap()));
    assert_eq!(rest, MINECRAFT);

    let mut ipv6 = "2001:db8::7".parse::<Ipv6Addr>().unwrap().octets().to_vec();
    ipv6.extend("2001:db8::1".parse::<Ipv6Addr>().unwrap().octets());
    ipv6.extend(40000u16.to_be_bytes());
    ipv6.extend(25565u16.to_be_bytes());
    let (result, rest) = parse(&v2_header(1, 0x21, &ipv6)).await;
    assert_eq!(
        result.unwrap(),
        Some("[2001:db8::7]:40000".parse().unwrap())
    );
    assert_eq!(rest, MINECRAFT);
}

#[tokio::test]
async fn v2_local_and_unspecified_keep_the_socket_peer() {
    for header in [
        v2_header(0, 0x11, &[0; 12]),
        v2_header(0, 0x00, &[]),
        v2_header(1, 0x00, &[]),
        v2_header(1, 0x31, &[0; 216]),
    ] {
        let (result, rest) = parse(&header).await;
        assert_eq!(result.unwrap(), None);
        assert_eq!(rest, MINECRAFT);
    }
}

#[tokio::test]
async fn malformed_or_missing_headers_are_rejected() {
    let mut wrong_version = v2_header(1, 0x11, &[0; 12]);
    wrong_version[12] = 0x11;
    let too_long = format!("PROXY TCP4 {}\r\n", "1".repeat(100));
    for header in [
        // A Minecraft client connecting directly to a PROXY listener.
        &b"\x10\x00\xff\x05\x09localhost"[..],
        b"PROXY TCP4 2001:db8::7 192.0.2.1 1 2\r\n",
        b"PROXY TCP4 203.0.113.7 192.0.2.1 70000 2\r\n",
        b"PROXY TCP4 203.0.113.7 192.0.2.1 1\r\n",
        b"PROXY TCP5 203.0.113.7 192.0.2.1 1 2\r\n",
        too_long.as_bytes(),
        &v2_header(1, 0x11, &[0; 8]),
        &v2_header(1, 0x12, &[0; 12]),
        &v2_header(2, 0x11, &[0; 12]),
        &wrong_version,
    ] {
        let (result, _) = parse(header).await;
        assert_eq!(
            result.unwrap_err().kind(),
            ErrorKind::InvalidData,
            "{header:?}"
        );
    }
    let mut truncated: &[u8] = b"PROXY TCP4 203.0.113.7";
    assert_eq!(
        read_header(&mut truncated).await.unwrap_err().kind(),
        ErrorKind::UnexpectedEof
    );
}

#[test]
fn networks_match_addresses_and_ipv4_mapped_peers() {
    let network: Network = "10.0.0.0/8".parse().unwrap();
    assert!(network.contains("10.255.0.1".parse().unwrap()));
    assert!(network.contains("::ffff:10.1.2.3".parse().unwrap()));
    assert!(!network.contains("11.0.0.1".parse().unwrap()));
    assert!(!network.contains("::a00:1".parse().unwrap()));

    let host: Network = "192.0.2.10".parse().unwrap();
    assert!(host.contains("192.0.2.10".parse().unwrap()));
    assert!(!host.contains("192.0.2.11".parse().unwrap()));
    assert_eq!(host.to_string(), "192.0.2.10/32");

    let ipv6: Network = "2001:db8::/32".parse().unwrap();
    assert!(ipv6.contains("2001:db8:ffff::1".parse().unwrap()));
    assert!(!ipv6.contains("2001:db9::1".parse().unwrap()));

    let everything: Network = "::/0".parse().unwrap();
    assert!(everything.contains("2001:db8::1".parse().unwrap()));
    assert!(!everything.contains("192.0.2.1".parse().unwrap()));
    assert!(
        "0.0.0.0/0"
            .parse::<Network>()
            .unwrap()
            .contains("192.0.2.1".parse().unwrap())
    );
}

#[test]
fn networks_reject_ambiguous_notation() {
    for value in [
        "10.0.0.1/8",
        "10.0.0.0/33",
        "::/129",
        "10.0.0.0/",
        "10.0.0.0/+8",
        "example.com",
        "",
    ] {
        assert!(value.parse::<Network>().is_err(), "{value}");
    }
}
