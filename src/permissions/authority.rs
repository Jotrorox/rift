//! [`Authority`]: the one place that writes permission data and the cache of
//! compiled subjects that every proxy-side check reads.
//!
//! # Consistency
//!
//! Writers hold the database connection for the whole of a write: the
//! transaction, the in-memory group table, cache invalidation, the eager
//! recompile of affected online players and the notification. Compiling on a
//! cache miss also holds the connection, so a compiled subject is always
//! built from one committed revision and notifications leave in revision
//! order. Checks that hit the cache take only a read lock.
//!
//! # Invalidation
//!
//! Each cached subject remembers every group name its compilation could
//! reach (inherit targets, dangling and negated ones included, plus
//! `default` and an explicit primary group). A change to a group drops the
//! subjects that name it; the reverse index from group to users is built on
//! first use and maintained while it exists.
use super::{
    PermError, audit,
    control::BackendView,
    http::{BackendDirectory, NoBackends},
    store::{self, Effects, Work},
};
use crate::{config::PermissionsConfig, database::Database};
use rift_permissions::{
    Actor, Applied, Change, ChangeEvent, CompiledSubject, ContextSet, DEFAULT_GROUP, Group, Limits,
    MetaView, Node, Source, SubjectRef, Trace, Track, Tristate, User, Uuid,
    change::{Filter, Op, Patch},
    compiled::compile_with_limit,
    context,
    node::{Payload, normalize_node},
    unix_now,
    wire::RegisteredPermission,
};
use rusqlite::{TransactionBehavior, params};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::Instant,
};
use tokio::sync::broadcast;

/// Most ops one published [`ChangeEvent`] carries; the change log keeps all.
const MAX_EVENT_OPS: usize = 256;
/// Compiled subjects kept before offline users are dropped from the cache.
const MAX_CACHED: usize = 16_384;
/// Notifications a subscriber may fall behind before it sees `Lagged`.
const NOTIFICATIONS: usize = 1024;
/// Most entries one owner registers at a time, as on `rift.perms.register`.
const MAX_REGISTERED: usize = 4096;
/// Longest registered description in bytes.
const MAX_DESCRIPTION: usize = 1024;
/// Most rows one search or membership listing returns.
pub(super) const MAX_RESULTS: usize = 1000;
/// `perm_meta` key marking the one-time upgrade from the extension store.
const MIGRATED: &str = "migrated_extension_store";

/// What the authority tells subscribers. The bus layer turns these into
/// `rift.perms.update`, `rift.perms.update.<uuid>`, `rift.events.permissions`
/// and the retained `rift.perms.changes` stream.
#[derive(Debug, Clone)]
pub enum Notification {
    /// A committed change: the `perm_change` row with the affected subjects
    /// (`ops` capped at 256, see [`ChangeEvent::ops_truncated`]) and the
    /// fresh compiled subject of every affected online player.
    Committed {
        event: Arc<ChangeEvent>,
        subjects: Vec<Arc<CompiledSubject>>,
    },
    /// A player joined; their compiled subject, to push before the backend
    /// connection is forwarded.
    Joined(Arc<CompiledSubject>),
}

/// The outcome of [`Authority::bulk`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Bulk {
    /// A dry run: what the change would do. Nothing was written.
    Preview(Vec<Op>),
    Applied(Applied),
}

/// A node and the subject holding it, from [`Authority::search`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Holder {
    pub subject: SubjectRef,
    pub node: Node,
}

/// A user's global, permanent-or-temporary group memberships.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
    pub uuid: Uuid,
    pub name: Option<String>,
    pub groups: Vec<String>,
}

/// A user in a listing: the online or last-seen name and the groups of their
/// global, positive, unexpired `inherit` nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UserSummary {
    pub uuid: Uuid,
    pub name: Option<String>,
    pub online: bool,
    pub groups: Vec<String>,
}

/// A plugin-registered permission and the bus identity that registered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Registered {
    pub owner: String,
    #[serde(flatten)]
    pub permission: RegisteredPermission,
}

struct Cached {
    subject: Arc<CompiledSubject>,
    /// Every group name the compilation could reach.
    dependencies: Arc<[String]>,
}

/// A joined player and the contexts proxy-side checks use for them.
#[derive(Default)]
struct Online {
    sessions: usize,
    name: String,
    /// `server`, `world`, `dimension` and `gamemode`, as far as the proxy
    /// knows them.
    proxy: ContextSet,
    /// Reported by the player's backend, keys from
    /// `permissions.contexts.external`.
    external: ContextSet,
    /// Computed by Lua `rift.permissions.contexts.provide` callbacks.
    provided: ContextSet,
}

/// A player whose connection to `server` is being forwarded: reports from
/// that backend are accepted before the proxy sees them arrive.
struct Arriving {
    server: String,
    since: Instant,
    external: ContextSet,
}

/// How long an expected arrival is remembered.
const ARRIVAL_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);
/// Most expected arrivals kept at once.
const MAX_ARRIVING: usize = 16_384;

struct State {
    revision: u64,
    groups: Arc<BTreeMap<String, Group>>,
    tracks: Arc<BTreeMap<String, Track>>,
    cache: HashMap<Uuid, Cached>,
    /// Group name to the cached users that depend on it; `None` until needed.
    index: Option<HashMap<String, HashSet<Uuid>>>,
    online: HashMap<Uuid, Online>,
    arriving: HashMap<Uuid, Arriving>,
}

impl State {
    fn forget(&mut self, uuid: Uuid) {
        if let Some(cached) = self.cache.remove(&uuid)
            && let Some(index) = &mut self.index
        {
            for group in cached.dependencies.iter() {
                if let Some(users) = index.get_mut(group) {
                    users.remove(&uuid);
                }
            }
        }
    }

    /// Cached users that depend on any of `groups`.
    fn dependents(&mut self, groups: &BTreeSet<String>) -> HashSet<Uuid> {
        let cache = &self.cache;
        let index = self.index.get_or_insert_with(|| {
            let mut index: HashMap<String, HashSet<Uuid>> = HashMap::new();
            for (uuid, cached) in cache {
                for group in cached.dependencies.iter() {
                    index.entry(group.clone()).or_default().insert(*uuid);
                }
            }
            index
        });
        groups
            .iter()
            .filter_map(|group| index.get(group))
            .flatten()
            .copied()
            .collect()
    }
}

/// Group names a user's compilation can reach through `groups`.
fn dependencies(user: &User, groups: &BTreeMap<String, Group>) -> Arc<[String]> {
    let mut seen = BTreeSet::from([DEFAULT_GROUP.to_owned()]);
    seen.extend(user.primary_group.clone());
    let mut pending: Vec<&[Node]> = vec![&user.nodes];
    if let Some(default) = groups.get(DEFAULT_GROUP) {
        pending.push(&default.nodes);
    }
    while let Some(nodes) = pending.pop() {
        for node in nodes {
            if let Payload::Inherit { group, .. } = &node.payload
                && seen.insert(group.clone())
                && let Some(parent) = groups.get(group)
            {
                pending.push(&parent.nodes);
            }
        }
    }
    seen.into_iter().collect()
}

/// A subject that resolves every check as undefined, for a user whose
/// compilation failed.
fn unresolved(uuid: Uuid, revision: u64) -> CompiledSubject {
    CompiledSubject {
        revision,
        uuid,
        primary_group: DEFAULT_GROUP.into(),
        entries: Vec::new(),
        prefix: Vec::new(),
        suffix: Vec::new(),
        meta: Vec::new(),
    }
}

/// The proxy's permission authority. Share it as `Arc<Authority>`; it lives
/// for the whole process and survives configuration reloads.
pub struct Authority {
    database: Arc<Database>,
    state: RwLock<State>,
    config: RwLock<PermissionsConfig>,
    notifications: broadcast::Sender<Notification>,
    /// What `GET /api/perms/backends` reports; the bus layer installs it.
    backends: RwLock<Arc<dyn BackendDirectory>>,
}

impl Authority {
    /// Open the permission tables in `database`, creating them and the
    /// `default` group on first start.
    pub fn open(database: Arc<Database>, config: &PermissionsConfig) -> Result<Self, PermError> {
        database
            .migrate(store::COMPONENT, store::SCHEMA_VERSION, |_| {
                store::SCHEMA.into()
            })
            .map_err(|e| PermError::Storage(e.to_string()))?;
        let (revision, groups, tracks) = {
            let connection = database.lock();
            store::ensure_default(&connection, unix_now())?;
            (
                store::revision(&connection)?,
                store::load_groups(&connection)?,
                store::load_tracks(&connection)?,
            )
        };
        Ok(Self {
            database,
            state: RwLock::new(State {
                revision,
                groups: Arc::new(groups),
                tracks: Arc::new(tracks),
                cache: HashMap::new(),
                index: None,
                online: HashMap::new(),
                arriving: HashMap::new(),
            }),
            config: RwLock::new(config.clone()),
            notifications: broadcast::channel(NOTIFICATIONS).0,
            backends: RwLock::new(Arc::new(NoBackends)),
        })
    }

    /// An authority over a private in-memory database.
    pub fn memory(config: &PermissionsConfig) -> Self {
        Self::open(Arc::new(Database::memory()), config).expect("in-memory permission tables open")
    }

    fn state(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    fn state_mut(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(|e| e.into_inner())
    }

    fn config(&self) -> PermissionsConfig {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Apply a committed configuration's `permissions` section.
    pub fn configure(&self, config: &PermissionsConfig) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config.clone();
    }

    pub fn limits(&self) -> Limits {
        self.config().limits
    }

    /// Custom context keys backends may report.
    pub fn external_contexts(&self) -> BTreeSet<String> {
        self.config().external_contexts
    }

    /// The revision of the last committed change; 0 before the first.
    pub fn revision(&self) -> u64 {
        self.state().revision
    }

    /// Receive every notification from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.notifications.subscribe()
    }

    // Reads.

    /// The user's compiled subject, compiled and cached on a miss. A user
    /// whose compilation fails (too many entries, unreadable rows) gets a
    /// subject that resolves everything as undefined; the error is logged.
    pub fn compiled(&self, uuid: Uuid) -> Arc<CompiledSubject> {
        if let Some(cached) = self.state().cache.get(&uuid) {
            return cached.subject.clone();
        }
        let connection = self.database.lock();
        self.compile_locked(&connection, uuid)
    }

    /// Compile while the caller holds the connection.
    fn compile_locked(
        &self,
        connection: &rusqlite::Connection,
        uuid: Uuid,
    ) -> Arc<CompiledSubject> {
        let (groups, revision) = {
            let state = self.state();
            if let Some(cached) = state.cache.get(&uuid) {
                return cached.subject.clone();
            }
            (state.groups.clone(), state.revision)
        };
        let limit = self.limits().compiled_entries;
        let compiled = store::load_user(connection, uuid).and_then(|user| {
            let subject = compile_with_limit(&user, &*groups, unix_now(), revision, limit)?;
            Ok((subject, dependencies(&user, &groups)))
        });
        let (subject, dependencies) = match compiled {
            Ok((subject, dependencies)) => (Arc::new(subject), dependencies),
            Err(error) => {
                eprintln!("rift: permissions for {uuid}: {error}; checks resolve as undefined");
                return Arc::new(unresolved(uuid, revision));
            }
        };
        let mut state = self.state_mut();
        if state.cache.len() >= MAX_CACHED {
            let State { cache, online, .. } = &mut *state;
            cache.retain(|uuid, _| online.contains_key(uuid));
            state.index = None;
        }
        if let Some(index) = &mut state.index {
            for group in dependencies.iter() {
                index.entry(group.clone()).or_default().insert(uuid);
            }
        }
        state.cache.insert(
            uuid,
            Cached {
                subject: subject.clone(),
                dependencies,
            },
        );
        subject
    }

    /// Resolve `node` for a user in exactly the contexts `active`.
    pub fn check(&self, uuid: Uuid, node: &str, active: &ContextSet) -> Tristate {
        self.compiled(uuid).check(node, active, unix_now())
    }

    /// Resolve `node` in the contexts the proxy knows for the player (none
    /// when they are offline).
    pub fn check_active(&self, uuid: Uuid, node: &str) -> Tristate {
        self.check(uuid, node, &self.contexts(uuid))
    }

    /// [`Authority::check`] with the winning entry and why the others lost.
    pub fn trace(&self, uuid: Uuid, node: &str, active: &ContextSet) -> Trace {
        self.compiled(uuid).trace(node, active, unix_now())
    }

    /// Prefix, suffix, meta and primary group in `active`.
    pub fn meta(&self, uuid: Uuid, active: &ContextSet) -> MetaView {
        self.compiled(uuid).meta(active, unix_now())
    }

    /// Whether the user is in `group` (directly, inherited or `default`) in
    /// the contexts the proxy knows for them. Group names ignore ASCII case.
    pub fn in_group(&self, uuid: Uuid, group: &str) -> bool {
        let group = group.to_ascii_lowercase();
        self.compiled(uuid)
            .in_group(&group, &self.contexts(uuid), unix_now())
    }

    /// Every group with its nodes.
    pub fn groups(&self) -> Arc<BTreeMap<String, Group>> {
        self.state().groups.clone()
    }

    pub fn group(&self, name: &str) -> Option<Group> {
        self.state().groups.get(name).cloned()
    }

    pub fn tracks(&self) -> Arc<BTreeMap<String, Track>> {
        self.state().tracks.clone()
    }

    /// A user's stored nodes; a user nobody has written is empty.
    pub fn user(&self, uuid: Uuid) -> Result<User, PermError> {
        store::load_user(&self.database.lock(), uuid)
    }

    /// The UUID of the player named `name` (ASCII case ignored): an online
    /// player first, otherwise the most recent last-seen name.
    pub fn user_by_name(&self, name: &str) -> Result<Option<Uuid>, PermError> {
        if let Some((uuid, _)) = self
            .state()
            .online
            .iter()
            .find(|(_, online)| online.name.eq_ignore_ascii_case(name))
        {
            return Ok(Some(*uuid));
        }
        let key: Option<String> =
            rusqlite::OptionalExtension::optional(self.database.lock().query_row(
                "SELECT key FROM perm_subject WHERE kind = 0 AND last_name = ?1 COLLATE NOCASE
                 ORDER BY id DESC LIMIT 1",
                [name],
                |r| r.get(0),
            ))?;
        key.map(|key| Uuid::parse(&key).map_err(PermError::from))
            .transpose()
    }

    /// Users with global positive `inherit` nodes, sorted by UUID, at most
    /// `limit` (capped at 1000); `true` when more exist.
    pub fn members(&self, limit: usize) -> Result<(Vec<Member>, bool), PermError> {
        let limit = limit.min(MAX_RESULTS);
        let connection = self.database.lock();
        let mut statement = connection.prepare(
            "SELECT s.key, s.last_name, n.key FROM perm_node n
             JOIN perm_subject s ON s.id = n.subject_id
             WHERE s.kind = 0 AND n.kind = 1 AND n.value = 'true' AND n.contexts = ''
             ORDER BY s.key, n.id",
        )?;
        let mut rows = statement.query([])?;
        let mut members: Vec<Member> = Vec::new();
        while let Some(row) = rows.next()? {
            let uuid = Uuid::parse(&row.get::<_, String>(0)?)?;
            let group: String = row.get(2)?;
            if let Some(member) = members.last_mut().filter(|m| m.uuid == uuid) {
                member.groups.push(group);
                continue;
            }
            match members.len() == limit {
                true => return Ok((members, true)),
                false => members.push(Member {
                    uuid,
                    name: row.get(1)?,
                    groups: vec![group],
                }),
            }
        }
        Ok((members, false))
    }

    /// Users with a row in the store (everyone who joined or was written),
    /// sorted by name and then UUID, at most `limit` (capped at 1000); `true`
    /// when more match. `query` matches part of the last-seen name or the
    /// start of the UUID (dashes ignored); `group` keeps users holding a
    /// positive, unexpired `inherit` node for it in any context. Everyone is
    /// in `default`, so that group filters nothing.
    pub fn users(
        &self,
        query: &str,
        group: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<UserSummary>, bool), PermError> {
        let limit = limit.clamp(1, MAX_RESULTS);
        let query = query.trim();
        let name = format!("%{}%", store::like_literal(query));
        let uuid = format!(
            "{}%",
            store::like_literal(&query.replace('-', "").to_ascii_lowercase())
        );
        let group = group
            .map(|group| group.trim().to_ascii_lowercase())
            .filter(|group| !group.is_empty() && group != DEFAULT_GROUP);
        let now = store::sql_time(unix_now());
        let online: HashMap<Uuid, String> = self
            .state()
            .online
            .iter()
            .map(|(uuid, online)| (*uuid, online.name.clone()))
            .collect();
        let connection = self.database.lock();
        let mut statement = connection.prepare(
            "SELECT s.id, s.key, s.last_name FROM perm_subject s
             WHERE s.kind = 0
               AND (?1 = '' OR s.last_name LIKE ?2 ESCAPE '\\' OR s.key LIKE ?3 ESCAPE '\\')
               AND (?4 IS NULL OR EXISTS (SELECT 1 FROM perm_node n WHERE n.subject_id = s.id
                    AND n.kind = 1 AND n.key = ?4 AND n.value = 'true'
                    AND (n.expires_at IS NULL OR n.expires_at > ?5)))
             ORDER BY s.last_name IS NULL, s.last_name COLLATE NOCASE, s.key
             LIMIT ?6",
        )?;
        let rows = statement
            .query_map(
                params![query, name, uuid, group, now, (limit + 1) as i64],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let truncated = rows.len() > limit;
        let mut groups = connection.prepare(
            "SELECT key FROM perm_node WHERE subject_id = ?1 AND kind = 1 AND value = 'true'
             AND contexts = '' AND (expires_at IS NULL OR expires_at > ?2) ORDER BY id",
        )?;
        let mut users = Vec::with_capacity(rows.len().min(limit));
        for (id, key, last_name) in rows.into_iter().take(limit) {
            let uuid = Uuid::parse(&key)?;
            let names = groups
                .query_map(params![id, now], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let current = online.get(&uuid);
            users.push(UserSummary {
                uuid,
                name: current.cloned().or(last_name),
                online: current.is_some(),
                groups: names,
            });
        }
        Ok((users, truncated))
    }

    /// The online or last-seen name of a user, if known.
    pub fn user_name(&self, uuid: Uuid) -> Result<Option<String>, PermError> {
        if let Some(online) = self.state().online.get(&uuid) {
            return Ok(Some(online.name.clone()));
        }
        Ok(self.user(uuid)?.last_name)
    }

    /// Whether the user has at least one session on the proxy.
    pub fn is_online(&self, uuid: Uuid) -> bool {
        self.state().online.contains_key(&uuid)
    }

    /// Install what `GET /api/perms/backends` reports. The bus layer calls
    /// this once it tracks connected backends; until then the list is empty.
    pub fn set_backends(&self, directory: Arc<dyn BackendDirectory>) {
        *self.backends.write().unwrap_or_else(|e| e.into_inner()) = directory;
    }

    /// Every backend the bus layer knows, with its replica revision.
    pub fn backends(&self) -> Vec<BackendView> {
        let directory = self
            .backends
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        directory.backends()
    }

    /// Who holds nodes matching `filter`, in insertion order, at most `limit`
    /// (capped at 1000).
    pub fn search(&self, filter: &Filter, limit: usize) -> Result<Vec<Holder>, PermError> {
        let found = store::select(&self.database.lock(), filter, limit.min(MAX_RESULTS))?;
        Ok(found
            .into_iter()
            .map(|(_, subject, node)| Holder { subject, node })
            .collect())
    }

    /// Committed changes, see [`audit::LogQuery`].
    pub fn log(&self, query: &audit::LogQuery) -> Result<Vec<ChangeEvent>, PermError> {
        audit::read(&self.database.lock(), query)
    }

    /// Every registered permission, sorted by node.
    pub fn registered(&self) -> Result<Vec<Registered>, PermError> {
        let connection = self.database.lock();
        let mut statement = connection
            .prepare("SELECT node, description, dflt, owner FROM perm_registered ORDER BY node")?;
        let rows = statement.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut registered = Vec::new();
        for row in rows {
            let (node, description, default, owner) = row?;
            let (owner, plugin) = match owner.split_once('/') {
                Some((owner, plugin)) => (owner.to_owned(), Some(plugin.to_owned())),
                None => (owner, None),
            };
            registered.push(Registered {
                owner,
                permission: RegisteredPermission {
                    node,
                    description,
                    default: default.parse()?,
                    plugin,
                },
            });
        }
        Ok(registered)
    }

    // Writes.

    /// Apply one change in one transaction and one revision. This and the
    /// helpers built on it are the only way permission data changes.
    pub fn apply(&self, actor: Actor, change: Change) -> Result<Applied, PermError> {
        self.apply_with(actor, change, None, None)
    }

    /// [`Authority::apply`] with an audit reason, giving up before commit
    /// once `deadline` has passed (Lua callbacks).
    pub fn apply_with(
        &self,
        actor: Actor,
        change: Change,
        reason: Option<&str>,
        deadline: Option<Instant>,
    ) -> Result<Applied, PermError> {
        let change = change.normalized()?;
        self.write(&actor, reason, deadline, |work| work.change(change))
            .map(|((), applied)| applied)
    }

    /// What `change` would do, without writing anything.
    pub fn preview(&self, change: Change) -> Result<Vec<Op>, PermError> {
        let change = change.normalized()?;
        let mut connection = self.database.lock();
        // Dropping the transaction rolls it back.
        let transaction = connection.transaction()?;
        let mut work = Work::new(&transaction, unix_now(), self.limits());
        work.change(change)?;
        Ok(work.finish()?.ops)
    }

    /// Move a user up a track in `contexts`.
    pub fn promote(
        &self,
        actor: Actor,
        uuid: Uuid,
        track: &str,
        contexts: &ContextSet,
    ) -> Result<Applied, PermError> {
        let change = Change::Promote {
            user: uuid,
            track: track.to_owned(),
            contexts: contexts.clone(),
        };
        self.apply(actor, change)
    }

    /// Move a user down a track in `contexts`.
    pub fn demote(
        &self,
        actor: Actor,
        uuid: Uuid,
        track: &str,
        contexts: &ContextSet,
    ) -> Result<Applied, PermError> {
        let change = Change::Demote {
            user: uuid,
            track: track.to_owned(),
            contexts: contexts.clone(),
        };
        self.apply(actor, change)
    }

    /// Patch every node `filter` selects in one revision, or with `dry_run`
    /// list what would change.
    pub fn bulk(
        &self,
        actor: Actor,
        filter: Filter,
        patch: Patch,
        dry_run: bool,
    ) -> Result<Bulk, PermError> {
        let change = Change::Bulk { filter, patch };
        if dry_run {
            self.preview(change).map(Bulk::Preview)
        } else {
            self.apply(actor, change).map(Bulk::Applied)
        }
    }

    /// Delete expired nodes in one revision with actor `system`, so
    /// subscribers see the removals. Checks treat expired nodes as absent
    /// already; this keeps the store and the replicas tidy.
    pub fn sweep(&self) -> Result<Applied, PermError> {
        self.write(&Actor::System, Some("expired"), None, |work| work.sweep())
            .map(|((), applied)| applied)
    }

    /// Run [`Authority::sweep`] every `permissions.sweep_interval_ms` until
    /// the task is aborted.
    pub fn spawn_sweeper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let authority = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(authority.config().sweep_interval).await;
                let sweeper = authority.clone();
                if let Ok(Err(error)) = tokio::task::spawn_blocking(move || sweeper.sweep()).await {
                    eprintln!("rift: permission expiry sweep failed: {error}");
                }
            }
        })
    }

    /// Make the `source = 'config'` group nodes equal `extensions.groups`, in
    /// one revision when anything differs.
    pub fn mirror_config(&self, groups: &BTreeMap<String, Group>) -> Result<Applied, PermError> {
        self.write(&Actor::System, Some("configuration"), None, |work| {
            work.mirror(groups)
        })
        .map(|((), applied)| applied)
    }

    /// Write the nodes `upgrade` returns once, the first time this store
    /// sees the extension store, and mark it so; `upgrade` is not called
    /// afterwards. Nodes that fail validation or limits are skipped and
    /// logged. `None` when the upgrade already ran.
    pub fn migrate_once(
        &self,
        upgrade: impl FnOnce() -> std::io::Result<Vec<(SubjectRef, Node)>>,
    ) -> Result<Option<Applied>, PermError> {
        let reason = Some("upgrade from the extension store");
        let (migrated, applied) = self.write(&Actor::System, reason, None, |work| {
            if store::meta(work.connection(), MIGRATED)?.is_some() {
                return Ok(false);
            }
            let nodes = upgrade().map_err(|e| PermError::Storage(e.to_string()))?;
            for (subject, node) in nodes {
                if let Err(error) = work.set_node(&subject, node) {
                    if matches!(error, PermError::Storage(_)) {
                        return Err(error);
                    }
                    eprintln!("rift: permission upgrade skipped a node of {subject}: {error}");
                }
            }
            store::set_meta(work.connection(), MIGRATED, &work.now().to_string())?;
            Ok(true)
        })?;
        Ok(migrated.then_some(applied))
    }

    /// Replace the permissions `owner` (a bus identity) registered. The
    /// catalogue never affects resolution, so it is not revisioned.
    pub fn register(
        &self,
        owner: &str,
        permissions: Vec<RegisteredPermission>,
    ) -> Result<usize, PermError> {
        let label = |name: &str| format!("plugin:{name}").parse::<Source>().map(|_| ());
        label(owner)?;
        if permissions.len() > MAX_REGISTERED {
            return Err(PermError::Limit {
                what: "registered permissions per owner",
                limit: MAX_REGISTERED,
            });
        }
        let mut rows = Vec::with_capacity(permissions.len());
        for permission in permissions {
            let node = normalize_node(&permission.node)?;
            if let Some(description) = &permission.description
                && (description.len() > MAX_DESCRIPTION
                    || description.chars().any(char::is_control))
            {
                return Err(PermError::Conflict(format!(
                    "the description of {node:?} must have at most {MAX_DESCRIPTION} bytes without control characters"
                )));
            }
            let column = match &permission.plugin {
                Some(plugin) => {
                    label(plugin)?;
                    format!("{owner}/{plugin}")
                }
                None => owner.to_owned(),
            };
            rows.push((node, permission.description, permission.default, column));
        }
        let mut connection = self.database.lock();
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM perm_registered WHERE owner = ?1 OR substr(owner, 1, length(?1) + 1) = ?1 || '/'",
            [owner],
        )?;
        for (node, description, default, column) in &rows {
            transaction.execute(
                "INSERT INTO perm_registered (node, description, dflt, owner) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(node) DO UPDATE SET description = excluded.description,
                   dflt = excluded.dflt, owner = excluded.owner",
                params![node, description, default.to_string(), column],
            )?;
        }
        transaction.commit()?;
        Ok(rows.len())
    }

    /// Run `body` in one transaction: commit, record one change row when it
    /// did anything, update memory, recompile affected online players and
    /// notify subscribers, all while holding the connection.
    pub(super) fn write<T>(
        &self,
        actor: &Actor,
        reason: Option<&str>,
        deadline: Option<Instant>,
        body: impl FnOnce(&mut Work) -> Result<T, PermError>,
    ) -> Result<(T, Applied), PermError> {
        let mut connection = match deadline {
            Some(deadline) => self
                .database
                .lock_until(deadline)
                .map_err(|_| PermError::Deadline)?,
            None => self.database.lock(),
        };
        let now = unix_now();
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut work = Work::new(&transaction, now, self.limits());
        let value = body(&mut work)?;
        let effects = work.finish()?;
        if effects.ops.is_empty() {
            transaction.commit()?;
            let applied = Applied {
                revision: self.revision(),
                ..Applied::default()
            };
            return Ok((value, applied));
        }
        let groups = effects
            .groups
            .iter()
            .map(|name| Ok((name.clone(), store::load_group(&transaction, name)?)))
            .collect::<Result<Vec<_>, PermError>>()?;
        let tracks = effects
            .tracks
            .then(|| store::load_tracks(&transaction))
            .transpose()?;
        let revision = store::record(&transaction, now, &actor.to_string(), &effects.ops, reason)?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(PermError::Deadline);
        }
        transaction.commit()?;
        // Durable from here on: memory must follow without failing.
        let online = self.publish(revision, groups, tracks, &effects);
        let subjects: Vec<_> = online
            .iter()
            .map(|uuid| self.compile_locked(&connection, *uuid))
            .collect();
        let mut changed: BTreeSet<Uuid> = effects.users.clone();
        changed.extend(online);
        let Effects {
            mut ops, groups, ..
        } = effects;
        let applied = Applied {
            revision,
            ops: ops.len(),
            changed_users: changed.into_iter().collect(),
            changed_groups: groups.into_iter().collect(),
        };
        let ops_truncated = ops.len() > MAX_EVENT_OPS;
        ops.truncate(MAX_EVENT_OPS);
        let event = ChangeEvent {
            revision,
            at: now,
            actor: actor.clone(),
            reason: reason.map(str::to_owned),
            ops,
            ops_truncated,
            changed: applied.changed_users.clone(),
            groups: applied.changed_groups.clone(),
        };
        // Nobody listening is fine.
        let _ = self.notifications.send(Notification::Committed {
            event: Arc::new(event),
            subjects,
        });
        drop(connection);
        Ok((value, applied))
    }

    /// Install a committed revision in memory and drop affected compiled
    /// subjects. Returns the affected online players, sorted.
    fn publish(
        &self,
        revision: u64,
        groups: Vec<(String, Option<Group>)>,
        tracks: Option<BTreeMap<String, Track>>,
        effects: &Effects,
    ) -> Vec<Uuid> {
        let mut state = self.state_mut();
        state.revision = revision;
        if !groups.is_empty() {
            let mut table = (*state.groups).clone();
            for (name, group) in groups {
                match group {
                    Some(group) => table.insert(name, group),
                    None => table.remove(&name),
                };
            }
            state.groups = Arc::new(table);
        }
        if let Some(tracks) = tracks {
            state.tracks = Arc::new(tracks);
        }
        let mut affected: HashSet<Uuid> = effects.users.iter().copied().collect();
        if !effects.groups.is_empty() {
            affected.extend(state.dependents(&effects.groups));
        }
        for uuid in &affected {
            state.forget(*uuid);
        }
        let mut online: Vec<Uuid> = affected
            .into_iter()
            .filter(|uuid| state.online.contains_key(uuid))
            .collect();
        online.sort();
        online
    }

    // Presence and contexts.

    /// A player joined on `server`: mark them online, record their name and
    /// compile them eagerly. The name write and compilation run on a
    /// blocking worker when called from the async runtime.
    pub fn join(self: &Arc<Self>, uuid: Uuid, name: &str, server: &str) {
        {
            let mut state = self.state_mut();
            let online = state.online.entry(uuid).or_default();
            online.sessions += 1;
            online.name = name.to_owned();
            if let Ok(set) = ContextSet::of(context::SERVER, server) {
                online.proxy = set;
            }
            if let Some(arriving) = state.arriving.remove(&uuid)
                && arriving.server.eq_ignore_ascii_case(server)
                && let Some(online) = state.online.get_mut(&uuid)
            {
                online.external = arriving.external;
            }
        }
        let authority = self.clone();
        let name = name.to_owned();
        let work = move || {
            let connection = authority.database.lock();
            if let Err(error) = store::set_last_name(&connection, uuid, &name, unix_now()) {
                eprintln!("rift: cannot record the name of {uuid}: {error}");
            }
            let subject = authority.compile_locked(&connection, uuid);
            let _ = authority.notifications.send(Notification::Joined(subject));
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(work)),
            Err(_) => work(),
        }
    }

    /// The player moved to `server`: the proxy contexts restart from the
    /// server alone, and backend-reported and Lua-provided contexts are
    /// cleared until they are reported again.
    pub fn switch_server(&self, uuid: Uuid, server: &str) {
        let mut state = self.state_mut();
        let arriving = state
            .arriving
            .remove(&uuid)
            .filter(|arriving| arriving.server.eq_ignore_ascii_case(server));
        if let Some(online) = state.online.get_mut(&uuid) {
            online.proxy = ContextSet::of(context::SERVER, server).unwrap_or_default();
            online.external = arriving.map(|a| a.external).unwrap_or_default();
            online.provided = ContextSet::new();
        }
    }

    /// The player's connection is about to be forwarded to `server`: until
    /// they arrive (or two minutes pass), that backend may already report
    /// contexts for them and act for them. Reports it makes before the
    /// arrival take effect when [`Authority::join`] or
    /// [`Authority::switch_server`] confirms it.
    pub fn expect(&self, uuid: Uuid, server: &str) {
        let mut state = self.state_mut();
        if state.arriving.len() >= MAX_ARRIVING {
            state
                .arriving
                .retain(|_, arriving| arriving.since.elapsed() < ARRIVAL_WINDOW);
            if state.arriving.len() >= MAX_ARRIVING {
                return;
            }
        }
        state.arriving.insert(
            uuid,
            Arriving {
                server: server.to_owned(),
                since: Instant::now(),
                external: ContextSet::new(),
            },
        );
    }

    /// The backend an online player is on, as far as the proxy knows.
    pub fn server_of(&self, uuid: Uuid) -> Option<String> {
        self.state()
            .online
            .get(&uuid)
            .and_then(|online| online.proxy.get(context::SERVER))
            .map(str::to_owned)
    }

    /// Whether the player is on `server`, or their connection is being
    /// forwarded there right now.
    pub fn on_server(&self, uuid: Uuid, server: &str) -> bool {
        let state = self.state();
        let current = state
            .online
            .get(&uuid)
            .and_then(|online| online.proxy.get(context::SERVER))
            .is_some_and(|current| current.eq_ignore_ascii_case(server));
        current
            || state.arriving.get(&uuid).is_some_and(|arriving| {
                arriving.server.eq_ignore_ascii_case(server)
                    && arriving.since.elapsed() < ARRIVAL_WINDOW
            })
    }

    /// The name an online player joined with.
    pub fn online_name(&self, uuid: Uuid) -> Option<String> {
        self.state()
            .online
            .get(&uuid)
            .map(|online| online.name.clone())
    }

    /// The last name the store recorded for a user, online or not.
    pub fn last_name(&self, uuid: Uuid) -> Option<String> {
        if let Some(name) = self.online_name(uuid) {
            return Some(name);
        }
        self.database
            .lock()
            .query_row(
                "SELECT last_name FROM perm_subject WHERE kind = 0 AND key = ?1",
                [uuid.to_string()],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
    }

    /// Nodes that have expired but that the sweeper has not deleted yet.
    pub fn expired_unswept(&self) -> Result<u64, PermError> {
        let count: i64 = self.database.lock().query_row(
            "SELECT count(*) FROM perm_node WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            [store::sql_time(unix_now())],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as u64)
    }

    /// Users holding a global or contextual positive `inherit` node for
    /// `group`, sorted by UUID, at most `limit` (capped at 1000); `true`
    /// when more exist.
    pub fn group_members(
        &self,
        group: &str,
        limit: usize,
    ) -> Result<(Vec<Member>, bool), PermError> {
        let limit = limit.min(MAX_RESULTS);
        let connection = self.database.lock();
        let mut statement = connection.prepare(
            "SELECT DISTINCT s.key, s.last_name FROM perm_node n
             JOIN perm_subject s ON s.id = n.subject_id
             WHERE s.kind = 0 AND n.kind = 1 AND n.key = ?1 AND n.value = 'true'
             ORDER BY s.key LIMIT ?2",
        )?;
        let rows = statement.query_map(params![group, limit as i64 + 1], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut members = Vec::new();
        for row in rows {
            let (key, name) = row?;
            members.push(Member {
                uuid: Uuid::parse(&key)?,
                name,
                groups: vec![group.to_owned()],
            });
        }
        let more = members.len() > limit;
        members.truncate(limit);
        Ok((members, more))
    }

    /// One of the player's sessions ended.
    pub fn leave(&self, uuid: Uuid) {
        let mut state = self.state_mut();
        if let Some(online) = state.online.get_mut(&uuid) {
            online.sessions -= 1;
            if online.sessions == 0 {
                state.online.remove(&uuid);
                state.arriving.remove(&uuid);
            }
        }
    }

    /// Set or clear one of the reserved contexts (`server`, `world`,
    /// `dimension`, `gamemode`) the proxy knows for an online player.
    pub fn set_proxy_context(
        &self,
        uuid: Uuid,
        key: &str,
        value: Option<&str>,
    ) -> Result<(), PermError> {
        if !context::RESERVED.contains(&key) {
            return Err(PermError::Forbidden(format!(
                "{key:?} is not a context the proxy fills"
            )));
        }
        if let Some(online) = self.state_mut().online.get_mut(&uuid) {
            match value {
                Some(value) => online.proxy.set(key, value)?,
                None => {
                    online.proxy.remove_key(key);
                }
            }
        }
        Ok(())
    }

    /// Custom contexts a backend reports for a player, replacing its earlier
    /// report. Accepted only while the player is on `server`; `false` when
    /// they are not. Every key must be in `permissions.contexts.external`.
    pub fn report_contexts(
        &self,
        uuid: Uuid,
        server: &str,
        contexts: ContextSet,
    ) -> Result<bool, PermError> {
        let allowed = self.external_contexts();
        if let Some((key, _)) = contexts.iter().find(|(key, _)| !allowed.contains(*key)) {
            return Err(PermError::Forbidden(format!(
                "context {key:?} is not listed in permissions.contexts.external"
            )));
        }
        let mut state = self.state_mut();
        if let Some(arriving) = state.arriving.get_mut(&uuid)
            && arriving.server.eq_ignore_ascii_case(server)
            && arriving.since.elapsed() < ARRIVAL_WINDOW
        {
            arriving.external = contexts;
            return Ok(true);
        }
        match state.online.get_mut(&uuid) {
            Some(online)
                if online
                    .proxy
                    .get(context::SERVER)
                    .is_some_and(|current| current.eq_ignore_ascii_case(server)) =>
            {
                online.external = contexts;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Contexts computed by Lua context providers for an online player.
    pub fn provide_contexts(&self, uuid: Uuid, contexts: ContextSet) {
        if let Some(online) = self.state_mut().online.get_mut(&uuid) {
            online.provided = contexts;
        }
    }

    /// The contexts proxy-side checks use for a player: the proxy's own,
    /// backend-reported and Lua-provided ones. Empty while offline.
    pub fn contexts(&self, uuid: Uuid) -> ContextSet {
        self.state()
            .online
            .get(&uuid)
            .map(|online| online.proxy.union(&online.external).union(&online.provided))
            .unwrap_or_default()
    }

    /// Online players, sorted.
    pub fn online(&self) -> Vec<Uuid> {
        let mut online: Vec<Uuid> = self.state().online.keys().copied().collect();
        online.sort();
        online
    }

    pub(super) fn database(&self) -> &Database {
        &self.database
    }
}
