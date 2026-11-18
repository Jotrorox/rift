//! Built-in backups, agent side: copy a server directory into a snapshot
//! folder next to it and hash every file on the way.
//!
//! The copy is local, so a running server only has its saving paused
//! ([`super::protocol::Backup::save_commands`]) for as long as the disk needs,
//! not for the whole upload. The controller then reads the snapshot at its
//! own pace with [`super::protocol::Command::Read`] and `snapshot`, uploads
//! the files its backup target lacks, and releases the snapshot. A snapshot
//! left behind by a controller that stopped half way is replaced by the next
//! backup of the same server.
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::protocol::MOVE_MARKER;
use crate::backup::Entry;

/// The manifest in a snapshot folder: a JSON array of [`Entry`].
pub const MANIFEST: &str = ".rift-backup.json";
/// Skipped symlinks and special files named in a reply.
const MAX_SKIPPED: usize = 100;
/// Free disk kept beyond the snapshot itself.
const DISK_MARGIN_MB: u64 = 256;
const MIB: u64 = 1024 * 1024;

/// The snapshot folder of `server`, next to its `directory`.
pub fn staging(server: &str, directory: &Path) -> PathBuf {
    directory
        .parent()
        .unwrap_or(Path::new(""))
        .join(format!(".rift-backup-{server}"))
}

/// One folder or file found in the server directory.
struct Found {
    path: String,
    folder: bool,
    size: u64,
    mode: u32,
}

/// Copy `base` into a new `staging` folder with a manifest. `free_mb` is the
/// free disk below the agent's root, if known: a snapshot that would not fit
/// is refused before anything is copied. Symlinks and special files are
/// skipped and named in the reply.
pub(crate) fn snapshot(base: &Path, staging: &Path, free_mb: Option<u64>) -> io::Result<Value> {
    if !fs::symlink_metadata(base)?.is_dir() {
        return Err(invalid("the server directory is not a folder"));
    }
    let mut found = Vec::new();
    let mut skipped = Vec::new();
    walk(base, "", &mut found, &mut skipped)?;
    let bytes: u64 = found.iter().map(|entry| entry.size).sum();
    if let Some(free) = free_mb
        && bytes / MIB + DISK_MARGIN_MB > free
    {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            format!(
                "the snapshot needs {} MiB of free disk next to the server, but only {free} MiB are free",
                bytes / MIB + DISK_MARGIN_MB
            ),
        ));
    }
    if exists(staging)? {
        fs::remove_dir_all(staging)?;
    }
    fs::create_dir_all(staging)?;
    let result = copy_all(base, staging, found);
    let (entries, copied) = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = fs::remove_dir_all(staging);
            return Err(error);
        }
    };
    let files = entries.iter().filter(|entry| !entry.folder).count();
    let folders = entries.len() - files;
    let manifest = serde_json::to_vec(&entries)?;
    if let Err(error) = fs::write(staging.join(MANIFEST), &manifest) {
        let _ = fs::remove_dir_all(staging);
        return Err(error);
    }
    let shown: Vec<&String> = skipped.iter().take(MAX_SKIPPED).collect();
    Ok(json!({
        "files": files,
        "folders": folders,
        "bytes": copied,
        "manifest": manifest.len(),
        "skipped": shown,
        "skipped_total": skipped.len(),
    }))
}

/// Copy every found entry, hashing files as they are copied. A file that
/// disappeared since the walk is left out, as if it had never been there.
fn copy_all(base: &Path, staging: &Path, found: Vec<Found>) -> io::Result<(Vec<Entry>, u64)> {
    let mut entries = Vec::with_capacity(found.len());
    let mut copied = 0;
    for entry in found {
        let target = staging.join(&entry.path);
        if entry.folder {
            fs::create_dir_all(&target)?;
            entries.push(Entry {
                path: entry.path,
                folder: true,
                ..Entry::default()
            });
            continue;
        }
        match copy_file(&base.join(&entry.path), &target) {
            Ok((size, sha256)) => {
                copied += size;
                entries.push(Entry {
                    path: entry.path,
                    folder: false,
                    size,
                    mode: entry.mode,
                    sha256,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok((entries, copied))
}

/// Copy one file to its own end, which may have moved since the walk, and
/// return its size and SHA-256.
fn copy_file(source: &Path, target: &Path) -> io::Result<(u64, String)> {
    let mut input = fs::File::open(source)?;
    let mut output = fs::File::create(target)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 256 * 1024];
    let mut size = 0;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        output.write_all(&buffer[..read])?;
        size += read as u64;
    }
    Ok((size, hex(&digest.finalize())))
}

/// Every folder and regular file below `directory`, folders before their
/// contents, in a stable order.
fn walk(
    directory: &Path,
    prefix: &str,
    found: &mut Vec<Found>,
    skipped: &mut Vec<String>,
) -> io::Result<()> {
    let mut names = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    for name in names {
        let full = directory.join(&name);
        let Some(text) = name.to_str() else {
            skipped.push(format!("{} (not a UTF-8 name)", full.display()));
            continue;
        };
        // The marker of an unfinished move is the controller's business.
        if prefix.is_empty() && text == MOVE_MARKER {
            continue;
        }
        let path = if prefix.is_empty() {
            text.to_owned()
        } else {
            format!("{prefix}/{text}")
        };
        let metadata = match fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_dir() {
            found.push(Found {
                path: path.clone(),
                folder: true,
                size: 0,
                mode: 0,
            });
            walk(&full, &path, found, skipped)?;
        } else if metadata.is_file() {
            found.push(Found {
                path,
                folder: false,
                size: metadata.len(),
                mode: mode(&metadata),
            });
        } else {
            skipped.push(path);
        }
    }
    Ok(())
}

/// Delete a snapshot folder; returns whether there was one.
pub(crate) fn release(staging: &Path) -> io::Result<bool> {
    match fs::remove_dir_all(staging) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether `log` gained a line containing `needle` after byte `start`. A log
/// that shrank was rotated, so it is read from its beginning.
pub(crate) fn logged_since(log: &Path, start: u64, needle: &str) -> io::Result<bool> {
    let mut file = match fs::File::open(log) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let length = file.metadata()?.len();
    let start = if length < start { 0 } else { start };
    // Only the newest megabyte: the confirmation follows the command.
    let start = start.max(length.saturating_sub(MIB));
    io::Seek::seek(&mut file, io::SeekFrom::Start(start))?;
    let mut text = Vec::new();
    file.take(length - start).read_to_end(&mut text)?;
    Ok(String::from_utf8_lossy(&text).contains(needle))
}

/// The length of `path`, or 0 if it does not exist.
pub(crate) fn length(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(unix)]
fn mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode(_metadata: &fs::Metadata) -> u32 {
    0
}

fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rift-node-backup-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(&path).unwrap()
    }

    #[test]
    fn snapshots_copy_and_hash_everything_but_links_and_the_move_marker() {
        let root = scratch("snapshot");
        let base = root.join("local/services/survival-1");
        fs::create_dir_all(base.join("world/region")).unwrap();
        fs::create_dir_all(base.join("empty")).unwrap();
        fs::write(base.join("world/region/r.0.0.mca"), b"region").unwrap();
        fs::write(base.join("server.properties"), b"").unwrap();
        fs::write(base.join(MOVE_MARKER), b"{}").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", base.join("world/link")).unwrap();
        let staging = staging("survival-1", Path::new("local/services/survival-1"));
        assert_eq!(staging, Path::new("local/services/.rift-backup-survival-1"));
        let staging = root.join(staging);
        // A stale snapshot is replaced.
        fs::create_dir_all(staging.join("stale")).unwrap();
        let reply = snapshot(&base, &staging, None).unwrap();
        assert_eq!(reply["files"], 2);
        assert_eq!(reply["folders"], 3);
        assert_eq!(reply["bytes"], 6);
        #[cfg(unix)]
        assert_eq!(reply["skipped"], json!(["world/link"]));
        assert!(!staging.join("stale").exists());
        assert!(!staging.join(MOVE_MARKER).exists());
        assert_eq!(
            fs::read(staging.join("world/region/r.0.0.mca")).unwrap(),
            b"region"
        );
        let entries: Vec<Entry> =
            serde_json::from_slice(&fs::read(staging.join(MANIFEST)).unwrap()).unwrap();
        let paths: Vec<_> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "empty",
                "server.properties",
                "world",
                "world/region",
                "world/region/r.0.0.mca"
            ]
        );
        let region = &entries[4];
        assert_eq!(region.size, 6);
        assert_eq!(region.sha256, hex(&Sha256::digest(b"region")));
        assert_eq!(
            entries[1].sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(entries[0].folder && entries[0].sha256.is_empty());
        // The snapshot is read like a moved folder.
        let chunk =
            super::super::transfer::read_chunk(&staging, "world/region/r.0.0.mca", 0).unwrap();
        assert_eq!(chunk["size"], 6);
        // Too little disk refuses before copying.
        let error = snapshot(&base, &staging, Some(10)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert!(release(&staging).unwrap());
        assert!(!release(&staging).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn flush_confirmations_are_found_after_the_command_even_across_rotation() {
        let root = scratch("log");
        let log = root.join("rift-server.log");
        assert!(!logged_since(&log, 0, "Saved the game").unwrap());
        fs::write(&log, "[12:00:00 INFO]: Saved the game\n").unwrap();
        let start = length(&log);
        assert!(!logged_since(&log, start, "Saved the game").unwrap());
        let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(b"[12:00:01 INFO]: Saving the game\n[12:00:02 INFO]: Saved the game\n")
            .unwrap();
        assert!(logged_since(&log, start, "Saved the game").unwrap());
        // Rotated: the new file is shorter than where we started.
        fs::write(&log, "Saved the game\n").unwrap();
        assert!(logged_since(&log, start, "Saved the game").unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
