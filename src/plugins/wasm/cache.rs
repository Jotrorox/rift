//! Compiled components on disk: `local/plugincache/<name>-<key>.cwasm` under
//! the configuration directory.
//!
//! `<key>` is a SHA-256 over the component's bytes, Rift's version and
//! Wasmtime's engine-compatibility hash, so a changed plugin, a different
//! Rift or different engine settings never match a stale file.
//!
//! Loading native code is `unsafe`, so Rift loads only files it wrote: each
//! ends in an HMAC-SHA256 over the key and contents under a per-install
//! secret, `local/plugincache/key` (32 random bytes, created on first use,
//! readable only by Rift's user). Knowing the cache key, which depends only
//! on public inputs, is not enough to forge a file. A key file that is not a
//! private regular file of Rift's user disables the cache. No WebAssembly
//! plugin can write here: the `host` grant is read-only and absolute grants
//! may not cover this directory (see [`PluginSpec::check_grants`]). Rift
//! compiles from source on any mismatch or error. `rift check --strict`
//! writes these files ahead of time; startup writes one after compiling when
//! it can, quietly skipping a read-only directory.
//!
//! [`PluginSpec::check_grants`]: crate::plugins::PluginSpec::check_grants
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::{
    hash::{Hash, Hasher},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use wasmtime::{Engine, component::Component};

const EXTENSION: &str = "cwasm";
const TRAILER: usize = 32;
/// The per-install secret's file name inside the cache directory.
const SECRET: &str = "key";
const SECRET_BYTES: usize = 32;

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

fn mac(secret: &[u8; SECRET_BYTES], key: &str, contents: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(b"rift-plugincache\0");
    mac.update(key.as_bytes());
    mac.update(&[0]);
    mac.update(contents);
    mac
}

pub(super) fn path(config_root: &Path, name: &str, key: &str) -> PathBuf {
    config_root
        .join(crate::plugins::CACHE_DIRECTORY)
        .join(format!("{name}-{key}.{EXTENSION}"))
}

/// The secret in `directory`, if it is a private regular file of this user.
fn read_secret(directory: &Path) -> io::Result<[u8; SECRET_BYTES]> {
    let path = directory.join(SECRET);
    let refuse = |why: &str| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{}: {why}; remove it to start over", path.display()),
        )
    };
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.len() != SECRET_BYTES as u64 {
        return Err(refuse("not a 32-byte regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(refuse("not private to Rift's user (expected mode 0600)"));
        }
    }
    let mut secret = [0; SECRET_BYTES];
    std::fs::File::open(&path)?.read_exact(&mut secret)?;
    Ok(secret)
}

/// The secret in `directory`, created (with the directory) when missing.
fn secret(directory: &Path) -> io::Result<[u8; SECRET_BYTES]> {
    match read_secret(directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        result => return result,
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory)?;
    let mut secret = [0; SECRET_BYTES];
    aws_lc_rs::rand::fill(&mut secret).map_err(|_| io::Error::other("random source failed"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(directory.join(SECRET)) {
        Ok(mut file) => {
            file.write_all(&secret)?;
            file.sync_all()?;
            Ok(secret)
        }
        // Another Rift (or `rift check --strict`) created it meanwhile.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_secret(directory),
        Err(error) => Err(error),
    }
}

/// A component this Rift compiled earlier, or `None`.
pub(super) fn load(engine: &Engine, path: &Path, key: &str) -> Option<Component> {
    let secret = read_secret(path.parent()?).ok()?;
    let data = std::fs::read(path).ok()?;
    let (contents, check) = data.split_at_checked(data.len().checked_sub(TRAILER)?)?;
    mac(&secret, key, contents).verify_slice(check).ok()?;
    // SAFETY: the trailer is an HMAC under this installation's private
    // secret over exactly these bytes and this key, so the file is a
    // serialized component that a Rift with the same version and engine
    // settings wrote here; no plugin can write this directory or read the
    // secret through a grant that could. Wasmtime verifies its own header
    // as well.
    unsafe { Component::deserialize(engine, contents) }.ok()
}

/// Write `serialized` for `name` and remove the plugin's older files.
pub(super) fn store(path: &Path, name: &str, key: &str, serialized: &[u8]) -> io::Result<()> {
    let directory = path.parent().unwrap_or(Path::new("."));
    let secret = secret(directory)?;
    let temporary = directory.join(format!(".{name}-{key}.{}.tmp", std::process::id()));
    let mut data = Vec::with_capacity(serialized.len() + TRAILER);
    data.extend_from_slice(serialized);
    data.extend_from_slice(&mac(&secret, key, serialized).finalize().into_bytes());
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
