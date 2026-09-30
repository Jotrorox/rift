//! BungeeCord's Java DataInput/DataOutput plugin-message envelope.
//! Channel framing uses Minecraft strings; the inner fields use modified UTF-8.
use crate::protocol::{self, Direction, Packet, ProtocolVersion, State};
use std::io;

/// The legacy serverbound custom-payload limit, retained across all versions.
pub const MAX_PAYLOAD_SIZE: usize = 32767;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Connect(String),
    ConnectOther { player: String, server: String },
    Ip,
    IpOther(String),
    PlayerCount(String),
    PlayerList(String),
    GetServers,
    GetServer,
    GetPlayerServer(String),
    Uuid,
    UuidOther(String),
    ServerIp(String),
}

/// Unknown subchannels have no effect. Invalid known requests are rejected by
/// the codec, and the session consumes them without closing the connection.
pub fn decode(mut bytes: &[u8]) -> io::Result<Option<Request>> {
    if bytes.len() > MAX_PAYLOAD_SIZE {
        return Err(protocol::invalid("BungeeCord payload exceeds size limit"));
    }
    let request = match read_utf(&mut bytes)?.as_str() {
        "Connect" => Request::Connect(read_utf(&mut bytes)?),
        "ConnectOther" => Request::ConnectOther {
            player: read_utf(&mut bytes)?,
            server: read_utf(&mut bytes)?,
        },
        "IP" => Request::Ip,
        "IPOther" => Request::IpOther(read_utf(&mut bytes)?),
        "PlayerCount" => Request::PlayerCount(read_utf(&mut bytes)?),
        "PlayerList" => Request::PlayerList(read_utf(&mut bytes)?),
        "GetServers" => Request::GetServers,
        "GetServer" => Request::GetServer,
        "GetPlayerServer" => Request::GetPlayerServer(read_utf(&mut bytes)?),
        "UUID" => Request::Uuid,
        "UUIDOther" => Request::UuidOther(read_utf(&mut bytes)?),
        "ServerIP" => Request::ServerIp(read_utf(&mut bytes)?),
        _ => return Ok(None),
    };
    if !bytes.is_empty() {
        return Err(protocol::invalid("trailing BungeeCord request data"));
    }
    Ok(Some(request))
}

/// Read Java DataInput.readUTF, including NUL and UTF-16 surrogate pairs.
pub fn read_utf(bytes: &mut &[u8]) -> io::Result<String> {
    let length = bytes
        .get(..2)
        .ok_or_else(|| protocol::invalid("missing modified UTF-8 length"))?;
    let length = u16::from_be_bytes(length.try_into().unwrap()) as usize;
    let text = bytes
        .get(2..2 + length)
        .ok_or_else(|| protocol::invalid("short modified UTF-8 string"))?;
    let mut units = Vec::with_capacity(length);
    let mut position = 0;
    while position < text.len() {
        let first = text[position];
        position += 1;
        let unit = match first {
            0..=0x7f => u16::from(first),
            0xc0..=0xdf => {
                let next = continuation(text, &mut position)?;
                (u16::from(first & 31) << 6) | u16::from(next)
            }
            0xe0..=0xef => {
                let middle = continuation(text, &mut position)?;
                let last = continuation(text, &mut position)?;
                (u16::from(first & 15) << 12) | (u16::from(middle) << 6) | u16::from(last)
            }
            _ => return Err(protocol::invalid("invalid modified UTF-8 leading byte")),
        };
        units.push(unit);
    }
    let value = String::from_utf16(&units)
        .map_err(|_| protocol::invalid("unpaired modified UTF-8 surrogate"))?;
    *bytes = &bytes[2 + length..];
    Ok(value)
}

fn continuation(bytes: &[u8], position: &mut usize) -> io::Result<u8> {
    let value = bytes
        .get(*position)
        .copied()
        .filter(|value| value & 0xc0 == 0x80)
        .ok_or_else(|| protocol::invalid("invalid modified UTF-8 continuation"))?;
    *position += 1;
    Ok(value & 63)
}

/// Write Java DataOutput.writeUTF. Reject overlong fields before changing output.
pub fn write_utf(value: &str, bytes: &mut Vec<u8>) -> io::Result<()> {
    let length = value
        .encode_utf16()
        .map(|unit| match unit {
            1..=0x7f => 1usize,
            0..=0x7ff => 2,
            _ => 3,
        })
        .sum::<usize>();
    let length = u16::try_from(length)
        .map_err(|_| protocol::invalid("modified UTF-8 string exceeds size limit"))?;
    bytes.extend_from_slice(&length.to_be_bytes());
    for unit in value.encode_utf16() {
        match unit {
            1..=0x7f => bytes.push(unit as u8),
            0..=0x7ff => bytes.extend([0xc0 | (unit >> 6) as u8, 0x80 | (unit & 63) as u8]),
            _ => bytes.extend([
                0xe0 | (unit >> 12) as u8,
                0x80 | ((unit >> 6) & 63) as u8,
                0x80 | (unit & 63) as u8,
            ]),
        }
    }
    Ok(())
}

pub(crate) fn channel(version: ProtocolVersion) -> &'static str {
    if version.number() < 393 {
        "BungeeCord"
    } else {
        "bungeecord:main"
    }
}

pub(crate) fn payload(
    version: ProtocolVersion,
    state: State,
    direction: Direction,
    packet: &Packet,
) -> Option<&[u8]> {
    if version.custom_payload_id(state, direction) != Some(packet.id) {
        return None;
    }
    let mut bytes = packet.data.as_slice();
    match protocol::read_string(&mut bytes, 32767).ok()? {
        "BungeeCord" | "bungeecord:main" => Some(bytes),
        _ => None,
    }
}

pub(crate) fn packet(version: ProtocolVersion, channel: &str, payload: &[u8]) -> Packet {
    let mut data = Vec::new();
    protocol::write_string(channel, &mut data);
    data.extend_from_slice(payload);
    Packet::new(
        version
            .custom_payload_id(State::Play, Direction::Serverbound)
            .unwrap(),
        data,
    )
}

pub(crate) fn registration(version: ProtocolVersion) -> Packet {
    packet(
        version,
        if version.number() < 393 {
            "REGISTER"
        } else {
            "minecraft:register"
        },
        channel(version).as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(values: &[&str]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for value in values {
            write_utf(value, &mut bytes).unwrap();
        }
        bytes
    }

    #[test]
    fn decodes_every_supported_request() {
        for (values, expected) in [
            (vec!["Connect", "lobby"], Request::Connect("lobby".into())),
            (
                vec!["ConnectOther", "Player", "lobby"],
                Request::ConnectOther {
                    player: "Player".into(),
                    server: "lobby".into(),
                },
            ),
            (vec!["IP"], Request::Ip),
            (vec!["IPOther", "Player"], Request::IpOther("Player".into())),
            (
                vec!["PlayerCount", "ALL"],
                Request::PlayerCount("ALL".into()),
            ),
            (
                vec!["PlayerList", "lobby"],
                Request::PlayerList("lobby".into()),
            ),
            (vec!["GetServers"], Request::GetServers),
            (vec!["GetServer"], Request::GetServer),
            (
                vec!["GetPlayerServer", "Player"],
                Request::GetPlayerServer("Player".into()),
            ),
            (vec!["UUID"], Request::Uuid),
            (
                vec!["UUIDOther", "Player"],
                Request::UuidOther("Player".into()),
            ),
            (vec!["ServerIP", "lobby"], Request::ServerIp("lobby".into())),
        ] {
            assert_eq!(decode(&fields(&values)).unwrap(), Some(expected));
        }
    }

    #[test]
    fn modified_utf_matches_java_wire_bytes_and_preserves_trailing_fields() {
        let expected = [
            0, 12, b'A', 0xc0, 0x80, 0xe2, 0x82, 0xac, 0xed, 0xa0, 0xbd, 0xed, 0xb8, 0x80,
        ];
        assert_eq!(fields(&["A\0€😀"]), expected);
        let encoded = fields(&["A\0€😀", "next"]);
        let mut bytes = encoded.as_slice();
        assert_eq!(read_utf(&mut bytes).unwrap(), "A\0€😀");
        assert_eq!(read_utf(&mut bytes).unwrap(), "next");
        assert!(bytes.is_empty());
    }

    #[test]
    fn malformed_and_oversize_payloads_fail_without_panicking() {
        for invalid in [
            vec![],
            vec![0],
            vec![0, 1],
            vec![0, 1, 0x80],
            vec![0, 2, 0xc0, 0x41],
            vec![0, 3, 0xed, 0xa0, 0x80],
            vec![0, 4, 0xf0, 0x9f, 0x98, 0x80],
        ] {
            assert!(read_utf(&mut invalid.as_slice()).is_err());
        }
        assert!(decode(&fields(&["Connect"])).is_err());
        assert!(decode(&fields(&["IP", "unexpected"])).is_err());
        assert!(decode(&vec![0; MAX_PAYLOAD_SIZE + 1]).is_err());
        assert_eq!(decode(&fields(&["Unknown", "opaque"])).unwrap(), None);
        let mut bytes = vec![42];
        assert!(write_utf(&"x".repeat(65536), &mut bytes).is_err());
        assert_eq!(bytes, [42]);
    }
}
