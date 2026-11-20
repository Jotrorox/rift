//! What a player did in a menu, as Rift reports it to whoever showed the
//! menu: the configuration script or one persistent plugin (and, with the
//! next contract, a WebAssembly plugin). The fields are those of the `menu`
//! event in docs/menus.md.
use super::{Click, Menu, Textures};
use crate::dialog::Owner;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

/// The kind of a menu event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Click,
    /// Anvil typing (not reported yet).
    Text,
    Close,
}

impl EventKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Text => "text",
            Self::Close => "close",
        }
    }
}

/// Why a menu ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The player pressed Escape or the inventory key.
    Player,
    /// A button with `close`, or `close_menu`.
    Plugin,
    /// Rift showed another menu or a dialog.
    Replaced,
    /// The backend opened or closed a screen, or the player respawned or
    /// changed worlds.
    Server,
    /// The player is switching servers.
    Switch,
    /// The player left.
    Disconnect,
}

impl CloseReason {
    pub fn name(self) -> &'static str {
        match self {
            Self::Player => "player",
            Self::Plugin => "plugin",
            Self::Replaced => "replaced",
            Self::Server => "server",
            Self::Switch => "switch",
            Self::Disconnect => "disconnect",
        }
    }
}

/// One event of one menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MenuEvent {
    /// The menu's `id`.
    pub menu: String,
    pub kind: EventKind,
    /// `click`: the button's `id`.
    pub action: Option<String>,
    /// `click`: the slot number, from 0.
    pub slot: Option<u32>,
    /// `click`: how the player clicked.
    pub click: Option<Click>,
    /// `click` with `number_key`: 1 to 9.
    pub key: Option<u8>,
    /// `click`: the button's `payload`, as the plugin wrote it.
    pub payload: Option<Value>,
    /// Anvil menus: what the text field holds. It comes from the client.
    pub text: Option<String>,
    /// `close`: why the menu ended.
    pub reason: Option<CloseReason>,
    /// The menu's `state`, as the plugin last set it.
    pub state: Option<Value>,
    /// Counts the event among the player's undelivered ones until the plugin
    /// receives it.
    #[serde(skip)]
    pub ticket: Ticket,
}

impl MenuEvent {
    fn of(menu: &Menu, kind: EventKind) -> Self {
        Self {
            menu: menu.id().to_owned(),
            kind,
            action: None,
            slot: None,
            click: None,
            key: None,
            payload: None,
            text: None,
            reason: None,
            state: menu.state().cloned(),
            ticket: Ticket::default(),
        }
    }

    /// A reported click on `slot` of `menu`.
    pub fn click(menu: &Menu, slot: usize, click: Click, key: Option<u8>) -> Self {
        let action = menu.slots().get(slot).and_then(Option::as_ref);
        Self {
            action: action.and_then(|button| button.action.id.clone()),
            slot: u32::try_from(slot).ok(),
            click: Some(click),
            key,
            payload: action.and_then(|button| button.action.payload.clone()),
            ..Self::of(menu, EventKind::Click)
        }
    }

    /// The end of `menu`.
    pub fn close(menu: &Menu, reason: CloseReason) -> Self {
        Self {
            reason: Some(reason),
            ..Self::of(menu, EventKind::Close)
        }
    }
}

/// A menu event on its way to the plugin that showed the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub owner: Owner,
    pub event: MenuEvent,
}

/// Holds one place in a player's count of undelivered menu events, and
/// gives it back when dropped: once the plugin has the event, or when it is
/// thrown away. A copy holds no place.
#[derive(Debug, Default)]
pub struct Ticket(Option<Arc<AtomicUsize>>);

impl Ticket {
    pub fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        Self(Some(count.clone()))
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(count) = &self.0 {
            count.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Clone for Ticket {
    fn clone(&self) -> Self {
        Self(None)
    }
}

impl PartialEq for Ticket {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Ticket {}

/// A menu for one player's session (a queued player action): the menu
/// parsed in the player's language, who showed it, and the skins of the
/// online players its heads name.
#[derive(Debug, Clone)]
pub struct Shown {
    pub menu: Arc<Menu>,
    pub owner: Owner,
    pub heads: Arc<BTreeMap<String, Textures>>,
}

impl Shown {
    /// The skin `head = { player = ... }` names, as the session needs it.
    pub fn head(&self, player: &str) -> Option<Textures> {
        self.heads.get(player).cloned()
    }
}

impl PartialEq for Shown {
    fn eq(&self, other: &Self) -> bool {
        self.owner == other.owner && self.menu == other.menu && self.heads == other.heads
    }
}

// Menus hold validated numbers only, never NaN.
impl Eq for Shown {}

/// Player actions travel to other cluster peers serialized; menus are shown
/// only to players of the peer whose plugin shows them.
impl Serialize for Shown {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom(
            "menus are shown only to players of this peer",
        ))
    }
}

impl<'de> Deserialize<'de> for Shown {
    fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "menus are shown only to players of this peer",
        ))
    }
}
