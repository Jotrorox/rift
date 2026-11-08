//! Active contexts for a player: the reserved keys the adapter fills from
//! Pumpkin's player state, plus custom keys from registered calculators.
//!
//! Everything here is generic over the player type so it is tested without
//! a server; the plugin instantiates it with `Arc<Player>`.

use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, PoisonError, RwLock},
};

use pumpkin_util::GameMode;
use rift_permissions::{
    ContextSet, Uuid,
    context::{DIMENSION, GAMEMODE, RESERVED, SERVER, WORLD},
};

/// What the adapter reads from a Pumpkin player for the reserved contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerState<'a> {
    /// The world folder name (`world`, `world_nether`, ...).
    pub world: &'a str,
    /// The dimension type, such as `minecraft:the_nether`.
    pub dimension: &'a str,
    pub gamemode: GameMode,
}

/// The `gamemode` context value: `survival`, `creative`, `adventure` or
/// `spectator`, as the proxy and the Paper adapter spell it.
pub const fn gamemode_name(mode: GameMode) -> &'static str {
    match mode {
        GameMode::Survival => "survival",
        GameMode::Creative => "creative",
        GameMode::Adventure => "adventure",
        GameMode::Spectator => "spectator",
    }
}

/// The reserved contexts for a player on this backend. A value the context
/// syntax refuses (a world folder containing `=`, say) is left out rather
/// than failing the check.
pub fn base_contexts(server: &str, state: &PlayerState<'_>) -> ContextSet {
    let mut set = ContextSet::new();
    for (key, value) in [
        (SERVER, server),
        (WORLD, state.world),
        (DIMENSION, state.dimension),
        (GAMEMODE, gamemode_name(state.gamemode)),
    ] {
        if let Err(error) = set.insert(key, value) {
            log::debug!("rift-permissions: leaving out context {key}: {error}");
        }
    }
    set
}

/// Whether `key` is one the adapter fills itself.
fn reserved(key: &str) -> bool {
    RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(key))
}

/// A custom context calculator: returns `(key, value)` pairs for a player.
/// Runs on every permission check, on the thread doing the check, so it must
/// be cheap and must not block.
pub type CalculatorFn<P> = dyn Fn(&P) -> Vec<(String, String)> + Send + Sync;

struct Entry<P> {
    id: u64,
    name: String,
    calculate: Arc<CalculatorFn<P>>,
}

struct Registry<P> {
    next: u64,
    entries: Vec<Entry<P>>,
}

/// The registered custom context calculators.
pub struct Calculators<P> {
    registry: Arc<RwLock<Registry<P>>>,
}

impl<P> Default for Calculators<P> {
    fn default() -> Self {
        Self {
            registry: Arc::new(RwLock::new(Registry {
                next: 0,
                entries: Vec::new(),
            })),
        }
    }
}

impl<P: 'static> Calculators<P> {
    /// Register `calculate` under `name` (used in log lines) until the
    /// returned handle is dropped.
    pub fn add(&self, name: &str, calculate: Box<CalculatorFn<P>>) -> CalculatorHandle {
        let mut registry = self
            .registry
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let id = registry.next;
        registry.next += 1;
        registry.entries.push(Entry {
            id,
            name: name.to_owned(),
            calculate: Arc::from(calculate),
        });
        let weak = Arc::downgrade(&self.registry);
        CalculatorHandle(Some(Box::new(move || {
            if let Some(registry) = weak.upgrade() {
                registry
                    .write()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entries
                    .retain(|entry| entry.id != id);
            }
        })))
    }

    /// The custom contexts for `player`: every calculator's pairs, without
    /// reserved keys and invalid pairs. A panicking calculator contributes
    /// nothing.
    pub fn calculate(&self, player: &P) -> ContextSet {
        let calculators: Vec<(String, Arc<CalculatorFn<P>>)> = {
            let registry = self.registry.read().unwrap_or_else(PoisonError::into_inner);
            if registry.entries.is_empty() {
                return ContextSet::new();
            }
            registry
                .entries
                .iter()
                .map(|entry| (entry.name.clone(), entry.calculate.clone()))
                .collect()
        };
        let mut set = ContextSet::new();
        for (name, calculate) in calculators {
            let Ok(pairs) = catch_unwind(AssertUnwindSafe(|| calculate(player))) else {
                log::error!("rift-permissions: context calculator {name} panicked");
                continue;
            };
            for (key, value) in pairs {
                if reserved(&key) {
                    log::debug!(
                        "rift-permissions: calculator {name} returned reserved context {key}; ignored"
                    );
                } else if let Err(error) = set.insert(&key, &value) {
                    log::debug!("rift-permissions: calculator {name}: {error}");
                }
            }
        }
        set
    }
}

/// A registered calculator. Dropping it unregisters the calculator; call
/// [`forget`](Self::forget) to keep it for the rest of the process.
#[must_use = "dropping a CalculatorHandle unregisters the calculator at once"]
pub struct CalculatorHandle(Option<Box<dyn FnOnce() + Send + Sync>>);

impl CalculatorHandle {
    /// Keep the calculator registered for as long as the adapter runs.
    pub fn forget(mut self) {
        self.0 = None;
    }
}

impl Drop for CalculatorHandle {
    fn drop(&mut self) {
        if let Some(remove) = self.0.take() {
            remove();
        }
    }
}

/// The custom contexts last reported to the proxy per player, so a report is
/// sent only when they change.
#[derive(Default)]
pub struct Reported {
    players: Mutex<HashMap<Uuid, ContextSet>>,
}

impl Reported {
    /// Record `custom` for `uuid`; true when it differs from the last report
    /// (no report counts as the empty set).
    pub fn update(&self, uuid: Uuid, custom: &ContextSet) -> bool {
        let mut players = self.players.lock().unwrap_or_else(PoisonError::into_inner);
        match players.get(&uuid) {
            Some(last) if last == custom => false,
            None if custom.is_empty() => false,
            _ => {
                players.insert(uuid, custom.clone());
                true
            }
        }
    }

    /// Forget a player who left.
    pub fn forget(&self, uuid: Uuid) {
        self.players
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&uuid);
    }

    /// Every non-empty report, to repeat after a reconnect.
    pub fn all(&self) -> Vec<(Uuid, ContextSet)> {
        self.players
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, set)| !set.is_empty())
            .map(|(uuid, set)| (*uuid, set.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(set: &ContextSet) -> Vec<(String, String)> {
        set.iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
    }

    #[test]
    fn reserved_contexts_from_player_state() {
        let state = PlayerState {
            world: "World_Nether",
            dimension: "minecraft:the_nether",
            gamemode: GameMode::Creative,
        };
        let set = base_contexts("Survival-1", &state);
        assert_eq!(
            set.to_string(),
            "dimension=minecraft:the_nether;gamemode=creative;server=survival-1;world=world_nether"
        );
        let odd = PlayerState {
            world: "a=b",
            ..state
        };
        let set = base_contexts("lobby", &odd);
        assert_eq!(set.get(WORLD), None);
        assert_eq!(set.get(SERVER), Some("lobby"));
    }

    #[test]
    fn every_gamemode_has_a_context_value() {
        let names: Vec<_> = GameMode::VALUES.iter().map(|m| gamemode_name(*m)).collect();
        assert_eq!(names, ["survival", "creative", "adventure", "spectator"]);
    }

    #[test]
    fn calculators_add_custom_keys_only() {
        let calculators = Calculators::<u32>::default();
        assert!(calculators.calculate(&7).is_empty());
        let region = calculators.add(
            "regions",
            Box::new(|player: &u32| {
                vec![
                    ("Region".into(), format!("r{player}")),
                    ("world".into(), "spoofed".into()),
                    ("bad key".into(), "x".into()),
                ]
            }),
        );
        let broken = calculators.add("broken", Box::new(|_: &u32| panic!("calculator bug")));
        let team = calculators.add(
            "teams",
            Box::new(|_: &u32| vec![("team".into(), "red".into())]),
        );
        let set = calculators.calculate(&7);
        assert_eq!(
            pairs(&set),
            [
                ("region".into(), "r7".into()),
                ("team".into(), "red".into())
            ]
        );
        drop(region);
        drop(broken);
        team.forget();
        assert_eq!(
            pairs(&calculators.calculate(&7)),
            [("team".into(), "red".into())]
        );
    }

    #[test]
    fn reports_only_on_change() {
        let reported = Reported::default();
        let alice = Uuid::from_u128(1);
        assert!(!reported.update(alice, &ContextSet::new()));
        let spawn = ContextSet::of("region", "spawn").unwrap();
        assert!(reported.update(alice, &spawn));
        assert!(!reported.update(alice, &spawn));
        assert_eq!(reported.all(), [(alice, spawn.clone())]);
        // Leaving a region is a change the proxy must hear about.
        assert!(reported.update(alice, &ContextSet::new()));
        assert!(reported.all().is_empty());
        assert!(reported.update(alice, &spawn));
        reported.forget(alice);
        assert!(reported.all().is_empty());
        assert!(reported.update(alice, &spawn));
    }
}
