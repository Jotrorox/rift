//! Configuration and files in a cluster (docs/clustering.md, "Configuration
//! and files"): plan step 8.
//!
//! - **Deployments** ([`config`]): a deployment made on any peer (the
//!   dashboard, `PUT /api/config`, a rollback, `rift admin reload` after an
//!   edit) is proposed as a `config` entry with its revision, source and
//!   actor; the leader stamps its time. Applying it records it in the
//!   replicated history and nothing else, so apply stays deterministic and
//!   fast. After the entry applied, each peer's deployer writes the source to
//!   its `rift.lua` atomically and reloads through the configuration
//!   service's ordinary reload path (the [`Reloader`] the binary installs).
//!   A peer whose reload fails keeps running its previous configuration and
//!   reports why; nothing else waits for it.
//! - **History and audit**: the deployment history and the operator audit
//!   trail are replicated tables; `rift cluster init` imports
//!   `deployments.json` and `audit.jsonl` ([`import`]).
//! - **Shared sections** ([`shared`]): before proposing, the proposer
//!   evaluates the candidate once per member name and refuses it if a
//!   shared section depends on `rift.peer`; applying a deployment refuses it
//!   unless it was evaluated for every member and they agree. Each peer
//!   reports the shared hash of what it runs, which handshakes carry, and
//!   [`status`] (`rift admin cluster`, `GET /api/cluster`) names the peers and
//!   sections that differ.
//! - **Startup** ([`startup`], decision 6): a peer whose `rift.lua` is the
//!   last deployment starts; one with an older deployed revision adopts the
//!   last one; one that matches no deployed revision refuses to start unless
//!   told to adopt; one without a `rift.lua` receives it.
//! - **Templates** ([`manifest`], [`blobs`]): `manifest` entries list the
//!   files of each template folder by path, size and SHA-256; contents are
//!   blobs under `cache/blobs/`. An upload, delete, git sync or import
//!   stores the blobs, copies them to a majority of voters and only then
//!   proposes the manifest, so a committed manifest never references a blob
//!   only one host has. Every peer materializes `template_storage` from the
//!   manifest, fetching missing blobs from peers that have them, so node
//!   agents fetch templates over `--web` from any peer. Git syncs run on the
//!   leader. Unreferenced blobs are collected after [`GC_GRACE`].
//!
//! # Wiring
//!
//! Create one [`FilesState`] per peer, register [`components`] in the
//! cluster's options before it starts (witnesses too: they apply every
//! entry), and call [`attach`] once the configuration path is known. The
//! binary installs the state as the process's [`global`] one, gives it a
//! [`Reloader`] and reports what it runs ([`FilesState::report_running`]).
//! Without a cluster none of this exists and everything works on local
//! files as before.
pub mod blobs;
pub mod config;
pub mod manifest;
pub mod shared;
#[cfg(test)]
mod tests;

use super::{
    Cluster, StreamKind, node,
    state::{ApplyContext, Changes, Component, Entry, EntryKind, Reply},
    transport::BoxFuture,
};
use crate::template_source::TemplateSource;
use blobs::{BlobRef, Request, Store};
use bytes::Bytes;
use config::{ConfigEntry, Deployment, ImportedDeployment, PeerReport};
use manifest::{FileRecord, Folder, ManifestEntry};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{Value, json};
use shared::Shared;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, Weak},
    time::Duration,
};
use tokio::{
    sync::{Notify, oneshot, watch},
    task::JoinHandle,
};

/// How often unreferenced blobs are looked for.
const GC_INTERVAL: Duration = Duration::from_secs(600);
/// How old an unreferenced blob must be before it is removed: uploads store
/// their blobs before the manifest that references them commits.
pub const GC_GRACE: Duration = Duration::from_secs(3600);
/// How soon an incomplete materialization is tried again.
const RETRY: Duration = Duration::from_secs(30);
/// How long a deployment this peer proposed may take to apply here before
/// the deployer reloads it itself.
const LOCAL_GRACE: Duration = Duration::from_secs(30);
/// How long a proposer waits for its deployer to write the file.
const ACK_WAIT: Duration = Duration::from_secs(10);
/// How long a git sync forwarded to the leader may take.
const SYNC_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Attempts of a change computed against a listing another change replaced.
const ATTEMPTS: usize = 5;
/// The folders this peer materialized, below its directory.
const MATERIALIZED: &str = "cache/materialized.json";

/// Applies the deployed configuration on this peer through the
/// configuration service's ordinary reload; `Err` says why it was refused.
pub type Reloader = Arc<dyn Fn() -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

#[derive(Clone, Debug, Default)]
struct Paths {
    config: Option<PathBuf>,
    storage: Option<PathBuf>,
    cache: Option<PathBuf>,
    sources: BTreeMap<String, TemplateSource>,
}

/// What this peer runs, as the binary reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Running {
    digest: String,
    source: Arc<str>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(io::Error::other)?
}

/// An opaque content revision of a source, as the configuration service
/// shows it (`control::revision`).
pub fn revision(source: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hash);
    format!("{:016x}", hash.finish())
}

/// The configuration and file state of one peer. Create it before the
/// cluster starts.
pub struct FilesState {
    /// The peer's directory: given, or the cluster's once connected.
    directory: OnceLock<PathBuf>,
    blobs: OnceLock<Arc<Store>>,
    cluster: RwLock<Weak<node::Inner>>,
    paths: RwLock<Paths>,
    deploy_wake: Notify,
    materialize_wake: Notify,
    report_wake: Notify,
    reloader: RwLock<Option<Reloader>>,
    /// Deployments this peer proposed, by digest: the transaction that
    /// proposed one applies it itself once its file is written.
    local: Mutex<HashMap<String, oneshot::Sender<()>>>,
    /// The digest of the deployment the last reload was requested for.
    attempted: Mutex<Option<String>>,
    running: watch::Sender<Option<Running>>,
    /// Why the last reload of a deployment failed.
    failure: Mutex<Option<String>>,
    /// Serializes materialization, git syncs and publishing a folder.
    folders: tokio::sync::Mutex<()>,
    #[cfg(test)]
    refuse_blobs: std::sync::atomic::AtomicBool,
}

impl FilesState {
    /// The state of the peer whose `rift.lua` (and `cluster.toml`) live in
    /// `directory`; its blobs go to `directory/cache/blobs`.
    pub fn new(directory: &Path) -> Arc<Self> {
        let state = Self::for_peer();
        let _ = state.directory.set(directory.to_owned());
        state
    }

    /// The state of the peer that starts next, whose directory is its
    /// cluster's (`cluster.toml` beside `rift.lua`) once it is attached.
    pub fn for_peer() -> Arc<Self> {
        Arc::new(Self {
            directory: OnceLock::new(),
            blobs: OnceLock::new(),
            cluster: RwLock::new(Weak::new()),
            paths: RwLock::new(Paths::default()),
            deploy_wake: Notify::new(),
            materialize_wake: Notify::new(),
            report_wake: Notify::new(),
            reloader: RwLock::new(None),
            local: Mutex::new(HashMap::new()),
            attempted: Mutex::new(None),
            running: watch::Sender::new(None),
            failure: Mutex::new(None),
            folders: tokio::sync::Mutex::new(()),
            #[cfg(test)]
            refuse_blobs: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn directory(&self) -> PathBuf {
        self.directory
            .get()
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// This peer's blob store, `cache/blobs` in its directory.
    pub fn blobs(&self) -> &Arc<Store> {
        self.blobs
            .get_or_init(|| Arc::new(Store::new(self.directory().join("cache").join("blobs"))))
    }

    fn cluster(&self) -> io::Result<Cluster> {
        let inner = self
            .cluster
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "the configuration component is not attached to a running cluster",
                )
            })?;
        Ok(Cluster { inner })
    }

    fn connect(&self, cluster: &Cluster) {
        let _ = self.directory.set(cluster.settings().directory.clone());
        *self.cluster.write().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&cluster.inner);
    }

    fn paths(&self) -> Paths {
        self.paths.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Read the replicated state off the runtime's threads.
    async fn read<T: Send + 'static>(
        &self,
        read: impl FnOnce(&Connection) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let cluster = self.cluster()?;
        blocking(move || cluster.state().read(read)).await
    }

    /// Follow a reloaded configuration: where `rift.lua`, template storage
    /// and the software cache are, and which git template sources exist.
    pub fn configure(&self, config: &crate::config::Config, path: Option<&Path>) {
        let moved = {
            let mut paths = self.paths.write().unwrap_or_else(|e| e.into_inner());
            let moved = paths.storage.as_deref() != Some(config.template_storage.as_path());
            if let Some(path) = path {
                paths.config = Some(path.to_owned());
            }
            paths.storage = Some(config.template_storage.clone());
            paths.cache = Some(config.software_cache.clone());
            paths.sources = config.template_sources.clone();
            moved
        };
        if moved {
            self.materialize_wake.notify_one();
        }
    }

    /// How this peer applies a deployment: the configuration service's
    /// reload. Set once the service runs; a deployment that arrived before
    /// is applied then.
    pub fn set_reloader(&self, reloader: Reloader) {
        *self.reloader.write().unwrap_or_else(|e| e.into_inner()) = Some(reloader);
        self.deploy_wake.notify_one();
    }

    /// The source this peer runs now (after every published reload).
    pub fn report_running(&self, source: &str) {
        let digest = shared::sha256(source.as_bytes());
        let changed = self.running.send_if_modified(|running| {
            if running
                .as_ref()
                .is_some_and(|running| running.digest == digest)
            {
                false
            } else {
                *running = Some(Running {
                    digest: digest.clone(),
                    source: Arc::from(source),
                });
                true
            }
        });
        if changed {
            *lock(&self.failure) = None;
            self.report_wake.notify_one();
            self.deploy_wake.notify_one();
        }
    }

    fn running_digest(&self) -> Option<String> {
        self.running
            .borrow()
            .as_ref()
            .map(|running| running.digest.clone())
    }

    /// The last deployment.
    pub fn latest_deployment(&self) -> io::Result<Option<Deployment>> {
        self.cluster()?.state().read(config::latest)
    }

    /// Whether `source` is a retained deployment.
    pub fn was_deployed(&self, source: &str) -> io::Result<bool> {
        let digest = shared::sha256(source.as_bytes());
        Ok(self
            .cluster()?
            .state()
            .read(config::deployments)?
            .iter()
            .any(|deployment| deployment.digest == digest))
    }

    /// The deployment history as `GET /api/deployments` shows it.
    pub fn deployments_value(&self) -> io::Result<Value> {
        let deployments = self.cluster()?.state().read(config::deployments)?;
        Ok(json!({
            "deployments": deployments.iter().map(|deployment| json!({
                "id": deployment.id,
                "timestamp_unix_ms": deployment.time_ms,
                "actor": deployment.actor,
                "action": deployment.action,
                "revision": deployment.revision,
            })).collect::<Vec<_>>(),
            "retained": config::HISTORY,
            "durable": true,
            "replicated": true,
        }))
    }

    /// The source of retained deployment `id`.
    pub fn deployment_source(&self, id: u64) -> io::Result<Option<String>> {
        self.cluster()?
            .state()
            .read(move |db| config::source(db, id))
    }

    /// The replicated operator audit records, oldest first.
    pub fn audit_records(&self) -> io::Result<Vec<Value>> {
        self.cluster()?.state().read(config::audit_records)
    }

    /// Append an operator audit record to the replicated trail.
    pub async fn audit(&self, record: Value) -> io::Result<()> {
        let cluster = self.cluster()?;
        match cluster
            .propose(
                ConfigEntry::Audit {
                    records: vec![record],
                }
                .entry(),
            )
            .await?
        {
            Reply::Ok(_) => Ok(()),
            Reply::Err(reason) => Err(io::Error::other(reason)),
        }
    }

    /// Deploy `source` (the configuration at `path`) to every peer: evaluate
    /// it once per member and refuse it if a shared section differs, propose
    /// it, and return its id once it applied here and this peer's deployer
    /// wrote the file. The caller applies it on this peer itself (it is the
    /// configuration transaction that asked); every other peer reloads.
    pub async fn deploy(
        &self,
        source: &str,
        path: &Path,
        actor: &str,
        action: &str,
    ) -> Result<u64, String> {
        self.propose_deployment(source, path, actor, action, true)
            .await
    }

    async fn propose_deployment(
        &self,
        source: &str,
        path: &Path,
        actor: &str,
        action: &str,
        local: bool,
    ) -> Result<u64, String> {
        let cluster = self.cluster().map_err(|error| error.to_string())?;
        if source.len() > config::MAX_SOURCE {
            return Err("configuration exceeds 256 KiB source limit".into());
        }
        let digest = shared::sha256(source.as_bytes());
        let mut refusal = String::new();
        for _ in 0..3 {
            let mut members = self
                .read(config::member_names)
                .await
                .map_err(|error| error.to_string())?;
            members.insert(cluster.name().to_owned());
            let (shared, hashes) = {
                let source = source.to_owned();
                let path = path.to_owned();
                tokio::task::spawn_blocking(move || shared::validate(&source, &path, &members))
                    .await
                    .map_err(|error| error.to_string())??
            };
            let acknowledged = local.then(|| {
                let (sender, receiver) = oneshot::channel();
                lock(&self.local).insert(digest.clone(), sender);
                receiver
            });
            let entry = ConfigEntry::Deploy {
                actor: actor.to_owned(),
                action: action.to_owned(),
                revision: revision(source),
                source: source.to_owned(),
                shared,
                members: hashes,
            }
            .entry();
            match cluster.propose(entry).await {
                Ok(Reply::Ok(value)) => {
                    if let Some(acknowledged) = acknowledged {
                        let _ = tokio::time::timeout(ACK_WAIT, acknowledged).await;
                    }
                    return Ok(value["id"].as_u64().unwrap_or(0));
                }
                Ok(Reply::Err(reason)) => {
                    lock(&self.local).remove(&digest);
                    if reason.contains(config::NOT_VALIDATED) {
                        refusal = reason;
                        continue;
                    }
                    return Err(reason);
                }
                Err(error) => {
                    lock(&self.local).remove(&digest);
                    return Err(format!("the deployment was not committed: {error}"));
                }
            }
        }
        Err(refusal)
    }

    /// Write the last deployment to `rift.lua` and have it applied here.
    async fn apply_latest(self: &Arc<Self>) -> io::Result<()> {
        let Some(latest) = self.read(config::latest).await? else {
            return Ok(());
        };
        let Some(path) = self.paths().config else {
            return Ok(());
        };
        let written = {
            let path = path.clone();
            let source = latest.source.clone();
            blocking(move || write_if_changed(&path, &source)).await?
        };
        if written {
            log_info!(
                "cluster: wrote deployment #{} (revision {}, by {}) to {}",
                latest.id,
                latest.revision,
                latest.actor,
                path.display()
            );
        }
        let proposed_here = lock(&self.local).remove(&latest.digest);
        if let Some(acknowledge) = proposed_here {
            let _ = acknowledge.send(());
            let state = self.clone();
            let digest = latest.digest.clone();
            tokio::spawn(async move { state.await_local(digest).await });
            return Ok(());
        }
        if self.running_digest().as_deref() == Some(latest.digest.as_str()) {
            return Ok(());
        }
        self.request_reload(&latest.digest);
        Ok(())
    }

    /// The transaction that proposed `digest` applies it; if it has not
    /// after [`LOCAL_GRACE`] (it failed after the commit), reload.
    async fn await_local(self: Arc<Self>, digest: String) {
        let mut running = self.running.subscribe();
        let applied = tokio::time::timeout(
            LOCAL_GRACE,
            running.wait_for(|running| {
                running
                    .as_ref()
                    .is_some_and(|running| running.digest == digest)
            }),
        )
        .await
        .is_ok_and(|result| result.is_ok());
        if !applied {
            self.request_reload(&digest);
        }
    }

    /// Ask the configuration service to reload (once per deployment).
    fn request_reload(self: &Arc<Self>, digest: &str) {
        if lock(&self.attempted).as_deref() == Some(digest) {
            return;
        }
        let reloader = self
            .reloader
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(reloader) = reloader else {
            log_debug!("cluster: a deployment waits until the configuration service runs");
            return;
        };
        *lock(&self.attempted) = Some(digest.to_owned());
        let state = self.clone();
        tokio::spawn(async move {
            if let Err(error) = reloader().await {
                log_warn!(
                    "cluster: this peer could not apply the deployed configuration and keeps running its previous one: {error}"
                );
                *lock(&state.failure) = Some(error);
                state.report_wake.notify_one();
            }
        });
    }

    /// Propose what this peer runs.
    async fn report(
        &self,
        running: &Running,
        shared: Shared,
        error: Option<String>,
    ) -> io::Result<()> {
        let cluster = self.cluster()?;
        let reply = cluster
            .propose(
                ConfigEntry::Report {
                    peer: cluster.name().to_owned(),
                    digest: running.digest.clone(),
                    shared,
                    error,
                }
                .entry(),
            )
            .await?;
        if let Reply::Err(reason) = reply {
            log_warn!("cluster: the configuration report was refused: {reason}");
        }
        Ok(())
    }

    /// The shared sections of `source` as this peer evaluates it.
    async fn evaluate_running(&self, source: Arc<str>) -> io::Result<Shared> {
        let cluster = self.cluster()?;
        let Some(path) = self.paths().config else {
            return Ok(Shared::default());
        };
        let name = cluster.name().to_owned();
        let evaluated = blocking(move || Ok(shared::evaluate(&source, &path, &name))).await?;
        Ok(evaluated.unwrap_or_else(|error| {
            log_warn!("cluster: hashing the shared configuration sections: {error}");
            Shared::default()
        }))
    }

    /// One pass over the manifest: fetch missing blobs, write every folder
    /// whose blobs are here, remove folders that left the manifest. Returns
    /// whether everything is in place.
    pub async fn materialize(&self) -> io::Result<bool> {
        let _folders = self.folders.lock().await;
        self.materialize_locked().await
    }

    async fn materialize_locked(&self) -> io::Result<bool> {
        let Some(storage) = self.paths().storage else {
            return Ok(true);
        };
        let cluster = self.cluster()?;
        let folders = self.read(manifest::folders).await?;
        let names: BTreeSet<String> = folders.keys().cloned().collect();
        let mut complete = true;
        let mut ready = BTreeMap::new();
        for (name, folder) in folders {
            let mut fetched = true;
            for file in &folder.files {
                if self.blobs().has(&file.sha256) {
                    continue;
                }
                let blob = BlobRef {
                    sha256: file.sha256.clone(),
                    size: file.size,
                };
                if let Err(error) = blobs::fetch(&cluster, self.blobs(), &blob, None).await {
                    log_warn!("cluster: template folder {name}: {error}");
                    fetched = false;
                    break;
                }
            }
            if fetched {
                ready.insert(name, folder);
            } else {
                complete = false;
            }
        }
        let blobs = self.blobs().clone();
        let record = self.directory().join(MATERIALIZED);
        let written =
            blocking(move || write_folders(&storage, &record, &ready, &names, &blobs)).await?;
        Ok(complete && written)
    }

    /// Remove unreferenced blobs older than `grace`.
    pub async fn collect_garbage(&self, grace: Duration) -> io::Result<usize> {
        let referenced = self.read(manifest::referenced).await?;
        let blobs = self.blobs().clone();
        blocking(move || blobs.collect_garbage(&referenced, grace)).await
    }

    /// Copy `wanted` (stored here) to a majority of the voters.
    async fn replicate(&self, cluster: &Cluster, wanted: &[BlobRef]) -> io::Result<()> {
        let membership = cluster.membership().ok_or_else(super::node::not_joined)?;
        let voters: Vec<String> = membership
            .voter_ids()
            .filter_map(|id| membership.get_node(&id).map(|node| node.name.clone()))
            .collect();
        let need = voters.len() / 2 + 1;
        let me = cluster.name().to_owned();
        let mut have = usize::from(voters.contains(&me));
        if have >= need || wanted.is_empty() {
            return Ok(());
        }
        let body = Bytes::from(
            serde_json::to_vec(&Request::Replicate {
                from: me.clone(),
                blobs: wanted.to_vec(),
            })
            .map_err(io::Error::other)?,
        );
        let total: u64 = wanted.iter().map(|blob| blob.size).sum();
        // At least 1 MiB/s, plus a minute.
        let timeout = Duration::from_secs(60 + total / (1024 * 1024));
        let mut requests = tokio::task::JoinSet::new();
        for voter in voters.into_iter().filter(|voter| *voter != me) {
            let links = cluster.links().clone();
            let body = body.clone();
            requests.spawn(async move {
                let result = links.request(&voter, StreamKind::Blob, body, timeout).await;
                (voter, result)
            });
        }
        let mut failures = Vec::new();
        while let Some(joined) = requests.join_next().await {
            match joined {
                Ok((_, Ok(_))) => {
                    have += 1;
                    if have >= need {
                        return Ok(());
                    }
                }
                Ok((voter, Err(error))) => failures.push(format!("{voter}: {error}")),
                Err(error) => failures.push(error.to_string()),
            }
        }
        Err(io::Error::other(format!(
            "the files reached {have} of the {need} voters a manifest needs, so it was not proposed: {}",
            failures.join("; ")
        )))
    }

    /// Store `body` at `segments` (or create a folder there) in the
    /// replicated template storage: `PUT /api/files/templates/...`.
    pub async fn put_template(
        &self,
        segments: &[String],
        directory: bool,
        body: &[u8],
    ) -> io::Result<Value> {
        let cluster = self.cluster()?;
        crate::files::protect(segments)?;
        let Some((folder, rest)) = segments.split_first() else {
            return Err(invalid("choose a path inside the folder"));
        };
        if folder.starts_with('.') {
            return Err(invalid(
                "top-level names starting with a dot are not replicated in a cluster",
            ));
        }
        let rest = rest.join("/");
        let record = if directory {
            None
        } else {
            let blobs = self.blobs().clone();
            let body = body.to_vec();
            let blob = blocking(move || blobs.store_bytes(&body)).await?;
            self.replicate(&cluster, std::slice::from_ref(&blob))
                .await?;
            Some(FileRecord {
                path: rest.clone(),
                size: blob.size,
                sha256: blob.sha256,
            })
        };
        for attempt in 0..ATTEMPTS {
            let current = {
                let folder = folder.clone();
                self.read(move |db| manifest::folder(db, &folder)).await?
            };
            let expected = current.as_ref().map_or(0, |folder| folder.version);
            let listing = match &record {
                Some(record) => with_file(folder, current.as_ref(), record.clone())?,
                None => with_directory(folder, current.as_ref(), &rest)?,
            };
            let Some((files, directories)) = listing else {
                break;
            };
            let entry = ManifestEntry::Put {
                folder: folder.clone(),
                expected: Some(expected),
                files,
                directories,
            };
            match cluster.propose(entry.entry()).await? {
                Reply::Ok(_) => break,
                Reply::Err(reason)
                    if reason.contains(manifest::CONCURRENT) && attempt + 1 < ATTEMPTS => {}
                Reply::Err(reason) => return Err(io::Error::other(reason)),
            }
        }
        if let Err(error) = self.materialize().await {
            log_warn!("cluster: materializing template_storage: {error}");
        }
        let path = segments.join("/");
        Ok(match &record {
            Some(record) => json!({"path": path, "type": "file", "size": record.size}),
            None => json!({"path": path, "type": "directory"}),
        })
    }

    /// Delete `segments` from the replicated template storage: `DELETE
    /// /api/files/templates/...`.
    pub async fn delete_template(&self, segments: &[String]) -> io::Result<Value> {
        let cluster = self.cluster()?;
        crate::files::protect(segments)?;
        let Some((folder, rest)) = segments.split_first() else {
            return Err(invalid("the folder root cannot be deleted"));
        };
        let rest = rest.join("/");
        for attempt in 0..ATTEMPTS {
            let current = {
                let folder = folder.clone();
                self.read(move |db| manifest::folder(db, &folder)).await?
            };
            let Some(current) = current else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} does not exist", segments.join("/")),
                ));
            };
            let entry = if rest.is_empty() {
                ManifestEntry::Remove {
                    folder: folder.clone(),
                    expected: Some(current.version),
                }
            } else {
                let (files, directories) = without(&current, &rest).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{} does not exist", segments.join("/")),
                    )
                })?;
                ManifestEntry::Put {
                    folder: folder.clone(),
                    expected: Some(current.version),
                    files,
                    directories,
                }
            };
            match cluster.propose(entry.entry()).await? {
                Reply::Ok(_) => break,
                Reply::Err(reason)
                    if reason.contains(manifest::CONCURRENT) && attempt + 1 < ATTEMPTS => {}
                Reply::Err(reason) => return Err(io::Error::other(reason)),
            }
        }
        if let Err(error) = self.materialize().await {
            log_warn!("cluster: materializing template_storage: {error}");
        }
        Ok(json!({"deleted": segments.join("/")}))
    }

    /// Publish the folder `name` of this peer's template storage as it is:
    /// store its files as blobs, copy them to a majority of voters, propose
    /// its manifest.
    pub async fn publish_folder(&self, name: &str) -> io::Result<()> {
        let _folders = self.folders.lock().await;
        self.publish_locked(name).await
    }

    async fn publish_locked(&self, name: &str) -> io::Result<()> {
        let cluster = self.cluster()?;
        let storage = self.paths().storage.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "this peer has no template_storage")
        })?;
        let (files, directories) = {
            let blobs = self.blobs().clone();
            let name = name.to_owned();
            blocking(move || manifest::scan(&storage, &name, &blobs)).await?
        };
        let unique: BTreeMap<&str, u64> = files
            .iter()
            .map(|file| (file.sha256.as_str(), file.size))
            .collect();
        let wanted: Vec<BlobRef> = unique
            .into_iter()
            .map(|(sha256, size)| BlobRef {
                sha256: sha256.to_owned(),
                size,
            })
            .collect();
        self.replicate(&cluster, &wanted).await?;
        let entry = ManifestEntry::Put {
            folder: name.to_owned(),
            expected: None,
            files,
            directories,
        };
        match cluster.propose(entry.entry()).await? {
            Reply::Ok(_) => Ok(()),
            Reply::Err(reason) => Err(io::Error::other(reason)),
        }
    }

    /// Sync (with `ensure`: check out if needed) a git template source on
    /// the leader and publish its folder; forwarded when this peer does not
    /// lead. Returns the source's status.
    pub async fn sync_template_source(
        &self,
        name: &str,
        source: &TemplateSource,
        ensure: bool,
    ) -> io::Result<Value> {
        let cluster = self.cluster()?;
        let leadership = cluster.leadership();
        if leadership.is_leader {
            return self.sync_here(name, source, ensure).await;
        }
        let leader = leadership.leader_name.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "git syncs run on the leader and there is none; retry once one is elected",
            )
        })?;
        let body = serde_json::to_vec(&Request::Sync {
            name: name.to_owned(),
            url: source.url.clone(),
            reference: source.reference.clone(),
            revision: source.revision.clone(),
            ensure,
        })
        .map_err(io::Error::other)?;
        let reply = cluster
            .links()
            .request(&leader, StreamKind::Blob, Bytes::from(body), SYNC_TIMEOUT)
            .await?;
        serde_json::from_slice(&reply).map_err(io::Error::other)
    }

    async fn sync_here(
        &self,
        name: &str,
        source: &TemplateSource,
        ensure: bool,
    ) -> io::Result<Value> {
        let paths = self.paths();
        let (Some(storage), Some(cache)) = (paths.storage, paths.cache) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the leader has no template_storage configured yet",
            ));
        };
        let _folders = self.folders.lock().await;
        let (status, synced) = if ensure {
            let synced =
                crate::template_source::ensure_local(&storage, &cache, name, source).await?;
            (
                crate::template_source::status(&storage, &cache, name, source),
                synced,
            )
        } else {
            (
                crate::template_source::sync_local(&storage, &cache, name, source).await?,
                true,
            )
        };
        let published = {
            let name = name.to_owned();
            self.read(move |db| manifest::folder(db, &name))
                .await?
                .is_some()
        };
        if synced || !published {
            self.publish_locked(name).await?;
        }
        Ok(status)
    }

    /// On becoming leader: check out and publish the configured git
    /// template sources the manifest does not have yet.
    async fn leader_gained(self: Arc<Self>, context: node::LeaderContext) {
        let sources = self.paths().sources;
        if sources.is_empty() {
            return;
        }
        let present = match self.read(manifest::folders).await {
            Ok(present) => present,
            Err(error) => {
                log_warn!("cluster: template sources: {error}");
                return;
            }
        };
        for (name, source) in sources {
            if present.contains_key(&name) {
                continue;
            }
            if !context.has_lease() {
                return;
            }
            if let Err(error) = self.sync_here(&name, &source, true).await {
                log_warn!("cluster: template source {name}: {error}");
            }
        }
    }

    /// Tests make a peer refuse to store replicated blobs.
    #[cfg(test)]
    fn refuses_blobs(&self) -> bool {
        self.refuse_blobs.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(not(test))]
    fn refuses_blobs(&self) -> bool {
        false
    }

    /// Answer a request on the `blob` stream.
    async fn serve(self: Arc<Self>, body: Bytes) -> Result<Bytes, String> {
        let request: Request = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
        match request {
            Request::Has { hashes } => {
                let present: Vec<bool> = hashes.iter().map(|hash| self.blobs().has(hash)).collect();
                serde_json::to_vec(&present)
                    .map(Bytes::from)
                    .map_err(|error| error.to_string())
            }
            Request::Get {
                sha256,
                offset,
                length,
            } => {
                let store = self.blobs().clone();
                blocking(move || store.read_chunk(&sha256, offset, length))
                    .await
                    .map(Bytes::from)
                    .map_err(|error| error.to_string())
            }
            Request::Replicate {
                from,
                blobs: wanted,
            } => {
                if self.refuses_blobs() {
                    return Err("this peer refuses blobs (test)".into());
                }
                let cluster = self.cluster().map_err(|error| error.to_string())?;
                for blob in &wanted {
                    blobs::fetch(&cluster, self.blobs(), blob, Some(&from))
                        .await
                        .map_err(|error| error.to_string())?;
                }
                Ok(Bytes::from(json!({ "stored": wanted.len() }).to_string()))
            }
            Request::Sync {
                name,
                url,
                reference,
                revision,
                ensure,
            } => {
                let cluster = self.cluster().map_err(|error| error.to_string())?;
                if !cluster.leadership().is_leader {
                    return Err(format!("{} no longer leads; retry", cluster.name()));
                }
                let source = TemplateSource {
                    url,
                    reference,
                    revision,
                };
                let status = self
                    .sync_here(&name, &source, ensure)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(Bytes::from(status.to_string()))
            }
        }
    }
}

/// The listing of `folder` with `record` stored; `None` if nothing changes.
type Listing = (Vec<FileRecord>, Vec<String>);

fn with_file(
    folder: &str,
    current: Option<&Folder>,
    record: FileRecord,
) -> io::Result<Option<Listing>> {
    let (mut files, directories) = current
        .map(|current| (current.files.clone(), current.directories.clone()))
        .unwrap_or_default();
    if record.path.is_empty() {
        if current.is_some_and(|current| !current.is_file()) {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "a folder already exists at this path",
            ));
        }
        return Ok(Some((vec![record], Vec::new())));
    }
    if current.is_some_and(Folder::is_file) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{folder} is not a folder"),
        ));
    }
    let mut prefix = String::new();
    for segment in record.path.split('/') {
        if !prefix.is_empty() {
            if files.iter().any(|file| file.path == prefix) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{prefix} is not a folder"),
                ));
            }
            prefix.push('/');
        }
        prefix.push_str(segment);
    }
    let nested = format!("{}/", record.path);
    if directories
        .iter()
        .any(|directory| *directory == record.path || directory.starts_with(&nested))
        || files.iter().any(|file| file.path.starts_with(&nested))
    {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "a folder already exists at this path",
        ));
    }
    files.retain(|file| file.path != record.path);
    files.push(record);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Some((files, directories)))
}

fn with_directory(
    folder: &str,
    current: Option<&Folder>,
    path: &str,
) -> io::Result<Option<Listing>> {
    if current.is_some_and(Folder::is_file) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{folder} is not a folder"),
        ));
    }
    let (files, mut directories) = current
        .map(|current| (current.files.clone(), current.directories.clone()))
        .unwrap_or_default();
    if path.is_empty() {
        return Ok(current.is_none().then_some((files, directories)));
    }
    let mut prefix = String::new();
    for segment in path.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        if files.iter().any(|file| file.path == prefix) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{prefix} is not a folder"),
            ));
        }
    }
    let nested = format!("{path}/");
    if directories
        .iter()
        .any(|directory| directory == path || directory.starts_with(&nested))
        || files.iter().any(|file| file.path.starts_with(&nested))
    {
        return Ok(None);
    }
    directories.push(path.to_owned());
    directories.sort();
    Ok(Some((files, directories)))
}

/// The listing of `current` without `path` and what lies below it, keeping
/// its parent folder; `None` if there is no such path.
fn without(current: &Folder, path: &str) -> Option<Listing> {
    let nested = format!("{path}/");
    let mut files = current.files.clone();
    let mut directories = current.directories.clone();
    let before = files.len() + directories.len();
    files.retain(|file| file.path != path && !file.path.starts_with(&nested));
    directories.retain(|directory| directory != path && !directory.starts_with(&nested));
    if files.len() + directories.len() == before {
        return None;
    }
    if let Some((parent, _)) = path.rsplit_once('/')
        && !directories.iter().any(|directory| directory == parent)
    {
        directories.push(parent.to_owned());
        directories.sort();
    }
    Some((files, directories))
}

/// Write `contents` to `path` atomically unless it holds them already;
/// returns whether it wrote.
fn write_if_changed(path: &Path, contents: &str) -> io::Result<bool> {
    match fs::read(path) {
        Ok(current) if current == contents.as_bytes() => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    write_atomic(path, contents.as_bytes())?;
    Ok(true)
}

/// Replace `path` with `contents` through a flushed sibling file, keeping
/// the access permissions of the file it replaces.
fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| invalid(format!("{}: has no file name", path.display())))?
        .to_string_lossy()
        .into_owned();
    let temporary = parent.join(format!(
        ".{name}.rift-deploy-{}.tmp",
        blobs::random_suffix()?
    ));
    let permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| -> io::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()
    })();
    // The handle is closed here, before the rename (Windows).
    let placed = written.and_then(|()| fs::rename(&temporary, path));
    if placed.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    placed
}

#[derive(serde::Deserialize, Serialize, Default)]
struct MaterializedRecord {
    storage: PathBuf,
    folders: BTreeSet<String>,
}

/// Write the folders in `ready`, remove those this peer materialized before
/// that `names` no longer lists, and remember `names`. Returns whether
/// every step worked.
fn write_folders(
    storage: &Path,
    record: &Path,
    ready: &BTreeMap<String, Folder>,
    names: &BTreeSet<String>,
    blobs: &Store,
) -> io::Result<bool> {
    let previous: BTreeSet<String> = fs::read(record)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<MaterializedRecord>(&bytes).ok())
        .filter(|previous| previous.storage == storage)
        .map(|previous| previous.folders)
        .unwrap_or_default();
    let mut complete = true;
    for (name, folder) in ready {
        if let Err(error) = manifest::materialize(storage, name, folder, blobs) {
            log_warn!(
                "cluster: writing template folder {name} into {}: {error}",
                storage.display()
            );
            complete = false;
        }
    }
    for name in previous.difference(names) {
        if let Err(error) = manifest::remove(storage, name) {
            log_warn!("cluster: removing template folder {name}: {error}");
            complete = false;
        }
    }
    if let Some(parent) = record.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_vec(&MaterializedRecord {
        storage: storage.to_owned(),
        folders: names.clone(),
    })
    .map_err(io::Error::other)?;
    write_atomic(record, &text)?;
    Ok(complete)
}

/// The component every peer registers: `config` and `manifest` entries.
struct Tables {
    state: Arc<FilesState>,
}

impl Component for Tables {
    fn name(&self) -> &'static str {
        "files"
    }

    fn kinds(&self) -> Vec<EntryKind> {
        vec![EntryKind::Config, EntryKind::Manifest]
    }

    fn apply(&self, db: &Connection, entry: &Entry, ctx: &mut ApplyContext) -> io::Result<Reply> {
        match entry {
            Entry::Config(payload) => config::apply(db, payload, ctx),
            Entry::Manifest(payload) => manifest::apply(db, payload, ctx),
            other => Ok(Reply::err(format!(
                "component files does not apply {} entries",
                other.kind()
            ))),
        }
    }

    fn after_apply(&self, _db: &Connection, changes: &Changes) {
        if changes.touched(config::DEPLOYMENTS) {
            self.state.deploy_wake.notify_one();
        }
        if changes.touched(manifest::FOLDERS) {
            self.state.materialize_wake.notify_one();
        }
    }

    fn full_reload(&self, _db: &Connection) -> io::Result<()> {
        self.state.deploy_wake.notify_one();
        self.state.materialize_wake.notify_one();
        Ok(())
    }
}

/// The components every peer registers before its cluster starts.
pub fn components(state: &Arc<FilesState>) -> Vec<Arc<dyn Component>> {
    vec![Arc::new(Tables {
        state: state.clone(),
    })]
}

/// What [`attach`] needs to know about this peer.
#[derive(Clone, Debug, Default)]
pub struct AttachOptions {
    /// `rift.lua`; without one (a witness) deployments are recorded only.
    pub config_path: Option<PathBuf>,
    /// `template_storage`; without one nothing is materialized.
    pub template_storage: Option<PathBuf>,
    /// `software_cache`, where git repositories of template sources live.
    pub software_cache: Option<PathBuf>,
    pub template_sources: BTreeMap<String, TemplateSource>,
}

impl AttachOptions {
    /// The options of a peer running `config` from `path`.
    pub fn from_config(config: &crate::config::Config, path: Option<&Path>) -> Self {
        Self {
            config_path: path.map(Path::to_owned),
            template_storage: Some(config.template_storage.clone()),
            software_cache: Some(config.software_cache.clone()),
            template_sources: config.template_sources.clone(),
        }
    }
}

/// The tasks of an attached state. Dropping it stops them.
pub struct Attached {
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Attached {
    fn drop(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

/// Connect `state` (whose [`components`] `cluster` was started with) to the
/// running cluster: serve and fetch blobs, apply deployments, materialize
/// templates, report what this peer runs, collect garbage and run git syncs
/// while leading.
pub async fn attach(
    cluster: &Cluster,
    state: &Arc<FilesState>,
    options: AttachOptions,
) -> io::Result<Attached> {
    if state
        .cluster()
        .is_ok_and(|attached| !Arc::ptr_eq(&attached.inner, &cluster.inner))
    {
        return Err(super::invalid(
            "cluster files: this state is attached to another running cluster",
        ));
    }
    state.connect(cluster);
    *state.paths.write().unwrap_or_else(|e| e.into_inner()) = Paths {
        config: options.config_path,
        storage: options.template_storage,
        cache: options.software_cache,
        sources: options.template_sources,
    };
    let handler = state.clone();
    cluster
        .links()
        .on_request(StreamKind::Blob, move |_peer, body: Bytes| {
            let state = handler.clone();
            async move { state.serve(body).await }
        });
    let gain = state.clone();
    cluster.on_leader_gain(move |context| {
        let state = gain.clone();
        async move { state.leader_gained(context).await }
    });
    let tasks = vec![
        tokio::spawn(deployer(state.clone())),
        tokio::spawn(materializer(state.clone())),
        tokio::spawn(reporter(state.clone())),
        tokio::spawn(collector(state.clone())),
    ];
    state.deploy_wake.notify_one();
    state.materialize_wake.notify_one();
    Ok(Attached { tasks })
}

static GLOBAL: OnceLock<Arc<FilesState>> = OnceLock::new();

/// Make `state` the process's: the configuration service, the operator
/// records, the web API and template sources use it. Only the first call
/// takes effect; returns whether it did.
pub fn install_global(state: Arc<FilesState>) -> bool {
    GLOBAL.set(state).is_ok()
}

/// The state of this process's cluster, `None` on a single Rift.
pub fn global() -> Option<&'static Arc<FilesState>> {
    GLOBAL.get()
}

/// [`global`], once every member supports `config` and `manifest` entries
/// (a committed `features` entry lists them). Until then, during an upgrade
/// from a build without them, configuration and files stay local to each
/// peer as on a single Rift.
pub fn active() -> Option<&'static Arc<FilesState>> {
    global().filter(|state| state.enabled())
}

impl FilesState {
    /// Whether this state is attached and its entry kinds are enabled.
    pub fn enabled(&self) -> bool {
        self.cluster().is_ok_and(|cluster| {
            [EntryKind::Config, EntryKind::Manifest]
                .into_iter()
                .all(|kind| cluster.is_enabled(kind).unwrap_or(false))
        })
    }
}

/// The state [`prepare`] made for the peer that starts next.
static PREPARED: Mutex<Option<Arc<FilesState>>> = Mutex::new(None);
/// The tasks of the process's attached state, kept for its lifetime.
static ATTACHED: Mutex<Option<Attached>> = Mutex::new(None);

/// A state for the peer that starts next, kept for [`start`]: what
/// `cluster::attach::cluster_components` registers.
pub fn prepare() -> Arc<FilesState> {
    let state = FilesState::for_peer();
    *lock(&PREPARED) = Some(state.clone());
    state
}

/// After the peer started with the [`prepare`]d components: decision 6 for
/// `rift.lua` ([`startup`]; `rift.lua` beside `cluster.toml` when `path` is
/// `None`), then [`attach`] and [`install_global`]. Returns the
/// configuration to serve and its path: loaded again when the file was
/// replaced or written. A cluster without a deployment yet records this
/// file as its first one.
pub async fn start(
    cluster: &Cluster,
    config: crate::config::Config,
    path: Option<PathBuf>,
    adopt: bool,
) -> io::Result<(crate::config::Config, Option<PathBuf>)> {
    let state = lock(&PREPARED)
        .take()
        .ok_or_else(|| super::invalid("cluster files: start the peer with cluster_components"))?;
    let path = path.unwrap_or_else(|| cluster.settings().directory.join("rift.lua"));
    let outcome = startup(cluster, &path, adopt).await?;
    let (config, path) = if outcome.rewrote() {
        let loaded = path.clone();
        let config = blocking(move || crate::config::Config::load_unrestored(&loaded)).await?;
        (config, Some(path))
    } else if path.is_file() {
        (config, Some(path))
    } else {
        // No deployment and no file: the built-in defaults, as without a
        // cluster.
        (config, None)
    };
    let options = AttachOptions::from_config(&config, path.as_deref());
    let attached = attach(cluster, &state, options).await?;
    *lock(&ATTACHED) = Some(attached);
    install_global(state.clone());
    if outcome == Startup::Undeployed
        && let Some(path) = &path
        && cluster.is_enabled(EntryKind::Config).unwrap_or(false)
    {
        let source = {
            let path = path.clone();
            blocking(move || fs::read_to_string(&path)).await?
        };
        match state
            .propose_deployment(&source, path, "system", "startup", false)
            .await
        {
            Ok(id) => log_info!(
                "cluster: recorded {} as the first deployment #{id}",
                path.display()
            ),
            Err(error) => log_warn!(
                "cluster: recording {} as the first deployment: {error}",
                path.display()
            ),
        }
    }
    Ok((config, path))
}

async fn deployer(state: Arc<FilesState>) {
    loop {
        if let Err(error) = state.apply_latest().await {
            log_warn!("cluster: applying the deployed configuration: {error}");
        }
        state.deploy_wake.notified().await;
    }
}

async fn materializer(state: Arc<FilesState>) {
    loop {
        let complete = match state.materialize().await {
            Ok(complete) => complete,
            Err(error) => {
                log_warn!("cluster: materializing template_storage: {error}");
                false
            }
        };
        if complete {
            state.materialize_wake.notified().await;
        } else {
            tokio::select! {
                _ = state.materialize_wake.notified() => {}
                _ = tokio::time::sleep(RETRY) => {}
            }
        }
    }
}

async fn reporter(state: Arc<FilesState>) {
    let mut sent: Option<(String, Option<String>)> = None;
    let mut evaluated: Option<(String, Shared)> = None;
    loop {
        let running = state.running.borrow().clone();
        let failure = lock(&state.failure).clone();
        if let Some(running) = running {
            let wanted = (running.digest.clone(), failure.clone());
            if sent.as_ref() != Some(&wanted) {
                let shared = match &evaluated {
                    Some((digest, shared)) if *digest == running.digest => Ok(shared.clone()),
                    _ => state.evaluate_running(running.source.clone()).await,
                };
                let outcome = match shared {
                    Ok(shared) => {
                        if !shared.hash.is_empty()
                            && let Ok(cluster) = state.cluster()
                        {
                            cluster.links().set_config_hash(&shared.hash);
                        }
                        evaluated = Some((running.digest.clone(), shared.clone()));
                        state.report(&running, shared, failure).await
                    }
                    Err(error) => Err(error),
                };
                match outcome {
                    Ok(()) => sent = Some(wanted),
                    Err(error) => {
                        log_debug!("cluster: configuration report: {error}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                }
            }
        }
        state.report_wake.notified().await;
    }
}

async fn collector(state: Arc<FilesState>) {
    loop {
        tokio::time::sleep(GC_INTERVAL).await;
        match state.collect_garbage(GC_GRACE).await {
            Ok(0) => {}
            Ok(removed) => log_info!("cluster: removed {removed} unreferenced blobs"),
            Err(error) => log_warn!("cluster: blob garbage collection: {error}"),
        }
    }
}

/// What [`startup`] decided about `rift.lua`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Startup {
    /// The cluster has no deployment yet; the file is used as it is.
    Undeployed,
    /// The file is the last deployment.
    Current { id: u64 },
    /// The file was an older deployed revision (`from`), or with
    /// `--adopt-deployed` none (`None`), and now is the last deployment.
    Adopted { id: u64, from: Option<u64> },
    /// There was no file; it now holds the last deployment.
    Received { id: u64 },
}

impl Startup {
    /// Whether the file changed, so the configuration must be loaded again.
    pub fn rewrote(&self) -> bool {
        matches!(self, Self::Adopted { .. } | Self::Received { .. })
    }
}

/// Decision 6, before this peer loads its configuration: start with a
/// `rift.lua` that is the last deployment; replace one that is an older
/// deployed revision with the last one; refuse one that matches no deployed
/// revision (a hand edit) and keep it, unless `adopt` (`--adopt-deployed`)
/// says to replace it (it is kept beside as `<name>.hand-edited`); write the
/// last deployment where there is no file yet.
pub async fn startup(cluster: &Cluster, path: &Path, adopt: bool) -> io::Result<Startup> {
    if let Err(error) = cluster.read_barrier().await {
        log_warn!(
            "cluster: deciding about {} from this replica, which may be behind: {error}",
            path.display()
        );
    }
    let deployments = {
        let cluster = cluster.clone();
        blocking(move || cluster.state().read(config::deployments)).await?
    };
    let Some(latest) = deployments.first().cloned() else {
        return Ok(Startup::Undeployed);
    };
    let current = {
        let path = path.to_owned();
        blocking(move || match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        })
        .await?
    };
    let write =
        |contents: Vec<u8>, target: PathBuf| blocking(move || write_atomic(&target, &contents));
    let Some(current) = current else {
        write(latest.source.clone().into_bytes(), path.to_owned()).await?;
        log_info!(
            "cluster: {} did not exist; wrote the deployed configuration #{} (revision {})",
            path.display(),
            latest.id,
            latest.revision
        );
        return Ok(Startup::Received { id: latest.id });
    };
    if current == latest.source.as_bytes() {
        return Ok(Startup::Current { id: latest.id });
    }
    let digest = shared::sha256(&current);
    if let Some(older) = deployments
        .iter()
        .find(|deployment| deployment.digest == digest)
    {
        write(latest.source.clone().into_bytes(), path.to_owned()).await?;
        log_info!(
            "cluster: {} was deployment #{} (revision {}); adopted the last deployment #{} (revision {})",
            path.display(),
            older.id,
            older.revision,
            latest.id,
            latest.revision
        );
        return Ok(Startup::Adopted {
            id: latest.id,
            from: Some(older.id),
        });
    }
    if !adopt {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} matches no deployed revision of the cluster (was it edited by hand?) and was kept; deploy it from a running peer (the dashboard, PUT /api/config, or `rift admin reload` where this file is in place), or start with --adopt-deployed to replace it with deployment #{} (revision {})",
                path.display(),
                latest.id,
                latest.revision
            ),
        ));
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rift.lua".to_owned());
    let kept = path.with_file_name(format!("{name}.hand-edited"));
    write(current, kept.clone()).await?;
    write(latest.source.clone().into_bytes(), path.to_owned()).await?;
    log_warn!(
        "cluster: {} matched no deployed revision; replaced it with deployment #{} (revision {}) as --adopt-deployed asks, and kept it as {}",
        path.display(),
        latest.id,
        latest.revision,
        kept.display()
    );
    Ok(Startup::Adopted {
        id: latest.id,
        from: None,
    })
}

/// What [`import`] moved into the cluster.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ImportSummary {
    /// Deployments from `deployments.json`.
    pub deployments: usize,
    /// Operator records from `audit.jsonl`.
    pub audit: usize,
    /// The deployment recorded for the current `rift.lua`, if it was not
    /// the last imported one.
    pub deployed: Option<u64>,
    /// Template folders published.
    pub folders: Vec<String>,
}

/// `rift cluster init`: move this Rift's deployment history, operator audit
/// trail and template folders into the cluster, which must have been
/// started with [`components`]. History and audit are imported only into a
/// cluster without deployments; the current `rift.lua` becomes the last
/// deployment.
pub async fn import(cluster: &Cluster, config_path: &Path) -> io::Result<ImportSummary> {
    for kind in [EntryKind::Config, EntryKind::Manifest] {
        if !cluster.is_enabled(kind)? {
            return Err(super::invalid(format!(
                "{kind} entries are not enabled: start the cluster with the components of cluster::files"
            )));
        }
    }
    let loaded = match crate::config::Config::load_unrestored(config_path) {
        Ok(config) => Some(config),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let state = FilesState::new(&cluster.settings().directory);
    state.connect(cluster);
    let storage = loaded.as_ref().map_or_else(
        || {
            cluster
                .settings()
                .directory
                .join(crate::config::DEFAULT_TEMPLATE_STORAGE)
        },
        |config| config.template_storage.clone(),
    );
    *state.paths.write().unwrap_or_else(|e| e.into_inner()) = Paths {
        config: Some(config_path.to_owned()),
        storage: Some(storage.clone()),
        cache: loaded.as_ref().map(|config| config.software_cache.clone()),
        sources: BTreeMap::new(),
    };
    let mut summary = ImportSummary::default();
    if state.read(config::latest).await?.is_none() {
        if let Some(directory) = records_directory(loaded.as_ref(), config_path) {
            let history = {
                let file = directory.join("deployments.json");
                blocking(move || read_history(&file)).await?
            };
            for deployment in history {
                propose_checked(
                    cluster,
                    ConfigEntry::Import {
                        deployments: vec![deployment],
                        audit: Vec::new(),
                    },
                )
                .await?;
                summary.deployments += 1;
            }
            let audit = {
                let file = directory.join("audit.jsonl");
                blocking(move || read_audit(&file)).await?
            };
            for chunk in audit.chunks(256) {
                propose_checked(
                    cluster,
                    ConfigEntry::Import {
                        deployments: Vec::new(),
                        audit: chunk.to_vec(),
                    },
                )
                .await?;
                summary.audit += chunk.len();
            }
        }
        let source = {
            let path = config_path.to_owned();
            blocking(move || match fs::read_to_string(&path) {
                Ok(source) => Ok(Some(source)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error),
            })
            .await?
        };
        if let Some(source) = source {
            let last = state.read(config::latest).await?;
            if last.is_none_or(|last| last.source != source) {
                let id = state
                    .propose_deployment(&source, config_path, "system", "init", false)
                    .await
                    .map_err(io::Error::other)?;
                summary.deployed = Some(id);
            }
        }
    }
    let present = state.read(manifest::folders).await?;
    let names = {
        let storage = storage.clone();
        blocking(move || {
            let mut names = Vec::new();
            let entries = match fs::read_dir(&storage) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(names),
                Err(error) => return Err(error),
            };
            for entry in entries {
                let entry = entry?;
                if let Ok(name) = entry.file_name().into_string()
                    && !name.starts_with('.')
                {
                    names.push(name);
                }
            }
            names.sort();
            Ok(names)
        })
        .await?
    };
    for name in names {
        if present.contains_key(&name) {
            continue;
        }
        state.publish_folder(&name).await?;
        summary.folders.push(name);
    }
    Ok(summary)
}

async fn propose_checked(cluster: &Cluster, entry: ConfigEntry) -> io::Result<()> {
    match cluster.propose(entry.entry()).await? {
        Reply::Ok(_) => Ok(()),
        Reply::Err(reason) => Err(io::Error::other(reason)),
    }
}

/// Where a single Rift keeps its operator records: `web.records_directory`,
/// or `<rift.lua>.operators` beside the configuration.
fn records_directory(config: Option<&crate::config::Config>, path: &Path) -> Option<PathBuf> {
    if let Some(directory) = config
        .and_then(|config| config.web.as_ref())
        .and_then(|web| web.records_directory.clone())
    {
        return Some(directory);
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    let default = path.with_file_name(format!("{name}.operators"));
    default.is_dir().then_some(default)
}

/// The deployments of a `deployments.json`, oldest first.
fn read_history(file: &Path) -> io::Result<Vec<ImportedDeployment>> {
    let bytes = match fs::read(file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let values: Vec<Value> = serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("{}: {error}", file.display())))?;
    let mut deployments: Vec<(u64, ImportedDeployment)> = Vec::new();
    for value in values {
        let Some(source) = value["source"].as_str() else {
            continue;
        };
        if source.len() > config::MAX_SOURCE {
            continue;
        }
        deployments.push((
            value["id"].as_u64().unwrap_or(0),
            ImportedDeployment {
                time_ms: value["timestamp_unix_ms"].as_u64().unwrap_or(0),
                actor: value["actor"].as_str().unwrap_or("unknown").to_owned(),
                action: value["action"].as_str().unwrap_or("save").to_owned(),
                revision: value["revision"]
                    .as_str()
                    .map_or_else(|| revision(source), str::to_owned),
                source: source.to_owned(),
            },
        ));
    }
    deployments.sort_by_key(|(id, _)| *id);
    let keep = deployments.len().saturating_sub(config::HISTORY as usize);
    Ok(deployments
        .into_iter()
        .skip(keep)
        .map(|(_, deployment)| deployment)
        .collect())
}

/// The operator records of an `audit.jsonl`, oldest first, without the
/// per-peer summaries of refused requests and without an unfinished last
/// line.
fn read_audit(file: &Path) -> io::Result<Vec<Value>> {
    let mut bytes = Vec::new();
    match fs::File::open(file) {
        Ok(mut opened) => {
            opened.read_to_end(&mut bytes)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    let mut records = Vec::new();
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if value["actor"] == "anonymous" && value["action"] == "authentication" {
            continue;
        }
        records.push(value);
    }
    let keep = records.len().saturating_sub(config::AUDIT as usize);
    Ok(records.split_off(keep))
}

/// The configuration part of `rift admin cluster` and `GET /api/cluster`:
/// the last deployment, what each member runs, whose shared sections differ
/// and why a reload failed, and the size of the template manifest.
pub fn status(cluster: &Cluster) -> Value {
    cluster
        .state()
        .read(status_of)
        .unwrap_or_else(|error| json!({ "error": error.to_string() }))
}

fn peer_status(name: &str, report: Option<&PeerReport>, latest: Option<&Deployment>) -> Value {
    let Some(report) = report else {
        return json!({ "peer": name, "reported": false });
    };
    let in_sync = latest.is_none_or(|latest| report.deployment == Some(latest.id));
    let differs = match latest {
        Some(latest)
            if in_sync
                && !latest.shared.hash.is_empty()
                && !report.shared.hash.is_empty()
                && latest.shared.hash != report.shared.hash =>
        {
            shared::differing(&latest.shared, &report.shared)
        }
        _ => Vec::new(),
    };
    json!({
        "peer": name,
        "reported": true,
        "deployment": report.deployment,
        "in_sync": in_sync,
        "shared_hash": report.shared.hash,
        "shared_differs": differs,
        "reload_error": report.error,
    })
}

fn status_of(db: &Connection) -> io::Result<Value> {
    let latest = config::latest(db)?;
    let reports = config::reports(db)?;
    let members = config::member_names(db)?;
    let (folders, files, bytes) = manifest::totals(db)?;
    let peers: Vec<Value> = members
        .iter()
        .map(|name| peer_status(name, reports.get(name), latest.as_ref()))
        .collect();
    Ok(json!({
        "deployment": latest.as_ref().map(|latest| json!({
            "id": latest.id,
            "revision": latest.revision,
            "actor": latest.actor,
            "action": latest.action,
            "timestamp_unix_ms": latest.time_ms,
            "shared_hash": latest.shared.hash,
        })),
        "peers": peers,
        "templates": { "folders": folders, "files": files, "bytes": bytes },
    }))
}

/// `rift_cluster_config_in_sync`, `rift_cluster_config_reload_failed` and
/// `rift_cluster_config_shared_mismatch` per peer that reported.
pub fn render_metrics(cluster: &Cluster) -> String {
    let Ok((latest, reports)) = cluster
        .state()
        .read(|db| Ok((config::latest(db)?, config::reports(db)?)))
    else {
        return String::new();
    };
    if reports.is_empty() {
        return String::new();
    }
    let mut in_sync = String::from("# TYPE rift_cluster_config_in_sync gauge\n");
    let mut failed = String::from("# TYPE rift_cluster_config_reload_failed gauge\n");
    let mut mismatch = String::from("# TYPE rift_cluster_config_shared_mismatch gauge\n");
    for (peer, report) in &reports {
        let synced = latest
            .as_ref()
            .is_none_or(|latest| report.deployment == Some(latest.id));
        let differs = synced
            && latest.as_ref().is_some_and(|latest| {
                !latest.shared.hash.is_empty()
                    && !report.shared.hash.is_empty()
                    && latest.shared.hash != report.shared.hash
            });
        in_sync.push_str(&format!(
            "rift_cluster_config_in_sync{{peer=\"{peer}\"}} {}\n",
            u8::from(synced)
        ));
        failed.push_str(&format!(
            "rift_cluster_config_reload_failed{{peer=\"{peer}\"}} {}\n",
            u8::from(report.error.is_some())
        ));
        mismatch.push_str(&format!(
            "rift_cluster_config_shared_mismatch{{peer=\"{peer}\"}} {}\n",
            u8::from(differs)
        ));
    }
    in_sync + &failed + &mismatch
}

/// What `rift check --strict` found about a cluster peer's configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

/// `rift check --strict` on a cluster peer: evaluate `source` (the file at
/// `path`) as every member in this peer's replica (`database`) would and
/// name the peers and shared sections that differ; compare it with the last
/// deployment (decision 6); report peers whose running configuration
/// differs or whose reload failed.
pub fn check(path: &Path, source: &str, database: &Path, peer: &str) -> CheckReport {
    let mut report = CheckReport::default();
    if !database.is_file() {
        report.notes.push(format!(
            "cluster: {} holds no cluster state yet; shared sections are compared once this peer joined",
            database.display()
        ));
        return report;
    }
    type Replica = (
        BTreeSet<String>,
        Vec<Deployment>,
        BTreeMap<String, PeerReport>,
    );
    let read = || -> io::Result<Replica> {
        let db = Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(super::sql_error)?;
        if !config::table_exists(&db, "cluster_peers")? {
            return Ok((BTreeSet::new(), Vec::new(), BTreeMap::new()));
        }
        Ok((
            config::member_names(&db)?,
            config::deployments(&db)?,
            config::reports(&db)?,
        ))
    };
    let (mut members, deployments, reports) = match read() {
        Ok(read) => read,
        Err(error) => {
            report
                .warnings
                .push(format!("cluster: reading {}: {error}", database.display()));
            return report;
        }
    };
    members.insert(peer.to_owned());
    match shared::validate(source, path, &members) {
        Ok((shared, _)) => {
            report.notes.push(format!(
                "cluster: the shared sections evaluate the same for {} (hash {})",
                members.iter().cloned().collect::<Vec<_>>().join(", "),
                shared.hash.get(..12).unwrap_or(&shared.hash)
            ));
        }
        Err(error) => report.errors.push(format!("cluster: {error}")),
    }
    if let Some(latest) = deployments.first() {
        let digest = shared::sha256(source.as_bytes());
        if latest.digest == digest {
            report.notes.push(format!(
                "cluster: {} is the last deployment #{}",
                path.display(),
                latest.id
            ));
        } else if let Some(older) = deployments
            .iter()
            .find(|deployment| deployment.digest == digest)
        {
            report.notes.push(format!(
                "cluster: {} is the older deployment #{}; this peer adopts #{} when it starts",
                path.display(),
                older.id,
                latest.id
            ));
        } else {
            report.warnings.push(format!(
                "cluster: {} matches no deployed revision; a peer refuses to start with it until it is deployed (or with --adopt-deployed)",
                path.display()
            ));
        }
        for (name, peer_report) in &reports {
            if peer_report.deployment == Some(latest.id)
                && !latest.shared.hash.is_empty()
                && !peer_report.shared.hash.is_empty()
                && latest.shared.hash != peer_report.shared.hash
            {
                report.errors.push(format!(
                    "cluster: peer {name} evaluates the shared sections {} of deployment #{} differently",
                    shared::differing(&latest.shared, &peer_report.shared).join(", "),
                    latest.id
                ));
            }
            if let Some(error) = &peer_report.error {
                report.warnings.push(format!(
                    "cluster: peer {name} could not apply the last deployment: {error}"
                ));
            }
        }
    }
    report
}
