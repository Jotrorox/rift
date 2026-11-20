//! Inventory menus (Minecraft 1.12 and newer): the packets Rift writes to
//! show its own container screens, the client packets for them that Rift
//! takes out of the stream, the backend packets that end them, and the
//! request that makes a backend send the player's inventory again. See
//! docs/menus.md, "How the proxy does it".
//!
//! Packet IDs come from the translation crate's per-protocol tables, and
//! the bodies from its window codecs where it has them. Menu packets are
//! written in the client's protocol; the inventory request, its rejection
//! and the acknowledgement in the backend's.
use super::{Packet, ProtocolVersion, invalid};
use rift_translation::{
    Direction, State,
    codec::item::{
        self, ItemStack,
        window::{self, ClickSlot},
    },
    data,
    wire::{Reader, Writer},
};
use std::io;

/// The oldest protocol with menus: 1.12.
pub const OLDEST: i32 = 335;
/// Rift's container IDs. Vanilla servers use 1 to 100; 127 is the largest
/// ID that stays positive where a container is a signed byte.
pub const FIRST_CONTAINER: i32 = 101;
pub const LAST_CONTAINER: i32 = 127;
/// `open_screen` names a `menu` registry entry and its container ID is a
/// VarInt from 1.14.
const MENUS: i32 = 477;
/// Container IDs are VarInts everywhere from 1.21.2, and the cursor has a
/// packet of its own.
const WIDE_CONTAINERS: i32 = 768;
/// The player's own inventory menu.
const INVENTORY: i32 = 0;
/// Slots of the player's inventory menu from 1.9: crafting result, 4
/// crafting slots, 4 armour slots, 27 main slots, 9 hotbar slots, off hand.
const INVENTORY_SLOTS: i16 = 46;
/// What a click names for the cursor, and in `container_set_slot` before
/// 1.21.2 the container and slot that are the cursor.
const CURSOR: i16 = -1;
/// A click between the slots of a window, which does nothing.
const BETWEEN_SLOTS: i16 = -1;
/// No server counts its state IDs this low, so a click with it is always
/// answered with the whole inventory (1.17.1 and newer backends).
const WRONG_STATE_ID: i32 = -1;
/// The action number of Rift's request to a backend up to 1.16.5.
const REQUEST_ACTION: i16 = -1;
/// The stack Rift's request claims: 99 stone (item 1 in every protocol),
/// which no slot can hold.
const CLAIMED_ITEM: i32 = 1;
const CLAIMED_COUNT: i32 = 99;
/// The button of F in a swap click.
const OFFHAND_BUTTON: i8 = 40;
/// Number keys 1 to 9 are swap buttons 0 to 8.
const HOTBAR_KEYS: i8 = 9;

/// Whether a player on `protocol` can be shown menus.
pub fn supported(protocol: i32) -> bool {
    protocol >= OLDEST && rift_translation::version::index(protocol).is_some()
}

/// The packet IDs of one protocol that menus look at, in play. A packet the
/// protocol lacks has the ID -1, which no packet has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ids {
    protocol: i32,
    open_screen: i32,
    mount_screen: i32,
    sign_editor: i32,
    open_book: i32,
    close: i32,
    set_slot: i32,
    set_content: i32,
    set_cursor: i32,
    respawn: i32,
    login: i32,
    click: i32,
    client_close: i32,
    creative: i32,
    button: i32,
    slot_state: i32,
}

impl Ids {
    pub(crate) fn of(protocol: i32) -> Option<Self> {
        if !supported(protocol) {
            return None;
        }
        let version = data::try_version(protocol)?;
        let clientbound = |name| {
            version
                .packet_id(State::Play, Direction::Clientbound, name)
                .unwrap_or(-1)
        };
        let serverbound = |name| {
            version
                .packet_id(State::Play, Direction::Serverbound, name)
                .unwrap_or(-1)
        };
        Some(Self {
            protocol,
            open_screen: clientbound("open_screen"),
            mount_screen: clientbound("mount_screen_open"),
            sign_editor: clientbound("open_sign_editor"),
            open_book: clientbound("open_book"),
            close: clientbound("container_close"),
            set_slot: clientbound("container_set_slot"),
            set_content: clientbound("container_set_content"),
            set_cursor: clientbound("set_cursor_item"),
            respawn: clientbound("respawn"),
            login: clientbound("login"),
            click: serverbound("container_click"),
            client_close: serverbound("container_close"),
            creative: serverbound("set_creative_mode_slot"),
            button: serverbound("container_button_click"),
            slot_state: serverbound("container_slot_state_changed"),
        })
    }

    fn of_version(version: ProtocolVersion) -> io::Result<Self> {
        Self::of(version.number()).ok_or_else(unsupported)
    }
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "menus need Minecraft 1.12 or newer",
    )
}

fn wide(protocol: i32) -> bool {
    protocol >= WIDE_CONTAINERS
}

fn container_byte(id: i32) -> io::Result<u8> {
    u8::try_from(id).map_err(|_| invalid("container ID out of range"))
}

/// A container ID as `container_close` and `container_click` carry it.
fn write_container(protocol: i32, w: &mut Writer, id: i32) -> io::Result<()> {
    if wide(protocol) {
        w.varint(id);
    } else {
        w.u8(container_byte(id)?);
    }
    Ok(())
}

fn read_container(protocol: i32, r: &mut Reader<'_>) -> io::Result<i32> {
    Ok(if wide(protocol) {
        r.varint()?
    } else {
        i32::from(r.u8()?)
    })
}

/// `open_screen` for Rift's container `container`: a window type and slot
/// count before 1.14, a `menu` registry ID from 1.14; the title in the
/// client's text format.
pub(crate) fn open_screen(
    version: ProtocolVersion,
    container: i32,
    kind: crate::menu::Kind,
    title: Option<&crate::text::Text>,
) -> io::Result<Packet> {
    use rift_translation::codec::misc::screen::MenuType;
    let ids = Ids::of_version(version)?;
    let menu = kind
        .menu_type(version.number())
        .ok_or_else(|| invalid("the client lacks the menu"))?;
    let mut w = Writer::new();
    match menu {
        MenuType::Window { kind, .. } => {
            w.u8(container_byte(container)?).string(kind);
        }
        MenuType::Registry(id) => {
            w.varint(container).varint(id);
        }
    }
    let mut data = w.into_inner();
    let nbt = version.nbt_components();
    match title {
        Some(title) => super::packets::with_text(title, Some(version), |text| {
            text.write(nbt, &mut data)
        })?,
        None => super::packets::Text::Literal("").write(nbt, &mut data)?,
    }
    if let MenuType::Window { slots, .. } = menu {
        data.push(slots);
    }
    Ok(Packet::new(ids.open_screen, data))
}

/// `container_set_slot` with `stack`, already encoded for the client (see
/// `crate::menu::Menu::slot_stack`).
pub(crate) fn set_slot(
    version: ProtocolVersion,
    container: i32,
    state_id: i32,
    slot: i16,
    stack: &[u8],
) -> io::Result<Packet> {
    let protocol = version.number();
    let ids = Ids::of_version(version)?;
    let mut w = Writer::new();
    if wide(protocol) {
        w.varint(container);
    } else {
        w.i8(i8::try_from(container).map_err(|_| invalid("container ID out of range"))?);
    }
    if window::has_state_ids(protocol) {
        w.varint(state_id);
    }
    w.i16(slot).bytes(stack);
    Ok(Packet::new(ids.set_slot, w.into_inner()))
}

/// What empties the cursor while a menu is open: `container_set_slot` for
/// container -1 up to 1.21.1, `set_cursor_item` from 1.21.2, which ignores
/// the older form.
pub(crate) fn empty_cursor(version: ProtocolVersion, state_id: i32) -> io::Result<Packet> {
    let protocol = version.number();
    let ids = Ids::of_version(version)?;
    let mut empty = Writer::new();
    item::write(protocol, None, &mut empty)?;
    if wide(protocol) {
        return Ok(Packet::new(ids.set_cursor, empty.into_inner()));
    }
    set_slot(version, i32::from(CURSOR), state_id, CURSOR, &empty.buf)
}

/// `container_close` for the client: it closes whatever screen is open.
pub(crate) fn close(version: ProtocolVersion, container: i32) -> io::Result<Packet> {
    let ids = Ids::of_version(version)?;
    let mut w = Writer::new();
    write_container(version.number(), &mut w, container)?;
    Ok(Packet::new(ids.close, w.into_inner()))
}

/// The `container_close` a client sends for `container`, in its protocol:
/// what the backend would have heard had the player pressed Escape.
pub(crate) fn client_close(version: ProtocolVersion, container: i32) -> io::Result<Packet> {
    let ids = Ids::of_version(version)?;
    let mut w = Writer::new();
    write_container(version.number(), &mut w, container)?;
    Ok(Packet::new(ids.client_close, w.into_inner()))
}

/// A client packet that names a container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClientPacket {
    Click,
    Close,
    /// `container_button_click` and `container_slot_state_changed`, which
    /// Rift's menus have no use for.
    Other,
}

/// The container a serverbound packet names, if it is one that names one;
/// nothing else of the packet is read.
pub(crate) fn client_container(
    ids: &Ids,
    packet: &Packet,
) -> io::Result<Option<(ClientPacket, i32)>> {
    let protocol = ids.protocol;
    let mut r = Reader::new(&packet.data);
    let found = if packet.id == ids.click {
        (ClientPacket::Click, read_container(protocol, &mut r)?)
    } else if packet.id == ids.client_close {
        (ClientPacket::Close, read_container(protocol, &mut r)?)
    } else if packet.id == ids.button {
        (ClientPacket::Other, read_container(protocol, &mut r)?)
    } else if packet.id == ids.slot_state {
        r.varint()?;
        (ClientPacket::Other, r.varint()?)
    } else {
        return Ok(None);
    };
    Ok(Some(found))
}

/// Whether a serverbound packet is `set_creative_mode_slot`.
pub(crate) fn creative(ids: &Ids, packet: &Packet) -> bool {
    packet.id == ids.creative
}

/// A `container_click`, whatever the client's protocol. Rift reads the slot,
/// the button, the mode, the state ID and the numbers of the changed slots;
/// the stacks the client sends are only skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Click {
    pub container: i32,
    pub slot: i16,
    pub button: i8,
    pub mode: i32,
    /// From 1.17.1: the state of the container the client saw.
    pub state_id: Option<i32>,
    /// From 1.17: the slots the client changed on its side.
    pub changed: Option<Vec<i16>>,
}

impl Click {
    pub(crate) fn read(ids: &Ids, packet: &Packet) -> io::Result<Self> {
        let protocol = ids.protocol;
        if packet.id != ids.click {
            return Err(invalid("not a container click"));
        }
        let mut r = Reader::new(&packet.data);
        let click = window::Click::read(protocol, &mut r)?;
        r.finish()?;
        Ok(Self {
            container: click.container,
            slot: click.slot,
            button: click.button,
            mode: click.mode,
            state_id: window::has_state_ids(protocol).then_some(click.state_id),
            changed: window::has_changed_slots(protocol)
                .then(|| click.changed.iter().map(|(slot, _)| *slot).collect()),
        })
    }

    /// The click kind a menu reports, with the number key for
    /// `number_key`. `None` for drags, double-click collection and clicks
    /// outside the slots, which are never reported.
    pub(crate) fn kind(&self) -> Option<(crate::menu::Click, Option<u8>)> {
        use crate::menu::Click as Kind;
        if self.slot < 0 {
            return None;
        }
        let kind = match (self.mode, self.button) {
            (0, 0) => Kind::Left,
            (0, 1) => Kind::Right,
            (1, 0) => Kind::ShiftLeft,
            (1, 1) => Kind::ShiftRight,
            (2, key @ 0..HOTBAR_KEYS) => return Some((Kind::NumberKey, Some(key as u8 + 1))),
            (2, OFFHAND_BUTTON) => Kind::Offhand,
            (3, _) => Kind::Middle,
            (4, 0) => Kind::Drop,
            (4, 1) => Kind::DropAll,
            _ => return None,
        };
        Some((kind, None))
    }

    /// A drag (mode 5) or a double click's collection (mode 6), which change
    /// every slot holding the same item.
    pub(crate) fn spreads(&self) -> bool {
        matches!(self.mode, 5 | 6)
    }

    /// Whether the click may have changed the player's inventory rows on the
    /// client. `menu_slots` is the number of the menu's own slots; the
    /// inventory rows follow them.
    pub(crate) fn touches_inventory(&self, menu_slots: usize) -> bool {
        let rows = |slot: i16| usize::try_from(slot).is_ok_and(|slot| slot >= menu_slots);
        // F swaps with the off hand, which is not one of the window's slots
        // and so never in the list of changed slots.
        if self.mode == 2 && self.button == OFFHAND_BUTTON {
            return true;
        }
        match &self.changed {
            Some(changed) => changed.iter().any(|slot| rows(*slot)),
            None => rows(self.slot) || matches!(self.mode, 1 | 2 | 5 | 6),
        }
    }

    /// The menu slots the click may have changed on the client.
    pub(crate) fn menu_slots(&self, menu_slots: usize) -> Vec<usize> {
        let ours = |slot: i16| usize::try_from(slot).ok().filter(|slot| *slot < menu_slots);
        if self.spreads() && self.changed.is_none() {
            return (0..menu_slots).collect();
        }
        let mut slots: Vec<usize> = ours(self.slot).into_iter().collect();
        for slot in self.changed.iter().flatten().filter_map(|slot| ours(*slot)) {
            if !slots.contains(&slot) {
                slots.push(slot);
            }
        }
        slots
    }

    /// Whether the slot clicked is outside the window, between its slots or
    /// in the inventory rows rather than one of the menu's own.
    pub(crate) fn on_menu(&self, menu_slots: usize) -> bool {
        usize::try_from(self.slot).is_ok_and(|slot| slot < menu_slots)
    }
}

/// What a clientbound backend packet, in the client's protocol, does to the
/// screens a menu cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackendPacket {
    /// The backend opened a container of its own.
    Opened(i32),
    /// The backend opened a screen without a container: a sign or a book.
    Screen,
    /// `container_close`, which closes whatever the client has open.
    Closed,
    /// A respawn or a new login: a new world.
    World,
    /// One slot of the player's inventory menu.
    InventorySlot,
    /// The whole inventory menu.
    InventoryContent,
}

pub(crate) fn backend_packet(ids: &Ids, packet: &Packet) -> Option<BackendPacket> {
    let protocol = ids.protocol;
    let id = packet.id;
    let mut r = Reader::new(&packet.data);
    Some(if id == ids.open_screen {
        let container = if protocol >= MENUS {
            r.varint().ok()
        } else {
            r.u8().ok().map(i32::from)
        };
        container.map_or(BackendPacket::Screen, BackendPacket::Opened)
    } else if id == ids.mount_screen {
        read_container(protocol, &mut r)
            .ok()
            .map_or(BackendPacket::Screen, BackendPacket::Opened)
    } else if id == ids.sign_editor || id == ids.open_book {
        BackendPacket::Screen
    } else if id == ids.close {
        BackendPacket::Closed
    } else if id == ids.respawn || id == ids.login {
        BackendPacket::World
    } else if id == ids.set_slot {
        let container = if wide(protocol) {
            r.varint().ok()?
        } else {
            i32::from(r.i8().ok()?)
        };
        if container != INVENTORY {
            return None;
        }
        BackendPacket::InventorySlot
    } else if id == ids.set_content {
        if read_container(protocol, &mut r).ok()? != INVENTORY {
            return None;
        }
        BackendPacket::InventoryContent
    } else {
        return None;
    })
}

/// How a backend is asked for the player's inventory (docs/menus.md, "The
/// inventory rows").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recipe {
    /// 1.17.1 and newer: a click with a state ID no server expects. One
    /// `container_set_content` answers.
    StateId,
    /// 1.12 to 1.16.5: a click that claims to pick up a stack. The backend
    /// rejects it, which must be acknowledged, and sends the inventory.
    Transaction,
    /// 1.17: a click that claims every slot changed. The backend answers
    /// slot by slot, which a client with a menu open partly drops, so Rift
    /// asks only when the menu closes.
    ClaimAll,
}

pub(crate) fn recipe(protocol: i32) -> Recipe {
    if window::has_state_ids(protocol) {
        Recipe::StateId
    } else if window::has_changed_slots(protocol) {
        Recipe::ClaimAll
    } else {
        Recipe::Transaction
    }
}

fn claimed() -> ItemStack {
    ItemStack {
        id: CLAIMED_ITEM,
        count: CLAIMED_COUNT,
        damage: 0,
        tag: None,
        components: None,
    }
}

/// The `container_click` that asks a backend on `protocol` to send the
/// player's inventory again, in that protocol. It names container 0 and the
/// space between its slots, so it moves nothing on any server.
pub(crate) fn inventory_request(protocol: i32) -> io::Result<Packet> {
    let id = data::try_version(protocol)
        .and_then(|version| version.packet_id(State::Play, Direction::Serverbound, "container_click"))
        .ok_or_else(unsupported)?;
    let empty = if window::hashed_clicks(protocol) {
        ClickSlot::Hashed(None)
    } else {
        ClickSlot::Full(None)
    };
    let mut click = window::Click {
        container: INVENTORY,
        state_id: WRONG_STATE_ID,
        slot: BETWEEN_SLOTS,
        button: 0,
        action: 0,
        mode: 0,
        clicked: None,
        changed: Vec::new(),
        carried: empty,
    };
    match recipe(protocol) {
        Recipe::StateId => {}
        Recipe::Transaction => {
            click.action = REQUEST_ACTION;
            click.clicked = Some(claimed());
        }
        Recipe::ClaimAll => {
            click.changed = (0..INVENTORY_SLOTS)
                .map(|slot| (slot, ClickSlot::Full(Some(claimed()))))
                .collect();
            click.carried = ClickSlot::Full(Some(claimed()));
        }
    }
    let mut w = Writer::new();
    click.write(protocol, &mut w)?;
    Ok(Packet::new(id, w.into_inner()))
}

fn transaction_id(protocol: i32, direction: Direction) -> Option<i32> {
    data::try_version(protocol)?.packet_id(State::Play, direction, "container_ack")
}

/// Whether a clientbound packet of a backend on `protocol` (up to 1.16.5) is
/// its rejection of Rift's inventory request: `container_ack` for container
/// 0 with Rift's action number, not accepted.
pub(crate) fn is_rejection(protocol: i32, packet: &Packet) -> bool {
    if transaction_id(protocol, Direction::Clientbound) != Some(packet.id) {
        return false;
    }
    let mut r = Reader::new(&packet.data);
    let fields = (|| Some((r.i8().ok()?, r.i16().ok()?, r.u8().ok()?)))();
    r.is_empty() && fields == Some((INVENTORY as i8, REQUEST_ACTION, 0))
}

/// What a client answers to that rejection, so the backend takes clicks
/// again: `container_ack` for container 0 and Rift's action, accepted.
pub(crate) fn acknowledgement(protocol: i32) -> io::Result<Packet> {
    let id = transaction_id(protocol, Direction::Serverbound).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "transactions ended with Minecraft 1.17",
        )
    })?;
    let mut w = Writer::new();
    w.i8(INVENTORY as i8).i16(REQUEST_ACTION).bool(true);
    Ok(Packet::new(id, w.into_inner()))
}

#[cfg(test)]
mod tests;
