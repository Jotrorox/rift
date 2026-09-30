//! Capture local Lua files once; callback VMs never read from the filesystem.
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

use super::MAX_SOURCE_BYTES;

const MAX_ENTRIES: usize = 1024;
const MAX_FILES: usize = 256;
const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceFile {
    pub source: Arc<str>,
    pub name: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScriptSource {
    pub entry: SourceFile,
    pub root: PathBuf,
    pub files: Arc<BTreeMap<String, SourceFile>>,
}

impl ScriptSource {
    pub fn new(source: &str, name: &str) -> Self {
        Self {
            entry: SourceFile {
                source: source.into(),
                name: name.into(),
            },
            root: Path::new(name)
                .parent()
                .unwrap_or(Path::new("."))
                .to_owned(),
            files: Arc::new(BTreeMap::new()),
        }
    }

    pub fn from_path(source: &str, path: &Path) -> io::Result<Self> {
        let mut snapshot = Self::new(source, &path.display().to_string());
        // Match runtime saves/reloads, which resolve the selected entry symlink.
        // Validation can also target a file that has not been saved yet.
        let resolved = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => path.to_owned(),
            Err(error) => return Err(error),
        };
        let path = &resolved;
        let root = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        snapshot.root = root.to_owned();
        let mut bytes = source.len();
        let mut entries = 0;
        if bytes > MAX_SOURCE_BYTES {
            return Err(io::Error::other("script exceeds 256 KiB source limit"));
        }
        let files = Arc::make_mut(&mut snapshot.files);
        for directory in ["lua", "plugins"] {
            match fs::symlink_metadata(root.join(directory)) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
                Ok(metadata) if metadata.is_file() => {
                    return Err(io::Error::other(format!(
                        "{}: expected a directory",
                        root.join(directory).display()
                    )));
                }
                Ok(_) => collect(root, directory, 0, files, &mut bytes, &mut entries)?,
            }
        }
        Ok(snapshot)
    }
}

fn collect(
    root: &Path,
    relative: &str,
    depth: usize,
    files: &mut BTreeMap<String, SourceFile>,
    bytes: &mut usize,
    entries: &mut usize,
) -> io::Result<()> {
    let path = root.join(relative);
    let fail = |message: &str| io::Error::other(format!("{}: {message}", path.display()));
    *entries += 1;
    if *entries > MAX_ENTRIES || depth > MAX_DEPTH {
        return Err(fail(
            "Lua directory exceeds 1024 entries or 16 directory levels",
        ));
    }
    let kind = fs::symlink_metadata(&path)?.file_type();
    if kind.is_symlink() {
        return Err(fail(
            "Lua module and plugin paths must not be symbolic links",
        ));
    }
    if kind.is_dir() {
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| fail("Lua paths must be UTF-8"))?;
            // Do not traverse plugin VCS metadata or hidden tooling directories.
            if !name.starts_with('.') {
                collect(
                    root,
                    &format!("{relative}/{name}"),
                    depth + 1,
                    files,
                    bytes,
                    entries,
                )?;
            } else {
                *entries += 1;
                if *entries > MAX_ENTRIES {
                    return Err(fail("Lua directory exceeds 1024 entries"));
                }
            }
        }
    } else if path.extension().is_some_and(|ext| ext == "lua") {
        if !kind.is_file() {
            return Err(fail("Lua source must be a regular file"));
        }
        if files.len() >= MAX_FILES {
            return Err(fail("Lua snapshot exceeds 256 module/plugin files"));
        }
        let mut source = String::new();
        fs::File::open(&path)?
            .take((MAX_SOURCE_BYTES - *bytes + 1) as u64)
            .read_to_string(&mut source)?;
        *bytes += source.len();
        if *bytes > MAX_SOURCE_BYTES {
            return Err(fail("combined Lua source exceeds 256 KiB source limit"));
        }
        files.insert(
            relative.to_owned(),
            SourceFile {
                source: source.into(),
                name: format!("@{}", path.display()).into(),
            },
        );
    }
    Ok(())
}
