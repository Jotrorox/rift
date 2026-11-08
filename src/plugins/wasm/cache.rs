//! Compiled components on disk: `local/plugincache/<name>-<key>.cwasm` under
//! the configuration directory.
//!
//! `<key>` is a SHA-256 over the component's bytes, Rift's version and
//! Wasmtime's engine-compatibility hash, so a changed plugin, a different
//! Rift or different engine settings never match a stale file. Loading
//! native code is `unsafe`: Rift only loads files that end in its own
//! checksum trailer over the key and contents, and compiles from source on
//! any mismatch or error. `rift check --strict` writes these files ahead of
//! time; startup writes one after compiling when it can, quietly skipping a
//! read-only directory.
use sha2::{Digest, Sha256};
use std::{
    hash::{Hash, Hasher},
    io,
    path::{Path, PathBuf},
};
use wasmtime::{Engine, component::Component};

pub(super) const DIRECTORY: &str = "local/plugincache";
const EXTENSION: &str = "cwasm";
const TRAILER: usize = 32;

/// The cache key for `wasm` under `engine`.
pub(super) fn key(engine: &Engine, wasm: &[u8]) -> String {
    let mut compatibility = std::hash::DefaultHasher::new();
    engine
        .precompile_compatibility_hash()
        .hash(&mut compatibility);
    let mut hash = Sha256::new();
    hash.update(b"rift-plugincache\0");
    hash.update(env!("CARGO_PKG_VERSION").as_bytes());
    hash.update([0]);
    hash.update(compatibility.finish().to_le_bytes());
    hash.update(Sha256::digest(wasm));
    hex(&hash.finalize()[..16])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn trailer(key: &str, contents: &[u8]) -> [u8; TRAILER] {
    let mut hash = Sha256::new();
    hash.update(key.as_bytes());
    hash.update([0]);
    hash.update(contents);
    hash.finalize().into()
}

pub(super) fn path(config_root: &Path, name: &str, key: &str) -> PathBuf {
    config_root
        .join(DIRECTORY)
        .join(format!("{name}-{key}.{EXTENSION}"))
}

/// A component this Rift compiled earlier, or `None`.
pub(super) fn load(engine: &Engine, path: &Path, key: &str) -> Option<Component> {
    let data = std::fs::read(path).ok()?;
    let (contents, check) = data.split_at_checked(data.len().checked_sub(TRAILER)?)?;
    if check != trailer(key, contents) {
        return None;
    }
    // SAFETY: the file carries Rift's checksum over exactly these bytes and
    // this key, so it is a serialized component this Rift (same version and
    // engine settings) wrote; Wasmtime verifies its own header as well.
    unsafe { Component::deserialize(engine, contents) }.ok()
}

/// Write `serialized` for `name` and remove the plugin's older files.
pub(super) fn store(path: &Path, name: &str, key: &str, serialized: &[u8]) -> io::Result<()> {
    let directory = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(".{name}-{key}.{}.tmp", std::process::id()));
    let mut data = Vec::with_capacity(serialized.len() + TRAILER);
    data.extend_from_slice(serialized);
    data.extend_from_slice(&trailer(key, serialized));
    std::fs::write(&temporary, &data)
        .and_then(|()| std::fs::rename(&temporary, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })?;
    let current = path.file_name();
    for entry in std::fs::read_dir(directory)?.flatten() {
        let file = entry.file_name();
        let text = file.to_string_lossy();
        let stale = Some(file.as_os_str()) != current
            && text
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('-'))
                .and_then(|rest| rest.strip_suffix(&format!(".{EXTENSION}")))
                .is_some_and(|rest| {
                    rest.len() == 32 && rest.bytes().all(|b| b.is_ascii_hexdigit())
                });
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}
