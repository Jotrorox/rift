//! In-memory Rift permissions with builders for users, groups, nodes and
//! tracks, so a plugin's unit tests need no proxy and no network.
//! [`TestPermissions`] implements the same [`RiftPermissions`] trait as the
//! bus client, applies [`Change`]s locally the way the proxy does, and
//! compiles players with [`rift_permissions::compile`].
//!
//! ```
//! use rift_permissions::{Change, ContextSet, Node, SubjectRef, Tristate, Uuid};
//! use rift_permissions_client::RiftPermissions;
//! use rift_permissions_testing::TestPermissions;
//!
//! let alice = Uuid::from_u128(1);
//! let perms = TestPermissions::builder()
//!     .group("vip", |g| g.weight(10).permission("essentials.fly", true).prefix(100, "[VIP] "))
//!     .user(alice, |u| u.parent("vip"))
//!     .build();
//! let global = ContextSet::new();
//! assert_eq!(perms.check(alice, "essentials.fly", &global), Tristate::True);
//! assert_eq!(perms.meta(alice, &global).unwrap().prefix.as_deref(), Some("[VIP] "));
//!
//! let deny = Node::permission("essentials.fly", false).unwrap();
//! perms.apply_now(None, Change::SetNode { subject: SubjectRef::User(alice), node: deny }).unwrap();
//! assert_eq!(perms.check(alice, "essentials.fly", &global), Tristate::False);
//! ```

mod apply;

use std::{
    borrow::Cow,
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use rift_permissions::{
    Actor, Applied, Change, ChangeEvent, CompiledSubject, ContextSet, DEFAULT_GROUP, Group,
    MetaView, Node, Track, Tristate, User, Uuid, compile, context::SERVER, unix_now,
    wire::GroupInfo,
};
use rift_permissions_client::{BoxFuture, Error, Listener, Listeners, RiftPermissions};

/// The store a test works against: users, groups and tracks in memory, with
/// a revision that grows with every change.
pub struct TestPermissions {
    store: Mutex<Store>,
    server: Option<String>,
    changes: Listeners<ChangeEvent>,
}

#[derive(Clone, Default)]
struct Store {
    revision: u64,
    users: BTreeMap<Uuid, User>,
    groups: BTreeMap<String, Group>,
    tracks: BTreeMap<String, Track>,
}

impl Default for TestPermissions {
    fn default() -> Self {
        Builder::default().build()
    }
}

impl TestPermissions {
    /// An empty store with only the `default` group.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builder() -> Builder {
        Builder::default()
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Apply a change at once, on behalf of `actor` (the backend when
    /// `None`), and notify [`on_change`](RiftPermissions::on_change)
    /// listeners. The synchronous form of [`RiftPermissions::apply`].
    pub fn apply_now(&self, actor: Option<Uuid>, change: Change) -> Result<Applied, Error> {
        let actor = actor.map_or_else(|| Actor::Backend("test".into()), Actor::Player);
        let (applied, event) = self.store().commit(actor, change)?;
        if let Some(event) = event {
            self.changes.emit(&event);
        }
        Ok(applied)
    }

    /// The player's compiled subject, as the proxy would send it.
    pub fn compiled(&self, uuid: Uuid) -> CompiledSubject {
        self.store()
            .compile(uuid)
            .unwrap_or_else(|error| panic!("compiling {uuid}: {error}"))
    }

    fn active<'a>(&self, contexts: &'a ContextSet) -> Cow<'a, ContextSet> {
        match &self.server {
            Some(server) if contexts.get(SERVER).is_none() => {
                Cow::Owned(contexts.clone().with(SERVER, server).expect("validated"))
            }
            _ => Cow::Borrowed(contexts),
        }
    }
}

impl Store {
    fn compile(&self, uuid: Uuid) -> Result<CompiledSubject, rift_permissions::Error> {
        let user = self
            .users
            .get(&uuid)
            .cloned()
            .unwrap_or_else(|| User::new(uuid));
        compile(&user, &self.groups, unix_now(), self.revision)
    }
}

impl RiftPermissions for TestPermissions {
    fn check(&self, uuid: Uuid, node: &str, contexts: &ContextSet) -> Tristate {
        self.store()
            .compile(uuid)
            .map_or(Tristate::Undefined, |subject| {
                subject.check(node, &self.active(contexts), unix_now())
            })
    }

    fn meta(&self, uuid: Uuid, contexts: &ContextSet) -> Option<MetaView> {
        let subject = self.store().compile(uuid).ok()?;
        Some(subject.meta(&self.active(contexts), unix_now()))
    }

    fn primary_group(&self, uuid: Uuid) -> Option<String> {
        Some(self.store().compile(uuid).ok()?.primary_group)
    }

    fn groups(&self, uuid: Uuid, contexts: &ContextSet) -> Vec<String> {
        let Ok(subject) = self.store().compile(uuid) else {
            return Vec::new();
        };
        subject
            .groups(&self.active(contexts), unix_now())
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn group_list(&self) -> Arc<Vec<GroupInfo>> {
        let groups = self
            .store()
            .groups
            .values()
            .map(|g| GroupInfo::from_group(g, false))
            .collect();
        Arc::new(groups)
    }

    fn on_change(&self, listener: Box<dyn Fn(&ChangeEvent) + Send + Sync>) -> Listener {
        self.changes.add(listener)
    }

    fn apply(&self, actor: Option<Uuid>, change: Change) -> BoxFuture<'_, Result<Applied, Error>> {
        Box::pin(async move { self.apply_now(actor, change) })
    }

    fn revision(&self) -> u64 {
        self.store().revision
    }
}

/// Builds a [`TestPermissions`]. Invalid names or nodes panic, as a broken
/// fixture should.
#[derive(Default)]
pub struct Builder {
    store: Store,
    server: Option<String>,
}

impl Builder {
    /// Add or extend a group.
    pub fn group(mut self, name: &str, build: impl FnOnce(Subject) -> Subject) -> Self {
        let subject = build(Subject::default());
        assert!(
            subject.primary_group.is_none(),
            "primary_group applies to users only"
        );
        let group = Group::new(name).unwrap_or_else(|error| panic!("{error}"));
        let group = self.store.groups.entry(group.name.clone()).or_insert(group);
        if let Some(weight) = subject.weight {
            group.weight = weight;
            group.nodes.push(Node::weight(weight));
        }
        if let Some(name) = subject.display_name {
            group
                .nodes
                .push(Node::display_name(&name).unwrap_or_else(|e| panic!("{e}")));
            group.display_name = Some(name);
        }
        group.nodes.extend(subject.nodes);
        self
    }

    /// Add or extend a user.
    pub fn user(mut self, uuid: Uuid, build: impl FnOnce(Subject) -> Subject) -> Self {
        let subject = build(Subject::default());
        assert!(
            subject.weight.is_none() && subject.display_name.is_none(),
            "weight and display_name apply to groups only"
        );
        let user = self
            .store
            .users
            .entry(uuid)
            .or_insert_with(|| User::new(uuid));
        if let Some(group) = subject.primary_group {
            user.primary_group = Some(group);
        }
        user.nodes.extend(subject.nodes);
        self
    }

    /// Add a track, such as `["default", "vip", "mod"]`.
    pub fn track(mut self, name: &str, groups: &[&str]) -> Self {
        let track = Track::new(name, groups).unwrap_or_else(|error| panic!("{error}"));
        self.store.tracks.insert(track.name.clone(), track);
        self
    }

    /// Fill in this `server` context when a check names none, as a backend
    /// client does.
    pub fn server(mut self, name: &str) -> Self {
        let set = ContextSet::of(SERVER, name).unwrap_or_else(|error| panic!("{error}"));
        self.server = set.get(SERVER).map(str::to_owned);
        self
    }

    /// The store at revision 1, with `default` created if no fixture named it.
    pub fn build(mut self) -> TestPermissions {
        self.store
            .groups
            .entry(DEFAULT_GROUP.into())
            .or_insert_with(|| Group::new(DEFAULT_GROUP).expect("valid name"));
        self.store.revision = 1;
        TestPermissions {
            store: Mutex::new(self.store),
            server: self.server,
            changes: Listeners::default(),
        }
    }
}

/// The nodes and columns of one user or group in a [`Builder`].
#[derive(Default)]
pub struct Subject {
    nodes: Vec<Node>,
    weight: Option<i32>,
    display_name: Option<String>,
    primary_group: Option<String>,
}

impl Subject {
    /// Any node, such as one with contexts or an expiry.
    pub fn node(mut self, node: Node) -> Self {
        self.nodes.push(node);
        self
    }

    pub fn permission(self, pattern: &str, value: bool) -> Self {
        self.node(Node::permission(pattern, value).unwrap_or_else(|error| panic!("{error}")))
    }

    /// Inherit `group`.
    pub fn parent(self, group: &str) -> Self {
        self.node(Node::inherit(group, true).unwrap_or_else(|error| panic!("{error}")))
    }

    pub fn prefix(self, priority: i32, text: &str) -> Self {
        self.node(Node::prefix(priority, text).unwrap_or_else(|error| panic!("{error}")))
    }

    pub fn suffix(self, priority: i32, text: &str) -> Self {
        self.node(Node::suffix(priority, text).unwrap_or_else(|error| panic!("{error}")))
    }

    pub fn meta(self, key: &str, value: &str) -> Self {
        self.node(Node::meta(key, value).unwrap_or_else(|error| panic!("{error}")))
    }

    /// Groups only.
    pub fn weight(mut self, weight: i32) -> Self {
        self.weight = Some(weight);
        self
    }

    /// Groups only.
    pub fn display_name(mut self, name: &str) -> Self {
        self.display_name = Some(name.to_owned());
        self
    }

    /// Users only: the explicit primary group.
    pub fn primary_group(mut self, group: &str) -> Self {
        let group = rift_permissions::subject::normalize_group(group)
            .unwrap_or_else(|error| panic!("{error}"));
        self.primary_group = Some(group);
        self
    }
}
