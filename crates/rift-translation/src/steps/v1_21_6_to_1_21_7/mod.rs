//! Minecraft 1.21.6 (protocol 771) and 1.21.7/1.21.8 (protocol 772).
//!
//! The two protocols share packet IDs and layouts and differ only in built-in
//! registry numbering: 1.21.7 inserted one item and one sound event. Registry
//! ID tables come from Mojang's own data reports; entries one version lacks
//! map to explicit stand-ins. Packets that cannot carry a changed ID pass
//! through untouched, and a packet that does carry one but cannot be parsed
//! fails the connection rather than reaching the peer corrupted.
mod down;
mod layout;
mod rewriter;
mod tables;
mod up;

use super::Make;
use crate::{
    Direction, Packet, State,
    step::{Cx, Step},
};
use layout::{Ids, Layout};
use std::{io, sync::OnceLock};
use tables::IdMap;

pub const OLDER: i32 = 771;
pub const NEWER: i32 = 772;

/// A 1.21.7 client on a 1.21.6 backend.
pub const UP: Option<Make> = up::STEP;
/// A 1.21.6 client on a 1.21.7 backend.
pub const DOWN: Option<Make> = down::STEP;

/// One adjacent pair of protocols with identical packet IDs and layouts,
/// differing only in built-in registry numbering.
pub(super) struct Renumbering {
    layout: Layout,
    /// Older to newer IDs, then newer to older.
    up: Tables,
    down: Tables,
}

/// One direction of a step. ID tables are `None` for a registry the step
/// does not renumber; `missing_*` name entries the receiving side lacks.
struct Tables {
    item: Option<IdMap>,
    sound: Option<IdMap>,
    /// Item names the receiver lacks, with their stand-in names.
    missing_items: Vec<(String, String)>,
    /// Sound event names the receiver lacks.
    missing_sounds: Vec<String>,
}

impl Tables {
    fn ids(&self) -> Ids<'_> {
        Ids {
            item: self.item.as_deref(),
            sound: self.sound.as_deref(),
            missing_items: &self.missing_items,
            missing_sounds: &self.missing_sounds,
        }
    }
}

const V1_21_6: &str = include_str!("data/1.21.6.json");
const V1_21_8: &str = include_str!("data/1.21.8.json");
const V1_21_6_TO_1_21_8: &str = include_str!("data/1.21.6-1.21.8.json");

/// Registries a registry-only step can renumber.
const REMAPPED: &[&str] = &["minecraft:item", "minecraft:sound_event"];

fn build(older: &str, newer: &str, step: &str) -> Renumbering {
    let (older, newer, step) = (
        tables::version(older),
        tables::version(newer),
        tables::step(step),
    );
    assert_eq!((step.from, step.to), (older.protocol, newer.protocol));
    assert!(
        step.packets.is_empty(),
        "registry-only steps keep packet IDs"
    );
    for name in step.registries.keys() {
        assert!(REMAPPED.contains(&name.as_str()), "{name} is not remapped");
    }
    let layout = Layout::new(&older).expect("older protocol layout");
    assert!(
        layout == Layout::new(&newer).expect("newer protocol layout"),
        "registry-only steps share one layout"
    );
    let item = step.registries.get("minecraft:item");
    let sound = step.registries.get("minecraft:sound_event");
    let names = |entries: Option<&Vec<(usize, String, usize, String)>>| {
        entries
            .into_iter()
            .flatten()
            .map(|(_, name, _, stand_in)| (name.clone(), stand_in.clone()))
            .collect::<Vec<_>>()
    };
    let sounds = |entries| names(entries).into_iter().map(|(name, _)| name).collect();
    Renumbering {
        layout,
        // Upgrading, the newer receiver lacks what the newer version removed.
        up: Tables {
            item: item.map(tables::RegistryDiff::upgrade),
            sound: sound.map(tables::RegistryDiff::upgrade),
            missing_items: names(item.map(|diff| &diff.removed)),
            missing_sounds: sounds(sound.map(|diff| &diff.removed)),
        },
        down: Tables {
            item: item.map(tables::RegistryDiff::downgrade),
            sound: sound.map(tables::RegistryDiff::downgrade),
            missing_items: names(item.map(|diff| &diff.inserted)),
            missing_sounds: sounds(sound.map(|diff| &diff.inserted)),
        },
    }
}

fn renumbering() -> &'static Renumbering {
    static RENUMBERING: OnceLock<Renumbering> = OnceLock::new();
    RENUMBERING.get_or_init(|| build(V1_21_6, V1_21_8, V1_21_6_TO_1_21_8))
}

/// The step in either orientation. Stateless: every packet is rewritten on
/// its own.
struct Renumber {
    /// The backend is the older side: upgrade clientbound, downgrade serverbound.
    upgrade: bool,
}

impl Renumber {
    fn rewrite(&self, state: State, direction: Direction, packet: Packet) -> io::Result<Packet> {
        let renumbering = renumbering();
        // Walk from the sender's version towards the receiver's.
        let towards_newer = self.upgrade == (direction == Direction::Clientbound);
        let tables = if towards_newer {
            &renumbering.up
        } else {
            &renumbering.down
        };
        renumbering
            .layout
            .rewrite(state, direction, packet, &tables.ids())
    }
}

impl Step for Renumber {
    fn clientbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let packet = self.rewrite(state, Direction::Clientbound, packet)?;
        cx.to_client(state, packet);
        Ok(())
    }

    fn serverbound(&mut self, cx: &mut Cx<'_>, state: State, packet: Packet) -> io::Result<()> {
        let packet = self.rewrite(state, Direction::Serverbound, packet)?;
        cx.to_backend(state, packet);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
