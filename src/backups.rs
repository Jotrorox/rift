//! Built-in backups of persistent instances on nodes: `rift admin backup`,
//! `backups` and `restore`, and the scheduled run. See docs/backups.md.
//!
//! A backup asks the instance's agent for a local snapshot
//! ([`rift::node::backup`]), reads the snapshot's manifest, uploads the files
//! the backup target does not hold yet ([`rift::backup::Store`]), writes the
//! manifest, releases the snapshot and deletes snapshots beyond `keep`.
//!
//! A restore stops the instance like a move does, sends the snapshot's files
//! to a staging folder on the instance's node, or on another node, and puts
//! it in place: swapped for the old folder, or committed like a move
//! (`runtime::move_instance`) when the instance changes node. Until then the
//! old folder is untouched, so any failure leaves the instance as it was.
//!
//! One backup, prune or restore runs at a time ([`LOCK`]), so garbage
//! collection never deletes an object whose manifest is not written yet.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rift::{
    backup::{self, Entry, Manifest, Store},
    config::{Backups, Config, InstanceStorage},
    node::{
        Nodes,
        protocol::{self, Command, FILE_PART, FileEnd, ReceiveStep, Requirement},
        transfer,
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, oneshot, watch},
};

use crate::{admin, runtime::Snapshot};

/// How long the administration service waits for a backup or restore.
pub(crate) const DEADLINE: Duration = Duration::from_secs(6 * 60 * 60);
/// One chunk read or receive step.
const WAIT: Duration = Duration::from_secs(60);
/// The agent's local copy, which a large world takes minutes for.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(60 * 60);
/// A failed scheduled backup is retried after this long at the earliest.
const RETRY: Duration = Duration::from_secs(15 * 60);
/// How often the schedule looks for instances that are due.
const TICK: Duration = Duration::from_secs(60);
const RESTORE_USAGE: &str = "usage: restore <instance> [<snapshot>|latest] [--node <node>]";

static LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));
/// The last backup attempt per instance, for `rift admin backups`.
static ATTEMPTS: LazyLock<Mutex<BTreeMap<String, Attempt>>> = LazyLock::new(Mutex::default);
/// The time of each instance's newest snapshot, once known; the schedule
/// reads the target for an instance missing here.
static NEWEST: LazyLock<Mutex<BTreeMap<String, u64>>> = LazyLock::new(Mutex::default);

#[derive(Debug, Clone)]
struct Attempt {
    unix: u64,
    error: Option<String>,
}

fn record(instance: &str, error: Option<String>) {
    ATTEMPTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(instance.to_owned(), Attempt { unix: now(), error });
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn configured(snapshot: &Snapshot) -> Result<(&Backups, Store), String> {
    let backups =
        snapshot.config.backups.as_ref().ok_or(
            "backups are not configured; add a backups table to the configuration and reload",
        )?;
    let store = Store::open(&backups.target)
        .map_err(|error| format!("cannot open the backup target: {error}"))?;
    Ok((backups, store))
}

/// An instance that can be backed up, as currently registered.
#[derive(Debug, Clone)]
struct Target {
    instance: String,
    group: String,
    node: String,
    /// Relative to the agent's `--root`.
    directory: PathBuf,
}

fn target(snapshot: &Snapshot, instance: &str) -> Result<Target, String> {
    let config = &snapshot.config;
    let Some(registration) = config.instances.get(instance) else {
        if config.managed_servers.contains_key(instance) {
            return Err(format!(
                "{instance} is configured in managed_servers; only persistent service group instances on nodes are backed up"
            ));
        }
        return Err(format!(
            "unknown service instance {instance:?}; use servers to list instances"
        ));
    };
    if registration.storage != InstanceStorage::Persistent {
        return Err(format!(
            "{instance} is {}: its folder is rebuilt from the template, so it is not backed up",
            registration.storage.as_str()
        ));
    }
    let node = registration.node.clone().ok_or_else(|| {
        format!("{instance} runs on this host; back up its folder together with the host")
    })?;
    let directory = config
        .managed_servers
        .get(instance)
        .map(|server| server.directory.clone())
        .ok_or_else(|| format!("{instance} has no managed server"))?;
    Ok(Target {
        instance: instance.to_owned(),
        group: registration.group.clone(),
        node,
        directory,
    })
}

/// Instances the schedule backs up.
fn eligible(config: &Config, backups: &Backups) -> Vec<String> {
    config
        .instances
        .iter()
        .filter(|(_, instance)| {
            instance.storage == InstanceStorage::Persistent
                && instance.node.is_some()
                && backups.covers(&instance.group)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Whether `node` can take part in backups now.
fn ready(snapshot: &Snapshot, node: &str) -> Result<(), String> {
    if !snapshot.nodes.online(node) {
        return Err(format!(
            "node {node} is {}",
            snapshot.nodes.reachability(node).as_str()
        ));
    }
    let backup = [Requirement {
        feature: protocol::feature::BACKUP,
        mandatory: true,
    }];
    if !snapshot.nodes.lacking(node, &backup).is_empty() {
        return Err(format!(
            "node {node} runs an agent without built-in backups; upgrade rift on that node to {}",
            env!("CARGO_PKG_VERSION")
        ));
    }
    Ok(())
}

/// `rift admin backup <instance>`.
pub(crate) async fn backup_now(snapshot: Arc<Snapshot>, instance: &str) -> Result<Value, String> {
    let (backups, store) = configured(&snapshot)?;
    let target = target(&snapshot, instance)?;
    let _lock = LOCK.lock().await;
    let result = take(&snapshot, backups, &store, &target).await;
    record(instance, result.as_ref().err().cloned());
    result
}

/// Take one snapshot of `target`; callers hold [`LOCK`].
async fn take(
    snapshot: &Snapshot,
    backups: &Backups,
    store: &Store,
    target: &Target,
) -> Result<Value, String> {
    let Target {
        instance,
        node,
        directory,
        ..
    } = target;
    if snapshot.managed.is_moving(instance) {
        return Err(format!(
            "{instance} is moving or being restored; back it up afterwards"
        ));
    }
    ready(snapshot, node)?;
    let created = now();
    let id = backup::snapshot_id(created);
    let existing = store
        .snapshots(instance)
        .await
        .map_err(|error| format!("cannot list the snapshots of {instance}: {error}"))?;
    if let Some(last) = existing.last()
        && *last >= id
    {
        return Err(format!(
            "{instance} already has snapshot {last}; try again in a second"
        ));
    }
    log_info!("backing up {instance} on node {node}");
    let command = Command::Backup(protocol::Backup {
        server: instance.clone(),
        directory: directory.clone(),
        save_commands: backups.save_commands,
    });
    let copied = snapshot
        .nodes
        .request(node, &command, SNAPSHOT_WAIT)
        .await
        .map_err(|error| format!("node {node} could not snapshot {instance}: {error}"))?;
    let uploaded = upload(&snapshot.nodes, store, target, &id, created, &copied).await;
    let release = Command::ReleaseBackup {
        server: instance.clone(),
        directory: directory.clone(),
    };
    if let Err(error) = snapshot.nodes.request(node, &release, WAIT).await {
        log_warn!(
            "node {node} kept the backup snapshot of {instance}: {error}; its next backup replaces it"
        );
    }
    let mut reply = uploaded?;
    NEWEST
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(instance.clone(), created);
    if let Some(skipped) = copied["skipped_total"].as_u64().filter(|&count| count > 0) {
        log_warn!(
            "the backup of {instance} left out {skipped} symlinks or special files; a restore does not bring them back"
        );
        reply["skipped"] = copied["skipped"].clone();
    }
    match prune(store, instance, backups.keep).await {
        Ok(pruned) => reply["pruned"] = json!(pruned),
        Err(error) => {
            log_warn!("deleting old snapshots of {instance} failed: {error}");
            reply["prune_error"] = json!(error);
        }
    }
    log_info!(
        "backed up {instance} as snapshot {id}: {} files, {} new",
        reply["files"],
        reply["uploaded_files"]
    );
    Ok(reply)
}

/// Read the agent's snapshot and store what the target lacks.
async fn upload(
    nodes: &Nodes,
    store: &Store,
    target: &Target,
    id: &str,
    created: u64,
    copied: &Value,
) -> Result<Value, String> {
    let Target {
        instance,
        group,
        node,
        directory,
    } = target;
    let size = copied["manifest"]
        .as_u64()
        .filter(|&size| size <= backup::MAX_MANIFEST as u64)
        .ok_or_else(|| format!("node {node} sent an invalid snapshot reply"))?;
    let reader = Reader {
        nodes,
        node,
        server: instance,
        directory,
    };
    let listing = reader.read_all(rift::node::backup::MANIFEST, size).await?;
    let entries: Vec<Entry> = serde_json::from_slice(&listing)
        .map_err(|error| format!("node {node} sent an invalid snapshot manifest: {error}"))?;
    let files = entries.iter().filter(|entry| !entry.folder).count() as u64;
    let manifest = Manifest {
        version: backup::MANIFEST_VERSION,
        id: id.to_owned(),
        instance: instance.clone(),
        group: group.clone(),
        node: node.clone(),
        directory: directory.clone(),
        created_unix: created,
        flushed: copied["flushed"] == true,
        files,
        folders: entries.len() as u64 - files,
        bytes: entries.iter().map(|entry| entry.size).sum(),
        entries,
    };
    manifest
        .validate()
        .map_err(|error| format!("node {node} sent an unusable snapshot: {error}"))?;
    let spool = store.spool();
    tokio::fs::create_dir_all(&spool)
        .await
        .map_err(|error| format!("cannot create {}: {error}", spool.display()))?;
    let mut known = store
        .objects()
        .await
        .map_err(|error| format!("cannot list the backup target: {error}"))?;
    let (mut uploaded, mut uploaded_bytes) = (0u64, 0u64);
    for entry in manifest.entries.iter().filter(|entry| !entry.folder) {
        if known.contains(&entry.sha256) {
            continue;
        }
        let file = spool.join(format!("upload-{}", entry.sha256));
        if let Err(error) = reader.fetch(entry, &file).await {
            let _ = tokio::fs::remove_file(&file).await;
            return Err(error);
        }
        if let Err(error) = store.put_object(&entry.sha256, &file).await {
            let _ = tokio::fs::remove_file(&file).await;
            return Err(format!(
                "cannot store {} of {instance}: {error}",
                entry.path
            ));
        }
        known.insert(entry.sha256.clone());
        uploaded += 1;
        uploaded_bytes += entry.size;
    }
    store
        .put_manifest(&manifest)
        .await
        .map_err(|error| format!("cannot write snapshot {id} of {instance}: {error}"))?;
    Ok(json!({
        "instance": instance,
        "snapshot": id,
        "node": node,
        "target": store.describe(),
        "flushed": manifest.flushed,
        "files": manifest.files,
        "folders": manifest.folders,
        "bytes": manifest.bytes,
        "uploaded_files": uploaded,
        "uploaded_bytes": uploaded_bytes,
    }))
}

/// Delete the oldest snapshots of `instance` beyond `keep`, then the objects
/// nothing names any more.
async fn prune(store: &Store, instance: &str, keep: usize) -> Result<Vec<String>, String> {
    let ids = store
        .snapshots(instance)
        .await
        .map_err(|error| error.to_string())?;
    let excess = ids.len().saturating_sub(keep);
    let old: Vec<String> = ids.into_iter().take(excess).collect();
    for id in &old {
        store
            .delete_manifest(instance, id)
            .await
            .map_err(|error| format!("cannot delete snapshot {id}: {error}"))?;
    }
    if !old.is_empty() {
        store
            .collect_garbage()
            .await
            .map_err(|error| format!("cannot delete unused files: {error}"))?;
    }
    Ok(old)
}

/// Reads one server's snapshot folder on its node.
struct Reader<'a> {
    nodes: &'a Nodes,
    node: &'a str,
    server: &'a str,
    directory: &'a Path,
}

impl Reader<'_> {
    /// Up to [`FILE_PART`] bytes at `offset`, and the file's size.
    async fn chunk(&self, path: &str, offset: u64) -> Result<(Vec<u8>, u64), String> {
        let command = Command::Read {
            server: self.server.into(),
            directory: self.directory.into(),
            path: path.into(),
            offset,
            snapshot: true,
        };
        let mut reply = self
            .nodes
            .request(self.node, &command, WAIT)
            .await
            .map_err(|error| format!("reading {path} from node {} failed: {error}", self.node))?;
        match (reply["content"].take(), reply["size"].as_u64()) {
            (Value::String(content), Some(size)) => {
                let bytes = protocol::decode_base64(&content)
                    .filter(|bytes| bytes.len() <= FILE_PART)
                    .ok_or_else(|| format!("node {} sent invalid file contents", self.node))?;
                Ok((bytes, size))
            }
            _ => Err(format!("node {} sent an invalid read reply", self.node)),
        }
    }

    fn changed(&self, path: &str) -> String {
        format!(
            "{path} changed in the snapshot of {} on node {}",
            self.server, self.node
        )
    }

    /// A whole file of `size` bytes, in memory.
    async fn read_all(&self, path: &str, size: u64) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::with_capacity(size as usize);
        while (bytes.len() as u64) < size {
            let (chunk, total) = self.chunk(path, bytes.len() as u64).await?;
            if total != size || chunk.is_empty() || bytes.len() + chunk.len() > size as usize {
                return Err(self.changed(path));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    /// The snapshot's copy of `entry` into the local `file`, checked against
    /// the SHA-256 the agent took while copying.
    async fn fetch(&self, entry: &Entry, file: &Path) -> Result<(), String> {
        let local = |error: std::io::Error| format!("cannot write {}: {error}", file.display());
        let mut output = tokio::fs::File::create(file).await.map_err(local)?;
        let mut digest = Sha256::new();
        let mut offset = 0;
        while offset < entry.size {
            let (chunk, total) = self.chunk(&entry.path, offset).await?;
            let next = offset + chunk.len() as u64;
            if total != entry.size || chunk.is_empty() || next > entry.size {
                return Err(self.changed(&entry.path));
            }
            digest.update(&chunk);
            output.write_all(&chunk).await.map_err(local)?;
            offset = next;
        }
        output.flush().await.map_err(local)?;
        drop(output);
        if hex(&digest.finalize()) != entry.sha256 {
            return Err(format!(
                "{} of {} arrived damaged from node {}: its SHA-256 differs from the snapshot",
                entry.path, self.server, self.node
            ));
        }
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `rift admin backups [<instance>]`.
pub(crate) async fn list(snapshot: Arc<Snapshot>, instance: Option<&str>) -> Result<Value, String> {
    let (backups, store) = configured(&snapshot)?;
    let failed = |error: std::io::Error| format!("cannot read the backup target: {error}");
    let attempts = ATTEMPTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let attempt = |name: &str| {
        attempts
            .get(name)
            .map(|attempt| json!({"unix": attempt.unix, "error": attempt.error}))
    };
    let scheduled: BTreeSet<String> = eligible(&snapshot.config, backups).into_iter().collect();
    if let Some(instance) = instance {
        if !backup::valid_instance(instance) {
            return Err(format!("invalid instance name {instance:?}"));
        }
        let mut snapshots = Vec::new();
        for id in store.snapshots(instance).await.map_err(failed)? {
            let manifest = store.manifest(instance, &id).await.map_err(failed)?;
            snapshots.push(json!({
                "id": id,
                "created_unix": manifest.created_unix,
                "node": manifest.node,
                "flushed": manifest.flushed,
                "files": manifest.files,
                "folders": manifest.folders,
                "bytes": manifest.bytes,
            }));
        }
        return Ok(json!({
            "target": store.describe(),
            "instance": instance,
            "scheduled": scheduled.contains(instance),
            "snapshots": snapshots,
            "last_attempt": attempt(instance),
        }));
    }
    let mut names: BTreeSet<String> = store
        .instances()
        .await
        .map_err(failed)?
        .into_iter()
        .collect();
    names.extend(scheduled.iter().cloned());
    let mut instances = Vec::new();
    for name in names {
        let ids = store.snapshots(&name).await.map_err(failed)?;
        instances.push(json!({
            "instance": name,
            "scheduled": scheduled.contains(&name),
            "snapshots": ids.len(),
            "latest": ids.last(),
            "last_attempt": attempt(&name),
        }));
    }
    Ok(json!({
        "target": store.describe(),
        "interval_ms": backups.interval.map(|interval| interval.as_millis() as u64),
        "keep": backups.keep,
        "save_commands": backups.save_commands,
        "instances": instances,
    }))
}

/// `rift admin backups forget <instance> <snapshot>|all`: delete snapshots,
/// also of instances that no longer exist.
pub(crate) async fn forget(
    snapshot: Arc<Snapshot>,
    instance: &str,
    which: &str,
) -> Result<Value, String> {
    let (_, store) = configured(&snapshot)?;
    if !backup::valid_instance(instance) {
        return Err(format!("invalid instance name {instance:?}"));
    }
    let failed = |error: std::io::Error| format!("cannot change the backup target: {error}");
    let _lock = LOCK.lock().await;
    let ids = store.snapshots(instance).await.map_err(failed)?;
    let doomed = if which == "all" {
        ids
    } else if ids.iter().any(|id| id == which) {
        vec![which.to_owned()]
    } else {
        return Err(format!(
            "no snapshot {which:?} of {instance}; use backups {instance} to list them"
        ));
    };
    for id in &doomed {
        store.delete_manifest(instance, id).await.map_err(failed)?;
    }
    NEWEST
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(instance);
    let deleted = store.collect_garbage().await.map_err(failed)?;
    Ok(json!({"instance": instance, "forgotten": doomed, "objects_deleted": deleted}))
}

/// A parsed `restore` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Restore {
    pub instance: String,
    /// `None` is the newest snapshot.
    pub snapshot: Option<String>,
    /// Restore onto this node instead of the instance's own.
    pub node: Option<String>,
}

impl Restore {
    pub(crate) fn parse(args: &[String]) -> Result<Self, String> {
        let mut positional = Vec::new();
        let mut node = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--node" if node.is_none() => {
                    node = Some(args.next().ok_or(RESTORE_USAGE)?.clone());
                }
                flag if flag.starts_with("--") => {
                    return Err(format!(
                        "unknown or repeated option {flag:?}; {RESTORE_USAGE}"
                    ));
                }
                _ => positional.push(arg.clone()),
            }
        }
        let (instance, snapshot) = match positional.as_slice() {
            [instance] => (instance.clone(), None),
            [instance, latest] if latest == "latest" => (instance.clone(), None),
            [instance, snapshot] => (instance.clone(), Some(snapshot.clone())),
            _ => return Err(RESTORE_USAGE.into()),
        };
        Ok(Self {
            instance,
            snapshot,
            node,
        })
    }
}

/// `rift admin restore`; see the module documentation.
pub(crate) async fn restore(
    snapshot: Arc<Snapshot>,
    commands: &mpsc::Sender<admin::Command>,
    request: Restore,
) -> Result<Value, String> {
    let (_, store) = configured(&snapshot)?;
    let current = target(&snapshot, &request.instance)?;
    let Target {
        instance,
        node: from,
        directory,
        ..
    } = current;
    let to = request.node.clone().unwrap_or_else(|| from.clone());
    let elsewhere = to != from;
    let _lock = LOCK.lock().await;
    let failed = |error: std::io::Error| format!("cannot read the backup target: {error}");
    let ids = store.snapshots(&instance).await.map_err(failed)?;
    let id = match &request.snapshot {
        None => ids.last().cloned().ok_or_else(|| {
            format!("{instance} has no snapshot; take one with: rift admin backup {instance}")
        })?,
        Some(id) if ids.contains(id) => id.clone(),
        Some(id) => {
            return Err(format!(
                "no snapshot {id:?} of {instance}; use backups {instance} to list them"
            ));
        }
    };
    let manifest = store.manifest(&instance, &id).await.map_err(failed)?;
    if elsewhere {
        crate::moving::check(&snapshot, &instance, &to, false)?;
    } else {
        ready(&snapshot, &to).map_err(|error| {
            format!("{error}; restore onto another node with: rift admin restore {instance} {id} --node <node>")
        })?;
    }
    let parked = snapshot
        .managed
        .park(&instance)
        .await
        .map_err(|error| format!("cannot stop {instance} for the restore: {error}"))?;
    log_info!("restoring {instance} from snapshot {id} onto node {to}");
    let nodes = &snapshot.nodes;
    if let Err(error) = send(
        nodes, &store, &manifest, &instance, &to, &directory, !elsewhere,
    )
    .await
    {
        transfer::abort(nodes, &instance, &to, &directory).await;
        parked.resume();
        return Err(format!(
            "restoring {instance} from snapshot {id} failed: {error}; {instance} is unchanged"
        ));
    }
    let result = if elsewhere {
        let (reply, receiver) = oneshot::channel();
        let commit = crate::moving::Commit {
            instance: instance.clone(),
            from: from.clone(),
            to: to.clone(),
            copy: true,
            // The old node may be gone; its folder is never touched.
            keep_source: true,
        };
        match commands
            .send(admin::Command::MoveInstance { commit, reply })
            .await
        {
            Ok(()) => receiver
                .await
                .unwrap_or_else(|_| Err("proxy is shutting down".into())),
            Err(_) => Err("proxy is shutting down".into()),
        }
    } else {
        commit_in_place(&snapshot, &instance, &to, &directory).await
    };
    match result {
        Ok(mut reply) => {
            reply["restored"] = json!({
                "snapshot": id,
                "created_unix": manifest.created_unix,
                "files": manifest.files,
                "bytes": manifest.bytes,
            });
            log_info!("restored {instance} from snapshot {id} onto node {to}");
            parked.resume();
            Ok(reply)
        }
        Err(error) => {
            transfer::abort(nodes, &instance, &to, &directory).await;
            parked.resume();
            Err(format!("{error}; {instance} stays on node {from}"))
        }
    }
}

/// Send every folder and file of `manifest` into the staging folder on
/// `node`. The agent checks each file against the snapshot's SHA-256.
async fn send(
    nodes: &Nodes,
    store: &Store,
    manifest: &Manifest,
    server: &str,
    node: &str,
    directory: &Path,
    replace: bool,
) -> Result<(), String> {
    let step =
        |step: ReceiveStep| transfer::send_step(nodes, node, server, directory, step, replace);
    let refused = |error: std::io::Error| format!("node {node} refused the restore: {error}");
    step(ReceiveStep::Begin).await.map_err(refused)?;
    let spool = store.spool();
    for entry in &manifest.entries {
        if entry.folder {
            let path = entry.path.clone();
            step(ReceiveStep::Folder { path }).await.map_err(refused)?;
            continue;
        }
        let object = store
            .object_file(&entry.sha256, &spool)
            .await
            .map_err(|error| format!("cannot read {} from the backup: {error}", entry.path))?;
        let local =
            |error: std::io::Error| format!("cannot read {}: {error}", object.path.display());
        let mut file = tokio::fs::File::open(&object.path).await.map_err(local)?;
        if file.metadata().await.map_err(local)?.len() != entry.size {
            return Err(format!(
                "{} is damaged in the backup: its size differs from the snapshot",
                entry.path
            ));
        }
        let mut offset = 0;
        loop {
            let mut chunk = Vec::with_capacity(FILE_PART.min(entry.size as usize));
            (&mut file)
                .take(FILE_PART as u64)
                .read_to_end(&mut chunk)
                .await
                .map_err(local)?;
            let next = offset + chunk.len() as u64;
            let end = (next >= entry.size).then(|| FileEnd {
                sha256: entry.sha256.clone(),
                mode: entry.mode,
            });
            let last = end.is_some();
            if chunk.is_empty() && !last {
                return Err(format!("{} shrank in the backup", entry.path));
            }
            let step = ReceiveStep::Chunk {
                path: entry.path.clone(),
                offset,
                content: protocol::encode_base64(&chunk),
                end,
            };
            transfer::send_step(nodes, node, server, directory, step, replace)
                .await
                .map_err(refused)?;
            if last {
                break;
            }
            offset = next;
        }
    }
    Ok(())
}

/// Swap the restored folder for the old one on the instance's own node.
async fn commit_in_place(
    snapshot: &Snapshot,
    instance: &str,
    node: &str,
    directory: &Path,
) -> Result<Value, String> {
    let nodes = &snapshot.nodes;
    transfer::send_step(nodes, node, instance, directory, ReceiveStep::Commit, true)
        .await
        .map_err(|error| {
            format!("node {node} could not put the restored folder of {instance} in place: {error}")
        })?;
    // From here on the restored folder is in place; failures are reported.
    let mut cleanup = Vec::new();
    if let Err(error) = transfer::finish(nodes, instance, node, directory).await {
        cleanup.push(format!(
            "the restore marker in the folder was not removed: {error}"
        ));
    }
    // The snapshot may be from before the instance got another port.
    let prepared = match rift::provisioning::remote_prepare(&snapshot.config, instance) {
        Ok(prepare) => nodes
            .request(
                node,
                &Command::Prepare(Box::new(prepare)),
                Duration::from_secs(120),
            )
            .await
            .map(|_| ()),
        Err(error) => Err(error),
    };
    if let Err(error) = prepared {
        cleanup.push(format!("its port was not refreshed: {error}"));
    }
    Ok(json!({
        "name": instance,
        "node": node,
        "in_place": true,
        "cleanup_error": (!cleanup.is_empty()).then(|| cleanup.join("; ")),
    }))
}

/// Back up every [`eligible`] instance whose newest snapshot is older than
/// `backups.interval`, one at a time.
pub(crate) async fn schedule(current: watch::Receiver<Arc<Snapshot>>) {
    // Give agents a moment to connect after a restart.
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut known_target = None;
    let mut paused: Option<String> = None;
    let mut outdated = BTreeSet::new();
    loop {
        tick.tick().await;
        let snapshot = current.borrow().clone();
        let Some(backups) = snapshot.config.backups.clone() else {
            continue;
        };
        let Some(interval) = backups.interval else {
            continue;
        };
        if known_target.as_ref() != Some(&backups.target) {
            NEWEST.lock().unwrap_or_else(|e| e.into_inner()).clear();
            known_target = Some(backups.target.clone());
        }
        let store = match Store::open(&backups.target) {
            Ok(store) => {
                paused = None;
                store
            }
            Err(error) => {
                let message = error.to_string();
                if paused.as_ref() != Some(&message) {
                    log_warn!("scheduled backups are paused: {message}");
                    paused = Some(message);
                }
                continue;
            }
        };
        for instance in eligible(&snapshot.config, &backups) {
            // A backup takes a while: look at the instance as it is now.
            let snapshot = current.borrow().clone();
            let Ok(target) = target(&snapshot, &instance) else {
                continue;
            };
            if !snapshot.nodes.online(&target.node) || snapshot.managed.is_moving(&instance) {
                continue;
            }
            if let Err(error) = ready(&snapshot, &target.node) {
                if outdated.insert(target.node.clone()) {
                    log_warn!(
                        "instances on node {} are not backed up: {error}",
                        target.node
                    );
                }
                continue;
            }
            let started = now();
            let retry = RETRY.min(interval).as_secs();
            let failed_recently = ATTEMPTS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&instance)
                .is_some_and(|attempt| attempt.error.is_some() && started < attempt.unix + retry);
            if failed_recently {
                continue;
            }
            let newest = NEWEST
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&instance)
                .copied();
            let newest = match newest {
                Some(newest) => newest,
                None => match store.snapshots(&instance).await {
                    Ok(ids) => {
                        let newest = ids
                            .last()
                            .map(String::as_str)
                            .and_then(backup::snapshot_time)
                            .unwrap_or(0);
                        NEWEST
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(instance.clone(), newest);
                        newest
                    }
                    Err(error) => {
                        log_warn!("cannot list the snapshots of {instance}: {error}");
                        continue;
                    }
                },
            };
            if started.saturating_sub(newest) < interval.as_secs() {
                continue;
            }
            let lock = LOCK.lock().await;
            let result = take(&snapshot, &backups, &store, &target).await;
            drop(lock);
            if let Err(error) = &result {
                log_warn!("scheduled backup of {instance} failed: {error}");
            }
            record(&instance, result.err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "rift.config.listeners.public='127.0.0.1:0'
rift.config.routes.public='game'
rift.config.messaging={listen='127.0.0.1:0',certificate='c.pem',private_key='k.pem'}
rift.config.nodes.node1={address='10.0.0.1'}
rift.config.nodes.node2={address='10.0.0.2'}
rift.config.templates.t={server_jar='paper.jar'}
rift.config.service_groups.game={template='t',storage='disposable',port_range={30000,30099},nodes={'node1','node2'}}
rift.config.service_groups.world={template='t',storage='persistent',port_range={31000,31099},nodes={'node1','node2'}}
rift.config.backups={directory='%DIRECTORY%',keep=2}";

    fn snapshot(directory: &Path) -> Arc<Snapshot> {
        let source = SOURCE.replace("%DIRECTORY%", &directory.display().to_string());
        let mut config = Config::from_lua(&source, "backups.lua").unwrap();
        config
            .add_instance_on("game", "game-1", 30000, Some("node1"))
            .unwrap();
        config
            .add_instance_on("world", "world-1", 31000, Some("node1"))
            .unwrap();
        Arc::new(Snapshot::new(config, None).unwrap())
    }

    #[tokio::test]
    async fn backups_cover_persistent_node_instances_and_restores_need_an_online_node() {
        let root = std::env::temp_dir().join(format!("rift-backups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let snapshot = snapshot(&root);
        let (commands, _receiver) = mpsc::channel(1);
        let backups = snapshot.config.backups.as_ref().unwrap();
        assert_eq!(backups.keep, 2);
        assert_eq!(eligible(&snapshot.config, backups), ["world-1"]);
        assert_eq!(target(&snapshot, "world-1").unwrap().node, "node1");
        let error = target(&snapshot, "game-1").unwrap_err();
        assert!(error.contains("disposable"), "{error}");
        let error = target(&snapshot, "ghost-1").unwrap_err();
        assert!(error.contains("unknown service instance"), "{error}");
        // Neither node ever connected.
        let error = backup_now(snapshot.clone(), "world-1").await.unwrap_err();
        assert!(error.contains("node node1 is"), "{error}");
        let request = |node: Option<&str>| Restore {
            instance: "world-1".into(),
            snapshot: None,
            node: node.map(str::to_owned),
        };
        let error = restore(snapshot.clone(), &commands, request(None))
            .await
            .unwrap_err();
        assert!(error.contains("has no snapshot"), "{error}");
        // A snapshot taken earlier, say before node1 died.
        let store = Store::Directory(root.clone());
        let spool = store.spool();
        std::fs::create_dir_all(&spool).unwrap();
        let level = spool.join("level");
        std::fs::write(&level, b"level").unwrap();
        let sha256 = hex(&Sha256::digest(b"level"));
        store.put_object(&sha256, &level).await.unwrap();
        let id = backup::snapshot_id(1_795_005_296);
        store
            .put_manifest(&Manifest {
                version: backup::MANIFEST_VERSION,
                id: id.clone(),
                instance: "world-1".into(),
                group: "world".into(),
                node: "node1".into(),
                directory: "local/services/world-1".into(),
                created_unix: 1_795_005_296,
                flushed: true,
                files: 1,
                folders: 0,
                bytes: 5,
                entries: vec![Entry {
                    path: "level.dat".into(),
                    size: 5,
                    mode: 0o644,
                    sha256,
                    ..Entry::default()
                }],
            })
            .await
            .unwrap();
        let listing = list(snapshot.clone(), Some("world-1")).await.unwrap();
        assert_eq!(listing["scheduled"], true);
        assert_eq!(listing["snapshots"][0]["id"], id);
        assert_eq!(listing["snapshots"][0]["bytes"], 5);
        let overview = list(snapshot.clone(), None).await.unwrap();
        assert_eq!(overview["keep"], 2);
        assert_eq!(overview["instances"][0]["instance"], "world-1");
        assert_eq!(overview["instances"][0]["latest"], id);
        // In place needs node1; elsewhere needs the other node online.
        let error = restore(snapshot.clone(), &commands, request(None))
            .await
            .unwrap_err();
        assert!(error.contains("--node"), "{error}");
        let error = restore(snapshot.clone(), &commands, request(Some("node2")))
            .await
            .unwrap_err();
        assert!(error.contains("node node2"), "{error}");
        let error = forget(snapshot.clone(), "world-1", "19700101T000000Z")
            .await
            .unwrap_err();
        assert!(error.contains("no snapshot"), "{error}");
        let forgotten = forget(snapshot.clone(), "world-1", "all").await.unwrap();
        assert_eq!(forgotten["forgotten"], json!([id]));
        assert_eq!(forgotten["objects_deleted"], 1);
        assert!(store.objects().await.unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn parse(args: &[&str]) -> Result<Restore, String> {
        Restore::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn restore_requests_take_an_instance_a_snapshot_and_a_node() {
        assert_eq!(
            parse(&["survival-1"]).unwrap(),
            Restore {
                instance: "survival-1".into(),
                snapshot: None,
                node: None,
            }
        );
        assert_eq!(parse(&["survival-1", "latest"]).unwrap().snapshot, None);
        let request = parse(&["survival-1", "20261118T123456Z", "--node", "node2"]).unwrap();
        assert_eq!(request.snapshot.as_deref(), Some("20261118T123456Z"));
        assert_eq!(request.node.as_deref(), Some("node2"));
        assert_eq!(
            parse(&["--node", "node2", "survival-1"])
                .unwrap()
                .node
                .as_deref(),
            Some("node2")
        );
        for bad in [
            &[][..],
            &["survival-1", "a", "b"],
            &["survival-1", "--node"],
            &["survival-1", "--node", "a", "--node", "b"],
            &["survival-1", "--force"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
