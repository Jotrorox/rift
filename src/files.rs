//! Path-safe access to template and server folders, shared by the web
//! console and `rift node` agents. Paths stay inside their root: `..`,
//! absolute paths and symlinks are refused, and Rift's ownership markers
//! cannot be changed.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs, io,
    path::{Path as FsPath, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

pub const MAX_ENTRIES: usize = 2000;
pub const MAX_DEPTH: usize = 64;
/// Files listed by one manifest; larger trees are refused rather than cut short.
pub const MAX_MANIFEST_FILES: usize = 20_000;

pub fn segments(relative: &str) -> io::Result<Vec<String>> {
    if relative.len() > 4096 {
        return Err(invalid("path exceeds 4096 bytes"));
    }
    let segments: Vec<String> = relative
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    if segments.len() > MAX_DEPTH {
        return Err(invalid("path is nested too deeply"));
    }
    for segment in &segments {
        if segment == "."
            || segment == ".."
            || segment.len() > 255
            || segment.contains(['\\', '\0', ':'])
            || segment.chars().any(char::is_control)
        {
            return Err(invalid(format!("invalid path segment {segment:?}")));
        }
    }
    Ok(segments)
}

/// Refuse symlinks at every existing level below the root.
pub fn resolve(base: &FsPath, segments: &[String]) -> io::Result<PathBuf> {
    let mut path = base.to_path_buf();
    for segment in segments {
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "symlinks are not served",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(path)
}

/// Rift's ownership markers and staging files prove instance ownership.
pub fn protect(segments: &[String]) -> io::Result<()> {
    if segments.iter().any(|segment| segment.starts_with(".rift-")) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Rift-owned files cannot be changed from the web console",
        ));
    }
    Ok(())
}

/// A folder's entries, folders first, at most [`MAX_ENTRIES`].
pub fn list(target: &FsPath, segments: &[String]) -> io::Result<Value> {
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in fs::read_dir(target)? {
        let entry = entry?;
        if entries.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let metadata = entry.metadata()?;
        let kind = if metadata.file_type().is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };
        entries.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "type": kind,
            "size": metadata.is_file().then_some(metadata.len()),
            "modified": metadata.modified().ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|time| time.as_millis() as u64),
        }));
    }
    entries.sort_by(|left, right| {
        (left["type"] != "directory", left["name"].as_str())
            .cmp(&(right["type"] != "directory", right["name"].as_str()))
    });
    Ok(json!({"path": segments.join("/"), "entries": entries, "truncated": truncated}))
}

/// Create a folder, or atomically replace a file, below `base`.
pub fn write(
    base: &FsPath,
    segments: &[String],
    target: &FsPath,
    directory: bool,
    body: &[u8],
) -> io::Result<Value> {
    // Create missing parents one level at a time, refusing symlinks raced in.
    let mut parent = base.to_path_buf();
    for segment in &segments[..segments.len() - if directory { 0 } else { 1 }] {
        parent.push(segment);
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{segment} is not a folder"),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&parent)?,
            Err(error) => return Err(error),
        }
    }
    let path = segments.join("/");
    if directory {
        return Ok(json!({"path": path, "type": "directory"}));
    }
    if fs::symlink_metadata(target).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "a folder already exists at this path",
        ));
    }
    let mut random = [0; 8];
    aws_lc_rs::rand::fill(&mut random).map_err(|_| io::Error::other("random source failed"))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let staged = target.with_file_name(format!(".rift-upload-{suffix}"));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        io::Write::write_all(&mut file, body)?;
        file.sync_all()?;
        fs::rename(&staged, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result?;
    Ok(json!({"path": path, "type": "file", "size": body.len()}))
}

/// Delete a file or folder below its root.
pub fn delete(target: &FsPath, segments: &[String]) -> io::Result<Value> {
    protect(segments)?;
    if segments.is_empty() {
        return Err(invalid("the folder root cannot be deleted"));
    }
    let metadata = fs::symlink_metadata(target)?;
    if metadata.is_dir() {
        fs::remove_dir_all(target)?;
    } else {
        fs::remove_file(target)?;
    }
    Ok(json!({"deleted": segments.join("/")}))
}

/// Every regular file below `target` with its size and SHA-256, so a node can
/// fetch only what changed. A file target lists itself with an empty path.
/// Hashes are cached by size and modification time.
pub fn manifest(target: &FsPath) -> io::Result<Value> {
    let metadata = fs::symlink_metadata(target)?;
    let mut files = Vec::new();
    let mut directories = Vec::new();
    if metadata.is_file() {
        files.push(json!({"path": "", "size": metadata.len(), "sha256": file_hash(target)?}));
    } else if metadata.is_dir() {
        walk(target, "", 0, &mut files, &mut directories)?;
    } else {
        return Err(invalid("only regular files and folders are served"));
    }
    Ok(json!({"files": files, "directories": directories}))
}

fn walk(
    directory: &FsPath,
    prefix: &str,
    depth: usize,
    files: &mut Vec<Value>,
    directories: &mut Vec<String>,
) -> io::Result<()> {
    if depth >= MAX_DEPTH {
        return Err(invalid("template folder is nested too deeply"));
    }
    let mut entries: Vec<_> = fs::read_dir(directory)?.collect::<io::Result<_>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("template file names must be UTF-8"))?;
        segments(&name)?;
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            directories.push(path.clone());
            walk(&entry.path(), &path, depth + 1, files, directories)?;
        } else if metadata.is_file() {
            if files.len() >= MAX_MANIFEST_FILES {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    format!("template folder has more than {MAX_MANIFEST_FILES} files"),
                ));
            }
            files.push(json!({
                "path": path,
                "size": metadata.len(),
                "sha256": file_hash(&entry.path())?,
            }));
        } else {
            return Err(invalid(
                "symlinks and special files are not allowed in templates",
            ));
        }
    }
    Ok(())
}

/// SHA-256 of a file in lowercase hex, cached by path, size and mtime.
pub fn file_hash(path: &FsPath) -> io::Result<String> {
    type Cache = Mutex<HashMap<PathBuf, (u64, SystemTime, String)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let metadata = fs::metadata(path)?;
    let modified = metadata.modified()?;
    let cache = CACHE.get_or_init(Cache::default);
    if let Some((size, time, hash)) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(path)
        && *size == metadata.len()
        && *time == modified
    {
        return Ok(hash.clone());
    }
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let hash: String = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() > 100_000 {
        cache.clear();
    }
    cache.insert(path.to_owned(), (metadata.len(), modified, hash.clone()));
    Ok(hash)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_their_root() {
        assert_eq!(segments("").unwrap(), Vec::<String>::new());
        assert_eq!(segments("/a//b/").unwrap(), ["a", "b"]);
        for path in ["..", "a/../b", "./a", "a\\b", "c:", "a\0b", "a\nb"] {
            assert!(segments(path).is_err(), "{path}");
        }
        assert!(segments(&"a/".repeat(MAX_DEPTH + 1)).is_err());
        assert!(protect(&["world".into(), ".rift-instance.json".into()]).is_err());
        assert!(protect(&["plugins".into(), "game.jar".into()]).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused_at_every_level() {
        let root = std::env::temp_dir().join(format!("rift-files-{}", std::process::id()));
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink("/", root.join("escape")).unwrap();
        resolve(&root, &["real".into(), "new.txt".into()]).unwrap();
        let error = resolve(&root, &["escape".into(), "etc".into()]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        write(
            &root,
            &["real".into(), "nested".into(), "a.txt".into()],
            &root.join("real/nested/a.txt"),
            false,
            b"hi",
        )
        .unwrap();
        assert_eq!(fs::read(root.join("real/nested/a.txt")).unwrap(), b"hi");
        let error = write(
            &root,
            &["escape".into(), "x".into()],
            &root.join("escape/x"),
            false,
            b"",
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        fs::remove_dir_all(root).unwrap();
    }
}
