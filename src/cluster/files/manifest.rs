//! `manifest` entries: the files of every template folder by path, size and
//! SHA-256, and their materialization into `template_storage`.
//!
//! | Table | Key | Holds |
//! | --- | --- | --- |
//! | `template_folders` | folder | the index of its last change and its (possibly empty) directories |
//! | `template_files` | folder, path | size and SHA-256 of each file |
//!
//! A folder is a top-level entry of `template_storage`; one whose only file
//! has the empty path is a single file. Every peer writes each folder from
//! the blob store: files that differ are replaced atomically (a staged copy
//! renamed over them), anything the manifest does not list is removed, and
//! symbolic links are removed, never followed.
use super::blobs::{Store, random_suffix, valid_hash};
use crate::cluster::{
    sql_error,
    state::{ApplyContext, Entry, Payload, Reply},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs, io,
    path::Path,
};

pub(super) const FOLDERS: &str = "template_folders";
const VERSION: u32 = 1;
/// Part of the refusal of a change computed against an older listing: the
/// proposer reads the folder again and retries.
pub(super) const CONCURRENT: &str = "changed concurrently";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS template_folders (
    folder TEXT PRIMARY KEY NOT NULL,
    version INTEGER NOT NULL,
    directories TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS template_files (
    folder TEXT NOT NULL,
    path TEXT NOT NULL,
    size INTEGER NOT NULL,
    sha256 TEXT NOT NULL,
    PRIMARY KEY (folder, path)
) STRICT;";

/// One file of a folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Below the folder, `/`-separated; empty when the folder is one file.
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// A folder as the manifest records it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Folder {
    /// The index of the entry that last changed it; 0 for none.
    pub version: u64,
    pub files: Vec<FileRecord>,
    pub directories: Vec<String>,
}

impl Folder {
    /// The folder is a single file.
    pub fn is_file(&self) -> bool {
        matches!(self.files.as_slice(), [only] if only.path.is_empty())
    }
}

/// The content of a `manifest` entry (version 1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ManifestEntry {
    /// Replace the listing of `folder`. With `expected`, only if the folder
    /// is still at that version (0: absent).
    Put {
        folder: String,
        expected: Option<u64>,
        files: Vec<FileRecord>,
        directories: Vec<String>,
    },
    /// Remove `folder`, with the same condition.
    Remove {
        folder: String,
        expected: Option<u64>,
    },
}

impl ManifestEntry {
    pub fn entry(&self) -> Entry {
        Entry::Manifest(Payload::new(
            VERSION,
            serde_json::to_value(self).unwrap_or(Value::Null),
        ))
    }
}

fn sql_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// A canonical relative path: segments [`crate::files::segments`] accepts,
/// joined by single slashes.
fn canonical(path: &str) -> bool {
    crate::files::segments(path)
        .is_ok_and(|segments| !segments.is_empty() && segments.join("/") == path)
}

/// Why a listing cannot be recorded, if it cannot.
fn check_listing(folder: &str, files: &[FileRecord], directories: &[String]) -> Result<(), String> {
    if !canonical(folder) || folder.contains('/') || folder.starts_with('.') {
        return Err(format!("invalid template folder name {folder:?}"));
    }
    if files.len() > crate::files::MAX_MANIFEST_FILES {
        return Err(format!(
            "template folder {folder} has more than {} files",
            crate::files::MAX_MANIFEST_FILES
        ));
    }
    let single = files.iter().any(|file| file.path.is_empty());
    if single && (files.len() != 1 || !directories.is_empty()) {
        return Err(format!(
            "template folder {folder}: a single file cannot have other entries"
        ));
    }
    let mut paths = BTreeSet::new();
    for file in files {
        if !(file.path.is_empty() || canonical(&file.path)) {
            return Err(format!("invalid path {:?} in {folder}", file.path));
        }
        if !valid_hash(&file.sha256) {
            return Err(format!("invalid SHA-256 for {:?} in {folder}", file.path));
        }
        if !paths.insert(file.path.as_str()) {
            return Err(format!("{:?} is listed twice in {folder}", file.path));
        }
    }
    for directory in directories {
        if !canonical(directory) {
            return Err(format!("invalid directory {directory:?} in {folder}"));
        }
        if paths.contains(directory.as_str()) {
            return Err(format!(
                "{directory:?} in {folder} is both a file and a folder"
            ));
        }
    }
    for path in &paths {
        let mut prefix = String::new();
        for segment in path.split('/') {
            if !prefix.is_empty() && paths.contains(prefix.as_str()) {
                return Err(format!("{prefix:?} in {folder} is a file and a folder"));
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
        }
    }
    Ok(())
}

/// Apply a `manifest` entry inside its transaction.
pub(super) fn apply(
    db: &Connection,
    payload: &Payload,
    ctx: &mut ApplyContext,
) -> io::Result<Reply> {
    if payload.v != VERSION {
        return Ok(Reply::err(format!(
            "manifest entry version {} is not supported by this peer",
            payload.v
        )));
    }
    let entry: ManifestEntry = match serde_json::from_value(payload.data.clone()) {
        Ok(entry) => entry,
        Err(error) => return Ok(Reply::err(format!("malformed manifest entry: {error}"))),
    };
    db.execute_batch(SCHEMA).map_err(sql_error)?;
    let (folder, expected) = match &entry {
        ManifestEntry::Put {
            folder, expected, ..
        }
        | ManifestEntry::Remove { folder, expected } => (folder.clone(), *expected),
    };
    let current: i64 = db
        .query_row(
            "SELECT version FROM template_folders WHERE folder = ?1",
            [&folder],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_error)?
        .unwrap_or(0);
    if let Some(expected) = expected
        && sql_i64(expected) != current
    {
        return Ok(Reply::err(format!(
            "template folder {folder} {CONCURRENT}: it is at version {current}, not {expected}; read it again"
        )));
    }
    match entry {
        ManifestEntry::Put {
            files, directories, ..
        } => {
            if let Err(reason) = check_listing(&folder, &files, &directories) {
                return Ok(Reply::err(reason));
            }
            let directories: BTreeSet<String> = directories.into_iter().collect();
            let version = sql_i64(ctx.index);
            db.execute(
                "INSERT INTO template_folders (folder, version, directories) VALUES (?1, ?2, ?3)
                 ON CONFLICT (folder) DO UPDATE SET version = excluded.version,
                 directories = excluded.directories",
                params![
                    folder,
                    version,
                    serde_json::to_string(&directories).map_err(crate::cluster::json_error)?
                ],
            )
            .map_err(sql_error)?;
            db.execute("DELETE FROM template_files WHERE folder = ?1", [&folder])
                .map_err(sql_error)?;
            for file in &files {
                db.execute(
                    "INSERT INTO template_files (folder, path, size, sha256) VALUES (?1, ?2, ?3, ?4)",
                    params![folder, file.path, sql_i64(file.size), file.sha256],
                )
                .map_err(sql_error)?;
            }
            ctx.changes.touch(FOLDERS, folder);
            Ok(Reply::ok(serde_json::json!({ "version": ctx.index })))
        }
        ManifestEntry::Remove { .. } => {
            let removed = db
                .execute("DELETE FROM template_folders WHERE folder = ?1", [&folder])
                .map_err(sql_error)?;
            db.execute("DELETE FROM template_files WHERE folder = ?1", [&folder])
                .map_err(sql_error)?;
            if removed > 0 {
                ctx.changes.touch(FOLDERS, folder);
            }
            Ok(Reply::ok(removed > 0))
        }
    }
}

/// Whether the manifest tables exist yet.
fn exists(db: &Connection) -> io::Result<bool> {
    super::config::table_exists(db, FOLDERS)
}

/// Every folder of the manifest.
pub fn folders(db: &Connection) -> io::Result<BTreeMap<String, Folder>> {
    let mut folders = BTreeMap::new();
    if !exists(db)? {
        return Ok(folders);
    }
    let mut statement = db
        .prepare("SELECT folder, version, directories FROM template_folders")
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    for (name, version, directories) in rows {
        folders.insert(
            name,
            Folder {
                version: u64::try_from(version).unwrap_or(0),
                files: Vec::new(),
                directories: serde_json::from_str(&directories).unwrap_or_default(),
            },
        );
    }
    let mut statement = db
        .prepare("SELECT folder, path, size, sha256 FROM template_files ORDER BY folder, path")
        .map_err(sql_error)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    for (name, path, size, sha256) in rows {
        if let Some(folder) = folders.get_mut(&name) {
            folder.files.push(FileRecord {
                path,
                size: u64::try_from(size).unwrap_or(0),
                sha256,
            });
        }
    }
    Ok(folders)
}

/// One folder of the manifest.
pub fn folder(db: &Connection, name: &str) -> io::Result<Option<Folder>> {
    Ok(folders(db)?.remove(name))
}

/// Every blob some folder references.
pub fn referenced(db: &Connection) -> io::Result<HashSet<String>> {
    if !exists(db)? {
        return Ok(HashSet::new());
    }
    let mut statement = db
        .prepare("SELECT DISTINCT sha256 FROM template_files")
        .map_err(sql_error)?;
    let hashes = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(sql_error)?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(sql_error)?;
    Ok(hashes)
}

/// The number of folders and files and their total size.
pub fn totals(db: &Connection) -> io::Result<(u64, u64, u64)> {
    if !exists(db)? {
        return Ok((0, 0, 0));
    }
    let folders: i64 = db
        .query_row("SELECT COUNT(*) FROM template_folders", [], |row| {
            row.get(0)
        })
        .map_err(sql_error)?;
    let (files, bytes): (i64, i64) = db
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(size), 0) FROM template_files",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(sql_error)?;
    let unsigned = |value: i64| u64::try_from(value).unwrap_or(0);
    Ok((unsigned(folders), unsigned(files), unsigned(bytes)))
}

/// Remove whatever is at `path` without following a link.
fn remove_entry(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        // A link to a directory is removed as a directory on Windows.
        fs::remove_file(path).or_else(|error| fs::remove_dir(path).map_err(|_| error))
    }
}

/// Make `base/relative` a real directory, level by level: a file or link
/// in the way is removed, never followed.
fn ensure_directory(base: &Path, relative: &str) -> io::Result<()> {
    let mut path = base.to_path_buf();
    for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(metadata) => {
                remove_entry(&path, &metadata)?;
                fs::create_dir(&path)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&path)?,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Write `record` at `path` from the blob store unless it is there already.
fn place_file(path: &Path, record: &FileRecord, blobs: &Store) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() == record.size && crate::files::file_hash(path)? == record.sha256 {
                return Ok(());
            }
        }
        Ok(metadata) => remove_entry(path, &metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let blob = blobs.path(&record.sha256);
    if !blob.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("blob {} is not in the blob store", record.sha256),
        ));
    }
    let staged = path.with_file_name(format!(".rift-materialize-{}", random_suffix()?));
    let copied = (|| -> io::Result<()> {
        let mut input = fs::File::open(&blob)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()
    })();
    let placed = copied.and_then(|()| fs::rename(&staged, path));
    if placed.is_err() {
        let _ = fs::remove_file(&staged);
    }
    placed
}

/// Remove everything below `directory` that is not listed.
fn remove_unlisted(
    directory: &Path,
    prefix: &str,
    files: &HashSet<String>,
    directories: &HashSet<String>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            remove_entry(&path, &metadata)?;
            continue;
        };
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if metadata.is_dir() {
            if directories.contains(&relative) {
                remove_unlisted(&path, &relative, files, directories)?;
            } else {
                fs::remove_dir_all(&path)?;
            }
        } else if !(metadata.is_file() && files.contains(&relative)) {
            remove_entry(&path, &metadata)?;
        }
    }
    Ok(())
}

/// Make `storage/name` what `folder` lists, from `blobs` (which must hold
/// every blob it references).
pub(super) fn materialize(
    storage: &Path,
    name: &str,
    folder: &Folder,
    blobs: &Store,
) -> io::Result<()> {
    if fs::symlink_metadata(storage).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{}: template_storage must not be a symbolic link",
                storage.display()
            ),
        ));
    }
    fs::create_dir_all(storage)?;
    let target = storage.join(name);
    if folder.is_file() {
        return place_file(&target, &folder.files[0], blobs);
    }
    ensure_directory(storage, name)?;
    let mut directories: BTreeSet<String> = folder.directories.iter().cloned().collect();
    for path in folder
        .files
        .iter()
        .map(|file| file.path.as_str())
        .chain(folder.directories.iter().map(String::as_str))
    {
        let mut prefix = String::new();
        let segments: Vec<&str> = path.split('/').collect();
        for segment in &segments[..segments.len().saturating_sub(1)] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            directories.insert(prefix.clone());
        }
    }
    // Parents sort before their children.
    for directory in &directories {
        ensure_directory(&target, directory)?;
    }
    for file in &folder.files {
        place_file(&target.join(&file.path), file, blobs)?;
    }
    let files: HashSet<String> = folder.files.iter().map(|file| file.path.clone()).collect();
    let directories: HashSet<String> = directories.into_iter().collect();
    remove_unlisted(&target, "", &files, &directories)
}

/// Remove `storage/name`, whatever it is.
pub(super) fn remove(storage: &Path, name: &str) -> io::Result<()> {
    let target = storage.join(name);
    match fs::symlink_metadata(&target) {
        Ok(metadata) => remove_entry(&target, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Store every file below `storage/name` in `blobs` and list them, as a
/// git sync or `rift cluster init` publishes a folder.
pub(super) fn scan(
    storage: &Path,
    name: &str,
    blobs: &Store,
) -> io::Result<(Vec<FileRecord>, Vec<String>)> {
    let target = storage.join(name);
    let metadata = fs::symlink_metadata(&target)?;
    if metadata.is_file() {
        let blob = blobs.store_file(&target)?;
        return Ok((
            vec![FileRecord {
                path: String::new(),
                size: blob.size,
                sha256: blob.sha256,
            }],
            Vec::new(),
        ));
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{}: only regular files and folders are templates",
                target.display()
            ),
        ));
    }
    let mut files = Vec::new();
    let mut directories = Vec::new();
    walk(&target, "", 0, blobs, &mut files, &mut directories)?;
    Ok((files, directories))
}

fn walk(
    directory: &Path,
    prefix: &str,
    depth: usize,
    blobs: &Store,
    files: &mut Vec<FileRecord>,
    directories: &mut Vec<String>,
) -> io::Result<()> {
    if depth >= crate::files::MAX_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "template folder is nested too deeply",
        ));
    }
    let mut entries: Vec<_> = fs::read_dir(directory)?.collect::<io::Result<_>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name().into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "template file names must be UTF-8",
            )
        })?;
        crate::files::segments(&name)?;
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            directories.push(path.clone());
            walk(&entry.path(), &path, depth + 1, blobs, files, directories)?;
        } else if metadata.is_file() {
            if files.len() >= crate::files::MAX_MANIFEST_FILES {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    format!(
                        "template folder has more than {} files",
                        crate::files::MAX_MANIFEST_FILES
                    ),
                ));
            }
            let blob = blobs.store_file(&entry.path())?;
            files.push(FileRecord {
                path,
                size: blob.size,
                sha256: blob.sha256,
            });
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{}: symlinks and special files are not allowed in templates",
                    entry.path().display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str) -> FileRecord {
        FileRecord {
            path: path.into(),
            size: 1,
            sha256: "a".repeat(64),
        }
    }

    #[test]
    fn listings_are_checked() {
        assert!(check_listing("lobby", &[record("a/b.txt")], &["c".into()]).is_ok());
        assert!(check_listing("server.jar", &[record("")], &[]).is_ok());
        assert!(check_listing("../x", &[], &[]).is_err());
        assert!(check_listing(".hidden", &[], &[]).is_err());
        assert!(check_listing("lobby", &[record(""), record("a")], &[]).is_err());
        assert!(check_listing("lobby", &[record("a/../b")], &[]).is_err());
        assert!(check_listing("lobby", &[record("a"), record("a/b")], &[]).is_err());
        assert!(check_listing("lobby", &[record("a")], &["a".into()]).is_err());
        assert!(check_listing("lobby", &[record("a"), record("a")], &[]).is_err());
    }

    #[test]
    fn materialization_replaces_changed_files_and_removes_unlisted_ones() {
        let root = std::env::temp_dir().join(format!("rift-materialize-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let blobs = Store::new(root.join("blobs"));
        let storage = root.join("templates");
        let one = blobs.store_bytes(b"one").unwrap();
        let two = blobs.store_bytes(b"two").unwrap();
        let file = |path: &str, blob: &super::super::blobs::BlobRef| FileRecord {
            path: path.into(),
            size: blob.size,
            sha256: blob.sha256.clone(),
        };
        fs::create_dir_all(storage.join("lobby/stale")).unwrap();
        fs::write(storage.join("lobby/a.txt"), b"old").unwrap();
        fs::write(storage.join("lobby/stale/x"), b"x").unwrap();
        let folder = Folder {
            version: 1,
            files: vec![file("a.txt", &one), file("plugins/b.jar", &two)],
            directories: vec!["world".into()],
        };
        materialize(&storage, "lobby", &folder, &blobs).unwrap();
        assert_eq!(fs::read(storage.join("lobby/a.txt")).unwrap(), b"one");
        assert_eq!(
            fs::read(storage.join("lobby/plugins/b.jar")).unwrap(),
            b"two"
        );
        assert!(storage.join("lobby/world").is_dir());
        assert!(!storage.join("lobby/stale").exists());
        let (files, directories) = scan(&storage, "lobby", &blobs).unwrap();
        assert_eq!(files, folder.files);
        assert_eq!(directories, ["plugins", "world"]);
        remove(&storage, "lobby").unwrap();
        assert!(!storage.join("lobby").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
