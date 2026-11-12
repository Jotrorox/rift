//! The frame codec of `docs/messaging-protocol.md`, shared by [`Client`] and
//! Rift's server. Applications use [`Client`] instead.
//!
//! [`Client`]: crate::Client

use std::{io, sync::Arc, time::Duration};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use quinn::{RecvStream, SendStream};
use tokio::time::timeout;

use crate::{MAX_FRAME_BYTES, MAX_PAYLOAD_BYTES, Message, Result};

/// Longest encoded string, including tokens.
pub const MAX_STRING: usize = 4096;
/// Most deliveries one CONSUMER_FETCH may return.
pub const MAX_FETCH: usize = 256;

pub const AUTH: u8 = 1;
pub const OK: u8 = 2;
pub const ERROR: u8 = 3;
pub const PUBLISH: u8 = 4;
pub const PUBLISH_ACK: u8 = 5;
pub const SUBSCRIBE: u8 = 6;
pub const MESSAGE: u8 = 7;
pub const PING: u8 = 8;
pub const REQUEST: u8 = 9;
pub const STREAM_PUBLISH: u8 = 10;
pub const STORED: u8 = 11;
pub const CONSUMER_OPEN: u8 = 12;
pub const CONSUMER_FETCH: u8 = 13;
pub const DELIVERY: u8 = 14;
pub const ACK: u8 = 15;
pub const CONSUMER_DELETE: u8 = 16;

/// Reads length-prefixed frames from one stream. Reads are cancellation safe:
/// a partial frame stays buffered for the next call.
#[derive(Default)]
pub struct FrameReader {
    max_frame_bytes: Option<usize>,
    header: [u8; 4],
    header_read: usize,
    body: BytesMut,
    first_chunk: Option<Bytes>,
    body_read: usize,
}

impl FrameReader {
    /// A reader that rejects frames longer than `max_frame_bytes` instead of
    /// [`MAX_FRAME_BYTES`].
    pub fn with_limit(max_frame_bytes: usize) -> Self {
        Self {
            max_frame_bytes: Some(max_frame_bytes),
            ..Self::default()
        }
    }

    /// Wait indefinitely for a frame to start, then read the rest within
    /// `duration`, so a peer cannot trickle bytes forever.
    pub async fn read_bounded(
        &mut self,
        recv: &mut RecvStream,
        duration: Duration,
    ) -> Result<Bytes> {
        if self.header_read == 0 {
            let n = recv
                .read(&mut self.header[..1])
                .await
                .map_err(other)?
                .ok_or_else(eof)?;
            self.header_read = n;
        }
        timeout(duration, self.read(recv))
            .await
            .map_err(timed_out)?
    }

    /// The next frame body (opcode and fields, without the length prefix).
    pub async fn read(&mut self, recv: &mut RecvStream) -> Result<Bytes> {
        while self.header_read < 4 {
            let n = recv
                .read(&mut self.header[self.header_read..])
                .await
                .map_err(other)?
                .ok_or_else(eof)?;
            self.header_read += n;
        }
        let size = u32::from_be_bytes(self.header) as usize;
        if size == 0 || size > self.max_frame_bytes.unwrap_or(MAX_FRAME_BYTES) {
            return Err(invalid("frame length exceeds limit or is zero"));
        }
        while self.body_read < size {
            let chunk = recv
                .read_chunk(size - self.body_read, true)
                .await
                .map_err(other)?
                .ok_or_else(eof)?
                .bytes;
            if self.body_read == 0 {
                if chunk.len() == size {
                    self.header_read = 0;
                    return Ok(chunk);
                }
                self.body_read = chunk.len();
                self.first_chunk = Some(chunk);
                continue;
            }
            if let Some(first) = self.first_chunk.take() {
                self.body.reserve(size);
                self.body.extend_from_slice(&first);
            }
            self.body_read += chunk.len();
            self.body.extend_from_slice(&chunk);
        }
        self.header_read = 0;
        self.body_read = 0;
        Ok(self.body.split().freeze())
    }
}

/// Write a frame that is only an opcode.
pub async fn write_simple(send: &mut SendStream, opcode: u8) -> Result<()> {
    write_frame(send, Bytes::copy_from_slice(&[opcode])).await
}

pub async fn write_frame(send: &mut SendStream, body: Bytes) -> Result<()> {
    write_parts(send, body, Bytes::new()).await
}

/// Write one frame whose body is `header` followed by `payload`.
pub async fn write_parts(send: &mut SendStream, header: Bytes, payload: Bytes) -> Result<()> {
    let size = header
        .len()
        .checked_add(payload.len())
        .filter(|s| *s > 0 && *s <= MAX_FRAME_BYTES)
        .ok_or_else(|| invalid("frame exceeds limit"))?;
    let mut prefix = BytesMut::with_capacity(4 + header.len());
    prefix.put_u32(size as u32);
    prefix.extend_from_slice(&header);
    // Quinn takes ownership of Bytes: payload data is not copied into a second
    // application buffer and header/payload enter the transport in one call.
    send.write_all_chunks(&mut [prefix.freeze(), payload])
        .await
        .map_err(other)
}

/// Write `opcode`, `extra` fixed fields, then the message's subject, reply
/// and payload.
pub async fn write_message(
    send: &mut SendStream,
    opcode: u8,
    message: &Message,
    extra: &[u8],
) -> Result<()> {
    let mut header = BytesMut::new();
    header.put_u8(opcode);
    header.extend_from_slice(extra);
    message_header(&mut header, message)?;
    write_parts(send, header.freeze(), message.payload.clone()).await
}

/// Append a message's subject and reply after validating it.
pub fn message_header(header: &mut BytesMut, message: &Message) -> Result<()> {
    validate_literal(&message.subject)?;
    if let Some(reply) = &message.reply {
        validate_literal(reply)?;
    }
    check_payload(&message.payload)?;
    put_string(header, &message.subject)?;
    put_string(header, message.reply.as_deref().unwrap_or_default())
}

/// Decode subject, reply and payload, the tail of MESSAGE-shaped frames.
pub fn decode_message(mut frame: Bytes) -> Result<Message> {
    let subject = take_string(&mut frame)?;
    let reply = take_string(&mut frame)?;
    validate_literal(&subject)?;
    if !reply.is_empty() {
        validate_literal(&reply)?;
    }
    check_payload(&frame)?;
    Ok(Message {
        subject: Arc::from(subject),
        reply: nonempty(&reply).map(Arc::from),
        payload: frame,
        sender: None,
    })
}

/// Write an ERROR frame: code 1 for permission errors, 2 otherwise, with a
/// message capped at 1 KiB.
pub async fn write_error(send: &mut SendStream, error: &io::Error) -> Result<()> {
    let mut frame = BytesMut::new();
    frame.put_u8(ERROR);
    frame.put_u16(if error.kind() == io::ErrorKind::PermissionDenied {
        1
    } else {
        2
    });
    let message = error.to_string();
    // Avoid unbounded peer-visible diagnostic strings from underlying libraries.
    let end = message
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= 1024)
        .last()
        .unwrap_or(0);
    put_string(
        &mut frame,
        if message.len() > 1024 {
            &message[..end]
        } else {
            &message
        },
    )?;
    write_frame(send, frame.freeze()).await
}

/// Consume the opcode, turning an ERROR frame into an error.
pub fn expect_opcode(frame: &mut Bytes, expected: u8) -> Result<()> {
    let opcode = take_u8(frame)?;
    if opcode == ERROR {
        let code = take_u16(frame)?;
        let message = take_string(frame)?;
        exhausted(frame)?;
        return Err(io::Error::new(
            if code == 1 {
                io::ErrorKind::PermissionDenied
            } else {
                io::ErrorKind::InvalidData
            },
            message,
        ));
    }
    if opcode != expected {
        return Err(invalid("unexpected response opcode"));
    }
    Ok(())
}

pub fn expect_ok(mut frame: Bytes) -> Result<()> {
    expect_opcode(&mut frame, OK)?;
    exhausted(&frame)
}

pub fn check_payload(payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        Err(invalid("payload exceeds 1 MiB transport limit"))
    } else {
        Ok(())
    }
}

/// A subject or subscription pattern: nonempty tokens, `*` and a terminal `>`
/// only as whole tokens, at most 1,024 bytes.
pub fn validate_pattern(pattern: &str) -> Result<()> {
    if pattern.is_empty() || pattern.len() > 1024 {
        return Err(invalid("invalid subject length"));
    }
    let mut parts = pattern.split('.').peekable();
    while let Some(part) = parts.next() {
        if part.is_empty()
            || part.chars().any(|c| c.is_whitespace() || c.is_control())
            || (part.contains(['*', '>']) && part != "*" && part != ">")
            || (part == ">" && parts.peek().is_some())
        {
            return Err(invalid("invalid subject pattern"));
        }
    }
    Ok(())
}

/// A subject without wildcards.
pub fn validate_literal(subject: &str) -> Result<()> {
    validate_pattern(subject)?;
    if subject.contains(['*', '>']) {
        return Err(invalid("publication subject must be literal"));
    }
    Ok(())
}

pub fn put_string(buffer: &mut BytesMut, value: &str) -> Result<()> {
    if value.len() > MAX_STRING {
        return Err(invalid("string exceeds protocol limit"));
    }
    buffer.put_u16(value.len() as u16);
    buffer.extend_from_slice(value.as_bytes());
    Ok(())
}

pub fn take_string(frame: &mut Bytes) -> Result<String> {
    let len = take_u16(frame)? as usize;
    if len > MAX_STRING || frame.len() < len {
        return Err(invalid("invalid string length"));
    }
    String::from_utf8(frame.split_to(len).to_vec()).map_err(|_| invalid("string must be UTF-8"))
}

pub fn take_u8(frame: &mut Bytes) -> Result<u8> {
    if frame.is_empty() {
        Err(invalid("truncated frame"))
    } else {
        Ok(frame.get_u8())
    }
}

pub fn take_u16(frame: &mut Bytes) -> Result<u16> {
    if frame.len() < 2 {
        Err(invalid("truncated frame"))
    } else {
        Ok(frame.get_u16())
    }
}

pub fn take_u32(frame: &mut Bytes) -> Result<u32> {
    if frame.len() < 4 {
        Err(invalid("truncated frame"))
    } else {
        Ok(frame.get_u32())
    }
}

pub fn take_u64(frame: &mut Bytes) -> Result<u64> {
    if frame.len() < 8 {
        Err(invalid("truncated frame"))
    } else {
        Ok(frame.get_u64())
    }
}

/// Reject trailing bytes after a fixed-layout frame.
pub fn exhausted(frame: &Bytes) -> Result<()> {
    if frame.is_empty() {
        Ok(())
    } else {
        Err(invalid("unexpected trailing frame bytes"))
    }
}

/// `None` for the empty string, the wire form of an absent field.
pub fn nonempty(value: &str) -> Option<&str> {
    if value.is_empty() { None } else { Some(value) }
}

pub fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn other(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::other(error)
}

pub fn timed_out(_: tokio::time::error::Elapsed) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "QUIC operation timed out")
}

fn eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "QUIC stream closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{client_config_from_der, server_config_from_der};
    use quinn::Endpoint;
    use rustls::pki_types::PrivateKeyDer;

    #[test]
    fn decoder_rejects_truncated_invalid_utf8_and_oversized_strings() {
        assert!(take_string(&mut Bytes::from_static(&[0, 2, b'a'])).is_err());
        assert!(take_string(&mut Bytes::from_static(&[0, 1, 255])).is_err());
        assert!(take_string(&mut Bytes::from_static(&[255, 255])).is_err());
        assert!(decode_message(Bytes::from_static(&[0, 1, b'*', 0, 0])).is_err());
        let bad = ["", "a..b", "a.>.b", "a*b", "a b", "a\0b"];
        for pattern in bad {
            assert!(validate_pattern(pattern).is_err(), "{pattern:?}");
        }
    }

    #[tokio::test]
    async fn fragmented_frame_read_survives_cancellation_mid_header_and_payload() {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let server_tls = server_config_from_der(
            vec![cert.cert.der().clone()],
            PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
        )
        .unwrap();
        let client_tls = client_config_from_der(vec![cert.cert.der().clone()]).unwrap();
        let server = Endpoint::server(server_tls, "127.0.0.1:0".parse().unwrap()).unwrap();
        let address = server.local_addr().unwrap();
        let mut client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(client_tls);
        let (phase_tx, mut phase_rx) = tokio::sync::mpsc::channel(2);
        let proceed = Arc::new(tokio::sync::Notify::new());
        let producer_proceed = proceed.clone();
        let task = tokio::spawn(async move {
            let connection = server.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = connection.accept_bi().await.unwrap();
            let mut start = [0];
            recv.read_exact(&mut start).await.unwrap();
            send.write_all(&[0, 0]).await.unwrap();
            phase_tx.send(()).await.unwrap();
            producer_proceed.notified().await;
            send.write_all(&[0, 7, MESSAGE, b'a']).await.unwrap();
            phase_tx.send(()).await.unwrap();
            producer_proceed.notified().await;
            send.write_all(b"bcdef").await.unwrap();
            send.finish().unwrap();
            send.stopped().await.unwrap();
            server.close(0u32.into(), b"test completed");
        });
        let connection = client.connect(address, "localhost").unwrap().await.unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        send.write_all(&[0]).await.unwrap();
        let mut reader = FrameReader::default();
        phase_rx.recv().await.unwrap();
        assert!(
            timeout(Duration::from_millis(10), reader.read(&mut recv))
                .await
                .is_err()
        );
        assert_eq!(reader.header_read, 2);
        proceed.notify_one();
        phase_rx.recv().await.unwrap();
        assert!(
            timeout(Duration::from_millis(10), reader.read(&mut recv))
                .await
                .is_err()
        );
        assert_eq!(reader.header_read, 4);
        assert_eq!(reader.body_read, 2);
        proceed.notify_one();
        let frame = timeout(Duration::from_secs(2), reader.read(&mut recv))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.as_ref(), b"\x07abcdef");
        task.await.unwrap();
    }
}
