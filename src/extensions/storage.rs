//! Bounded host-owned state, shared by every callback in an extension runtime.
//!
//! Snapshots are atomically replaced after syncing their contents. On Unix the
//! containing directory is also synced best effort after the commit; an error at
//! that point cannot undo the rename and must not imply that the mutation failed.
//! One runtime owns a path: separate processes must not share a snapshot file.
use mlua::{Lua, Table, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_LABEL: usize = 128;
const MAX_VALUE: usize = 64 * 1024;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
// LuaJIT numbers cannot distinguish every integer outside this range.
const MAX_INTEGER: mlua::Integer = (1 << 53) - 1;
const MAGIC: &[u8; 8] = b"RIFTKV\0\x01";
const CHECKSUM_BYTES: usize = 32;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);
type Entries = BTreeMap<(String, String), Vec<u8>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StorageConfig {
    pub path: PathBuf,
}

pub(super) fn parse(value: Value) -> mlua::Result<Option<StorageConfig>> {
    let table = match value {
        Value::Nil => return Ok(None),
        Value::Table(table) => table,
        _ => return Err(mlua::Error::runtime("extensions.storage must be a table")),
    };
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if !matches!(key, Value::String(key) if key.as_bytes().as_ref() == b"path") {
            return Err(mlua::Error::runtime("unknown extensions.storage field"));
        }
    }
    let Value::String(path) = table.raw_get::<Value>("path")? else {
        return Err(mlua::Error::runtime(
            "extensions.storage.path must be a string",
        ));
    };
    let path = path.to_str()?;
    if path.is_empty() || path.contains('\0') {
        return Err(mlua::Error::runtime(
            "extensions.storage.path must be nonempty and contain no NUL bytes",
        ));
    }
    Ok(Some(StorageConfig {
        path: PathBuf::from(path.as_ref()),
    }))
}

struct State {
    path: Option<PathBuf>,
    entries: Entries,
}

#[derive(Clone)]
pub(super) struct Store(Arc<Mutex<State>>);

fn invalid(message: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "extension store deadline exceeded",
        ));
    }
    Ok(())
}

fn exact_integer(value: mlua::Integer) -> io::Result<mlua::Integer> {
    if !(-MAX_INTEGER..=MAX_INTEGER).contains(&value) {
        return Err(invalid(
            "store integer exceeds the exact Lua number range (-9007199254740991..=9007199254740991)",
        ));
    }
    Ok(value)
}

fn label(value: Value, name: &str) -> mlua::Result<String> {
    let Value::String(value) = value else {
        return Err(mlua::Error::runtime(format!(
            "store {name} must be a string"
        )));
    };
    if !(1..=MAX_LABEL).contains(&value.as_bytes().len()) {
        return Err(mlua::Error::runtime(format!(
            "store {name} must contain 1..=128 bytes"
        )));
    }
    Ok(value.to_str()?.to_owned())
}

impl Store {
    pub(super) fn open(config: Option<&StorageConfig>) -> io::Result<Self> {
        let path = config.map(|config| config.path.clone());
        let entries = if let Some(path) = &path {
            // Inspect before opening: opening a named pipe can block indefinitely.
            // Check the open descriptor again below in case the path was replaced.
            match fs::metadata(path) {
                Ok(metadata) => validate_metadata(&metadata)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            match File::open(path) {
                Ok(file) => {
                    validate_metadata(&file.metadata()?)?;
                    let mut bytes = Vec::new();
                    file.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
                    decode(&bytes)?
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let entries = Entries::new();
                    persist(path, &encode(&entries)?, None)?;
                    entries
                }
                Err(error) => return Err(error),
            }
        } else {
            Entries::new()
        };
        Ok(Self(Arc::new(Mutex::new(State { path, entries }))))
    }

    fn lock(&self, deadline: Instant) -> io::Result<MutexGuard<'_, State>> {
        loop {
            check_deadline(deadline)?;
            match self.0.try_lock() {
                Ok(state) => {
                    check_deadline(deadline)?;
                    return Ok(state);
                }
                Err(TryLockError::Poisoned(_)) => return Err(invalid("extension store poisoned")),
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    fn get(
        &self,
        namespace: String,
        key: String,
        deadline: Instant,
    ) -> io::Result<Option<Vec<u8>>> {
        Ok(self.lock(deadline)?.entries.get(&(namespace, key)).cloned())
    }

    fn mutate<T>(
        &self,
        deadline: Instant,
        change: impl FnOnce(&mut Entries) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut state = self.lock(deadline)?;
        let mut entries = state.entries.clone();
        let result = change(&mut entries)?;
        if entries != state.entries {
            let bytes = encode(&entries)?;
            check_deadline(deadline)?;
            if let Some(path) = &state.path {
                persist(path, &bytes, Some(deadline))?;
            }
            // The rename commits durable state. Do not return a deadline error
            // after it: memory must agree with the committed snapshot.
            state.entries = entries;
        }
        Ok(result)
    }

    pub(super) fn install(&self, lua: &Lua, api: &Table, deadline: Instant) -> mlua::Result<()> {
        crate::script::check_deadline(deadline)?;
        let table = lua.create_table()?;
        let store = self.clone();
        table.set(
            "get",
            lua.create_function(move |lua, (namespace, key): (Value, Value)| {
                let value = store
                    .get(label(namespace, "namespace")?, label(key, "key")?, deadline)
                    .map_err(mlua::Error::external)?;
                value.map(|value| lua.create_string(value)).transpose()
            })?,
        )?;
        let store = self.clone();
        table.set(
            "set",
            lua.create_function(move |_, (namespace, key, value): (Value, Value, Value)| {
                let key = (label(namespace, "namespace")?, label(key, "key")?);
                let Value::String(value) = value else {
                    return Err(mlua::Error::runtime("store value must be a string"));
                };
                if value.as_bytes().len() > MAX_VALUE {
                    return Err(mlua::Error::runtime("store value exceeds 64 KiB"));
                }
                store
                    .mutate(deadline, |entries| {
                        entries.insert(key, value.as_bytes().to_vec());
                        Ok(())
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
        let store = self.clone();
        table.set(
            "delete",
            lua.create_function(move |_, (namespace, key): (Value, Value)| {
                let key = (label(namespace, "namespace")?, label(key, "key")?);
                store
                    .mutate(deadline, |entries| Ok(entries.remove(&key).is_some()))
                    .map_err(mlua::Error::external)
            })?,
        )?;
        let store = self.clone();
        table.set(
            "increment",
            lua.create_function(move |_, (namespace, key, delta): (Value, Value, Value)| {
                let key = (label(namespace, "namespace")?, label(key, "key")?);
                let Value::Integer(delta) = delta else {
                    return Err(mlua::Error::runtime(
                        "store increment delta must be an integer",
                    ));
                };
                let delta = exact_integer(delta).map_err(mlua::Error::external)?;
                store
                    .mutate(deadline, |entries| {
                        let current = match entries.get(&key) {
                            Some(value) => std::str::from_utf8(value)
                                .ok()
                                .and_then(|value| value.parse::<mlua::Integer>().ok())
                                .ok_or_else(|| invalid("store value is not a decimal integer"))?,
                            None => 0,
                        };
                        let current = exact_integer(current)?;
                        let next = current
                            .checked_add(delta)
                            .ok_or_else(|| invalid("store integer overflow"))?;
                        let next = exact_integer(next)?;
                        entries.insert(key, next.to_string().into_bytes());
                        Ok(next)
                    })
                    .map_err(mlua::Error::external)
            })?,
        )?;
        api.set("store", table)
    }
}

fn validate_metadata(metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_file() {
        return Err(invalid("extension store must be a regular file"));
    }
    if metadata.len() > MAX_BYTES as u64 {
        return Err(invalid("extension store exceeds 1 MiB"));
    }
    Ok(())
}

fn encode(entries: &Entries) -> io::Result<Vec<u8>> {
    if entries.len() > MAX_ENTRIES {
        return Err(invalid("extension store exceeds 4096 entries"));
    }
    let mut size = MAGIC.len() + 4 + CHECKSUM_BYTES;
    for ((namespace, key), value) in entries {
        if !(1..=MAX_LABEL).contains(&namespace.len()) || !(1..=MAX_LABEL).contains(&key.len()) {
            return Err(invalid(
                "store namespace and key must contain 1..=128 bytes",
            ));
        }
        if value.len() > MAX_VALUE {
            return Err(invalid("store value exceeds 64 KiB"));
        }
        size += 8 + namespace.len() + key.len() + value.len();
        if size > MAX_BYTES {
            return Err(invalid("extension store exceeds 1 MiB"));
        }
    }
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for ((namespace, key), value) in entries {
        bytes.extend_from_slice(&(namespace.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(key.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        bytes.extend_from_slice(namespace.as_bytes());
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(value);
    }
    let checksum = Sha256::digest(&bytes);
    bytes.extend_from_slice(&checksum);
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> io::Result<Entries> {
    if bytes.len() > MAX_BYTES
        || bytes.len() < MAGIC.len() + 4 + CHECKSUM_BYTES
        || !bytes.starts_with(MAGIC)
    {
        return Err(invalid("invalid extension store snapshot"));
    }
    let (bytes, checksum) = bytes.split_at(bytes.len() - CHECKSUM_BYTES);
    if Sha256::digest(bytes).as_slice() != checksum {
        return Err(invalid("extension store checksum mismatch"));
    }
    fn take<'a>(bytes: &mut &'a [u8], len: usize) -> io::Result<&'a [u8]> {
        if bytes.len() < len {
            return Err(invalid("truncated extension store snapshot"));
        }
        let (value, rest) = bytes.split_at(len);
        *bytes = rest;
        Ok(value)
    }
    let mut bytes = &bytes[MAGIC.len()..];
    let count = u32::from_le_bytes(take(&mut bytes, 4)?.try_into().unwrap()) as usize;
    if count > MAX_ENTRIES {
        return Err(invalid("extension store exceeds 4096 entries"));
    }
    let mut entries = Entries::new();
    for _ in 0..count {
        let namespace_len = u16::from_le_bytes(take(&mut bytes, 2)?.try_into().unwrap()) as usize;
        let key_len = u16::from_le_bytes(take(&mut bytes, 2)?.try_into().unwrap()) as usize;
        let value_len = u32::from_le_bytes(take(&mut bytes, 4)?.try_into().unwrap()) as usize;
        if !(1..=MAX_LABEL).contains(&namespace_len)
            || !(1..=MAX_LABEL).contains(&key_len)
            || value_len > MAX_VALUE
        {
            return Err(invalid("invalid extension store entry lengths"));
        }
        let namespace = std::str::from_utf8(take(&mut bytes, namespace_len)?)
            .map_err(invalid)?
            .to_owned();
        let key = std::str::from_utf8(take(&mut bytes, key_len)?)
            .map_err(invalid)?
            .to_owned();
        let value = take(&mut bytes, value_len)?.to_vec();
        if entries.insert((namespace, key), value).is_some() {
            return Err(invalid("duplicate extension store entry"));
        }
    }
    if !bytes.is_empty() {
        return Err(invalid("trailing data in extension store snapshot"));
    }
    Ok(entries)
}

fn persist(path: &Path, bytes: &[u8], deadline: Option<Instant>) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temporary = None;
    for _ in 0..16 {
        let candidate = parent.join(format!(
            ".rift-store-{}-{}.tmp",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let (temporary, mut file) = temporary.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot create extension store temporary file",
        )
    })?;
    let result = (|| {
        if let Some(deadline) = deadline {
            check_deadline(deadline)?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Some(deadline) = deadline {
            check_deadline(deadline)?;
        }
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rift-store-test-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn config(&self) -> StorageConfig {
            StorageConfig {
                path: self.0.join("state"),
            }
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }
    fn lua(store: &Store) -> Lua {
        let lua = Lua::new();
        let api = lua.create_table().unwrap();
        store.install(&lua, &api, deadline()).unwrap();
        lua.globals().set("rift", api).unwrap();
        lua
    }

    #[test]
    fn config_requires_an_explicit_nonempty_path_and_rejects_unknown_fields() {
        let lua = Lua::new();
        assert!(parse(Value::Nil).unwrap().is_none());
        for source in [
            "true",
            "{}",
            "{path=2}",
            "{path=''}",
            "{path='a', typo=true}",
            "{path='a', [1]=true}",
            "{path='a\\0b'}",
        ] {
            assert!(parse(lua.load(source).eval().unwrap()).is_err(), "{source}");
        }
        assert_eq!(
            parse(lua.load("{path='state.bin'}").eval().unwrap()).unwrap(),
            Some(StorageConfig {
                path: "state.bin".into()
            })
        );
    }

    #[test]
    fn namespaces_binary_values_and_deletes_survive_reopening() {
        let temp = Temporary::new();
        let config = temp.config();
        let store = Store::open(Some(&config)).unwrap();
        lua(&store)
            .load(
                r#"
            assert(rift.store.get('one', 'key') == nil)
            rift.store.set('one', 'key', 'hello\0\255')
            rift.store.set('two', 'key', '')
            assert(rift.store.increment('one', 'counter', 3) == 3)
            assert(rift.store.increment('one', 'counter', -1) == 2)
            rift.store.set('two', 'deleted', 'value')
            assert(rift.store.delete('two', 'deleted'))
            assert(not rift.store.delete('two', 'deleted'))
        "#,
            )
            .exec()
            .unwrap();
        drop(store);
        let reopened = Store::open(Some(&config)).unwrap();
        lua(&reopened)
            .load(
                r#"
            assert(rift.store.get('one', 'key') == 'hello\0\255')
            assert(rift.store.get('two', 'key') == '')
            assert(rift.store.get('one', 'counter') == '2')
            assert(rift.store.get('two', 'deleted') == nil)
        "#,
            )
            .exec()
            .unwrap();
    }

    #[test]
    fn concurrent_lua_callbacks_increment_without_losing_updates() {
        let store = Store::open(None).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                std::thread::spawn(move || {
                    lua(&store)
                        .load("for i=1,100 do rift.store.increment('shared', 'count', 1) end")
                        .exec()
                        .unwrap();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            store
                .get("shared".into(), "count".into(), deadline())
                .unwrap(),
            Some(b"800".to_vec())
        );
    }

    #[test]
    fn invalid_types_limits_and_integer_overflow_leave_values_unchanged() {
        let store = Store::open(None).unwrap();
        lua(&store)
            .load(
                r#"
            rift.store.set('n', 'k', '9223372036854775807')
            assert(not pcall(rift.store.increment, 'n', 'k', 1))
            assert(rift.store.get('n', 'k') == '9223372036854775807')
            rift.store.set('n', 'k', 'not a number')
            assert(not pcall(rift.store.increment, 'n', 'k', 1))
            assert(rift.store.get('n', 'k') == 'not a number')
            assert(not pcall(rift.store.increment, 'n', 'k', 1.5))
            assert(not pcall(rift.store.increment, 'n', 'k', '1'))
            assert(not pcall(rift.store.set, 1, 'k', 'v'))
            assert(not pcall(rift.store.set, 'n', 1, 'v'))
            assert(not pcall(rift.store.set, 'n', 'k', 1))
            assert(not pcall(rift.store.get, '', 'k'))
            assert(not pcall(rift.store.get, 'n', string.rep('k', 129)))
            assert(not pcall(rift.store.set, 'n', 'k', string.rep('v', 65537)))
            assert(rift.store.get('n', 'k') == 'not a number')
            rift.store.set(string.rep('n',128), string.rep('k',128), string.rep('v',65536))
        "#,
            )
            .exec()
            .unwrap();
        assert!(
            store
                .mutate(deadline(), |entries| {
                    for i in 0..MAX_ENTRIES {
                        entries.insert(("n".into(), i.to_string()), vec![]);
                    }
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(store.0.lock().unwrap().entries.len(), 2);
        assert!(
            store
                .mutate(deadline(), |entries| {
                    for i in 0..16 {
                        entries.insert(("n".into(), i.to_string()), vec![0; MAX_VALUE]);
                    }
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(store.0.lock().unwrap().entries.len(), 2);
    }

    #[test]
    fn increment_rejects_values_outside_luajit_exact_integer_range() {
        let store = Store::open(None).unwrap();
        lua(&store)
            .load(
                r#"
            rift.store.set('n', 'positive', '9007199254740990')
            assert(rift.store.increment('n', 'positive', 1) == 9007199254740991)
            assert(not pcall(rift.store.increment, 'n', 'positive', 1))
            assert(rift.store.get('n', 'positive') == '9007199254740991')
            rift.store.set('n', 'negative', '-9007199254740990')
            assert(rift.store.increment('n', 'negative', -1) == -9007199254740991)
            assert(not pcall(rift.store.increment, 'n', 'negative', -1))
            assert(rift.store.get('n', 'negative') == '-9007199254740991')
            assert(not pcall(rift.store.increment, 'n', 'new', 9007199254740992))
            assert(not pcall(rift.store.increment, 'n', 'new', -9007199254740992))
            assert(rift.store.get('n', 'new') == nil)
            assert(rift.store.increment('n', 'new', 9007199254740991) == 9007199254740991)
            assert(rift.store.increment('n', 'new', -9007199254740991) == 0)
            for _, unsafe in ipairs({'9007199254740992', '-9007199254740992'}) do
                rift.store.set('n', 'unsafe', unsafe)
                assert(not pcall(rift.store.increment, 'n', 'unsafe', 0))
                assert(rift.store.get('n', 'unsafe') == unsafe)
            end
        "#,
            )
            .exec()
            .unwrap();
    }

    #[test]
    fn expired_operations_cannot_mutate_state() {
        let store = Store::open(None).unwrap();
        let lua = Lua::new();
        let api = lua.create_table().unwrap();
        let expires = Instant::now() + Duration::from_millis(10);
        store.install(&lua, &api, expires).unwrap();
        lua.globals().set("rift", api).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        lua.load(
            r#"
            assert(not pcall(rift.store.set, 'n', 'k', 'v'))
            assert(not pcall(rift.store.increment, 'n', 'k', 1))
            assert(not pcall(rift.store.delete, 'n', 'k'))
            assert(not pcall(rift.store.get, 'n', 'k'))
        "#,
        )
        .exec()
        .unwrap();
        assert!(store.0.lock().unwrap().entries.is_empty());
    }

    #[test]
    fn contended_operations_stop_at_the_deadline() {
        let store = Store::open(None).unwrap();
        let held = store.0.lock().unwrap();
        let waiting = store.clone();
        let (send, receive) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = waiting.get(
                "n".into(),
                "k".into(),
                Instant::now() + Duration::from_millis(20),
            );
            send.send(result.unwrap_err().kind()).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(1));
        drop(held);
        thread.join().unwrap();
        assert_eq!(result.unwrap(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn failed_snapshot_replace_preserves_memory_and_cleans_temporary_file() {
        let temp = Temporary::new();
        let config = temp.config();
        let store = Store::open(Some(&config)).unwrap();
        let lua = lua(&store);
        lua.load("rift.store.set('n', 'k', 'old')").exec().unwrap();
        let saved = temp.0.join("saved");
        fs::rename(&config.path, &saved).unwrap();
        fs::create_dir(&config.path).unwrap();
        assert!(lua.load("rift.store.set('n', 'k', 'new')").exec().is_err());
        lua.load("assert(rift.store.get('n', 'k') == 'old')")
            .exec()
            .unwrap();
        assert_eq!(fs::read_dir(&temp.0).unwrap().count(), 2);
        fs::remove_dir(&config.path).unwrap();
        fs::rename(saved, &config.path).unwrap();
        assert_eq!(
            Store::open(Some(&config))
                .unwrap()
                .get("n".into(), "k".into(), deadline())
                .unwrap(),
            Some(b"old".to_vec())
        );
    }

    #[test]
    fn malformed_truncated_oversized_and_duplicate_snapshots_fail_startup() {
        let temp = Temporary::new();
        let config = temp.config();
        let entries = BTreeMap::from([(("n".into(), "k".into()), b"v".to_vec())]);
        let valid = encode(&entries).unwrap();
        let mut trailing = valid.clone();
        trailing.push(0);
        let mut duplicate = valid.clone();
        duplicate[8..12].copy_from_slice(&2u32.to_le_bytes());
        duplicate.extend_from_slice(&valid[12..]);
        let mut invalid_utf8 = valid.clone();
        invalid_utf8[20] = 255;
        for bytes in [
            b"not a store".to_vec(),
            vec![],
            valid[..valid.len() - 1].to_vec(),
            trailing,
            duplicate,
            invalid_utf8,
            vec![0; MAX_BYTES + 1],
        ] {
            fs::write(&config.path, bytes).unwrap();
            assert!(Store::open(Some(&config)).is_err());
        }
    }

    #[test]
    #[cfg(unix)]
    fn opening_a_fifo_fails_instead_of_blocking_startup() {
        let temp = Temporary::new();
        let config = temp.config();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&config.path)
                .status()
                .unwrap()
                .success()
        );
        let (send, receive) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let error = Store::open(Some(&config)).err().unwrap();
            send.send(error.kind()).unwrap();
        });
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            io::ErrorKind::InvalidData
        );
        thread.join().unwrap();
    }
}
