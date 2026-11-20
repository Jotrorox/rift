//! Online players and the actions Lua and the BungeeCord bridge can queue for
//! them. Each session drains its own bounded queue from its connection loop;
//! callers never touch a socket.
//!
//! Joins, server switches and quits are published on `rift.events.players`
//! and tell the permission authority who is online and on which server.
//!
//! The directory also owns the network roster ([`PlayerRegistry`]) and keeps
//! each player's visibility rule on both records: the `rift.visible-to` meta
//! value, resolved in the player's contexts when they log in, join, switch
//! servers and whenever their permissions change (see [`Players::watch`]).
//! The roster record's tab list display name, the player's prefix and suffix
//! around their name, is resolved at the same moments from the join on.
//!
//! In a cluster, the players of other peers are on the roster too, and every
//! action for one of them goes to its peer through a [`PlayerRouter`] (see
//! [`routing`]). Synchronous callers (Lua, plugins, the BungeeCord bridge)
//! cannot wait for that peer's answer: for them, `true` means the action
//! went to the player's peer. The `async` variants ([`Players::deliver`],
//! [`Players::broadcast_routed`], [`Players::route_transfer`]) wait for it.
use super::fields;
use crate::{
    messaging::Broker,
    permissions::Authority,
    players::{
        PlayerRegistry, RemotePlayer, VISIBLE_TO, Viewer,
        claims::{ClaimChange, ClaimOp, Claims, Refusal},
    },
    text::{
        Text, Unrendered,
        catalog::{self, Notice},
    },
};
use mlua::{Lua, Table, Value};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub mod routing;
use routing::{LocalRouter, PlayerRouter, SessionHooks, WireBody, WireRequest, WireResult};

/// Undelivered actions per player. A full queue reports failure instead of waiting.
const QUEUE: usize = 32;
const MAX_TEXT: usize = 1024;
/// Action calls per invocation; a broadcast counts once.
const MAX_CALLS: usize = 256;
const MAX_TITLE_MS: i64 = 60_000;
/// How long [`Players::disconnect_all`] waits for one session's queue.
const DISCONNECT_WAIT: Duration = Duration::from_secs(5);

/// An action for one player's session. Serializable, so that a peer of a
/// cluster can send it to the peer that holds the player.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PlayerAction {
    /// Plain text or a JSON text component already checked by `Text`.
    Message(Text),
    ActionBar(Text),
    /// Fade-in, stay and fade-out are ticks.
    /// At least one of title and subtitle is set.
    Title {
        title: Option<Text>,
        subtitle: Option<Text>,
        times: [i32; 3],
    },
    Kick(Text),
    Transfer(String),
    /// A BungeeCord plugin message for the player's backend. The session drops
    /// it if the player has left `server` before the message can be sent.
    Forward {
        server: String,
        data: Vec<u8>,
    },
    /// A tab list header and footer for this player; `None` restores the
    /// configured ones.
    TabList(Option<crate::tab_list::Override>),
    /// Show, change or (with `None`) remove one of the player's boss bars.
    BossBar {
        name: String,
        #[serde(with = "boss_bar_wire")]
        bar: Option<crate::protocol::BossBar>,
    },
    /// Show a dialog, replacing any open one; clicks on its plugin actions go
    /// to `owner`.
    Dialog {
        dialog: Arc<crate::dialog::Dialog>,
        owner: crate::dialog::Owner,
    },
    /// Close the dialog Rift showed, if it is still open.
    ClearDialog,
    /// Show a menu, replacing any open menu or dialog of Rift's, or redraw
    /// the open one with the same `id` and owner; its events go to the
    /// owner.
    Menu(crate::menu::Shown),
    /// Change part of the menu the player has open, if it is still the
    /// menu of that `id` and owner.
    UpdateMenu(crate::menu::Update),
    /// Close the menu Rift shows: any menu without an `id`, else only the
    /// menu `id` of `owner`.
    CloseMenu {
        owner: crate::dialog::Owner,
        id: Option<String>,
    },
    /// Leave this proxy: a 1.20.5+ client in configuration or play gets the
    /// Transfer packet to `host:port` (another peer of the cluster); any
    /// other is disconnected with `fallback`. Sent by a peer that drains.
    Handoff {
        host: String,
        port: u16,
        fallback: Text,
    },
}

/// What [`Players::hand_off`] did: sessions told to go to another peer, and
/// sessions told to reconnect. A session that did not take the action in
/// time counts in neither.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HandOff {
    pub transferred: usize,
    pub disconnected: usize,
}

/// The serde form of a boss bar, which the protocol type does not have.
mod boss_bar_wire {
    use crate::protocol::BossBar;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value as Json;

    #[derive(Serialize)]
    struct Borrowed<'a> {
        text: &'a Json,
        progress: f32,
        color: u8,
        style: u8,
        flags: u8,
    }

    #[derive(Deserialize)]
    struct Owned {
        text: Json,
        progress: f32,
        color: u8,
        style: u8,
        flags: u8,
    }

    pub fn serialize<S: Serializer>(
        bar: &Option<BossBar>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        bar.as_ref()
            .map(|bar| Borrowed {
                text: &bar.text,
                progress: bar.progress,
                color: bar.color,
                style: bar.style,
                flags: bar.flags,
            })
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<BossBar>, D::Error> {
        Ok(
            Option::<Owned>::deserialize(deserializer)?.map(|bar| BossBar {
                text: bar.text,
                progress: bar.progress.clamp(0.0, 1.0),
                color: bar.color,
                style: bar.style,
                flags: bar.flags,
            }),
        )
    }
}

struct Entry {
    uuid: String,
    /// `uuid` as the roster keys it.
    key: [u8; 16],
    name: String,
    /// The node a viewer needs to see this player; `None` is everyone.
    visible_to: Option<Arc<str>>,
    /// The client's language, once its Client Information arrived.
    locale: Option<String>,
    /// The language a plugin set for the player, from the directory.
    language: Option<String>,
    /// The address the player connected from, without the port.
    ip: String,
    properties: Vec<crate::plugins::api::ProfileProperty>,
    server: String,
    protocol: i32,
    actions: mpsc::Sender<PlayerAction>,
    /// Names of the boss bars queued for this player and not removed since.
    boss_bars: BTreeSet<String>,
}

pub(crate) type Destinations = Arc<Mutex<BTreeSet<String>>>;
pub(crate) type Groups = Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>;

/// Joined players, shared by every reload generation and listener.
#[derive(Clone)]
pub struct Players {
    sessions: Arc<Mutex<BTreeMap<u64, Entry>>>,
    /// The network roster sessions, tab lists and counts read.
    roster: Arc<PlayerRegistry>,
    destinations: Destinations,
    groups: Groups,
    events: Broker,
    permissions: Arc<Authority>,
    /// Every player who ever joined, online or not.
    directory: crate::directory::Directory,
    /// `players.record_ip` of the configuration in effect.
    record_ip: Arc<std::sync::atomic::AtomicBool>,
    /// Counts language changes, so sessions notice one without a lock.
    languages: Arc<std::sync::atomic::AtomicU64>,
    /// `tab_list.format` of the configuration in effect.
    tab_format: Arc<Mutex<Option<Arc<str>>>>,
    /// Delivers actions for other peers' players.
    router: Arc<RwLock<Arc<dyn PlayerRouter>>>,
    /// What remote requests need from the running proxy.
    hooks: Arc<RwLock<Option<Arc<dyn SessionHooks>>>>,
    /// Events of menus this peer showed to other peers' players that wait
    /// for their plugin here, by player UUID. The player's own session
    /// counts an event only until it crossed.
    remote_menu_events: Arc<Mutex<BTreeMap<String, Arc<std::sync::atomic::AtomicUsize>>>>,
}

/// A player's tab list name: `format` with `{prefix}`, `{name}`, `{suffix}`,
/// `{group}` and `{server}` replaced in one pass, so a prefix that contains
/// a placeholder stays text. Without a format it is the prefix and suffix
/// around the name, and `None` when the player has neither.
pub(crate) fn format_name(
    format: Option<&str>,
    prefix: &str,
    name: &str,
    suffix: &str,
    group: &str,
    server: &str,
) -> Option<String> {
    let Some(format) = format else {
        return (!prefix.is_empty() || !suffix.is_empty())
            .then(|| format!("{prefix}{name}{suffix}"));
    };
    let mut result = String::with_capacity(format.len() + prefix.len() + name.len() + suffix.len());
    let mut rest = format;
    'outer: while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        rest = &rest[start..];
        for (key, value) in [
            ("{prefix}", prefix),
            ("{name}", name),
            ("{suffix}", suffix),
            ("{group}", group),
            ("{server}", server),
        ] {
            if let Some(after) = rest.strip_prefix(key) {
                result.push_str(value);
                rest = after;
                continue 'outer;
            }
        }
        result.push('{');
        rest = &rest[1..];
    }
    result.push_str(rest);
    Some(result)
}

/// What a session knows about its player beyond the name.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// The address the player connected from, without the port.
    pub ip: String,
    pub properties: Vec<crate::plugins::api::ProfileProperty>,
}

/// The catalogue language for a client locale: `de_DE` is `de_de`.
pub fn locale_language(locale: &str) -> Option<String> {
    crate::directory::language_tag(locale).ok()
}

pub const EVENTS: &str = "rift.events.players";

/// Presence in the directory. Dropping it removes the player and closes its queue.
pub struct PlayerHandle {
    players: Players,
    id: u64,
    uuid: rift_permissions::Uuid,
}

impl PlayerHandle {
    /// Commit the server only after the player has joined it.
    pub fn set_server(&self, server: &str) {
        let mut sessions = self.players.lock();
        let Some(entry) = sessions.get_mut(&self.id) else {
            return;
        };
        if entry.server == server {
            return;
        }
        let group = self.players.group_of(server);
        self.players
            .permissions
            .switch_server(self.uuid, server, group.as_deref());
        // Contexts changed, so the rule may have: resolve it before the
        // switch is published or the roster moves the player.
        let visible_to = self.players.visibility(&entry.key, server);
        entry.visible_to = visible_to.clone();
        self.players.roster.set_visibility(&entry.key, visible_to);
        self.players.decorate(&entry.key, &entry.name, server);
        let previous = std::mem::replace(&mut entry.server, server.to_owned());
        let event = self
            .players
            .event("switch", self.id, entry, Some(&previous));
        drop(sessions);
        self.players.publish(event);
    }

    /// The language Rift's messages use for this player: what a plugin set,
    /// else the client's locale.
    pub fn language(&self) -> Option<String> {
        self.players
            .lock()
            .get(&self.id)
            .and_then(Players::effective_language)
    }

    /// The language the client reported, as the session last read it.
    pub fn set_locale(&self, locale: Option<&str>) {
        if let Some(entry) = self.players.lock().get_mut(&self.id)
            && entry.locale.as_deref() != locale
        {
            entry.locale = locale.map(str::to_owned);
            self.players.client_attributes(entry);
        }
    }
}

impl Drop for PlayerHandle {
    fn drop(&mut self) {
        let removed = self.players.lock().remove(&self.id);
        self.players.permissions.leave(self.uuid);
        if let Some(entry) = removed {
            let directory = self.players.directory.clone();
            let (uuid, server) = (entry.uuid.clone(), entry.server.clone());
            background(move || directory.left(&uuid, &server));
            let event = self.players.event("quit", self.id, &entry, None);
            self.players.publish(event);
        }
    }
}

/// Run a directory write off the session's task when there is a runtime.
fn background(work: impl FnOnce() + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn_blocking(work)),
        Err(_) => work(),
    }
}

fn error(message: &str) -> mlua::Error {
    mlua::Error::runtime(message)
}

/// Lua sees the same message as before `Players::send` became engine-neutral.
fn lua_error(error: std::io::Error) -> mlua::Error {
    mlua::Error::runtime(error.to_string())
}

fn text(value: String, label: &str) -> mlua::Result<String> {
    if value.is_empty() || value.len() > MAX_TEXT {
        return Err(mlua::Error::runtime(format!(
            "{label} must contain 1..=1024 bytes"
        )));
    }
    Ok(value)
}

fn ticks(options: &Table, field: &str, default: i64) -> mlua::Result<i32> {
    match options.raw_get::<Value>(field)? {
        Value::Nil => Ok((default / 50) as i32),
        Value::Integer(ms) if (0..=MAX_TITLE_MS).contains(&ms) => Ok((ms / 50) as i32),
        _ => Err(mlua::Error::runtime(format!(
            "title {field} must be an integer in 0..=60000"
        ))),
    }
}

pub(crate) fn title(value: Value) -> mlua::Result<PlayerAction> {
    let options = match value {
        Value::String(title) => {
            return Ok(PlayerAction::Title {
                title: Some(Text::Plain(text(title.to_str()?.to_owned(), "title")?)),
                subtitle: None,
                times: [10, 70, 20],
            });
        }
        Value::Table(options) => options,
        _ => return Err(error("title expects a string or an options table")),
    };
    fields(
        &options,
        &["title", "subtitle", "fade_in_ms", "stay_ms", "fade_out_ms"],
    )?;
    // Each is a string or a chat component table, such as `{ mini = "..." }`.
    let optional = |field| -> mlua::Result<Option<Text>> {
        match options.raw_get::<Value>(field)? {
            Value::Nil => Ok(None),
            value => Text::lua(value, field)
                .map(Some)
                .map_err(mlua::Error::runtime),
        }
    };
    let (title, subtitle) = (optional("title")?, optional("subtitle")?);
    if title.is_none() && subtitle.is_none() {
        return Err(error("title requires a title or subtitle"));
    }
    Ok(PlayerAction::Title {
        title,
        subtitle,
        times: [
            ticks(&options, "fade_in_ms", 500)?,
            ticks(&options, "stay_ms", 3500)?,
            ticks(&options, "fade_out_ms", 1000)?,
        ],
    })
}

pub(crate) fn tab_list(value: Option<Table>) -> mlua::Result<PlayerAction> {
    let Some(options) = value else {
        return Ok(PlayerAction::TabList(None));
    };
    fields(&options, &["header", "footer"])?;
    let text = |field: &str| -> mlua::Result<Option<serde_json::Value>> {
        match options.raw_get::<Value>(field)? {
            Value::Nil => Ok(None),
            value => crate::server_list::motd(value, &format!("tab list {field}"))
                .map(Some)
                .map_err(mlua::Error::runtime),
        }
    };
    Ok(PlayerAction::TabList(Some(crate::tab_list::Override {
        header: text("header")?,
        footer: text("footer")?,
    })))
}

pub(crate) fn boss_bar(name: String, value: Option<Table>) -> mlua::Result<PlayerAction> {
    if !super::token(&name) {
        return Err(error(
            "boss bar name must be 1..=64 lowercase letters, digits, '.', '_' or '-'",
        ));
    }
    let Some(options) = value else {
        return Ok(PlayerAction::BossBar { name, bar: None });
    };
    fields(
        &options,
        &[
            "text",
            "progress",
            "color",
            "style",
            "darken_screen",
            "play_music",
            "create_fog",
        ],
    )?;
    let text = crate::server_list::motd(options.raw_get("text")?, "boss bar text")
        .map_err(mlua::Error::runtime)?;
    let progress = match options.raw_get::<Value>("progress")? {
        Value::Nil => 1.0,
        Value::Integer(value) => value as f64,
        Value::Number(value) => value,
        _ => return Err(error("boss bar progress must be a number from 0 to 1")),
    };
    if !(0.0..=1.0).contains(&progress) {
        return Err(error("boss bar progress must be a number from 0 to 1"));
    }
    let choice = |field: &str, names: &[&str]| -> mlua::Result<u8> {
        match options.raw_get::<Option<String>>(field)? {
            None => Ok(0),
            Some(value) => names
                .iter()
                .position(|name| *name == value)
                .map(|index| index as u8)
                .ok_or_else(|| {
                    mlua::Error::runtime(format!(
                        "boss bar {field} must be one of {}",
                        names.join(", ")
                    ))
                }),
        }
    };
    let mut flags = 0;
    for (bit, field) in crate::protocol::FLAG_NAMES.iter().enumerate() {
        if options.raw_get::<Option<bool>>(*field)?.unwrap_or(false) {
            flags |= 1 << bit;
        }
    }
    Ok(PlayerAction::BossBar {
        name,
        bar: Some(crate::protocol::BossBar {
            text,
            progress: progress as f32,
            color: choice("color", &crate::protocol::COLORS)?,
            style: choice("style", &crate::protocol::STYLES)?,
            flags,
        }),
    })
}

/// A dialog from Lua: a table in the vanilla dialog format or the same as
/// JSON text.
pub(crate) fn dialog(value: Value) -> mlua::Result<crate::dialog::Dialog> {
    match value {
        Value::String(text) => crate::dialog::Dialog::parse_json(&text.to_str()?),
        Value::Table(_) => {
            crate::dialog::from_lua(value).and_then(|spec| crate::dialog::Dialog::parse(&spec))
        }
        _ => Err("dialog expects a table or JSON text".into()),
    }
    .map_err(mlua::Error::runtime)
}

/// Values of a menu table built in Lua.
const MAX_MENU_LUA_VALUES: usize = 65_536;

/// A menu from Lua: a table in the format of docs/menus.md or the same as
/// JSON text.
pub(crate) fn menu(value: Value) -> mlua::Result<crate::menu::Menu> {
    match value {
        Value::String(text) => crate::menu::Menu::parse_json(&text.to_str()?),
        Value::Table(_) => crate::dialog::lua_json(value, "menu", MAX_MENU_LUA_VALUES)
            .and_then(|spec| crate::menu::Menu::parse(&spec)),
        _ => Err("menu expects a table or JSON text".into()),
    }
    .map_err(mlua::Error::runtime)
}

/// A menu patch from Lua (`update_menu`): a table in the format of
/// docs/menus.md or the same as JSON text.
pub(crate) fn menu_patch(value: Value) -> mlua::Result<crate::menu::Patch> {
    match value {
        Value::String(text) => crate::menu::Patch::parse_json(&text.to_str()?),
        Value::Table(_) => crate::dialog::lua_json(value, "patch", MAX_MENU_LUA_VALUES)
            .and_then(|spec| crate::menu::Patch::parse(&spec)),
        _ => Err("patch expects a table or JSON text".into()),
    }
    .map_err(mlua::Error::runtime)
}

const INVALID_TARGET: &str = "player must be a lowercase UUID (32 hex digits) or a player name";

/// Whether `target` is a lowercase UUID (else a name).
fn is_uuid(target: &str) -> bool {
    target.len() == 32
        && target
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Find a player by lowercase UUID or case-insensitive name.
fn find<'a>(
    sessions: &'a BTreeMap<u64, Entry>,
    target: &str,
) -> Result<Option<(&'a u64, &'a Entry)>, &'static str> {
    let uuid = is_uuid(target);
    if !uuid && !crate::players::valid_username(target) {
        return Err(INVALID_TARGET);
    }
    Ok(sessions.iter().find(|(_, entry)| {
        if uuid {
            entry.uuid == target
        } else {
            entry.name.eq_ignore_ascii_case(target)
        }
    }))
}

/// The skins of the online players whose heads `buttons` show, by the name
/// or UUID the buttons give.
fn heads<'a>(
    sessions: &BTreeMap<u64, Entry>,
    buttons: impl Iterator<Item = &'a crate::menu::Button>,
) -> BTreeMap<String, crate::menu::Textures> {
    let mut heads = BTreeMap::new();
    for button in buttons {
        let Some(crate::menu::Head::Player(player)) = &button.look.head else {
            continue;
        };
        let uuid = player.replace('-', "").to_ascii_lowercase();
        let textures = sessions
            .values()
            .find(|entry| entry.uuid == uuid || entry.name.eq_ignore_ascii_case(player))
            .and_then(|entry| {
                entry
                    .properties
                    .iter()
                    .find(|property| property.name == "textures")
            })
            .map(|property| crate::menu::Textures {
                value: property.value.clone(),
                signature: property.signature.clone(),
            });
        if let Some(textures) = textures {
            heads.insert(player.clone(), textures);
        }
    }
    heads
}

fn hex(uuid: &[u8; 16]) -> String {
    uuid.iter().map(|b| format!("{b:02x}")).collect()
}

/// The UUID a lowercase hexadecimal target names.
fn parse_uuid(target: &str) -> Option<[u8; 16]> {
    if !is_uuid(target) {
        return None;
    }
    let mut uuid = [0; 16];
    for (index, byte) in uuid.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&target[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(uuid)
}

/// Queue a boss bar change for `entry`, tracking the per-player limit.
fn queue_boss_bar(
    entry: &mut Entry,
    name: &str,
    bar: Option<crate::protocol::BossBar>,
) -> Result<bool, &'static str> {
    if !crate::protocol::boss_bars_supported(entry.protocol) {
        return Ok(false);
    }
    let shown = bar.is_some();
    if shown
        && !entry.boss_bars.contains(name)
        && entry.boss_bars.len() >= crate::protocol::MAX_BOSS_BARS
    {
        return Err("a player can have at most 8 boss bars");
    }
    let action = PlayerAction::BossBar {
        name: name.to_owned(),
        bar,
    };
    if entry.actions.try_send(action).is_err() {
        return Ok(false);
    }
    if shown {
        entry.boss_bars.insert(name.to_owned());
    } else {
        entry.boss_bars.remove(name);
    }
    Ok(true)
}

impl Players {
    pub(crate) fn new(
        destinations: Destinations,
        groups: Groups,
        events: Broker,
        permissions: Arc<Authority>,
    ) -> Self {
        // The table lives beside the permission store, in the same file.
        let directory = crate::directory::Directory::open(permissions.database_handle().clone())
            .unwrap_or_else(|error| {
                eprintln!("rift: the player directory is kept in memory only: {error}");
                crate::directory::Directory::memory()
            });
        Self {
            sessions: Arc::default(),
            // A cluster peer's presence component holds the roster already.
            roster: crate::cluster::attach::take_roster().unwrap_or_default(),
            destinations,
            groups,
            events,
            permissions,
            directory,
            record_ip: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            languages: Arc::default(),
            tab_format: Arc::default(),
            router: Arc::new(RwLock::new(Arc::new(LocalRouter))),
            hooks: Arc::default(),
            remote_menu_events: Arc::default(),
        }
    }

    /// Deliver actions for other peers' players through `router`.
    pub fn set_router(&self, router: Arc<dyn PlayerRouter>) {
        *self.router.write().unwrap_or_else(PoisonError::into_inner) = router;
    }

    fn router(&self) -> Arc<dyn PlayerRouter> {
        self.router
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// What remote transfers and dialog clicks run through.
    pub fn set_session_hooks(&self, hooks: Arc<dyn SessionHooks>) {
        *self.hooks.write().unwrap_or_else(PoisonError::into_inner) = Some(hooks);
    }

    fn hooks(&self) -> Option<Arc<dyn SessionHooks>> {
        self.hooks
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// `tab_list.format`: how tab lists name players. A change renames every
    /// joined player at once.
    pub fn set_tab_format(&self, format: Option<&str>) {
        {
            let mut current = self
                .tab_format
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if current.as_deref() == format {
                return;
            }
            *current = format.map(Arc::from);
        }
        let sessions = self.lock();
        for entry in sessions.values() {
            self.decorate(&entry.key, &entry.name, &entry.server);
        }
    }

    /// `players.record_ip`: whether joins remember the player's address.
    pub fn set_record_ip(&self, record: bool) {
        self.record_ip
            .store(record, std::sync::atomic::Ordering::Relaxed);
    }

    /// Changes whenever a plugin sets a player's language.
    pub fn language_generation(&self) -> u64 {
        self.languages.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The language a plugin set for a player, by lowercase UUID; known
    /// before the player joins.
    pub fn stored_language(&self, uuid: &str) -> Option<String> {
        if uuid.is_empty() {
            return None;
        }
        self.directory.language(uuid)
    }

    /// The language Rift uses for an online player by lowercase UUID or
    /// name, as `player-info.language` reports it; `None` when the player
    /// is offline, the target is not valid, or Rift uses the default.
    pub fn language_of(&self, target: &str) -> Option<String> {
        {
            let sessions = self.lock();
            match find(&sessions, target) {
                Ok(Some((_, entry))) => return Self::effective_language(entry),
                Ok(None) => {}
                Err(_) => return None,
            }
        }
        // A player of another peer, with the language that peer reported.
        self.remote(target)?
            .player
            .language
            .as_deref()
            .map(str::to_owned)
    }

    /// Run `convert`, which turns text for `target` into [`Text`], with the
    /// caller's `namespace` and the target's language, so a `message`
    /// object in it renders for the player who sees it.
    pub fn rendering_for<R>(
        &self,
        namespace: Option<&str>,
        target: &str,
        convert: impl FnOnce() -> R,
    ) -> R {
        let language = self.language_of(target);
        crate::text::rendering(namespace, language.as_deref(), convert)
    }

    fn effective_language(entry: &Entry) -> Option<String> {
        entry
            .language
            .clone()
            .or_else(|| entry.locale.as_deref().and_then(locale_language))
    }

    /// Tell other peers what the client of `entry` reported.
    fn client_attributes(&self, entry: &Entry) {
        let language = Self::effective_language(entry);
        self.roster.set_client(
            &entry.key,
            entry.protocol,
            entry.locale.as_deref(),
            language.as_deref(),
        );
    }

    fn record(&self, record: crate::directory::Record) -> crate::plugins::api::PlayerRecord {
        let online = self.lock().values().any(|entry| entry.uuid == record.uuid)
            || parse_uuid(&record.uuid)
                .is_some_and(|uuid| self.roster.remote_by_uuid(&uuid).is_some());
        crate::plugins::api::PlayerRecord {
            uuid: record.uuid,
            name: record.name,
            first_seen: record.first_seen,
            last_seen: record.last_seen,
            last_ip: record.last_ip,
            last_server: record.last_server,
            online,
            language: record.language,
        }
    }

    /// What Rift remembers about a player by UUID or last name, online or
    /// not.
    pub fn lookup(
        &self,
        target: &str,
    ) -> Result<Option<crate::plugins::api::PlayerRecord>, String> {
        Ok(self
            .directory
            .lookup(target)?
            .map(|record| self.record(record)))
    }

    /// Remembered players whose name starts with `prefix`, most recently
    /// seen first.
    pub fn search(
        &self,
        prefix: &str,
        limit: u32,
    ) -> Result<Vec<crate::plugins::api::PlayerRecord>, String> {
        Ok(self
            .directory
            .search(prefix, limit)?
            .into_iter()
            .map(|record| self.record(record))
            .collect())
    }

    /// Set (or with `None` clear) the language Rift's messages use for a
    /// player Rift has seen, by UUID or name. False for an unknown player.
    pub fn set_language(&self, target: &str, language: Option<&str>) -> Result<bool, String> {
        let language = language.map(crate::directory::language_tag).transpose()?;
        let Some(record) = self.directory.lookup(target)? else {
            return Ok(false);
        };
        if !self
            .directory
            .set_language(&record.uuid, language.as_deref())?
        {
            return Ok(false);
        }
        for entry in self.lock().values_mut() {
            if entry.uuid == record.uuid {
                entry.language.clone_from(&language);
                self.client_attributes(entry);
            }
        }
        self.languages
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(true)
    }

    /// Only the owning session changes its entry, one step at a time, so the
    /// events for one player are published in order.
    fn event(&self, kind: &str, id: u64, entry: &Entry, previous: Option<&str>) -> String {
        let mut event = serde_json::json!({
            "event": kind,
            "connection_id": id.to_string(),
            "uuid": entry.uuid,
            "name": entry.name,
            "server": entry.server,
            "visible_to": entry.visible_to.as_deref(),
        });
        if let Some(previous) = previous {
            event["previous"] = previous.into();
        }
        event.to_string()
    }

    fn publish(&self, event: String) {
        // Ephemeral: nobody listening, or a slow listener, never delays a session.
        let _ = self.events.publish(EVENTS, event.into());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Entry>> {
        self.sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Register a player once it is in a world. Actions wait in the returned
    /// queue until the session loop can write them.
    pub fn register(
        &self,
        id: u64,
        uuid: &[u8; 16],
        name: &str,
        protocol: i32,
        server: &str,
    ) -> (PlayerHandle, mpsc::Receiver<PlayerAction>) {
        self.register_with(id, uuid, name, protocol, server, Profile::default())
    }

    /// [`Players::register`] with the address and profile properties the
    /// session knows.
    pub fn register_with(
        &self,
        id: u64,
        uuid: &[u8; 16],
        name: &str,
        protocol: i32,
        server: &str,
        profile: Profile,
    ) -> (PlayerHandle, mpsc::Receiver<PlayerAction>) {
        let (actions, receiver) = mpsc::channel(QUEUE);
        let player = rift_permissions::Uuid::from_bytes(*uuid);
        self.permissions
            .join(player, name, server, self.group_of(server).as_deref());
        // The rule is in place before the join is published, and on the
        // roster record the login created (usually already with this rule).
        let visible_to = self.visibility(uuid, server);
        self.roster.set_visibility(uuid, visible_to.clone());
        let hex = hex(uuid);
        // Read before the join is written: a first join has no language.
        let language = self.directory.language(&hex);
        {
            let directory = self.directory.clone();
            let record_ip = self.record_ip.load(std::sync::atomic::Ordering::Relaxed);
            let ip = (record_ip && !profile.ip.is_empty()).then(|| profile.ip.clone());
            let (hex, name, server) = (hex.clone(), name.to_owned(), server.to_owned());
            background(move || directory.joined(&hex, &name, ip.as_deref(), &server));
        }
        self.decorate(uuid, name, server);
        let entry = Entry {
            uuid: hex,
            key: *uuid,
            visible_to,
            locale: None,
            language,
            ip: profile.ip,
            properties: profile.properties,
            name: name.to_owned(),
            server: server.to_owned(),
            protocol,
            actions,
            boss_bars: BTreeSet::new(),
        };
        self.client_attributes(&entry);
        let event = self.event("join", id, &entry, None);
        self.lock().insert(id, entry);
        self.publish(event);
        (
            PlayerHandle {
                players: self.clone(),
                id,
                uuid: player,
            },
            receiver,
        )
    }

    /// Names of joined players by lowercase UUID, on every peer.
    pub fn names(&self) -> BTreeMap<String, String> {
        let mut names: BTreeMap<String, String> = self
            .lock()
            .values()
            .map(|entry| (entry.uuid.clone(), entry.name.clone()))
            .collect();
        for remote in self.roster.remote_players() {
            names
                .entry(hex(&remote.player.uuid))
                .or_insert(remote.player.name);
        }
        names
    }

    /// Another peer's player by lowercase UUID or case-insensitive name; the
    /// target is already valid.
    fn remote(&self, target: &str) -> Option<RemotePlayer> {
        match parse_uuid(target) {
            Some(uuid) => self.roster.remote_by_uuid(&uuid),
            None => self.roster.remote_by_name(target),
        }
    }

    /// The request that carries `action` to a player of another peer.
    fn request(remote: &RemotePlayer, action: PlayerAction) -> WireRequest {
        WireRequest {
            incarnation: remote.incarnation,
            body: WireBody::Action {
                connection: remote.player.connection,
                uuid: remote.player.uuid,
                action,
            },
        }
    }

    /// Send `request` to `peer` without waiting for the answer; false when
    /// there is no runtime to send it from.
    fn dispatch(&self, peer: u16, request: WireRequest) -> bool {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        let delivery = self.router().deliver(peer, request);
        drop(runtime.spawn(async move {
            match delivery.await {
                Ok(WireResult::Error(reason)) => {
                    log_warn!("player action refused by peer {peer}: {reason}");
                }
                Err(error) => log_warn!("player action not delivered: {error}"),
                Ok(_) => {}
            }
        }));
        true
    }

    /// Queue an action for a lowercase UUID or case-insensitive player name.
    /// False means that the player is offline or has a full queue. For a
    /// player on another peer, true means the action went to that peer.
    pub fn send(&self, target: &str, action: PlayerAction) -> std::io::Result<bool> {
        {
            let sessions = self.lock();
            let found = find(&sessions, target).map_err(|message| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
            })?;
            if let Some((_, entry)) = found {
                return Ok(entry.actions.try_send(action).is_ok());
            }
        }
        Ok(self.remote(target).is_some_and(|remote| {
            self.dispatch(remote.player.peer, Self::request(&remote, action))
        }))
    }

    /// [`Players::send`] that waits for the answer of the player's peer.
    pub async fn deliver(&self, target: &str, action: PlayerAction) -> std::io::Result<bool> {
        let remote = {
            let sessions = self.lock();
            let found = find(&sessions, target).map_err(|message| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
            })?;
            if let Some((_, entry)) = found {
                return Ok(entry.actions.try_send(action).is_ok());
            }
            drop(sessions);
            self.remote(target)
        };
        let Some(remote) = remote else {
            return Ok(false);
        };
        let delivery = self
            .router()
            .deliver(remote.player.peer, Self::request(&remote, action));
        match delivery.await {
            Ok(WireResult::Sent(sent)) => Ok(sent),
            Ok(WireResult::Error(reason)) => Err(std::io::Error::other(reason)),
            Ok(_) => Ok(false),
            Err(error) => {
                log_warn!("player action not delivered: {error}");
                Ok(false)
            }
        }
    }

    /// Queue an action for a case-insensitive player name. False means that
    /// the player is offline or has a full queue.
    pub fn send_named(&self, name: &str, action: PlayerAction) -> bool {
        {
            let sessions = self.lock();
            if let Some(entry) = sessions
                .values()
                .find(|entry| entry.name.eq_ignore_ascii_case(name))
            {
                return entry.actions.try_send(action).is_ok();
            }
        }
        self.roster
            .remote_by_name(name)
            .is_some_and(|remote| self.dispatch(remote.player.peer, Self::request(&remote, action)))
    }

    /// Queue an action for every joined player and count those of this peer
    /// that accepted it; every other peer delivers it to its own players.
    pub fn send_all(&self, action: &PlayerAction) -> usize {
        let count = self.broadcast_local(None, action);
        self.fan_out(None, action);
        count
    }

    /// Queue `action` for this peer's players on `servers` (everyone for
    /// `None`) and count those that accepted it.
    fn broadcast_local(&self, servers: Option<&BTreeSet<String>>, action: &PlayerAction) -> usize {
        self.lock()
            .values()
            .filter(|entry| servers.is_none_or(|servers| servers.contains(&entry.server)))
            .filter(|entry| entry.actions.try_send(action.clone()).is_ok())
            .count()
    }

    /// The broadcast request for every other peer, with the incarnation it
    /// is addressed to.
    fn broadcasts(
        &self,
        servers: Option<&BTreeSet<String>>,
        action: &PlayerAction,
    ) -> Vec<(u16, WireRequest)> {
        let incarnations = self.roster.peer_incarnations();
        self.router()
            .peers()
            .into_iter()
            .filter_map(|peer| {
                Some((
                    peer,
                    WireRequest {
                        incarnation: *incarnations.get(&peer)?,
                        body: WireBody::Broadcast {
                            servers: servers.cloned(),
                            action: action.clone(),
                        },
                    },
                ))
            })
            .collect()
    }

    /// Send a broadcast to every other peer without waiting.
    fn fan_out(&self, servers: Option<&BTreeSet<String>>, action: &PlayerAction) {
        for (peer, request) in self.broadcasts(servers, action) {
            self.dispatch(peer, request);
        }
    }

    /// Queue a plugin message for the backend of a case-insensitive player name.
    pub fn forward_to_player(&self, name: &str, data: &[u8]) -> bool {
        {
            let sessions = self.lock();
            if let Some(entry) = sessions
                .values()
                .find(|entry| entry.name.eq_ignore_ascii_case(name))
            {
                return entry
                    .actions
                    .try_send(PlayerAction::Forward {
                        server: entry.server.clone(),
                        data: data.to_vec(),
                    })
                    .is_ok();
            }
        }
        self.roster.remote_by_name(name).is_some_and(|remote| {
            let action = PlayerAction::Forward {
                server: remote.player.server.clone(),
                data: data.to_vec(),
            };
            self.dispatch(remote.player.peer, Self::request(&remote, action))
        })
    }

    /// Queue a plugin message through one player on each server that `include`
    /// selects, oldest connection first, passing over full queues. Servers
    /// without players are skipped. A server with players only on other
    /// peers is reached through one of them. Returns the number of servers
    /// reached.
    pub fn forward_to_servers(&self, include: impl Fn(&str) -> bool, data: &[u8]) -> usize {
        let mut reached = BTreeSet::new();
        {
            let sessions = self.lock();
            for entry in sessions.values() {
                if include(&entry.server)
                    && !reached.contains(entry.server.as_str())
                    && entry
                        .actions
                        .try_send(PlayerAction::Forward {
                            server: entry.server.clone(),
                            data: data.to_vec(),
                        })
                        .is_ok()
                {
                    reached.insert(entry.server.clone());
                }
            }
        }
        let mut carriers: BTreeMap<String, RemotePlayer> = BTreeMap::new();
        for remote in self.roster.remote_players() {
            let server = &remote.player.server;
            if !include(server) || reached.contains(server) {
                continue;
            }
            match carriers.get(server) {
                Some(carrier) if carrier.player.connection <= remote.player.connection => {}
                _ => {
                    carriers.insert(server.clone(), remote);
                }
            }
        }
        for (server, carrier) in carriers {
            let action = PlayerAction::Forward {
                server: server.clone(),
                data: data.to_vec(),
            };
            if self.dispatch(carrier.player.peer, Self::request(&carrier, action)) {
                reached.insert(server);
            }
        }
        reached.len()
    }

    /// Whether `server` is a configured server or service group.
    pub fn known(&self, server: &str) -> Result<(), &'static str> {
        if !self
            .destinations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(server)
        {
            return Err("unknown server or service group");
        }
        Ok(())
    }

    /// The network roster, shared with sessions and every count.
    pub fn roster(&self) -> &Arc<PlayerRegistry> {
        &self.roster
    }

    /// The player's visibility rule on `server`: the node in their
    /// `rift.visible-to` meta, resolved in their current contexts once the
    /// authority has them on `server`, and in `server` and its group before
    /// (a login on its first backend). A value that is not a valid node
    /// still hides the player; nobody can hold it.
    pub fn visibility(&self, uuid: &[u8; 16], server: &str) -> Option<Arc<str>> {
        let value = self.meta_on(uuid, server).meta.remove(VISIBLE_TO)?;
        let node = rift_permissions::node::normalize_node(value.trim())
            .unwrap_or_else(|_| value.trim().to_ascii_lowercase());
        Some(Arc::from(node))
    }

    /// Put the player's permission prefix and suffix around `name` on their
    /// roster record, as tab lists show them, with the weight of their
    /// primary group as the list order. `tab_list.format` arranges them (see
    /// [`format_name`]). The text takes `&` codes and a colour carries into
    /// the name, as in a chat format. Without a format, a player with neither
    /// keeps the plain entry. Resolved like [`Players::visibility`].
    fn decorate(&self, uuid: &[u8; 16], name: &str, server: &str) {
        let meta = self.meta_on(uuid, server);
        let (prefix, suffix) = (
            meta.prefix.unwrap_or_default(),
            meta.suffix.unwrap_or_default(),
        );
        let groups = self.permissions.groups();
        let group = groups.get(&meta.primary_group);
        let format = self
            .tab_format
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let label = group
            .and_then(|group| group.display_name.as_deref())
            .unwrap_or(meta.primary_group.as_str());
        let Some(text) = format_name(format.as_deref(), &prefix, name, &suffix, label, server)
        else {
            self.roster.set_display(uuid, None, 0);
            return;
        };
        let display = crate::text::legacy::parse(&text, '&');
        let order = group.map_or(0, |group| group.weight);
        self.roster
            .set_display(uuid, Some(Arc::new(display)), order);
    }

    /// The player's meta on `server`; see [`Players::visibility`].
    fn meta_on(&self, uuid: &[u8; 16], server: &str) -> rift_permissions::MetaView {
        let player = rift_permissions::Uuid::from_bytes(*uuid);
        let contexts = if self
            .permissions
            .server_of(player)
            .is_some_and(|current| current.eq_ignore_ascii_case(server))
        {
            self.permissions.contexts(player)
        } else {
            let mut contexts =
                rift_permissions::ContextSet::of(rift_permissions::context::SERVER, server)
                    .unwrap_or_default();
            if let Some(group) = self.group_of(server) {
                let _ = contexts.insert(rift_permissions::context::GROUP, &group);
            }
            contexts
        };
        self.permissions.meta(player, &contexts)
    }

    /// Resolve one online player's rule again; publishes a `visibility`
    /// event when it changed.
    pub fn refresh(&self, uuid: &[u8; 16]) -> bool {
        let mut sessions = self.lock();
        let Some((id, entry)) = sessions.iter_mut().find(|(_, entry)| entry.key == *uuid) else {
            return false;
        };
        self.decorate(uuid, &entry.name, &entry.server);
        let visible_to = self.visibility(uuid, &entry.server);
        if visible_to == entry.visible_to {
            return false;
        }
        let previous = std::mem::replace(&mut entry.visible_to, visible_to.clone());
        self.roster.set_visibility(uuid, visible_to);
        let mut event: serde_json::Value =
            serde_json::from_str(&self.event("visibility", *id, entry, None)).unwrap_or_default();
        event["previous_visible_to"] = previous.as_deref().into();
        drop(sessions);
        self.publish(event.to_string());
        true
    }

    /// Follow permission changes until the authority goes away: a changed
    /// player gets their rule resolved again, and every session rebuilds its
    /// view, because the change may be to a viewer's own nodes. Expired
    /// nodes take effect when the sweeper commits their removal.
    pub async fn watch(self) {
        use tokio::sync::broadcast::error::RecvError;
        let mut changes = self.permissions.subscribe();
        loop {
            match changes.recv().await {
                Ok(crate::permissions::Notification::Committed { subjects, .. }) => {
                    let mut online = false;
                    for subject in &subjects {
                        let uuid = *subject.uuid.as_bytes();
                        online |= self.lock().values().any(|entry| entry.key == uuid);
                        self.refresh(&uuid);
                    }
                    if online {
                        self.roster.touch();
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {
                    let keys: Vec<_> = self.lock().values().map(|entry| entry.key).collect();
                    for key in keys {
                        self.refresh(&key);
                    }
                    self.roster.touch();
                }
                Err(RecvError::Closed) => return,
            }
        }
    }

    /// Names of joined players `viewer` may see, for name completion.
    pub fn names_for(&self, viewer: Viewer<'_>) -> Vec<String> {
        let mut names: Vec<String> = self
            .lock()
            .values()
            .filter(|entry| viewer.sees(&entry.key, entry.visible_to.as_deref()))
            .map(|entry| entry.name.clone())
            .collect();
        names.extend(
            self.roster
                .remote_players()
                .into_iter()
                .filter(|remote| {
                    viewer.sees(&remote.player.uuid, remote.player.visible_to.as_deref())
                })
                .map(|remote| remote.player.name),
        );
        names
    }

    /// The service group `server` is an instance of, for the `group`
    /// permission context.
    pub fn group_of(&self, server: &str) -> Option<String> {
        self.groups
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .find(|(_, members)| members.contains(server))
            .map(|(group, _)| group.clone())
    }

    /// The servers a filter names: a server and the instances of the service
    /// group of that name; `None` matches everyone.
    fn selection(&self, server: Option<&str>) -> Result<Option<BTreeSet<String>>, &'static str> {
        let Some(server) = server else {
            return Ok(None);
        };
        self.known(server)?;
        let mut servers = self
            .groups
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(server)
            .cloned()
            .unwrap_or_default();
        servers.insert(server.to_owned());
        Ok(Some(servers))
    }

    fn player_info(
        &self,
        entry: &Entry,
        id: u64,
        peer: Option<&str>,
    ) -> crate::plugins::api::PlayerInfo {
        crate::plugins::api::PlayerInfo {
            connection_id: id,
            uuid: entry.uuid.clone(),
            name: entry.name.clone(),
            server: entry.server.clone(),
            protocol: entry.protocol,
            visible_to: entry.visible_to.as_deref().map(str::to_owned),
            locale: entry.locale.clone(),
            ip: entry.ip.clone(),
            properties: entry.properties.clone(),
            language: Self::effective_language(entry),
            peer: peer.map(str::to_owned),
        }
    }

    fn remote_info(player: crate::players::Player) -> crate::plugins::api::PlayerInfo {
        crate::plugins::api::PlayerInfo {
            connection_id: player.connection,
            uuid: hex(&player.uuid),
            peer: Some(crate::players::identity::peer_label(player.peer)),
            name: player.name,
            server: player.server,
            protocol: player.protocol,
            visible_to: player.visible_to.as_deref().map(str::to_owned),
            locale: player.locale.as_deref().map(str::to_owned),
            ip: player.ip.to_string(),
            properties: player
                .properties
                .iter()
                .map(|property| crate::plugins::api::ProfileProperty {
                    name: property.name.clone(),
                    value: property.value.clone(),
                    signature: property.signature.clone(),
                })
                .collect(),
            language: player.language.as_deref().map(str::to_owned),
        }
    }

    /// This peer's name in listings; `None` without a cluster, which keeps
    /// listings as they always were.
    fn own_label(&self) -> Option<String> {
        let identity = self.roster.identity();
        identity
            .is_clustered()
            .then(|| crate::players::identity::peer_label(identity.peer))
    }

    /// Joined players, optionally only those on one server or service group,
    /// on every peer.
    pub fn list(
        &self,
        server: Option<&str>,
    ) -> Result<Vec<crate::plugins::api::PlayerInfo>, String> {
        let servers = self.selection(server)?;
        let matches = |name: &str| {
            servers
                .as_ref()
                .is_none_or(|servers| servers.contains(name))
        };
        let own = self.own_label();
        let mut list: Vec<_> = self
            .lock()
            .iter()
            .filter(|(_, entry)| matches(&entry.server))
            .map(|(id, entry)| self.player_info(entry, *id, own.as_deref()))
            .collect();
        list.extend(
            self.roster
                .remote_players()
                .into_iter()
                .filter(|remote| matches(&remote.player.server))
                .map(|remote| Self::remote_info(remote.player)),
        );
        Ok(list)
    }

    /// One player by lowercase UUID or case-insensitive name, on any peer.
    pub fn get(&self, target: &str) -> Result<Option<crate::plugins::api::PlayerInfo>, String> {
        let own = self.own_label();
        {
            let sessions = self.lock();
            if let Some((id, entry)) = find(&sessions, target)? {
                return Ok(Some(self.player_info(entry, *id, own.as_deref())));
            }
        }
        Ok(self
            .remote(target)
            .map(|remote| Self::remote_info(remote.player)))
    }

    /// Names starting with `prefix` (ignoring ASCII case), sorted, on every
    /// peer.
    pub fn names_with_prefix(&self, prefix: &str) -> Vec<String> {
        let starts = |name: &str| {
            name.get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        };
        let mut names: Vec<_> = self
            .lock()
            .values()
            .filter(|entry| starts(&entry.name))
            .map(|entry| entry.name.clone())
            .collect();
        names.extend(
            self.roster
                .remote_players()
                .into_iter()
                .filter(|remote| starts(&remote.player.name))
                .map(|remote| remote.player.name),
        );
        names.sort_by_key(|name| name.to_ascii_lowercase());
        names
    }

    /// Move a player to a configured server or service group.
    pub fn transfer(&self, target: &str, server: &str) -> Result<bool, String> {
        self.known(server)?;
        self.send(target, PlayerAction::Transfer(server.to_owned()))
            .map_err(|error| error.to_string())
    }

    /// Queue a chat message for every player on `server` (everyone for
    /// `None`) and count those of this peer that accepted it; every other
    /// peer delivers it to its own players. Text that names a message is
    /// rendered once per language among the recipients, and each player
    /// gets theirs.
    pub fn broadcast(&self, message: &Unrendered, server: Option<&str>) -> Result<usize, String> {
        let servers = self.selection(server)?;
        if !message.varies() {
            let action = PlayerAction::Message(message.render(None)?);
            let count = self.broadcast_local(servers.as_ref(), &action);
            self.fan_out(servers.as_ref(), &action);
            return Ok(count);
        }
        let mut rendered = BTreeMap::new();
        let count = self.broadcast_rendered(servers.as_ref(), message, &mut rendered)?;
        for (remote, text) in self.remote_renders(servers.as_ref(), message, &mut rendered)? {
            self.dispatch(
                remote.player.peer,
                Self::request(&remote, PlayerAction::Message(text)),
            );
        }
        Ok(count)
    }

    /// [`Players::broadcast`] that waits for every peer and counts the
    /// players of the whole network that accepted it.
    pub async fn broadcast_routed(
        &self,
        message: &Unrendered,
        server: Option<&str>,
    ) -> Result<usize, String> {
        let servers = self.selection(server)?;
        let router = self.router();
        if !message.varies() {
            let action = PlayerAction::Message(message.render(None)?);
            let mut count = self.broadcast_local(servers.as_ref(), &action);
            let deliveries: Vec<_> = self
                .broadcasts(servers.as_ref(), &action)
                .into_iter()
                .map(|(peer, request)| router.deliver(peer, request))
                .collect();
            for delivery in deliveries {
                match delivery.await {
                    Ok(WireResult::Count(delivered)) => count += delivered,
                    Ok(_) => {}
                    Err(error) => log_warn!("broadcast not delivered: {error}"),
                }
            }
            return Ok(count);
        }
        let mut rendered = BTreeMap::new();
        let mut count = self.broadcast_rendered(servers.as_ref(), message, &mut rendered)?;
        // A peer delivers one action to all of its players, so text that
        // differs by language goes to each of its players on its own.
        let deliveries: Vec<_> = self
            .remote_renders(servers.as_ref(), message, &mut rendered)?
            .into_iter()
            .map(|(remote, text)| {
                router.deliver(
                    remote.player.peer,
                    Self::request(&remote, PlayerAction::Message(text)),
                )
            })
            .collect();
        for delivery in deliveries {
            match delivery.await {
                Ok(WireResult::Sent(true)) => count += 1,
                Ok(_) => {}
                Err(error) => log_warn!("broadcast not delivered: {error}"),
            }
        }
        Ok(count)
    }

    /// `message` in `language`, rendered once per language in `rendered`.
    /// The call was checked, so only size can fail a language; that
    /// language gets the default text.
    fn render_in(
        message: &Unrendered,
        language: Option<String>,
        rendered: &mut BTreeMap<Option<String>, Text>,
    ) -> Result<Text, String> {
        if let Some(text) = rendered.get(&language) {
            return Ok(text.clone());
        }
        let text = message
            .render(language.as_deref())
            .or_else(|_| message.render(None))?;
        rendered.insert(language, text.clone());
        Ok(text)
    }

    /// Queue `message` for this peer's players on `servers`, each in their
    /// language, and count those that accepted it.
    fn broadcast_rendered(
        &self,
        servers: Option<&BTreeSet<String>>,
        message: &Unrendered,
        rendered: &mut BTreeMap<Option<String>, Text>,
    ) -> Result<usize, String> {
        // Rendered outside the lock: it reads the catalogue.
        let recipients: Vec<_> = self
            .lock()
            .values()
            .filter(|entry| servers.is_none_or(|servers| servers.contains(&entry.server)))
            .map(|entry| (entry.actions.clone(), Self::effective_language(entry)))
            .collect();
        let mut reached = 0;
        for (actions, language) in recipients {
            let text = Self::render_in(message, language, rendered)?;
            if actions.try_send(PlayerAction::Message(text)).is_ok() {
                reached += 1;
            }
        }
        Ok(reached)
    }

    /// `message` for each player of another peer on `servers`, in the
    /// language that peer reported for them.
    fn remote_renders(
        &self,
        servers: Option<&BTreeSet<String>>,
        message: &Unrendered,
        rendered: &mut BTreeMap<Option<String>, Text>,
    ) -> Result<Vec<(RemotePlayer, Text)>, String> {
        self.roster
            .remote_players()
            .into_iter()
            .filter(|remote| servers.is_none_or(|servers| servers.contains(&remote.player.server)))
            .map(|remote| {
                let language = remote.player.language.as_deref().map(str::to_owned);
                Self::render_in(message, language, rendered).map(|text| (remote, text))
            })
            .collect()
    }

    /// Show, change or remove one boss bar, tracking the per-player limit.
    /// False means that the player is offline, too old for boss bars, or has
    /// a full queue. The peer of a player on another peer checks the limit.
    pub fn boss_bar(
        &self,
        target: &str,
        name: &str,
        bar: Option<crate::protocol::BossBar>,
    ) -> Result<bool, &'static str> {
        if !super::token(name) {
            return Err("boss bar name must be 1..=64 lowercase letters, digits, '.', '_' or '-'");
        }
        {
            let mut sessions = self.lock();
            if let Some(id) = find(&sessions, target)?.map(|(id, _)| *id) {
                let entry = sessions.get_mut(&id).unwrap();
                return queue_boss_bar(entry, name, bar);
            }
        }
        let Some(remote) = self.remote(target) else {
            return Ok(false);
        };
        if !crate::protocol::boss_bars_supported(remote.player.protocol) {
            return Ok(false);
        }
        let action = PlayerAction::BossBar {
            name: name.to_owned(),
            bar,
        };
        Ok(self.dispatch(remote.player.peer, Self::request(&remote, action)))
    }

    /// Show a dialog (`Some`) or close the one Rift showed (`None`). False
    /// means that the player is offline, older than 1.21.6, or has a full
    /// queue. A dialog for a player on another peer brings its clicks back
    /// here.
    pub fn dialog(
        &self,
        target: &str,
        dialog: Option<crate::dialog::Dialog>,
        owner: crate::dialog::Owner,
    ) -> std::io::Result<bool> {
        let action = |owner| match dialog {
            Some(dialog) => PlayerAction::Dialog {
                dialog: Arc::new(dialog),
                owner,
            },
            None => PlayerAction::ClearDialog,
        };
        {
            let sessions = self.lock();
            let entry = find(&sessions, target).map_err(|message| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
            })?;
            if let Some((_, entry)) = entry {
                if !crate::protocol::dialogs_supported(entry.protocol) {
                    return Ok(false);
                }
                return Ok(entry.actions.try_send(action(owner)).is_ok());
            }
        }
        let Some(remote) = self.remote(target) else {
            return Ok(false);
        };
        if !crate::protocol::dialogs_supported(remote.player.protocol) {
            return Ok(false);
        }
        let owner = self.remote_owner(owner);
        Ok(self.dispatch(remote.player.peer, Self::request(&remote, action(owner))))
    }

    /// Who showed a dialog or a menu, as the peer of a player of another
    /// peer names it: this peer, so that the events come back here.
    fn remote_owner(&self, owner: crate::dialog::Owner) -> crate::dialog::Owner {
        let identity = self.roster.identity();
        match owner {
            crate::dialog::Owner::Script => crate::dialog::Owner::Remote {
                peer: identity.peer,
                incarnation: identity.incarnation,
                plugin: None,
            },
            crate::dialog::Owner::Plugin(plugin) => crate::dialog::Owner::Remote {
                peer: identity.peer,
                incarnation: identity.incarnation,
                plugin: Some(plugin),
            },
            forwarded @ crate::dialog::Owner::Remote { .. } => forwarded,
        }
    }

    /// Show a menu (`Some`) or close the one Rift shows (`None`, only the
    /// menu `id` of `owner` when an `id` is given). False means that the
    /// player is offline, older than 1.12, or has a full queue. A menu for
    /// a player on another peer brings its events back here.
    pub fn menu(
        &self,
        target: &str,
        menu: Option<crate::menu::Menu>,
        owner: crate::dialog::Owner,
        id: Option<String>,
    ) -> std::io::Result<bool> {
        self.menu_action(target, owner, |sessions, owner| match menu {
            Some(menu) => PlayerAction::Menu(crate::menu::Shown {
                heads: Arc::new(heads(sessions, menu.slots().iter().flatten())),
                menu: Arc::new(menu),
                owner,
            }),
            None => PlayerAction::CloseMenu { owner, id },
        })
    }

    /// Change part of the menu `id` that `owner` showed the player, if it is
    /// still open when the session takes the action. Returns as
    /// [`Self::menu`] does.
    pub fn update_menu(
        &self,
        target: &str,
        id: &str,
        patch: crate::menu::Patch,
        owner: crate::dialog::Owner,
    ) -> std::io::Result<bool> {
        self.menu_action(target, owner, |sessions, owner| {
            PlayerAction::UpdateMenu(crate::menu::Update {
                heads: Arc::new(heads(sessions, patch.buttons())),
                patch: Arc::new(patch),
                id: id.to_owned(),
                owner,
            })
        })
    }

    /// Queue the menu action `action` builds for `target`, on this peer or
    /// another one.
    fn menu_action(
        &self,
        target: &str,
        owner: crate::dialog::Owner,
        action: impl FnOnce(&BTreeMap<u64, Entry>, crate::dialog::Owner) -> PlayerAction,
    ) -> std::io::Result<bool> {
        let sessions = self.lock();
        let entry = find(&sessions, target)
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        if let Some((_, entry)) = entry {
            if !crate::protocol::menus_supported(entry.protocol) {
                return Ok(false);
            }
            return Ok(entry.actions.try_send(action(&sessions, owner)).is_ok());
        }
        let Some(remote) = self.remote(target) else {
            return Ok(false);
        };
        if !crate::protocol::menus_supported(remote.player.protocol) {
            return Ok(false);
        }
        let action = action(&sessions, self.remote_owner(owner));
        drop(sessions);
        Ok(self.dispatch(remote.player.peer, Self::request(&remote, action)))
    }

    /// Send a click on a dialog that peer `peer` showed back to it.
    pub async fn route_dialog_click(
        &self,
        peer: u16,
        incarnation: u64,
        plugin: Option<String>,
        context: super::Context,
    ) -> std::io::Result<()> {
        let request = WireRequest {
            incarnation,
            body: WireBody::DialogClick { plugin, context },
        };
        self.route_event(peer, request, "dialog click").await
    }

    /// Send an event of a menu that peer `peer` showed back to it.
    /// `context.menu` holds the event.
    pub async fn route_menu_event(
        &self,
        peer: u16,
        incarnation: u64,
        plugin: Option<String>,
        context: super::Context,
    ) -> std::io::Result<()> {
        let request = WireRequest {
            incarnation,
            body: WireBody::MenuEvent { plugin, context },
        };
        self.route_event(peer, request, "menu event").await
    }

    /// Deliver an event for the peer that showed a dialog or a menu.
    async fn route_event(
        &self,
        peer: u16,
        request: WireRequest,
        what: &str,
    ) -> std::io::Result<()> {
        match self.router().deliver(peer, request).await {
            Ok(WireResult::Sent(_)) => Ok(()),
            Ok(WireResult::Stale) => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("the peer that showed this {what} has restarted"),
            )),
            Ok(WireResult::Error(reason)) => Err(std::io::Error::other(reason)),
            Ok(other) => Err(std::io::Error::other(format!(
                "unexpected answer to a {what}: {other:?}"
            ))),
            Err(error) => Err(std::io::Error::other(error)),
        }
    }

    /// Move another peer's session `connection` to a backend through its
    /// control queue and wait for the answer, as `rift admin transfer` does
    /// for this peer's sessions.
    pub async fn route_transfer(
        &self,
        connection: u64,
        backend: &str,
        admin: bool,
    ) -> Result<serde_json::Value, String> {
        const GONE: &str = "player is no longer online; refresh status";
        let Some(remote) = self.roster.remote_by_connection(connection) else {
            return Err(GONE.into());
        };
        let request = WireRequest {
            incarnation: remote.incarnation,
            body: WireBody::Transfer {
                connection,
                backend: backend.to_owned(),
                admin,
            },
        };
        match self.router().deliver(remote.player.peer, request).await {
            Ok(WireResult::Transfer(result)) => result,
            Ok(WireResult::Stale) => Err(GONE.into()),
            Ok(WireResult::Error(reason)) => Err(reason),
            Ok(other) => Err(format!("unexpected answer to a transfer: {other:?}")),
            Err(error) => Err(format!("the player's proxy is unreachable: {error}")),
        }
    }

    /// Move another peer's player, by case-insensitive name, to `backend`
    /// through their session's control queue, as BungeeCord `ConnectOther`
    /// does for this peer's players. Does not wait; false when nobody of
    /// that name is on another peer.
    pub fn transfer_remote(&self, name: &str, backend: &str) -> bool {
        let Some(remote) = self.roster.remote_by_name(name) else {
            return false;
        };
        let request = WireRequest {
            incarnation: remote.incarnation,
            body: WireBody::Transfer {
                connection: remote.player.connection,
                backend: backend.to_owned(),
                admin: false,
            },
        };
        self.dispatch(remote.player.peer, request)
    }

    /// Run a request another peer routed here. A request for another
    /// incarnation of this peer, or for a connection id this incarnation did
    /// not mint, is refused: it is meant for a session that is gone.
    pub async fn handle_remote(&self, request: WireRequest) -> WireResult {
        let identity = self.roster.identity();
        if request.incarnation != identity.incarnation {
            return WireResult::Stale;
        }
        match request.body {
            WireBody::Action {
                connection,
                uuid,
                action,
            } => {
                if !identity.owns(connection) {
                    return WireResult::Stale;
                }
                self.act(connection, &uuid, action)
            }
            WireBody::Broadcast { servers, action } => {
                WireResult::Count(self.broadcast_local(servers.as_ref(), &action))
            }
            WireBody::Transfer {
                connection,
                backend,
                admin,
            } => {
                if !identity.owns(connection) {
                    return WireResult::Stale;
                }
                match self.hooks() {
                    Some(hooks) => {
                        WireResult::Transfer(hooks.transfer(connection, backend, admin).await)
                    }
                    None => WireResult::Error("this peer runs no sessions".into()),
                }
            }
            WireBody::DialogClick { plugin, context } => {
                let owner = match plugin {
                    Some(plugin) => crate::dialog::Owner::Plugin(plugin),
                    None => crate::dialog::Owner::Script,
                };
                match self.hooks() {
                    Some(hooks) => match hooks.dialog(context, owner).await {
                        Ok(()) => WireResult::Sent(true),
                        Err(error) => WireResult::Error(error.to_string()),
                    },
                    None => WireResult::Error("this peer runs no extensions".into()),
                }
            }
            WireBody::MenuEvent {
                plugin,
                mut context,
            } => {
                if let Some(event) = context.menu.as_mut() {
                    let mut counts = self
                        .remote_menu_events
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    // Players with no event waiting are forgotten.
                    counts.retain(|_, count| Arc::strong_count(count) > 1);
                    let count = counts.entry(context.uuid.clone()).or_default();
                    // As in the player's session: an ending always goes.
                    if event.kind != crate::menu::EventKind::Close
                        && count.load(std::sync::atomic::Ordering::Relaxed)
                            >= crate::menu::MAX_PENDING_EVENTS
                    {
                        return WireResult::Sent(false);
                    }
                    event.ticket = crate::menu::Ticket::new(count);
                }
                let owner = match plugin {
                    Some(plugin) => crate::dialog::Owner::Plugin(plugin),
                    None => crate::dialog::Owner::Script,
                };
                match self.hooks() {
                    Some(hooks) => match hooks.menu(context, owner).await {
                        Ok(()) => WireResult::Sent(true),
                        Err(error) => WireResult::Error(error.to_string()),
                    },
                    None => WireResult::Error("this peer runs no extensions".into()),
                }
            }
            WireBody::Contexts {
                uuid,
                server,
                contexts,
            } => {
                let contexts = match rift_permissions::ContextSet::from_pairs(contexts) {
                    Ok(contexts) => contexts,
                    Err(error) => return WireResult::Error(error.to_string()),
                };
                match self.permissions.report_contexts(
                    rift_permissions::Uuid::from_bytes(uuid),
                    &server,
                    contexts,
                ) {
                    Ok(accepted) => WireResult::Sent(accepted),
                    Err(error) => WireResult::Error(error.to_string()),
                }
            }
        }
    }

    /// The peer holding player `uuid` and the incarnation to address, when
    /// it is another peer: from the player's attributes, else from the
    /// claims (a login whose attributes have not arrived yet).
    fn owner(&self, uuid: &[u8; 16]) -> Option<(u16, u64)> {
        if let Some(remote) = self.roster.remote_by_uuid(uuid) {
            return Some((remote.player.peer, remote.incarnation));
        }
        let own = self.roster.identity();
        if !own.is_clustered() {
            return None;
        }
        Claims::live(&*self.roster.claims())
            .into_iter()
            .find(|record| record.uuid == Some(*uuid) && record.holder.peer != own.peer)
            .map(|record| (record.holder.peer, record.holder.incarnation))
    }

    /// The backend of player `uuid` when another peer holds them.
    pub fn remote_server(&self, uuid: &[u8; 16]) -> Option<String> {
        self.roster
            .remote_by_uuid(uuid)
            .map(|remote| remote.player.server)
    }

    /// Hand permission contexts that backend `server` reported to the peer
    /// holding player `uuid`, and wait for its answer: whether it accepted
    /// them (the player is on `server` there). `None` when no other peer
    /// holds the player.
    pub async fn route_contexts(
        &self,
        uuid: [u8; 16],
        server: &str,
        contexts: &rift_permissions::ContextSet,
    ) -> Option<Result<bool, String>> {
        let (peer, incarnation) = self.owner(&uuid)?;
        let request = WireRequest {
            incarnation,
            body: WireBody::Contexts {
                uuid,
                server: server.to_owned(),
                contexts: contexts
                    .iter()
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect(),
            },
        };
        Some(match self.router().deliver(peer, request).await {
            Ok(WireResult::Sent(accepted)) => Ok(accepted),
            Ok(WireResult::Stale) => Ok(false),
            Ok(WireResult::Error(reason)) => Err(reason),
            Ok(other) => Err(format!("unexpected answer to a context report: {other:?}")),
            Err(error) => Err(error.to_string()),
        })
    }

    /// [`Players::route_contexts`] for a blocking worker thread of the
    /// runtime, such as the permission bus's request handlers. `None` as
    /// well when called outside a runtime.
    pub fn route_contexts_blocking(
        &self,
        uuid: [u8; 16],
        server: &str,
        contexts: &rift_permissions::ContextSet,
    ) -> Option<Result<bool, String>> {
        self.owner(&uuid)?;
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        runtime.block_on(self.route_contexts(uuid, server, contexts))
    }

    /// Queue `action` for this peer's session `connection` of player `uuid`.
    fn act(&self, connection: u64, uuid: &[u8; 16], action: PlayerAction) -> WireResult {
        let mut sessions = self.lock();
        let Some(entry) = sessions
            .get_mut(&connection)
            .filter(|entry| entry.key == *uuid)
        else {
            return WireResult::Sent(false);
        };
        match action {
            PlayerAction::BossBar { name, bar } => match queue_boss_bar(entry, &name, bar) {
                Ok(sent) => WireResult::Sent(sent),
                Err(reason) => WireResult::Error(reason.into()),
            },
            PlayerAction::Dialog { .. } | PlayerAction::ClearDialog
                if !crate::protocol::dialogs_supported(entry.protocol) =>
            {
                WireResult::Sent(false)
            }
            PlayerAction::Menu(_)
            | PlayerAction::UpdateMenu(_)
            | PlayerAction::CloseMenu { .. }
                if !crate::protocol::menus_supported(entry.protocol) =>
            {
                WireResult::Sent(false)
            }
            // Heads of this peer's players get their skins here.
            PlayerAction::Menu(mut shown) => {
                let mut found = heads(&sessions, shown.menu.slots().iter().flatten());
                found.extend(shown.heads.iter().map(|(k, v)| (k.clone(), v.clone())));
                shown.heads = Arc::new(found);
                let entry = sessions.get(&connection);
                WireResult::Sent(
                    entry.is_some_and(|entry| {
                        entry.actions.try_send(PlayerAction::Menu(shown)).is_ok()
                    }),
                )
            }
            PlayerAction::UpdateMenu(mut update) => {
                let mut found = heads(&sessions, update.patch.buttons());
                found.extend(update.heads.iter().map(|(k, v)| (k.clone(), v.clone())));
                update.heads = Arc::new(found);
                let entry = sessions.get(&connection);
                WireResult::Sent(entry.is_some_and(|entry| {
                    entry
                        .actions
                        .try_send(PlayerAction::UpdateMenu(update))
                        .is_ok()
                }))
            }
            action => WireResult::Sent(entry.actions.try_send(action).is_ok()),
        }
    }

    /// Queue a kick for this peer's player `uuid`; whether a session took it.
    fn kick(&self, uuid: &[u8; 16], reason: Text) -> bool {
        self.lock()
            .values()
            .find(|entry| entry.key == *uuid)
            .is_some_and(|entry| entry.actions.try_send(PlayerAction::Kick(reason)).is_ok())
    }

    /// Disconnect every player of this peer with `message`: a peer that
    /// stayed cut off from the cluster for `cluster.partition_grace` lets its
    /// players log in again through a healthy peer. Returns how many
    /// sessions took the kick.
    pub async fn disconnect_all(&self, message: Text) -> usize {
        let sessions: Vec<_> = self
            .lock()
            .values()
            .map(|entry| entry.actions.clone())
            .collect();
        let mut kicked = 0;
        for actions in sessions {
            let kick = actions.send(PlayerAction::Kick(message.clone()));
            if matches!(
                tokio::time::timeout(DISCONNECT_WAIT, kick).await,
                Ok(Ok(()))
            ) {
                kicked += 1;
            }
        }
        kicked
    }

    /// How many players are connected to this process.
    pub fn local_sessions(&self) -> usize {
        self.lock().len()
    }

    /// Send every player of this peer elsewhere as the peer drains: clients
    /// that know the Transfer packet (1.20.5+) go to `targets` (`host`,
    /// `port`) in turn, so they spread over the other peers; the others, and
    /// everyone when there is no target, are disconnected with `message`.
    /// A session whose client turns out not to be transferable when the
    /// action arrives is disconnected with `message` as well.
    pub async fn hand_off(&self, targets: &[(String, u16)], message: Text) -> HandOff {
        let sessions: Vec<_> = self
            .lock()
            .values()
            .map(|entry| (entry.protocol, entry.actions.clone()))
            .collect();
        let mut done = HandOff::default();
        let mut next = 0;
        for (protocol, actions) in sessions {
            let transferable = crate::protocol::ProtocolVersion::new(protocol)
                .is_ok_and(|version| version.has_transfer());
            let action = match targets.get(next % targets.len().max(1)) {
                Some((host, port)) if transferable => {
                    next += 1;
                    PlayerAction::Handoff {
                        host: host.clone(),
                        port: *port,
                        fallback: message.clone(),
                    }
                }
                _ => PlayerAction::Kick(message.clone()),
            };
            let transfer = matches!(action, PlayerAction::Handoff { .. });
            if matches!(
                tokio::time::timeout(DISCONNECT_WAIT, actions.send(action)).await,
                Ok(Ok(()))
            ) {
                if transfer {
                    done.transferred += 1;
                } else {
                    done.disconnected += 1;
                }
            }
        }
        done
    }

    /// Claim this peer's players again after it rejoined the cluster. A
    /// player who logged in elsewhere meanwhile is disconnected here.
    /// Returns how many sessions were disconnected.
    pub async fn reclaim(&self) -> usize {
        let claims = self.roster.claims();
        let mut disconnected = 0;
        for record in self.roster.local_claims() {
            let Some(uuid) = record.uuid else {
                continue;
            };
            let claimed = Claims::propose(
                &*claims,
                ClaimOp::Reclaim {
                    key: record.key,
                    name: record.name,
                    uuid,
                    holder: record.holder,
                },
            );
            match claimed.await {
                Err(Refusal::AlreadyConnected | Refusal::DuplicateName) => {
                    if self.roster.lose(&uuid)
                        && self.kick(&uuid, catalog::notice(Notice::LoginAlreadyConnected))
                    {
                        disconnected += 1;
                    }
                }
                Err(refusal) => log_warn!("reclaiming a player failed: {refusal}"),
                Ok(()) => {}
            }
        }
        disconnected
    }

    /// Follow one change of the claims: another peer's player whose claim
    /// was released leaves the roster, and a player of this peer whose claim
    /// somebody else now holds is disconnected. Returns how many sessions
    /// were disconnected.
    pub fn apply_claim_change(&self, change: &ClaimChange) -> usize {
        self.roster
            .apply_claim_change(change)
            .iter()
            .filter(|uuid| self.kick(uuid, catalog::notice(Notice::LoginAlreadyConnected)))
            .count()
    }

    /// Compare the roster with every live claim, after missing changes.
    pub fn reconcile_claims(&self) -> usize {
        let live = Claims::live(&*self.roster.claims());
        self.roster
            .reconcile(&live)
            .iter()
            .filter(|uuid| self.kick(uuid, catalog::notice(Notice::LoginAlreadyConnected)))
            .count()
    }

    /// Follow the claims until they go away; see
    /// [`Players::apply_claim_change`]. Install the claims first.
    pub async fn follow_claims(self) {
        use tokio::sync::broadcast::error::RecvError;
        let mut changes = Claims::subscribe(&*self.roster.claims());
        loop {
            match changes.recv().await {
                Ok(change) => {
                    self.apply_claim_change(&change);
                }
                Err(RecvError::Lagged(_)) => {
                    self.reconcile_claims();
                }
                Err(RecvError::Closed) => return,
            }
        }
    }

    /// The table Lua sees for a player.
    fn lua_info(lua: &Lua, info: &crate::plugins::api::PlayerInfo) -> mlua::Result<Table> {
        let player = lua.create_table()?;
        player.raw_set("uuid", info.uuid.as_str())?;
        player.raw_set("name", info.name.as_str())?;
        player.raw_set("server", info.server.as_str())?;
        player.raw_set("connection_id", info.connection_id.to_string())?;
        player.raw_set("protocol", info.protocol)?;
        player.raw_set("visible_to", info.visible_to.as_deref())?;
        player.raw_set("locale", info.locale.as_deref())?;
        player.raw_set("language", info.language.as_deref())?;
        player.raw_set("ip", info.ip.as_str())?;
        let properties = lua.create_table_with_capacity(info.properties.len(), 0)?;
        for property in &info.properties {
            let item = lua.create_table()?;
            item.raw_set("name", property.name.as_str())?;
            item.raw_set("value", property.value.as_str())?;
            item.raw_set("signature", property.signature.as_deref())?;
            properties.raw_push(item)?;
        }
        player.raw_set("properties", properties)?;
        if let Some(peer) = &info.peer {
            player.raw_set("peer", peer.as_str())?;
        }
        Ok(player)
    }

    pub(crate) fn install(
        &self,
        lua: &Lua,
        runtime: &Table,
        deadline: Instant,
    ) -> mlua::Result<()> {
        let api = lua.create_table()?;
        let calls = Rc::new(Cell::new(0));
        let budget = move || {
            crate::script::check_deadline(deadline)?;
            calls.set(calls.get() + 1);
            if calls.get() > MAX_CALLS {
                return Err(error("player action budget exceeded (256 calls)"));
            }
            Ok(())
        };

        let players = self.clone();
        api.raw_set(
            "list",
            lua.create_function(move |lua, server: Option<String>| {
                crate::script::check_deadline(deadline)?;
                let list = lua.create_table()?;
                for info in players
                    .list(server.as_deref())
                    .map_err(|message| error(&message))?
                {
                    list.raw_push(Self::lua_info(lua, &info)?)?;
                }
                Ok(list)
            })?,
        )?;
        let players = self.clone();
        api.raw_set(
            "get",
            lua.create_function(move |lua, target: String| {
                crate::script::check_deadline(deadline)?;
                players
                    .get(&target)
                    .map_err(|message| error(&message))?
                    .map(|info| Self::lua_info(lua, &info))
                    .transpose()
            })?,
        )?;

        // The directory of everyone who ever joined, online or not.
        fn remembered(
            lua: &Lua,
            record: &crate::plugins::api::PlayerRecord,
        ) -> mlua::Result<Table> {
            let table = lua.create_table()?;
            table.raw_set("uuid", record.uuid.as_str())?;
            table.raw_set("name", record.name.as_str())?;
            table.raw_set("first_seen", record.first_seen)?;
            table.raw_set("last_seen", record.last_seen)?;
            table.raw_set("last_ip", record.last_ip.as_str())?;
            table.raw_set("last_server", record.last_server.as_str())?;
            table.raw_set("online", record.online)?;
            table.raw_set("language", record.language.as_deref())?;
            Ok(table)
        }
        let players = self.clone();
        api.raw_set(
            "lookup",
            lua.create_function(move |lua, target: String| {
                crate::script::check_deadline(deadline)?;
                players
                    .lookup(&target)
                    .map_err(|message| error(&message))?
                    .map(|record| remembered(lua, &record))
                    .transpose()
            })?,
        )?;
        let players = self.clone();
        api.raw_set(
            "search",
            lua.create_function(move |lua, (prefix, limit): (String, Option<u32>)| {
                crate::script::check_deadline(deadline)?;
                let list = lua.create_table()?;
                for record in players
                    .search(&prefix, limit.unwrap_or(20))
                    .map_err(|message| error(&message))?
                {
                    list.raw_push(remembered(lua, &record)?)?;
                }
                Ok(list)
            })?,
        )?;
        let players = self.clone();
        let spend = budget.clone();
        api.raw_set(
            "set_language",
            lua.create_function(move |_, (target, language): (String, Option<String>)| {
                spend()?;
                players
                    .set_language(&target, language.as_deref())
                    .map_err(|message| error(&message))
            })?,
        )?;

        let players = self.clone();
        api.raw_set(
            "names",
            lua.create_function(move |lua, prefix: Option<String>| {
                crate::script::check_deadline(deadline)?;
                let prefix = prefix.unwrap_or_default();
                lua.create_sequence_from(players.names_with_prefix(&prefix))
            })?,
        )?;

        let text_actions = [
            (
                "message",
                "message",
                PlayerAction::Message as fn(Text) -> PlayerAction,
            ),
            ("action_bar", "action bar text", PlayerAction::ActionBar),
            ("kick", "kick reason", PlayerAction::Kick),
        ];
        // `rift.lua` has no catalogue of its own: its messages are
        // qualified, rendered in the target's language.
        for (name, label, action) in text_actions {
            let (players, charge) = (self.clone(), budget.clone());
            api.raw_set(
                name,
                lua.create_function(move |_, (target, value): (String, Value)| {
                    charge()?;
                    let text = players
                        .rendering_for(None, &target, || Text::lua(value, label))
                        .map_err(mlua::Error::runtime)?;
                    players.send(&target, action(text)).map_err(lua_error)
                })?,
            )?;
        }
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "title",
            lua.create_function(move |_, (target, options): (String, Value)| {
                charge()?;
                let action = players.rendering_for(None, &target, || title(options))?;
                players.send(&target, action).map_err(lua_error)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "tab_list",
            lua.create_function(move |_, (target, options): (String, Option<Table>)| {
                charge()?;
                let action = players.rendering_for(None, &target, || tab_list(options))?;
                players.send(&target, action).map_err(lua_error)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "boss_bar",
            lua.create_function(
                move |_, (target, name, options): (String, String, Option<Table>)| {
                    charge()?;
                    let PlayerAction::BossBar { name, bar } =
                        players.rendering_for(None, &target, || boss_bar(name, options))?
                    else {
                        unreachable!("boss_bar builds a boss bar action");
                    };
                    players.boss_bar(&target, &name, bar).map_err(error)
                },
            )?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "dialog",
            lua.create_function(move |_, (target, spec): (String, Value)| {
                charge()?;
                let dialog = players.rendering_for(None, &target, || dialog(spec))?;
                players
                    .dialog(&target, Some(dialog), crate::dialog::Owner::Script)
                    .map_err(lua_error)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "clear_dialog",
            lua.create_function(move |_, target: String| {
                charge()?;
                players
                    .dialog(&target, None, crate::dialog::Owner::Script)
                    .map_err(lua_error)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "menu",
            lua.create_function(move |_, (target, spec): (String, Value)| {
                charge()?;
                let menu = players.rendering_for(None, &target, || menu(spec))?;
                players
                    .menu(&target, Some(menu), crate::dialog::Owner::Script, None)
                    .map_err(lua_error)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "update_menu",
            lua.create_function(move |_, (target, id, spec): (String, String, Value)| {
                charge()?;
                let patch = players.rendering_for(None, &target, || menu_patch(spec))?;
                players
                    .update_menu(&target, &id, patch, crate::dialog::Owner::Script)
                    .map_err(lua_error)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "close_menu",
            lua.create_function(move |_, (target, id): (String, Option<String>)| {
                charge()?;
                players
                    .menu(&target, None, crate::dialog::Owner::Script, id)
                    .map_err(lua_error)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "transfer",
            lua.create_function(move |_, (target, server): (String, String)| {
                charge()?;
                players.known(&server).map_err(error)?;
                players
                    .send(&target, PlayerAction::Transfer(server))
                    .map_err(lua_error)
            })?,
        )?;
        let players = self.clone();
        api.raw_set(
            "broadcast",
            lua.create_function(move |_, (message, server): (Value, Option<String>)| {
                budget()?;
                let message = Text::lua_as_written(message, "message")
                    .and_then(|message| Unrendered::new(message, None, "message"))
                    .map_err(mlua::Error::runtime)?;
                players
                    .broadcast(&message, server.as_deref())
                    .map_err(|message| error(&message))
            })?,
        )?;
        runtime.raw_set("players", api)
    }
}
