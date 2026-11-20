//! Inventory menus that plugins show to players (Minecraft 1.12 and newer):
//! chest, hopper, dispenser and anvil screens whose slots hold items that
//! act as buttons. See docs/menus.md.
//!
//! This module is the data: a plugin's definition parsed and checked
//! ([`Menu::parse`]), so a mistake fails the plugin's call instead of
//! disconnecting the player; changes to an open menu ([`Patch`],
//! [`Menu::apply`]); what a redraw must send ([`Menu::diff`]); and the
//! anvil's text rules ([`clean_anvil_text`]). [`stack`] turns a button into
//! the item stack each client version reads.
//!
//! Texts (the title, names and lore) are resolved when the definition is
//! parsed, like a dialog's: markup, legacy codes and the theme, and
//! `message` objects in the language of the render scope the caller set up
//! for the recipient. A parsed menu therefore belongs to one player.
use crate::text::Text;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

mod event;
pub mod stack;
#[cfg(test)]
mod tests;

pub use event::{CloseReason, Delivery, EventKind, MenuEvent, Shown, Ticket, Update};
pub use stack::{StackCache, Textures};

/// The largest menu definition or patch, as compact JSON.
pub const MAX_MENU_BYTES: usize = 256 * 1024;
/// The largest `state`, as compact JSON.
pub const MAX_STATE_BYTES: usize = 16 * 1024;
/// The largest `payload` of one button, as compact JSON.
pub const MAX_PAYLOAD_BYTES: usize = 4 * 1024;
/// Lore lines per button.
pub const MAX_LORE_LINES: usize = 32;
/// Menu events a player may have waiting for the plugin before Rift
/// reports no more.
pub const MAX_PENDING_EVENTS: usize = 32;
/// `text` events per second while a player types in an anvil.
pub const TEXT_EVENTS_PER_SECOND: u32 = 5;
/// The largest count of a button's stack.
pub const MAX_COUNT: u8 = 99;
/// Rows of the largest chest.
pub const MAX_ROWS: u8 = 6;
/// Rows of a chest that names neither `rows` nor a `layout`.
const DEFAULT_ROWS: u8 = 3;
/// Characters of a menu or button `id`.
const MAX_ID: usize = 64;
/// Values in each list of a `model`.
const MAX_MODEL_VALUES: usize = 64;
/// Bytes of a string in a `model`, `item_model` or `head`.
const MAX_STRING: usize = 1024;
/// The longest texture value of a `head`, as the game's profile codec
/// allows.
const MAX_TEXTURE: usize = 32_767;

/// The anvil's left input slot, which holds the item being renamed.
pub const ANVIL_LEFT: usize = 0;
/// The anvil's right input slot.
pub const ANVIL_RIGHT: usize = 1;
/// The anvil's result slot.
pub const ANVIL_RESULT: usize = 2;
/// The action of the default anvil's result button.
pub const ANVIL_SUBMIT: &str = "submit";

/// The longest text an anvil takes from 1.17 on, in UTF-16 units as the
/// game counts.
pub const ANVIL_TEXT_MAX: usize = 50;
/// The longest text an anvil takes before 1.17.
pub const ANVIL_TEXT_MAX_BEFORE_1_17: usize = 35;
/// The protocol of 1.17, where the anvil's limit grew.
const ANVIL_LONGER_TEXT: i32 = 755;

/// The fields of a menu definition.
const MENU_FIELDS: &[&str] = &[
    "id", "type", "rows", "title", "layout", "keys", "slots", "state", "text", "typing",
];
/// The fields of a button.
const BUTTON_FIELDS: &[&str] = &[
    "item",
    "count",
    "name",
    "lore",
    "glint",
    "model",
    "item_model",
    "head",
    "color",
    "details",
    "tooltip",
    "id",
    "payload",
    "clicks",
    "close",
];
/// What a `slots` entry takes beside the button's fields.
const PLACEMENT_FIELDS: &[&str] = &["slot", "slots", "empty"];
/// The fields of a patch.
const PATCH_FIELDS: &[&str] = &["slots", "keys", "title", "state", "text"];

/// The menu types Rift shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    /// A chest with 1 to 6 rows of 9 slots.
    Chest { rows: u8 },
    /// 5 slots in a row.
    Hopper,
    /// 3 rows of 3 slots.
    Dispenser,
    /// Left input, right input, result, and a text field.
    Anvil,
}

impl Kind {
    /// The menu's own slots, numbered from 0.
    pub fn slots(self) -> usize {
        match self {
            Self::Chest { rows } => usize::from(rows) * 9,
            Self::Hopper => 5,
            Self::Dispenser => 9,
            Self::Anvil => 3,
        }
    }

    /// Characters per line of a `layout`.
    pub fn width(self) -> usize {
        match self {
            Self::Chest { .. } => 9,
            Self::Hopper => 5,
            Self::Dispenser | Self::Anvil => 3,
        }
    }

    /// Lines of a `layout`.
    pub fn lines(self) -> usize {
        self.slots() / self.width()
    }

    /// The menu in the newest protocol's `menu` registry, without namespace.
    pub fn menu(self) -> &'static str {
        match self {
            Self::Chest { rows: 1 } => "generic_9x1",
            Self::Chest { rows: 2 } => "generic_9x2",
            Self::Chest { rows: 3 } => "generic_9x3",
            Self::Chest { rows: 4 } => "generic_9x4",
            Self::Chest { rows: 5 } => "generic_9x5",
            Self::Chest { .. } => "generic_9x6",
            Self::Hopper => "hopper",
            Self::Dispenser => "generic_3x3",
            Self::Anvil => "anvil",
        }
    }

    /// How `open_screen` names this menu for a client on `protocol`: a
    /// `menu` registry ID from 1.14, a window type and slot count before.
    pub fn menu_type(
        self,
        protocol: i32,
    ) -> Option<rift_translation::codec::misc::screen::MenuType> {
        rift_translation::codec::misc::screen::menu_type(protocol, self.menu())
    }

    /// The `type` a definition names.
    pub fn name(self) -> &'static str {
        match self {
            Self::Chest { .. } => "chest",
            Self::Hopper => "hopper",
            Self::Dispenser => "dispenser",
            Self::Anvil => "anvil",
        }
    }

    /// "a chest with 3 rows", for errors.
    fn describe(self) -> String {
        match self {
            Self::Chest { rows: 1 } => "a chest with 1 row".into(),
            Self::Chest { rows } => format!("a chest with {rows} rows"),
            Self::Hopper => "a hopper".into(),
            Self::Dispenser => "a dispenser".into(),
            Self::Anvil => "an anvil".into(),
        }
    }

    /// Slot `number` as an index, or the error that names the range.
    fn slot(self, number: i64) -> Result<usize, String> {
        usize::try_from(number)
            .ok()
            .filter(|slot| *slot < self.slots())
            .ok_or_else(|| {
                format!(
                    "slot {number}: {} has slots 0 to {}",
                    self.describe(),
                    self.slots() - 1
                )
            })
    }
}

/// How a player clicked a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Click {
    Left,
    Right,
    ShiftLeft,
    ShiftRight,
    /// The mouse wheel; newer clients send it only in creative mode.
    Middle,
    /// Q over the slot.
    Drop,
    /// Ctrl+Q over the slot.
    DropAll,
    /// 1 to 9 over the slot.
    NumberKey,
    /// F over the slot (1.16 and newer clients).
    Offhand,
}

impl Click {
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

    /// The name events and definitions use.
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
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|click| click.name() == name)
    }

    fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// The click kinds a button reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Clicks(u16);

impl Clicks {
    pub const NONE: Self = Self(0);
    /// The default: the four mouse clicks.
    pub const MOUSE: Self = Self(0b1111);
    pub const ALL: Self = Self((1 << Click::ALL.len()) - 1);

    pub fn contains(self, click: Click) -> bool {
        self.0 & click.bit() != 0
    }

    pub fn with(self, click: Click) -> Self {
        Self(self.0 | click.bit())
    }
}

impl Default for Clicks {
    fn default() -> Self {
        Self::MOUSE
    }
}

/// Custom model data: one number, or the lists 1.21.4 and newer read.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub floats: Vec<f32>,
    pub flags: Vec<bool>,
    pub strings: Vec<String>,
    /// RGB.
    pub colors: Vec<i32>,
}

/// The skin of a `player_head` button.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Head {
    /// A texture property as head databases publish it.
    Texture(Textures),
    /// The skin of an online player, by name or UUID, looked up when the
    /// stack is built.
    Player(String),
}

/// An item name of the newest protocol, without namespace. An alias, so
/// that serde reads it through [`item_name`] instead of borrowing it.
pub type Item = &'static str;

/// Everything about a button that goes into its item stack.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Look {
    /// The item in the newest protocol, without namespace.
    #[serde(with = "item_name")]
    pub item: Item,
    pub count: u8,
    /// `None` keeps the item's own name; an empty plain text is no name.
    #[serde(with = "exact_text::option")]
    pub name: Option<Text>,
    #[serde(with = "exact_text::list")]
    pub lore: Vec<Text>,
    /// `None` keeps the item's own glint.
    pub glint: Option<bool>,
    pub model: Option<Model>,
    pub item_model: Option<String>,
    pub head: Option<Head>,
    /// RGB, for dyeable items and potions.
    pub color: Option<u32>,
    /// Whether the lines the game adds on its own show.
    pub details: bool,
    /// Whether the tooltip shows at all.
    pub tooltip: bool,
}

impl Look {
    /// A plain stack of `item`.
    fn of(item: &'static str) -> Self {
        Self {
            item,
            count: 1,
            name: None,
            lore: Vec::new(),
            glint: None,
            model: None,
            item_model: None,
            head: None,
            color: None,
            details: false,
            tooltip: true,
        }
    }
}

/// What clicking a button does.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Action {
    /// The action name clicks report; `None` for decoration.
    pub id: Option<String>,
    /// Handed back with the click; the client never sees it.
    pub payload: Option<Value>,
    /// Rift closes the menu on a click, without waiting for the plugin.
    pub close: bool,
    pub clicks: Clicks,
}

impl Action {
    /// Whether a click of this kind is reported to the plugin.
    pub fn reports(&self, click: Click) -> bool {
        self.id.is_some() && self.clicks.contains(click)
    }

    /// Whether a click of this kind closes the menu. A button without an
    /// `id` closes on the four mouse clicks.
    pub fn closes(&self, click: Click) -> bool {
        self.close && self.clicks.contains(click)
    }

    /// Whether the two do the same when clicked: the same `id`, `payload`
    /// and `close`. A click on a slot whose action changed since the
    /// player saw it is stale.
    pub fn same_effect(&self, other: &Self) -> bool {
        self.id == other.id && self.payload == other.payload && self.close == other.close
    }
}

/// An item with an optional action.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Button {
    pub look: Look,
    pub action: Action,
}

/// A validated menu: what every slot holds, ready to send to any client
/// from 1.12 on.
///
/// Serializable, so that a peer of a cluster can show it to a player of
/// another peer. The receiving peer checks the menu it reads by the rules
/// of a definition and refuses one that breaks them.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "checked::UncheckedMenu")]
pub struct Menu {
    id: String,
    kind: Kind,
    #[serde(with = "exact_text::option")]
    title: Option<Text>,
    state: Option<Value>,
    /// Anvil: what the text field holds.
    text: String,
    /// Anvil: the text Rift last put in the field, which is the left
    /// item's name. Typing changes `text` only.
    field: String,
    typing: bool,
    slots: Vec<Option<Button>>,
    /// The layout character each slot got, until a `slots` entry replaces
    /// it; a patch's `keys` applies to these.
    keyed: Vec<Option<char>>,
    /// Anvil: the result slot holds Rift's default `submit` button.
    default_result: bool,
}

/// What a redraw has to send to turn one menu into another.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// The type or the number of rows changed: the menu opens anew, and
    /// `slots` and `actions` list every slot.
    pub reopen: bool,
    pub title: bool,
    /// Slots that look different, to be sent again.
    pub slots: Vec<usize>,
    /// Slots whose action (`id`, `payload` or `close`) changed: a click
    /// that names an earlier state of these is stale.
    pub actions: Vec<usize>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        !self.reopen && !self.title && self.slots.is_empty() && self.actions.is_empty()
    }
}

/// A change to part of an open menu (`update_menu`). Slot numbers are
/// checked against the menu when it is applied; one from another peer is
/// checked as it is read, like a [`Menu`].
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "checked::UncheckedPatch")]
pub struct Patch {
    /// Slot numbers as written and what they get: `None` empties them.
    slots: Vec<(Vec<i64>, Option<Button>)>,
    keys: Vec<(char, Button)>,
    #[serde(with = "exact_text::patch")]
    title: Option<Option<Text>>,
    state: Option<Value>,
    text: Option<String>,
}

impl Menu {
    /// Validate a menu definition.
    pub fn parse(spec: &Value) -> Result<Self, String> {
        size(spec, MAX_MENU_BYTES, "menu")?;
        let fields = Fields::new(spec, "menu".into(), MENU_FIELDS)?;
        let id = fields
            .string("id")?
            .ok_or_else(|| "menu.id is required".to_owned())?;
        check_id(id, &fields.path("id"))?;
        let layout = match fields.list("layout")? {
            Some(lines) if !lines.is_empty() => Some(
                lines
                    .iter()
                    .enumerate()
                    .map(|(index, line)| {
                        line.as_str().ok_or_else(|| {
                            format!("{}[{}] must be a string", fields.path("layout"), index + 1)
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            _ => None,
        };
        let kind = kind(&fields, layout.as_deref())?;
        let keys = match fields.get("keys") {
            Some(keys) => parse_keys(keys, &fields.path("keys"))?,
            None => BTreeMap::new(),
        };
        let mut menu = Self {
            id: id.to_owned(),
            kind,
            title: None,
            state: None,
            text: String::new(),
            field: String::new(),
            typing: false,
            slots: vec![None; kind.slots()],
            keyed: vec![None; kind.slots()],
            default_result: false,
        };
        if let Some(layout) = layout {
            menu.lay_out(&layout, &keys, &fields.path("layout"))?;
        }
        if let Some(entries) = fields.list("slots")? {
            for (index, entry) in entries.into_iter().enumerate() {
                let path = format!("{}[{}]", fields.path("slots"), index + 1);
                let (numbers, button) = placement(entry, &path)?;
                for number in numbers {
                    let slot = kind.slot(number)?;
                    menu.slots[slot] = button.clone();
                    menu.keyed[slot] = None;
                }
            }
        }
        menu.title = fields.title("title")?;
        menu.state = fields.state("state")?;
        let anvil = kind == Kind::Anvil;
        if let Some(text) = fields.string("text")? {
            if !anvil {
                return Err("menu.text is for anvils only".into());
            }
            menu.text = anvil_text(text, &fields.path("text"))?;
            menu.field = menu.text.clone();
        }
        if let Some(typing) = fields.boolean("typing")? {
            if !anvil {
                return Err("menu.typing is for anvils only".into());
            }
            menu.typing = typing;
        }
        if anvil {
            menu.fill_anvil();
        }
        Ok(menu)
    }

    /// Validate a menu definition given as JSON text.
    pub fn parse_json(text: &str) -> Result<Self, String> {
        Self::parse(&from_json(text, "menu")?)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// `None` for an empty title.
    pub fn title(&self) -> Option<&Text> {
        self.title.as_ref()
    }

    /// The `state` the plugin last set, handed back with every event.
    pub fn state(&self) -> Option<&Value> {
        self.state.as_ref()
    }

    /// Anvil: what the text field holds; empty for other menus.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Anvil: whether the plugin asked for `text` events while the player
    /// types.
    pub fn typing(&self) -> bool {
        self.typing
    }

    /// What every slot holds, by slot number.
    pub fn slots(&self) -> &[Option<Button>] {
        &self.slots
    }

    /// The button in slot `slot`, as a click names it; `None` for an empty
    /// slot or one outside the menu.
    pub fn button(&self, slot: i32) -> Option<&Button> {
        self.slots.get(usize::try_from(slot).ok()?)?.as_ref()
    }

    /// Anvil: the player typed `text` (already cleaned with
    /// [`clean_anvil_text`]). The default result is renamed after it; the
    /// left item keeps its name, which would reset the field if sent again.
    pub fn typed(&mut self, text: String) {
        if self.kind == Kind::Anvil {
            self.text = text;
            self.fill_anvil();
        }
    }

    /// Anvil: the text Rift last put in the field, as the left item's name.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// Anvil: name the left item after what the field holds, for when Rift
    /// sends that slot again anyway. The client then fills the field with
    /// what the player typed instead of an older text.
    pub fn field_from_text(&mut self) {
        if self.kind == Kind::Anvil && self.field != self.text {
            self.field = self.text.clone();
            self.fill_anvil();
        }
    }

    /// Anvil: cut the text to what a client on `protocol` takes, as its
    /// field would.
    pub fn fit_text(&mut self, protocol: i32) {
        if self.kind == Kind::Anvil {
            self.text = clean_anvil_text(protocol, &self.text);
            self.field = clean_anvil_text(protocol, &self.field);
            self.fill_anvil();
        }
    }

    /// Anvil: keep what the player typed in `open`, the menu this one
    /// redraws. Only a patch with `text` replaces what the player typed.
    pub fn keep_text(&mut self, open: &Menu) {
        if self.kind == Kind::Anvil && open.kind == Kind::Anvil {
            self.text = open.text.clone();
            self.field = open.field.clone();
            self.fill_anvil();
        }
    }

    /// Apply a patch. Fails without changing anything when the patch names
    /// a slot outside the menu, or `text` for a menu that is not an anvil.
    pub fn apply(&mut self, patch: &Patch) -> Result<(), String> {
        let anvil = self.kind == Kind::Anvil;
        if patch.text.is_some() && !anvil {
            return Err("patch.text is for anvils only".into());
        }
        let mut placed = Vec::new();
        for (numbers, button) in &patch.slots {
            for number in numbers {
                placed.push((self.kind.slot(*number)?, button));
            }
        }
        for (key, button) in &patch.keys {
            for slot in 0..self.slots.len() {
                if self.keyed[slot] == Some(*key) {
                    self.slots[slot] = Some(button.clone());
                    if slot == ANVIL_RESULT {
                        self.default_result = false;
                    }
                }
            }
        }
        for (slot, button) in placed {
            self.slots[slot] = button.clone();
            self.keyed[slot] = None;
            if slot == ANVIL_RESULT {
                self.default_result = false;
            }
        }
        if let Some(title) = &patch.title {
            self.title = title.clone();
        }
        if let Some(state) = &patch.state {
            self.state = Some(state.clone());
        }
        if let Some(text) = &patch.text {
            self.text = text.clone();
            self.field = text.clone();
        }
        if anvil {
            self.fill_anvil();
        }
        Ok(())
    }

    /// What a redraw from this menu (what the client shows) to `new` has to
    /// send.
    pub fn diff(&self, new: &Menu) -> Diff {
        if self.kind != new.kind {
            let all: Vec<usize> = (0..new.slots.len()).collect();
            return Diff {
                reopen: true,
                title: true,
                slots: all.clone(),
                actions: all,
            };
        }
        let mut diff = Diff {
            title: self.title != new.title,
            ..Diff::default()
        };
        let none = Action::default();
        for (slot, (old, new)) in self.slots.iter().zip(&new.slots).enumerate() {
            if old.as_ref().map(|button| &button.look) != new.as_ref().map(|button| &button.look) {
                diff.slots.push(slot);
            }
            let action = |button: &Option<Button>| {
                button
                    .as_ref()
                    .map_or(none.clone(), |button| button.action.clone())
            };
            if !action(old).same_effect(&action(new)) {
                diff.actions.push(slot);
            }
        }
        diff
    }

    /// Fill the layout's slots with the buttons of `keys`.
    fn lay_out(
        &mut self,
        layout: &[&str],
        keys: &BTreeMap<char, Button>,
        path: &str,
    ) -> Result<(), String> {
        let width = self.kind.width();
        for (line, text) in layout.iter().enumerate() {
            let characters: Vec<char> = text.chars().collect();
            if characters.len() != width {
                return Err(format!(
                    "{path}[{}]: a line of {} has {width} characters, not {}",
                    line + 1,
                    self.kind.describe(),
                    characters.len()
                ));
            }
            for (column, key) in characters.into_iter().enumerate() {
                if key == ' ' {
                    continue;
                }
                let button = keys.get(&key).ok_or_else(|| {
                    format!(
                        "{path}[{}]: the character {key:?} has no entry in keys",
                        line + 1
                    )
                })?;
                let slot = line * width + column;
                self.slots[slot] = Some(button.clone());
                self.keyed[slot] = Some(key);
            }
        }
        Ok(())
    }

    /// The anvil's rules: the left slot always holds an item, named after
    /// the text Rift put in the field; the result is never empty (the client
    /// draws a red cross over the arrow then), so without a button of the
    /// plugin's it holds the default result, a copy of the left item named
    /// after what the field holds.
    fn fill_anvil(&mut self) {
        if self.slots[ANVIL_RESULT].is_none() {
            self.default_result = true;
        }
        let left = self.slots[ANVIL_LEFT].get_or_insert_with(|| Button {
            look: Look::of("paper"),
            action: Action::default(),
        });
        left.look.name = Some(Text::Plain(self.field.clone()));
        if self.default_result {
            let mut look = left.look.clone();
            look.name = Some(Text::Plain(self.text.clone()));
            self.slots[ANVIL_RESULT] = Some(Button {
                look,
                action: Action {
                    id: Some(ANVIL_SUBMIT.into()),
                    ..Action::default()
                },
            });
        }
    }
}

impl Patch {
    /// Validate a patch (`update_menu`).
    pub fn parse(spec: &Value) -> Result<Self, String> {
        size(spec, MAX_MENU_BYTES, "patch")?;
        let fields = Fields::new(spec, "patch".into(), PATCH_FIELDS)?;
        let mut slots = Vec::new();
        if let Some(entries) = fields.list("slots")? {
            for (index, entry) in entries.into_iter().enumerate() {
                let path = format!("{}[{}]", fields.path("slots"), index + 1);
                slots.push(placement(entry, &path)?);
            }
        }
        let keys = match fields.get("keys") {
            Some(keys) => parse_keys(keys, &fields.path("keys"))?
                .into_iter()
                .collect(),
            None => Vec::new(),
        };
        let title = match fields.get("title") {
            Some(_) => Some(fields.title("title")?),
            None => None,
        };
        let text = fields
            .string("text")?
            .map(|text| anvil_text(text, &fields.path("text")))
            .transpose()?;
        Ok(Self {
            slots,
            keys,
            title,
            state: fields.state("state")?,
            text,
        })
    }

    /// Validate a patch given as JSON text.
    pub fn parse_json(text: &str) -> Result<Self, String> {
        Self::parse(&from_json(text, "patch")?)
    }

    /// Every button the patch places.
    pub fn buttons(&self) -> impl Iterator<Item = &Button> {
        self.slots
            .iter()
            .filter_map(|(_, button)| button.as_ref())
            .chain(self.keys.iter().map(|(_, button)| button))
    }
}

/// The checks [`Menu::parse`] and [`Patch::parse`] make, run again on a
/// menu or a patch another peer of a cluster sent: a peer of another build
/// could send one this build would never make, such as more slots than its
/// type has, and a session must not index past them.
mod checked {
    use super::*;

    /// [`Menu`] as it arrives, before [`Menu::check`].
    #[derive(serde::Deserialize)]
    pub(super) struct UncheckedMenu {
        id: String,
        kind: Kind,
        #[serde(with = "exact_text::option")]
        title: Option<Text>,
        state: Option<Value>,
        text: String,
        field: String,
        typing: bool,
        slots: Vec<Option<Button>>,
        keyed: Vec<Option<char>>,
        default_result: bool,
    }

    impl TryFrom<UncheckedMenu> for Menu {
        type Error = String;

        fn try_from(menu: UncheckedMenu) -> Result<Self, String> {
            let menu = Self {
                id: menu.id,
                kind: menu.kind,
                title: menu.title,
                state: menu.state,
                text: menu.text,
                field: menu.field,
                typing: menu.typing,
                slots: menu.slots,
                keyed: menu.keyed,
                default_result: menu.default_result,
            };
            menu.check()?;
            Ok(menu)
        }
    }

    /// [`Patch`] as it arrives, before [`Patch::check`].
    #[derive(serde::Deserialize)]
    pub(super) struct UncheckedPatch {
        slots: Vec<(Vec<i64>, Option<Button>)>,
        keys: Vec<(char, Button)>,
        #[serde(with = "exact_text::patch")]
        title: Option<Option<Text>>,
        state: Option<Value>,
        text: Option<String>,
    }

    impl TryFrom<UncheckedPatch> for Patch {
        type Error = String;

        fn try_from(patch: UncheckedPatch) -> Result<Self, String> {
            let patch = Self {
                slots: patch.slots,
                keys: patch.keys,
                title: patch.title,
                state: patch.state,
                text: patch.text,
            };
            patch.check()?;
            Ok(patch)
        }
    }

    impl Menu {
        /// What every parsed menu holds: a valid `id`, a type Rift shows,
        /// exactly as many slots as the type has, an anvil's texts within
        /// its rules and nothing of an anvil in other menus, and buttons
        /// within their limits.
        pub(super) fn check(&self) -> Result<(), String> {
            check_id(&self.id, "menu.id")?;
            if let Kind::Chest { rows } = self.kind
                && !(1..=MAX_ROWS).contains(&rows)
            {
                return Err(format!(
                    "menu: a chest has 1 to {MAX_ROWS} rows, not {rows}"
                ));
            }
            let slots = self.kind.slots();
            if self.slots.len() != slots || self.keyed.len() != slots {
                return Err(format!(
                    "menu: {} has {slots} slots, not {}",
                    self.kind.describe(),
                    self.slots.len()
                ));
            }
            check_state(self.state.as_ref(), "menu.state")?;
            if self.kind == Kind::Anvil {
                anvil_text(&self.text, "menu.text")?;
                anvil_text(&self.field, "menu.text")?;
            } else if !self.text.is_empty()
                || !self.field.is_empty()
                || self.typing
                || self.default_result
            {
                return Err("menu: text and typing are for anvils only".into());
            }
            for (slot, button) in self.slots.iter().enumerate() {
                if let Some(button) = button {
                    button.check(&format!("menu.slots[{slot}]"))?;
                }
            }
            Ok(())
        }
    }

    impl Patch {
        /// What every parsed patch holds: buttons within their limits, keys
        /// that are not a space, a `state` and a `text` within theirs. Slot
        /// numbers are checked when the patch is applied, as always.
        pub(super) fn check(&self) -> Result<(), String> {
            for (index, (_, button)) in self.slots.iter().enumerate() {
                if let Some(button) = button {
                    button.check(&format!("patch.slots[{}]", index + 1))?;
                }
            }
            for (key, button) in &self.keys {
                if *key == ' ' {
                    return Err("patch.keys: a space is always an empty slot".into());
                }
                button.check(&format!("patch.keys.{key}"))?;
            }
            check_state(self.state.as_ref(), "patch.state")?;
            if let Some(text) = &self.text {
                anvil_text(text, "patch.text")?;
            }
            Ok(())
        }
    }

    impl Button {
        /// The limits [`parse_button`] checks.
        fn check(&self, path: &str) -> Result<(), String> {
            let look = &self.look;
            if look.item == "air" {
                return Err(format!("{path}.item: unknown item \"air\""));
            }
            if !(1..=MAX_COUNT).contains(&look.count) {
                return Err(format!(
                    "{path}.count must be an integer from 1 to {MAX_COUNT}"
                ));
            }
            if look.lore.len() > MAX_LORE_LINES {
                return Err(format!("{path}.lore: at most {MAX_LORE_LINES} lines"));
            }
            if let Some(model) = &look.model {
                let lists = [
                    model.floats.len(),
                    model.flags.len(),
                    model.strings.len(),
                    model.colors.len(),
                ];
                if lists.iter().any(|len| *len > MAX_MODEL_VALUES)
                    || model.floats.iter().any(|float| !float.is_finite())
                    || model.strings.iter().any(|text| text.len() > MAX_STRING)
                    || model
                        .colors
                        .iter()
                        .any(|rgb| !(0..=0xff_ffff).contains(rgb))
                {
                    return Err(format!("{path}.model is out of its limits"));
                }
            }
            if let Some(model) = &look.item_model
                && (model.len() > MAX_STRING || !crate::dialog::identifier(model))
            {
                return Err(format!(
                    "{path}.item_model: {model:?} is not a valid identifier"
                ));
            }
            match &look.head {
                Some(_) if look.item != "player_head" => {
                    return Err(format!("{path}.head is for player_head only"));
                }
                Some(Head::Texture(textures))
                    if textures.value.is_empty()
                        || textures.value.len() > MAX_TEXTURE
                        || textures
                            .signature
                            .as_ref()
                            .is_some_and(|signature| signature.len() > MAX_STRING) =>
                {
                    return Err(format!("{path}.head is out of its limits"));
                }
                Some(Head::Player(player))
                    if !(1..=36).contains(&player.len())
                        || !player
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') =>
                {
                    return Err(format!("{path}.head.player must be a player name or UUID"));
                }
                _ => {}
            }
            if look.color.is_some_and(|rgb| rgb > 0xff_ffff) {
                return Err(format!("{path}.color must be a colour like \"#RRGGBB\""));
            }
            let action = &self.action;
            if let Some(id) = &action.id {
                check_id(id, &format!("{path}.id"))?;
            } else if action.payload.is_some() {
                return Err(format!(
                    "{path}.payload: a button without an id reports no clicks"
                ));
            }
            if let Some(payload) = &action.payload {
                size(payload, MAX_PAYLOAD_BYTES, &format!("{path}.payload"))?;
            }
            if action.clicks.0 & !Clicks::ALL.0 != 0 {
                return Err(format!("{path}.clicks names an unknown click kind"));
            }
            Ok(())
        }
    }

    fn check_state(state: Option<&Value>, path: &str) -> Result<(), String> {
        state.map_or(Ok(()), |state| size(state, MAX_STATE_BYTES, path))
    }
}

/// The longest anvil text a client on `protocol` takes.
pub fn anvil_text_limit(protocol: i32) -> usize {
    if protocol >= ANVIL_LONGER_TEXT {
        ANVIL_TEXT_MAX
    } else {
        ANVIL_TEXT_MAX_BEFORE_1_17
    }
}

/// What the player typed in an anvil, as Rift keeps it: without the
/// characters the game filters out of names (control characters and `§`),
/// and cut to the client's limit as the game counts it (UTF-16 units).
pub fn clean_anvil_text(protocol: i32, text: &str) -> String {
    let limit = anvil_text_limit(protocol);
    let mut units = 0;
    let mut clean = String::new();
    for c in text.chars().filter(|c| allowed_character(*c)) {
        units += c.len_utf16();
        if units > limit {
            break;
        }
        clean.push(c);
    }
    clean
}

/// The text of an anvil rename from a client on `protocol`, as the game
/// takes it: without the characters it filters out of names, and `None`
/// when that is still longer than the client's limit, which the game
/// ignores too (a vanilla client never sends one).
pub fn anvil_rename(protocol: i32, name: &str) -> Option<String> {
    let clean: String = name.chars().filter(|c| allowed_character(*c)).collect();
    (clean.encode_utf16().count() <= anvil_text_limit(protocol)).then_some(clean)
}

/// `SharedConstants.isAllowedChatCharacter`.
fn allowed_character(c: char) -> bool {
    c != '§' && c >= ' ' && c != '\u{7f}'
}

/// An anvil `text` a plugin wrote.
fn anvil_text(text: &str, path: &str) -> Result<String, String> {
    if !text.chars().all(allowed_character) {
        return Err(format!("{path} must not contain control characters or '§'"));
    }
    if text.encode_utf16().count() > ANVIL_TEXT_MAX {
        return Err(format!(
            "{path} is longer than an anvil takes ({ANVIL_TEXT_MAX} characters)"
        ));
    }
    Ok(text.to_owned())
}

fn from_json(text: &str, what: &str) -> Result<Value, String> {
    if text.len() > MAX_MENU_BYTES {
        return Err(format!("{what} exceeds {} KiB", MAX_MENU_BYTES / 1024));
    }
    serde_json::from_str(text).map_err(|error| format!("{what}: invalid JSON: {error}"))
}

/// Fail when `value` is larger than `limit` as compact JSON.
fn size(value: &Value, limit: usize, what: &str) -> Result<(), String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| error.to_string())?
        .len();
    if bytes > limit {
        return Err(format!("{what} exceeds {} KiB", limit / 1024));
    }
    Ok(())
}

/// A menu or button `id`: 1 to 64 characters of `a-z`, `0-9`, `_`, `-`,
/// `.` and `/`.
fn check_id(id: &str, path: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id.len() <= MAX_ID
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.' | b'/')
        });
    if valid {
        Ok(())
    } else {
        Err(format!(
            "{path} must be 1 to {MAX_ID} characters of a-z, 0-9, '_', '-', '.' and '/'"
        ))
    }
}

/// The type, from `type`, `rows` and the layout's lines.
fn kind(fields: &Fields<'_>, layout: Option<&[&str]>) -> Result<Kind, String> {
    let name = fields.string("type")?.unwrap_or("chest");
    let kind = match name {
        "chest" => {
            let lines = layout.map(<[&str]>::len);
            let rows = match fields.integer("rows", 1..=i64::from(MAX_ROWS))? {
                Some(rows) => rows as u8,
                None => match lines {
                    Some(lines) if (1..=usize::from(MAX_ROWS)).contains(&lines) => lines as u8,
                    Some(lines) => {
                        return Err(format!(
                            "menu.layout has {lines} lines, but a chest has 1 to {MAX_ROWS} rows"
                        ));
                    }
                    None => DEFAULT_ROWS,
                },
            };
            Kind::Chest { rows }
        }
        "hopper" => Kind::Hopper,
        "dispenser" => Kind::Dispenser,
        "anvil" => Kind::Anvil,
        other => {
            return Err(format!(
                "menu.type: unknown type {other:?} (expected chest, hopper, dispenser or anvil)"
            ));
        }
    };
    if fields.get("rows").is_some() && !matches!(kind, Kind::Chest { .. }) {
        return Err("menu.rows is for chests only".into());
    }
    if let Some(layout) = layout
        && layout.len() != kind.lines()
    {
        return Err(format!(
            "menu.layout has {} lines, but {} has {}",
            layout.len(),
            kind.describe(),
            kind.lines()
        ));
    }
    Ok(kind)
}

/// `keys`: one button per layout character.
fn parse_keys(value: &Value, path: &str) -> Result<BTreeMap<char, Button>, String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("{path} must be an object"))?;
    let mut keys = BTreeMap::new();
    for (key, button) in map {
        let mut characters = key.chars();
        let (Some(character), None) = (characters.next(), characters.next()) else {
            return Err(format!("{path}: {key:?} is not one character"));
        };
        if character == ' ' {
            return Err(format!("{path}: a space is always an empty slot"));
        }
        let button_path = format!("{path}.{key}");
        let fields = Fields::new(button, button_path, BUTTON_FIELDS)?;
        keys.insert(character, parse_button(&fields)?);
    }
    Ok(keys)
}

/// A `slots` entry: the slot numbers, and the button or `None` to empty
/// them.
fn placement(value: &Value, path: &str) -> Result<(Vec<i64>, Option<Button>), String> {
    let allowed: Vec<&str> = BUTTON_FIELDS
        .iter()
        .chain(PLACEMENT_FIELDS)
        .copied()
        .collect();
    let fields = Fields::new(value, path.to_owned(), &allowed)?;
    let slot = |value: &Value, path: &str| {
        value
            .as_i64()
            .ok_or_else(|| format!("{path} must be an integer"))
    };
    let numbers = match (fields.get("slot"), fields.list("slots")?) {
        (Some(_), Some(_)) => return Err(format!("{path}: give slot or slots, not both")),
        (Some(value), None) => vec![slot(value, &fields.path("slot"))?],
        (None, Some(list)) if !list.is_empty() => list
            .into_iter()
            .enumerate()
            .map(|(index, value)| slot(value, &format!("{}[{}]", fields.path("slots"), index + 1)))
            .collect::<Result<_, _>>()?,
        (None, Some(_)) => {
            return Err(format!(
                "{} must list at least one slot",
                fields.path("slots")
            ));
        }
        (None, None) => return Err(format!("{path} needs slot or slots")),
    };
    if fields.boolean("empty")? == Some(true) {
        if let Some(key) = fields
            .map
            .keys()
            .find(|key| !PLACEMENT_FIELDS.contains(&key.as_str()) && !fields.map[*key].is_null())
        {
            return Err(format!("{path}: an empty slot takes no {key:?}"));
        }
        return Ok((numbers, None));
    }
    Ok((numbers, Some(parse_button(&fields)?)))
}

fn parse_button(fields: &Fields<'_>) -> Result<Button, String> {
    let name = fields
        .string("item")?
        .ok_or_else(|| format!("{} is required", fields.path("item")))?;
    let item = rift_translation::codec::item::newest::item_id(name)
        .and_then(rift_translation::codec::item::newest::item_name)
        .filter(|item| *item != "air")
        .ok_or_else(|| format!("{}: unknown item {name:?}", fields.path("item")))?;
    let mut look = Look::of(item);
    if let Some(count) = fields.integer("count", 1..=i64::from(MAX_COUNT))? {
        look.count = count as u8;
    }
    if let Some(name) = fields.get("name") {
        look.name = Some(text(name, &fields.path("name"))?);
    }
    if let Some(lines) = fields.list("lore")? {
        if lines.len() > MAX_LORE_LINES {
            return Err(format!(
                "{}: at most {MAX_LORE_LINES} lines",
                fields.path("lore")
            ));
        }
        look.lore = lines
            .into_iter()
            .enumerate()
            .map(|(index, line)| text(line, &format!("{}[{}]", fields.path("lore"), index + 1)))
            .collect::<Result<_, _>>()?;
    }
    look.glint = fields.boolean("glint")?;
    if let Some(model) = fields.get("model") {
        look.model = Some(parse_model(model, &fields.path("model"))?);
    }
    if let Some(model) = fields.string("item_model")? {
        if model.len() > MAX_STRING || !crate::dialog::identifier(model) {
            return Err(format!(
                "{}: {model:?} is not a valid identifier",
                fields.path("item_model")
            ));
        }
        look.item_model = Some(model.to_owned());
    }
    if let Some(head) = fields.get("head") {
        if item != "player_head" {
            return Err(format!("{} is for player_head only", fields.path("head")));
        }
        look.head = Some(parse_head(head, &fields.path("head"))?);
    }
    if let Some(color) = fields.string("color")? {
        look.color = Some(parse_color(color).ok_or_else(|| {
            format!("{} must be a colour like \"#RRGGBB\"", fields.path("color"))
        })?);
    }
    look.details = fields.boolean("details")?.unwrap_or(false);
    look.tooltip = fields.boolean("tooltip")?.unwrap_or(true);

    let mut action = Action::default();
    if let Some(id) = fields.string("id")? {
        check_id(id, &fields.path("id"))?;
        action.id = Some(id.to_owned());
    }
    if let Some(payload) = fields.get("payload") {
        size(payload, MAX_PAYLOAD_BYTES, &fields.path("payload"))?;
        action.payload = Some(payload.clone());
    }
    if let Some(clicks) = fields.get("clicks") {
        action.clicks = parse_clicks(clicks, &fields.path("clicks"))?;
    }
    action.close = fields.boolean("close")?.unwrap_or(false);
    if action.id.is_none() {
        for key in ["payload", "clicks"] {
            if fields.get(key).is_some() {
                return Err(format!(
                    "{}: a button without an id reports no clicks",
                    fields.path(key)
                ));
            }
        }
    }
    Ok(Button { look, action })
}

fn parse_model(value: &Value, path: &str) -> Result<Model, String> {
    let float = |value: &Value, path: &str| {
        value
            .as_f64()
            .filter(|number| number.is_finite() && number.abs() <= f64::from(f32::MAX))
            .map(|number| number as f32)
            .ok_or_else(|| format!("{path} must be a number"))
    };
    if value.is_number() {
        return Ok(Model {
            floats: vec![float(value, path)?],
            flags: Vec::new(),
            strings: Vec::new(),
            colors: Vec::new(),
        });
    }
    let fields = Fields::new(
        value,
        path.to_owned(),
        &["floats", "flags", "strings", "colors"],
    )?;
    fn each<T>(
        fields: &Fields<'_>,
        key: &str,
        convert: impl Fn(&Value, &str) -> Result<T, String>,
    ) -> Result<Vec<T>, String> {
        let list = fields.list(key)?.unwrap_or_default();
        if list.len() > MAX_MODEL_VALUES {
            return Err(format!(
                "{}: at most {MAX_MODEL_VALUES} values",
                fields.path(key)
            ));
        }
        list.into_iter()
            .enumerate()
            .map(|(index, value)| convert(value, &format!("{}[{}]", fields.path(key), index + 1)))
            .collect()
    }
    Ok(Model {
        floats: each(&fields, "floats", float)?,
        flags: each(&fields, "flags", |value, path| {
            value
                .as_bool()
                .ok_or_else(|| format!("{path} must be a boolean"))
        })?,
        strings: each(&fields, "strings", |value, path| {
            value
                .as_str()
                .filter(|text| text.len() <= MAX_STRING)
                .map(str::to_owned)
                .ok_or_else(|| format!("{path} must be a string of at most {MAX_STRING} bytes"))
        })?,
        colors: each(&fields, "colors", |value, path| {
            let rgb = match value {
                Value::String(text) => parse_color(text),
                value => value
                    .as_u64()
                    .and_then(|number| u32::try_from(number).ok())
                    .filter(|number| *number <= 0xff_ffff),
            };
            rgb.map(|rgb| rgb as i32).ok_or_else(|| {
                format!("{path} must be a colour like \"#RRGGBB\" or a number up to 0xFFFFFF")
            })
        })?,
    })
}

fn parse_head(value: &Value, path: &str) -> Result<Head, String> {
    let fields = Fields::new(value, path.to_owned(), &["texture", "signature", "player"])?;
    match (fields.string("texture")?, fields.string("player")?) {
        (Some(texture), None) => {
            if texture.is_empty() || texture.len() > MAX_TEXTURE {
                return Err(format!(
                    "{} must contain 1 to {MAX_TEXTURE} bytes",
                    fields.path("texture")
                ));
            }
            let signature = fields.string("signature")?;
            if signature.is_some_and(|signature| signature.len() > MAX_STRING) {
                return Err(format!(
                    "{} must contain at most {MAX_STRING} bytes",
                    fields.path("signature")
                ));
            }
            Ok(Head::Texture(Textures {
                value: texture.to_owned(),
                signature: signature.map(str::to_owned),
            }))
        }
        (None, Some(player)) => {
            let valid = (1..=36).contains(&player.len())
                && player
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            if !valid {
                return Err(format!(
                    "{} must be a player name or UUID",
                    fields.path("player")
                ));
            }
            if fields.get("signature").is_some() {
                return Err(format!(
                    "{}: a signature goes with a texture",
                    fields.path("signature")
                ));
            }
            Ok(Head::Player(player.to_owned()))
        }
        _ => Err(format!("{path} needs either texture or player")),
    }
}

fn parse_clicks(value: &Value, path: &str) -> Result<Clicks, String> {
    let names = || {
        Click::ALL
            .iter()
            .map(|click| click.name())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let list = match value {
        Value::String(all) if all == "all" => return Ok(Clicks::ALL),
        Value::Array(list) => list.iter().collect::<Vec<_>>(),
        Value::Object(map) if map.is_empty() => Vec::new(),
        _ => return Err(format!("{path} must be a list of click kinds or \"all\"")),
    };
    let mut clicks = Clicks::NONE;
    for (index, name) in list.into_iter().enumerate() {
        let click = name.as_str().and_then(Click::from_name).ok_or_else(|| {
            format!(
                "{path}[{}]: unknown click kind {name} (expected one of {})",
                index + 1,
                names()
            )
        })?;
        clicks = clicks.with(click);
    }
    Ok(clicks)
}

/// `"#RRGGBB"` as RGB.
fn parse_color(text: &str) -> Option<u32> {
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// A name, lore line or title: a text component, with markup objects
/// expanded and checked like every text Rift sends (see
/// [`Text::component`]). An empty string stays empty.
fn text(value: &Value, path: &str) -> Result<Text, String> {
    if value.as_str() == Some("") {
        return Ok(Text::Plain(String::new()));
    }
    Text::component(value.clone(), path)
}

/// The fields of one object, checked for unknown keys up front.
struct Fields<'a> {
    map: &'a Map<String, Value>,
    path: String,
}

impl<'a> Fields<'a> {
    fn new(value: &'a Value, path: String, allowed: &[&str]) -> Result<Self, String> {
        let map = value
            .as_object()
            .ok_or_else(|| format!("{path} must be an object"))?;
        if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(format!(
                "{path}: unknown field {key:?} (expected one of {})",
                allowed.join(", ")
            ));
        }
        Ok(Self { map, path })
    }

    /// A field's value; `null` counts as absent.
    fn get(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key).filter(|value| !value.is_null())
    }

    fn path(&self, key: &str) -> String {
        format!("{}.{key}", self.path)
    }

    fn string(&self, key: &str) -> Result<Option<&'a str>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(format!("{} must be a string", self.path(key))),
        }
    }

    fn boolean(&self, key: &str) -> Result<Option<bool>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(_) => Err(format!("{} must be a boolean", self.path(key))),
        }
    }

    fn integer(
        &self,
        key: &str,
        range: std::ops::RangeInclusive<i64>,
    ) -> Result<Option<i64>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(value) => value
                .as_i64()
                .filter(|number| range.contains(number))
                .map(Some)
                .ok_or_else(|| {
                    format!(
                        "{} must be an integer from {} to {}",
                        self.path(key),
                        range.start(),
                        range.end()
                    )
                }),
        }
    }

    /// A list, allowing `{}` for an empty one (an empty Lua table).
    fn list(&self, key: &str) -> Result<Option<Vec<&'a Value>>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Array(items)) => Ok(Some(items.iter().collect())),
            Some(Value::Object(map)) if map.is_empty() => Ok(Some(Vec::new())),
            Some(_) => Err(format!("{} must be a list", self.path(key))),
        }
    }

    /// A title: `None` when it is empty.
    fn title(&self, key: &str) -> Result<Option<Text>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(value) => {
                let title = text(value, &self.path(key))?;
                Ok((title != Text::Plain(String::new())).then_some(title))
            }
        }
    }

    /// A `state`: any JSON value of at most 16 KiB.
    fn state(&self, key: &str) -> Result<Option<Value>, String> {
        let Some(state) = self.get(key) else {
            return Ok(None);
        };
        size(state, MAX_STATE_BYTES, &self.path(key))?;
        Ok(Some(state.clone()))
    }
}

/// A button's item as its name in the newest protocol: a menu from another
/// peer names an item this build knows, or is refused.
mod item_name {
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(item: &&'static str, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(item)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<&'static str, D::Error> {
        let name = String::deserialize(deserializer)?;
        rift_translation::codec::item::newest::item_id(&name)
            .and_then(rift_translation::codec::item::newest::item_name)
            .ok_or_else(|| D::Error::custom(format!("unknown item {name:?}")))
    }
}

/// Texts as their variant: untagged, a component that is a JSON string
/// would come back as plain text, which a stack spells differently.
mod exact_text {
    use crate::text::Text;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;

    #[derive(Serialize, Deserialize)]
    enum Exact {
        Plain(String),
        Component(Value),
    }

    impl From<&Text> for Exact {
        fn from(text: &Text) -> Self {
            match text {
                Text::Plain(text) => Self::Plain(text.clone()),
                Text::Component(value) => Self::Component(value.clone()),
            }
        }
    }

    impl From<Exact> for Text {
        fn from(text: Exact) -> Self {
            match text {
                Exact::Plain(text) => Self::Plain(text),
                Exact::Component(value) => Self::Component(value),
            }
        }
    }

    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(
            text: &Option<Text>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            text.as_ref().map(Exact::from).serialize(serializer)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Text>, D::Error> {
            Ok(Option::<Exact>::deserialize(deserializer)?.map(Text::from))
        }
    }

    pub mod list {
        use super::*;

        pub fn serialize<S: Serializer>(texts: &[Text], serializer: S) -> Result<S::Ok, S::Error> {
            texts
                .iter()
                .map(Exact::from)
                .collect::<Vec<_>>()
                .serialize(serializer)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Vec<Text>, D::Error> {
            Ok(Vec::<Exact>::deserialize(deserializer)?
                .into_iter()
                .map(Text::from)
                .collect())
        }
    }

    /// A patch's title: absent, emptied or set. Nested options would lose
    /// "emptied" in JSON.
    pub mod patch {
        use super::*;

        #[derive(Serialize, Deserialize)]
        enum Title {
            Keep,
            Empty,
            Set(Exact),
        }

        pub fn serialize<S: Serializer>(
            title: &Option<Option<Text>>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            match title {
                None => Title::Keep,
                Some(None) => Title::Empty,
                Some(Some(text)) => Title::Set(text.into()),
            }
            .serialize(serializer)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Option<Text>>, D::Error> {
            Ok(match Title::deserialize(deserializer)? {
                Title::Keep => None,
                Title::Empty => Some(None),
                Title::Set(text) => Some(Some(text.into())),
            })
        }
    }
}
