//! Process-wide player presence. A session owns its registration, so cancellation,
//! login failure and disconnect all remove the player without a separate cleanup.
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    pub uuid: [u8; 16],
    pub name: String,
    pub server: String,
}

#[derive(Debug, Default)]
pub struct PlayerRegistry {
    state: Mutex<RegistryState>,
}

#[derive(Debug, Default)]
struct RegistryState {
    players: BTreeMap<[u8; 16], Player>,
    // Includes pending logins and active players, under the same lock as UUIDs.
    names: BTreeSet<String>,
}

impl PlayerRegistry {
    /// Reserve a case-insensitive name before contacting a backend, which might
    /// otherwise evict an existing player before replying to a duplicate login.
    /// Pending logins are not exposed as connected players.
    pub fn reserve_name(self: &Arc<Self>, name: impl Into<String>) -> io::Result<NameReservation> {
        let name = checked_name(name.into())?;
        let key = name.to_ascii_lowercase();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.names.insert(key.clone()) {
            return Err(already_connected());
        }
        Ok(NameReservation {
            registry: self.clone(),
            name,
            key,
            pending: true,
        })
    }

    /// Atomically reserve both the UUID and case-insensitive name. The registry
    /// must be shared across configuration reloads and all listening sockets.
    pub fn register(
        self: &Arc<Self>,
        uuid: [u8; 16],
        name: impl Into<String>,
        server: impl Into<String>,
    ) -> io::Result<PlayerRegistration> {
        let name = checked_name(name.into())?;
        let key = name.to_ascii_lowercase();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.players.contains_key(&uuid) || state.names.contains(&key) {
            return Err(already_connected());
        }
        state.names.insert(key);
        state.players.insert(
            uuid,
            Player {
                uuid,
                name,
                server: server.into(),
            },
        );
        Ok(PlayerRegistration {
            registry: self.clone(),
            uuid,
        })
    }

    pub fn snapshot(&self) -> Vec<Player> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .players
            .values()
            .cloned()
            .collect()
    }

    /// Count connected players from one consistent view of the registry.
    /// Clone only distinct server names, not player records or pending logins.
    pub(crate) fn server_counts(&self) -> BTreeMap<String, usize> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut counts = BTreeMap::new();
        for player in state.players.values() {
            if let Some(count) = counts.get_mut(&player.server) {
                *count += 1;
            } else {
                counts.insert(player.server.clone(), 1);
            }
        }
        counts
    }

    /// Check one backend's occupancy without cloning player records.
    pub(crate) fn count_on_server(&self, server: &str) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .players
            .values()
            .filter(|player| player.server == server)
            .count()
    }

    pub fn get(&self, uuid: &[u8; 16]) -> Option<Player> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .players
            .get(uuid)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .players
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A pending login's exclusive name claim. Cancellation releases it, while
/// successful backend login converts it into a full player registration.
#[derive(Debug)]
pub struct NameReservation {
    registry: Arc<PlayerRegistry>,
    name: String,
    key: String,
    pending: bool,
}

impl NameReservation {
    pub fn register(
        mut self,
        uuid: [u8; 16],
        server: impl Into<String>,
    ) -> io::Result<PlayerRegistration> {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let std::collections::btree_map::Entry::Vacant(entry) = state.players.entry(uuid) else {
            // RAII cleanup needs this lock to release the pending name claim.
            drop(state);
            return Err(already_connected());
        };
        entry.insert(Player {
            uuid,
            name: self.name.clone(),
            server: server.into(),
        });
        // Keep the existing name claim without an unreserved interval.
        self.pending = false;
        Ok(PlayerRegistration {
            registry: self.registry.clone(),
            uuid,
        })
    }
}

impl Drop for NameReservation {
    fn drop(&mut self) {
        if self.pending {
            self.registry
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .names
                .remove(&self.key);
        }
    }
}

/// A unique session reservation. Intentionally not cloneable.
#[derive(Debug)]
pub struct PlayerRegistration {
    registry: Arc<PlayerRegistry>,
    uuid: [u8; 16],
}

impl PlayerRegistration {
    /// Commit presence only after the replacement server has accepted the player.
    pub fn set_server(&self, server: &str) {
        if let Some(player) = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .players
            .get_mut(&self.uuid)
        {
            player.server = server.to_owned();
        }
    }
}

impl Drop for PlayerRegistration {
    fn drop(&mut self) {
        let mut state = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(player) = state.players.remove(&self.uuid) {
            state.names.remove(&player.name.to_ascii_lowercase());
        }
    }
}

fn checked_name(name: String) -> io::Result<String> {
    if valid_username(&name) {
        Ok(name)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid player name",
        ))
    }
}

fn already_connected() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "That player is already connected to this network.",
    )
}

pub(crate) fn valid_username(name: &str) -> bool {
    (1..=16).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_tracks_switches_and_cleans_up_on_drop() {
        let registry = Arc::new(PlayerRegistry::default());
        let registration = registry.register([1; 16], "Alice", "lobby").unwrap();
        assert_eq!(registry.len(), 1);
        let before = registry.snapshot();
        registration.set_server("survival");
        assert_eq!(before[0].server, "lobby");
        assert_eq!(registry.get(&[1; 16]).unwrap().server, "survival");
        drop(registration);
        assert!(registry.is_empty());
        assert!(registry.register([1; 16], "Alice", "lobby").is_ok());
    }

    #[test]
    fn uuid_and_names_are_independently_unique() {
        let registry = Arc::new(PlayerRegistry::default());
        let original = registry.register([1; 16], "Alice", "lobby").unwrap();
        for (uuid, name) in [([1; 16], "Bob"), ([2; 16], "aLiCe")] {
            assert_eq!(
                registry
                    .register(uuid, name, "survival")
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::AlreadyExists
            );
        }
        assert_eq!(registry.snapshot()[0].name, "Alice");
        drop(original);
        assert!(registry.register([2; 16], "alice", "survival").is_ok());
    }

    #[test]
    fn simultaneous_registrations_cannot_claim_one_name() {
        let registry = Arc::new(PlayerRegistry::default());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let attempts: Vec<_> = (0..2)
            .map(|index| {
                let registry = registry.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    registry.register([index; 16], "Alice", "lobby")
                })
            })
            .collect();
        let registrations: Vec<_> = attempts
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            registrations.iter().filter(|entry| entry.is_ok()).count(),
            1
        );
        assert_eq!(registry.len(), 1);
        drop(registrations);
        assert!(registry.is_empty());
    }

    #[test]
    fn player_names_are_bounded_and_cannot_contain_command_syntax() {
        let registry = Arc::new(PlayerRegistry::default());
        for name in [
            "",
            "too_long_username",
            "Alice Bob",
            "*",
            "Alice\n",
            "Ålice",
        ] {
            assert_eq!(
                registry
                    .register([1; 16], name, "lobby")
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(registry.register([1; 16], "Alice_123", "lobby").is_ok());
    }

    #[test]
    fn pending_names_are_exclusive_but_invisible_and_cancel_safely() {
        let registry = Arc::new(PlayerRegistry::default());
        let pending = registry.reserve_name("Alice").unwrap();
        assert!(registry.is_empty());
        assert!(registry.snapshot().is_empty());
        assert_eq!(
            registry.reserve_name("ALICE").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            registry
                .register([1; 16], "alice", "lobby")
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        drop(pending);
        assert!(registry.reserve_name("Alice").is_ok());
        assert!(registry.register([1; 16], "Alice", "lobby").is_ok());
        for invalid in ["", "Alice Bob", "*", "too_long_username"] {
            assert_eq!(
                registry.reserve_name(invalid).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn pending_conversion_keeps_name_reserved_until_player_disconnects() {
        let registry = Arc::new(PlayerRegistry::default());
        let pending = registry.reserve_name("Alice").unwrap();
        let player = pending.register([1; 16], "lobby").unwrap();
        assert_eq!(
            registry.snapshot(),
            [Player {
                uuid: [1; 16],
                name: "Alice".into(),
                server: "lobby".into()
            }]
        );
        assert_eq!(
            registry.reserve_name("aLiCe").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        player.set_server("survival");
        assert_eq!(registry.get(&[1; 16]).unwrap().server, "survival");
        drop(player);
        assert!(registry.reserve_name("Alice").is_ok());
    }

    #[test]
    fn duplicates_never_replace_existing_players_and_failed_conversion_releases_name() {
        let registry = Arc::new(PlayerRegistry::default());
        let original = registry.register([1; 16], "Alice", "survival").unwrap();
        assert_eq!(
            registry.reserve_name("Alice").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let pending = registry.reserve_name("Bob").unwrap();
        assert_eq!(
            pending.register([1; 16], "lobby").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            registry.snapshot(),
            [Player {
                uuid: [1; 16],
                name: "Alice".into(),
                server: "survival".into()
            }]
        );
        let bob = registry
            .reserve_name("Bob")
            .unwrap()
            .register([2; 16], "lobby")
            .unwrap();
        assert_eq!(registry.len(), 2);
        drop(bob);
        drop(original);
        assert!(registry.is_empty());
    }

    #[test]
    fn converting_a_pending_name_never_opens_a_duplicate_registration_gap() {
        let registry = Arc::new(PlayerRegistry::default());
        let pending = registry.reserve_name("Alice").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let other_registry = registry.clone();
        let other_barrier = barrier.clone();
        let duplicate = std::thread::spawn(move || {
            other_barrier.wait();
            other_registry.register([2; 16], "ALICE", "survival")
        });
        barrier.wait();
        let _player = pending.register([1; 16], "lobby").unwrap();
        assert_eq!(
            duplicate.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(registry.len(), 1);
    }
}
