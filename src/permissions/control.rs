//! The proxy's side of the permission protocol (`docs/permissions-protocol.md`):
//! request handlers for `rift.perms.*`, the fan-out of committed changes to
//! `rift.perms.update`, `rift.perms.update.<uuid>`, `rift.events.permissions`
//! and the retained `rift.perms.changes` stream, and the view of which
//! backends are connected at which revision.
//!
//! # Who is asking
//!
//! The QUIC listener stamps every remote message with the principal that
//! sent it ([`Message::sender`]). A principal named `server.<name>` speaks
//! for the Rift backend `<name>`: it may act for players on that backend
//! and report their contexts, nothing else. A principal with
//! `control = true` may also write as itself (`backend:<principal>`) and
//! run console commands. In-process publishers (Lua message scripts) are
//! trusted like a control principal, as on `rift.control.*`.
use super::{
    Authority, LogQuery, Notification, PermError,
    command::{self, Env, Sender},
};
use crate::messaging::{Broker, Message, Stream, StreamConfig, Subscription};
use bytes::Bytes;
use rift_permissions::{
    Actor, Change, NodeKind, Uuid, unix_now,
    wire::{
        self, ApplyRequest, CommandReply, CommandRequest, ContextReport, ErrorCode, GroupInfo,
        GroupsGetRequest, GroupsReply, HelloReply, HelloRequest, RegisterRequest, Reply,
        ResyncRequest, SubjectBatchReply, SubjectBatchRequest, SubjectGetRequest,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    io,
    path::Path,
    sync::{Arc, Mutex, OnceLock, RwLock, Weak},
};
use tokio::{
    sync::{Semaphore, broadcast},
    task::JoinSet,
};

/// What `messaging.principals[].preset = "backend"` may publish.
pub const BACKEND_PUBLISH: [&str; 7] = [
    wire::HELLO,
    "rift.perms.subject.*",
    "rift.perms.groups.*",
    wire::APPLY,
    wire::REGISTER,
    wire::COMMAND,
    wire::CONTEXT,
];
/// What `messaging.principals[].preset = "backend"` may subscribe to.
pub const BACKEND_SUBSCRIBE: [&str; 4] =
    [wire::UPDATE, wire::UPDATE_ALL, wire::EVENTS, wire::CHANGES];
/// The principal prefix that names a backend.
pub const BACKEND_PRINCIPAL_PREFIX: &str = "server.";
/// Requests handled at once; more wait in the subscription queues.
const IN_FLIGHT: usize = 64;
/// Most group names one `groups.get` may list.
const MAX_GROUP_NAMES: usize = 64;
/// Longest audit reason accepted from a backend.
const MAX_REASON: usize = 256;

/// The retained stream configuration for `rift.perms.changes`.
pub fn changes_stream_config() -> StreamConfig {
    StreamConfig {
        subjects: vec![wire::CHANGES.into()],
        ..StreamConfig::default()
    }
}

/// Open the `rift.perms.changes` stream: in `directory` when given (beside
/// the instance database), otherwise in memory.
pub async fn open_changes_stream(directory: Option<&Path>) -> io::Result<Stream> {
    match directory {
        Some(directory) => Stream::open(directory, changes_stream_config()).await,
        None => Stream::memory(changes_stream_config()),
    }
    .map_err(|error| io::Error::other(format!("permission change stream: {error}")))
}

/// Reports [`Bus::backends`] to `GET /api/perms/backends`. Weak, since the
/// authority holding it is owned by the bus.
struct Directory(Weak<Bus>);

impl super::BackendDirectory for Directory {
    fn backends(&self) -> Vec<BackendView> {
        self.0
            .upgrade()
            .map(|bus| bus.backends())
            .unwrap_or_default()
    }
}

/// A backend principal as the bus has seen it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendView {
    /// The bus principal, `server.<name>` by convention.
    pub principal: String,
    /// The server name its last `hello` gave.
    pub server: Option<String>,
    /// The replica revision its last `hello` reported.
    pub revision: Option<u64>,
    /// When it last said `hello`, Unix seconds.
    pub hello_at: Option<u64>,
    /// When it last sent anything, Unix seconds.
    pub last_seen: u64,
    pub client_version: Option<String>,
    /// Requests it sent since the proxy started.
    pub requests: u64,
    /// Its last reported revision is ahead of the proxy's (the store went
    /// back), or behind it and it has not said `hello` since the proxy moved on.
    pub stale: bool,
}

#[derive(Default)]
struct Backend {
    server: Option<String>,
    revision: Option<u64>,
    hello_at: Option<u64>,
    last_seen: u64,
    client_version: Option<String>,
    requests: u64,
}

/// Who sent a request.
#[derive(Debug, Clone)]
struct Caller {
    /// The principal, or `local` for in-process publishers.
    principal: String,
    control: bool,
    /// The Rift backend a `server.<name>` principal speaks for.
    server: Option<String>,
    remote: bool,
}

/// A request handler's failure, sent as an error envelope.
struct Refusal {
    code: ErrorCode,
    message: String,
}

fn refuse(code: ErrorCode, message: impl Into<String>) -> Refusal {
    Refusal {
        code,
        message: message.into(),
    }
}

impl From<PermError> for Refusal {
    fn from(error: PermError) -> Self {
        Self {
            code: error.code(),
            message: error.to_string(),
        }
    }
}

type Answer = Result<Option<Value>, Refusal>;

/// The permission bus service. Share it as `Arc<Bus>`; it lives for the
/// whole process.
pub struct Bus {
    broker: Broker,
    authority: Arc<Authority>,
    /// Principal name to its `control` flag.
    principals: HashMap<String, bool>,
    /// Some principal may subscribe to `rift.perms.update.<uuid>`.
    replicas: bool,
    backends: Mutex<BTreeMap<String, Backend>>,
    changes: OnceLock<Stream>,
    commands: RwLock<Vec<(String, String)>>,
    /// The newest revision published on the bus.
    published: Mutex<u64>,
}

impl Bus {
    /// A bus over `broker` for the principals of `messaging`.
    pub fn new(
        broker: Broker,
        authority: Arc<Authority>,
        messaging: Option<&crate::config::MessagingConfig>,
    ) -> Self {
        let principals = messaging
            .map(|settings| {
                settings
                    .principals
                    .iter()
                    .map(|principal| (principal.name.clone(), principal.control))
                    .collect()
            })
            .unwrap_or_default();
        let published = Mutex::new(authority.revision());
        let probe = wire::update_subject(Uuid::from_u128(0));
        let replicas = messaging.is_some_and(|settings| {
            settings.principals.iter().any(|principal| {
                principal
                    .subscribe
                    .iter()
                    .any(|pattern| pattern_matches(pattern, &probe))
            })
        });
        Self {
            broker,
            authority,
            principals,
            replicas,
            backends: Mutex::default(),
            changes: OnceLock::new(),
            commands: RwLock::default(),
            published,
        }
    }

    pub fn authority(&self) -> &Arc<Authority> {
        &self.authority
    }

    /// Whether any principal may receive pushed snapshots, so publishing
    /// them before a connection is forwarded has a reader.
    pub fn has_principals(&self) -> bool {
        self.replicas
    }

    /// Append committed changes to `stream` from now on. Set once.
    pub fn set_changes_stream(&self, stream: Stream) {
        let _ = self.changes.set(stream);
    }

    /// The `(command, permission)` pairs of extension commands, for
    /// `/perms doctor`.
    pub fn set_commands(&self, commands: Vec<(String, String)>) {
        *self.commands.write().unwrap_or_else(|e| e.into_inner()) = commands;
    }

    /// What `/perms` commands run against.
    pub fn env(self: &Arc<Self>) -> Env {
        Env {
            authority: self.authority.clone(),
            bus: Some(self.clone()),
            commands: self
                .commands
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// Report [`Bus::backends`] through [`Authority::backends`], which the
    /// HTTP API reads.
    pub fn install_directory(self: &Arc<Self>) {
        self.authority
            .set_backends(Arc::new(Directory(Arc::downgrade(self))));
    }

    /// Every backend principal that has sent a request, sorted by name.
    pub fn backends(&self) -> Vec<BackendView> {
        let current = self.authority.revision();
        let last_change = self.last_change_at();
        let backends = self.backends.lock().unwrap_or_else(|e| e.into_inner());
        backends
            .iter()
            .map(|(principal, backend)| BackendView {
                principal: principal.clone(),
                server: backend.server.clone(),
                revision: backend.revision,
                hello_at: backend.hello_at,
                last_seen: backend.last_seen,
                client_version: backend.client_version.clone(),
                requests: backend.requests,
                stale: backend.revision.is_some_and(|revision| revision > current)
                    || backend.revision.is_some_and(|revision| revision < current)
                        && backend
                            .hello_at
                            .is_some_and(|at| last_change.is_some_and(|change| at <= change)),
            })
            .collect()
    }

    fn last_change_at(&self) -> Option<u64> {
        let revision = self.authority.revision();
        if revision == 0 {
            return None;
        }
        self.authority
            .log(&LogQuery {
                since: Some(revision - 1),
                limit: 1,
                ..LogQuery::default()
            })
            .ok()
            .and_then(|events| events.first().map(|event| event.at))
    }

    /// Ask every replica to resync (`/perms sync`): publish
    /// `rift.perms.update.resync`. Returns the current revision.
    pub fn resync(&self) -> Result<u64, String> {
        let revision = self.authority.revision();
        self.publish(wire::RESYNC, &ResyncRequest { revision });
        Ok(revision)
    }

    /// Publish the player's compiled subject on `rift.perms.update.<uuid>`
    /// and note that their connection is about to reach `server`. Called
    /// before a connection is forwarded to a backend, so the replica holds
    /// the snapshot before the player logs in there.
    pub fn push_before_connect(&self, uuid: Uuid, server: &str) {
        self.authority.expect(uuid, server);
        let subject = self.authority.compiled(uuid);
        self.publish(&wire::update_subject(uuid), &*subject);
    }

    fn publish(&self, subject: &str, body: &impl Serialize) {
        match serde_json::to_vec(body) {
            Ok(payload) => {
                if let Err(error) = self.broker.publish(subject, Bytes::from(payload)) {
                    eprintln!("rift: cannot publish {subject}: {error}");
                }
            }
            Err(error) => eprintln!("rift: cannot encode {subject}: {error}"),
        }
    }

    /// Subscribe to every request subject and to the authority's
    /// notifications, then return the service future, which runs forever.
    /// Subscribing before returning means no request sent after startup is
    /// missed.
    pub fn listen(self: &Arc<Self>) -> io::Result<impl Future<Output = ()> + Send + 'static> {
        let mut subscriptions = Vec::with_capacity(wire::REQUESTS.len());
        for subject in wire::REQUESTS {
            subscriptions.push((
                subject,
                self.broker
                    .subscribe(subject, None)
                    .map_err(io::Error::other)?,
            ));
        }
        let notifications = self.authority.subscribe();
        let bus = self.clone();
        Ok(async move {
            let permits = Arc::new(Semaphore::new(IN_FLIGHT));
            let mut tasks = JoinSet::new();
            for (subject, subscription) in subscriptions {
                tasks.spawn(bus.clone().serve(subject, subscription, permits.clone()));
            }
            tasks.spawn(bus.clone().fan_out(notifications));
            while tasks.join_next().await.is_some() {}
            std::future::pending::<()>().await
        })
    }

    async fn serve(
        self: Arc<Self>,
        subject: &'static str,
        mut subscription: Subscription,
        permits: Arc<Semaphore>,
    ) {
        loop {
            let message = match subscription.recv().await {
                Ok(message) => message,
                Err(error) => {
                    eprintln!("rift: permission requests on {subject}: {error}; resubscribing");
                    match self.broker.subscribe(subject, None) {
                        Ok(fresh) => {
                            subscription = fresh;
                            continue;
                        }
                        Err(error) => {
                            eprintln!("rift: cannot resubscribe to {subject}: {error}");
                            return;
                        }
                    }
                }
            };
            let Ok(permit) = permits.clone().acquire_owned().await else {
                return;
            };
            let bus = self.clone();
            tokio::spawn(async move {
                bus.handle(subject, message).await;
                drop(permit);
            });
        }
    }

    fn caller(&self, message: &Message) -> Caller {
        match &message.sender {
            Some(principal) => Caller {
                principal: principal.to_string(),
                control: self.principals.get(&**principal).copied().unwrap_or(false),
                server: principal
                    .strip_prefix(BACKEND_PRINCIPAL_PREFIX)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned),
                remote: true,
            },
            None => Caller {
                principal: "local".into(),
                control: true,
                server: None,
                remote: false,
            },
        }
    }

    fn seen(&self, caller: &Caller, hello: Option<&HelloRequest>) {
        if !caller.remote {
            return;
        }
        let now = unix_now();
        let mut backends = self.backends.lock().unwrap_or_else(|e| e.into_inner());
        let backend = backends.entry(caller.principal.clone()).or_default();
        backend.last_seen = now;
        backend.requests += 1;
        if let Some(hello) = hello {
            backend.server = Some(hello.server.clone());
            backend.revision = Some(hello.revision);
            backend.hello_at = Some(now);
            backend.client_version = Some(hello.client_version.clone());
        }
    }

    async fn handle(self: Arc<Self>, subject: &'static str, message: Message) {
        let caller = self.caller(&message);
        let answer = match subject {
            wire::HELLO => self.hello(&caller, &message),
            wire::SUBJECT_GET => self.blocking(&caller, &message, Self::subject_get).await,
            wire::SUBJECT_BATCH => self.blocking(&caller, &message, Self::subject_batch).await,
            wire::GROUPS_GET => self.blocking(&caller, &message, Self::groups_get).await,
            wire::APPLY => self.blocking(&caller, &message, Self::apply).await,
            wire::REGISTER => self.blocking(&caller, &message, Self::register).await,
            wire::CONTEXT => self.blocking(&caller, &message, Self::context).await,
            wire::COMMAND => self.command(&caller, &message).await,
            _ => Err(refuse(
                ErrorCode::InvalidRequest,
                "unknown permission subject",
            )),
        };
        if let Err(refusal) = &answer
            && message.reply.is_none()
        {
            eprintln!(
                "rift: {subject} from {} refused: {}",
                caller.principal, refusal.message
            );
        }
        let Some(reply) = message.reply.as_deref() else {
            return;
        };
        let envelope = match answer {
            Ok(data) => serde_json::to_vec(&Reply::Ok(data.unwrap_or(Value::Null))),
            Err(refusal) => {
                serde_json::to_vec(&Reply::<Value>::error(refusal.code, refusal.message))
            }
        };
        if let Ok(payload) = envelope {
            let _ = self.broker.publish(reply, Bytes::from(payload));
        }
    }

    /// Run a handler on a blocking worker: they read or write the database.
    async fn blocking(
        self: &Arc<Self>,
        caller: &Caller,
        message: &Message,
        handler: fn(&Self, &Caller, &Message) -> Answer,
    ) -> Answer {
        self.seen(caller, None);
        let (bus, caller, message) = (self.clone(), caller.clone(), message.clone());
        tokio::task::spawn_blocking(move || handler(&bus, &caller, &message))
            .await
            .unwrap_or_else(|error| Err(refuse(ErrorCode::Internal, error.to_string())))
    }

    fn hello(&self, caller: &Caller, message: &Message) -> Answer {
        let request: HelloRequest = parse(message)?;
        if let Some(server) = &caller.server
            && !server.eq_ignore_ascii_case(&request.server)
        {
            return Err(refuse(
                ErrorCode::Forbidden,
                format!(
                    "principal {} speaks for server {server:?}, not {:?}",
                    caller.principal, request.server
                ),
            ));
        }
        self.seen(caller, Some(&request));
        let mut external: Vec<String> = self.authority.external_contexts().into_iter().collect();
        external.sort();
        data(&HelloReply {
            revision: self.authority.revision(),
            protocol: wire::PROTOCOL_VERSION,
            now: unix_now(),
            limits: self.authority.limits(),
            external_contexts: external,
        })
    }

    fn subject_get(&self, _: &Caller, message: &Message) -> Answer {
        let request: SubjectGetRequest = parse(message)?;
        data(&*self.authority.compiled(request.uuid))
    }

    fn subject_batch(&self, _: &Caller, message: &Message) -> Answer {
        let request: SubjectBatchRequest = parse(message)?;
        let limit = self.authority.limits().batch;
        if request.uuids.len() > limit {
            return Err(refuse(
                ErrorCode::Limit,
                format!("at most {limit} UUIDs per batch"),
            ));
        }
        let revision = self.authority.revision();
        let subjects = request
            .uuids
            .iter()
            .map(|uuid| (*self.authority.compiled(*uuid)).clone())
            .collect();
        data(&SubjectBatchReply { revision, subjects })
    }

    fn groups_get(&self, _: &Caller, message: &Message) -> Answer {
        let request: GroupsGetRequest = parse(message)?;
        if request.permissions && request.names.is_none() {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                "permissions: true needs names",
            ));
        }
        let revision = self.authority.revision();
        let table = self.authority.groups();
        let groups = match &request.names {
            Some(names) => {
                if names.len() > MAX_GROUP_NAMES {
                    return Err(refuse(
                        ErrorCode::Limit,
                        format!("at most {MAX_GROUP_NAMES} group names"),
                    ));
                }
                let mut groups = Vec::new();
                for name in names {
                    let name = rift_permissions::subject::normalize_group(name)
                        .map_err(PermError::from)?;
                    if let Some(group) = table.get(&name) {
                        groups.push(GroupInfo::from_group(group, request.permissions));
                    }
                }
                groups
            }
            None => table
                .values()
                .map(|group| GroupInfo::from_group(group, false))
                .collect(),
        };
        data(&GroupsReply { revision, groups })
    }

    /// The player `actor` may act through `caller`: they are on its backend
    /// (for a `server.<name>` principal) and online at all otherwise.
    fn actor_present(&self, caller: &Caller, actor: Uuid) -> Result<(), Refusal> {
        let present = match &caller.server {
            Some(server) => self.authority.on_server(actor, server),
            None if caller.remote => false,
            None => true,
        };
        if present {
            Ok(())
        } else {
            Err(refuse(
                ErrorCode::Forbidden,
                format!(
                    "player {actor} is not online on the backend of {}",
                    caller.principal
                ),
            ))
        }
    }

    fn apply(&self, caller: &Caller, message: &Message) -> Answer {
        let request: ApplyRequest = parse(message)?;
        if let Some(reason) = &request.reason
            && (reason.len() > MAX_REASON || reason.chars().any(char::is_control))
        {
            return Err(refuse(
                ErrorCode::InvalidRequest,
                format!("reason must have at most {MAX_REASON} bytes without control characters"),
            ));
        }
        let change = request.change.normalized().map_err(PermError::from)?;
        let actor = match request.actor_uuid {
            Some(uuid) => {
                self.actor_present(caller, uuid)?;
                for node in required_nodes(&change) {
                    if self.authority.check_active(uuid, &node) != rift_permissions::Tristate::True
                    {
                        return Err(refuse(
                            ErrorCode::Forbidden,
                            format!("player {uuid} lacks {node}"),
                        ));
                    }
                }
                command::escalation(&self.authority, uuid, &change)
                    .map_err(|message| refuse(ErrorCode::Forbidden, message))?;
                Actor::Player(uuid)
            }
            None if caller.control => match caller.remote {
                true => Actor::Backend(caller.principal.clone()),
                false => Actor::Lua("bus".into()),
            },
            None => {
                return Err(refuse(
                    ErrorCode::Forbidden,
                    format!(
                        "principal {} needs control = true to write without a player",
                        caller.principal
                    ),
                ));
            }
        };
        let applied = self
            .authority
            .apply_with(actor, change, request.reason.as_deref(), None)?;
        data(&applied)
    }

    fn register(&self, caller: &Caller, message: &Message) -> Answer {
        let request: RegisterRequest = parse(message)?;
        let count = self.authority.register(&caller.principal, request)?;
        Ok(Some(serde_json::json!({ "registered": count })))
    }

    fn context(&self, caller: &Caller, message: &Message) -> Answer {
        let report: ContextReport = parse(message)?;
        let allowed = self.authority.external_contexts();
        let mut contexts = rift_permissions::ContextSet::new();
        let mut dropped = Vec::new();
        for (key, value) in report.contexts.iter() {
            if allowed.contains(key) {
                contexts.insert(key, value).map_err(PermError::from)?;
            } else {
                dropped.push(key.to_owned());
            }
        }
        if !dropped.is_empty() {
            dropped.dedup();
            eprintln!(
                "rift: {} reported contexts outside permissions.contexts.external, dropped: {}",
                caller.principal,
                dropped.join(", ")
            );
        }
        let server = match &caller.server {
            Some(server) => server.clone(),
            None if caller.remote => {
                return Err(refuse(
                    ErrorCode::Forbidden,
                    format!(
                        "principal {} is not a server.<name> backend",
                        caller.principal
                    ),
                ));
            }
            None => self
                .authority
                .server_of(report.uuid)
                .ok_or_else(|| refuse(ErrorCode::NotFound, "player is offline"))?,
        };
        let accepted = self
            .authority
            .report_contexts(report.uuid, &server, contexts)?;
        if !accepted {
            return Err(refuse(
                ErrorCode::Forbidden,
                format!("player {} is not on {server}", report.uuid),
            ));
        }
        Ok(None)
    }

    async fn command(self: &Arc<Self>, caller: &Caller, message: &Message) -> Answer {
        self.seen(caller, None);
        let request: CommandRequest = parse(message)?;
        let sender = match request.actor_uuid {
            Some(uuid) => {
                self.actor_present(caller, uuid)?;
                Sender::Player(uuid)
            }
            None if caller.control => Sender::Console(match caller.remote {
                true => Actor::Backend(caller.principal.clone()),
                false => Actor::Lua("bus".into()),
            }),
            None => {
                return Err(refuse(
                    ErrorCode::Forbidden,
                    format!(
                        "principal {} needs control = true to run console commands",
                        caller.principal
                    ),
                ));
            }
        };
        let args = command::rejoin(&request.args);
        let output = command::execute(&self.env(), &sender, &args).await;
        data(&CommandReply {
            revision: output.revision,
            component: output.component(),
        })
    }

    /// Publish every notification of the authority, in revision order.
    async fn fan_out(self: Arc<Self>, mut notifications: broadcast::Receiver<Notification>) {
        loop {
            match notifications.recv().await {
                Ok(Notification::Committed { event, subjects }) => {
                    for subject in &subjects {
                        self.publish(&wire::update_subject(subject.uuid), &**subject);
                    }
                    self.publish_event(&event).await;
                }
                Ok(Notification::Joined(subject)) => {
                    self.publish(&wire::update_subject(subject.uuid), &*subject);
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    eprintln!(
                        "rift: the permission bus fell {missed} notifications behind; republishing from the change log"
                    );
                    self.recover().await;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    async fn publish_event(&self, event: &rift_permissions::ChangeEvent) {
        let Ok(payload) = serde_json::to_vec(event) else {
            return;
        };
        let payload = Bytes::from(payload);
        let _ = self.broker.publish(wire::UPDATE, payload.clone());
        let _ = self.broker.publish(wire::EVENTS, payload.clone());
        if let Some(stream) = self.changes.get()
            && let Err(error) = stream.publish(wire::CHANGES, None, payload).await
        {
            eprintln!(
                "rift: cannot append revision {} to {}: {error}",
                event.revision,
                wire::CHANGES
            );
        }
        let mut published = self.published.lock().unwrap_or_else(|e| e.into_inner());
        *published = (*published).max(event.revision);
    }

    /// After missing notifications: republish the change events since the
    /// last published revision from the change log (so `rift.perms.update`
    /// subscribers see the gap closed and the stream stays complete), then
    /// every online player's current subject.
    async fn recover(&self) {
        loop {
            let since = *self.published.lock().unwrap_or_else(|e| e.into_inner());
            let authority = self.authority.clone();
            let events = tokio::task::spawn_blocking(move || {
                authority.log(&LogQuery {
                    since: Some(since),
                    limit: 1000,
                    ..LogQuery::default()
                })
            })
            .await;
            let events = match events {
                Ok(Ok(events)) => events,
                Ok(Err(error)) => {
                    eprintln!("rift: cannot read the permission change log: {error}");
                    break;
                }
                Err(_) => break,
            };
            let done = events.len() < 1000;
            for event in &events {
                self.publish_event(event).await;
            }
            if done || events.is_empty() {
                break;
            }
        }
        let authority = self.authority.clone();
        let subjects = tokio::task::spawn_blocking(move || {
            authority
                .online()
                .into_iter()
                .map(|uuid| authority.compiled(uuid))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        for subject in subjects {
            self.publish(&wire::update_subject(subject.uuid), &*subject);
        }
    }
}

/// Whether a subscription `pattern` (`*` one token, `>` the rest) matches
/// the literal `subject`.
fn pattern_matches(pattern: &str, subject: &str) -> bool {
    let mut pattern = pattern.split('.');
    let mut subject = subject.split('.');
    loop {
        match (pattern.next(), subject.next()) {
            (None, None) => return true,
            (Some(">"), Some(_)) => return true,
            (Some("*"), Some(_)) => {}
            (Some(p), Some(s)) if p == s => {}
            _ => return false,
        }
    }
}

fn parse<T: DeserializeOwned>(message: &Message) -> Result<T, Refusal> {
    serde_json::from_slice(&message.payload).map_err(|error| {
        refuse(
            ErrorCode::InvalidRequest,
            format!("invalid request: {error}"),
        )
    })
}

fn data(body: &impl Serialize) -> Answer {
    serde_json::to_value(body)
        .map(Some)
        .map_err(|error| refuse(ErrorCode::Internal, error.to_string()))
}

/// The `rift.perms.<verb>` nodes a player needs for a change sent on
/// `rift.perms.apply`, mirroring the `/perms` verb tree.
pub fn required_nodes(change: &Change) -> Vec<String> {
    use rift_permissions::SubjectRef;
    let side = |subject: &SubjectRef| match subject {
        SubjectRef::User(_) => "user",
        SubjectRef::Group(_) => "group",
    };
    let verb = |kind: NodeKind, set: bool, temporary: bool| -> &'static str {
        match (kind, set, temporary) {
            (NodeKind::Permission, true, false) => "permission.set",
            (NodeKind::Permission, true, true) => "permission.settemp",
            (NodeKind::Permission, false, _) => "permission.unset",
            (NodeKind::Inherit, true, false) => "parent.add",
            (NodeKind::Inherit, true, true) => "parent.addtemp",
            (NodeKind::Inherit, false, _) => "parent.remove",
            (NodeKind::Prefix, true, _) => "meta.setprefix",
            (NodeKind::Prefix, false, _) => "meta.removeprefix",
            (NodeKind::Suffix, true, _) => "meta.setsuffix",
            (NodeKind::Suffix, false, _) => "meta.removesuffix",
            (NodeKind::Meta, true, false) => "meta.set",
            (NodeKind::Meta, true, true) => "meta.settemp",
            (NodeKind::Meta, false, _) => "meta.unset",
            (NodeKind::Weight, _, _) => "setweight",
            (NodeKind::DisplayName, _, _) => "setdisplayname",
        }
    };
    let node = |text: String| format!("{}{text}", command::NODE_PREFIX);
    match change {
        Change::SetNode { subject, node: set } => {
            let kind = set.kind();
            let verb = verb(kind, true, set.expires_at.is_some());
            let side = if matches!(kind, NodeKind::Weight | NodeKind::DisplayName) {
                "group"
            } else {
                side(subject)
            };
            vec![node(format!("{side}.{verb}"))]
        }
        Change::UnsetNode { subject, key } => {
            vec![node(format!(
                "{}.{}",
                side(subject),
                verb(key.kind, false, false)
            ))]
        }
        Change::ClearSubject { subject, .. } => vec![node(format!("{}.clear", side(subject)))],
        Change::CreateGroup { .. } => vec![node("group.create".into())],
        Change::DeleteGroup { .. } => vec![node("group.delete".into())],
        Change::SetWeight { .. } => vec![node("group.setweight".into())],
        Change::SetDisplayName { .. } => vec![node("group.setdisplayname".into())],
        Change::SetPrimaryGroup { .. } => vec![node("user.parent.switchprimarygroup".into())],
        Change::SetTrack { .. } => vec![node("track.edit".into())],
        Change::DeleteTrack { .. } => vec![node("track.delete".into())],
        Change::Promote { .. } => vec![node("user.promote".into())],
        Change::Demote { .. } => vec![node("user.demote".into())],
        Change::Bulk { .. } => vec![node("bulk".into())],
        Change::Batch { changes } => {
            let mut nodes: Vec<String> = changes.iter().flat_map(required_nodes).collect();
            nodes.sort();
            nodes.dedup();
            nodes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MessagingConfig, MessagingPrincipal, PermissionsConfig};
    use rift_permissions::{Node, SubjectRef};
    use serde_json::json;
    use std::time::Duration;

    const ALICE: Uuid = Uuid::from_u128(0xa11ce);

    fn principal(name: &str, control: bool) -> MessagingPrincipal {
        let (publish, subscribe) = crate::permissions::backend_grants();
        MessagingPrincipal {
            name: name.into(),
            token_env: "UNUSED".into(),
            publish,
            subscribe,
            control,
        }
    }

    async fn bus() -> (Arc<Bus>, Broker) {
        let broker = Broker::default();
        let authority = Arc::new(Authority::memory(&PermissionsConfig {
            external_contexts: ["region".to_owned()].into(),
            ..PermissionsConfig::default()
        }));
        let messaging = MessagingConfig {
            principals: vec![
                principal("server.survival", false),
                principal("operator", true),
            ],
            ..MessagingConfig::default()
        };
        let bus = Arc::new(Bus::new(broker.clone(), authority, Some(&messaging)));
        tokio::spawn(bus.listen().unwrap());
        (bus, broker)
    }

    async fn ask(broker: &Broker, sender: &str, subject: &str, body: Value) -> Value {
        let reply = broker
            .request_as(
                subject,
                Bytes::from(body.to_string()),
                Duration::from_secs(5),
                Some(sender),
            )
            .await
            .unwrap();
        serde_json::from_slice(&reply.payload).unwrap()
    }

    #[tokio::test]
    async fn requests_are_authorized_by_the_sending_principal() {
        let (bus, broker) = bus().await;
        assert!(bus.has_principals());
        let hello = ask(
            &broker,
            "server.survival",
            wire::HELLO,
            json!({"server": "survival", "revision": 0, "client_version": "test"}),
        )
        .await;
        assert_eq!(hello["ok"], true, "{hello}");
        assert_eq!(hello["data"]["external_contexts"], json!(["region"]));
        let backends = bus.backends();
        assert_eq!(backends[0].principal, "server.survival");
        assert_eq!(backends[0].revision, Some(0));
        let impostor = ask(
            &broker,
            "server.survival",
            wire::HELLO,
            json!({"server": "lobby", "revision": 0, "client_version": "test"}),
        )
        .await;
        assert_eq!(impostor["code"], "forbidden", "{impostor}");

        let change = json!({"op": "create_group", "name": "vip"});
        let refused = ask(
            &broker,
            "server.survival",
            wire::APPLY,
            json!({"change": change}),
        )
        .await;
        assert_eq!(refused["code"], "forbidden", "{refused}");
        let applied = ask(&broker, "operator", wire::APPLY, json!({"change": change})).await;
        assert_eq!(applied["data"]["revision"], 1, "{applied}");
        let log = bus.authority().log(&LogQuery::default()).unwrap();
        assert_eq!(log[0].actor, Actor::Backend("operator".into()));

        let absent = ask(
            &broker,
            "server.survival",
            wire::APPLY,
            json!({"change": change, "actor_uuid": ALICE}),
        )
        .await;
        assert!(
            absent["error"].as_str().unwrap().contains("not online"),
            "{absent}"
        );
        let console = ask(
            &broker,
            "server.survival",
            wire::COMMAND,
            json!({"args": ["listgroups"]}),
        )
        .await;
        assert_eq!(console["code"], "forbidden", "{console}");
        let operator = ask(
            &broker,
            "operator",
            wire::COMMAND,
            json!({"args": ["listgroups"]}),
        )
        .await;
        assert!(
            operator["data"]["component"]
                .to_string()
                .contains("2 groups"),
            "{operator}"
        );

        let groups = ask(
            &broker,
            "server.survival",
            wire::GROUPS_GET,
            json!({"permissions": true}),
        )
        .await;
        assert_eq!(groups["code"], "invalid_request", "{groups}");
        let uuids: Vec<String> = (0..257).map(|i| Uuid::from_u128(i).to_string()).collect();
        let batch = ask(
            &broker,
            "server.survival",
            wire::SUBJECT_BATCH,
            json!({"uuids": uuids}),
        )
        .await;
        assert_eq!(batch["code"], "limit", "{batch}");
        let malformed = ask(
            &broker,
            "server.survival",
            wire::SUBJECT_GET,
            json!({"uuid": 5}),
        )
        .await;
        assert_eq!(malformed["code"], "invalid_request", "{malformed}");
    }

    #[tokio::test]
    async fn arriving_players_can_be_acted_for_and_report_contexts() {
        let (bus, broker) = bus().await;
        let authority = bus.authority().clone();
        authority
            .apply(
                Actor::Cli,
                Change::SetNode {
                    subject: SubjectRef::User(ALICE),
                    node: Node::permission("warp.spawn", true)
                        .unwrap()
                        .with_context("region", "spawn")
                        .unwrap(),
                },
            )
            .unwrap();
        let mut pushes = broker
            .subscribe(&wire::update_subject(ALICE), None)
            .unwrap();
        bus.push_before_connect(ALICE, "survival");
        let pushed = pushes.recv().await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&pushed.payload).unwrap()["revision"],
            1
        );
        let report = ask(
            &broker,
            "server.survival",
            wire::CONTEXT,
            json!({"uuid": ALICE, "contexts": {"region": "spawn", "evil": "x"}}),
        )
        .await;
        assert_eq!(report["ok"], true, "{report}");
        let lobby = ask(
            &broker,
            "server.lobby",
            wire::CONTEXT,
            json!({"uuid": ALICE, "contexts": {"region": "market"}}),
        )
        .await;
        assert_eq!(lobby["code"], "forbidden", "{lobby}");
        authority.join(ALICE, "Alice", "survival");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            authority.contexts(ALICE).get("region"),
            Some("spawn"),
            "the report made during the login took effect"
        );
        assert_eq!(authority.contexts(ALICE).get("evil"), None);
        assert_eq!(
            authority.check_active(ALICE, "warp.spawn"),
            rift_permissions::Tristate::True
        );
    }

    #[test]
    fn apply_nodes_mirror_the_command_tree() {
        let set = |node: Node| Change::SetNode {
            subject: SubjectRef::User(ALICE),
            node,
        };
        assert_eq!(
            required_nodes(&set(Node::permission("a", true).unwrap())),
            ["rift.perms.user.permission.set"]
        );
        assert_eq!(
            required_nodes(&set(Node::inherit("vip", true).unwrap().expiring_at(5))),
            ["rift.perms.user.parent.addtemp"]
        );
        assert_eq!(
            required_nodes(&Change::Batch {
                changes: vec![
                    set(Node::prefix(1, "x").unwrap()),
                    Change::CreateGroup {
                        name: "vip".into(),
                        weight: 0,
                        display_name: None,
                    },
                ],
            }),
            ["rift.perms.group.create", "rift.perms.user.meta.setprefix"]
        );
        assert!(pattern_matches(
            "rift.perms.update.>",
            "rift.perms.update.abc"
        ));
        assert!(pattern_matches(">", "rift.perms.update.abc"));
        assert!(!pattern_matches(
            "rift.perms.update",
            "rift.perms.update.abc"
        ));
    }
}
