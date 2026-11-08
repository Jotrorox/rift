//! Process-wide player presence. A session owns its registration, so cancellation,
//! login failure and disconnect all remove the player without a separate cleanup.
use crate::{auth::ProfileProperty, protocol::TabEntry};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    pub uuid: [u8; 16],
    pub name: String,
    pub server: String,
    /// Signed profile properties (the skin) of an authenticated player.
    pub properties: Arc<[ProfileProperty]>,
}

#[derive(Debug, Default)]
pub struct PlayerRegistry {
    state: Mutex<RegistryState>,
    /// Changes on every join, server change and disconnect, so that tab lists
    /// can skip rebuilding while the network is unchanged.
    generation: AtomicU64,
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
                properties: Arc::default(),
            },
        );
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(PlayerRegistration {
            registry: self.clone(),
            uuid,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Tab list entries for every connected player who is not on `server`.
    pub fn tab_entries(&self, server: &str) -> Vec<TabEntry> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .players
            .values()
            .filter(|player| player.server != server)
            .map(|player| TabEntry {
                uuid: player.uuid,
                name: player.name.clone(),
                properties: player.properties.clone(),
            })
            .collect()
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
            properties: Arc::default(),
        });
        self.registry.generation.fetch_add(1, Ordering::Relaxed);
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
            self.registry.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record the signed profile properties of an authenticated player.
    pub fn set_properties(&self, properties: &[ProfileProperty]) {
        if let Some(player) = self
            .registry
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .players
            .get_mut(&self.uuid)
        {
            player.properties = properties.into();
            self.registry.generation.fetch_add(1, Ordering::Relaxed);
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
            self.registry.generation.fetch_add(1, Ordering::Relaxed);
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

/// The UUID an offline-mode server gives `name`: a version 3 UUID of the MD5
/// of `OfflinePlayer:<name>`, as vanilla and Bukkit compute it. Pumpkin
/// derives its own (the first 16 bytes of SHA-256 of the name); sessions then
/// use the UUID the backend confirms at login.
pub fn offline_uuid(name: &str) -> [u8; 16] {
    let mut digest = md5(format!("OfflinePlayer:{name}").as_bytes());
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    digest
}

/// RFC 1321 MD5, only for offline UUIDs; never for security.
fn md5(input: &[u8]) -> [u8; 16] {
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let table: [u32; 64] =
        std::array::from_fn(|i| ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u32);
    let mut message = input.to_vec();
    let bits = (input.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_le_bytes());
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    for chunk in message.as_chunks::<64>().0 {
        let words: [u32; 16] = std::array::from_fn(|i| {
            u32::from_le_bytes(chunk[i * 4..i * 4 + 4].try_into().expect("four bytes"))
        });
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(table[i])
                .wrapping_add(words[g])
                .rotate_left(SHIFTS[i]);
            (a, b, c, d) = (d, b.wrapping_add(rotated), b, c);
        }
        for (word, value) in state.iter_mut().zip([a, b, c, d]) {
            *word = word.wrapping_add(value);
        }
    }
    let mut digest = [0; 16];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    digest
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
    fn offline_uuids_match_bukkit() {
        assert_eq!(
            md5(b""),
            [
                0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
                0x42, 0x7e
            ]
        );
        // python3 -c 'import uuid,hashlib;print(uuid.UUID(bytes=hashlib.md5(b"OfflinePlayer:Notch").digest(),version=3).hex)'
        assert_eq!(
            rift_permissions::Uuid::from_bytes(offline_uuid("Notch")).to_string(),
            "b50ad385829d3141a2167e7d7539ba7f"
        );
    }

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
                server: "lobby".into(),
                properties: Arc::default(),
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
                server: "survival".into(),
                properties: Arc::default(),
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
