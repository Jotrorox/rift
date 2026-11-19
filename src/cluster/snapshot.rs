//! Snapshot transfer on the bulk connection, in resumable chunks.
//!
//! The leader sends the file in 1 MiB chunks, each a request on the
//! `snapshot` stream: `[u32 header length][JSON header][bytes]`. The
//! receiver appends a chunk only at the offset it already holds and answers
//! with the offset it expects next, so a transfer interrupted by a lost
//! connection resumes where it stopped instead of starting over. The last
//! chunk carries the leader's vote and the snapshot's description; the
//! receiver then installs it (or, for a repair after a divergence, swaps its
//! replica for it) and answers with its vote.
use super::{
    SnapshotMeta, TypeConfig, Vote,
    link::{Links, StreamKind, remote_error},
    state::{SnapshotFile, StateMachine},
};
use bytes::{BufMut, Bytes, BytesMut};
use openraft::{
    errors::{NetworkError, ReplicationClosed, StreamingError, Unreachable},
    type_config::alias::SnapshotOf,
};
use serde::{Deserialize, Serialize};
use std::{fs, future::Future, io, io::Write, path::PathBuf, time::Duration};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Bytes per chunk.
pub const CHUNK: usize = 1024 * 1024;
const MAX_HEADER: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkHeader {
    /// Names the snapshot, so chunks of another one restart the file.
    pub id: String,
    pub offset: u64,
    pub total: u64,
    /// A repair copy rather than a Raft snapshot.
    #[serde(default)]
    pub repair: bool,
    /// With the last chunk: the sender's vote and the snapshot description.
    pub done: Option<(Vote, SnapshotMeta)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChunkReply {
    /// Send from this offset next.
    Next(u64),
    /// Installed; the receiver's vote.
    Installed(Vote),
}

/// What [`receive`] made of a chunk.
pub enum Received {
    Expect(u64),
    /// The whole file is here.
    Complete(PathBuf),
}

pub fn encode(header: &ChunkHeader, data: &[u8]) -> io::Result<Bytes> {
    let header = serde_json::to_vec(header).map_err(super::json_error)?;
    let length =
        u32::try_from(header.len()).map_err(|_| super::invalid("snapshot header too large"))?;
    let mut message = BytesMut::with_capacity(4 + header.len() + data.len());
    message.put_u32(length);
    message.put_slice(&header);
    message.put_slice(data);
    Ok(message.freeze())
}

pub fn decode(message: &Bytes) -> io::Result<(ChunkHeader, Bytes)> {
    let malformed = || io::Error::new(io::ErrorKind::InvalidData, "malformed snapshot chunk");
    let length = message
        .get(..4)
        .map(|bytes| u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
        .ok_or_else(malformed)?;
    if length > MAX_HEADER || message.len() < 4 + length {
        return Err(malformed());
    }
    let header: ChunkHeader =
        serde_json::from_slice(&message[4..4 + length]).map_err(super::json_error)?;
    Ok((header, message.slice(4 + length..)))
}

pub fn encode_reply(reply: &ChunkReply) -> io::Result<Bytes> {
    serde_json::to_vec(reply)
        .map(Bytes::from)
        .map_err(super::json_error)
}

/// Write one chunk into the incoming file of `state`'s snapshot directory.
pub fn receive(state: &StateMachine, header: &ChunkHeader, data: &[u8]) -> io::Result<Received> {
    if header.id.is_empty()
        || header.id.len() > 128
        || !header
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(super::invalid("invalid snapshot id"));
    }
    let path = state.incoming_path(&header.id);
    let held = match fs::metadata(&path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
        Err(error) => return Err(error),
    };
    if header.offset == 0 && held > 0 {
        // A new transfer of this snapshot: start over.
        fs::remove_file(&path)?;
    } else if header.offset != held {
        return Ok(Received::Expect(held));
    }
    if header.offset == 0
        && let Some(directory) = path.parent()
    {
        // Partial files of other snapshots are useless now.
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("incoming-") && name.ends_with(".part") && entry.path() != path {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let end = header.offset + data.len() as u64;
    if end > header.total {
        return Err(super::invalid("snapshot chunk beyond its total size"));
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(data)?;
    if end < header.total {
        return Ok(Received::Expect(end));
    }
    file.sync_all()?;
    Ok(Received::Complete(path))
}

/// Why a snapshot did not arrive.
#[derive(Debug)]
pub enum SendError {
    /// The caller gave up (openraft closed the replication).
    Closed(ReplicationClosed),
    /// The peer was not reached, several times in a row.
    Unreachable(io::Error),
    /// The file could not be read, or the peer refused it.
    Failed(io::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed(closed) => write!(f, "{closed}"),
            Self::Unreachable(error) => write!(f, "unreachable: {error}"),
            Self::Failed(error) => write!(f, "{error}"),
        }
    }
}

impl SendError {
    /// openraft's form of this error.
    pub fn streaming(self) -> StreamingError<TypeConfig> {
        match self {
            Self::Closed(closed) => StreamingError::Closed(closed),
            Self::Unreachable(error) => StreamingError::Unreachable(Unreachable::new(&error)),
            Self::Failed(error) => StreamingError::Network(NetworkError::new(&error)),
        }
    }
}

/// Send `snapshot` to `peer` and wait until it is installed there. Returns
/// the receiver's vote. Stops early when `cancel` resolves.
pub async fn send(
    links: &Links,
    peer: &str,
    vote: Vote,
    snapshot: SnapshotOf<TypeConfig, SnapshotFile>,
    repair: bool,
    cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
    per_chunk: Duration,
) -> Result<Vote, SendError> {
    let storage = SendError::Failed;
    let mut file = tokio::fs::File::open(snapshot.snapshot.path.as_path())
        .await
        .map_err(storage)?;
    let total = file.metadata().await.map_err(storage)?.len();
    let id = match &snapshot.meta.last_log_id {
        Some(log_id) => format!(
            "{}-{}-{}-{total}",
            log_id.leader_id.term, log_id.leader_id.node_id, log_id.index
        ),
        None => format!("0-0-0-{total}"),
    };
    let mut cancel = std::pin::pin!(cancel);
    let mut offset = 0u64;
    let mut buffer = vec![0u8; CHUNK];
    let mut failures = 0u32;
    loop {
        let length = usize::try_from((total - offset).min(CHUNK as u64)).unwrap_or(CHUNK);
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(storage)?;
        file.read_exact(&mut buffer[..length])
            .await
            .map_err(storage)?;
        let last = offset + length as u64 == total;
        let header = ChunkHeader {
            id: id.clone(),
            offset,
            total,
            repair,
            done: last.then(|| (vote, snapshot.meta.clone())),
        };
        let message = encode(&header, &buffer[..length]).map_err(storage)?;
        let reply = tokio::select! {
            closed = &mut cancel => return Err(SendError::Closed(closed)),
            reply = links.request(peer, StreamKind::Snapshot, message, per_chunk) => reply,
        };
        let reply = match reply {
            Ok(reply) => reply,
            Err(error) if remote_error(&error).is_some() => return Err(SendError::Failed(error)),
            Err(error) => {
                // Resumable: the receiver keeps what it has.
                failures += 1;
                if failures > 5 {
                    return Err(SendError::Unreachable(error));
                }
                tokio::time::sleep(Duration::from_millis(200) * failures).await;
                continue;
            }
        };
        failures = 0;
        match serde_json::from_slice::<ChunkReply>(&reply) {
            Ok(ChunkReply::Next(next)) if next <= total => offset = next,
            Ok(ChunkReply::Next(_)) => {
                return Err(SendError::Failed(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "receiver expects an offset beyond the snapshot",
                )));
            }
            Ok(ChunkReply::Installed(vote)) => return Ok(vote),
            Err(error) => return Err(SendError::Failed(super::json_error(error))),
        }
        // A receiver that already holds every byte answers with the total;
        // the next round then sends an empty last chunk with the description.
    }
}
