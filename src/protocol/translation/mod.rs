//! Protocol translation between a client and a backend on different versions.
//!
//! Like ViaVersion, a [`Translator`] chains adjacent version steps. Each step
//! knows one older and one newer protocol and rewrites packets in both
//! directions: upgrading backend packets for a newer client and downgrading
//! that client's packets for the backend, or the reverse for an older client.
//!
//! Registry ID tables are generated from Mojang's own data reports by
//! scripts/translation_data.py; entries one version lacks map to explicit
//! stand-ins. Packets a step does not recognise as carrying a changed ID pass
//! through untouched, and a packet that does carry one but cannot be parsed
//! fails the connection rather than reaching the peer corrupted.
mod data;
mod v1_21_6;
mod wire;

use super::{Direction, Packet, ProtocolVersion, State};
use data::IdMap;
use std::{io, sync::OnceLock};
use v1_21_6::{Ids, Layout};

/// One adjacent pair of protocols with identical packet IDs and layouts,
/// differing only in built-in registry numbering.
struct Step {
    older: i32,
    newer: i32,
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

fn registry_step(older: &str, newer: &str, step: &str) -> Step {
    let (older, newer, step) = (data::version(older), data::version(newer), data::step(step));
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
    Step {
        older: step.from,
        newer: step.to,
        layout,
        // Upgrading, the newer receiver lacks what the newer version removed.
        up: Tables {
            item: item.map(data::RegistryDiff::upgrade),
            sound: sound.map(data::RegistryDiff::upgrade),
            missing_items: names(item.map(|diff| &diff.removed)),
            missing_sounds: sounds(sound.map(|diff| &diff.removed)),
        },
        down: Tables {
            item: item.map(data::RegistryDiff::downgrade),
            sound: sound.map(data::RegistryDiff::downgrade),
            missing_items: names(item.map(|diff| &diff.inserted)),
            missing_sounds: sounds(sound.map(|diff| &diff.inserted)),
        },
    }
}

fn steps() -> &'static [Step] {
    static STEPS: OnceLock<Vec<Step>> = OnceLock::new();
    STEPS.get_or_init(|| vec![registry_step(V1_21_6, V1_21_8, V1_21_6_TO_1_21_8)])
}

/// The steps from `low` to `high`, or none when a link is missing.
fn chain(low: i32, high: i32) -> Option<Vec<&'static Step>> {
    let mut chain = Vec::new();
    let mut at = low;
    while at != high {
        let step = steps().iter().find(|step| step.older == at)?;
        if step.newer > high {
            return None;
        }
        chain.push(step);
        at = step.newer;
    }
    Some(chain)
}

/// Whether a client on `client` can play on a backend on `backend`.
pub fn supported(client: ProtocolVersion, backend: ProtocolVersion) -> bool {
    let (low, high) = ordered(client, backend);
    chain(low, high).is_some()
}

fn ordered(a: ProtocolVersion, b: ProtocolVersion) -> (i32, i32) {
    let (a, b) = (a.number(), b.number());
    (a.min(b), a.max(b))
}

/// Translates one client/backend pair. Stateless today, but owned per backend
/// attachment so later steps can track per-connection state.
pub(crate) struct Translator {
    /// Backend older than the client: upgrade clientbound, downgrade serverbound.
    upgrade: bool,
    steps: Vec<&'static Step>,
}

impl Translator {
    /// `None` when both sides already speak the same protocol.
    pub(crate) fn new(
        client: ProtocolVersion,
        backend: ProtocolVersion,
    ) -> io::Result<Option<Self>> {
        if client == backend {
            return Ok(None);
        }
        let (low, high) = ordered(client, backend);
        let steps = chain(low, high).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Rift cannot translate between protocols {} and {}.",
                    client.number(),
                    backend.number()
                ),
            )
        })?;
        Ok(Some(Self {
            upgrade: backend.number() < client.number(),
            steps,
        }))
    }

    /// Translate a backend packet received in `state` for the client.
    pub(crate) fn clientbound(&mut self, state: State, packet: Packet) -> io::Result<Packet> {
        self.translate(state, Direction::Clientbound, packet)
    }

    /// Translate a client packet received in `state` for the backend.
    pub(crate) fn serverbound(&mut self, state: State, packet: Packet) -> io::Result<Packet> {
        self.translate(state, Direction::Serverbound, packet)
    }

    fn translate(
        &self,
        state: State,
        direction: Direction,
        mut packet: Packet,
    ) -> io::Result<Packet> {
        // Walk from the sender's version towards the receiver's.
        let towards_newer = self.upgrade == (direction == Direction::Clientbound);
        let mut apply = |step: &Step| {
            let tables = if towards_newer { &step.up } else { &step.down };
            let taken = std::mem::replace(&mut packet, Packet::empty(0));
            packet = step
                .layout
                .rewrite(state, direction, taken, &tables.ids())?;
            Ok::<_, io::Error>(())
        };
        if towards_newer {
            self.steps.iter().try_for_each(|step| apply(step))?;
        } else {
            self.steps.iter().rev().try_for_each(|step| apply(step))?;
        }
        Ok(packet)
    }
}

#[cfg(test)]
mod tests;
