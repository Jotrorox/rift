use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};

// Bound allocation before reading the body. A vanilla address is at most 255
// UTF-16 code units and 1020 UTF-8 bytes; leave room for the packet fields.
pub const MAX_PACKET_SIZE: usize = 2048;
const MAX_ADDRESS_BYTES: usize = 255 * 4;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn varint(bytes: &mut &[u8]) -> io::Result<u32> {
    let mut value = 0;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or_else(|| invalid("short VarInt"))?;
        *bytes = rest;
        if shift == 28 && byte > 15 {
            return Err(invalid("invalid VarInt"));
        }
        value |= u32::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    Err(invalid("invalid VarInt"))
}

fn hostname(mut body: &[u8]) -> io::Result<String> {
    if varint(&mut body)? != 0 {
        return Err(invalid("expected handshake packet"));
    }
    varint(&mut body)?; // Protocol version, including -1 for status discovery.
    let length = varint(&mut body)? as usize;
    if length == 0 || length > MAX_ADDRESS_BYTES || length > body.len() {
        return Err(invalid("invalid handshake address length"));
    }
    let address = std::str::from_utf8(&body[..length])
        .map_err(|_| invalid("handshake address is not UTF-8"))?;
    if address.encode_utf16().count() > 255 {
        return Err(invalid("handshake address exceeds 255 characters"));
    }
    body = &body[length..];
    body = body
        .get(2..)
        .ok_or_else(|| invalid("missing handshake port"))?;
    if !matches!(varint(&mut body)?, 1..=3) || !body.is_empty() {
        return Err(invalid("invalid handshake state or trailing data"));
    }
    // Modded clients can append NUL-delimited metadata. Only the hostname is
    // used for routing; the complete original packet is forwarded unchanged.
    let host = address.split('\0').next().unwrap();
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    if host.is_empty() {
        return Err(invalid("empty handshake hostname"));
    }
    Ok(host)
}

/// Read precisely one frame without consuming any pipelined login/status data.
/// The caller applies a single deadline across the length prefix and body.
pub async fn read(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<(String, Vec<u8>)> {
    let mut packet = Vec::with_capacity(5);
    loop {
        let byte = reader.read_u8().await?;
        packet.push(byte);
        if packet.len() == 5 && byte > 15 {
            return Err(invalid("invalid handshake length VarInt"));
        }
        if byte & 128 == 0 {
            break;
        }
    }
    let size = varint(&mut packet.as_slice())? as usize;
    if size == 0 || size > MAX_PACKET_SIZE {
        return Err(invalid("handshake packet exceeds size limit"));
    }
    let prefix = packet.len();
    packet.resize(prefix + size, 0);
    reader.read_exact(&mut packet[prefix..]).await?;
    Ok((hostname(&packet[prefix..])?, packet))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn encode(mut value: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        while value > 127 {
            bytes.push((value as u8 & 127) | 128);
            value >>= 7;
        }
        bytes.push(value as u8);
        bytes
    }

    fn body(address: &str, state: u8) -> Vec<u8> {
        let mut bytes = vec![0, 0xff, 0xff, 0xff, 0xff, 0x0f]; // Version -1.
        bytes.extend(encode(address.len()));
        bytes.extend(address.as_bytes());
        bytes.extend([0x63, 0xdd, state]);
        bytes
    }

    #[tokio::test]
    async fn fragmented_handshake_is_preserved_and_leaves_following_packets_unread() {
        for state in 1..=3 {
            let body = body("PLAY.Example.COM.\0FML3\0", state);
            // Legal non-minimal encoding: forwarding must preserve the prefix too.
            let mut packet = vec![body.len() as u8 | 128, 0];
            packet.extend(body);
            let expected = packet.clone();
            let (mut writer, mut reader) = tokio::io::duplex(1);
            let task = tokio::spawn(async move {
                for byte in packet {
                    writer.write_all(&[byte]).await.unwrap();
                }
                writer.write_all(b"next packet").await.unwrap();
            });
            let (host, forwarded) = read(&mut reader).await.unwrap();
            assert_eq!(host, "play.example.com");
            assert_eq!(forwarded, expected);
            let mut tail = Vec::new();
            reader.read_to_end(&mut tail).await.unwrap();
            assert_eq!(tail, b"next packet");
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn invalid_and_oversized_frames_fail_before_reading_a_body() {
        for bytes in [
            vec![0],
            encode(MAX_PACKET_SIZE + 1),
            vec![0xff, 0xff, 0xff, 0xff, 0x0f],
            vec![0x80; 5],
            vec![0xff, 0xff, 0xff, 0xff, 0x10],
        ] {
            let (mut writer, mut reader) = tokio::io::duplex(32);
            writer.write_all(&bytes).await.unwrap();
            let error = tokio::time::timeout(std::time::Duration::from_secs(1), read(&mut reader))
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[tokio::test]
    async fn truncated_frames_fail_at_every_boundary() {
        let body = body("example.com", 2);
        let mut packet = encode(body.len());
        packet.extend(body);
        for length in 0..packet.len() {
            assert!(
                read(&mut &packet[..length]).await.is_err(),
                "length {length}"
            );
        }
    }

    #[test]
    fn validates_all_fields_and_address_limits() {
        assert!(hostname(&body(&"a".repeat(255), 1)).is_ok());
        assert!(hostname(&body(&"é".repeat(255), 1)).is_ok());
        assert!(hostname(&body(&"😀".repeat(128), 1)).is_err());
        for address in [
            "".to_owned(),
            "a".repeat(256),
            "a".repeat(1021),
            "\0FML".to_owned(),
        ] {
            assert!(hostname(&body(&address, 1)).is_err());
        }
        for state in [0, 4, 127, 128, 255] {
            assert!(hostname(&body("example.com", state)).is_err());
        }
        let valid = body("example.com", 1);
        let mut wrong_id = valid.clone();
        wrong_id[0] = 1;
        let mut trailing = valid.clone();
        trailing.push(0);
        let mut invalid_utf8 = valid.clone();
        invalid_utf8[7] = 0xff;
        let mut negative_length = vec![0, 1];
        negative_length.extend([0xff, 0xff, 0xff, 0xff, 0x0f]);
        for bytes in [
            wrong_id,
            trailing,
            invalid_utf8,
            negative_length,
            vec![0, 0x80, 0x80, 0x80, 0x80, 0x80],
        ] {
            assert!(hostname(&bytes).is_err());
        }
    }
}
