//! Inventory menus: chest, hopper, dispenser and anvil screens whose slots
//! hold items that act as buttons (Minecraft 1.12 and newer). See
//! `docs/menus.md` for the format and the rules.
//!
//! [`Menu`] and [`MenuPatch`] build the JSON that [`players::show_menu`] and
//! [`players::update_menu`] send; [`Button`] is one slot. The builders check
//! nothing themselves: Rift validates the menu and fails the call with a
//! reason, as for a menu written as JSON text. Slots are numbered from 0,
//! left to right and top to bottom.
//!
//! Everything a player does in a menu arrives at [`Plugin::menu`] as a
//! [`MenuEvent`]; [`MenuEvent::kind`] sorts it.
//!
//! ```no_run
//! use rift_plugin::menu::{Button, Menu, MenuEvent, MenuKind};
//! use rift_plugin::{Action, CommandSpec, Context, Plugin, Registration, players, text};
//!
//! fn selector() -> Menu {
//!     Menu::chest("servers")
//!         .title(text::mini("<dark_gray>Choose a server"))
//!         .layout(["#########", "# S C M #", "####X####"])
//!         .key('#', Button::new("gray_stained_glass_pane").name(" "))
//!         .key('S', Button::new("grass_block")
//!             .name(text::mini("<green>Survival"))
//!             .lore(["Click to join"])
//!             .id("join")
//!             .payload("survival"))
//!         .key('C', Button::new("bricks").name(text::mini("<gold>Creative")).id("join").payload("creative"))
//!         .key('M', Button::new("bow").name(text::mini("<red>Minigames")).id("join").payload("minigames"))
//!         .key('X', Button::new("barrier").name(text::mini("<red>Close")).close())
//! }
//!
//! #[derive(Default)]
//! struct Lobby;
//!
//! impl Plugin for Lobby {
//!     fn init(&mut self, _config: &str) -> Result<Registration, String> {
//!         Ok(Registration::new().command(CommandSpec::new("servers")))
//!     }
//!
//!     fn command(&mut self, ctx: Context) -> Action {
//!         let _ = players::show_menu(&ctx.uuid, &selector());
//!         Action::Continue
//!     }
//!
//!     fn menu(&mut self, event: MenuEvent) {
//!         if let MenuKind::Click { action: "join", .. } = event.kind() {
//!             if let Ok(Some(server)) = event.payload::<String>() {
//!                 let _ = players::transfer(&event.ctx.uuid, &server);
//!             }
//!         }
//!     }
//! }
//!
//! rift_plugin::export!(Lobby);
//! ```
//!
//! [`players::show_menu`]: crate::players::show_menu
//! [`players::update_menu`]: crate::players::update_menu
//! [`Plugin::menu`]: crate::Plugin::menu
use crate::Text;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Something a player did in a menu this plugin showed: the contract's
/// `menu-event` record. Use [`MenuEvent::kind`] instead of comparing the
/// strings in `kind`, `click` and `reason`, and [`MenuEvent::payload`] and
/// [`MenuEvent::state`] to decode the JSON fields.
pub use crate::bindings::rift::plugin::types::MenuEvent;

/// A JSON object being built, with the first error any of its values had.
#[derive(Debug, Clone, Default, PartialEq)]
struct Object {
    fields: Map<String, Value>,
    error: Option<String>,
}

impl Object {
    fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.fields.insert(key.into(), value.into());
    }

    fn fail(&mut self, error: String) {
        self.error.get_or_insert(error);
    }

    fn text(&mut self, key: &str, text: impl Into<Text>) {
        match text_value(text.into()) {
            Ok(value) => self.set(key, value),
            Err(error) => self.fail(format!("{key}: {error}")),
        }
    }

    fn json(&mut self, key: &str, value: impl Serialize) {
        match serde_json::to_value(value) {
            Ok(value) => self.set(key, value),
            Err(error) => self.fail(format!("{key}: {error}")),
        }
    }

    /// Adds `entry` to the list `key`.
    fn push(&mut self, key: &str, entry: Object) {
        if let Some(error) = entry.error {
            self.fail(error);
        }
        match self.fields.get_mut(key) {
            Some(Value::Array(entries)) => entries.push(Value::Object(entry.fields)),
            _ => self.set(key, vec![Value::Object(entry.fields)]),
        }
    }

    /// Sets `name` in the object `key`.
    fn insert(&mut self, key: &str, name: String, entry: Object) {
        if let Some(error) = entry.error {
            self.fail(error);
        }
        match self.fields.get_mut(key) {
            Some(Value::Object(entries)) => {
                entries.insert(name, Value::Object(entry.fields));
            }
            _ => {
                let mut entries = Map::new();
                entries.insert(name, Value::Object(entry.fields));
                self.set(key, entries);
            }
        }
    }

    fn to_value(&self) -> Result<Value, String> {
        match &self.error {
            Some(error) => Err(error.clone()),
            None => Ok(Value::Object(self.fields.clone())),
        }
    }
}

/// A text as the menu format takes it: plain text is a JSON string, shown as
/// written; JSON text is the component itself.
fn text_value(text: Text) -> Result<Value, String> {
    match text {
        Text::Plain(text) => Ok(Value::String(text)),
        Text::Json(json) => {
            serde_json::from_str(&json).map_err(|error| format!("invalid JSON text: {error}"))
        }
    }
}

/// A button placed by slot number.
fn placed(slot: Value, button: Button) -> Object {
    let mut entry = button.0;
    entry.set(if slot.is_array() { "slots" } else { "slot" }, slot);
    entry
}

/// An emptied slot.
fn emptied(slot: u32) -> Object {
    let mut entry = Object::default();
    entry.set("slot", slot);
    entry.set("empty", true);
    entry
}

/// A menu definition. Start with [`Menu::chest`], [`Menu::hopper`],
/// [`Menu::dispenser`] or [`Menu::anvil`]; show it with
/// [`players::show_menu`](crate::players::show_menu). Showing a menu with
/// the `id` of the menu the player has open redraws it in place.
#[derive(Debug, Clone, PartialEq)]
pub struct Menu(Object);

impl Menu {
    fn of(kind: &str, id: impl Into<String>) -> Self {
        let mut menu = Object::default();
        menu.set("id", id.into());
        menu.set("type", kind);
        Self(menu)
    }

    /// A chest: 9 slots per row, 1 to 6 rows. Without [`Menu::rows`] it has
    /// as many rows as the layout has lines, else 3.
    ///
    /// `id` names the menu in every event: 1 to 64 characters of `a-z`,
    /// `0-9`, `_`, `-`, `.` and `/`.
    pub fn chest(id: impl Into<String>) -> Self {
        Self::of("chest", id)
    }

    /// A hopper: 5 slots in one row.
    pub fn hopper(id: impl Into<String>) -> Self {
        Self::of("hopper", id)
    }

    /// A dispenser: 3 rows of 3 slots.
    pub fn dispenser(id: impl Into<String>) -> Self {
        Self::of("dispenser", id)
    }

    /// An anvil, a one-line text prompt: slot 0 is the left input, 1 the
    /// right input and 2 the result. Without buttons the left slot holds a
    /// sheet of paper and the result its copy with the action `submit`.
    pub fn anvil(id: impl Into<String>) -> Self {
        Self::of("anvil", id)
    }

    /// Chest only: the number of rows, 1 to 6. With a layout it must be the
    /// number of its lines.
    pub fn rows(mut self, rows: u8) -> Self {
        self.0.set("rows", rows);
        self
    }

    /// The title above the slots.
    pub fn title(mut self, title: impl Into<Text>) -> Self {
        self.0.text("title", title);
        self
    }

    /// The slots as lines of characters, one per slot: 9 per line for a
    /// chest, 5 for a hopper, 3 for a dispenser and an anvil. A space is an
    /// empty slot; every other character needs a [`Menu::key`].
    pub fn layout<S: Into<String>>(mut self, lines: impl IntoIterator<Item = S>) -> Self {
        let lines: Vec<Value> = lines
            .into_iter()
            .map(|line| Value::String(line.into()))
            .collect();
        self.0.set("layout", lines);
        self
    }

    /// The button the layout character `character` stands for.
    pub fn key(mut self, character: char, button: Button) -> Self {
        self.0.insert("keys", character.to_string(), button.0);
        self
    }

    /// A button in slot `slot`, placed after the layout, so it replaces what
    /// the layout put there.
    pub fn slot(mut self, slot: u32, button: Button) -> Self {
        self.0.push("slots", placed(slot.into(), button));
        self
    }

    /// The same button in each of `slots`; see [`Menu::slot`].
    pub fn slots(mut self, slots: impl IntoIterator<Item = u32>, button: Button) -> Self {
        let slots: Vec<Value> = slots.into_iter().map(Value::from).collect();
        self.0.push("slots", placed(slots.into(), button));
        self
    }

    /// Empties slot `slot` after the layout.
    pub fn empty(mut self, slot: u32) -> Self {
        self.0.push("slots", emptied(slot));
        self
    }

    /// Any JSON value of at most 16 KiB that Rift keeps with the menu and
    /// hands back in every event ([`MenuEvent::state`]), such as the page a
    /// paged list shows. The client never sees it.
    pub fn state(mut self, state: impl Serialize) -> Self {
        self.0.json("state", state);
        self
    }

    /// Anvil only: what the text field starts with.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.0.set("text", text.into());
        self
    }

    /// Anvil only: report a [`MenuKind::Text`] event while the player types,
    /// at most 5 a second and always for the final value.
    pub fn typing(mut self) -> Self {
        self.0.set("typing", true);
        self
    }

    /// The menu as JSON, as `players::menu` takes it. Fails only for a text
    /// or a value that is not valid JSON.
    pub fn to_value(&self) -> Result<Value, String> {
        self.0.to_value().map_err(|error| format!("menu.{error}"))
    }

    /// The menu as JSON text; see [`Menu::to_value`].
    pub fn to_json(&self) -> Result<String, String> {
        self.to_value().map(|value| value.to_string())
    }
}

/// A change to part of an open menu, for
/// [`players::update_menu`](crate::players::update_menu): a clock, a
/// countdown, a new title. It does nothing if the player no longer has the
/// menu open.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MenuPatch(Object);

impl MenuPatch {
    /// A patch that changes nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A new button for slot `slot`.
    pub fn slot(mut self, slot: u32, button: Button) -> Self {
        self.0.push("slots", placed(slot.into(), button));
        self
    }

    /// The same new button for each of `slots`.
    pub fn slots(mut self, slots: impl IntoIterator<Item = u32>, button: Button) -> Self {
        let slots: Vec<Value> = slots.into_iter().map(Value::from).collect();
        self.0.push("slots", placed(slots.into(), button));
        self
    }

    /// Empties slot `slot`.
    pub fn empty(mut self, slot: u32) -> Self {
        self.0.push("slots", emptied(slot));
        self
    }

    /// A new button for every slot the layout gave `character`.
    pub fn key(mut self, character: char, button: Button) -> Self {
        self.0.insert("keys", character.to_string(), button.0);
        self
    }

    /// A new title.
    pub fn title(mut self, title: impl Into<Text>) -> Self {
        self.0.text("title", title);
        self
    }

    /// A new state.
    pub fn state(mut self, state: impl Serialize) -> Self {
        self.0.json("state", state);
        self
    }

    /// Anvil only: replaces what the text field holds. A redraw keeps what
    /// the player typed; only this replaces it.
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.0.set("text", text.into());
        self
    }

    /// The patch as JSON, as `players::update_menu_json` takes it.
    pub fn to_value(&self) -> Result<Value, String> {
        self.0.to_value().map_err(|error| format!("patch.{error}"))
    }

    /// The patch as JSON text; see [`MenuPatch::to_value`].
    pub fn to_json(&self) -> Result<String, String> {
        self.to_value().map(|value| value.to_string())
    }
}

/// One slot of a menu: an item with an optional action. A button with an
/// [`id`](Button::id) reports its clicks; one without is decoration.
#[derive(Debug, Clone, PartialEq)]
pub struct Button(Object);

impl Button {
    /// The item `item`, an item name of the newest Minecraft version such as
    /// `diamond_sword` (`minecraft:` is optional). A client that lacks the
    /// item gets a stand-in.
    pub fn new(item: impl Into<String>) -> Self {
        let mut button = Object::default();
        button.set("item", item.into());
        Self(button)
    }

    /// The stack size, 1 to 99; clients before 1.20.5 show at most 64.
    pub fn count(mut self, count: u8) -> Self {
        self.0.set("count", count);
        self
    }

    /// The name instead of the item's own. Not italic unless the text says
    /// so.
    pub fn name(mut self, name: impl Into<Text>) -> Self {
        self.0.text("name", name);
        self
    }

    /// Lines below the name, at most 32. Not italic unless the text says so.
    pub fn lore<T: Into<Text>>(mut self, lines: impl IntoIterator<Item = T>) -> Self {
        let mut values = Vec::new();
        for line in lines {
            match text_value(line.into()) {
                Ok(value) => values.push(value),
                Err(error) => self.0.fail(format!("lore: {error}")),
            }
        }
        self.0.set("lore", values);
        self
    }

    /// `true` adds the enchantment glint; `false` removes an item's own glint
    /// on 1.20.5 and newer.
    pub fn glint(mut self, glint: bool) -> Self {
        self.0.set("glint", glint);
        self
    }

    /// Custom model data for resource packs: a number, or [`Model`] for
    /// 1.21.4 and newer.
    pub fn model(mut self, model: impl Into<Model>) -> Self {
        self.0.set("model", model.into().to_value());
        self
    }

    /// A model ID for 1.21.2 and newer, such as `myserver:menu/back`. Older
    /// clients ignore it.
    pub fn item_model(mut self, model: impl Into<String>) -> Self {
        self.0.set("item_model", model.into());
        self
    }

    /// The skin of a `player_head`.
    pub fn head(mut self, head: Head) -> Self {
        self.0.set("head", head.to_value());
        self
    }

    /// `"#RRGGBB"` for leather armor, potions and other dyeable items.
    pub fn color(mut self, color: impl Into<String>) -> Self {
        self.0.set("color", color.into());
        self
    }

    /// `true` shows the lines the game adds on its own (attack damage,
    /// enchantments, potion effects), which are hidden by default.
    pub fn details(mut self, shown: bool) -> Self {
        self.0.set("details", shown);
        self
    }

    /// `false` hides the tooltip altogether on 1.20.5 and newer; older
    /// clients get an empty name.
    pub fn tooltip(mut self, shown: bool) -> Self {
        self.0.set("tooltip", shown);
        self
    }

    /// The action reported when the button is clicked
    /// ([`MenuKind::Click`]'s `action`): the same characters as a menu id.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.0.set("id", id.into());
        self
    }

    /// Any JSON value of at most 4 KiB, handed back with the click
    /// ([`MenuEvent::payload`]). Rift keeps it; the client never sees it.
    /// Needs an [`id`](Button::id).
    pub fn payload(mut self, payload: impl Serialize) -> Self {
        self.0.json("payload", payload);
        self
    }

    /// The click kinds that report, instead of the four mouse clicks
    /// (`left`, `right`, `shift_left`, `shift_right`). Needs an
    /// [`id`](Button::id). [`Click::Other`] is left out.
    pub fn clicks(mut self, clicks: impl IntoIterator<Item = Click>) -> Self {
        let names: Vec<Value> = clicks
            .into_iter()
            .filter(|click| *click != Click::Other)
            .map(|click| Value::from(click.name()))
            .collect();
        self.0.set("clicks", names);
        self
    }

    /// Every click kind reports. Needs an [`id`](Button::id).
    pub fn all_clicks(mut self) -> Self {
        self.0.set("clicks", "all");
        self
    }

    /// The menu closes when the button is clicked with one of the four
    /// mouse clicks, without waiting for the plugin. Needs no
    /// [`id`](Button::id).
    pub fn close(mut self) -> Self {
        self.0.set("close", true);
        self
    }

    /// The button as JSON. Fails only for a text or a value that is not
    /// valid JSON.
    pub fn to_value(&self) -> Result<Value, String> {
        self.0.to_value()
    }
}

/// Custom model data: a number for every version, or lists for 1.21.4 and
/// newer.
///
/// ```
/// use rift_plugin::menu::{Button, Model};
///
/// let simple = Button::new("paper").model(7.0);
/// let full = Button::new("paper").model(Model::new().floats([7.0]).strings(["red"]).colors([0xff0000]));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    number: Option<f32>,
    floats: Vec<f32>,
    flags: Vec<bool>,
    strings: Vec<String>,
    colors: Vec<u32>,
}

impl Model {
    /// Empty lists.
    pub fn new() -> Self {
        Self::default()
    }

    /// The `floats` list.
    pub fn floats(mut self, floats: impl IntoIterator<Item = f32>) -> Self {
        self.number = None;
        self.floats = floats.into_iter().collect();
        self
    }

    /// The `flags` list.
    pub fn flags(mut self, flags: impl IntoIterator<Item = bool>) -> Self {
        self.number = None;
        self.flags = flags.into_iter().collect();
        self
    }

    /// The `strings` list.
    pub fn strings<S: Into<String>>(mut self, strings: impl IntoIterator<Item = S>) -> Self {
        self.number = None;
        self.strings = strings.into_iter().map(Into::into).collect();
        self
    }

    /// The `colors` list, as RGB numbers such as `0xff8800`.
    pub fn colors(mut self, colors: impl IntoIterator<Item = u32>) -> Self {
        self.number = None;
        self.colors = colors.into_iter().collect();
        self
    }

    fn to_value(&self) -> Value {
        if let Some(number) = self.number {
            return Value::from(number);
        }
        let mut model = Map::new();
        model.insert("floats".into(), self.floats.clone().into());
        model.insert("flags".into(), self.flags.clone().into());
        model.insert("strings".into(), self.strings.clone().into());
        model.insert("colors".into(), self.colors.clone().into());
        Value::Object(model)
    }
}

impl From<f32> for Model {
    /// The single number every version reads.
    fn from(number: f32) -> Self {
        Self {
            number: Some(number),
            ..Self::default()
        }
    }
}

impl From<i32> for Model {
    /// The single number every version reads.
    fn from(number: i32) -> Self {
        Self::from(number as f32)
    }
}

/// The skin of a `player_head` button.
#[derive(Debug, Clone, PartialEq)]
pub struct Head(Map<String, Value>);

impl Head {
    /// The skin of the online player with this name or UUID.
    pub fn player(player: impl Into<String>) -> Self {
        let mut head = Map::new();
        head.insert("player".into(), Value::String(player.into()));
        Self(head)
    }

    /// A `textures` property value, as head databases publish it.
    pub fn texture(value: impl Into<String>) -> Self {
        let mut head = Map::new();
        head.insert("texture".into(), Value::String(value.into()));
        Self(head)
    }

    /// The signature of a [`Head::texture`].
    pub fn signature(mut self, signature: impl Into<String>) -> Self {
        self.0
            .insert("signature".into(), Value::String(signature.into()));
        self
    }

    fn to_value(&self) -> Value {
        Value::Object(self.0.clone())
    }
}

/// How a player clicked a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Click {
    /// A left click.
    Left,
    /// A right click.
    Right,
    /// A left click while holding Shift.
    ShiftLeft,
    /// A right click while holding Shift.
    ShiftRight,
    /// The mouse wheel. Clients from 1.18 send it only in creative mode.
    Middle,
    /// Q over the slot.
    Drop,
    /// Ctrl+Q over the slot.
    DropAll,
    /// 1 to 9 over the slot; [`MenuKind::Click`]'s `key` says which.
    NumberKey,
    /// F over the slot (1.16 and newer clients).
    Offhand,
    /// A kind this SDK does not know, from a newer Rift.
    Other,
}

impl Click {
    /// Every kind a button can report, without [`Click::Other`].
    pub const ALL: [Self; 9] = [
        Self::Left,
        Self::Right,
        Self::ShiftLeft,
        Self::ShiftRight,
        Self::Middle,
        Self::Drop,
        Self::DropAll,
        Self::NumberKey,
        Self::Offhand,
    ];

    /// The name events and definitions use, such as `shift_left`; `other`
    /// for [`Click::Other`].
    pub fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::ShiftLeft => "shift_left",
            Self::ShiftRight => "shift_right",
            Self::Middle => "middle",
            Self::Drop => "drop",
            Self::DropAll => "drop_all",
            Self::NumberKey => "number_key",
            Self::Offhand => "offhand",
            Self::Other => "other",
        }
    }

    /// The kind called `name`, or [`Click::Other`].
    pub fn from_name(name: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|click| click.name() == name)
            .unwrap_or(Self::Other)
    }
}

/// Why a menu ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseReason {
    /// The player pressed Escape or the inventory key.
    Player,
    /// A button with [`close`](Button::close), or
    /// [`players::close_menu`](crate::players::close_menu).
    Plugin,
    /// Rift showed another menu (a different id, or another plugin's) or a
    /// dialog.
    Replaced,
    /// The backend opened a screen of its own, closed the player's screens,
    /// or the player died or changed worlds.
    Server,
    /// The player is switching servers.
    Switch,
    /// The player left.
    Disconnect,
    /// A reason this SDK does not know, from a newer Rift.
    Other,
}

impl CloseReason {
    /// The reason called `name`, or [`CloseReason::Other`].
    pub fn from_name(name: &str) -> Self {
        match name {
            "player" => Self::Player,
            "plugin" => Self::Plugin,
            "replaced" => Self::Replaced,
            "server" => Self::Server,
            "switch" => Self::Switch,
            "disconnect" => Self::Disconnect,
            _ => Self::Other,
        }
    }
}

/// What a [`MenuEvent`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKind<'a> {
    /// The player clicked a button with an id.
    Click {
        /// The button's id.
        action: &'a str,
        /// The slot, from 0.
        slot: u32,
        /// How the player clicked.
        click: Click,
        /// 1 to 9 for [`Click::NumberKey`].
        key: Option<u8>,
    },
    /// The player typed in an anvil menu shown with [`Menu::typing`].
    Text {
        /// What the text field holds. It comes from the client: check it
        /// like chat.
        text: &'a str,
    },
    /// The menu ended. Every menu that was shown ends with exactly one.
    Close {
        /// Why.
        reason: CloseReason,
    },
    /// A kind this SDK does not know, from a newer Rift: ignore it.
    Other(&'a str),
}

impl MenuEvent {
    /// What the event is about.
    pub fn kind(&self) -> MenuKind<'_> {
        match self.kind.as_str() {
            "click" => MenuKind::Click {
                action: self.action.as_deref().unwrap_or_default(),
                slot: self.slot.unwrap_or_default(),
                click: Click::from_name(self.click.as_deref().unwrap_or_default()),
                key: self.key.and_then(|key| u8::try_from(key).ok()),
            },
            "text" => MenuKind::Text {
                text: self.text.as_deref().unwrap_or_default(),
            },
            "close" => MenuKind::Close {
                reason: CloseReason::from_name(self.reason.as_deref().unwrap_or_default()),
            },
            other => MenuKind::Other(other),
        }
    }

    /// The clicked button's [`payload`](Button::payload), as the plugin
    /// wrote it; `None` when it had none or the event is no click.
    pub fn payload<T: DeserializeOwned>(&self) -> Result<Option<T>, String> {
        self.payload_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| format!("invalid menu payload: {error}"))
    }

    /// The menu's [`state`](Menu::state), as the plugin last set it; `None`
    /// when it has none.
    pub fn state<T: DeserializeOwned>(&self) -> Result<Option<T>, String> {
        self.state_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| format!("invalid menu state: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Context, text};
    use serde_json::json;

    fn value(menu: &Menu) -> Value {
        menu.to_value().unwrap()
    }

    // The JSON literals below are also parsed by the host's own tests
    // (`src/menu/tests.rs`, `sdk_shaped_definitions_parse`): keep them equal.

    #[test]
    fn a_chest_with_a_layout_and_keys() {
        let menu = Menu::chest("servers")
            .title(text::mini("<dark_gray>Choose a server"))
            .layout(["#########", "# S C M #", "####X####"])
            .key('#', Button::new("gray_stained_glass_pane").name(" "))
            .key(
                'S',
                Button::new("grass_block")
                    .name(text::mini("<green>Survival"))
                    .lore(["Click to join"])
                    .id("join")
                    .payload("survival"),
            )
            .key('C', Button::new("bricks").id("join").payload("creative"))
            .key('M', Button::new("bow").id("join").payload("minigames"))
            .key(
                'X',
                Button::new("barrier")
                    .name(text::mini("<red>Close"))
                    .close(),
            );
        assert_eq!(
            value(&menu),
            json!({
                "id": "servers", "type": "chest",
                "title": {"mini": "<dark_gray>Choose a server"},
                "layout": ["#########", "# S C M #", "####X####"],
                "keys": {
                    "#": {"item": "gray_stained_glass_pane", "name": " "},
                    "S": {"item": "grass_block", "name": {"mini": "<green>Survival"},
                          "lore": ["Click to join"], "id": "join", "payload": "survival"},
                    "C": {"item": "bricks", "id": "join", "payload": "creative"},
                    "M": {"item": "bow", "id": "join", "payload": "minigames"},
                    "X": {"item": "barrier", "name": {"mini": "<red>Close"}, "close": true},
                },
            })
        );
        assert_eq!(
            serde_json::from_str::<Value>(&menu.to_json().unwrap()).unwrap(),
            value(&menu)
        );
    }

    #[test]
    fn every_button_field() {
        let button = Button::new("player_head")
            .count(3)
            .name(json!({"text": "Alex", "color": "gold"}))
            .lore(["one".into(), Text::from(text::legacy("&7two"))])
            .glint(true)
            .model(
                Model::new()
                    .floats([1.5])
                    .flags([true])
                    .strings(["a"])
                    .colors([0xff8800]),
            )
            .item_model("rift:menu/head")
            .head(Head::texture("dGV4dHVyZQ==").signature("c2ln"))
            .color("#112233")
            .details(true)
            .tooltip(false)
            .id("pick")
            .payload(json!({"n": 1}))
            .clicks([Click::Left, Click::NumberKey, Click::Other])
            .close();
        assert_eq!(
            button.to_value().unwrap(),
            json!({
                "item": "player_head", "count": 3,
                "name": {"text": "Alex", "color": "gold"},
                "lore": ["one", {"legacy": "&7two", "marker": "&"}],
                "glint": true,
                "model": {"floats": [1.5], "flags": [true], "strings": ["a"], "colors": [16746496]},
                "item_model": "rift:menu/head",
                "head": {"texture": "dGV4dHVyZQ==", "signature": "c2ln"},
                "color": "#112233", "details": true, "tooltip": false,
                "id": "pick", "payload": {"n": 1}, "clicks": ["left", "number_key"],
                "close": true,
            })
        );
        let simple = Button::new("paper")
            .model(7)
            .head(Head::player("Alex"))
            .all_clicks()
            .id("x");
        assert_eq!(
            simple.to_value().unwrap(),
            json!({"item": "paper", "model": 7.0, "head": {"player": "Alex"}, "clicks": "all", "id": "x"})
        );
    }

    #[test]
    fn slots_rows_state_and_the_other_types() {
        let menu = Menu::chest("kits")
            .rows(6)
            .title("Kits, page 2")
            .slot(
                0,
                Button::new("iron_sword")
                    .id("kit")
                    .payload(json!({"kit": "pvp"})),
            )
            .slots([45, 53], Button::new("arrow").name("Page").id("page"))
            .empty(4)
            .state(json!({"page": 2}));
        assert_eq!(
            value(&menu),
            json!({
                "id": "kits", "type": "chest", "rows": 6, "title": "Kits, page 2",
                "slots": [
                    {"slot": 0, "item": "iron_sword", "id": "kit", "payload": {"kit": "pvp"}},
                    {"slots": [45, 53], "item": "arrow", "name": "Page", "id": "page"},
                    {"slot": 4, "empty": true},
                ],
                "state": {"page": 2},
            })
        );
        assert_eq!(
            value(
                &Menu::hopper("h")
                    .layout(["ab ba"])
                    .key('a', Button::new("stone"))
                    .key('b', Button::new("dirt"))
            ),
            json!({"id": "h", "type": "hopper", "layout": ["ab ba"],
                   "keys": {"a": {"item": "stone"}, "b": {"item": "dirt"}}})
        );
        assert_eq!(
            value(&Menu::dispenser("d")),
            json!({"id": "d", "type": "dispenser"})
        );
        assert_eq!(
            value(
                &Menu::anvil("nick")
                    .title("Choose a nickname")
                    .text("Steve")
                    .typing()
            ),
            json!({"id": "nick", "type": "anvil", "title": "Choose a nickname", "text": "Steve", "typing": true})
        );
    }

    #[test]
    fn patches() {
        let patch = MenuPatch::new()
            .slot(4, Button::new("clock").name("9"))
            .slots([0, 8], Button::new("glass_pane"))
            .empty(5)
            .key('.', Button::new("black_stained_glass_pane"))
            .title(text::mini("<red>Hurry"))
            .state(json!({"left": 9}))
            .text("Alex");
        assert_eq!(
            patch.to_value().unwrap(),
            json!({
                "slots": [
                    {"slot": 4, "item": "clock", "name": "9"},
                    {"slots": [0, 8], "item": "glass_pane"},
                    {"slot": 5, "empty": true},
                ],
                "keys": {".": {"item": "black_stained_glass_pane"}},
                "title": {"mini": "<red>Hurry"},
                "state": {"left": 9},
                "text": "Alex",
            })
        );
        assert_eq!(MenuPatch::new().to_json().unwrap(), "{}");
    }

    #[test]
    fn invalid_json_text_fails_the_conversion() {
        let menu = Menu::chest("m").key('a', Button::new("stone").name(Text::json("{nope")));
        let error = menu.to_json().unwrap_err();
        assert!(error.starts_with("menu.name: invalid JSON text"), "{error}");
        let menu = Menu::chest("m").title(Text::json("[1,"));
        assert!(menu.to_value().unwrap_err().starts_with("menu.title:"));
        let patch = MenuPatch::new().slot(0, Button::new("stone").lore([Text::json("x")]));
        assert!(patch.to_json().unwrap_err().starts_with("patch.lore:"));
        // A JSON string is still JSON text.
        assert_eq!(
            Button::new("stone")
                .name(Text::json("\"a\""))
                .to_value()
                .unwrap(),
            json!({"item": "stone", "name": "a"})
        );
    }

    fn event(kind: &str) -> MenuEvent {
        MenuEvent {
            ctx: Context {
                connection_id: 1,
                uuid: "ab".repeat(16),
                name: "Alex".into(),
                listener: String::new(),
                hostname: String::new(),
                peer_ip: String::new(),
                protocol: 774,
                server: Some("lobby".into()),
                default_server: None,
                target: None,
                reason: None,
                success: None,
                command: None,
                args: String::new(),
                word: String::new(),
                position: 0,
                language: None,
                console: false,
            },
            menu: "servers".into(),
            kind: kind.into(),
            action: None,
            slot: None,
            click: None,
            key: None,
            text: None,
            reason: None,
            payload_json: None,
            state_json: None,
        }
    }

    #[test]
    fn events_decode_their_kind_payload_and_state() {
        let click = MenuEvent {
            action: Some("join".into()),
            slot: Some(13),
            click: Some("number_key".into()),
            key: Some(4),
            payload_json: Some(r#""survival""#.into()),
            state_json: Some(r#"{"page":2}"#.into()),
            ..event("click")
        };
        assert_eq!(
            click.kind(),
            MenuKind::Click {
                action: "join",
                slot: 13,
                click: Click::NumberKey,
                key: Some(4)
            }
        );
        assert!(matches!(
            click.kind(),
            MenuKind::Click { action: "join", .. }
        ));
        assert_eq!(click.payload::<String>(), Ok(Some("survival".into())));
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct State {
            page: u32,
        }
        assert_eq!(click.state::<State>(), Ok(Some(State { page: 2 })));
        assert!(
            click
                .payload::<u32>()
                .unwrap_err()
                .contains("invalid menu payload")
        );

        let text = MenuEvent {
            text: Some("Ale".into()),
            ..event("text")
        };
        assert_eq!(text.kind(), MenuKind::Text { text: "Ale" });
        assert_eq!(text.payload::<Value>(), Ok(None));
        assert_eq!(text.state::<Value>(), Ok(None));

        for (name, reason) in [
            ("player", CloseReason::Player),
            ("plugin", CloseReason::Plugin),
            ("replaced", CloseReason::Replaced),
            ("server", CloseReason::Server),
            ("switch", CloseReason::Switch),
            ("disconnect", CloseReason::Disconnect),
            ("timeout", CloseReason::Other),
        ] {
            let close = MenuEvent {
                reason: Some(name.into()),
                ..event("close")
            };
            assert_eq!(close.kind(), MenuKind::Close { reason });
        }
        assert_eq!(event("hover").kind(), MenuKind::Other("hover"));
        for click in Click::ALL {
            assert_eq!(Click::from_name(click.name()), click);
        }
        assert_eq!(Click::from_name("triple"), Click::Other);
        let unknown = MenuEvent {
            action: Some("x".into()),
            click: Some("triple".into()),
            ..event("click")
        };
        assert!(matches!(
            unknown.kind(),
            MenuKind::Click {
                click: Click::Other,
                slot: 0,
                key: None,
                ..
            }
        ));
    }
}
