//! Online players and the actions Lua can queue for them. Each session drains its
//! own bounded queue from its connection loop; callbacks never touch a socket.
use super::fields;
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
}

struct Entry {
    uuid: String,
    name: String,
    server: String,
    protocol: i32,
    actions: mpsc::Sender<PlayerAction>,
}

type Destinations = Arc<Mutex<BTreeSet<String>>>;
type Groups = Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>;

/// Joined players, shared by every reload generation and listener.
#[derive(Clone)]
pub struct Players {
    sessions: Arc<Mutex<BTreeMap<u64, Entry>>>,
    destinations: Destinations,
    groups: Groups,
}

/// Presence in the directory. Dropping it removes the player and closes its queue.
pub struct PlayerHandle {
    players: Players,
    id: u64,
}

impl PlayerHandle {
    /// Commit the server only after the player has joined it.
    pub fn set_server(&self, server: &str) {
        if let Some(entry) = self.players.lock().get_mut(&self.id) {
            server.clone_into(&mut entry.server);
        }
    }
}

impl Drop for PlayerHandle {
    fn drop(&mut self) {
        self.players.lock().remove(&self.id);
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
    pub(super) fn new(destinations: Destinations, groups: Groups) -> Self {
        Self {
            sessions: Arc::default(),
            destinations,
            groups,
        }
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
        self.lock().insert(
            id,
            Entry {
                uuid: uuid.iter().map(|b| format!("{b:02x}")).collect(),
                name: name.to_owned(),
                server: server.to_owned(),
                protocol,
                actions,
            },
        );
        (
            PlayerHandle {
                players: self.clone(),
                id,
            },
            receiver,
        )
    }

    /// Queue an action for a lowercase UUID or case-insensitive player name.
    /// False means that the player is offline or has a full queue.
    pub fn send(&self, target: &str, action: PlayerAction) -> mlua::Result<bool> {
        let sessions = self.lock();
        Ok(find(&sessions, target)?
            .is_some_and(|(_, entry)| entry.actions.try_send(action).is_ok()))
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
