//! Online players and the actions Lua and the BungeeCord bridge can queue for
//! them. Each session drains its own bounded queue from its connection loop;
//! callers never touch a socket.
//!
//! Joins, server switches and quits are published on `rift.events.players`
//! and tell the permission authority who is online and on which server.
use super::fields;
use crate::{messaging::Broker, permissions::Authority};
use mlua::{Lua, Table, Value};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::mpsc;

/// Undelivered actions per player. A full queue reports failure instead of waiting.
const QUEUE: usize = 32;
const MAX_TEXT: usize = 1024;
/// Action calls per invocation; a broadcast counts once.
const MAX_CALLS: usize = 256;
const MAX_TITLE_MS: i64 = 60_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerAction {
    Message(String),
    ActionBar(String),
    /// Fade-in, stay and fade-out are ticks.
    Title {
        title: String,
        subtitle: Option<String>,
        times: [i32; 3],
    },
    Kick(String),
    Transfer(String),
    /// A JSON text component, already checked by `protocol::validate_component`.
    RawMessage(serde_json::Value),
    RawKick(serde_json::Value),
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
}

struct Entry {
    uuid: String,
    name: String,
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
    destinations: Destinations,
    groups: Groups,
    events: Broker,
    permissions: Arc<Authority>,
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
        self.players.permissions.switch_server(self.uuid, server);
        let previous = std::mem::replace(&mut entry.server, server.to_owned());
        let event = self
            .players
            .event("switch", self.id, entry, Some(&previous));
        drop(sessions);
        self.players.publish(event);
    }
}

impl Drop for PlayerHandle {
    fn drop(&mut self) {
        let removed = self.players.lock().remove(&self.id);
        self.players.permissions.leave(self.uuid);
        if let Some(entry) = removed {
            let event = self.players.event("quit", self.id, &entry, None);
            self.players.publish(event);
        }
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
                title: text(title.to_str()?.to_owned(), "title")?,
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
    let optional = |field| -> mlua::Result<Option<String>> {
        options
            .raw_get::<Option<String>>(field)?
            .map(|value| text(value, field))
            .transpose()
    };
    let (title, subtitle) = (optional("title")?, optional("subtitle")?);
    if title.is_none() && subtitle.is_none() {
        return Err(error("title requires a title or subtitle"));
    }
    Ok(PlayerAction::Title {
        title: title.unwrap_or_default(),
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

const INVALID_TARGET: &str = "player must be a lowercase UUID (32 hex digits) or a player name";

/// Find a player by lowercase UUID or case-insensitive name.
fn find<'a>(
    sessions: &'a BTreeMap<u64, Entry>,
    target: &str,
) -> Result<Option<(&'a u64, &'a Entry)>, &'static str> {
    let uuid = target.len() == 32
        && target
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
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

impl Players {
    pub(crate) fn new(
        destinations: Destinations,
        groups: Groups,
        events: Broker,
        permissions: Arc<Authority>,
    ) -> Self {
        Self {
            sessions: Arc::default(),
            destinations,
            groups,
            events,
            permissions,
        }
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
        let (actions, receiver) = mpsc::channel(QUEUE);
        let entry = Entry {
            uuid: uuid.iter().map(|b| format!("{b:02x}")).collect(),
            name: name.to_owned(),
            server: server.to_owned(),
            protocol,
            actions,
            boss_bars: BTreeSet::new(),
        };
        let event = self.event("join", id, &entry, None);
        let player = rift_permissions::Uuid::from_bytes(*uuid);
        self.permissions.join(player, name, server);
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

    /// Names of joined players by lowercase UUID.
    pub fn names(&self) -> BTreeMap<String, String> {
        self.lock()
            .values()
            .map(|entry| (entry.uuid.clone(), entry.name.clone()))
            .collect()
    }

    /// Queue an action for a lowercase UUID or case-insensitive player name.
    /// False means that the player is offline or has a full queue.
    pub fn send(&self, target: &str, action: PlayerAction) -> std::io::Result<bool> {
        let sessions = self.lock();
        Ok(find(&sessions, target)
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?
            .is_some_and(|(_, entry)| entry.actions.try_send(action).is_ok()))
    }

    /// Queue an action for a case-insensitive player name. False means that
    /// the player is offline or has a full queue.
    pub fn send_named(&self, name: &str, action: PlayerAction) -> bool {
        self.lock()
            .values()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            .is_some_and(|entry| entry.actions.try_send(action).is_ok())
    }

    /// Queue an action for every joined player and count those that accepted it.
    pub fn send_all(&self, action: &PlayerAction) -> usize {
        self.lock()
            .values()
            .filter(|entry| entry.actions.try_send(action.clone()).is_ok())
            .count()
    }

    /// Queue a plugin message for the backend of a case-insensitive player name.
    pub fn forward_to_player(&self, name: &str, data: &[u8]) -> bool {
        self.lock()
            .values()
            .find(|entry| entry.name.eq_ignore_ascii_case(name))
            .is_some_and(|entry| {
                entry
                    .actions
                    .try_send(PlayerAction::Forward {
                        server: entry.server.clone(),
                        data: data.to_vec(),
                    })
                    .is_ok()
            })
    }

    /// Queue a plugin message through one player on each server that `include`
    /// selects, oldest connection first, passing over full queues. Servers
    /// without players are skipped. Returns the number of servers reached.
    pub fn forward_to_servers(&self, include: impl Fn(&str) -> bool, data: &[u8]) -> usize {
        let sessions = self.lock();
        let mut reached = BTreeSet::new();
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
                reached.insert(entry.server.as_str());
            }
        }
        reached.len()
    }

    fn known(&self, server: &str) -> Result<(), &'static str> {
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

    /// Match players on a server or in a service group; nil matches everyone.
    fn matcher(&self, server: Option<String>) -> Result<impl Fn(&Entry) -> bool, &'static str> {
        let mut members = None;
        if let Some(server) = &server {
            self.known(server)?;
            members = self
                .groups
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(server)
                .cloned();
        }
        Ok(move |entry: &Entry| {
            server.as_ref().is_none_or(|server| {
                *server == entry.server
                    || members
                        .as_ref()
                        .is_some_and(|members| members.contains(&entry.server))
            })
        })
    }

    fn player_info(entry: &Entry, id: u64) -> crate::plugins::api::PlayerInfo {
        crate::plugins::api::PlayerInfo {
            connection_id: id,
            uuid: entry.uuid.clone(),
            name: entry.name.clone(),
            server: entry.server.clone(),
            protocol: entry.protocol,
        }
    }

    /// Joined players, optionally only those on one server or service group.
    pub fn list(
        &self,
        server: Option<&str>,
    ) -> Result<Vec<crate::plugins::api::PlayerInfo>, String> {
        let matches = self.matcher(server.map(str::to_owned))?;
        Ok(self
            .lock()
            .iter()
            .filter(|(_, entry)| matches(entry))
            .map(|(id, entry)| Self::player_info(entry, *id))
            .collect())
    }

    /// One player by lowercase UUID or case-insensitive name.
    pub fn get(&self, target: &str) -> Result<Option<crate::plugins::api::PlayerInfo>, String> {
        let sessions = self.lock();
        Ok(find(&sessions, target)?.map(|(id, entry)| Self::player_info(entry, *id)))
    }

    /// Names starting with `prefix` (ignoring ASCII case), sorted.
    pub fn names_with_prefix(&self, prefix: &str) -> Vec<String> {
        let mut names: Vec<_> = self
            .lock()
            .values()
            .filter(|entry| {
                entry
                    .name
                    .get(..prefix.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
            })
            .map(|entry| entry.name.clone())
            .collect();
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
    /// `None`) and count those that accepted it.
    pub fn broadcast(&self, message: &str, server: Option<&str>) -> Result<usize, String> {
        if message.is_empty() || message.len() > MAX_TEXT {
            return Err("message must contain 1..=1024 bytes".into());
        }
        let matches = self.matcher(server.map(str::to_owned))?;
        Ok(self
            .lock()
            .values()
            .filter(|entry| matches(entry))
            .filter(|entry| {
                entry
                    .actions
                    .try_send(PlayerAction::Message(message.to_owned()))
                    .is_ok()
            })
            .count())
    }

    /// Show, change or remove one boss bar, tracking the per-player limit.
    /// False means that the player is offline, too old for boss bars, or has
    /// a full queue.
    pub fn boss_bar(
        &self,
        target: &str,
        name: &str,
        bar: Option<crate::protocol::BossBar>,
    ) -> Result<bool, &'static str> {
        if !super::token(name) {
            return Err("boss bar name must be 1..=64 lowercase letters, digits, '.', '_' or '-'");
        }
        let mut sessions = self.lock();
        let Some(id) = find(&sessions, target)?.map(|(id, _)| *id) else {
            return Ok(false);
        };
        let entry = sessions.get_mut(&id).unwrap();
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

    /// Show a dialog (`Some`) or close the one Rift showed (`None`). False
    /// means that the player is offline, older than 1.21.6, or has a full
    /// queue.
    pub fn dialog(
        &self,
        target: &str,
        dialog: Option<crate::dialog::Dialog>,
        owner: crate::dialog::Owner,
    ) -> std::io::Result<bool> {
        let sessions = self.lock();
        let entry = find(&sessions, target)
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
        let Some((_, entry)) = entry else {
            return Ok(false);
        };
        if !crate::protocol::dialogs_supported(entry.protocol) {
            return Ok(false);
        }
        let action = match dialog {
            Some(dialog) => PlayerAction::Dialog {
                dialog: Arc::new(dialog),
                owner,
            },
            None => PlayerAction::ClearDialog,
        };
        Ok(entry.actions.try_send(action).is_ok())
    }

    fn info(lua: &Lua, entry: &Entry, id: u64) -> mlua::Result<Table> {
        let player = lua.create_table()?;
        player.raw_set("uuid", entry.uuid.as_str())?;
        player.raw_set("name", entry.name.as_str())?;
        player.raw_set("server", entry.server.as_str())?;
        player.raw_set("connection_id", id.to_string())?;
        player.raw_set("protocol", entry.protocol)?;
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
                let matches = players.matcher(server).map_err(error)?;
                let list = lua.create_table()?;
                for (id, entry) in players.lock().iter() {
                    if matches(entry) {
                        list.raw_push(Self::info(lua, entry, *id)?)?;
                    }
                }
                Ok(list)
            })?,
        )?;
        let players = self.clone();
        api.raw_set(
            "get",
            lua.create_function(move |lua, target: String| {
                crate::script::check_deadline(deadline)?;
                let sessions = players.lock();
                find(&sessions, &target)
                    .map_err(error)?
                    .map(|(id, entry)| Self::info(lua, entry, *id))
                    .transpose()
            })?,
        )?;

        let players = self.clone();
        api.raw_set(
            "names",
            lua.create_function(move |lua, prefix: Option<String>| {
                crate::script::check_deadline(deadline)?;
                let prefix = prefix.unwrap_or_default();
                let mut names: Vec<_> = players
                    .lock()
                    .values()
                    .filter(|entry| {
                        entry
                            .name
                            .get(..prefix.len())
                            .is_some_and(|start| start.eq_ignore_ascii_case(&prefix))
                    })
                    .map(|entry| entry.name.clone())
                    .collect();
                names.sort_by_key(|name| name.to_ascii_lowercase());
                lua.create_sequence_from(names)
            })?,
        )?;

        let text_actions = [
            (
                "message",
                "message",
                PlayerAction::Message as fn(String) -> PlayerAction,
            ),
            ("action_bar", "action bar text", PlayerAction::ActionBar),
            ("kick", "kick reason", PlayerAction::Kick),
        ];
        for (name, label, action) in text_actions {
            let (players, charge) = (self.clone(), budget.clone());
            api.raw_set(
                name,
                lua.create_function(move |_, (target, value): (String, String)| {
                    charge()?;
                    players
                        .send(&target, action(text(value, label)?))
                        .map_err(lua_error)
                })?,
            )?;
        }
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "title",
            lua.create_function(move |_, (target, options): (String, Value)| {
                charge()?;
                players.send(&target, title(options)?).map_err(lua_error)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "tab_list",
            lua.create_function(move |_, (target, options): (String, Option<Table>)| {
                charge()?;
                players.send(&target, tab_list(options)?).map_err(lua_error)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "boss_bar",
            lua.create_function(
                move |_, (target, name, options): (String, String, Option<Table>)| {
                    charge()?;
                    let PlayerAction::BossBar { name, bar } = boss_bar(name, options)? else {
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
                let dialog = dialog(spec)?;
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
            lua.create_function(move |_, (message, server): (String, Option<String>)| {
                budget()?;
                let message = text(message, "message")?;
                let matches = players.matcher(server).map_err(error)?;
                Ok(players
                    .lock()
                    .values()
                    .filter(|entry| matches(entry))
                    .filter(|entry| {
                        entry
                            .actions
                            .try_send(PlayerAction::Message(message.clone()))
                            .is_ok()
                    })
                    .count())
            })?,
        )?;
        runtime.raw_set("players", api)
    }
}
