use std::{
    borrow::Cow,
    collections::HashSet,
    fs, io, mem,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use log::{info, warn};
use rift_messaging::{Client as Bus, RemoteSubscription};
use rift_permissions::{
    Applied, Change, ChangeEvent, CompiledSubject, ContextSet, MetaView, Replica, Tristate, Uuid,
    context::{GROUP, SERVER},
    replica::{EVICT_AFTER, UpdateOutcome},
    unix_now,
    wire::{
        self, ApplyRequest, CommandReply, CommandRequest, ContextReport, ErrorCode, ErrorReply,
        GroupInfo, GroupsGetRequest, GroupsReply, HelloReply, HelloRequest, RegisteredPermission,
        Reply, SubjectBatchReply, SubjectBatchRequest, SubjectGetRequest,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};

use crate::{
    BoxFuture, Config, Error, Listener, Listeners, OfflinePolicy, RiftPermissions, config::Fallback,
};

const CLIENT_VERSION: &str = concat!("rift-permissions-client/", env!("CARGO_PKG_VERSION"));
/// Retry interval for players whose snapshot could not be fetched.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// How often a disconnected client logs that its replica is stale.
const STALE_LOG_INTERVAL: Duration = Duration::from_secs(60);
const EVICT_INTERVAL: Duration = Duration::from_secs(60);
const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long an update waits for its players' pushed snapshots before
/// fetching the ones still missing.
const PUSH_GRACE: Duration = Duration::from_millis(250);
/// Clock difference to the proxy, in seconds, worth a warning: it shifts
/// every expiry.
const MAX_CLOCK_SKEW: u64 = 30;
const MAX_REQUEST_WAIT: Duration = Duration::from_secs(120);

/// A backend's connection to the proxy's permission service and the
/// [`Replica`] it keeps current. Start it once on plugin load inside a tokio
/// runtime; it reconnects on its own and answers checks from memory meanwhile.
/// Cheap to clone; its tasks stop when the last clone drops.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Arc<Shared>,
    tasks: [JoinHandle<()>; 2],
}

impl Drop for Inner {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.shared.disconnect();
    }
}

#[derive(Default)]
struct Players {
    /// Players on this backend, between `join` and `leave`.
    online: HashSet<Uuid>,
    /// Players to fetch on the next retry: online without a snapshot, or a
    /// failed refetch.
    pending: HashSet<Uuid>,
    /// Players served from the fallback file rather than the proxy.
    fallback: HashSet<Uuid>,
    /// Players loaded from the last snapshot, kept until the first connection
    /// so that a long outage does not evict them.
    loaded: Vec<Uuid>,
}

struct Shared {
    server: String,
    /// The `group` context filled in with `server`; `None` outside a group.
    group: Option<String>,
    join_timeout: Duration,
    request_timeout: Duration,
    snapshot_path: Option<PathBuf>,
    fallback: Option<Fallback>,
    replica: Replica,
    bus: Mutex<Option<Bus>>,
    online: watch::Sender<bool>,
    /// Most UUIDs per `subject.batch`, from the `hello` limits.
    batch: AtomicUsize,
    players: Mutex<Players>,
    retrying: AtomicBool,
    groups_stale: AtomicBool,
    changes: Listeners<ChangeEvent>,
    snapshots: Listeners<CompiledSubject>,
}

/// Where and as whom to connect.
struct Target {
    address: String,
    server_name: String,
    tls: quinn::ClientConfig,
    token: String,
}

struct Session {
    updates: RemoteSubscription,
    pushes: RemoteSubscription,
    events: RemoteSubscription,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn config_error(error: impl std::fmt::Display) -> Error {
    Error::Config(error.to_string())
}

impl Client {
    /// Start with the token from the environment variable `config.token_env`.
    /// Must be called inside a tokio runtime. Fails only on unusable
    /// configuration; an unreachable proxy is retried in the background.
    pub fn start(config: Config) -> Result<Self, Error> {
        let token = std::env::var(&config.token_env).map_err(|_| {
            Error::Config(format!(
                "environment variable {} is not set",
                config.token_env
            ))
        })?;
        Self::start_with_token(config, token)
    }

    /// [`start`](Self::start) with a token from elsewhere, such as a secret
    /// store; `config.token_env` is ignored.
    pub fn start_with_token(config: Config, token: impl Into<String>) -> Result<Self, Error> {
        let server = ContextSet::of(SERVER, &config.server)
            .map_err(config_error)?
            .get(SERVER)
            .expect("just inserted")
            .to_owned();
        let group = config
            .group
            .as_deref()
            .map(|group| {
                ContextSet::of(GROUP, group)
                    .map_err(config_error)
                    .map(|set| set.get(GROUP).expect("just inserted").to_owned())
            })
            .transpose()?;
        for wait in [config.join_timeout, config.request_timeout] {
            if wait.is_zero() || wait > MAX_REQUEST_WAIT {
                return Err(Error::Config("timeouts must be 1 ms to 120 s".into()));
            }
        }
        let tls = rift_messaging::tls_client_config(&config.ca).map_err(config_error)?;
        let (snapshot_path, fallback) = match config.offline_policy {
            OfflinePolicy::Deny => (None, None),
            OfflinePolicy::LastSnapshot(path) => (Some(path), None),
            OfflinePolicy::FallbackFile(path) => {
                let json = fs::read_to_string(&path)
                    .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
                (None, Some(Fallback::parse(&json)?))
            }
        };
        let shared = Arc::new(Shared {
            server,
            group,
            join_timeout: config.join_timeout,
            request_timeout: config.request_timeout,
            snapshot_path,
            fallback,
            replica: Replica::new(),
            bus: Mutex::new(None),
            online: watch::Sender::new(false),
            batch: AtomicUsize::new(256),
            players: Mutex::default(),
            retrying: AtomicBool::new(false),
            groups_stale: AtomicBool::new(false),
            changes: Listeners::default(),
            snapshots: Listeners::default(),
        });
        if let Some(path) = &shared.snapshot_path {
            shared.load_snapshot(path);
        }
        let target = Target {
            address: config.address,
            server_name: config.server_name,
            tls,
            token: token.into(),
        };
        let tasks = [
            tokio::spawn(shared.clone().run(target)),
            tokio::spawn(shared.clone().evict_loop()),
        ];
        Ok(Self {
            inner: Arc::new(Inner { shared, tasks }),
        })
    }

    fn shared(&self) -> &Arc<Shared> {
        &self.inner.shared
    }

    /// The replica, for direct reads of compiled subjects.
    pub fn replica(&self) -> &Replica {
        &self.shared().replica
    }

    /// Whether the client is connected and has synchronised with the proxy.
    pub fn is_online(&self) -> bool {
        *self.shared().online.borrow()
    }

    /// Wait up to `wait` for [`is_online`](Self::is_online).
    pub async fn wait_online(&self, wait: Duration) -> bool {
        let mut online = self.shared().online.subscribe();
        matches!(timeout(wait, online.wait_for(|up| *up)).await, Ok(Ok(_)))
    }

    /// A player is logging in to this backend: make sure their snapshot is in
    /// the replica, fetching it with the join timeout when no push brought it.
    /// Returns false when it could not be fetched; the offline policy then
    /// applies to this player and the fetch is retried every 5 s.
    pub async fn join(&self, uuid: Uuid) -> bool {
        let shared = self.shared();
        {
            let mut players = lock(&shared.players);
            players.online.insert(uuid);
            shared.replica.mark_joined(uuid);
            if shared.replica.contains(uuid) && !players.fallback.contains(&uuid) {
                return true;
            }
        }
        let request = SubjectGetRequest {
            uuid,
            server: Some(shared.server.clone()),
        };
        let result = shared
            .request::<_, CompiledSubject>(wire::SUBJECT_GET, &request, shared.join_timeout)
            .await
            .and_then(|subject| {
                if subject.uuid == uuid {
                    Ok(subject)
                } else {
                    Err(Error::Transport(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the proxy answered with another player's snapshot",
                    )))
                }
            });
        match result {
            Ok(subject) => {
                shared.store(subject, true);
                true
            }
            Err(error) => {
                warn!(
                    "rift-permissions: no snapshot for {uuid} ({error}); applying the offline policy and retrying every {}s",
                    RETRY_INTERVAL.as_secs()
                );
                shared.offline(uuid);
                false
            }
        }
    }

    /// A player left this backend. Their snapshot is evicted 10 minutes
    /// later unless they join again.
    pub fn leave(&self, uuid: Uuid) {
        let shared = self.shared();
        let mut players = lock(&shared.players);
        players.online.remove(&uuid);
        players.pending.remove(&uuid);
        shared.replica.mark_left(uuid, unix_now());
    }

    /// Forward a `/perms` command typed on this backend, split into arguments
    /// without the command name; `actor` is the player, `None` the console.
    pub async fn command(
        &self,
        actor: Option<Uuid>,
        args: Vec<String>,
    ) -> Result<CommandReply, Error> {
        let shared = self.shared();
        let request = CommandRequest {
            actor_uuid: actor,
            args,
        };
        shared
            .request(wire::COMMAND, &request, shared.request_timeout)
            .await
    }

    /// Declare the permissions this backend's plugins know, with
    /// descriptions and defaults; confirmed by the broker.
    pub async fn register(&self, permissions: &[RegisteredPermission]) -> Result<(), Error> {
        self.shared().publish(wire::REGISTER, &permissions).await
    }

    /// Report custom contexts for a player on this backend, replacing the
    /// previous report. Keys outside the proxy's `external_contexts` are
    /// dropped by the proxy.
    pub async fn report_context(&self, uuid: Uuid, contexts: ContextSet) -> Result<(), Error> {
        let report = ContextReport { uuid, contexts };
        self.shared().publish(wire::CONTEXT, &report).await
    }

    /// Ask the proxy for groups (`rift.perms.groups.get`), for example with
    /// permission nodes, which the replica's [`group_list`] leaves out.
    ///
    /// [`group_list`]: RiftPermissions::group_list
    pub async fn fetch_groups(&self, request: &GroupsGetRequest) -> Result<GroupsReply, Error> {
        let shared = self.shared();
        shared
            .request(wire::GROUPS_GET, request, shared.request_timeout)
            .await
    }

    /// Call `listener` whenever a player's snapshot in the replica changes,
    /// including fallback snapshots, until the returned [`Listener`] drops.
    pub fn on_snapshot(
        &self,
        listener: impl Fn(&CompiledSubject) + Send + Sync + 'static,
    ) -> Listener {
        self.shared().snapshots.add(listener)
    }

    /// Disconnect and stop the background tasks; with
    /// [`OfflinePolicy::LastSnapshot`], write the replica to its file. Reads
    /// keep answering from the replica afterwards.
    pub fn shutdown(&self) -> Result<(), Error> {
        for task in &self.inner.tasks {
            task.abort();
        }
        let shared = self.shared();
        shared.disconnect();
        match &shared.snapshot_path {
            Some(path) => shared.persist(path).map_err(|e| {
                Error::Transport(io::Error::new(
                    e.kind(),
                    format!("writing {}: {e}", path.display()),
                ))
            }),
            None => Ok(()),
        }
    }

    /// `contexts` with this backend's `server` when it names none, and its
    /// `group` when it names neither: a check for another server is not
    /// about this server's group.
    fn active<'a>(&self, contexts: &'a ContextSet) -> Cow<'a, ContextSet> {
        if contexts.get(SERVER).is_some() {
            return Cow::Borrowed(contexts);
        }
        let shared = self.shared();
        let mut active = contexts.clone();
        active
            .insert(SERVER, &shared.server)
            .expect("validated on start");
        if let Some(group) = &shared.group
            && contexts.get(GROUP).is_none()
        {
            active.insert(GROUP, group).expect("validated on start");
        }
        Cow::Owned(active)
    }

    /// This backend's service group, the `group` context, if it has one.
    pub fn group(&self) -> Option<&str> {
        self.shared().group.as_deref()
    }
}

impl RiftPermissions for Client {
    fn check(&self, uuid: Uuid, node: &str, contexts: &ContextSet) -> Tristate {
        self.replica()
            .check(uuid, node, &self.active(contexts), unix_now())
    }

    fn meta(&self, uuid: Uuid, contexts: &ContextSet) -> Option<MetaView> {
        self.replica()
            .meta(uuid, &self.active(contexts), unix_now())
    }

    fn primary_group(&self, uuid: Uuid) -> Option<String> {
        self.replica()
            .get(uuid)
            .map(|subject| subject.primary_group.clone())
    }

    fn groups(&self, uuid: Uuid, contexts: &ContextSet) -> Vec<String> {
        let Some(subject) = self.replica().get(uuid) else {
            return Vec::new();
        };
        subject
            .groups(&self.active(contexts), unix_now())
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn group_list(&self) -> Arc<Vec<GroupInfo>> {
        self.replica().groups().1
    }

    fn on_change(&self, listener: Box<dyn Fn(&ChangeEvent) + Send + Sync>) -> Listener {
        self.shared().changes.add(listener)
    }

    fn apply(&self, actor: Option<Uuid>, change: Change) -> BoxFuture<'_, Result<Applied, Error>> {
        Box::pin(async move {
            let change = change.normalized().map_err(|error| {
                Error::Rejected(ErrorReply {
                    code: ErrorCode::InvalidRequest,
                    error: error.to_string(),
                })
            })?;
            let request = ApplyRequest {
                change,
                actor_uuid: actor,
                reason: None,
            };
            let shared = self.shared();
            shared
                .request(wire::APPLY, &request, shared.request_timeout)
                .await
        })
    }

    fn revision(&self) -> u64 {
        self.replica().revision()
    }
}

/// Map a bus failure, telling timeouts and missing responders apart.
fn request_error(error: io::Error) -> Error {
    match error.to_string().as_str() {
        _ if error.kind() == io::ErrorKind::TimedOut => Error::Timeout,
        "request timed out" => Error::Timeout,
        "no responders available" => Error::Unavailable,
        _ => Error::Transport(error),
    }
}

fn parse<T: DeserializeOwned>(subject: &str, payload: &[u8]) -> Option<T> {
    serde_json::from_slice(payload)
        .inspect_err(|error| {
            warn!("rift-permissions: ignoring a malformed message on {subject}: {error}");
        })
        .ok()
}

impl Shared {
    fn bus(&self) -> Result<Bus, Error> {
        lock(&self.bus).clone().ok_or(Error::Unavailable)
    }

    fn disconnect(&self) {
        self.online.send_replace(false);
        if let Some(bus) = lock(&self.bus).take() {
            bus.close();
        }
    }

    async fn request<T: Serialize, R: DeserializeOwned>(
        &self,
        subject: &str,
        body: &T,
        wait: Duration,
    ) -> Result<R, Error> {
        let bus = self.bus()?;
        let payload = serde_json::to_vec(body).expect("wire types serialise");
        let message = timeout(wait, bus.request(subject, payload.into(), wait))
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(request_error)?;
        serde_json::from_slice::<Reply<R>>(&message.payload)
            .map_err(|error| Error::Transport(io::Error::new(io::ErrorKind::InvalidData, error)))?
            .into_result()
            .map_err(Error::Rejected)
    }

    async fn publish<T: Serialize>(&self, subject: &str, body: &T) -> Result<(), Error> {
        let bus = self.bus()?;
        let payload = serde_json::to_vec(body).expect("wire types serialise");
        timeout(self.request_timeout, bus.publish(subject, payload.into()))
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(Error::Transport)
    }

    /// Store a snapshot from the proxy (`real`) or the fallback file. A real
    /// snapshot always replaces a fallback one; otherwise the newer wins.
    fn store(&self, subject: CompiledSubject, real: bool) {
        let subject = Arc::new(subject);
        let uuid = subject.uuid;
        let stored = {
            let mut players = lock(&self.players);
            if real {
                players.pending.remove(&uuid);
                if players.fallback.remove(&uuid) {
                    self.replica.remove(uuid);
                }
            }
            let known = self.replica.contains(uuid);
            let stored = self.replica.insert(subject.clone());
            if stored {
                if !real {
                    players.fallback.insert(uuid);
                }
                // A push ahead of a join, or for a player elsewhere on the
                // network: evicted unless the player joins here.
                if !known && !players.online.contains(&uuid) {
                    self.replica.mark_left(uuid, unix_now());
                }
            }
            stored
        };
        if stored {
            self.snapshots.emit(&subject);
        }
    }

    /// Apply the offline policy to a player without a snapshot and queue a
    /// retry.
    fn offline(&self, uuid: Uuid) {
        {
            let mut players = lock(&self.players);
            if !players.online.contains(&uuid) {
                return;
            }
            players.pending.insert(uuid);
        }
        let Some(fallback) = &self.fallback else {
            return;
        };
        if self.replica.contains(uuid) {
            return;
        }
        match fallback.subject(uuid) {
            Ok(subject) => self.store(subject, false),
            Err(error) => warn!("rift-permissions: fallback for {uuid}: {error}"),
        }
    }

    /// Fetch players with `subject.batch`; failures are retried every 5 s.
    async fn refetch(&self, uuids: Vec<Uuid>) {
        let size = self.batch.load(Ordering::Relaxed).max(1);
        for chunk in uuids.chunks(size) {
            let request = SubjectBatchRequest {
                uuids: chunk.to_vec(),
            };
            match self
                .request::<_, SubjectBatchReply>(
                    wire::SUBJECT_BATCH,
                    &request,
                    self.request_timeout,
                )
                .await
            {
                Ok(reply) => {
                    for subject in reply.subjects {
                        self.store(subject, true);
                    }
                }
                Err(error) => {
                    warn!(
                        "rift-permissions: fetching {} snapshots failed ({error}); retrying",
                        chunk.len()
                    );
                    lock(&self.players).pending.extend(chunk);
                }
            }
        }
    }

    async fn refresh_groups(&self) {
        let request = GroupsGetRequest::default();
        match self
            .request::<_, GroupsReply>(wire::GROUPS_GET, &request, self.request_timeout)
            .await
        {
            Ok(reply) => {
                self.replica.set_groups(reply.revision, reply.groups);
                self.groups_stale.store(false, Ordering::Relaxed);
            }
            Err(error) => {
                warn!("rift-permissions: fetching the group table failed ({error}); retrying");
                self.groups_stale.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Retry pending players and a stale group table, one retry at a time.
    fn spawn_retry(self: &Arc<Self>) {
        let pending: Vec<Uuid> = lock(&self.players).pending.iter().copied().collect();
        let groups = self.groups_stale.load(Ordering::Relaxed);
        if (pending.is_empty() && !groups) || self.retrying.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = self.clone();
        tokio::spawn(async move {
            shared.refetch(pending).await;
            if groups {
                shared.refresh_groups().await;
            }
            shared.retrying.store(false, Ordering::Release);
        });
    }

    async fn run(self: Arc<Self>, target: Target) {
        let mut backoff = MIN_BACKOFF;
        let mut down_since = Instant::now();
        let mut reported: Option<Instant> = None;
        loop {
            match self.connect(&target).await {
                Ok(session) => {
                    info!(
                        "rift-permissions: connected to {} at revision {}",
                        target.address,
                        self.replica.revision()
                    );
                    backoff = MIN_BACKOFF;
                    let error = self.serve(session).await;
                    self.disconnect();
                    warn!(
                        "rift-permissions: connection lost ({error}); serving revision {} from the replica",
                        self.replica.revision()
                    );
                    down_since = Instant::now();
                    reported = Some(down_since);
                }
                Err(error) => {
                    if reported.is_none_or(|at| at.elapsed() >= STALE_LOG_INTERVAL) {
                        warn!(
                            "rift-permissions: permissions unavailable ({error}); replica at revision {} is stale, proxy unreachable for {}s",
                            self.replica.revision(),
                            down_since.elapsed().as_secs()
                        );
                        reported = Some(Instant::now());
                    }
                }
            }
            sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    async fn connect(self: &Arc<Self>, target: &Target) -> Result<Session, Error> {
        let address = tokio::net::lookup_host(&target.address)
            .await
            .map_err(Error::Transport)?
            .next()
            .ok_or_else(|| {
                Error::Transport(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{} resolves to no address", target.address),
                ))
            })?;
        let bus = Bus::connect(
            address,
            &target.server_name,
            target.tls.clone(),
            &target.token,
        )
        .await
        .map_err(Error::Transport)?;
        // Subscribe before `hello`, so no update between the two is lost.
        let session = Session {
            updates: bus
                .subscribe(wire::UPDATE, None)
                .await
                .map_err(Error::Transport)?,
            pushes: bus
                .subscribe(wire::UPDATE_ALL, None)
                .await
                .map_err(Error::Transport)?,
            events: bus
                .subscribe(wire::EVENTS, None)
                .await
                .map_err(Error::Transport)?,
        };
        *lock(&self.bus) = Some(bus);
        if let Err(error) = self.handshake(false).await {
            self.disconnect();
            return Err(error);
        }
        self.online.send_replace(true);
        Ok(session)
    }

    /// `hello`, then refetch what the reply shows to be stale, or with
    /// `force` (a `/perms sync`) every stored subject.
    async fn handshake(&self, force: bool) -> Result<(), Error> {
        let request = HelloRequest {
            server: self.server.clone(),
            revision: self.replica.revision(),
            client_version: CLIENT_VERSION.into(),
        };
        let hello: HelloReply = self
            .request(wire::HELLO, &request, self.request_timeout)
            .await?;
        if hello.protocol != wire::PROTOCOL_VERSION {
            return Err(Error::Transport(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "the proxy speaks permission protocol {}, this client {}",
                    hello.protocol,
                    wire::PROTOCOL_VERSION
                ),
            )));
        }
        if hello.now.abs_diff(unix_now()) > MAX_CLOCK_SKEW {
            warn!(
                "rift-permissions: this clock is {}s off the proxy's; node expiry shifts accordingly",
                hello.now.abs_diff(unix_now())
            );
        }
        self.batch.store(hello.limits.batch, Ordering::Relaxed);
        let stored = self.replica.uuids();
        let local = self.replica.revision();
        let backwards = hello.revision < local;
        if backwards {
            warn!(
                "rift-permissions: the proxy's revision went back from {local} to {}; refetching every snapshot",
                hello.revision
            );
            for uuid in &stored {
                self.replica.remove(*uuid);
            }
            self.replica.clear_groups();
        }
        let differs = self.replica.sync_revision(hello.revision) || backwards;
        if differs {
            info!(
                "rift-permissions: replica revision {local} differs from the proxy's {}; refetching {} snapshots",
                hello.revision,
                stored.len()
            );
        } else if force {
            info!(
                "rift-permissions: resyncing at revision {}; refetching {} snapshots",
                hello.revision,
                stored.len()
            );
        }
        let stale = differs || force;
        let refetch: Vec<Uuid> = {
            let mut players = lock(&self.players);
            let now = unix_now();
            for uuid in mem::take(&mut players.loaded) {
                if !players.online.contains(&uuid) {
                    self.replica.mark_left(uuid, now);
                }
            }
            let mut refetch: HashSet<Uuid> = players.pending.clone();
            if stale {
                refetch.extend(stored);
            }
            refetch.into_iter().collect()
        };
        self.refetch(refetch).await;
        self.refresh_groups().await;
        Ok(())
    }

    /// Process pushes until the connection fails.
    async fn serve(self: &Arc<Self>, mut session: Session) -> io::Error {
        let mut retry = tokio::time::interval(RETRY_INTERVAL);
        retry.reset();
        loop {
            let result = tokio::select! {
                message = session.updates.recv() => message.map(|m| self.on_update(&m.payload)),
                message = session.pushes.recv() => message.map(|m| match &*m.subject {
                    wire::RESYNC => self.resync(),
                    subject => self.on_push(subject, &m.payload),
                }),
                message = session.events.recv() => message.map(|m| {
                    if let Some(event) = parse(wire::EVENTS, &m.payload) {
                        self.changes.emit(&event);
                    }
                }),
                _ = retry.tick() => {
                    self.spawn_retry();
                    Ok(())
                }
            };
            if let Err(error) = result {
                return error;
            }
        }
    }

    fn on_update(self: &Arc<Self>, payload: &[u8]) {
        let Some(event) = parse::<ChangeEvent>(wire::UPDATE, payload) else {
            return;
        };
        let revision = event.revision;
        let (refetch, groups, wait) = match self.replica.apply_update(&event) {
            UpdateOutcome::Stale => return,
            UpdateOutcome::Applied { refetch, groups } => (refetch, groups, true),
            UpdateOutcome::Gap { refetch } => {
                warn!(
                    "rift-permissions: missed an update before revision {revision}; refetching {} snapshots",
                    refetch.len()
                );
                (refetch, true, false)
            }
        };
        if refetch.is_empty() && !groups {
            return;
        }
        let shared = self.clone();
        tokio::spawn(async move {
            let refetch = if wait && !refetch.is_empty() {
                sleep(PUSH_GRACE).await;
                refetch
                    .into_iter()
                    .filter(|uuid| {
                        shared
                            .replica
                            .get(*uuid)
                            .is_some_and(|subject| subject.revision < revision)
                    })
                    .collect()
            } else {
                refetch
            };
            shared.refetch(refetch).await;
            if groups {
                shared.refresh_groups().await;
            }
        });
    }

    /// The proxy asked every replica to resync (`/perms sync`): `hello`
    /// again and refetch everything, in the background.
    fn resync(self: &Arc<Self>) {
        info!("rift-permissions: the proxy requested a resync");
        let shared = self.clone();
        tokio::spawn(async move {
            if let Err(error) = shared.handshake(true).await {
                warn!("rift-permissions: resync failed: {error}");
            }
        });
    }

    fn on_push(&self, subject: &str, payload: &[u8]) {
        let Some(uuid) = wire::parse_update_subject(subject) else {
            return;
        };
        let Some(snapshot) = parse::<CompiledSubject>(subject, payload) else {
            return;
        };
        if snapshot.uuid != uuid {
            warn!(
                "rift-permissions: ignoring a snapshot for {} on {subject}",
                snapshot.uuid
            );
            return;
        }
        self.store(snapshot, true);
    }

    async fn evict_loop(self: Arc<Self>) {
        loop {
            sleep(EVICT_INTERVAL).await;
            let mut players = lock(&self.players);
            for uuid in self.replica.evict(unix_now(), EVICT_AFTER) {
                players.pending.remove(&uuid);
                players.fallback.remove(&uuid);
            }
        }
    }

    /// Load the replica written by [`persist`](Self::persist). A missing or
    /// unreadable file starts empty.
    fn load_snapshot(&self, path: &Path) {
        let snapshot: SubjectBatchReply = match fs::read(path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    warn!(
                        "rift-permissions: ignoring unreadable {}: {error}",
                        path.display()
                    );
                    return;
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                warn!("rift-permissions: cannot read {}: {error}", path.display());
                return;
            }
        };
        self.replica.sync_revision(snapshot.revision);
        let mut players = lock(&self.players);
        for subject in snapshot.subjects {
            players.loaded.push(subject.uuid);
            self.replica.insert(subject);
        }
        info!(
            "rift-permissions: loaded {} snapshots at revision {} from {}",
            players.loaded.len(),
            snapshot.revision,
            path.display()
        );
    }

    /// Write the replica's proxy snapshots as `{"revision", "subjects"}`,
    /// atomically through a temporary file.
    fn persist(&self, path: &Path) -> io::Result<()> {
        let fallback = lock(&self.players).fallback.clone();
        let snapshot = SubjectBatchReply {
            revision: self.replica.revision(),
            subjects: self
                .replica
                .subjects()
                .into_iter()
                .filter(|subject| !fallback.contains(&subject.uuid))
                .map(Arc::unwrap_or_clone)
                .collect(),
        };
        let mut temporary = path.as_os_str().to_owned();
        temporary.push(".tmp");
        fs::write(&temporary, serde_json::to_vec(&snapshot)?)?;
        fs::rename(&temporary, path)
    }
}
