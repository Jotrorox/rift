//! The engine-neutral host API: plain Rust with one method per WIT import
//! (`players`, `store`, `permissions`, `bus`, `servers`, `timers`, `log`,
//! `meta`), so engine bindings are thin forwarders.
//!
//! Persistent plugins are trusted and get no per-call budgets. Methods that
//! wait (bus requests, server operations, store writes) block the calling
//! thread, which is always the plugin's own thread; they must never be
//! called from an async task.
use super::{
    PluginSpec,
    api::{BossBarSpec, BusMessage, Event, Level, PlayerInfo, TitleSpec},
    manifest::Filesystem,
    worker::{Queue, Shared},
};
use crate::{
    extensions::{PlayerAction, Players, storage::Store},
    messaging::{Broker, Error as BusError, Stream},
    permissions::Authority,
    text::Text,
};
use bytes::Bytes;
use rift_permissions::{
    Actor, Change, ContextSet, DEFAULT_GROUP, Node, NodeKind, Source, SubjectRef, Tristate, Uuid,
    node::NodeKey,
};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

/// Host-side bounds that keep one plugin from exhausting shared resources.
const MAX_SUBSCRIPTIONS: usize = 256;
const MAX_TIMERS: usize = 1024;
const MIN_TIMER_MS: u32 = 10;
const MAX_REQUEST_MS: u32 = 60_000;
/// Store writes wait at most this long for the store's writer lock.
const STORE_WAIT: Duration = Duration::from_secs(5);
/// How long `servers.*` waits for the control service's reply.
const CONTROL_WAIT: Duration = Duration::from_secs(60);
const MAX_TEXT: usize = 1024;
const MAX_LOG: usize = 4096;

/// What every instance shares: the process-wide directory, store,
/// permissions and bus. Lives across reload generations.
#[derive(Clone)]
pub(crate) struct Base {
    pub players: Players,
    pub store: Store,
    /// The process-wide permission authority.
    pub permissions: Arc<Authority>,
    pub broker: Broker,
    /// Retained streams, attached by the runtime once they are open.
    pub streams: Arc<RwLock<Arc<HashMap<String, Stream>>>>,
    /// Log tails, counters and lifecycle notifications for operators.
    pub monitor: Arc<super::monitor::Monitor>,
}

enum Kind {
    Subscription,
    Timer,
}

struct Task {
    kind: Kind,
    task: JoinHandle<()>,
}

struct Inner {
    spec: Arc<PluginSpec>,
    base: Base,
    queue: Queue,
    handle: tokio::runtime::Handle,
    shared: Arc<Shared>,
    next: AtomicU64,
    tasks: Mutex<BTreeMap<u64, Task>>,
    closed: AtomicBool,
    dropped: AtomicU64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().unwrap().values() {
            task.task.abort();
        }
    }
}

/// The host API for one plugin instance. Cheap to clone; subscriptions and
/// timers end with the instance (see [`Services::close`]).
#[derive(Clone)]
pub struct Services(Arc<Inner>);

impl Services {
    pub(crate) fn new(
        spec: Arc<PluginSpec>,
        base: Base,
        queue: Queue,
        handle: tokio::runtime::Handle,
        shared: Arc<Shared>,
    ) -> Self {
        Self(Arc::new(Inner {
            spec,
            base,
            queue,
            handle,
            shared,
            next: AtomicU64::new(1),
            tasks: Mutex::default(),
            closed: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
        }))
    }

    /// End every subscription and timer. The instance's engine is gone, even
    /// if it leaked a clone of these services.
    pub(crate) fn close(&self) {
        self.0.closed.store(true, Ordering::Release);
        for (_, task) in std::mem::take(&mut *self.0.tasks.lock().unwrap()) {
            task.task.abort();
        }
    }

    /// True once Rift gave up waiting for this instance to stop. Engines that
    /// can interrupt guest code should poll this.
    pub fn stop_requested(&self) -> bool {
        self.0.shared.stop.load(Ordering::Acquire) || self.0.closed.load(Ordering::Acquire)
    }

    pub fn players(&self) -> PlayersApi<'_> {
        PlayersApi(&self.0)
    }

    pub fn store(&self) -> StoreApi<'_> {
        StoreApi(&self.0)
    }

    pub fn permissions(&self) -> PermissionsApi<'_> {
        PermissionsApi(&self.0)
    }

    pub fn bus(&self) -> BusApi<'_> {
        BusApi(&self.0)
    }

    pub fn servers(&self) -> ServersApi<'_> {
        ServersApi(&self.0)
    }

    pub fn timers(&self) -> TimersApi<'_> {
        TimersApi(&self.0)
    }

    pub fn meta(&self) -> MetaApi<'_> {
        MetaApi(&self.0)
    }

    /// `log.log`: Rift's log stream, prefixed with the plugin name. Control
    /// characters are replaced so a message cannot forge other log lines.
    pub fn log(&self, level: Level, message: &str) {
        let message: String = message
            .chars()
            .take(MAX_LOG)
            .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
            .collect();
        match level {
            Level::Info => eprintln!("rift: plugin {}: {message}", self.0.spec.name),
            level => eprintln!(
                "rift: plugin {}: {}: {message}",
                self.0.spec.name,
                level.as_str()
            ),
        }
        self.0.base.monitor.log(&self.0.spec.name, level, message);
    }
}

impl Inner {
    fn handle(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    fn count(&self, kind: fn(&Kind) -> bool) -> usize {
        let mut tasks = self.tasks.lock().unwrap();
        tasks.retain(|_, task| !task.task.is_finished());
        tasks.values().filter(|task| kind(&task.kind)).count()
    }

    fn deliver(&self, event: Event) {
        let hook = event.hook();
        let queued = self.queue.notify(event);
        let outcome = if queued {
            super::monitor::Outcome::Queued
        } else {
            super::monitor::Outcome::Dropped
        };
        self.base.monitor.call(&self.spec.name, hook, outcome, None);
        if !queued {
            // Report the first drop and then every power of two.
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                eprintln!(
                    "rift: plugin {}: queue full; dropped {dropped} timer or bus event(s) so far",
                    self.spec.name
                );
            }
        }
    }
}

/// A JSON text component or plain text. Text starting with `{`, `[` or `"`
/// must be valid JSON (markup objects included); anything else is shown as
/// written.
fn component(text: &str, label: &str) -> Result<serde_json::Value, String> {
    Ok(match Text::json(text, label)? {
        Text::Component(component) => component,
        Text::Plain(text) => serde_json::Value::String(text),
    })
}

/// `players`: the joined players. `target` is a lowercase UUID or a name.
pub struct PlayersApi<'a>(&'a Arc<Inner>);

impl PlayersApi<'_> {
    fn players(&self) -> &Players {
        &self.0.base.players
    }

    fn send(&self, target: &str, action: PlayerAction) -> Result<bool, String> {
        self.players()
            .send(target, action)
            .map_err(|error| error.to_string())
    }

    pub fn list(&self, server: Option<&str>) -> Result<Vec<PlayerInfo>, String> {
        self.players().list(server)
    }

    pub fn get(&self, target: &str) -> Result<Option<PlayerInfo>, String> {
        self.players().get(target)
    }

    pub fn names(&self, prefix: Option<&str>) -> Vec<String> {
        self.players().names_with_prefix(prefix.unwrap_or_default())
    }

    pub fn message(&self, target: &str, message: &str) -> Result<bool, String> {
        self.message_text(target, Text::plain(message, "message")?)
    }

    /// `players.message-json`: a JSON text component.
    pub fn message_json(&self, target: &str, message: &str) -> Result<bool, String> {
        self.message_text(target, Text::json(message, "message")?)
    }

    /// A message already checked by [`Text`].
    pub fn message_text(&self, target: &str, message: Text) -> Result<bool, String> {
        self.send(target, PlayerAction::Message(message))
    }

    pub fn action_bar(&self, target: &str, message: &str) -> Result<bool, String> {
        self.action_bar_text(target, Text::plain(message, "action bar text")?)
    }

    /// `players.action-bar-json`: a JSON text component.
    pub fn action_bar_json(&self, target: &str, message: &str) -> Result<bool, String> {
        self.action_bar_text(target, Text::json(message, "action bar text")?)
    }

    /// An action bar already checked by [`Text`].
    pub fn action_bar_text(&self, target: &str, message: Text) -> Result<bool, String> {
        self.send(target, PlayerAction::ActionBar(message))
    }

    /// `players.title`: plain or rich text; an empty plain title or
    /// subtitle is left out.
    pub fn title(&self, target: &str, title: &TitleSpec) -> Result<bool, String> {
        let checked = |text: &Text, label: &str| -> Result<Option<Text>, String> {
            match text {
                Text::Plain(plain) if plain.is_empty() => Ok(None),
                Text::Plain(plain) if plain.len() > MAX_TEXT => {
                    Err(format!("{label} must contain at most 1024 bytes"))
                }
                text => text.validate(label).map(|()| Some(text.clone())),
            }
        };
        let main = checked(&title.title, "title")?;
        let subtitle = match &title.subtitle {
            Some(subtitle) => checked(subtitle, "subtitle")?,
            None => None,
        };
        if main.is_none() && subtitle.is_none() {
            return Err("title requires a title or subtitle".into());
        }
        let times = [title.fade_in, title.stay, title.fade_out];
        if times.iter().any(|ticks| !(0..=1200).contains(ticks)) {
            return Err("title times must be 0..=1200 ticks".into());
        }
        self.send(
            target,
            PlayerAction::Title {
                title: main,
                subtitle,
                times,
            },
        )
    }

    /// `players.title-json`: like `title`, with JSON text components (or
    /// markup objects); text that does not start with `{`, `[` or `"` is
    /// shown as written, and an empty title or subtitle is left out.
    pub fn title_json(
        &self,
        target: &str,
        title_json: &str,
        subtitle_json: Option<&str>,
        times: [i32; 3],
    ) -> Result<bool, String> {
        let rich = |text: &str, label: &str| -> Result<Text, String> {
            if text.is_empty() {
                Ok(Text::Plain(String::new()))
            } else {
                Text::json(text, label)
            }
        };
        self.title(
            target,
            &TitleSpec {
                title: rich(title_json, "title")?,
                subtitle: subtitle_json
                    .map(|text| rich(text, "subtitle"))
                    .transpose()?,
                fade_in: times[0],
                stay: times[1],
                fade_out: times[2],
            },
        )
    }

    pub fn tab_list(
        &self,
        target: &str,
        header_json: Option<&str>,
        footer_json: Option<&str>,
    ) -> Result<bool, String> {
        let action = if header_json.is_none() && footer_json.is_none() {
            PlayerAction::TabList(None)
        } else {
            PlayerAction::TabList(Some(crate::tab_list::Override {
                header: header_json
                    .map(|header| component(header, "tab list header"))
                    .transpose()?,
                footer: footer_json
                    .map(|footer| component(footer, "tab list footer"))
                    .transpose()?,
            }))
        };
        self.send(target, action)
    }

    pub fn boss_bar(
        &self,
        target: &str,
        name: &str,
        bar: Option<&BossBarSpec>,
    ) -> Result<bool, String> {
        let bar = bar
            .map(|bar| -> Result<_, String> {
                if !(0.0..=1.0).contains(&bar.progress) {
                    return Err("boss bar progress must be a number from 0 to 1".into());
                }
                let choice = |value: &str, field: &str, names: &[&str]| {
                    names
                        .iter()
                        .position(|name| *name == value)
                        .map(|index| index as u8)
                        .ok_or_else(|| {
                            format!("boss bar {field} must be one of {}", names.join(", "))
                        })
                };
                let flags = u8::from(bar.darken_sky)
                    | (u8::from(bar.play_music) << 1)
                    | (u8::from(bar.create_fog) << 2);
                Ok(crate::protocol::BossBar {
                    text: component(&bar.text_json, "boss bar text")?,
                    progress: bar.progress,
                    color: choice(&bar.color, "color", &crate::protocol::COLORS)?,
                    style: choice(&bar.style, "style", &crate::protocol::STYLES)?,
                    flags,
                })
            })
            .transpose()?;
        self.players()
            .boss_bar(target, name, bar)
            .map_err(str::to_owned)
    }

    /// `players.dialog`: show a dialog given as JSON (the vanilla dialog
    /// format). Clicks on its plugin actions come back to this plugin.
    pub fn dialog(&self, target: &str, dialog_json: &str) -> Result<bool, String> {
        self.show_dialog(target, crate::dialog::Dialog::parse_json(dialog_json)?)
    }

    /// Show an already validated dialog on behalf of this plugin.
    pub fn show_dialog(&self, target: &str, dialog: crate::dialog::Dialog) -> Result<bool, String> {
        let owner = crate::dialog::Owner::Plugin(self.0.spec.name.clone());
        self.players()
            .dialog(target, Some(dialog), owner)
            .map_err(|error| error.to_string())
    }

    /// `players.clear-dialog`: close the dialog Rift showed, if still open.
    pub fn clear_dialog(&self, target: &str) -> Result<bool, String> {
        let owner = crate::dialog::Owner::Plugin(self.0.spec.name.clone());
        self.players()
            .dialog(target, None, owner)
            .map_err(|error| error.to_string())
    }

    pub fn kick(&self, target: &str, reason: &str) -> Result<bool, String> {
        self.kick_text(target, Text::plain(reason, "kick reason")?)
    }

    /// `players.kick-json`: a JSON text component.
    pub fn kick_json(&self, target: &str, reason: &str) -> Result<bool, String> {
        self.kick_text(target, Text::json(reason, "kick reason")?)
    }

    /// A kick reason already checked by [`Text`].
    pub fn kick_text(&self, target: &str, reason: Text) -> Result<bool, String> {
        self.send(target, PlayerAction::Kick(reason))
    }

    pub fn transfer(&self, target: &str, server: &str) -> Result<bool, String> {
        self.players().transfer(target, server)
    }

    /// Returns how many players received the message.
    pub fn broadcast(&self, message: &str) -> Result<u32, String> {
        self.broadcast_to(message, None)
    }

    /// `players.broadcast-json`: a JSON text component.
    pub fn broadcast_json(&self, message: &str) -> Result<u32, String> {
        self.broadcast_text(Text::json(message, "message")?, None)
    }

    /// Like [`PlayersApi::broadcast`], only to the players on one server or
    /// service group when `server` is set.
    pub fn broadcast_to(&self, message: &str, server: Option<&str>) -> Result<u32, String> {
        self.broadcast_text(Text::plain(message, "message")?, server)
    }

    /// A message already checked by [`Text`], to everyone or to the players
    /// on one server or service group.
    pub fn broadcast_text(&self, message: Text, server: Option<&str>) -> Result<u32, String> {
        self.players()
            .broadcast(&message, server)
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
    }
}

/// `store`: the durable key-value store, under `plugin.<name>.<namespace>`.
pub struct StoreApi<'a>(&'a Arc<Inner>);

impl StoreApi<'_> {
    fn key(&self, namespace: &str, key: &str) -> Result<(String, String), String> {
        if namespace.is_empty() || key.is_empty() || key.len() > 128 {
            return Err("store namespace and key must contain 1..=128 bytes".into());
        }
        let full = format!("plugin.{}.{namespace}", self.0.spec.name);
        if full.len() > 128 {
            return Err(format!(
                "store namespace too long: {full:?} must fit in 128 bytes"
            ));
        }
        Ok((full, key.to_owned()))
    }

    fn deadline() -> Instant {
        Instant::now() + STORE_WAIT
    }

    pub fn get(&self, namespace: &str, key: &str) -> Result<Option<Vec<u8>>, String> {
        let (namespace, key) = self.key(namespace, key)?;
        self.0
            .base
            .store
            .get(namespace, key, Self::deadline())
            .map_err(|error| error.to_string())
    }

    pub fn set(&self, namespace: &str, key: &str, value: &[u8]) -> Result<(), String> {
        let key = self.key(namespace, key)?;
        self.0
            .base
            .store
            .mutate(Self::deadline(), |entries| {
                entries.insert(key, value.to_vec());
                Ok(())
            })
            .map_err(|error| error.to_string())
    }

    pub fn delete(&self, namespace: &str, key: &str) -> Result<bool, String> {
        let key = self.key(namespace, key)?;
        self.0
            .base
            .store
            .mutate(Self::deadline(), |entries| Ok(entries.remove(&key)))
            .map_err(|error| error.to_string())
    }

    pub fn increment(&self, namespace: &str, key: &str, delta: i64) -> Result<i64, String> {
        let key = self.key(namespace, key)?;
        self.0
            .base
            .store
            .increment(key, delta, Self::deadline())
            .map_err(|error| error.to_string())
    }

    pub fn list(&self, namespace: &str, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
        let (namespace, _) = self.key(namespace, "-")?;
        Ok(self
            .0
            .base
            .store
            .namespace(&namespace)
            .into_iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .collect())
    }
}

/// `permissions`: the contract's six calls over Rift's permission store
/// (see docs/permissions.md#persistent-plugins). Writes are recorded with
/// actor `lua:plugin:<name>` and nodes carry source `plugin:<name>`.
pub struct PermissionsApi<'a>(&'a Arc<Inner>);

impl PermissionsApi<'_> {
    fn authority(&self) -> &Authority {
        &self.0.base.permissions
    }

    /// The plugin name as an actor and source label, which allow 96 bytes
    /// of `[A-Za-z0-9_.:-]` (plugin names allow 128 of `[A-Za-z0-9_-]`).
    fn label(&self) -> String {
        let mut label = format!("plugin:{}", self.0.spec.name);
        label.truncate(96);
        label
    }

    /// `has`: the node resolves to true in the contexts the proxy knows for
    /// the player, like `ctx.has_permission`. Undefined is false.
    pub fn has(&self, uuid: &str, node: &str) -> bool {
        Uuid::parse(uuid)
            .is_ok_and(|uuid| self.authority().check_active(uuid, node) == Tristate::True)
    }

    /// The node in `contexts`, or in the player's current contexts:
    /// `Some(true)`, `Some(false)` or `None` for undefined.
    pub fn check(
        &self,
        uuid: &str,
        node: &str,
        contexts: Option<&ContextSet>,
    ) -> Result<Option<bool>, String> {
        let uuid = Uuid::parse(uuid).map_err(|error| error.to_string())?;
        let active = contexts
            .cloned()
            .unwrap_or_else(|| self.authority().contexts(uuid));
        Ok(self.authority().check(uuid, node, &active).as_option())
    }

    /// Prefix, suffix, meta and primary group as JSON, in `contexts` or the
    /// player's current contexts.
    pub fn meta(&self, uuid: &str, contexts: Option<&ContextSet>) -> Result<String, String> {
        let uuid = Uuid::parse(uuid).map_err(|error| error.to_string())?;
        let active = contexts
            .cloned()
            .unwrap_or_else(|| self.authority().contexts(uuid));
        serde_json::to_string(&self.authority().meta(uuid, &active))
            .map_err(|error| error.to_string())
    }

    /// `set`: a global permission node of the user (`*`: of `default`);
    /// `None` removes it.
    pub fn set(&self, uuid: &str, node: &str, value: Option<bool>) -> Result<(), String> {
        let subject = if uuid == "*" {
            SubjectRef::Group(DEFAULT_GROUP.to_owned())
        } else {
            SubjectRef::User(Uuid::parse(uuid).map_err(|error| error.to_string())?)
        };
        let change = match value {
            Some(value) => Change::SetNode {
                subject,
                node: self.node(Node::permission(node, value))?,
            },
            None => Change::UnsetNode {
                subject,
                key: NodeKey::new(NodeKind::Permission, node, ContextSet::new())
                    .map_err(|error| error.to_string())?,
            },
        };
        self.apply(change).map(drop)
    }

    /// `add-group`: a global `inherit` node. Returns whether anything changed.
    pub fn add_group(&self, uuid: &str, group: &str) -> Result<bool, String> {
        let user = SubjectRef::User(Uuid::parse(uuid).map_err(|error| error.to_string())?);
        let node = self.node(Node::inherit(group, true))?;
        self.apply(Change::SetNode {
            subject: user,
            node,
        })
    }

    /// `remove-group`: removes the global `inherit` node. Returns whether
    /// anything changed.
    pub fn remove_group(&self, uuid: &str, group: &str) -> Result<bool, String> {
        let user = SubjectRef::User(Uuid::parse(uuid).map_err(|error| error.to_string())?);
        let key = NodeKey::new(NodeKind::Inherit, group, ContextSet::new())
            .map_err(|error| error.to_string())?;
        self.apply(Change::UnsetNode { subject: user, key })
    }

    /// `groups`: the groups the player is in, in the contexts the proxy
    /// knows for them: direct, inherited and `default`, in rank order.
    pub fn groups(&self, uuid: &str) -> Vec<String> {
        let Ok(uuid) = Uuid::parse(uuid) else {
            return Vec::new();
        };
        let active = self.authority().contexts(uuid);
        self.authority()
            .compiled(uuid)
            .groups(&active, rift_permissions::unix_now())
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// `list-groups`: every group of the store with its weight, display name
    /// and nodes, as a JSON object keyed by group name.
    pub fn list_groups(&self) -> String {
        serde_json::to_string(&*self.authority().groups()).unwrap_or_else(|_| "{}".into())
    }

    fn node(&self, node: Result<Node, rift_permissions::Error>) -> Result<Node, String> {
        let node = node.map_err(|error| error.to_string())?;
        Ok(match self.label().parse::<Source>() {
            Ok(source) => node.with_source(source),
            Err(_) => node,
        })
    }

    fn apply(&self, change: Change) -> Result<bool, String> {
        let actor = format!("lua:{}", self.label())
            .parse::<Actor>()
            .unwrap_or_else(|_| Actor::Lua("plugin".into()));
        self.authority()
            .apply_with(actor, change, None, Some(Instant::now() + STORE_WAIT))
            .map(|applied| applied.ops > 0)
            .map_err(|error| error.to_string())
    }
}

/// `bus`: the message bus. Trusted plugins may use any subject.
pub struct BusApi<'a>(&'a Arc<Inner>);

impl BusApi<'_> {
    pub fn publish(
        &self,
        subject: &str,
        payload: &[u8],
        reply: Option<&str>,
    ) -> Result<u32, String> {
        self.0
            .base
            .broker
            .publish_with_reply(subject, reply, Bytes::copy_from_slice(payload))
            .map(|report| u32::try_from(report.delivered).unwrap_or(u32::MAX))
            .map_err(|error| error.to_string())
    }

    pub fn request(
        &self,
        subject: &str,
        payload: &[u8],
        timeout_ms: u32,
    ) -> Result<Vec<u8>, String> {
        if !(1..=MAX_REQUEST_MS).contains(&timeout_ms) {
            return Err(format!(
                "bus request timeout must be 1..={MAX_REQUEST_MS} ms"
            ));
        }
        let broker = self.0.base.broker.clone();
        let payload = Bytes::copy_from_slice(payload);
        self.0
            .handle
            .block_on(broker.request(subject, payload, Duration::from_millis(timeout_ms.into())))
            .map(|message| message.payload.to_vec())
            .map_err(|error| error.to_string())
    }

    /// Messages arrive as [`Event::Message`] with the returned handle.
    pub fn subscribe(&self, subject: &str, queue_group: Option<&str>) -> Result<u64, String> {
        if self.0.closed.load(Ordering::Acquire) {
            return Err("plugin is stopping".into());
        }
        if self.0.count(|kind| matches!(kind, Kind::Subscription)) >= MAX_SUBSCRIPTIONS {
            return Err(format!(
                "a plugin may hold at most {MAX_SUBSCRIPTIONS} subscriptions"
            ));
        }
        let broker = self.0.base.broker.clone();
        let mut subscription = broker
            .subscribe(subject, queue_group)
            .map_err(|error| error.to_string())?;
        let id = self.0.handle();
        let inner = Arc::downgrade(self.0);
        let (subject, queue_group) = (subject.to_owned(), queue_group.map(str::to_owned));
        let task = self.0.handle.spawn(async move {
            loop {
                let message = match subscription.recv().await {
                    Ok(message) => message,
                    Err(BusError::SlowConsumer) => {
                        match broker.subscribe(&subject, queue_group.as_deref()) {
                            Ok(next) => {
                                subscription = next;
                                continue;
                            }
                            Err(_) => break,
                        }
                    }
                    Err(_) => break,
                };
                let Some(inner) = inner.upgrade() else { break };
                inner.deliver(Event::Message(BusMessage {
                    handle: id,
                    subject: message.subject.to_string(),
                    payload: message.payload.to_vec(),
                    reply: message.reply.map(|reply| reply.to_string()),
                }));
            }
        });
        self.0.tasks.lock().unwrap().insert(
            id,
            Task {
                kind: Kind::Subscription,
                task,
            },
        );
        Ok(id)
    }

    pub fn unsubscribe(&self, handle: u64) -> bool {
        remove(self.0, handle, |kind| matches!(kind, Kind::Subscription))
    }

    /// Append to the first retained stream that captures `subject`.
    pub fn stream_publish(&self, subject: &str, payload: &[u8]) -> Result<u64, String> {
        let streams = self.0.base.streams.read().unwrap().clone();
        let payload = Bytes::copy_from_slice(payload);
        self.0.handle.block_on(async {
            for stream in streams.values() {
                match stream.publish(subject, None, payload.clone()).await {
                    Ok(record) => return Ok(record.sequence),
                    Err(BusError::StreamNotMatched) => continue,
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err(format!("no retained stream captures {subject:?}"))
        })
    }
}

fn remove(inner: &Inner, handle: u64, kind: fn(&Kind) -> bool) -> bool {
    let mut tasks = inner.tasks.lock().unwrap();
    match tasks.get(&handle) {
        Some(task) if kind(&task.kind) && !task.task.is_finished() => {
            if let Some(task) = tasks.remove(&handle) {
                task.task.abort();
            }
            true
        }
        _ => false,
    }
}

/// `servers`: managed servers through `rift.control.*`, awaiting the reply.
pub struct ServersApi<'a>(&'a Arc<Inner>);

impl ServersApi<'_> {
    fn operation(&self, operation: &str, field: &str, target: &str) -> Result<String, String> {
        if target.is_empty() || target.len() > 256 {
            return Err(format!(
                "servers.{operation}: expected a {field} name of 1..=256 bytes"
            ));
        }
        self.control(operation, &serde_json::json!({ field: target }).to_string())
    }

    pub fn create(&self, group: &str) -> Result<String, String> {
        self.operation("create", "group", group)
    }

    pub fn remove(&self, name: &str) -> Result<String, String> {
        self.operation("remove", "name", name)
    }

    pub fn start(&self, name: &str) -> Result<String, String> {
        self.operation("start", "name", name)
    }

    pub fn stop(&self, name: &str) -> Result<String, String> {
        self.operation("stop", "name", name)
    }

    pub fn download(&self, template: &str) -> Result<String, String> {
        self.operation("download", "template", template)
    }

    /// Any `rift.control.<operation>` with a JSON object payload.
    pub fn control(&self, operation: &str, payload_json: &str) -> Result<String, String> {
        if operation.is_empty()
            || operation.len() > 64
            || !operation
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_')
        {
            return Err("servers.control: operation must be 1..=64 bytes of a-z or '_'".into());
        }
        if payload_json.len() > 64 * 1024
            || !serde_json::from_str::<serde_json::Value>(payload_json)
                .is_ok_and(|value| value.is_object())
        {
            return Err("servers.control: payload must be a JSON object of at most 64 KiB".into());
        }
        let broker = self.0.base.broker.clone();
        let reply = self
            .0
            .handle
            .block_on(broker.request(
                &format!("rift.control.{operation}"),
                Bytes::copy_from_slice(payload_json.as_bytes()),
                CONTROL_WAIT,
            ))
            .map_err(|error| match error {
                BusError::NoResponders => "the control service is unavailable".to_owned(),
                error => error.to_string(),
            })?;
        String::from_utf8(reply.payload.to_vec())
            .map_err(|_| "the control service replied with invalid UTF-8".into())
    }
}

/// `timers`: fire as [`Event::Timer`] with the returned handle. A tick that
/// finds the plugin's queue full is dropped, never queued up.
pub struct TimersApi<'a>(&'a Arc<Inner>);

impl TimersApi<'_> {
    fn start(&self, ms: u32, repeat: bool) -> u64 {
        let id = self.0.handle();
        if self.0.closed.load(Ordering::Acquire)
            || self.0.count(|kind| matches!(kind, Kind::Timer)) >= MAX_TIMERS
        {
            // The handle never fires; `cancel` reports it as unknown. The
            // contract has no error result for timers.
            self.0.spec_log(&format!(
                "timer not started: at most {MAX_TIMERS} timers per plugin"
            ));
            return id;
        }
        let inner = Arc::downgrade(self.0);
        let period = Duration::from_millis(ms.into());
        let task = self.0.handle.spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                let Some(inner) = inner.upgrade() else { break };
                inner.deliver(Event::Timer(id));
                if !repeat {
                    break;
                }
            }
        });
        self.0.tasks.lock().unwrap().insert(
            id,
            Task {
                kind: Kind::Timer,
                task,
            },
        );
        id
    }

    /// Repeats every `ms` milliseconds (at least 10).
    pub fn every(&self, ms: u32) -> u64 {
        self.start(ms.max(MIN_TIMER_MS), true)
    }

    pub fn after(&self, ms: u32) -> u64 {
        self.start(ms, false)
    }

    pub fn cancel(&self, handle: u64) -> bool {
        remove(self.0, handle, |kind| matches!(kind, Kind::Timer))
    }
}

impl Inner {
    fn spec_log(&self, message: &str) {
        eprintln!("rift: plugin {}: warn: {message}", self.spec.name);
    }
}

/// `meta`. Directories are host paths; a sandboxing engine maps them to its
/// guest paths (`/data`, `/host`).
pub struct MetaApi<'a>(&'a Arc<Inner>);

impl MetaApi<'_> {
    pub fn name(&self) -> &str {
        &self.0.spec.name
    }

    pub fn version(&self) -> &str {
        &self.0.spec.manifest.version
    }

    pub fn data_dir(&self) -> PathBuf {
        self.0.spec.data_dir()
    }

    /// The configuration directory, when the manifest grants `host`.
    pub fn config_dir(&self) -> Option<&Path> {
        self.0
            .spec
            .manifest
            .capabilities
            .filesystem
            .contains(&Filesystem::Host)
            .then_some(self.0.spec.config_root.as_path())
    }

    /// Monotonic seconds since Rift started.
    pub fn clock(&self) -> f64 {
        crate::script::started().elapsed().as_secs_f64()
    }

    /// Unix time in seconds.
    pub fn time(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() as i64)
    }
}
