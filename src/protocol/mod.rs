//! Bounded Minecraft Java framing and the control packets used by a session.
//! Gameplay payloads remain opaque; this is not a protocol translator.
mod compression;
mod framing;
mod packets;
mod state;

pub use framing::{Codec, MAX_FRAME_SIZE, MAX_PACKET_SIZE, Packet, Reader};
pub(crate) use packets::login_identity;
pub use packets::{Handshake, NextState, PacketKind, ProtocolVersion, disconnect, status_response};
pub use state::{ConnectionState, Direction, State};
use std::io;

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn read_varint(bytes: &mut &[u8]) -> io::Result<i32> {
    let mut value = 0u32;
    for shift in (0..35).step_by(7) {
        let (&byte, rest) = bytes.split_first().ok_or_else(|| invalid("short VarInt"))?;
        *bytes = rest;
        if shift == 28 && byte > 15 {
            return Err(invalid("invalid VarInt"));
        }
        value |= u32::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value as i32);
        }
    }
    Err(invalid("invalid VarInt"))
}

pub fn write_varint(value: i32, bytes: &mut Vec<u8>) {
    let mut value = value as u32;
    while value > 127 {
        bytes.push(value as u8 & 127 | 128);
        value >>= 7;
    }
    bytes.push(value as u8);
}

pub fn read_string<'a>(bytes: &mut &'a [u8], max: usize) -> io::Result<&'a str> {
    let length = read_varint(bytes)?;
    if length < 0 || length as usize > max.saturating_mul(4) || length as usize > bytes.len() {
        return Err(invalid("invalid string length"));
    }
    let (value, rest) = bytes.split_at(length as usize);
    let value = std::str::from_utf8(value).map_err(|_| invalid("string is not UTF-8"))?;
    if value.encode_utf16().count() > max {
        return Err(invalid("string exceeds character limit"));
    }
    *bytes = rest;
    Ok(value)
}

pub fn write_string(value: &str, bytes: &mut Vec<u8>) {
    write_varint(value.len() as i32, bytes);
    bytes.extend_from_slice(value.as_bytes());
}

#[cfg(test)]
mod tests;
