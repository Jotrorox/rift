//! Capture local Lua files once; callback VMs never read from the filesystem.
//!
//! A `plugins/<name>/` folder with a `plugin.toml` is an installed plugin.
//! Persistent ones are trusted code that must never run in the configuration
//! sandbox, so only their manifest is captured, with a fingerprint of the
//! folder for the reload diff. `mode = "script"` folders load as before.
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

use super::MAX_SOURCE_BYTES;
use crate::plugins::manifest::{self, Manifest, Mode};

const MAX_ENTRIES: usize = 1024;
const MAX_FILES: usize = 256;
const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceFile {
    pub source: Arc<str>,
    pub name: Arc<str>,
}

/// An installed plugin's `plugin.toml`, captured with the Lua files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstalledPlugin {
    pub manifest: Result<Manifest, String>,
    /// SHA-256 over the manifest and the folder's files (except `data/` and
    /// hidden entries), or why the folder cannot be used.
    pub fingerprint: Result<String, String>,
}

impl InstalledPlugin {
    pub fn persistent(&self) -> bool {
        !matches!(&self.manifest, Ok(manifest) if manifest.mode == Mode::Script)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScriptSource {
    pub entry: SourceFile,
    pub root: PathBuf,
    pub files: Arc<BTreeMap<String, SourceFile>>,
    /// `plugins/<name>/plugin.toml` by folder name.
    pub installed: Arc<BTreeMap<String, InstalledPlugin>>,
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
            installed: Arc::new(BTreeMap::new()),
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
        let installed = Arc::make_mut(&mut snapshot.installed);
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
                Ok(_) if directory == "plugins" => {
                    collect_plugins(root, files, installed, &mut bytes, &mut entries)?
                }
                Ok(_) => collect(root, directory, 0, files, &mut bytes, &mut entries)?,
            }
        }
        Ok(snapshot)
    }
}

/// Like [`collect`] for `plugins/`, setting installed plugins aside.
fn collect_plugins(
    root: &Path,
    files: &mut BTreeMap<String, SourceFile>,
    installed: &mut BTreeMap<String, InstalledPlugin>,
    bytes: &mut usize,
    entries: &mut usize,
) -> io::Result<()> {
    let path = root.join("plugins");
    let fail = |message: &str| io::Error::other(format!("{}: {message}", path.display()));
    *entries += 1;
    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
        return Err(fail(
            "Lua module and plugin paths must not be symbolic links",
        ));
    }
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| fail("Lua paths must be UTF-8"))?;
        let relative = format!("plugins/{name}");
        if name.starts_with('.') {
            *entries += 1;
            if *entries > MAX_ENTRIES {
                return Err(fail("Lua directory exceeds 1024 entries"));
            }
            continue;
        }
        let folder = root.join(&relative);
        let manifest_path = folder.join(manifest::FILE);
        let is_plugin = fs::symlink_metadata(&folder).is_ok_and(|meta| meta.is_dir())
            && fs::symlink_metadata(&manifest_path).is_ok_and(|meta| meta.is_file());
        if !is_plugin {
            collect(root, &relative, 1, files, bytes, entries)?;
            continue;
        }
        *entries += 1;
        if *entries > MAX_ENTRIES {
            return Err(fail("Lua directory exceeds 1024 entries"));
        }
        let mut text = String::new();
        let read = fs::File::open(&manifest_path)
            .and_then(|file| {
                file.take(manifest::MAX_BYTES as u64 + 1)
                    .read_to_string(&mut text)
            })
            .map_err(|error| format!("{}: {error}", manifest_path.display()));
        let manifest = read.and_then(|_| {
            Manifest::parse(&text, name)
                .map_err(|error| format!("{}: {error}", manifest_path.display()))
        });
        let plugin = InstalledPlugin {
            fingerprint: match &manifest {
                Ok(manifest) if manifest.mode == Mode::Persistent => fingerprint(&folder, manifest),
                _ => Ok(String::new()),
            },
            manifest,
        };
        if !plugin.persistent() {
            collect(root, &relative, 1, files, bytes, entries)?;
        }
        installed.insert(name.to_owned(), plugin);
    }
    Ok(())
}

/// Files hashed per plugin folder, and bytes, before the folder is refused.
const MAX_PLUGIN_FILES: usize = 4096;
const MAX_PLUGIN_BYTES: u64 = 1024 * 1024 * 1024;

/// Hash a persistent plugin's folder: manifest and artifacts, sorted by
/// path. The entry must be a regular file inside the folder, and the folder
/// may not contain symbolic links (an artifact must not escape it). Hidden
/// (dot) entries and the top-level `data/` are not hashed; a hidden entry may
/// not be a symbolic link either, and the manifest keeps the entry out of
/// both.
fn fingerprint(folder: &Path, manifest: &Manifest) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let fail =
        |path: &Path, message: &dyn std::fmt::Display| format!("{}: {message}", path.display());
    if let Some(entry) = &manifest.entry {
        let path = folder.join(entry);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Err(fail(&path, &"entry must be a regular file")),
            Err(error) => return Err(fail(&path, &error)),
        }
    }
    let mut paths = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let directory = folder.join(&relative);
        let listing = fs::read_dir(&directory).map_err(|error| fail(&directory, &error))?;
        for entry in listing {
            let entry = entry.map_err(|error| fail(&directory, &error))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(fail(&directory, &"plugin paths must be UTF-8"));
            };
            if relative.as_os_str().is_empty() && name == "data" {
                continue;
            }
            let child = relative.join(name);
            let kind = entry
                .file_type()
                .map_err(|error| fail(&folder.join(&child), &error))?;
            if kind.is_symlink() {
                return Err(fail(
                    &folder.join(&child),
                    &"plugin folders must not contain symbolic links",
                ));
            }
            if name.starts_with('.') {
                continue;
            }
            if kind.is_dir() {
                pending.push(child);
            } else if kind.is_file() {
                paths.push(child);
            }
            if paths.len() + pending.len() > MAX_PLUGIN_FILES {
                return Err(fail(folder, &format!("more than {MAX_PLUGIN_FILES} files")));
            }
        }
    }
    paths.sort();
    let mut hash = Sha256::new();
    let mut total = 0u64;
    for relative in paths {
        let path = folder.join(&relative);
        let mut file = fs::File::open(&path).map_err(|error| fail(&path, &error))?;
        let name = relative.to_string_lossy();
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        let mut buffer = [0u8; 64 * 1024];
        let mut length = 0u64;
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| fail(&path, &error))?;
            if read == 0 {
                break;
            }
            length += read as u64;
            total += read as u64;
            if total > MAX_PLUGIN_BYTES {
                return Err(fail(folder, &"plugin folder exceeds 1 GiB"));
            }
            hash.update(&buffer[..read]);
        }
        hash.update(length.to_le_bytes());
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
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
