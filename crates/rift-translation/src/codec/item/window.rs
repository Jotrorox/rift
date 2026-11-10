//! Inventory packets: container contents and slots, the cursor, equipment,
//! cooldowns, creative slots, clicks and book edits, in every protocol's
//! layout.
use super::{ItemStack, Stack, components, stack};
use crate::wire::{Reader, Writer, invalid};
use std::io;

/// Container IDs are a VarInt from 1.21.2 on, a byte before.
fn varint_containers(protocol: i32) -> bool {
    protocol >= 768
}

/// State IDs synchronise clicks with container updates from 1.17.1 on.
pub fn has_state_ids(protocol: i32) -> bool {
    protocol >= 756
}

/// Clicks list the slots they changed instead of an action number from 1.17.
pub fn has_changed_slots(protocol: i32) -> bool {
    protocol >= 755
}

/// Clicks carry hashed stacks from 1.21.5 on.
pub fn hashed_clicks(protocol: i32) -> bool {
    protocol >= 770
}

/// Creative clients length-prefix every component from 1.21.5 on.
pub fn untrusted_creative(protocol: i32) -> bool {
    protocol >= 770
}

fn read_container(protocol: i32, r: &mut Reader<'_>, signed: bool) -> io::Result<i32> {
    Ok(if varint_containers(protocol) {
        r.varint()?
    } else if signed {
        i32::from(r.i8()?)
    } else {
        i32::from(r.u8()?)
    })
}

fn write_container(protocol: i32, w: &mut Writer, id: i32, signed: bool) -> io::Result<()> {
    if varint_containers(protocol) {
        w.varint(id);
    } else if signed {
        w.i8(i8::try_from(id).map_err(|_| invalid("container ID out of range"))?);
    } else {
        w.u8(u8::try_from(id).map_err(|_| invalid("container ID out of range"))?);
    }
    Ok(())
}

pub fn read_stack(protocol: i32, r: &mut Reader<'_>) -> io::Result<Stack> {
    stack::read(protocol, r)
}

pub fn write_stack(protocol: i32, w: &mut Writer, stack: &Stack) -> io::Result<()> {
    stack::write(protocol, stack.as_ref(), w)
}

/// `container_set_content`.
#[derive(Clone, Debug, PartialEq)]
pub struct SetContent {
    pub container: i32,
    /// 1.17.1 on.
    pub state_id: i32,
    pub items: Vec<Stack>,
    /// The cursor, 1.17.1 on.
    pub carried: Stack,
}

impl SetContent {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let container = read_container(protocol, r, false)?;
        let state_id = if has_state_ids(protocol) {
            r.varint()?
        } else {
            0
        };
        let count = if has_state_ids(protocol) {
            r.count()?
        } else {
            let count = r.i16()?;
            usize::try_from(count)
                .ok()
                .filter(|count| *count <= r.remaining())
                .ok_or_else(|| invalid("invalid slot count"))?
        };
        let items = (0..count)
            .map(|_| read_stack(protocol, r))
            .collect::<io::Result<_>>()?;
        let carried = if has_state_ids(protocol) {
            read_stack(protocol, r)?
        } else {
            None
        };
        Ok(Self {
            container,
            state_id,
            items,
            carried,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        write_container(protocol, w, self.container, false)?;
        if has_state_ids(protocol) {
            w.varint(self.state_id).varint(self.items.len() as i32);
        } else {
            w.i16(i16::try_from(self.items.len()).map_err(|_| invalid("too many slots"))?);
        }
        for item in &self.items {
            write_stack(protocol, w, item)?;
        }
        if has_state_ids(protocol) {
            write_stack(protocol, w, &self.carried)?;
        }
        Ok(())
    }
}

/// `container_set_slot`. Before 1.21.2 container -1 (slot -1) is the cursor
/// and -2 the player's inventory.
#[derive(Clone, Debug, PartialEq)]
pub struct SetSlot {
    pub container: i32,
    pub state_id: i32,
    pub slot: i16,
    pub item: Stack,
}

impl SetSlot {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let container = read_container(protocol, r, true)?;
        let state_id = if has_state_ids(protocol) {
            r.varint()?
        } else {
            0
        };
        Ok(Self {
            container,
            state_id,
            slot: r.i16()?,
            item: read_stack(protocol, r)?,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        write_container(protocol, w, self.container, true)?;
        if has_state_ids(protocol) {
            w.varint(self.state_id);
        }
        w.i16(self.slot);
        write_stack(protocol, w, &self.item)
    }
}

/// `set_player_inventory` (1.21.2 on): one slot of the player's inventory.
#[derive(Clone, Debug, PartialEq)]
pub struct SetPlayerInventory {
    pub slot: i32,
    pub item: Stack,
}

impl SetPlayerInventory {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            slot: r.varint()?,
            item: read_stack(protocol, r)?,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        w.varint(self.slot);
        write_stack(protocol, w, &self.item)
    }
}

/// Equipment slots as `set_equipment` numbers them. Before 1.16 one packet
/// carries one slot (`set_equipped_item`).
#[derive(Clone, Debug, PartialEq)]
pub struct SetEquipment {
    pub entity: i32,
    pub slots: Vec<(i8, Stack)>,
}

/// Equipment travels as a list from 1.16 on.
pub fn equipment_list(protocol: i32) -> bool {
    protocol >= 735
}

/// The equipment slots a protocol knows: main hand, off hand, feet, legs,
/// chest and head; the body from 1.20.5 (wolf armour) and the saddle from
/// 1.21.5.
pub fn equipment_slots(protocol: i32) -> i8 {
    if protocol >= 770 {
        8
    } else if protocol >= 766 {
        7
    } else {
        6
    }
}

impl SetEquipment {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let entity = r.varint()?;
        let mut slots = Vec::new();
        if !equipment_list(protocol) {
            let slot = r.varint()?;
            let slot = i8::try_from(slot).map_err(|_| invalid("invalid equipment slot"))?;
            slots.push((slot, read_stack(protocol, r)?));
        } else {
            loop {
                let slot = r.i8()?;
                slots.push((slot & 0x7f, read_stack(protocol, r)?));
                if slot & -0x80 == 0 {
                    break;
                }
            }
        }
        if slots
            .iter()
            .any(|(slot, _)| !(0..equipment_slots(protocol)).contains(slot))
        {
            return Err(invalid("invalid equipment slot"));
        }
        Ok(Self { entity, slots })
    }

    /// Before 1.16 there must be exactly one slot; see [`Self::split`].
    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        w.varint(self.entity);
        if !equipment_list(protocol) {
            let [(slot, item)] = self.slots.as_slice() else {
                return Err(invalid("one equipment slot per packet before 1.16"));
            };
            w.varint(i32::from(*slot));
            return write_stack(protocol, w, item);
        }
        if self.slots.is_empty() {
            return Err(invalid("equipment packet without slots"));
        }
        for (index, (slot, item)) in self.slots.iter().enumerate() {
            let more = if index + 1 < self.slots.len() {
                -0x80
            } else {
                0
            };
            w.i8(slot & 0x7f | more);
            write_stack(protocol, w, item)?;
        }
        Ok(())
    }

    /// One packet per slot, for protocols before 1.16.
    pub fn split(self) -> Vec<SetEquipment> {
        let entity = self.entity;
        self.slots
            .into_iter()
            .map(|slot| SetEquipment {
                entity,
                slots: vec![slot],
            })
            .collect()
    }
}

/// `cooldown`: an item ID before 1.21.2, a cooldown group identifier after.
#[derive(Clone, Debug, PartialEq)]
pub enum CooldownKey {
    Item(i32),
    Group(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cooldown {
    pub key: CooldownKey,
    pub ticks: i32,
}

pub fn cooldown_groups(protocol: i32) -> bool {
    protocol >= 768
}

impl Cooldown {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let key = if cooldown_groups(protocol) {
            CooldownKey::Group(r.str()?.to_owned())
        } else {
            CooldownKey::Item(r.varint()?)
        };
        Ok(Self {
            key,
            ticks: r.varint()?,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        match (&self.key, cooldown_groups(protocol)) {
            (CooldownKey::Group(group), true) => {
                w.string(group);
            }
            (CooldownKey::Item(item), false) => {
                w.varint(*item);
            }
            _ => return Err(invalid("cooldown key does not fit the protocol")),
        }
        w.varint(self.ticks);
        Ok(())
    }
}

/// `set_creative_mode_slot`.
#[derive(Clone, Debug, PartialEq)]
pub struct CreativeSlot {
    pub slot: i16,
    pub item: Stack,
}

impl CreativeSlot {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let slot = r.i16()?;
        let item = if untrusted_creative(protocol) {
            read_untrusted(protocol, r)?
        } else {
            read_stack(protocol, r)?
        };
        Ok(Self { slot, item })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        w.i16(self.slot);
        if untrusted_creative(protocol) {
            write_untrusted(protocol, w, &self.item)
        } else {
            write_stack(protocol, w, &self.item)
        }
    }
}

/// A creative client's stack (1.21.5 on): each added component is prefixed
/// with its length. Read by removing the prefixes and handing the trusted
/// layout to the component codec.
fn read_untrusted(protocol: i32, r: &mut Reader<'_>) -> io::Result<Stack> {
    let count = r.varint()?;
    if count == 0 {
        return Ok(None);
    }
    if count < 0 {
        return Err(invalid("negative item count"));
    }
    let id = r.varint()?;
    let added = r.count()?;
    let removed = r.count()?;
    let mut trusted = Writer::new();
    trusted.varint(added as i32).varint(removed as i32);
    for _ in 0..added {
        trusted.varint(r.varint()?);
        trusted.bytes(r.byte_array()?);
    }
    for _ in 0..removed {
        trusted.varint(r.varint()?);
    }
    let mut inner = Reader::new(&trusted.buf);
    let components = components::read(protocol, &mut inner)?;
    inner.finish()?;
    Ok(Some(ItemStack {
        id,
        count,
        damage: 0,
        tag: None,
        components: Some(components),
    }))
}

fn write_untrusted(protocol: i32, w: &mut Writer, stack: &Stack) -> io::Result<()> {
    let Some(stack) = stack else {
        w.varint(0);
        return Ok(());
    };
    if stack.tag.is_some() {
        return Err(invalid(
            "an NBT item tag in a protocol with data components",
        ));
    }
    w.varint(stack.count.max(1)).varint(stack.id);
    components::write_delimited(
        protocol,
        stack.components.as_ref().unwrap_or(&Default::default()),
        w,
    )
}

/// A stack a click reports: the full stack before 1.21.5, a hashed one
/// after (item, count, and per component type a CRC32C hash of its value).
#[derive(Clone, Debug, PartialEq)]
pub enum ClickSlot {
    Full(Stack),
    Hashed(Option<HashedStack>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct HashedStack {
    pub item: i32,
    pub count: i32,
    pub components: Vec<(i32, i32)>,
    pub removed: Vec<i32>,
}

/// Vanilla bounds the hashed component lists at 256 entries.
const MAX_HASHED_COMPONENTS: usize = 256;
/// And a click's changed slots at 128.
const MAX_CHANGED_SLOTS: usize = 128;

impl ClickSlot {
    fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        if !hashed_clicks(protocol) {
            return Ok(Self::Full(read_stack(protocol, r)?));
        }
        if !r.bool()? {
            return Ok(Self::Hashed(None));
        }
        let item = r.varint()?;
        let count = r.varint()?;
        let added = r.count()?;
        if added > MAX_HASHED_COMPONENTS {
            return Err(invalid("too many hashed components"));
        }
        let components = (0..added)
            .map(|_| Ok((r.varint()?, r.i32()?)))
            .collect::<io::Result<_>>()?;
        let removed = r.count()?;
        if removed > MAX_HASHED_COMPONENTS {
            return Err(invalid("too many removed components"));
        }
        let removed = (0..removed)
            .map(|_| r.varint())
            .collect::<io::Result<_>>()?;
        Ok(Self::Hashed(Some(HashedStack {
            item,
            count,
            components,
            removed,
        })))
    }

    fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        match (self, hashed_clicks(protocol)) {
            (Self::Full(stack), false) => write_stack(protocol, w, stack),
            (Self::Hashed(None), true) => {
                w.bool(false);
                Ok(())
            }
            (Self::Hashed(Some(hashed)), true) => {
                w.bool(true).varint(hashed.item).varint(hashed.count);
                w.varint(hashed.components.len() as i32);
                for (kind, hash) in &hashed.components {
                    w.varint(*kind).i32(*hash);
                }
                w.varint(hashed.removed.len() as i32);
                for kind in &hashed.removed {
                    w.varint(*kind);
                }
                Ok(())
            }
            _ => Err(invalid("click stack does not fit the protocol")),
        }
    }
}

/// `container_click`. Before 1.17: an action number and the clicked stack;
/// from 1.17: the changed slots and the carried stack; from 1.17.1 a state
/// ID; from 1.21.5 hashed stacks.
#[derive(Clone, Debug, PartialEq)]
pub struct Click {
    pub container: i32,
    pub state_id: i32,
    pub slot: i16,
    pub button: i8,
    /// Before 1.17.
    pub action: i16,
    pub mode: i32,
    /// Before 1.17: the stack in the clicked slot.
    pub clicked: Stack,
    /// From 1.17.
    pub changed: Vec<(i16, ClickSlot)>,
    /// From 1.17: the stack on the cursor after the click.
    pub carried: ClickSlot,
}

impl Click {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let container = read_container(protocol, r, false)?;
        let state_id = if has_state_ids(protocol) {
            r.varint()?
        } else {
            0
        };
        let slot = r.i16()?;
        let button = r.i8()?;
        if !has_changed_slots(protocol) {
            let action = r.i16()?;
            let mode = r.varint()?;
            return Ok(Self {
                container,
                state_id,
                slot,
                button,
                action,
                mode,
                clicked: read_stack(protocol, r)?,
                changed: Vec::new(),
                carried: ClickSlot::Full(None),
            });
        }
        let mode = r.varint()?;
        let count = r.count()?;
        if count > MAX_CHANGED_SLOTS {
            return Err(invalid("too many changed slots"));
        }
        let changed = (0..count)
            .map(|_| Ok((r.i16()?, ClickSlot::read(protocol, r)?)))
            .collect::<io::Result<_>>()?;
        Ok(Self {
            container,
            state_id,
            slot,
            button,
            action: 0,
            mode,
            clicked: None,
            changed,
            carried: ClickSlot::read(protocol, r)?,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        write_container(protocol, w, self.container, false)?;
        if has_state_ids(protocol) {
            w.varint(self.state_id);
        }
        w.i16(self.slot).i8(self.button);
        if !has_changed_slots(protocol) {
            w.i16(self.action).varint(self.mode);
            return write_stack(protocol, w, &self.clicked);
        }
        w.varint(self.mode).varint(self.changed.len() as i32);
        for (slot, stack) in &self.changed {
            w.i16(*slot);
            stack.write(protocol, w)?;
        }
        self.carried.write(protocol, w)
    }
}

/// `edit_book`: a whole stack (with the hand from 1.13.1, the inventory slot
/// from 1.16.4) until 1.17, then the slot, the pages and the title when
/// signing.
#[derive(Clone, Debug, PartialEq)]
pub enum EditBook {
    Stack {
        book: Stack,
        signing: bool,
        /// 1.13.1 to 1.16.3: 0 main hand, 1 off hand; from 1.16.4 the
        /// book's inventory slot (see [`book_slot`]).
        hand: i32,
    },
    Pages {
        slot: i32,
        pages: Vec<String>,
        title: Option<String>,
    },
}

/// Book edits name the book's inventory slot instead of a hand from 1.16.4
/// on (`ServerboundEditBookPacket.slot`).
pub fn book_slot(protocol: i32) -> bool {
    protocol >= 754
}

/// Books are edited by pages instead of a whole stack from 1.17.1 on.
pub fn book_pages(protocol: i32) -> bool {
    protocol >= 756
}

/// Vanilla reads at most 200 pages of 8192 characters and titles of 128.
const MAX_PAGES: usize = 200;

impl EditBook {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        if book_pages(protocol) {
            let slot = r.varint()?;
            let count = r.count()?;
            if count > MAX_PAGES {
                return Err(invalid("too many book pages"));
            }
            let pages = (0..count)
                .map(|_| Ok(r.string(8192)?.to_owned()))
                .collect::<io::Result<_>>()?;
            let title = if r.bool()? {
                Some(r.string(128)?.to_owned())
            } else {
                None
            };
            return Ok(Self::Pages { slot, pages, title });
        }
        let book = read_stack(protocol, r)?;
        let signing = r.bool()?;
        let hand = if protocol >= 401 { r.varint()? } else { 0 };
        Ok(Self::Stack {
            book,
            signing,
            hand,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        match (self, book_pages(protocol)) {
            (Self::Pages { slot, pages, title }, true) => {
                w.varint(*slot).varint(pages.len() as i32);
                for page in pages {
                    w.string(page);
                }
                match title {
                    Some(title) => {
                        w.bool(true).string(title);
                    }
                    None => {
                        w.bool(false);
                    }
                }
                Ok(())
            }
            (
                Self::Stack {
                    book,
                    signing,
                    hand,
                },
                false,
            ) => {
                write_stack(protocol, w, book)?;
                w.bool(*signing);
                if protocol >= 401 {
                    w.varint(*hand);
                }
                Ok(())
            }
            _ => Err(invalid("book edit does not fit the protocol")),
        }
    }
}
