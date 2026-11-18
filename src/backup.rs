//! Where backups of persistent instances live: a folder on the controller
//! host or an S3 bucket, with the same layout in both.
//!
//! ```text
//! objects/<first two hex digits>/<sha256>   file contents
//! snapshots/<instance>/<id>.json            one Manifest per snapshot
//! ```
//!
//! Objects are named by the SHA-256 of their contents, so a file that did not
//! change since an earlier snapshot is stored and uploaded only once: every
//! snapshot is complete on its own, but costs only what changed. Deleting a
//! snapshot deletes its manifest; [`Store::collect_garbage`] then deletes the
//! objects no manifest names any more. The controller runs one backup, prune
//! or restore at a time, so garbage collection never races an upload whose
//! manifest is not written yet.
//!
//! The node side, which copies a server folder into a snapshot folder and
//! hashes it, is [`crate::node::backup`]; the controller side runs in the
//! proxy binary (`src/backups.rs`).
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::config::BackupTarget;

pub mod s3;

pub const MANIFEST_VERSION: u32 = 1;
/// Manifests stay well below this; a larger one is refused rather than read.
pub const MAX_MANIFEST: usize = 256 * 1024 * 1024;

/// One folder or file of a snapshot, relative to the server folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// `/`-separated.
    pub path: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub folder: bool,
    #[serde(default)]
    pub size: u64,
    /// Unix permission bits; 0 where the node has none.
    #[serde(default)]
    pub mode: u32,
    /// Lowercase hex SHA-256 of the contents; empty for folders.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
}

/// Everything needed to restore one snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// [`snapshot_id`] of `created_unix`.
    pub id: String,
    pub instance: String,
    pub group: String,
    /// The node the instance ran on when the snapshot was taken.
    pub node: String,
    /// The instance folder, relative to the agent's `--root`.
    pub directory: PathBuf,
    pub created_unix: u64,
    /// Whether a running server confirmed `save-all flush` first.
    pub flushed: bool,
    pub files: u64,
    pub folders: u64,
    pub bytes: u64,
    pub entries: Vec<Entry>,
}

impl Manifest {
    /// The objects this snapshot needs.
    pub fn objects(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|entry| !entry.folder)
            .map(|entry| entry.sha256.as_str())
    }

    /// Check what a restore relies on: valid paths and object names, and
    /// folders before their contents.
    pub fn validate(&self) -> io::Result<()> {
        if self.version != MANIFEST_VERSION {
            return Err(invalid(format!(
                "snapshot {} has manifest version {}; this rift reads version {MANIFEST_VERSION}",
                self.id, self.version
            )));
        }
        for entry in &self.entries {
            if !valid_path(&entry.path) {
                return Err(invalid(format!(
                    "snapshot {} lists an invalid path {:?}",
                    self.id, entry.path
                )));
            }
            if !entry.folder && !valid_object(&entry.sha256) {
                return Err(invalid(format!(
                    "snapshot {} lists {} without a valid SHA-256",
                    self.id, entry.path
                )));
            }
        }
        Ok(())
    }
}

/// `YYYYMMDDTHHMMSSZ` in UTC: sorts like time, and is safe as a file name
/// and an S3 key.
pub fn snapshot_id(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let seconds = unix % 86_400;
    let (year, month, day) = civil(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// The Unix time of a [`snapshot_id`].
pub fn snapshot_time(id: &str) -> Option<u64> {
    if !valid_id(id) {
        return None;
    }
    let number = |range: std::ops::Range<usize>| id[range].parse::<u64>().ok();
    let (year, month, day) = (number(0..4)?, number(4..6)?, number(6..8)?);
    let (hour, minute, second) = (number(9..11)?, number(11..13)?, number(13..15)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = days_from_civil(year as i64, month as u32, day as u32);
    let unix = days * 86_400 + (hour * 3600 + minute * 60 + second) as i64;
    u64::try_from(unix).ok()
}

pub fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 16
        && bytes[8] == b'T'
        && bytes[15] == b'Z'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 8 | 15) || byte.is_ascii_digit())
}

/// Lowercase hex SHA-256.
pub fn valid_object(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Instance names as service groups create them.
pub fn valid_instance(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// A relative `/`-separated path without empty, `.` or `..` segments.
fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.contains('\0')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The inverse of [`civil`].
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An object as a local file: the store's own file for a folder target, or
/// a downloaded copy that is deleted when this is dropped.
#[derive(Debug)]
pub struct ObjectFile {
    pub path: PathBuf,
    temporary: bool,
}

impl Drop for ObjectFile {
    fn drop(&mut self) {
        if self.temporary {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A backup target, opened from the configuration.
#[derive(Debug, Clone)]
pub enum Store {
    Directory(PathBuf),
    S3(s3::Bucket),
}

impl Store {
    /// Opening an S3 target reads its credentials from the environment.
    pub fn open(target: &BackupTarget) -> io::Result<Self> {
        Ok(match target {
            BackupTarget::Directory(path) => Self::Directory(path.clone()),
            BackupTarget::S3(target) => Self::S3(s3::Bucket::new(target)?),
        })
    }

    /// Where the backups are, for replies and logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Directory(path) => path.display().to_string(),
            Self::S3(bucket) => bucket.describe(),
        }
    }

    /// A local folder for files on their way into or out of the store,
    /// created on demand. A folder target keeps it inside itself, so new
    /// objects are renamed into place.
    pub fn spool(&self) -> PathBuf {
        match self {
            Self::Directory(path) => path.join("tmp"),
            Self::S3(_) => std::env::temp_dir().join(format!("rift-backup-{}", std::process::id())),
        }
    }

    /// Every stored object.
    pub async fn objects(&self) -> io::Result<BTreeSet<String>> {
        match self {
            Self::Directory(root) => {
                let root = root.join("objects");
                blocking(move || {
                    let mut objects = BTreeSet::new();
                    for shard in read_names(&root)? {
                        for name in read_names(&root.join(&shard))? {
                            if valid_object(&name) && name.starts_with(&shard) {
                                objects.insert(name);
                            }
                        }
                    }
                    Ok(objects)
                })
                .await
            }
            Self::S3(bucket) => Ok(bucket
                .list("objects/")
                .await?
                .into_iter()
                .filter_map(|key| {
                    let name = key.rsplit('/').next()?.to_owned();
                    valid_object(&name).then_some(name)
                })
                .collect()),
        }
    }

    /// Store the verified local file `file` as object `sha256`; the file is
    /// moved or deleted.
    pub async fn put_object(&self, sha256: &str, file: &Path) -> io::Result<()> {
        if !valid_object(sha256) {
            return Err(invalid("invalid object name"));
        }
        match self {
            Self::Directory(root) => {
                let target = object_path(root, sha256);
                let file = file.to_owned();
                blocking(move || {
                    if fs::symlink_metadata(&target).is_ok() {
                        return fs::remove_file(&file);
                    }
                    fs::create_dir_all(target.parent().expect("objects have a shard folder"))?;
                    sync(&file)?;
                    fs::rename(&file, &target)
                })
                .await
            }
            Self::S3(bucket) => {
                let result = bucket.put_file(&object_key(sha256), file).await;
                let _ = tokio::fs::remove_file(file).await;
                result
            }
        }
    }

    /// Object `sha256` as a local file below `spool`.
    pub async fn object_file(&self, sha256: &str, spool: &Path) -> io::Result<ObjectFile> {
        if !valid_object(sha256) {
            return Err(invalid("invalid object name"));
        }
        match self {
            Self::Directory(root) => {
                let path = object_path(root, sha256);
                if !tokio::fs::symlink_metadata(&path).await?.is_file() {
                    return Err(invalid(format!("object {sha256} is not a file")));
                }
                Ok(ObjectFile {
                    path,
                    temporary: false,
                })
            }
            Self::S3(bucket) => {
                tokio::fs::create_dir_all(spool).await?;
                let file = ObjectFile {
                    path: spool.join(format!("restore-{sha256}")),
                    temporary: true,
                };
                bucket.download(&object_key(sha256), &file.path).await?;
                Ok(file)
            }
        }
    }

    pub async fn put_manifest(&self, manifest: &Manifest) -> io::Result<()> {
        if !valid_instance(&manifest.instance) || !valid_id(&manifest.id) {
            return Err(invalid("invalid snapshot name"));
        }
        let bytes = serde_json::to_vec(manifest)?;
        match self {
            Self::Directory(root) => {
                let target = manifest_path(root, &manifest.instance, &manifest.id);
                let spool = self.spool();
                blocking(move || {
                    fs::create_dir_all(&spool)?;
                    fs::create_dir_all(target.parent().expect("manifests have a folder"))?;
                    let staged = spool.join(format!(
                        "manifest-{}",
                        target.file_name().unwrap().to_string_lossy()
                    ));
                    fs::write(&staged, &bytes)?;
                    sync(&staged)?;
                    fs::rename(&staged, &target)
                })
                .await
            }
            Self::S3(bucket) => {
                bucket
                    .put(&manifest_key(&manifest.instance, &manifest.id), bytes)
                    .await
            }
        }
    }

    pub async fn manifest(&self, instance: &str, id: &str) -> io::Result<Manifest> {
        if !valid_instance(instance) || !valid_id(id) {
            return Err(invalid("invalid snapshot name"));
        }
        let bytes = match self {
            Self::Directory(root) => {
                let path = manifest_path(root, instance, id);
                match tokio::fs::metadata(&path).await {
                    Ok(metadata) if metadata.len() > MAX_MANIFEST as u64 => {
                        return Err(invalid(format!("snapshot {id} is too large to read")));
                    }
                    Ok(_) => tokio::fs::read(&path).await?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("no snapshot {id} of {instance}"),
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }
            Self::S3(bucket) => {
                bucket
                    .get(&manifest_key(instance, id), MAX_MANIFEST)
                    .await?
            }
        };
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|error| invalid(format!("snapshot {id} of {instance} is damaged: {error}")))?;
        if manifest.instance != instance || manifest.id != id {
            return Err(invalid(format!(
                "snapshot {id} of {instance} names another snapshot"
            )));
        }
        manifest.validate()?;
        Ok(manifest)
    }

    /// The snapshot ids of `instance`, oldest first.
    pub async fn snapshots(&self, instance: &str) -> io::Result<Vec<String>> {
        if !valid_instance(instance) {
            return Err(invalid("invalid instance name"));
        }
        let names = match self {
            Self::Directory(root) => {
                let folder = root.join("snapshots").join(instance);
                blocking(move || read_names(&folder)).await?
            }
            Self::S3(bucket) => bucket
                .list(&format!("snapshots/{instance}/"))
                .await?
                .into_iter()
                .filter_map(|key| key.rsplit('/').next().map(str::to_owned))
                .collect(),
        };
        let mut ids: Vec<String> = names
            .into_iter()
            .filter_map(|name| name.strip_suffix(".json").map(str::to_owned))
            .filter(|id| valid_id(id))
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// Every instance with at least one snapshot.
    pub async fn instances(&self) -> io::Result<Vec<String>> {
        let mut instances: Vec<String> = match self {
            Self::Directory(root) => {
                let folder = root.join("snapshots");
                blocking(move || read_names(&folder)).await?
            }
            Self::S3(bucket) => bucket
                .list("snapshots/")
                .await?
                .into_iter()
                .filter_map(|key| {
                    let mut parts = key.split('/');
                    let (_, instance) = (parts.next()?, parts.next()?);
                    Some(instance.to_owned())
                })
                .collect(),
        };
        instances.retain(|name| valid_instance(name));
        instances.sort();
        instances.dedup();
        Ok(instances)
    }

    /// Delete one manifest; its objects stay until [`Store::collect_garbage`].
    pub async fn delete_manifest(&self, instance: &str, id: &str) -> io::Result<()> {
        if !valid_instance(instance) || !valid_id(id) {
            return Err(invalid("invalid snapshot name"));
        }
        match self {
            Self::Directory(root) => {
                let path = manifest_path(root, instance, id);
                let folder = path.parent().unwrap().to_owned();
                blocking(move || {
                    match fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                    // Only succeeds once the instance has no snapshot left.
                    let _ = fs::remove_dir(&folder);
                    Ok(())
                })
                .await
            }
            Self::S3(bucket) => bucket.delete(&manifest_key(instance, id)).await,
        }
    }

    /// Delete every object that no manifest names, and leftovers in the
    /// spool folder; returns how many objects were deleted. Callers make
    /// sure no backup runs meanwhile.
    pub async fn collect_garbage(&self) -> io::Result<u64> {
        let mut referenced = BTreeSet::new();
        for instance in self.instances().await? {
            for id in self.snapshots(&instance).await? {
                let manifest = self.manifest(&instance, &id).await?;
                referenced.extend(manifest.objects().map(str::to_owned));
            }
        }
        let unreferenced: Vec<String> = self
            .objects()
            .await?
            .into_iter()
            .filter(|object| !referenced.contains(object))
            .collect();
        let count = unreferenced.len() as u64;
        match self {
            Self::Directory(root) => {
                let (root, spool) = (root.clone(), self.spool());
                blocking(move || {
                    for object in unreferenced {
                        let path = object_path(&root, &object);
                        match fs::remove_file(&path) {
                            Ok(()) => {
                                let _ = fs::remove_dir(path.parent().unwrap());
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error),
                        }
                    }
                    match fs::remove_dir_all(&spool) {
                        Ok(()) => Ok(()),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                        Err(error) => Err(error),
                    }
                })
                .await?;
            }
            Self::S3(bucket) => {
                for object in unreferenced {
                    bucket.delete(&object_key(&object)).await?;
                }
                let _ = tokio::fs::remove_dir_all(self.spool()).await;
            }
        }
        Ok(count)
    }
}

fn object_key(sha256: &str) -> String {
    format!("objects/{}/{sha256}", &sha256[..2])
}

fn manifest_key(instance: &str, id: &str) -> String {
    format!("snapshots/{instance}/{id}.json")
}

fn object_path(root: &Path, sha256: &str) -> PathBuf {
    root.join(object_key(sha256))
}

fn manifest_path(root: &Path, instance: &str, id: &str) -> PathBuf {
    root.join(manifest_key(instance, id))
}

/// Flush a finished file to disk before it is renamed into place. Windows
/// only flushes through a handle opened for writing.
fn sync(path: &Path) -> io::Result<()> {
    fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// The UTF-8 names in `folder`; none if it does not exist.
fn read_names(folder: &Path) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut names = Vec::new();
    for entry in entries {
        if let Ok(name) = entry?.file_name().into_string() {
            names.push(name);
        }
    }
    Ok(names)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(io::Error::other)?
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rift-backup-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn sha256(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn snapshot_ids_are_utc_timestamps_that_sort_like_time() {
        assert_eq!(snapshot_id(0), "19700101T000000Z");
        // 2026-11-18 12:34:56 UTC
        assert_eq!(snapshot_id(1_795_005_296), "20261118T123456Z");
        // Leap day.
        assert_eq!(snapshot_id(951_782_400), "20000229T000000Z");
        for unix in [0, 951_782_400, 1_795_005_296, 4_102_444_799] {
            assert_eq!(snapshot_time(&snapshot_id(unix)), Some(unix));
        }
        assert!(snapshot_id(1_795_005_296) > snapshot_id(1_795_005_295));
        for bad in [
            "",
            "20261118T123456",
            "20261118X123456Z",
            "2026111aT123456Z",
            "20261318T123456Z",
            "../../../etc/x",
        ] {
            assert!(snapshot_time(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn manifests_refuse_paths_and_objects_a_restore_cannot_trust() {
        let mut manifest = Manifest {
            version: MANIFEST_VERSION,
            id: snapshot_id(0),
            instance: "survival-1".into(),
            group: "survival".into(),
            node: "node1".into(),
            directory: "local/services/survival-1".into(),
            created_unix: 0,
            flushed: true,
            files: 1,
            folders: 1,
            bytes: 2,
            entries: vec![
                Entry {
                    path: "world".into(),
                    folder: true,
                    ..Entry::default()
                },
                Entry {
                    path: "world/level.dat".into(),
                    size: 2,
                    mode: 0o644,
                    sha256: sha256(b"hi"),
                    ..Entry::default()
                },
            ],
        };
        manifest.validate().unwrap();
        assert_eq!(manifest.objects().collect::<Vec<_>>(), [sha256(b"hi")]);
        for path in ["../x", "a//b", "/abs", ".", "a/./b", ""] {
            manifest.entries[1].path = path.into();
            assert!(manifest.validate().is_err(), "{path}");
        }
        manifest.entries[1].path = "world/level.dat".into();
        manifest.entries[1].sha256 = "../objects".into();
        assert!(manifest.validate().is_err());
    }

    #[tokio::test]
    async fn a_folder_store_keeps_each_object_once_and_collects_garbage() {
        let root = scratch("store");
        let store = Store::Directory(root.clone());
        let spool = store.spool();
        fs::create_dir_all(&spool).unwrap();
        let put = |name: &str, bytes: &[u8]| {
            let path = spool.join(name);
            fs::write(&path, bytes).unwrap();
            path
        };
        let (shared, old) = (sha256(b"shared"), sha256(b"old"));
        store
            .put_object(&shared, &put("a", b"shared"))
            .await
            .unwrap();
        // Storing it again only drops the new copy.
        let again = put("b", b"shared");
        store.put_object(&shared, &again).await.unwrap();
        assert!(!again.exists());
        store.put_object(&old, &put("c", b"old")).await.unwrap();
        assert_eq!(
            store.objects().await.unwrap(),
            BTreeSet::from([shared.clone(), old.clone()])
        );
        let manifest = |id: u64, objects: &[&String]| Manifest {
            version: MANIFEST_VERSION,
            id: snapshot_id(id),
            instance: "survival-1".into(),
            group: "survival".into(),
            node: "node1".into(),
            directory: "local/services/survival-1".into(),
            created_unix: id,
            flushed: false,
            files: objects.len() as u64,
            folders: 0,
            bytes: 0,
            entries: objects
                .iter()
                .enumerate()
                .map(|(index, sha256)| Entry {
                    path: format!("file{index}"),
                    sha256: (*sha256).clone(),
                    ..Entry::default()
                })
                .collect(),
        };
        store
            .put_manifest(&manifest(100, &[&shared, &old]))
            .await
            .unwrap();
        store
            .put_manifest(&manifest(200, &[&shared]))
            .await
            .unwrap();
        assert_eq!(
            store.snapshots("survival-1").await.unwrap(),
            [snapshot_id(100), snapshot_id(200)]
        );
        assert_eq!(store.instances().await.unwrap(), ["survival-1"]);
        assert_eq!(
            store
                .manifest("survival-1", &snapshot_id(200))
                .await
                .unwrap(),
            manifest(200, &[&shared])
        );
        let file = store.object_file(&old, &spool).await.unwrap();
        assert_eq!(fs::read(&file.path).unwrap(), b"old");
        drop(file);
        // Nothing is collected while a manifest still names it.
        assert_eq!(store.collect_garbage().await.unwrap(), 0);
        store
            .delete_manifest("survival-1", &snapshot_id(100))
            .await
            .unwrap();
        assert_eq!(store.collect_garbage().await.unwrap(), 1);
        assert_eq!(
            store.objects().await.unwrap(),
            BTreeSet::from([shared.clone()])
        );
        assert!(!spool.exists());
        store
            .delete_manifest("survival-1", &snapshot_id(200))
            .await
            .unwrap();
        assert!(store.instances().await.unwrap().is_empty());
        assert!(
            store
                .manifest("survival-1", &snapshot_id(200))
                .await
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
