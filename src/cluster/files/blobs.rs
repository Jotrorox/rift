//! The content-addressed blob store under `cache/blobs/` and the transfers
//! on the `blob` stream (bulk connection).
//!
//! A blob is stored as `cache/blobs/<first two hex digits>/<sha256>`. It is
//! written to `cache/blobs/tmp/` first, flushed through its writable handle
//! and closed, then renamed into place; a blob that is already there is
//! never replaced, so no open file is ever renamed over (Windows refuses).
//!
//! A missing blob is fetched from a peer in chunks of [`CHUNK`] bytes into
//! `tmp/<sha256>.part`. A transfer that breaks off resumes from the length
//! of that file, from the same or another peer; the whole file is checked
//! against its SHA-256 before it is renamed into place, and a copy whose
//! hash differs is deleted and refused. Memory stays bounded by one chunk.
use super::super::{Cluster, StreamKind};
use super::shared::sha256;
use aws_lc_rs::digest;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime},
};

/// The bytes one `get` request returns at most.
pub const CHUNK: u32 = 1024 * 1024;
/// How long one chunk may take.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
/// The directory of temporary and partial files inside the store.
const TEMPORARY: &str = "tmp";

/// A blob by its content.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobRef {
    pub sha256: String,
    pub size: u64,
}

/// A request on the `blob` stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(super) enum Request {
    /// Which of these blobs the receiver holds: a JSON list of booleans.
    Has { hashes: Vec<String> },
    /// Up to `length` bytes of a blob from `offset`, raw.
    Get {
        sha256: String,
        offset: u64,
        length: u32,
    },
    /// Fetch these blobs (from `from` first) and answer once they are all
    /// stored: how an upload reaches a majority of voters before its
    /// manifest is proposed.
    Replicate { from: String, blobs: Vec<BlobRef> },
    /// Sync (or with `ensure`, check out if needed) a git template source
    /// and publish its manifest: sent to the leader, which runs git syncs.
    Sync {
        name: String,
        url: String,
        reference: Option<String>,
        revision: Option<String>,
        ensure: bool,
    },
}

/// Whether `hash` is a lowercase hex SHA-256.
pub fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid_hash(hash: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{hash:?} is not a SHA-256 in lowercase hex"),
    )
}

/// One peer's blob store.
pub struct Store {
    root: PathBuf,
    /// One fetch per blob at a time.
    locks: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where blob `hash` lives (which must be a valid hash).
    pub fn path(&self, hash: &str) -> PathBuf {
        self.root.join(&hash[..2]).join(hash)
    }

    pub fn has(&self, hash: &str) -> bool {
        valid_hash(hash) && self.path(hash).is_file()
    }

    fn partial(&self, hash: &str) -> PathBuf {
        self.root.join(TEMPORARY).join(format!("{hash}.part"))
    }

    fn temporary(&self) -> io::Result<PathBuf> {
        let directory = self.root.join(TEMPORARY);
        fs::create_dir_all(&directory)?;
        Ok(directory.join(format!("{}.tmp", random_suffix()?)))
    }

    /// Store `bytes`; returns its reference.
    pub fn store_bytes(&self, bytes: &[u8]) -> io::Result<BlobRef> {
        let blob = BlobRef {
            sha256: sha256(bytes),
            size: bytes.len() as u64,
        };
        if self.has(&blob.sha256) {
            return Ok(blob);
        }
        let temporary = self.temporary()?;
        let written = (|| -> io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        self.place(&temporary, &blob.sha256)?;
        Ok(blob)
    }

    /// Store a copy of the file at `source`, hashing it while copying.
    pub fn store_file(&self, source: &Path) -> io::Result<BlobRef> {
        let temporary = self.temporary()?;
        let copied = (|| -> io::Result<BlobRef> {
            let mut input = fs::File::open(source)?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            let mut context = digest::Context::new(&digest::SHA256);
            let mut buffer = vec![0; 64 * 1024];
            let mut size = 0u64;
            loop {
                let read = input.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                context.update(&buffer[..read]);
                output.write_all(&buffer[..read])?;
                size += read as u64;
            }
            output.sync_all()?;
            Ok(BlobRef {
                sha256: crate::cluster::hex(context.finish().as_ref()),
                size,
            })
        })();
        match copied {
            Ok(blob) => {
                self.place(&temporary, &blob.sha256)?;
                Ok(blob)
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                Err(error)
            }
        }
    }

    /// Move a complete, closed file into place as blob `hash`.
    fn place(&self, file: &Path, hash: &str) -> io::Result<()> {
        if !valid_hash(hash) {
            let _ = fs::remove_file(file);
            return Err(invalid_hash(hash));
        }
        let target = self.path(hash);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if target.is_file() {
            // Never replaced: it may be open for a transfer.
            let _ = fs::remove_file(file);
            return Ok(());
        }
        if let Err(error) = fs::rename(file, &target) {
            let _ = fs::remove_file(file);
            if target.is_file() {
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }

    /// Up to `length` bytes of blob `hash` from `offset`.
    pub fn read_chunk(&self, hash: &str, offset: u64, length: u32) -> io::Result<Vec<u8>> {
        if !valid_hash(hash) {
            return Err(invalid_hash(hash));
        }
        let mut file = fs::File::open(self.path(hash))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut data = Vec::with_capacity(length.min(CHUNK) as usize);
        file.take(u64::from(length.min(CHUNK)))
            .read_to_end(&mut data)?;
        Ok(data)
    }

    /// Remove blobs no manifest references and temporary files, each only
    /// once it is older than `grace` (uploads store their blobs before they
    /// propose the manifest that references them). Returns how many went.
    pub fn collect_garbage(
        &self,
        referenced: &HashSet<String>,
        grace: Duration,
    ) -> io::Result<usize> {
        let now = SystemTime::now();
        let old = |path: &Path| {
            fs::symlink_metadata(path)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| now.duration_since(modified).is_ok_and(|age| age >= grace))
        };
        let mut removed = 0;
        let directories = match fs::read_dir(&self.root) {
            Ok(directories) => directories,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        for directory in directories {
            let directory = directory?;
            if !directory.file_type()?.is_dir() {
                continue;
            }
            let name = directory.file_name();
            let temporary = name == TEMPORARY;
            for entry in fs::read_dir(directory.path())? {
                let entry = entry?;
                let path = entry.path();
                let keep = !temporary
                    && entry
                        .file_name()
                        .to_str()
                        .is_some_and(|hash| referenced.contains(hash));
                if !keep && entry.file_type()?.is_file() && old(&path) {
                    fs::remove_file(&path)?;
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// The lock that serializes fetches of `hash`.
    fn lock(&self, hash: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(hash).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(hash.to_owned(), Arc::downgrade(&lock));
        lock
    }
}

/// SHA-256 and size of the file at `path`.
pub(super) fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let mut file = fs::File::open(path)?;
    let mut context = digest::Context::new(&digest::SHA256);
    let mut buffer = vec![0; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((crate::cluster::hex(context.finish().as_ref()), size))
}

/// 16 random hex digits for temporary file names.
pub(super) fn random_suffix() -> io::Result<String> {
    let mut random = [0u8; 8];
    aws_lc_rs::rand::fill(&mut random).map_err(|_| io::Error::other("random source failed"))?;
    Ok(crate::cluster::hex(&random))
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(io::Error::other)?
}

/// Make sure `store` holds `blob`: fetch it from `preferred` first, then
/// from any other peer that is up, until a copy with the right hash
/// arrives.
pub(super) async fn fetch(
    cluster: &Cluster,
    store: &Arc<Store>,
    blob: &BlobRef,
    preferred: Option<&str>,
) -> io::Result<()> {
    if !valid_hash(&blob.sha256) {
        return Err(invalid_hash(&blob.sha256));
    }
    let lock = store.lock(&blob.sha256);
    let _fetching = lock.lock().await;
    if store.has(&blob.sha256) {
        return Ok(());
    }
    let mut candidates: Vec<String> = preferred.map(str::to_owned).into_iter().collect();
    for status in cluster.links().statuses() {
        if status.up && status.member && !candidates.contains(&status.name) {
            candidates.push(status.name);
        }
    }
    candidates.retain(|peer| peer != cluster.name());
    if candidates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "blob {}: no other peer is reachable to fetch it from",
                &blob.sha256[..12]
            ),
        ));
    }
    let mut failures = Vec::new();
    for peer in candidates {
        match download(cluster, store, blob, &peer).await {
            Ok(()) => return Ok(()),
            Err(error) => failures.push(format!("{peer}: {error}")),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "blob {} could not be fetched: {}",
            &blob.sha256[..12],
            failures.join("; ")
        ),
    ))
}

/// Download `blob` from `peer` into its partial file, resuming where an
/// earlier attempt stopped, and check it before it is placed.
async fn download(
    cluster: &Cluster,
    store: &Arc<Store>,
    blob: &BlobRef,
    peer: &str,
) -> io::Result<()> {
    let partial = store.partial(&blob.sha256);
    let mut offset = {
        let partial = partial.clone();
        let directory = store.root.join(TEMPORARY);
        let size = blob.size;
        blocking(move || {
            fs::create_dir_all(&directory)?;
            let length = match fs::metadata(&partial) {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
                Err(error) => return Err(error),
            };
            if length > size {
                fs::remove_file(&partial)?;
                return Ok(0);
            }
            Ok(length)
        })
        .await?
    };
    while offset < blob.size {
        let length = u32::try_from((blob.size - offset).min(u64::from(CHUNK))).unwrap_or(CHUNK);
        let request = serde_json::to_vec(&Request::Get {
            sha256: blob.sha256.clone(),
            offset,
            length,
        })
        .map_err(io::Error::other)?;
        let data = cluster
            .links()
            .request(peer, StreamKind::Blob, Bytes::from(request), CHUNK_TIMEOUT)
            .await?;
        if data.is_empty() || data.len() > length as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "sent {} bytes for a chunk of {length} at offset {offset}",
                    data.len()
                ),
            ));
        }
        let received = data.len() as u64;
        let partial = partial.clone();
        let at = offset;
        blocking(move || {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&partial)?;
            if file.metadata()?.len() != at {
                return Err(io::Error::other(
                    "the partial file changed during the transfer",
                ));
            }
            file.write_all(&data)
        })
        .await?;
        offset += received;
    }
    let checked = {
        let partial = partial.clone();
        blocking(move || hash_file(&partial)).await?
    };
    if checked.0 != blob.sha256 || checked.1 != blob.size {
        let partial = partial.clone();
        let _ = blocking(move || fs::remove_file(&partial)).await;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "refused a corrupt copy: its SHA-256 is {} ({} bytes), not {} ({} bytes)",
                checked.0, checked.1, blob.sha256, blob.size
            ),
        ));
    }
    let store = store.clone();
    let hash = blob.sha256.clone();
    blocking(move || store.place(&partial, &hash)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_are_stored_by_content_and_unreferenced_ones_collected() {
        let root = std::env::temp_dir().join(format!("rift-blobs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = Store::new(root.clone());
        let one = store.store_bytes(b"one").unwrap();
        assert_eq!(one.size, 3);
        assert!(store.has(&one.sha256));
        assert_eq!(store.store_bytes(b"one").unwrap(), one);
        let source = root.join("source.bin");
        fs::write(&source, b"two").unwrap();
        let two = store.store_file(&source).unwrap();
        assert_eq!(two.sha256, sha256(b"two"));
        assert_eq!(store.read_chunk(&two.sha256, 1, 10).unwrap(), b"wo");
        let referenced: HashSet<String> = [one.sha256.clone()].into_iter().collect();
        // Within the grace period nothing goes.
        assert_eq!(
            store
                .collect_garbage(&referenced, Duration::from_secs(3600))
                .unwrap(),
            0
        );
        assert_eq!(
            store.collect_garbage(&referenced, Duration::ZERO).unwrap(),
            1
        );
        assert!(store.has(&one.sha256));
        assert!(!store.has(&two.sha256));
        assert!(!store.has("../../etc/passwd"));
        fs::remove_dir_all(root).unwrap();
    }
}
