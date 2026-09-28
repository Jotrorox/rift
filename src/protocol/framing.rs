use super::{compression, invalid, read_varint, write_varint};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Minecraft's outer length uses at most three bytes (VarInt21).
pub const MAX_FRAME_SIZE: usize = (1 << 21) - 1;
pub const MAX_PACKET_SIZE: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub id: i32,
    pub data: Vec<u8>,
}

impl Packet {
    pub fn new(id: i32, data: Vec<u8>) -> Self {
        Self { id, data }
    }

    pub fn empty(id: i32) -> Self {
        Self::new(id, Vec::new())
    }

    pub fn from_body(mut body: &[u8]) -> io::Result<Self> {
        let id = read_varint(&mut body)?;
        if id < 0 {
            return Err(invalid("negative packet ID"));
        }
        Ok(Self::new(id, body.to_vec()))
    }

    pub fn body(&self) -> io::Result<Vec<u8>> {
        if self.id < 0 || self.data.len() >= MAX_PACKET_SIZE {
            return Err(invalid("invalid packet ID or size"));
        }
        let mut body = Vec::with_capacity(self.data.len() + 5);
        write_varint(self.id, &mut body);
        body.extend_from_slice(&self.data);
        if body.len() > MAX_PACKET_SIZE {
            return Err(invalid("packet exceeds size limit"));
        }
        Ok(body)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Codec {
    threshold: Option<usize>,
}

impl Codec {
    pub fn threshold(&self) -> Option<usize> {
        self.threshold
    }

    pub fn set_compression(&mut self, threshold: i32) {
        self.threshold = (threshold >= 0).then_some(threshold as usize);
    }

    pub fn decode(&self, frame: &[u8]) -> io::Result<Packet> {
        if frame.is_empty() || frame.len() > MAX_FRAME_SIZE {
            return Err(invalid("frame exceeds size limit"));
        }
        let Some(threshold) = self.threshold else {
            return Packet::from_body(frame);
        };
        let mut bytes = frame;
        let length = read_varint(&mut bytes)?;
        if length < 0 || length as usize > MAX_PACKET_SIZE {
            return Err(invalid("invalid uncompressed packet length"));
        }
        if length == 0 {
            if bytes.len() >= threshold {
                return Err(invalid("uncompressed packet meets compression threshold"));
            }
            Packet::from_body(bytes)
        } else {
            if (length as usize) < threshold {
                return Err(invalid("compressed packet is below compression threshold"));
            }
            Packet::from_body(&compression::inflate(bytes, length as usize)?)
        }
    }

    pub fn encode(&self, packet: &Packet) -> io::Result<Vec<u8>> {
        let body = packet.body()?;
        let mut frame = Vec::new();
        if let Some(threshold) = self.threshold {
            if body.len() >= threshold {
                write_varint(body.len() as i32, &mut frame);
                frame.extend(compression::deflate(&body));
            } else {
                frame.push(0);
                frame.extend(body);
            }
        } else {
            frame = body;
        }
        if frame.len() > MAX_FRAME_SIZE {
            return Err(invalid("frame exceeds size limit"));
        }
        let mut result = Vec::with_capacity(frame.len() + 3);
        write_varint(frame.len() as i32, &mut result);
        result.extend(frame);
        Ok(result)
    }

    /// Writes must complete before changing compression or starting another write.
    pub async fn write(
        &self,
        writer: &mut (impl AsyncWrite + Unpin),
        packet: &Packet,
    ) -> io::Result<()> {
        writer.write_all(&self.encode(packet)?).await
    }
}

/// Partial reads live here, not in a future: `read` is safe to cancel in select!.
pub struct Reader {
    prefix: Vec<u8>,
    body: Vec<u8>,
    filled: usize,
    expected: Option<usize>,
    read_chunk_size: usize,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new(32 * 1024)
    }
}

impl Reader {
    pub fn new(read_chunk_size: usize) -> Self {
        Self {
            prefix: Vec::new(),
            body: Vec::new(),
            filled: 0,
            expected: None,
            read_chunk_size: read_chunk_size.clamp(1, MAX_FRAME_SIZE),
        }
    }

    pub fn set_read_chunk_size(&mut self, size: usize) {
        self.read_chunk_size = size.clamp(1, MAX_FRAME_SIZE);
    }

    pub async fn read_frame(
        &mut self,
        reader: &mut (impl AsyncRead + Unpin),
        max: usize,
    ) -> io::Result<Option<Vec<u8>>> {
        while self.expected.is_none() {
            let mut byte = [0];
            if reader.read(&mut byte).await? == 0 {
                if self.prefix.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated frame length",
                ));
            }
            self.prefix.push(byte[0]);
            if self.prefix.len() == 3 && byte[0] & 128 != 0 {
                return Err(invalid("frame length exceeds three bytes"));
            }
            if byte[0] & 128 == 0 {
                let size = read_varint(&mut self.prefix.as_slice())? as usize;
                if size == 0 || size > max.min(MAX_FRAME_SIZE) {
                    return Err(invalid("frame exceeds size limit"));
                }
                self.expected = Some(size);
            }
        }
        let size = self.expected.unwrap();
        while self.filled < size {
            // Grow only as data arrives. A length prefix alone cannot allocate
            // the maximum frame size on every idle connection.
            if self.filled == self.body.len() {
                self.body
                    .resize((self.filled + self.read_chunk_size).min(size), 0);
            }
            let count = reader.read(&mut self.body[self.filled..]).await?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated frame body",
                ));
            }
            self.filled += count;
        }
        self.prefix.clear();
        self.filled = 0;
        self.expected = None;
        Ok(Some(std::mem::take(&mut self.body)))
    }

    pub async fn read(
        &mut self,
        reader: &mut (impl AsyncRead + Unpin),
        codec: Codec,
    ) -> io::Result<Option<Packet>> {
        self.read_frame(reader, MAX_FRAME_SIZE)
            .await?
            .map(|body| codec.decode(&body))
            .transpose()
    }
}
