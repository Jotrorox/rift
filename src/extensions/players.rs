//! Online players and the actions Lua and the BungeeCord bridge can queue for
//! them. Each session drains its own bounded queue from its connection loop;
//! callers never touch a socket.
//!
//! Joins, server switches and quits are published on `rift.events.players`.
use super::fields;
use crate::messaging::Broker;
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

type Destinations = Arc<Mutex<BTreeSet<String>>>;
type Groups = Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>;

/// Joined players, shared by every reload generation and listener.
#[derive(Clone)]
pub struct Players {
    sessions: Arc<Mutex<BTreeMap<u64, Entry>>>,
    destinations: Destinations,
    groups: Groups,
    events: Broker,
}

pub const EVENTS: &str = "rift.events.players";

/// Presence in the directory. Dropping it removes the player and closes its queue.
pub struct PlayerHandle {
    players: Players,
    id: u64,
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
        if let Some(entry) = removed {
            let event = self.players.event("quit", self.id, &entry, None);
            self.players.publish(event);
        }
    }
}

fn error(message: &str) -> mlua::Error {
    mlua::Error::runtime(message)
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

fn title(value: Value) -> mlua::Result<PlayerAction> {
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

fn tab_list(value: Option<Table>) -> mlua::Result<PlayerAction> {
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

fn boss_bar(name: String, value: Option<Table>) -> mlua::Result<PlayerAction> {
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

/// Find a player by lowercase UUID or case-insensitive name.
fn find<'a>(
    sessions: &'a BTreeMap<u64, Entry>,
    target: &str,
) -> mlua::Result<Option<(&'a u64, &'a Entry)>> {
    let uuid = target.len() == 32
        && target
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !uuid && !crate::players::valid_username(target) {
        return Err(error(
            "player must be a lowercase UUID (32 hex digits) or a player name",
        ));
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
    pub(super) fn new(destinations: Destinations, groups: Groups, events: Broker) -> Self {
        Self {
            sessions: Arc::default(),
            destinations,
            groups,
            events,
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
        self.lock().insert(id, entry);
        self.publish(event);
        (
            PlayerHandle {
                players: self.clone(),
                id,
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
    pub fn send(&self, target: &str, action: PlayerAction) -> mlua::Result<bool> {
        let sessions = self.lock();
        Ok(find(&sessions, target)?
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

    fn known(&self, server: &str) -> mlua::Result<()> {
        if !self
            .destinations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(server)
        {
            return Err(error("unknown server or service group"));
        }
        Ok(())
    }

    /// Match players on a server or in a service group; nil matches everyone.
    fn matcher(&self, server: Option<String>) -> mlua::Result<impl Fn(&Entry) -> bool> {
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
                let matches = players.matcher(server)?;
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
                find(&sessions, &target)?
                    .map(|(id, entry)| Self::info(lua, entry, *id))
                    .transpose()
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
                    players.send(&target, action(text(value, label)?))
                })?,
            )?;
        }
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "title",
            lua.create_function(move |_, (target, options): (String, Value)| {
                charge()?;
                players.send(&target, title(options)?)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "tab_list",
            lua.create_function(move |_, (target, options): (String, Option<Table>)| {
                charge()?;
                players.send(&target, tab_list(options)?)
            })?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "boss_bar",
            lua.create_function(
                move |_, (target, name, options): (String, String, Option<Table>)| {
                    charge()?;
                    let action = boss_bar(name.clone(), options)?;
                    let mut sessions = players.lock();
                    let Some(id) = find(&sessions, &target)?.map(|(id, _)| *id) else {
                        return Ok(false);
                    };
                    let entry = sessions.get_mut(&id).unwrap();
                    if !crate::protocol::boss_bars_supported(entry.protocol) {
                        return Ok(false);
                    }
                    let shown = matches!(action, PlayerAction::BossBar { bar: Some(_), .. });
                    if shown
                        && !entry.boss_bars.contains(&name)
                        && entry.boss_bars.len() >= crate::protocol::MAX_BOSS_BARS
                    {
                        return Err(error("a player can have at most 8 boss bars"));
                    }
                    if entry.actions.try_send(action).is_err() {
                        return Ok(false);
                    }
                    if shown {
                        entry.boss_bars.insert(name);
                    } else {
                        entry.boss_bars.remove(&name);
                    }
                    Ok(true)
                },
            )?,
        )?;
        let (players, charge) = (self.clone(), budget.clone());
        api.raw_set(
            "transfer",
            lua.create_function(move |_, (target, server): (String, String)| {
                charge()?;
                players.known(&server)?;
                players.send(&target, PlayerAction::Transfer(server))
            })?,
        )?;
        let players = self.clone();
        api.raw_set(
            "broadcast",
            lua.create_function(move |_, (message, server): (String, Option<String>)| {
                budget()?;
                let message = text(message, "message")?;
                let matches = players.matcher(server)?;
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
