//! The slot contents of the client's menus as a 1.16 backend sees them.
//!
//! A 1.16 server checks every container click against the stack the click
//! picked up and, on a mismatch, rejects it and resends the whole menu. A
//! 1.17 client no longer names that stack: it reports the slots it changed.
//! To name it for the backend the step remembers what each slot holds, from
//! the backend's slot packets and the client's own reports of what it
//! changed (an accepted 1.16 click is not echoed back to the clicker).
use super::common::{EMPTY_ITEM, raw_item};
use crate::wire::Reader;
use std::io;

/// The player's inventory menu, always open underneath.
const INVENTORY: u8 = 0;

/// At most this many slots per menu: the largest vanilla menu (a double
/// chest with the inventory) has 90; anything beyond is ignored.
const MAX_SLOTS: usize = 256;

#[derive(Default)]
pub struct Menus {
    /// The inventory menu and at most one container on top, by window ID;
    /// each slot as raw 1.16 item stack bytes.
    windows: Vec<(u8, Vec<Vec<u8>>)>,
}

impl Menus {
    fn window(&mut self, window: u8) -> &mut Vec<Vec<u8>> {
        if let Some(index) = self.windows.iter().position(|(id, _)| *id == window) {
            return &mut self.windows[index].1;
        }
        // One container is open at a time: a new one replaces the last.
        if window != INVENTORY {
            self.windows.retain(|(id, _)| *id == INVENTORY);
        }
        self.windows.push((window, Vec::new()));
        &mut self.windows.last_mut().unwrap().1
    }

    /// `container_set_content` of 1.16: window (unsigned byte), a short
    /// count, then the stacks.
    pub fn set_content(&mut self, data: &[u8]) -> io::Result<()> {
        let mut r = Reader::new(data);
        let window = r.u8()?;
        let count = r.i16()?;
        let count =
            usize::try_from(count).map_err(|_| crate::wire::invalid("negative slot count"))?;
        let mut slots = Vec::with_capacity(count.min(MAX_SLOTS));
        for index in 0..count {
            let item = raw_item(&mut r)?;
            if index < MAX_SLOTS {
                slots.push(item.to_vec());
            }
        }
        r.finish()?;
        *self.window(window) = slots;
        Ok(())
    }

    /// `container_set_slot` of 1.16: window (signed byte), slot, stack.
    /// Window -1 is the cursor; -2 addresses the player's inventory by its
    /// own numbering rather than the menu's.
    pub fn set_slot(&mut self, data: &[u8]) -> io::Result<()> {
        let mut r = Reader::new(data);
        let window = r.i8()?;
        let slot = r.i16()?;
        let item = raw_item(&mut r)?;
        r.finish()?;
        let (window, slot) = match window {
            -2 => (INVENTORY, inventory_slot(slot)),
            window if window >= 0 => (window as u8, usize::try_from(slot).ok()),
            _ => return Ok(()),
        };
        if let Some(slot) = slot {
            self.set(window, slot, item);
        }
        Ok(())
    }

    /// Record a stack the client reports a slot now holds.
    pub fn set(&mut self, window: u8, slot: usize, item: &[u8]) {
        if slot >= MAX_SLOTS {
            return;
        }
        let slots = self.window(window);
        if slots.len() <= slot {
            slots.resize(slot + 1, EMPTY_ITEM.to_vec());
        }
        slots[slot] = item.to_vec();
    }

    /// What `slot` of `window` holds, as far as the step knows.
    pub fn get(&self, window: u8, slot: i16) -> &[u8] {
        self.windows
            .iter()
            .find(|(id, _)| *id == window)
            .and_then(|(_, slots)| slots.get(usize::try_from(slot).ok()?))
            .map_or(EMPTY_ITEM, Vec::as_slice)
    }

    /// Whether the step follows `window`, so client reports are worth keeping.
    pub fn knows(&self, window: u8) -> bool {
        self.windows.iter().any(|(id, _)| *id == window)
    }

    pub fn close(&mut self, window: u8) {
        if window != INVENTORY {
            self.windows.retain(|(id, _)| *id != window);
        }
    }
}

/// The inventory menu slot of a player inventory index: the hotbar (0-8)
/// sits at 36-44, armour (36 feet to 39 head) at 8 down to 5, the offhand
/// (40) at 45; the main inventory keeps its numbers.
fn inventory_slot(index: i16) -> Option<usize> {
    Some(match index {
        0..=8 => index as usize + 36,
        9..=35 => index as usize,
        36..=39 => 44 - index as usize,
        40 => 45,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Writer;

    fn stack(id: i32, count: i8) -> Vec<u8> {
        let mut w = Writer::new();
        w.bool(true).varint(id).i8(count).u8(0);
        w.into_inner()
    }

    #[test]
    fn content_and_slot_updates_are_followed() {
        let mut menus = Menus::default();
        let mut w = Writer::new();
        w.u8(0).i16(3).bytes(&[0]).bytes(&stack(1, 5)).bytes(&[0]);
        menus.set_content(&w.buf).unwrap();
        assert_eq!(menus.get(0, 1), stack(1, 5));
        assert_eq!(menus.get(0, 2), EMPTY_ITEM);
        assert_eq!(menus.get(0, 99), EMPTY_ITEM);

        let mut w = Writer::new();
        w.i8(-2).i16(0).bytes(&stack(7, 1));
        menus.set_slot(&w.buf).unwrap();
        assert_eq!(menus.get(0, 36), stack(7, 1));

        let mut w = Writer::new();
        w.i8(3).i16(2).bytes(&stack(9, 2));
        menus.set_slot(&w.buf).unwrap();
        assert!(menus.knows(3));
        assert_eq!(menus.get(3, 2), stack(9, 2));
        menus.set(4, 0, &stack(1, 1));
        assert!(!menus.knows(3), "a new container replaces the last");
        menus.close(0);
        let mut fresh = Menus::default();
        fresh.set(4, 0, &stack(1, 1));
        fresh.set(0, 0, &stack(1, 1));
        assert!(fresh.knows(4), "the inventory does not replace a container");
        assert!(menus.knows(0));
        menus.close(4);
        menus.close(0);
        assert!(!menus.knows(4));
        assert!(menus.knows(0));
    }

    #[test]
    fn cursor_updates_and_garbage_are_handled() {
        let mut menus = Menus::default();
        let mut w = Writer::new();
        w.i8(-1).i16(-1).bytes(&stack(1, 1));
        menus.set_slot(&w.buf).unwrap();
        assert!(!menus.knows(0));
        assert!(menus.set_slot(&[0, 0]).is_err());
        assert!(menus.set_content(&[0, 0xff, 0xff]).is_err());
        assert!(menus.set_content(&[0, 0x7f, 0xff]).is_err());
        assert_eq!(inventory_slot(39), Some(5));
        assert_eq!(inventory_slot(36), Some(8));
        assert_eq!(inventory_slot(41), None);
    }
}
